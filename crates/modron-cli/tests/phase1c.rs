//! V-gates for Phase 1c (interactivity): chat REPL through the real binary,
//! sessions listing with token/cost accounting, streaming sink behavior at
//! the engine boundary (engine-level tests live in modron-core).

use std::process::Command;
use std::time::Duration;

fn modron_bin() -> &'static str {
    env!("CARGO_BIN_EXE_modron")
}

/// I3: the chat REPL drives multiple exchanges through ONE session; answers
/// stream (the delta sink prints them); a cumulative usage line appears per
/// exchange; `/exit` ends cleanly with a resume hint. Stdin is piped, so the
/// interactive approver is absent and Ask mode blocks the demo's bash call —
/// the conversation continues anyway.
#[test]
fn chat_repl_runs_multi_turn_through_one_session() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let mut child = Command::new(modron_bin())
        .args([
            "chat",
            "--provider",
            "mock",
            "--sessions-dir",
            sessions.to_str().unwrap(),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("spawn modron chat");

    // Two exchanges + exit. Give the child a moment between lines.
    use std::io::Write as _;
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "first task").unwrap();
    stdin.flush().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    writeln!(stdin, "second task").unwrap();
    stdin.flush().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    writeln!(stdin, "/exit").unwrap();
    stdin.flush().unwrap();
    drop(stdin);

    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("modron chat"), "banner: {stdout}");
    assert_eq!(
        stdout.matches("Demo complete").count(),
        2,
        "two answers: {stdout}"
    );
    assert_eq!(
        stdout.matches("cumulative").count(),
        2,
        "two usage lines: {stdout}"
    );
    assert!(
        stdout.contains("resume with --resume"),
        "session hint: {stdout}"
    );
}

/// I5: `modron sessions` reports per-session tokens and estimated cost
/// (provider-reported usage events, catalog-priced).
#[test]
fn sessions_list_shows_tokens_and_cost() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let out = Command::new(modron_bin())
        .args([
            "demo",
            "--provider",
            "mock",
            "--approve-all",
            "--sessions-dir",
            sessions.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    let list = Command::new(modron_bin())
        .args(["sessions", "--sessions-dir", sessions.to_str().unwrap()])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("tokens"), "header: {stdout}");
    assert!(stdout.contains("est. cost"), "header: {stdout}");
    assert!(stdout.contains('$'), "a cost figure appears: {stdout}");
    // The demo run's usage (1152 + 80 tokens) shows up.
    assert!(
        stdout.contains("1232"),
        "total tokens in the list: {stdout}"
    );
}

/// I1 at the CLI boundary: a piped one-shot run does NOT stream mid-answer
/// (the delta sink is chat-only) — the answer arrives once, complete.
#[test]
fn one_shot_print_mode_does_not_double_print() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(modron_bin())
        .args([
            "demo",
            "--provider",
            "mock",
            "--approve-all",
            "--sessions-dir",
            dir.path().join("s").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.matches("Demo complete").count(),
        1,
        "answer printed exactly once: {stdout}"
    );
}
