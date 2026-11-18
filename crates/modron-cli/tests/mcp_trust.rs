//! Program V (V1): a project MCP server declared in `.mcp.json` is spawned
//! only after the user trusts it for the workspace, and a changed definition
//! needs trust again. The fake server is a shell script that records each
//! spawn in a marker file and then exits, so the spawn is observable without
//! speaking MCP.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn modron_bin() -> &'static str {
    env!("CARGO_BIN_EXE_modron")
}

/// Run `modron <args>` in `workspace`, with the config dir (and so the trust
/// file) and HOME inside `home`.
fn modron(workspace: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(modron_bin())
        .args(args)
        .current_dir(workspace)
        .env("MODRON_CONFIG", home)
        .env("HOME", home)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("modron runs")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// A fake MCP server: appends one line per spawn to `marker`, then exits 1.
fn write_fake_server(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("fake-server.sh");
    std::fs::write(&script, "#!/bin/sh\necho spawned >> \"$MARKER\"\nexit 1\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn write_project_mcp(workspace: &Path, script: &Path, args: &[&str], marker: &Path) {
    let doc = serde_json::json!({
        "mcpServers": {
            "demo": {
                "command": script.display().to_string(),
                "args": args,
                "env": { "MARKER": marker.display().to_string() }
            }
        }
    });
    std::fs::write(workspace.join(".mcp.json"), doc.to_string()).unwrap();
}

fn spawn_count(marker: &Path) -> usize {
    std::fs::read_to_string(marker)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

#[test]
fn untrusted_project_server_is_not_spawned_until_trusted_and_changes_untrust() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    let home = root.path().join("home");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let script = write_fake_server(root.path());
    let marker = root.path().join("spawns.txt");
    write_project_mcp(&workspace, &script, &["v1"], &marker);

    // 1. Declared but untrusted: refused before any spawn.
    let out = modron(&workspace, &home, &["mcp", "info", "demo"]);
    assert!(!out.status.success(), "untrusted info must fail");
    assert!(stderr(&out).contains("not trusted"), "{}", stderr(&out));
    assert_eq!(spawn_count(&marker), 0, "untrusted server was spawned");

    // The list shows it as untrusted, with its source file.
    let list = stdout(&modron(&workspace, &home, &["mcp", "list"]));
    assert!(list.contains("demo"), "{list}");
    assert!(list.contains("untrusted"), "{list}");
    assert!(list.contains(".mcp.json"), "{list}");

    // 2. Trusted: the next start spawns it (the fake server then exits, so
    // `info` reports a connect failure, which is not a trust refusal).
    let trust = modron(&workspace, &home, &["mcp", "trust", "demo"]);
    assert!(trust.status.success(), "{}", stderr(&trust));
    assert!(
        stdout(&trust).contains("trusted `demo`"),
        "{}",
        stdout(&trust)
    );
    let out = modron(&workspace, &home, &["mcp", "info", "demo"]);
    assert!(!stderr(&out).contains("not trusted"), "{}", stderr(&out));
    assert_eq!(spawn_count(&marker), 1, "trusted server was not spawned");
    let list = stdout(&modron(&workspace, &home, &["mcp", "list"]));
    assert!(list.contains("trusted"), "{list}");

    // 3. Changed definition: trust no longer applies, nothing spawns.
    write_project_mcp(&workspace, &script, &["v2"], &marker);
    let out = modron(&workspace, &home, &["mcp", "info", "demo"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("not trusted"), "{}", stderr(&out));
    assert_eq!(
        spawn_count(&marker),
        1,
        "changed definition spawned without trust"
    );

    // 4. Trust the new definition, then revoke: back to untrusted.
    assert!(modron(&workspace, &home, &["mcp", "trust", "--all"])
        .status
        .success());
    let out = modron(&workspace, &home, &["mcp", "info", "demo"]);
    assert!(!stderr(&out).contains("not trusted"), "{}", stderr(&out));
    assert_eq!(spawn_count(&marker), 2);
    assert!(modron(&workspace, &home, &["mcp", "untrust"])
        .status
        .success());
    let out = modron(&workspace, &home, &["mcp", "info", "demo"]);
    assert!(stderr(&out).contains("not trusted"), "{}", stderr(&out));
    assert_eq!(spawn_count(&marker), 2, "revoked server was spawned");
}

#[test]
fn trust_needs_a_name_or_all_and_unknown_names_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    let home = root.path().join("home");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let script = write_fake_server(root.path());
    write_project_mcp(&workspace, &script, &["v1"], &root.path().join("m.txt"));

    let out = modron(&workspace, &home, &["mcp", "trust"]);
    assert!(!out.status.success());
    let out = modron(&workspace, &home, &["mcp", "trust", "nope"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no project MCP server named `nope`"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn doctor_reports_project_files_and_trust_state() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    let home = root.path().join("home");
    std::fs::create_dir_all(workspace.join(".cursor/rules")).unwrap();
    std::fs::create_dir_all(workspace.join(".claude/commands")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(workspace.join("GEMINI.md"), "be terse").unwrap();
    std::fs::write(workspace.join(".cursor/rules/style.mdc"), "use tabs").unwrap();
    std::fs::write(
        workspace.join(".claude/commands/triage.md"),
        "triage $ARGUMENTS",
    )
    .unwrap();
    let script = write_fake_server(root.path());
    write_project_mcp(&workspace, &script, &["v1"], &root.path().join("m.txt"));

    let out = modron(&workspace, &home, &["doctor"]);
    let text = stdout(&out);
    assert!(text.contains("project files:"), "{text}");
    assert!(text.contains("GEMINI.md"), "{text}");
    assert!(text.contains(".cursor/rules/style.mdc"), "{text}");
    assert!(text.contains("/triage"), "{text}");
    assert!(text.contains("mcp demo: untrusted"), "{text}");
}
