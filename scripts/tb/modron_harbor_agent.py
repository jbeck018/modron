"""Modron Harbor (Terminal-Bench 2.x) agent adapter.

Modeled on jcode's adapter (scripts/jcode_harbor_agent.py): the modron
binary is uploaded into the task container and run once per trial,
headless, with an isolated sessions dir. Token usage is read back from
the final session's JSONL transcript (Usage events), so per-trial
input/output/cache numbers land in Harbor's results.

Usage (Harbor):
  harbor datasets add terminal-bench@2.0
  harbor run --agent scripts/tb/modron_harbor_agent.py \
      --agent-args model=glm-5.3 --env-var ZAI_API_KEY ... \
      --task regex-log -n 1

Fairness rules copied from jcode's audit tooling:
  - one fresh MODRON session store per trial (no cross-trial memory)
  - no timeout multipliers (agent time uncapped, verifier untouched)
  - k >= 5 trials per task for any published comparison
"""

from __future__ import annotations

import json
import os
import shutil
from pathlib import Path
from typing import Any

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

IN_CONTAINER_BINARY = "/usr/local/bin/modron"
IN_CONTAINER_HOME = "/tmp/modron-home"
DEFAULT_BINARY_PATH = os.environ.get("MODRON_HARBOR_BINARY", "./target/release/modron")


class ModronHarborAgent(BaseAgent):
    def __init__(self, logs_dir: Path, model_name: str | None = None, *args: Any, **kwargs: Any):
        super().__init__(logs_dir, model_name, *args, **kwargs)
        self._model = model_name or "glm-5.3"

    @staticmethod
    def name() -> str:
        return "modron"

    def version(self) -> str | None:
        return os.environ.get("MODRON_HARBOR_VERSION")

    async def setup(self, environment: BaseEnvironment) -> None:
        binary = Path(DEFAULT_BINARY_PATH).expanduser().resolve()
        if not binary.is_file():
            raise FileNotFoundError(
                f"modron binary not found at {binary} — build it first "
                "(cargo build --release) or set MODRON_HARBOR_BINARY"
            )
        await environment.execute("mkdir -p /usr/local/bin")
        contents = binary.read_bytes()
        await environment.upload(contents, remote_path=IN_CONTAINER_BINARY)
        await environment.execute(f"chmod +x {IN_CONTAINER_BINARY}")

    async def run(
        self, instruction: str, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        home = f"{IN_CONTAINER_HOME}/{context.task_id}-{context.trial_id}"
        # Fresh home per trial: sessions, memory, and skills never leak
        # across trials (jcode's fairness rule).
        await environment.execute(f"mkdir -p {home}")
        env_prefix = " ".join(
            f"{key}={value}"
            for key, value in {
                "HOME": home,
                "MODRON_SESSIONS_DIR": f"{home}/sessions",
                "ZAI_API_KEY": os.environ.get("ZAI_API_KEY", ""),
                "ANTHROPIC_API_KEY": os.environ.get("ANTHROPIC_API_KEY", ""),
                "OPENAI_API_KEY": os.environ.get("OPENAI_API_KEY", ""),
                "GEMINI_API_KEY": os.environ.get("GEMINI_API_KEY", ""),
            }.items()
            if value
        )
        # One-shot headless run, auto-approve tools, no model rotation.
        command = (
            f"{env_prefix} {IN_CONTAINER_BINARY} "
            f"--model {self._model} --yolo "
            f"--working-dir {environment.get_working_dir()} "
            f"--sessions-dir {home}/sessions "
            f"--max-turns 80 {json.dumps(instruction)}"
        )
        await environment.execute(command, timeout=None)

        # Report token usage from the newest session transcript.
        out, _ = await environment.execute(
            f"ls -t {home}/sessions/*.jsonl 2>/dev/null | head -1", timeout=30
        )
        newest = (out or "").strip()
        if newest:
            usage_out, _ = await environment.execute(
                f"grep '\"type\":\"usage\"' {newest} | tail -20", timeout=30
            )
            usage = _summarize_usage((usage_out or "").splitlines())
            if usage:
                context.n_input_tokens = usage["input"]
                context.n_output_tokens = usage["output"]
                context.n_cache_tokens = usage["cached"]


def _summarize_usage(lines: list[str]) -> dict[str, int] | None:
    """Sum the LAST Usage event per model (turn-level events repeat)."""
    total: dict[str, dict[str, int]] = {}
    for line in lines:
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") != "usage":
            continue
        model = event.get("model", "?")
        u = event.get("usage", {})
        total[model] = {
            "input": int(u.get("input_tokens", 0)),
            "output": int(u.get("output_tokens", 0)),
            "cached": int(u.get("cached_input_tokens", 0)),
        }
    if not total:
        return None
    return {
        key: sum(per_model[key] for per_model in total.values())
        for key in ("input", "output", "cached")
    }
