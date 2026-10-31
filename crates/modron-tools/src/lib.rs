//! modron-tools — built-in `Tool` implementations.
//!
//! LEDGER items: T1 (read/glob/grep/bash/write/edit), T2 (output-reduction
//! layer — every tool result passes through one choke point that caps and
//! elides before it enters model context), T3 (registry arg validation).
//!
//! Design notes:
//! - Every tool validates its arguments with [`validate_args`] before doing
//!   work; bad arguments produce `ToolOutcome::err`, never a panic.
//! - Every tool routes successful **and** error output through
//!   [`reduce_output`] (see [`reduce`]) before returning — the single
//!   choke point for token efficiency (PLAN.md §4.6).
//! - Expected failures (missing file, no grep matches, edit mismatch) are
//!   `ToolOutcome::err`; only "the tool could not run" cases (spawn failure,
//!   task-join failure) surface as `Result::Err`, per the frozen `Tool`
//!   contract.
//! - `glob`/`grep` walks are sync and run on tokio's blocking pool; file
//!   IO in the other tools is async. Blocking-in-async for tiny local
//!   metadata calls is accepted for MVP.

mod bash;
mod common;
mod delegate;
mod edit;
mod explore;
mod glob;
mod grep;
mod read;
pub mod reduce;
mod remember;
mod skill;
pub mod validate;
mod verify;
mod write;

pub use bash::BashTool;
pub use delegate::{DelegateStatusTool, DelegateTool};
pub use edit::EditTool;
pub use explore::ExploreTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use read::ReadTool;
pub use reduce::reduce_output;
pub use remember::RememberTool;
pub use skill::SkillTool;
pub use validate::validate_args;
pub use verify::VerifyTool;
pub use write::WriteTool;

use modron_core::ToolRegistry;
use std::path::Path;
use std::sync::Arc;

/// Registry containing the six built-in tools: `read`, `glob`, `grep`,
/// `bash`, `write`, `edit`.
pub fn default_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ReadTool));
    registry.register(Arc::new(GlobTool));
    registry.register(Arc::new(GrepTool));
    registry.register(Arc::new(BashTool));
    registry.register(Arc::new(WriteTool));
    registry.register(Arc::new(EditTool));
    registry
}

/// [`default_registry`] plus the `remember` tool wired to persist under
/// `memory_root` (e.g. `~/.modron/memory`). The plain [`default_registry`]
/// stays memory-free so memory-less runs and tests are unaffected.
pub fn default_registry_with_memory(memory_root: &Path) -> ToolRegistry {
    let mut registry = default_registry();
    registry.register(Arc::new(RememberTool::new(memory_root)));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_registry_has_six_tools() {
        let registry = default_registry();
        let names: Vec<String> = registry.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec!["bash", "edit", "glob", "grep", "read", "write"],
            "sorted spec names"
        );
    }

    #[test]
    fn default_registry_with_memory_adds_remember() {
        let registry =
            default_registry_with_memory(std::path::Path::new("/tmp/modron-test-memory"));
        let names: Vec<String> = registry.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec!["bash", "edit", "glob", "grep", "read", "remember", "write"],
            "sorted spec names"
        );
    }

    #[tokio::test]
    async fn registry_dispatch_rejects_bad_args_as_error_outcome() {
        // T3 acceptance: bad args -> an error ToolOutcome with a
        // helpful message, through the registry.
        let registry = default_registry();
        let dir = tempfile::tempdir().unwrap();
        let ctx = modron_core::ToolCtx {
            engine: None,
            sandbox: modron_core::SandboxPolicy::Off,
            working_dir: dir.path().to_path_buf(),
        };
        let tool = registry.get("read").expect("read tool registered");
        let outcome = tool
            .execute(json!({"path": "f.txt", "bogus": true}), &ctx)
            .await
            .expect("execute");
        assert!(outcome.is_error, "{outcome:?}");
        assert!(
            outcome.content.contains("unknown argument `bogus`"),
            "{outcome:?}"
        );
    }
}
