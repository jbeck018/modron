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
mod diagnostics;
mod edit;
mod explore;
mod glob;
mod grep;
mod interact;
mod lsp;
mod memory_search;
mod minify;
mod plan;
mod read;
pub mod reduce;
mod remember;
mod skill;
mod tool_search;
pub mod validate;
mod verify;
mod write;

pub use bash::BashTool;
pub use delegate::{DelegateStatusTool, DelegateTool, MergeWorktreeTool};
pub use edit::EditTool;
pub use explore::ExploreTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use interact::InteractTool;
pub use plan::PlanTool;
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
    // A user interrupt must also stop detached background delegates.
    modron_core::interrupt::on_interrupt(crate::delegate::abort_background_delegates);
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
    registry.register(Arc::new(crate::memory_search::MemorySearchTool::new(
        memory_root,
    )));
    registry.register(Arc::new(crate::minify::ReadMinifiedTool));
    registry.register(Arc::new(crate::tool_search::ToolSearchTool));
    registry.register(Arc::new(crate::diagnostics::DiagnosticsTool));
    registry.register(Arc::new(crate::interact::InteractTool::new()));
    registry
}

/// Tools that stay in the model-visible schema. Harness research (2026:
/// Harness-Bench, oh-my-pi) is unambiguous: top harnesses pin a SMALL
/// deliberate tool surface (~14) and mount the rest on demand — every
/// hidden schema is tokens paid on every request, and `tool_search`
/// keeps the rest discoverable. `delegate` is essential (2026-11-11):
/// the model must see it to auto-delegate — the oh-my-pi pattern of
/// spinning off background/isolated subtasks unprompted.
pub const ESSENTIAL_TOOLS: [&str; 14] = [
    "read",
    "write",
    "edit",
    "bash",
    "grep",
    "glob",
    "tool_search",
    "skill",
    "memory_search",
    "diagnostics",
    "verify",
    "interact",
    "delegate",
    "plan",
];

/// Demote every registered tool not in [`ESSENTIAL_TOOLS`] to on-demand
/// (hidden from the prompt schema; still callable and discoverable via
/// `tool_search`). Applied once, after the production registry is fully
/// assembled — MCP tools (already on-demand) are unaffected.
pub fn tier_default_tools(registry: &mut ToolRegistry) {
    let hide: Vec<String> = registry
        .specs()
        .into_iter()
        .filter(|s| !ESSENTIAL_TOOLS.contains(&s.name.as_str()))
        .filter(|s| !registry.is_on_demand(&s.name))
        .map(|s| s.name)
        .collect();
    for name in hide {
        registry.mark_on_demand(&name);
    }
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
    fn tiering_keeps_only_essential_tools_in_the_prompt() {
        let mut registry = default_registry_with_memory(Path::new("/tmp/modron-test-memory"));
        tier_default_tools(&mut registry);
        let prompt = registry.specs_prompt();
        let names: Vec<&str> = prompt.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"remember"),
            "on-demand tools stay declared: {names:?}"
        );
        for spec in &prompt {
            let essential = ESSENTIAL_TOOLS.contains(&spec.name.as_str());
            assert_eq!(
                essential,
                !spec.description.contains("on-demand tool"),
                "{}: essential tools keep full schemas, others get stubs",
                spec.name
            );
        }
        for hidden in ["remember", "read_minified"] {
            assert!(
                registry.specs().iter().any(|s| s.name == hidden),
                "{hidden} stays registered"
            );
            assert!(registry.is_on_demand(hidden), "{hidden} is on-demand");
        }
    }

    #[test]
    fn default_registry_with_memory_adds_remember() {
        let registry =
            default_registry_with_memory(std::path::Path::new("/tmp/modron-test-memory"));
        let names: Vec<String> = registry.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec![
                "bash",
                "diagnostics",
                "edit",
                "glob",
                "grep",
                "interact",
                "memory_search",
                "read",
                "read_minified",
                "remember",
                "tool_search",
                "write"
            ],
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
            session_id: None,
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
