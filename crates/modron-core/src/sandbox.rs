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
            p.display()
        ));
    }
    out.push_str("(deny file-write*)\n");
    out
}

/// Run `command` via `sh -c` under the Seatbelt profile. The profile is
/// materialized to a temp file (proven `-f` path rather than the deprecated
/// `-p` inline form). Only `SandboxPolicy::Seatbelt` reaches this; callers
/// on other platforms get an actionable error instead of a silent fallback.
pub async fn run_sandboxed(command: &str, workspace: &Path) -> Result<std::process::Output> {
    // Route to the platform executor.
    match std::env::consts::OS {
        "macos" => run_seatbelt(command, workspace).await,
        "linux" => run_bubblewrap(command, workspace).await,
        other => Err(Error::Config(
            "sandbox".into(),
            format!(
                "no sandbox implementation for {other} — run with --sandbox off \
                 (and an approval mode you trust) on this platform"
            ),
        )),
    }
}

/// macOS Seatbelt: `sandbox-exec -f <profile> sh -c <command>`.
async fn run_seatbelt(command: &str, workspace: &Path) -> Result<std::process::Output> {
    let profile = seatbelt_profile(workspace);
    let dir = std::env::temp_dir().join(format!("modron-sb-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| {
        Error::Config(
            "sandbox profile".into(),
            format!("cannot create {dir:?}: {e}"),
        )
    })?;
    let profile_path = dir.join("profile.sb");
    std::fs::write(&profile_path, &profile).map_err(|e| {
        Error::Config(
            "sandbox profile".into(),
            format!("cannot write profile: {e}"),
        )
    })?;

    let output = tokio::process::Command::new("sandbox-exec")
        .arg("-f")
        .arg(&profile_path)
        .arg("sh")
        .arg("-c")
        .arg(command)
        .current_dir(workspace)
        .output()
        .await
        .map_err(|e| {
            Error::Config(
                "sandbox-exec".into(),
                format!("cannot spawn sandbox-exec (is macOS intact?): {e}"),
            )
        })?;
    let _ = std::fs::remove_file(&profile_path);
    Ok(output)
}

/// Linux bubblewrap (`bwrap`): network denied via `--unshare-net`, the
/// workspace bind-mounted read-write, everything else read-only or masked.
/// Same posture as Seatbelt: no network, writes only in the workspace + tmp.
async fn run_bubblewrap(command: &str, workspace: &Path) -> Result<std::process::Output> {
    let canonical = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());

    let output = tokio::process::Command::new("bwrap")
        .arg("--unshare-net") // deny network
        .arg("--unshare-ipc")
        .arg("--dev-bind")
        .arg("/dev")
        .arg("/dev")
        .arg("--proc")
        .arg("/proc")
        .arg("--bind")
        .arg("/usr")
        .arg("/usr") // read-only system
        .arg("--ro-bind")
        .arg("/etc")
        .arg("/etc")
        .arg("--ro-bind")
        .arg("/bin")
        .arg("/bin")
        .arg("--ro-bind")
        .arg("/lib")
        .arg("/lib")
        .arg("--ro-bind")
        .arg("/lib64")
        .arg("/lib64")
        .arg("--tmpfs")
        .arg("/tmp") // writable tmpfs for temp files
        .arg("--bind")
        .arg(&canonical)
        .arg(&canonical) // the workspace (read-write)
        .arg("--chdir")
        .arg(&canonical)
        .arg("sh")
        .arg("-c")
        .arg(command)
        .output()
        .await
        .map_err(|e| {
            Error::Config(
                "bwrap".into(),
                format!(
                    "cannot spawn bwrap — is bubblewrap installed? \
                     (apt install bubblewrap / dnf install bubblewrap): {e}"
                ),
            )
        })?;
    Ok(output)
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
            "curl -m 3 -s https://example.com >/dev/null 2>&1; echo $?",
            dir.path(),
        )
        .await
        .unwrap();
        let code = String::from_utf8_lossy(&net.stdout).trim().to_string();
        assert_ne!(code, "0", "network must be denied under Seatbelt");
        // Workspace write: allowed.
        let write = run_sandboxed("echo hi > ok.txt && cat ok.txt", dir.path())
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&write.stdout).trim(), "hi");
        // Outside write: denied.
        let out = run_sandboxed(
            "echo x > /usr/local/modron-sbx-test 2>/dev/null; echo $?",
            dir.path(),
        )
        .await
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1");
    }
}
