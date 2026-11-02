---
name: using-git-worktrees
description: Use when starting feature work that needs isolation, or before executing implementation plans
---

# Using Git Worktrees

Isolated workspace before implementation - never build on main without
your partner's explicit consent.

## In modron

- Dispatching an implementing worker: `delegate {"isolate": true, ...}`
  creates `.modron/worktrees/<id>` automatically.
- Merging back: the `merge_worktree` tool - conflicts ABORT cleanly for
  review; nothing is force-resolved. Merge/push decisions belong to the
  partner.
- Leading the work yourself: create the worktree with
  `git worktree add .modron/worktrees/<slug> -b <branch>` and work
  there.

## Rules

- One plan per worktree; the ledger lives with the plan (see
  `executing-plans`).
- Non-git directory: say so and work in place with checkpoints
  (`modron rewind` is the safety net), noting the limitation.
- When done: `finishing-a-development-branch`.

Adapted from Jesse Vincent's superpowers (MIT): github.com/obra/superpowers
