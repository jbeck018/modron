---
name: systematic-debugging
description: Use when encountering any bug, test failure, or unexpected behavior, before proposing fixes
---

# Systematic Debugging

**The Iron Law: NO FIXES WITHOUT ROOT-CAUSE INVESTIGATION FIRST.**
Symptom patches are failure. This applies ESPECIALLY under time
pressure, when the fix "seems obvious", and after your previous fix
didn't work - those are exactly when guessing multiplies rework.

## Phase 1 - Root cause (no fixes allowed)

1. **Read the error.** The whole message, the actual failing line.
2. **Reproduce** minimally and reliably; note the exact trigger.
3. **Locate** the failing code (read/grep/diagnostics).
4. **Trace** the wrong value back to its origin - bisect the chain
   until you can say: "the bug is HERE, because THIS assumption is
   false, and here is the evidence."
5. **Hypothesis:** one sentence, falsifiable, with a check that
   confirms or kills it.

## Phase 2 - Fix

Fix the ROOT CAUSE (not the symptom layer). Add the failing test that
would have caught it (see `test-driven-development`). Run the full
suite. Only then consider the bug closed.

## Rules

- Changed three things to fix one? Revert and change one at a time -
  you don't know which worked.
- "It works now but I don't know why" = not done; it will regress.
- Record findings in the ledger when working a plan.

Adapted from Jesse Vincent's superpowers (MIT): github.com/obra/superpowers
