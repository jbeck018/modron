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

/// Start the app with `args` and return the process exit code. On unix this
/// replaces the process (`exec`) and returns only when that fails.
pub fn launch(args: &[OsString]) -> i32 {
    let env_app = std::env::var_os("MODRON_APP_BIN");
    let current = std::env::current_exe().ok();
    let path_var = std::env::var_os("PATH");
    let app = match find_app(
        env_app.as_deref(),
        current.as_deref(),
        path_var.as_deref(),
        &|p: &Path| p.is_file(),
    ) {
        Ok(path) => path,
        Err(message) => {
            eprintln!("{message}");
            return EXIT_NOT_FOUND;
        }
    };

    let mut command = Command::new(&app);
    command.args(args);
    let existing = std::env::var_os("MODRON_BIN");
    if let Some(bin) = modron_bin_for_app(existing.as_deref(), current.as_deref()) {
        command.env("MODRON_BIN", bin);
    }
    run_command(command, &app)
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
}
