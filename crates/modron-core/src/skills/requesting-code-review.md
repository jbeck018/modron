---
name: requesting-code-review
description: Use when completing tasks or major features, or before merging
---

# Requesting Code Review

Generate a review package and dispatch a FRESH reviewer (a `delegate`
that has not seen the work): fresh eyes find what author-eyes cannot.

## The package (give the reviewer, not the codebase)

- The task/spec: what was asked.
- The diff (or branch + commits).
- What to check: spec compliance, correctness, edge cases, error
  handling, naming, tests that actually assert the behavior.
- What NOT to check (explicitly out of scope).

## Rules

- Reviewers get context, never your conclusions about your own code.
- Findings are specific: file:line, what's wrong, why, suggested fix.
- Receiving findings -> `receiving-code-review`.

Adapted from Jesse Vincent's superpowers (MIT): github.com/obra/superpowers
