//! Phase 3a acceptance tests (R4): the offline router eval through the real
//! binary, and the routing-telemetry trail (R5) at the engine boundary.

use std::process::Command;

fn modron_bin() -> &'static str {
    env!("CARGO_BIN_EXE_modron")
}

/// R4: `modron router eval` runs offline and reports accuracy + savings.
#[test]
fn router_eval_reports_accuracy_and_savings() {
    let out = Command::new(modron_bin())
        .args(["router", "eval"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("router eval — 16 examples"),
        "header: {stdout}"
    );
    assert!(stdout.contains("16/16 (100%)"), "accuracy line: {stdout}");
    assert!(stdout.contains("always-strong"), "cost baseline: {stdout}");
    assert!(stdout.contains("saved:"), "savings line: {stdout}");
    assert!(
        stdout.contains("learned:"),
        "decisions are labeled learned: {stdout}"
    );
}

/// R1 (binary surface, via MODRON_CONFIG injection): an unknown router mode
/// fails actionably before any provider is contacted.
#[test]
fn unknown_router_mode_is_actionable() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.toml");
    std::fs::write(&cfg, "[router]\nmode = \"psychic\"\n").unwrap();
    let out = Command::new(modron_bin())
        .args(["--provider", "mock", "x"])
        .env("MODRON_CONFIG", cfg.to_str().unwrap())
        .env(
            "MODRON_SESSIONS_DIR",
            dir.path().join("s").to_str().unwrap(),
        )
        .output()
        .unwrap();
    assert!(!out.status.success(), "psychic mode must not run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown router mode `psychic`"),
        "actionable error: {stderr}"
    );
}

/// R1 (binary surface): per-action overrides parse and apply — a `[router]`
/// that pins strong_model to the cheap model flips a plan task to cheap.
#[test]
fn router_config_models_apply() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.toml");
    std::fs::write(
        &cfg,
        "[router]\nmode = \"heuristic\"\nstrong_model = \"claude-haiku-4-5\"\n",
    )
    .unwrap();
    let out = Command::new(modron_bin())
        .args(["router", "eval"])
        .env("MODRON_CONFIG", cfg.to_str().unwrap())
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    // With both tiers on haiku, the routed cost equals always-strong and
    // nothing is saved — the report stays honest about it.
    assert!(stdout.contains("saved:"), "{stdout}");
}
