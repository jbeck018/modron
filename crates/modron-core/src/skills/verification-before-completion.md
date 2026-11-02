---
name: verification-before-completion
description: Use before claiming work is complete, fixed, or passing - evidence before assertions, always
---

# Verification Before Completion

Never claim success without RUNNING the verification and reading its
output in THIS turn. "The tests should pass" is not verification; a
green run you watched is.

## Before saying done/fixed/passing

1. Run the actual command (test suite, `diagnostics`, the project's
   verify command, the app itself).
2. Read the output - exit code AND body. A command that errored while
   you summarized it as passing is the cardinal failure.
3. If verification covers only part of the claim, say exactly which
   part: "tests pass; I did not run the app manually."

## Language rules

- "I did X and verified Y" only with evidence in hand.
- Unknowns get stated as unknowns - not glossed, not assumed.
- A partial claim honestly scoped beats a complete claim you can't back.

The partner will trust "verified: [output]" forever, and "I believe"
exactly once. Spend the one run that earns the first.

Adapted from Jesse Vincent's superpowers (MIT): github.com/obra/superpowers
