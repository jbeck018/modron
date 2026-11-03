---
name: ai-slop-cleaner
description: Regression-tests-first refactor to remove AI-generated slop — lock behavior, categorize smells, ordered cleanup passes, quality gates, evidence report. Use when a codebase reads machine-generated and needs tightening.
---

# AI Slop Cleaner

Adapted from the OMX/jcode ai-slop-cleaner skill; behavior-locking first.

## Iron laws

- Lock behavior BEFORE cleanup: write or identify regression tests that
  fail if the refactor changes behavior. No tests, no cleanup.
- Bound the work to an explicit file list. Cleanup that sprawls is how
  refactors break things.
- Every pass must end green (tests + project verify command).

## Smell categories

- **Duplication**: near-identical logic in multiple places.
- **Dead code**: unused functions, stale comments describing removed
  behavior, unreachable branches, commented-out code.
- **Needless abstraction**: one-impl traits, pass-through wrappers,
  indirection that adds no seam.
- **Boundary violations**: business logic in I/O layers, secrets handling
  scattered, module reaching into another's internals.
- **Missing tests**: changed behavior with no coverage.

## Ordered passes (re-verify after each)

1. **Dead code** — delete. Lowest risk, shrinks everything after it.
2. **Duplication** — extract to one place; prefer deletion over
   abstraction when unifying.
3. **Naming and errors** — names that say what it does; error messages
   that name the fix. No renaming churn for taste alone.
4. **Test reinforcement** — add the tests the refactor just proved
   missing.

## Output format

End with an "AI SLOP CLEANUP REPORT":
- files changed (the bounded list, no others)
- per smell category: what was removed/renamed, with file:line
- test evidence: suite green before and after (name the command)
- residual smells deliberately left (with reasons)

## Rules

- Never `git add -A`; stage exactly the files in the list.
- Do not fix bugs you find along the way — note them and leave them;
  mixing fixes into the refactor destroys the behavior-lock.
- If a pass makes tests fail and the fix is not obvious in 5 minutes,
  revert that pass and report it as a finding.
