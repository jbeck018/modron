//! D4 — the prompt-budget CI gate: the real default registry's tool specs +
//! assembled system prompt must stay under 2048 estimated tokens. This is
//! the forcing function behind PLAN.md §3.1 ("token budget is a CI metric"):
//! any change that fattens the prompt or adds chatty tool descriptions
//! fails the build instead of silently taxing every turn.

use modron_core::{context::build_system_prompt, estimate_tokens};
use modron_tools::{default_registry_with_memory, VerifyTool};
use std::sync::Arc;

const BUDGET: u64 = 2048;

fn registry_with_verify() -> modron_core::ToolRegistry {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = default_registry_with_memory(dir.path());
    registry.register(Arc::new(VerifyTool::new("cargo test --quiet")));
    registry
}

#[test]
fn default_registry_system_prompt_stays_under_budget() {
    let dir = tempfile::tempdir().unwrap();
    let registry = default_registry_with_memory(dir.path());
    let specs = registry.specs();
    let system = build_system_prompt(&specs, "");
    let spent = estimate_tokens(&system);
    assert!(
        spent < BUDGET,
        "system prompt + tool specs cost {spent} est. tokens (budget {BUDGET}) — \
         shorten descriptions or raise the budget deliberately"
    );
}

#[test]
fn with_verify_tool_registered_still_under_budget() {
    let registry = registry_with_verify();
    let specs = registry.specs();
    let system = build_system_prompt(&specs, "");
    let spent = estimate_tokens(&system);
    assert!(
        spent < BUDGET,
        "with verify: {spent} est. tokens (budget {BUDGET})"
    );
}

#[test]
fn typical_memory_section_still_under_budget() {
    let registry = registry_with_verify();
    let specs = registry.specs();
    // A realistic user-memory payload (index + a few entries).
    let memory = "## Memory\n- 2026-10-31 prefers small diffs\n- 2026-10-31 uses pnpm not npm\n\
                  - 2026-10-31 CI runs on self-hosted runners\n- 2026-10-31 main branch is protected\n";
    let system = build_system_prompt(&specs, memory);
    let spent = estimate_tokens(&system);
    assert!(
        spent < BUDGET,
        "with memory: {spent} est. tokens (budget {BUDGET})"
    );
}

#[test]
fn verify_tool_description_is_terse() {
    let registry = registry_with_verify();
    let spec = registry
        .specs()
        .into_iter()
        .find(|s| s.name == "verify")
        .expect("verify registered");
    assert!(
        spec.description.len() < 200,
        "verify spec must stay terse: {}",
        spec.description
    );
}
