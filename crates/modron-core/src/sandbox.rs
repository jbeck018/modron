//! macOS Seatbelt sandbox (Phase-1b A2): wrap a shell command so it cannot
//! touch the network and can write only inside the workspace and system temp
//! dirs — even when the approval gate let it run. Defense in depth for the
//! `bash` tool; the profile generator is a pure function so tests can pin
//! its shape, and the empirical behavior was verified on this machine:
//! `curl` fails, workspace writes succeed, `/usr/local` writes fail.
//!
//! The Linux (bubblewrap + seccomp) port is Phase-1 residual work; asking
//! for `SandboxPolicy::Seatbelt` on Linux is a configuration error, not a
//! silent fallback.

use crate::{Error, Result, SandboxPolicy};
use std::path::{Path, PathBuf};

/// Seatbelt profile for `sandbox-exec`: allow everything by default, deny
/// the network outright, and fence file writes to the workspace plus the
/// standard temp locations. Read access stays world-wide (the agent needs to
/// read the repo and toolchain). Paths are canonicalized because macOS
/// resolves /tmp → /private/tmp (and Seatbelt matches literal subpaths).
pub fn seatbelt_profile(workspace: &Path) -> String {
    let mut paths: Vec<PathBuf> = vec![workspace.to_path_buf(), std::env::temp_dir()];
    if let Ok(real) = std::fs::canonicalize(workspace) {
        paths.insert(0, real);
    }
    for extra in [
        "/tmp",
        "/var/tmp",
        "/private/tmp",
        "/private/var/tmp",
        "/dev/null",
    ] {
        let p = PathBuf::from(extra);
        if !paths.contains(&p) {
            paths.push(p);
        }
    }
    let mut out = String::from("(version 1)\n(allow default)\n(deny network*)\n");
    for p in &paths {
        out.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            sbpl_escape(&p.display().to_string())
        ));
    }
    out.push_str("(deny file-write*)\n");
    out
}

/// Escape a path for an SBPL string literal: a `"` or `\` in a workspace
/// path must not be able to close the string and inject profile rules.
fn sbpl_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A shell command prepared to run under a sandbox policy: not yet spawned,
/// so the caller chooses stdio, process group and capture. Holds the Seatbelt
/// profile file (if any) until dropped; keep it alive until the child exits.
pub struct SandboxedCommand {
    /// `sh -c <command>` wrapped for the policy, cwd set, provider secrets
    /// scrubbed from its environment (D9).
    pub command: tokio::process::Command,
    _profile: Option<ProfileFile>,
}

/// Removes the Seatbelt profile file on drop.
struct ProfileFile(PathBuf);

impl Drop for ProfileFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Build `sh -c <command>` for `policy`, to run in `workspace`:
/// `Off` = a plain shell; Seatbelt (macOS) = `sandbox-exec -f <profile>`;
/// Bubblewrap (Linux) = `bwrap` with [`bubblewrap_args`]. A policy/platform
/// mismatch is an actionable error, never a silent fallback. Every tool that
/// runs a shell command (bash, verify, diagnostics) goes through here so the
/// sandbox covers all of them, not only `bash`.
pub fn shell_command(
    policy: SandboxPolicy,
    command: &str,
    workspace: &Path,
) -> Result<SandboxedCommand> {
    let (mut cmd, profile) = match (policy, std::env::consts::OS) {
        (SandboxPolicy::Off, _) => {
            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-c").arg(command).current_dir(workspace);
            (cmd, None)
        }
        (SandboxPolicy::Seatbelt, "macos") => {
            let profile = write_seatbelt_profile(workspace)?;
            let mut cmd = tokio::process::Command::new("sandbox-exec");
            cmd.arg("-f")
                .arg(&profile.0)
                .arg("sh")
                .arg("-c")
                .arg(command)
                .current_dir(workspace);
            (cmd, Some(profile))
        }
        (SandboxPolicy::Bubblewrap, "linux") => {
            let canonical = workspace
                .canonicalize()
                .unwrap_or_else(|_| workspace.to_path_buf());
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let mut cmd = tokio::process::Command::new("bwrap");
            cmd.args(bubblewrap_args(&canonical, home.as_deref(), &|p| {
                std::fs::metadata(p).ok().map(|m| m.is_dir())
            }))
            .arg("sh")
            .arg("-c")
            .arg(command);
            (cmd, None)
        }
        (policy, os) => {
            return Err(Error::Config(
                "sandbox".into(),
                format!(
                    "sandbox policy {policy:?} is not supported on {os} — run with --sandbox off \
                     (and an approval mode you trust) on this platform"
                ),
            ))
        }
    };
    crate::hooks::scrub_tokio_command(&mut cmd);
    // A dropped (timed out, interrupted) future must not leak the child.
    cmd.kill_on_drop(true);
    Ok(SandboxedCommand {
        command: cmd,
        _profile: profile,
    })
}

/// Run `command` via `sh -c` under the selected sandbox policy and collect
/// its whole output. Callers that need capped or streamed capture use
/// [`shell_command`] and spawn the child themselves.
pub async fn run_sandboxed(
    policy: SandboxPolicy,
    command: &str,
    workspace: &Path,
) -> Result<std::process::Output> {
    let mut prepared = shell_command(policy, command, workspace)?;
    prepared
        .command
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| spawn_error(policy, &e))
}

/// The actionable error for a sandbox launcher that could not be spawned.
pub fn spawn_error(policy: SandboxPolicy, e: &std::io::Error) -> Error {
    match policy {
        SandboxPolicy::Bubblewrap => Error::Config(
            "bwrap".into(),
            format!(
                "cannot spawn bwrap — is bubblewrap installed? \
                 (apt install bubblewrap / dnf install bubblewrap): {e}"
            ),
        ),
        SandboxPolicy::Seatbelt => Error::Config(
            "sandbox-exec".into(),
            format!("cannot spawn sandbox-exec (is macOS intact?): {e}"),
        ),
        SandboxPolicy::Off => Error::Config("sh".into(), format!("cannot spawn sh: {e}")),
    }
}

/// Materialize the Seatbelt profile for `workspace` in a file only this
/// process could have named: `create_new` with a pid + clock + counter
/// name, mode 0600. The profile directory is the system temp dir, which the
/// profile itself makes writable, so the name must not be predictable (a
/// surviving sandboxed process must not be able to pre-plant or swap it).
fn write_seatbelt_profile(workspace: &Path) -> Result<ProfileFile> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let profile = seatbelt_profile(workspace);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "modron-sb-{}-{nanos}-{}.sb",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options
        .open(&path)
        .and_then(|mut file| file.write_all(profile.as_bytes()));
    match written {
        Ok(()) => Ok(ProfileFile(path)),
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            Err(Error::Config(
                "sandbox profile".into(),
                format!("cannot write profile {}: {e}", path.display()),
            ))
        }
    }
}

/// Home-relative directories masked inside the bubblewrap sandbox: the
/// command can read the rest of the filesystem (it needs toolchains), but
/// not the user's credentials.
const MASKED_HOME_DIRS: &[&str] = &[
    ".modron",
    ".config/modron",
    ".ssh",
    ".aws",
    ".gnupg",
    ".config/gh",
    ".config/gcloud",
    ".docker",
    ".kube",
    ".netrc",
];

/// The `bwrap` arguments (everything before `sh -c <command>`). Same posture
/// as Seatbelt: no network, the whole filesystem readable but read-only,
/// writes only in the workspace and a private `/tmp`. Hardening:
/// - `--die-with-parent`, `--unshare-pid`: nothing outlives the tool call,
///   and the command cannot see or signal host processes;
/// - `--new-session`: no controlling terminal, so it cannot inject
///   keystrokes into the TUI (TIOCSTI);
/// - `--dev /dev`: a minimal private /dev instead of the host's;
/// - credential directories under `$HOME` are masked with empty tmpfs
///   (unless the workspace lives inside one).
///
/// Pure: `probe` reports `Some(is_dir)` for a path that exists, so tests
/// pin the shape without touching the filesystem.
pub fn bubblewrap_args(
    workspace: &Path,
    home: Option<&Path>,
    probe: &dyn Fn(&Path) -> Option<bool>,
) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;
    let mut args: Vec<OsString> = Vec::new();
    let mut push = |parts: &[&std::ffi::OsStr]| {
        for part in parts {
            args.push(part.to_os_string());
        }
    };
    let s = std::ffi::OsStr::new;
    push(&[s("--die-with-parent")]);
    push(&[s("--new-session")]);
    push(&[s("--unshare-net")]);
    push(&[s("--unshare-ipc")]);
    push(&[s("--unshare-pid")]);
    push(&[s("--ro-bind"), s("/"), s("/")]);
    push(&[s("--dev"), s("/dev")]);
    push(&[s("--proc"), s("/proc")]);
    push(&[s("--tmpfs"), s("/tmp")]);
    if let Some(home) = home {
        for rel in MASKED_HOME_DIRS {
            let masked = home.join(rel);
            // Never mask the workspace itself or one of its ancestors.
            if workspace.starts_with(&masked) {
                continue;
            }
            match probe(&masked) {
                Some(true) => push(&[s("--tmpfs"), masked.as_os_str()]),
                // A credential FILE (e.g. ~/.netrc): hide it behind /dev/null.
                Some(false) => push(&[s("--ro-bind"), s("/dev/null"), masked.as_os_str()]),
                None => {}
            }
        }
    }
    push(&[s("--bind"), workspace.as_os_str(), workspace.as_os_str()]);
    push(&[s("--chdir"), workspace.as_os_str()]);
    args
}

/// Whether the `SandboxPolicy` can actually be honored on this platform.
pub fn is_supported(policy: SandboxPolicy) -> bool {
    match policy {
        SandboxPolicy::Off => true,
        SandboxPolicy::Seatbelt => cfg!(target_os = "macos"),
        SandboxPolicy::Bubblewrap => cfg!(target_os = "linux"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_denies_network_and_fences_writes() {
        let dir = tempfile::tempdir().unwrap();
        let profile = seatbelt_profile(dir.path());
        assert!(profile.contains("(deny network*)"));
        assert!(profile.contains(&format!(
            "(allow file-write* (subpath \"{}\"))",
            dir.path().display()
        )));
        assert!(profile.contains("(deny file-write*)"));
        // Stable across calls (byte-stable prompts/contexts matter).
        assert_eq!(profile, seatbelt_profile(dir.path()));
    }

    #[test]
    fn temp_alias_paths_are_included_so_writes_work_everywhere() {
        let dir = tempfile::tempdir().unwrap();
        let profile = seatbelt_profile(dir.path());
        // macOS /tmp == /private/tmp: both spellings must be allowed.
        assert!(profile.contains("(allow file-write* (subpath \"/tmp\"))"));
        assert!(profile.contains("(allow file-write* (subpath \"/private/tmp\"))"));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn seatbelt_blocks_network_but_allows_workspace_writes() {
        if std::process::Command::new("which")
            .arg("sandbox-exec")
            .output()
            .map(|o| o.stdout.is_empty())
            .unwrap_or(true)
        {
            eprintln!("skipping: sandbox-exec not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // Network: denied (curl cannot resolve/connect).
        let net = run_sandboxed(
            SandboxPolicy::Seatbelt,
            "curl -m 3 -s https://example.com >/dev/null 2>&1; echo $?",
            dir.path(),
        )
        .await
        .unwrap();
        let code = String::from_utf8_lossy(&net.stdout).trim().to_string();
        assert_ne!(code, "0", "network must be denied under Seatbelt");
        // Workspace write: allowed.
        let write = run_sandboxed(
            SandboxPolicy::Seatbelt,
            "echo hi > ok.txt && cat ok.txt",
            dir.path(),
        )
        .await
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&write.stdout).trim(), "hi");
        // Outside write: denied.
        let out = run_sandboxed(
            SandboxPolicy::Seatbelt,
            "echo x > /usr/local/modron-sbx-test 2>/dev/null; echo $?",
            dir.path(),
        )
        .await
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1");
    }

    #[test]
    fn profile_paths_cannot_inject_rules() {
        let evil = Path::new("/tmp/x\") (allow network*) (\"");
        let profile = seatbelt_profile(evil);
        assert!(
            !profile.contains("\n(allow network*)"),
            "a quote in the path must stay inside the string literal: {profile}"
        );
        assert!(
            profile.contains(r#"x\") (allow network*) (\""#),
            "{profile}"
        );
        assert_eq!(sbpl_escape(r#"a\b"c"#), r#"a\\b\"c"#);
    }

    fn arg_strings(args: &[std::ffi::OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn bubblewrap_args_harden_and_fence_writes() {
        let ws = Path::new("/home/u/proj");
        let home = Path::new("/home/u");
        let exists = |p: &Path| {
            (p == Path::new("/home/u/.ssh") || p == Path::new("/home/u/.modron")).then_some(true)
        };
        let args = arg_strings(&bubblewrap_args(ws, Some(home), &exists));
        for flag in [
            "--die-with-parent",
            "--new-session",
            "--unshare-net",
            "--unshare-pid",
        ] {
            assert!(args.iter().any(|a| a == flag), "missing {flag}: {args:?}");
        }
        let joined = args.join(" ");
        assert!(joined.contains("--ro-bind / /"), "{joined}");
        assert!(joined.contains("--dev /dev"), "{joined}");
        assert!(
            !joined.contains("--dev-bind"),
            "host /dev must not be bound"
        );
        assert!(
            joined.contains("--bind /home/u/proj /home/u/proj"),
            "{joined}"
        );
        // Existing credential dirs are masked; missing ones add nothing.
        assert!(joined.contains("--tmpfs /home/u/.ssh"), "{joined}");
        assert!(joined.contains("--tmpfs /home/u/.modron"), "{joined}");
        assert!(!joined.contains(".aws"), "{joined}");
        // The workspace bind comes after the read-only root and the masks.
        let ro = joined.find("--ro-bind / /").unwrap();
        let mask = joined.find("--tmpfs /home/u/.ssh").unwrap();
        let bind = joined.find("--bind /home/u/proj").unwrap();
        assert!(ro < mask && mask < bind, "{joined}");
    }

    #[test]
    fn bubblewrap_never_masks_the_workspace() {
        let ws = Path::new("/home/u/.modron/work");
        let home = Path::new("/home/u");
        let args = arg_strings(&bubblewrap_args(ws, Some(home), &|_| Some(true)));
        let joined = args.join(" ");
        assert!(!joined.contains("--tmpfs /home/u/.modron "), "{joined}");
        assert!(joined.contains("--bind /home/u/.modron/work"), "{joined}");
    }

    #[test]
    fn off_policy_is_a_plain_shell_and_mismatch_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let prepared = shell_command(SandboxPolicy::Off, "true", dir.path()).unwrap();
        assert_eq!(prepared.command.as_std().get_program(), "sh");
        #[cfg(target_os = "linux")]
        assert!(shell_command(SandboxPolicy::Seatbelt, "true", dir.path()).is_err());
    }

    /// Real bubblewrap, when it is installed and usable here.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bubblewrap_blocks_network_and_outside_writes_but_allows_workspace_writes() {
        let dir = tempfile::tempdir_in(std::env::var("HOME").unwrap_or_else(|_| "/var/tmp".into()))
            .or_else(|_| tempfile::tempdir_in("/var/tmp"))
            .unwrap();
        let probe = run_sandboxed(SandboxPolicy::Bubblewrap, "true", dir.path()).await;
        if !probe.as_ref().is_ok_and(|o| o.status.success()) {
            eprintln!("skipping: bwrap not usable here ({probe:?})");
            return;
        }
        let run = |cmd: &'static str| {
            let path = dir.path().to_path_buf();
            async move {
                let out = run_sandboxed(SandboxPolicy::Bubblewrap, cmd, &path)
                    .await
                    .unwrap();
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
        };
        assert_eq!(run("echo hi > ok.txt && cat ok.txt").await, "hi");
        assert!(
            dir.path().join("ok.txt").exists(),
            "workspace write lands on the host"
        );
        assert_ne!(
            run("echo x > /usr/modron-sbx-test 2>/dev/null; echo $?").await,
            "0",
            "writes outside the workspace are refused"
        );
        assert!(!Path::new("/usr/modron-sbx-test").exists());
        // Only loopback exists inside the network namespace (/proc is
        // mounted fresh, so /proc/net/dev shows the sandbox's own view).
        assert_eq!(
            run("grep -c ':' /proc/net/dev").await,
            "1",
            "no host network interface is reachable"
        );
        // The command is pid 1's child in its own pid namespace.
        let pids: usize = run("ls /proc | grep -c '^[0-9]'").await.parse().unwrap();
        assert!(pids < 10, "host processes are not visible (saw {pids})");
        // No controlling terminal.
        assert_ne!(run("sh -c ': > /dev/tty' 2>/dev/null; echo $?").await, "0");
    }
}
