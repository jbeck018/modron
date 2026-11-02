---
name: subagent-driven-development
description: Use when executing implementation plans with independent tasks - leader dispatches fresh worker delegates per task, with per-task review
---

# Subagent-Driven Development (leader/worker)

Fresh implementer delegate per task + a task review after each + one
broad final review = high quality, fast iteration. You are the LEADER:
you never implement; you dispatch, review, and keep the ledger. Workers
never inherit your context - their `delegate` task text IS their world;
craft it precisely (files, signatures, the exact test, the acceptance
check, what NOT to touch).

**Narration:** one short line between dispatches; the ledger carries
the record. **Continuous execution:** never ask "should I continue?" -
see `executing-plans` for rulings-not-stalls and the only four stops.

## Setup

1. Isolated workspace: `delegate {"isolate": true}` worktree (see
   `using-git-worktrees`) or the current worktree per the plan.
2. Ledger at `.modron/sdd/<plan-slug>/progress.md` (see
   `executing-plans` for the format). If one exists, resume - never
   re-dispatch completed tasks.
3. Read the plan; pre-flight it (missing files? ambiguous tasks ->
   rulings now).

## Per task loop

1. **Dispatch the implementer** (foreground or `background`):
   the task brief + exact context (files to read, interface, the
   failing test to write first, run tests, commit).
2. Worker asks questions? Answer with context, not code.
3. **Dispatch a fresh reviewer** delegate with the task brief + the
   diff: check spec compliance AND code quality (bugs, edge cases,
   naming, tests that actually test).
4. Review fails -> fix loop: fresh implementer delegate per round with
   the review findings, capped at 3 rounds; record `Task N: parked at
   cap — <findings>` in the ledger and move on.
5. Append `Task N: done — <line>` to the ledger. Next task.

## Finish

Final whole-branch review delegate (spec coverage + integration), then
`finishing-a-development-branch`.

Adapted from Jesse Vincent's superpowers (MIT): github.com/obra/superpowers
