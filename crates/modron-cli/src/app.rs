//! `modron app`: start the NSH terminal app (`modron-app-tui`, built from
//! app/) next to this binary. The app is a separate executable; this module
//! only finds it, sets `MODRON_BIN` so the app starts THIS server binary,
//! and passes every remaining argument through unchanged.
//!
//! Lookup order for the app binary:
//! 1. `MODRON_APP_BIN` (when set and non-empty it is authoritative: a path
//!    that is not a file is an error, with no fallback),
//! 2. `modron-app-tui` next to the running `modron` executable,
//! 3. `modron-app-tui` on `PATH`.
//!
//! `modron` with no subcommand is unchanged: it still starts the built-in UI.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The app's executable name (the `[[bin]]` name in app/Cargo.toml).
pub const APP_BIN: &str = "modron-app-tui";
/// Exit code when the app binary cannot be found.
pub const EXIT_NOT_FOUND: i32 = 2;

/// The app's file name on this platform.
fn app_file_name() -> String {
    format!("{APP_BIN}{}", std::env::consts::EXE_SUFFIX)
}

/// Find the app binary. `is_file` is the filesystem probe (injected in tests).
/// On failure the message names the three places that were checked.
pub fn find_app(
    env_app: Option<&OsStr>,
    current_exe: Option<&Path>,
    path_var: Option<&OsStr>,
    is_file: &dyn Fn(&Path) -> bool,
) -> Result<PathBuf, String> {
    let file_name = app_file_name();
    let env_app = env_app.filter(|v| !v.is_empty());
    if let Some(value) = env_app {
        let path = PathBuf::from(value);
        return if is_file(&path) {
            Ok(path)
        } else {
            Err(format!(
                "modron app: MODRON_APP_BIN is set to {}, which is not a file",
                path.display()
            ))
        };
    }

    let beside = current_exe
        .and_then(|exe| exe.parent())
        .map(|dir| dir.join(&file_name));
    if let Some(candidate) = &beside {
        if is_file(candidate) {
            return Ok(candidate.clone());
        }
    }
    if let Some(path_var) = path_var {
        for dir in std::env::split_paths(path_var) {
            let candidate = dir.join(&file_name);
            if is_file(&candidate) {
                return Ok(candidate);
            }
        }
    }

    let beside_text = match &beside {
        Some(path) => format!("{} (not found)", path.display()),
        None => "(the modron executable path is unknown)".to_string(),
    };
    Err(format!(
        "modron app: cannot find {APP_BIN}. Looked in:\n\
         \x20 1. MODRON_APP_BIN (not set)\n\
         \x20 2. next to this modron: {beside_text}\n\
         \x20 3. PATH (not found)\n\
         Get it from the release archive (it is packaged next to modron), or build it:\n\
         \x20 cd app && cargo build --release --bin {APP_BIN}\n\
         then put it next to modron or set MODRON_APP_BIN to its path."
    ))
}

/// The `MODRON_BIN` value to give the app's child: the running modron binary
/// unless `MODRON_BIN` is already set (non-empty). `None` = leave it alone.
pub fn modron_bin_for_app(existing: Option<&OsStr>, current_exe: Option<&Path>) -> Option<PathBuf> {
    if existing.is_some_and(|v| !v.is_empty()) {
        return None;
    }
    current_exe.map(Path::to_path_buf)
}

/// The arguments after `app` when the command line is `modron app ...`.
/// `app` must be the first argument: everything after it, including `--`
/// and flags that modron itself also defines, goes to the app untouched.
pub fn passthrough_args(argv: &[OsString]) -> Option<&[OsString]> {
    match argv.get(1) {
        Some(first) if first.as_os_str() == OsStr::new("app") => Some(&argv[2..]),
        _ => None,
    }
}

/// Start the app with `args` and extra environment, and return the process
/// exit code. On unix this replaces the process (`exec`) and returns only when
/// that fails.
pub fn launch_with(args: &[OsString], extra_env: &[(&str, OsString)]) -> i32 {
    let env_app = std::env::var_os("MODRON_APP_BIN");
    let current = std::env::current_exe().ok();
    let path_var = std::env::var_os("PATH");
    let existing = std::env::var_os("MODRON_BIN");
    match plan_launch(
        args,
        extra_env,
        env_app.as_deref(),
        current.as_deref(),
        path_var.as_deref(),
        &|p: &Path| p.is_file(),
        existing.as_deref(),
    ) {
        Ok((app, command)) => run_command(command, &app),
        Err(message) => {
            eprintln!("{message}");
            EXIT_NOT_FOUND
        }
    }
}

/// `modron app ...` with no interception: the app's own arguments.
pub fn launch(args: &[OsString]) -> i32 {
    launch_with(args, &[])
}

/// Start the app for a mapped request (see [`request_from_cli`]): print the
/// notes for old flags first, then launch.
pub fn launch_request(request: &AppRequest) -> i32 {
    for note in &request.notes {
        eprintln!("{note}");
    }
    launch_with(&request.args, &request.env)
}

/// Find the app and build the command that starts it, without running it.
/// Errors carry the lookup message for a missing binary. The command gets
/// `MODRON_BIN` (unless already set), then `extra_env`.
pub fn plan_launch(
    args: &[OsString],
    extra_env: &[(&str, OsString)],
    env_app: Option<&OsStr>,
    current: Option<&Path>,
    path_var: Option<&OsStr>,
    is_file: &dyn Fn(&Path) -> bool,
    existing_modron_bin: Option<&OsStr>,
) -> Result<(PathBuf, Command), String> {
    let app = find_app(env_app, current, path_var, is_file)?;
    let mut command = Command::new(&app);
    command.args(args);
    if let Some(bin) = modron_bin_for_app(existing_modron_bin, current) {
        command.env("MODRON_BIN", bin);
    }
    for (key, value) in extra_env {
        command.env(key, value);
    }
    Ok((app, command))
}

/// What `modron` hands the app: its arguments, extra environment variables,
/// and one-line notes for old flags that no longer apply.
#[derive(Debug, Default, PartialEq)]
pub struct AppRequest {
    pub args: Vec<OsString>,
    pub env: Vec<(&'static str, OsString)>,
    pub notes: Vec<String>,
}

/// Map the parsed `modron` command line of an interactive run (bare `modron`
/// on a terminal, `modron tui`) to the app's arguments.
///
/// - No `--resume`: `--new`, the fresh session the old terminal started.
/// - `--resume` (bare): `--continue`; `--resume <id>`: `--session <id>`.
/// - `--connect unix` / `unix:PATH`: `--socket`; `--connect ws://...`: `--url`.
/// - `--provider`, `--model`, `--working-dir`, `--sessions-dir`: the app passes
///   them to a server it starts. With `--connect` no server is started, so
///   they are dropped with a note.
/// - `--yolo` / `--approve-all`, `--effort`, `--goal`, `--agent`, `--sandbox`,
///   `--allow`, `--deny`: the app's own config flags (`--sandbox bubblewrap`
///   is `--sandbox bwrap`).
/// - `--verbose`: `RUST_LOG=debug` for the server the app starts (unless set).
/// - Limits, compaction and `--attach` have no app flag: each gets a note.
///
/// `rust_log` is the caller's `RUST_LOG`, if set.
pub(crate) fn request_from_cli(cli: &crate::Cli, rust_log: Option<&OsStr>) -> AppRequest {
    let mut request = AppRequest::default();
    let args = &mut request.args;
    let notes = &mut request.notes;

    match &cli.resume {
        Some(Some(id)) => {
            args.push("--session".into());
            args.push(id.to_string().into());
        }
        Some(None) => args.push("--continue".into()),
        None => args.push("--new".into()),
    }

    match cli.connect.as_deref() {
        None => {}
        Some("unix") => match modron_server::pairing::default_modron_dir() {
            Ok(dir) => {
                args.push("--socket".into());
                args.push(dir.join("serve.sock").into_os_string());
            }
            Err(err) => notes.push(format!("modron: --connect unix: {err}; no socket to use")),
        },
        Some(target) => match target.strip_prefix("unix:") {
            Some(path) => {
                args.push("--socket".into());
                args.push(path.into());
            }
            None => {
                args.push("--url".into());
                args.push(target.into());
            }
        },
    }
    let remote = cli.connect.is_some();

    let working_dir = cli.working_dir.as_os_str();
    let working_dir_typed = working_dir != OsStr::new(".");
    if remote {
        if cli.provider.is_some() || cli.model.is_some() {
            notes.push(
                "note: --provider and --model are not sent with --connect; the server keeps its own"
                    .into(),
            );
        }
        if working_dir_typed || cli.sessions_dir.is_some() {
            notes.push(
                "note: --working-dir and --sessions-dir apply only to a server modron app starts; not with --connect"
                    .into(),
            );
        }
    } else {
        if cli.provider.is_some() || cli.model.is_some() {
            notes.push(
                "note: --provider and --model apply when modron app starts the server; a running server keeps its own"
                    .into(),
            );
        }
        if let Some(provider) = &cli.provider {
            args.push("--provider".into());
            args.push(provider.id().into());
        }
        if let Some(model) = &cli.model {
            args.push("--model".into());
            args.push(model.into());
        }
        if working_dir_typed {
            args.push("--working-dir".into());
            args.push(working_dir.to_owned());
        }
        if let Some(dir) = &cli.sessions_dir {
            args.push("--sessions-dir".into());
            args.push(dir.as_os_str().to_owned());
        }
    }

    if cli.approve_all || cli.yolo {
        args.push("--yolo".into());
    }
    if let Some(effort) = &cli.effort {
        args.push("--effort".into());
        args.push(effort.into());
    }
    if let Some(goal) = &cli.goal {
        args.push("--goal".into());
        args.push(goal.into());
    }
    if let Some(agent) = &cli.agent {
        args.push("--agent".into());
        args.push(agent.into());
    }
    if let Some(sandbox) = cli.sandbox {
        args.push("--sandbox".into());
        args.push(sandbox.app_word().into());
    }
    for tool in &cli.allow {
        args.push("--allow".into());
        args.push(tool.into());
    }
    for tool in &cli.deny {
        args.push("--deny".into());
        args.push(tool.into());
    }

    let dropped = [
        ("--max-input-tokens", cli.max_input_tokens.is_some()),
        ("--max-total-tokens", cli.max_total_tokens.is_some()),
        ("--max-turns", cli.max_turns.is_some()),
        ("--max-cost", cli.max_cost.is_some()),
        ("--compact-threshold", cli.compact_threshold.is_some()),
        ("--no-compact", cli.no_compact),
        ("--attach", !cli.attach.is_empty()),
    ];
    for (flag, typed) in dropped {
        if typed {
            notes.push(format!(
                "note: {flag} does not apply in modron app; ignored"
            ));
        }
    }

    let mut env = Vec::new();
    if cli.verbose && rust_log.is_none_or(|v| v.is_empty()) {
        env.push(("RUST_LOG", OsString::from("debug")));
    }
    request.env = env;
    request
}

#[cfg(unix)]
fn run_command(mut command: Command, app: &Path) -> i32 {
    use std::os::unix::process::CommandExt;
    // exec only returns on failure.
    let err = command.exec();
    eprintln!("modron app: cannot start {}: {err}", app.display());
    1
}

#[cfg(not(unix))]
fn run_command(mut command: Command, app: &Path) -> i32 {
    match command.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(err) => {
            eprintln!("modron app: cannot start {}: {err}", app.display());
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command as CliCommand};
    use clap::Parser;
    use std::collections::HashSet;

    fn probe(files: &[&str]) -> impl Fn(&Path) -> bool {
        let set: HashSet<PathBuf> = files.iter().map(PathBuf::from).collect();
        move |p: &Path| set.contains(p)
    }

    fn file_name() -> String {
        app_file_name()
    }

    #[test]
    fn env_var_wins_when_it_is_a_file() {
        let env = PathBuf::from("/opt/app/modron-app-tui");
        let exe = PathBuf::from("/usr/bin/modron");
        let found = find_app(
            Some(env.as_os_str()),
            Some(&exe),
            Some(OsStr::new("/usr/bin")),
            &probe(&["/opt/app/modron-app-tui", "/usr/bin/modron-app-tui"]),
        );
        assert_eq!(found, Ok(env));
    }

    #[test]
    fn env_var_that_is_not_a_file_is_an_error_with_no_fallback() {
        let exe = PathBuf::from("/usr/bin/modron");
        let found = find_app(
            Some(OsStr::new("/nowhere/modron-app-tui")),
            Some(&exe),
            None,
            &probe(&["/usr/bin/modron-app-tui"]),
        );
        let err = found.unwrap_err();
        assert!(err.contains("MODRON_APP_BIN is set to /nowhere/modron-app-tui"));
    }

    #[test]
    fn empty_env_var_counts_as_unset() {
        let exe = PathBuf::from("/usr/bin/modron");
        let found = find_app(
            Some(OsStr::new("")),
            Some(&exe),
            None,
            &probe(&["/usr/bin/modron-app-tui"]),
        );
        assert_eq!(found, Ok(PathBuf::from("/usr/bin/modron-app-tui")));
    }

    #[test]
    fn next_to_the_running_modron_beats_path() {
        let exe = PathBuf::from("/opt/modron/bin/modron");
        let found = find_app(
            None,
            Some(&exe),
            Some(OsStr::new("/usr/local/bin:/home/u/bin")),
            &probe(&[
                "/opt/modron/bin/modron-app-tui",
                "/usr/local/bin/modron-app-tui",
            ]),
        );
        assert_eq!(found, Ok(PathBuf::from("/opt/modron/bin/modron-app-tui")));
    }

    #[test]
    fn path_is_searched_in_order_when_not_next_to_modron() {
        let exe = PathBuf::from("/opt/modron/bin/modron");
        let found = find_app(
            None,
            Some(&exe),
            Some(OsStr::new("/a:/b")),
            &probe(&["/b/modron-app-tui"]),
        );
        assert_eq!(found, Ok(PathBuf::from("/b/modron-app-tui")));
    }

    #[test]
    fn missing_everywhere_names_all_three_places_and_how_to_get_it() {
        let exe = PathBuf::from("/opt/modron/bin/modron");
        let err = find_app(None, Some(&exe), Some(OsStr::new("/a")), &probe(&[])).unwrap_err();
        assert!(err.contains("MODRON_APP_BIN (not set)"), "{err}");
        assert!(
            err.contains(&format!("/opt/modron/bin/{}", file_name())),
            "{err}"
        );
        assert!(err.contains("PATH (not found)"), "{err}");
        assert!(
            err.contains("cargo build --release --bin modron-app-tui"),
            "{err}"
        );
    }

    #[test]
    fn modron_bin_is_the_running_binary_unless_already_set() {
        let exe = PathBuf::from("/opt/modron/bin/modron");
        assert_eq!(
            modron_bin_for_app(None, Some(&exe)),
            Some(PathBuf::from("/opt/modron/bin/modron"))
        );
        assert_eq!(
            modron_bin_for_app(Some(OsStr::new("/custom/modron")), Some(&exe)),
            None
        );
        assert_eq!(
            modron_bin_for_app(Some(OsStr::new("")), Some(&exe)),
            Some(PathBuf::from("/opt/modron/bin/modron"))
        );
        assert_eq!(modron_bin_for_app(None, None), None);
    }

    fn argv(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn passthrough_keeps_every_argument_after_app_untouched() {
        let line = argv(&[
            "modron",
            "app",
            "--model",
            "x",
            "--",
            "--demo",
            "--provider",
        ]);
        let rest = passthrough_args(&line).unwrap();
        assert_eq!(
            rest,
            &argv(&["--model", "x", "--", "--demo", "--provider"])[..]
        );
    }

    #[test]
    fn bare_app_passes_no_arguments_and_other_commands_are_not_intercepted() {
        assert_eq!(passthrough_args(&argv(&["modron", "app"])), Some(&[][..]));
        assert_eq!(passthrough_args(&argv(&["modron"])), None);
        assert_eq!(passthrough_args(&argv(&["modron", "--model", "x"])), None);
        assert_eq!(passthrough_args(&argv(&["modron", "chat"])), None);
    }

    #[test]
    fn clap_takes_app_arguments_that_modron_also_defines_so_main_refuses_that_path() {
        // clap hands --model and -- to modron itself when `app` is not the
        // first argument. `modron app ...` never reaches clap (passthrough_args
        // runs first), so main() refuses this path with a message instead.
        let cli = Cli::try_parse_from(["modron", "--verbose", "app", "--demo"]).unwrap();
        assert!(matches!(cli.command, Some(CliCommand::App { .. })));
        let clap_takes_model = Cli::try_parse_from(["modron", "app", "--model", "x"])
            .map(|cli| cli.model.is_some())
            .unwrap_or(false);
        assert!(clap_takes_model, "documents why the intercept exists");
    }

    fn mapped(line: &[&str], rust_log: Option<&str>) -> AppRequest {
        let cli = Cli::try_parse_from(line).expect("parses");
        request_from_cli(&cli, rust_log.map(OsStr::new))
    }

    fn words(request: &AppRequest) -> Vec<String> {
        request
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    const RESUME_ID: &str = "00000000-0000-0000-0000-000000000001";

    #[test]
    fn bare_modron_starts_a_new_session_like_the_old_terminal() {
        let request = mapped(&["modron"], None);
        assert_eq!(words(&request), ["--new"]);
        assert!(request.notes.is_empty());
        assert!(request.env.is_empty());
    }

    #[test]
    fn resume_maps_to_continue_or_session() {
        assert_eq!(
            words(&mapped(&["modron", "--resume"], None)),
            ["--continue"]
        );
        let by_id = format!("--resume={RESUME_ID}");
        assert_eq!(
            words(&mapped(&["modron", &by_id], None)),
            ["--session", RESUME_ID]
        );
    }

    #[test]
    fn connect_targets_map_to_socket_or_url() {
        assert_eq!(
            words(&mapped(&["modron", "--connect", "unix:/tmp/s.sock"], None)),
            ["--new", "--socket", "/tmp/s.sock"]
        );
        assert_eq!(
            words(&mapped(
                &["modron", "--connect", "ws://127.0.0.1:7699"],
                None
            )),
            ["--new", "--url", "ws://127.0.0.1:7699"]
        );
        let default = words(&mapped(&["modron", "--connect", "unix"], None));
        assert_eq!(default[0], "--new");
        assert_eq!(default[1], "--socket");
        assert!(default[2].ends_with("serve.sock"), "{default:?}");
    }

    #[test]
    fn provider_model_and_dirs_reach_a_server_the_app_starts() {
        let request = mapped(
            &[
                "modron",
                "--provider",
                "openai-compat",
                "--model",
                "m1",
                "--working-dir",
                "/w",
                "--sessions-dir",
                "/s",
            ],
            None,
        );
        let w = words(&request);
        for pair in [
            ["--provider", "openai-compat"],
            ["--model", "m1"],
            ["--working-dir", "/w"],
            ["--sessions-dir", "/s"],
        ] {
            assert!(
                w.windows(2).any(|p| p[0] == pair[0] && p[1] == pair[1]),
                "{pair:?} in {w:?}"
            );
        }
        assert_eq!(request.notes.len(), 1, "{:?}", request.notes);
    }

    #[test]
    fn default_working_dir_is_not_sent() {
        assert!(!words(&mapped(&["modron"], None)).contains(&"--working-dir".to_string()));
    }

    #[test]
    fn server_only_flags_are_dropped_with_a_note_under_connect() {
        let request = mapped(
            &[
                "modron",
                "--connect",
                "ws://127.0.0.1:7699",
                "--provider",
                "mock",
                "--working-dir",
                "/w",
            ],
            None,
        );
        let w = words(&request);
        assert!(!w.contains(&"--provider".to_string()), "{w:?}");
        assert!(!w.contains(&"--working-dir".to_string()), "{w:?}");
        assert_eq!(request.notes.len(), 2, "{:?}", request.notes);
        assert!(request.notes[0].contains("--provider"));
        assert!(request.notes[1].contains("--working-dir"));
    }

    #[test]
    fn config_flags_map_to_the_apps_names() {
        let request = mapped(
            &[
                "modron",
                "--yolo",
                "--effort",
                "high",
                "--goal",
                "ship it",
                "--agent",
                "rev",
                "--sandbox",
                "bubblewrap",
                "--allow",
                "bash",
                "--deny",
                "write",
            ],
            None,
        );
        assert_eq!(
            words(&request),
            [
                "--new",
                "--yolo",
                "--effort",
                "high",
                "--goal",
                "ship it",
                "--agent",
                "rev",
                "--sandbox",
                "bwrap",
                "--allow",
                "bash",
                "--deny",
                "write",
            ]
        );
        assert_eq!(
            words(&mapped(
                &["modron", "--approve-all", "--sandbox", "off"],
                None
            ))[1],
            "--yolo"
        );
    }

    #[test]
    fn flags_with_no_app_equivalent_are_listed_not_silently_ignored() {
        let request = mapped(
            &[
                "modron",
                "--max-turns",
                "5",
                "--attach",
                "x.png",
                "--no-compact",
            ],
            None,
        );
        assert_eq!(request.notes.len(), 3, "{:?}", request.notes);
        assert!(request.notes.iter().any(|n| n.contains("--max-turns")));
        assert!(request.notes.iter().any(|n| n.contains("--attach")));
        assert!(request.notes.iter().any(|n| n.contains("--no-compact")));
        assert_eq!(words(&request), ["--new"]);
    }

    #[test]
    fn verbose_sets_debug_logging_unless_rust_log_is_set() {
        assert_eq!(
            mapped(&["modron", "--verbose"], None).env,
            [("RUST_LOG", OsString::from("debug"))]
        );
        assert!(mapped(&["modron", "--verbose"], Some("warn"))
            .env
            .is_empty());
        assert!(mapped(&["modron"], None).env.is_empty());
    }

    #[test]
    fn tui_subcommand_maps_the_same_way_as_bare_modron() {
        assert_eq!(
            words(&mapped(&["modron", "tui", "--provider", "mock"], None)),
            ["--new", "--provider", "mock"]
        );
    }

    #[test]
    fn the_app_binary_is_checked_and_its_command_carries_modron_bin_and_env() {
        let exe = PathBuf::from("/opt/modron/bin/modron");
        let app = "/opt/modron/bin/modron-app-tui";
        let args = [OsString::from("--new")];
        let extra = [("RUST_LOG", OsString::from("debug"))];
        let (path, command) =
            plan_launch(&args, &extra, None, Some(&exe), None, &probe(&[app]), None).unwrap();
        assert_eq!(path, PathBuf::from(app));
        let envs: Vec<_> = command
            .get_envs()
            .map(|(k, v)| (k.to_os_string(), v.map(|v| v.to_os_string())))
            .collect();
        assert!(envs.contains(&(OsString::from("MODRON_BIN"), Some(exe.into_os_string()))));
        assert!(envs.contains(&(OsString::from("RUST_LOG"), Some(OsString::from("debug")))));
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["--new"]);
    }

    #[test]
    fn a_missing_app_binary_is_the_exit_two_instructions_not_a_panic() {
        let exe = PathBuf::from("/opt/modron/bin/modron");
        let err = plan_launch(&[], &[], None, Some(&exe), None, &probe(&[]), None).unwrap_err();
        assert!(
            err.contains("cargo build --release --bin modron-app-tui"),
            "{err}"
        );
        assert_eq!(EXIT_NOT_FOUND, 2);
    }
}
