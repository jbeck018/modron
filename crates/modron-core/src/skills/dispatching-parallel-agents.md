---
name: dispatching-parallel-agents
description: Use when facing 2+ independent tasks with no shared state or sequential dependencies
---

# Dispatching Parallel Agents

Independent tasks run CONCURRENTLY - one delegate each, dispatched in
the same turn (modron runs same-turn non-mutating calls in parallel;
mutating work gets `isolate` worktrees).

## When parallel is right

- No task reads what another writes; no ordering between them.
- Each brief is complete on its own (see
  `subagent-driven-development` for brief quality).
- A shared resource (one file, one port, one index) serializes them?
  That's a dependency - run them in sequence or re-split the tasks.

## Rules

- Dispatch ALL workers first, then collect (background delegates +
  `delegate_status`, or same-turn parallel calls).
- Verify each result independently (`verification-before-completion`);
- One fails: others' results still stand - fix or redispatch only the
  failure.
- Record per-task outcomes in the ledger.

Adapted from Jesse Vincent's superpowers (MIT): github.com/obra/superpowers
