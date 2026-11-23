#!/usr/bin/env python3
"""Side-by-side table for a scripts/tb/compare.sh run.

    compare.py <output dir> [agent ...]

Reads every finished trial under <output dir>/<agent>/<task>/<trial>/, so
it also works on a run that is still going. Trials whose agent never
started (image build or container failure) are left out. Per trial it takes the verdict
and timestamps from the harness's results.json, and requests and tokens
from the relay row named by the trial's tag (agent-logs/trial.json), read
from the relay when it is up and from relay-stats.json otherwise. Writes
compare.md and compare.json next to the runs and prints the table.
"""

from __future__ import annotations

import json
import os
import sys
import urllib.request
from datetime import datetime
from pathlib import Path


def load_relay(out: Path) -> dict:
    url = os.environ.get("BENCH_RELAY_HOST_URL", "http://127.0.0.1:8787")
    try:
        with urllib.request.urlopen(f"{url}/stats", timeout=5) as resp:
            return json.loads(resp.read())
    except Exception:
        pass
    try:
        return json.loads((out / "relay-stats.json").read_text())
    except Exception:
        return {}


def seconds(start: str | None, end: str | None) -> float | None:
    if not start or not end:
        return None
    return (datetime.fromisoformat(end) - datetime.fromisoformat(start)).total_seconds()


def trials(out: Path, agent: str, relay: dict) -> list[dict]:
    rows = []
    for path in sorted((out / agent).glob("*/*/results.json")):
        try:
            result = json.loads(path.read_text())
        except Exception:
            continue
        tag = ""
        try:
            tag = json.loads((path.parent / "agent-logs" / "trial.json").read_text())["tag"]
        except Exception:
            pass
        if not tag:
            # The agent never started (the task image failed to build or
            # the container did not come up). That is the harness, not the
            # agent, so the trial is left out rather than scored as a fail.
            continue
        usage = relay.get(tag, {})
        rows.append(
            {
                "task": result.get("task_id", path.parent.parent.name),
                "resolved": bool(result.get("is_resolved")),
                "failure_mode": result.get("failure_mode"),
                "agent_seconds": seconds(
                    result.get("agent_started_at"), result.get("agent_ended_at")
                ),
                "requests": usage.get("requests"),
                "input": usage.get("input"),
                "output": usage.get("output"),
                "cached": usage.get("cached"),
                "models": usage.get("models", {}),
                "tag": tag,
            }
        )
    return rows


def mean(values: list) -> float | None:
    values = [value for value in values if value is not None]
    return sum(values) / len(values) if values else None


def cell(value: float | None, unit: str = "") -> str:
    if value is None:
        return "-"
    if unit == "k":
        return f"{value / 1000:.0f}k"
    return f"{value:.0f}{unit}"


def summarize(rows: list[dict]) -> dict:
    return {
        "attempts": len(rows),
        "resolved": sum(row["resolved"] for row in rows),
        "agent_seconds": mean([row["agent_seconds"] for row in rows]),
        "requests": mean([row["requests"] for row in rows]),
        "input": mean([row["input"] for row in rows]),
        "output": mean([row["output"] for row in rows]),
        "cached": mean([row["cached"] for row in rows]),
    }


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    out = Path(sys.argv[1])
    agents = sys.argv[2:] or sorted(
        path.name for path in out.iterdir() if any(path.glob("*/*/results.json"))
    )
    relay = load_relay(out)
    data = {agent: trials(out, agent, relay) for agent in agents}
    tasks = sorted({row["task"] for rows in data.values() for row in rows})

    head = ["task"]
    for agent in agents:
        head += [f"{agent} pass", "sec", "reqs", "in", "out"]
    lines = ["| " + " | ".join(head) + " |", "|" + "---|" * len(head)]
    report: dict = {"tasks": {}, "totals": {}, "models": {}}
    for task in tasks:
        line = [task]
        report["tasks"][task] = {}
        for agent in agents:
            stats = summarize([row for row in data[agent] if row["task"] == task])
            report["tasks"][task][agent] = stats
            line += [
                f"{stats['resolved']}/{stats['attempts']}",
                cell(stats["agent_seconds"]),
                cell(stats["requests"]),
                cell(stats["input"], "k"),
                cell(stats["output"], "k"),
            ]
        lines.append("| " + " | ".join(line) + " |")

    # Totals over tasks BOTH agents attempted, so a half-finished run does
    # not compare different task sets.
    shared = [
        task
        for task in tasks
        if all(report["tasks"][task][agent]["attempts"] for agent in agents)
    ]
    line = [f"**all ({len(shared)} tasks)**"]
    for agent in agents:
        rows = [row for row in data[agent] if row["task"] in shared]
        stats = summarize(rows)
        report["totals"][agent] = stats
        models: dict = {}
        for row in rows:
            for model, count in row["models"].items():
                models[model] = models.get(model, 0) + count
        report["models"][agent] = models
        rate = 100 * stats["resolved"] / stats["attempts"] if stats["attempts"] else 0
        line += [
            f"**{stats['resolved']}/{stats['attempts']} ({rate:.0f}%)**",
            cell(stats["agent_seconds"]),
            cell(stats["requests"]),
            cell(stats["input"], "k"),
            cell(stats["output"], "k"),
        ]
    lines.append("| " + " | ".join(line) + " |")
    lines.append("")
    lines.append(
        "pass = resolved/attempts; sec, reqs, in, out = mean per trial of agent "
        "wall time, model requests, prompt tokens (cached included) and output "
        "tokens, as counted by the relay."
    )
    for agent in agents:
        lines.append(f"models called by {agent}: {report['models'][agent] or 'none recorded'}")

    text = "\n".join(lines)
    (out / "compare.md").write_text(text + "\n")
    (out / "compare.json").write_text(json.dumps(report, indent=1))
    print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
