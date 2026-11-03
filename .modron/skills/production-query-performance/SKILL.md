---
name: production-query-performance
description: Evidence-driven remediation of slow production queries/operations — inventory, reproduce, classify (N+1, sort/spill, pool contention), design, validate, deploy, observe. Evidence before edits, never guess at indexes.
---

# Production Query Performance

Adapted from the OMX/jcode production-query-performance skill; its 12
rules compress to the non-negotiables below.

## Non-negotiable rules

- Never guess at an index or a rewrite: reproduce the slow operation
  first and read its plan.
- Never manufacture a favorable result (no timing the warm cache twice,
  no shrinking the dataset to make a win).
- Protect credentials: read them from the environment; never paste them
  into queries, plans, or reports.
- DDL against production is reversible-or-die: migrations must have a
  documented rollback.

## Per-operation loop

1. **Inventory**: which operation is slow, from where, how often, at
   what data size. Rank by impact.
2. **Map**: the exact query/endpoint, its callers, its write path.
3. **Reproduce**: run it at production-shaped data/volume and capture
   the plan (`EXPLAIN ANALYZE`, profiler, trace).
4. **Classify**: N+1, missing index, sort/spill to disk, lock/pool
   contention, chatty round-trips, oversized payloads, stale cache.
5. **Design**: one hypothesis, one change, predicted effect.
6. **Validate**: measure after the change at the SAME shape/volume.
   No improvement = revert, re-classify.
7. **Review** and **deploy** (rollback attached).
8. **Observe** in production for a full traffic cycle (peak included).
9. **Disposition**: fixed / partially improved (numbers) / structural
   (documented).

## Completion gate

Done means post-deploy production observation shows the improvement —
not a merged PR, not a staging run.

## Rules

- One operation at a time through the full loop; batching them makes
  attribution impossible.
- Keep a run ledger (file: hypothesis, change, before, after, verdict)
  so a later regression has a comparison point.
