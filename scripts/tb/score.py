#!/usr/bin/env python3
"""Scorecard across tb runs: per-task resolve rate, tokens, failure modes.

Usage: python3 scripts/tb/score.py [runs-dir ...]   (default: runs/)
Reads runs/<ts>/<task>/<trial>/results.json.
"""

import glob
import json
import sys
from collections import defaultdict

rows = defaultdict(lambda: {"attempts": 0, "resolved": 0, "in_tok": 0,
                            "out_tok": 0, "failures": defaultdict(int),
                            "runs": set()})
for path in glob.glob("runs/*/*/*/results.json"):
    try:
        d = json.load(open(path))
    except (json.JSONDecodeError, OSError):
        continue
    task = d.get("task_id", "?")
    r = rows[task]
    r["attempts"] += 1
    r["resolved"] += bool(d.get("is_resolved"))
    r["in_tok"] += d.get("total_input_tokens") or 0
    r["out_tok"] += d.get("total_output_tokens") or 0
    fm = d.get("failure_mode") or ("resolved" if d.get("is_resolved") else "?")
    if not d.get("is_resolved"):
        r["failures"][fm] += 1
    r["runs"].add(path.split("/")[1])

total_a = total_r = total_in = total_out = 0
print(f"{'task':34} {'att':>3} {'ok':>3} {'rate':>6} {'in_tok':>9} "
      f"{'out_tok':>8}  failures")
for task in sorted(rows):
    r = rows[task]
    total_a += r["attempts"]; total_r += r["resolved"]
    total_in += r["in_tok"]; total_out += r["out_tok"]
    fails = ",".join(f"{k}x{v}" for k, v in sorted(r["failures"].items()))
    rate = f"{100.0 * r['resolved'] / r['attempts']:.0f}%"
    print(f"{task:34} {r['attempts']:>3} {r['resolved']:>3} {rate:>6} "
          f"{r['in_tok']:>9} {r['out_tok']:>8}  {fails}")
if total_a:
    print(f"{'TOTAL':34} {total_a:>3} {total_r:>3} "
          f"{100.0 * total_r / total_a:>5.0f}% {total_in:>9} {total_out:>8}")
