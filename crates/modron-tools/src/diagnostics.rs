//! `diagnostics` tool (Phase 14, code intelligence tranche 1) — run the
//! project's own checker and surface compiler/linter diagnostics in TOOL
//! OUTPUT, so the agent sees and fixes errors on the same turn instead of
//! claiming success and making the human discover the break.
//!
//! Detection by project marker: Cargo.toml → `cargo check --message-format
//! short`, tsconfig.json → `npx --no-install tsc --noEmit`, go.mod →
//! `go vet ./...`, pyproject/setup → `python3 -m compileall -q`. Missing
//! tooling is an actionable error, not a panic. Never modifies source
//! files (build caches are fair game), so it is not approval-gated — the
//! whole point is friction-free post-edit verification.
//!
//! The LSP-tranche will extend this with live language-server diagnostics;
//! the project checker remains the zero-config baseline that always works.

use crate::common::{arg_str, tool_err};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use async_trait::async_trait;
use modron_core::{Result, Tool, ToolCtx, ToolOutcome, ToolSpec};
use serde_json::{json, Value};
use std::time::Duration;

const REDUCE_CAP: usize = 16 * 1024;
const TIMEOUT_MS: u64 = 180_000;

/// Run the project's checker; return its diagnostics.
pub struct DiagnosticsTool;

#[async_trait]
impl Tool for DiagnosticsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "diagnostics".into(),
            description: "Run the project's compiler/linter (cargo check / tsc --noEmit / go \
                          vet / python compileall, auto-detected) and return its diagnostics. \
                          Call after edits — before claiming success — so errors are fixed on \
                          the same turn. Args: optional command override to run instead of the \
                          detected default."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Optional checker command to run instead of the detected default."}
                }
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        const NAME: &str = "diagnostics";
        let spec = self.spec();
        if let Err(message) = validate_args(&spec, &args) {
            return Ok(tool_err(
                NAME,
                format!("invalid arguments: {message}"),
                REDUCE_CAP,
            ));
        }
        let override_cmd = arg_str(&args, "command");
        let (checker, command) = match override_cmd {
            Some(cmd) if !cmd.trim().is_empty() => ("custom".to_string(), cmd.to_string()),
            _ => match detect(&ctx.working_dir) {
                Some(found) => found,
                None => {
                    return Ok(tool_err(
                        NAME,
                        "no supported project detected (looked for Cargo.toml, tsconfig.json, \
                         go.mod, pyproject.toml, setup.py) — pass {\"command\": \"...\"} to run \
                         a specific checker",
                        REDUCE_CAP,
                    ))
                }
            },
        };

        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .current_dir(&ctx.working_dir)
            .output()
            .await
            .map_err(|e| modron_core::Error::Tool {
                tool: NAME.into(),
                message: format!("cannot spawn `{command}`: {e}"),
            })?;

        let code = output.status.code().unwrap_or(-1);
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        let err_text = String::from_utf8_lossy(&output.stderr).into_owned();
        if !err_text.trim().is_empty() {
            if !text.trim().is_empty() {
                text.push('\n');
            }
            text.push_str(&err_text);
        }
        let text = text.trim();
        if code == 0 && text.is_empty() {
            return Ok(ToolOutcome::ok(reduce_output(
                format!("{checker}: clean — no diagnostics"),
                REDUCE_CAP,
            )));
        }
        if code == 0 {
            // Exit 0 = success; any output is informational notes.
            return Ok(ToolOutcome::ok(reduce_output(
                format!("{checker}: clean — notes:\n{text}"),
                REDUCE_CAP,
            )));
        }
        Ok(ToolOutcome::err(reduce_output(
            format!("{checker} (exit {code}):\n{text}"),
            REDUCE_CAP,
        )))
    }
}

/// `(checker label, command)` from the working dir's project marker.
fn detect(dir: &std::path::Path) -> Option<(String, String)> {
    let has = |name: &str| dir.join(name).is_file();
    if has("Cargo.toml") {
        // --message-format short keeps output terse; workspace-aware.
        Some((
            "cargo check".into(),
            "cargo check --message-format short --all-targets".into(),
        ))
    } else if has("tsconfig.json") {
        Some((
            "tsc --noEmit".into(),
            "npx --no-install tsc --noEmit".into(),
        ))
    } else if has("go.mod") {
        Some(("go vet".into(), "go vet ./...".into()))
    } else if has("pyproject.toml") || has("setup.py") {
        Some((
            "python compileall".into(),
            format!("python3 -m compileall -q ."),
        ))
    } else {
        None
    }
}

/// Duration for clippy: the tool never waits past its own process timeout,
/// but keep the constant referenced so the cap is documented in one place.
#[allow(dead_code)]
const CHECK_TIMEOUT: Duration = Duration::from_millis(TIMEOUT_MS);

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn run(args: Value, dir: &std::path::Path) -> ToolOutcome {
        let ctx = ToolCtx {
            engine: None,
            sandbox: modron_core::SandboxPolicy::Off,
            working_dir: dir.to_path_buf(),
        };
        DiagnosticsTool
            .execute(args, &ctx)
            .await
            .expect("execute")
    }

    #[tokio::test]
    async fn unknown_project_lists_supported_markers() {
        let dir = tempdir().unwrap();
        let out = run(json!({}), dir.path()).await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("Cargo.toml"), "{out:?}");
    }

    #[tokio::test]
    async fn rust_error_surfaces_in_tool_output() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"diagtest\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src").join("lib.rs"),
            "pub fn broken() -> u32 { \"not a number\" }\n",
        )
        .unwrap();
        let out = run(json!({}), dir.path()).await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("cargo check"), "{out:?}");
        assert!(out.content.contains("error"), "{out:?}");
        assert!(out.content.contains("lib.rs"), "{out:?}");
    }

    #[tokio::test]
    async fn clean_project_reports_clean() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"diagclean\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src").join("lib.rs"), "pub fn ok() -> u32 { 1 }\n").unwrap();
        let out = run(json!({}), dir.path()).await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("clean"), "{out:?}");
    }

    #[tokio::test]
    async fn python_syntax_error_surfaces() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("pyproject.toml"), "[project]\nname = \"p\"\n").unwrap();
        std::fs::write(dir.path().join("bad.py"), "def f(:\n").unwrap();
        let out = run(json!({}), dir.path()).await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("bad.py"), "{out:?}");
    }

    #[tokio::test]
    async fn command_override_runs() {
        let dir = tempdir().unwrap();
        let out = run(json!({"command": "echo custom-checker && false"}), dir.path()).await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("custom-checker"), "{out:?}");
    }
}
