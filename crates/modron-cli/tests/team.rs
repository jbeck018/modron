//! Team mode wiring (V3): the `modron team` flags, and a team size outside
//! 1 to 5 refused before any provider or MCP server is set up. The team
//! behaviour itself is tested in modron-core (`team.rs`) on the mock provider.

use std::process::Command;

fn modron() -> Command {
    Command::new(env!("CARGO_BIN_EXE_modron"))
}

#[test]
fn team_help_lists_the_flags() {
    let out = modron().args(["team", "--help"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    for flag in ["--workers", "--until-verified", "--max-rounds"] {
        assert!(
            text.contains(flag),
            "{flag} missing from `modron team --help`:\n{text}"
        );
    }
}

#[test]
fn team_refuses_more_than_five_workers_before_setup() {
    let out = modron()
        .args(["team", "-n", "9", "split this task"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "a team of 9 must not run");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("team size must be 1 to 5, got 9"), "{err}");
}
