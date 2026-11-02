---
name: test-driven-development
description: Use when implementing any feature or bugfix, before writing implementation code
---

# Test-Driven Development

**The Iron Law: NO PRODUCTION CODE WITHOUT A FAILING TEST FIRST.**

Wrote code before the test? Delete it - not "keep as reference", not
"adapt it": delete means delete - and implement fresh from the test.

## The cycle

1. **RED:** write the failing test. Run it. WATCH it fail for the
   reason you expect - if you didn't watch it fail, you don't know it
   tests the right thing.
2. **GREEN:** the minimal code that passes. Minimal means minimal -
   no "while I'm here".
3.Commit with a message naming the behavior.
4. **REFACTOR** only with the suite green.

## Discipline

- One behavior per cycle; small steps beat big leaps.
- Exceptions (throwaway prototypes, generated code, pure config) are
  the PARTNER's call, not yours. Thinking "skip TDD just this once"?
  That's the rationalization; use it as the signal to slow down.
- Bug fix = first write the test that reproduces the bug, watch it
  fail, then fix.

Adapted from Jesse Vincent's superpowers (MIT): github.com/obra/superpowers
