---
name: finishing-a-development-branch
description: Use when implementation is complete, tests pass, and the work needs integrating
---

# Finishing a Development Branch

1. **Verify:** full suite green on this branch, working tree clean,
   every plan task `done` or explicitly parked in the ledger.
2. **Final review:** one whole-branch review round
   (`requesting-code-review`) - integration gaps, dead code, docs.
3. **Present options to the partner** (their call, not yours):
   - merge locally (worktree: `merge_worktree`; branch: `git merge
     --no-ff`) and delete the feature branch,
   - push and open a PR,
   - keep the branch for later.
4. **Cleanup:** after an integration decision, remove the worktree
   (`git worktree remove`) and the plan's `.modron/sdd/<slug>/`
   workspace; keep the plan document.

Never merge, push, or delete a shared resource without the partner's
explicit choice. Record the decision in the ledger before cleanup.

Adapted from Jesse Vincent's superpowers (MIT): github.com/obra/superpowers
