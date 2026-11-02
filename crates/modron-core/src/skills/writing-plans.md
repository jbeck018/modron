---
name: writing-plans
description: Use when you have a spec or requirements for a multi-step task, before touching code
---

# Writing Plans

Write the plan for an engineer (or a fresh delegate) who has NOT seen
this codebase: they write idiomatic code once told the exact files,
names, signatures, and the exact test that proves each task. What they
cannot know is what you decided - document those decisions.

Save plans to `.modron/plans/YYYY-MM-DD-<slug>.md`.

## Structure

1. **Header:** one-paragraph goal + the spec it implements.
2. **File structure:** which files are created/modified and what each is
   responsible for (this is where decomposition gets locked in). Prefer
   small focused files; follow the codebase's existing patterns.
3. **Tasks:** bite-sized, each independently testable and committable.
   Fold setup/config/docs into the task that needs them; split only
   where a reviewer could reject one task while approving its neighbor.
4. **Steps:** one action with a checkable result each -
   write the failing test / run it and watch it fail / minimal
   implementation / run tests / commit.

## Rules

- DRY, YAGNI, TDD, frequent commits.
- Every task names its files, its exact test, and its acceptance check.
- Multiple independent subsystems in one spec? Propose separate plans.
- End with a handoff note: recommend `subagent-driven-development` for
  independent tasks, `executing-plans` for tightly-coupled ones.

Adapted from Jesse Vincent's superpowers (MIT): github.com/obra/superpowers
