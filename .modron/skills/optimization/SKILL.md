---
name: optimization
description: Metrics-first performance work — define the target metric, attribute the bottleneck with evidence, macro before micro. Use when asked to make anything faster or cheaper.
---

# Optimization

Ported from jcode's repo-shipped skill; methodology is tool-free.

## Iron laws

- Never claim an optimization without a before/after measurement of the
  target metric.
- Define the metric BEFORE touching code (latency p50/p99, throughput,
  allocations, tokens, bytes on the wire — pick the one the user cares
  about and write down the baseline).
- Attribute the bottleneck with evidence (instrument, time, count) before
  choosing a fix. Guessing is the most expensive step to get wrong.

## Process

1. **Define**: what metric, what target, what workload. Write all three
   down before the first edit.
2. **Baseline**: reproduce the workload and measure. If you cannot
   measure it, you cannot optimize it — build the measurement first.
3. **Attribute**: find where the time/tokens/bytes actually go. Prefer
   static analysis first: asymptotics (is this O(n^2) in something that
   grew?), wrong layer (parsing in a loop, N+1 queries, re-reading files),
   redundant work that can be deleted wholesale.
4. **Macro before micro**: remove classes of work (cache, batch, hoist,
   delete) before tuning constants. A 2x from an algorithm change beats a
   5% from a micro-tweak every time.
5. **Fix ONE thing, re-measure.** If the metric did not move, revert and
   re-attribute — the bottleneck was not where you thought.
6. **Repeat** until the target is met or the remaining costs are
   structural (document which and stop).

## Rules

- Keep every intermediate state green: `cargo test` (or the project's
  verify command) after each change.
- Prefer deletes over adds; prefer making the common path cheaper over
  making the rare path faster.
- Report at the end: baseline -> final numbers for the metric, what the
  bottleneck turned out to be, and what you deliberately did NOT do.
