"""Modron Terminal-Bench agent adapter (tb CLI).

terminal-bench harness interface: ``perform_task(instruction, session,
logging_dir) -> AgentResult``, driving the task container's tmux session.
The modron Linux binary is uploaded into the container and run ONCE per
trial, headless, with an isolated HOME/sessions dir (fairness rules kept
from the jcode pipeline: fresh state per trial, tokens reported from the
session transcript's Usage events).

Run (pilot):

    source ~/.zshrc   # ZAI_API_KEY (coding-plan key)
    export ZAI_BASE_URL=https://api.z.ai/api/coding/paas/v4
    export MODRON_TB_BINARY=$PWD/target-linux-musl/x86_64-unknown-linux-musl/release/modron
    PYTHONPATH=scripts/tb tb run \\
        --dataset-path /path/to/terminal-bench \\
        --task-id hello-world \\
        --agent-import-path modron_tb_agent:ModronTbAgent \\
        --agent-kwarg model_name=glm-5.3 \\
        --n-attempts 1 --n-concurrent 1
"""

from __future__ import annotations

import json
import os
import tempfile
import time
from pathlib import Path

from terminal_bench.agents.base_agent import AgentResult, BaseAgent, FailureMode
from terminal_bench.terminal.tmux_session import TmuxSession

# musl static build runs on any task image regardless of glibc age.
DEFAULT_BINARY = (
    "target-linux-musl/x86_64-unknown-linux-musl/release/modron"
)
HOME_IN_CONTAINER = "/tmp/modron-home"
BINARY_IN_CONTAINER = "/usr/local/bin/modron"
POLL_SEC = 5
MAX_RUN_SEC = 30 * 60


class ModronTbAgent(BaseAgent):
    def __init__(self, **kwargs):
        super().__init__(**kwargs)
        self._model_name = kwargs.get("model_name", "glm-5.3")

    @staticmethod
    def name() -> str:
        return "modron"

    @property
    def version(self) -> str | None:
        return os.environ.get("MODRON_TB_VERSION")

    def perform_task(
        self,
        instruction: str,
        session: TmuxSession,
        logging_dir: Path | None = None,
    ) -> AgentResult:
        binary = Path(
            os.environ.get("MODRON_TB_BINARY", DEFAULT_BINARY)
        ).resolve()
        if not binary.is_file():
            raise FileNotFoundError(
                f"modron Linux binary not found at {binary} — build it with: "
                "docker run --platform linux/amd64 -v \"$PWD\":/repo -w /repo "
                "rust:1.99 sh -c 'apt-get update -qq && apt-get install -y "
                "musl-tools && rustup target add x86_64-unknown-linux-musl && "
                "CARGO_TARGET_DIR=/repo/target-linux-musl cargo build --release "
                "-p modron-cli --target x86_64-unknown-linux-musl'"
            )
        api_key = os.environ.get("ZAI_API_KEY", "")
        if not api_key:
            raise RuntimeError(
                "ZAI_API_KEY not set — source ~/.zshrc before tb run. "
                "Coding-plan keys also need ZAI_BASE_URL="
                "https://api.z.ai/api/coding/paas/v4"
            )
        base_url = os.environ.get(
            "ZAI_BASE_URL", "https://api.z.ai/api/coding/paas/v4"
        )

        # 1. Binary into the container (copy does not preserve the exec bit).
        session.copy_to_container(
            [binary], container_dir="/usr/local/bin", container_filename="modron"
        )

        # 2. Instruction file: avoids quoting hell in the tmux command line.
        tmp = Path(tempfile.mkstemp(suffix=".txt")[1])
        tmp.write_text(instruction)
        session.copy_to_container(
            [tmp], container_dir="/tmp", container_filename="modron-instruction.txt"
        )

        # 3. One-shot headless run, detached with an exit marker; fresh
        #    HOME + sessions dir per trial. Env rides the command line
        #    (container-scoped, trial-lifetime — documented tradeoff).
        #    ONE literal send ending in a newline: non-blocking sends go
        #    through verbatim (the blocking path strips and injects its
        #    own Enter machinery — two separate sends raced and merged).
        home = HOME_IN_CONTAINER
        cmd = (
            f"chmod +x {BINARY_IN_CONTAINER}; "
            f"export HOME={home} MODRON_SESSIONS_DIR={home}/sessions "
            f"ZAI_API_KEY='{api_key}' ZAI_BASE_URL='{base_url}'; "
            f"mkdir -p {home}/sessions; rm -f /tmp/modron-exit; "
            f"{BINARY_IN_CONTAINER} --model {self._model_name} --yolo "
            f"--sessions-dir {home}/sessions --max-turns 60 "
            f'"$(cat /tmp/modron-instruction.txt)" > /tmp/modron-run.log 2>&1; '
            "echo $? > /tmp/modron-exit\n"
        )
        session.send_keys(cmd, block=False, min_timeout_sec=1.0)

        # 4. Poll for the exit marker (tmux prompt detection is unreliable
        #    for long silent stretches).
        deadline = time.monotonic() + MAX_RUN_SEC
        while time.monotonic() < deadline:
            result = session.container.exec_run(
                ["cat", "/tmp/modron-exit"], user="root"
            )
            if result.exit_code == 0 and (result.output or b"").strip():
                break
            time.sleep(POLL_SEC)
        else:
            return AgentResult(failure_mode=FailureMode.AGENT_TIMEOUT)

        # 5. Token usage: the LAST Usage event per model from the newest
        #    transcript (turn-level events repeat).
        out = session.container.exec_run(
            ["sh", "-c",
             f"ls -t {home}/sessions/*.jsonl 2>/dev/null | head -1 | xargs "
             f"grep -h '\"type\":\"usage\"' 2>/dev/null"],
            user="root",
        )
        input_tokens, output_tokens = _summarize_usage(
            (out.output or b"").decode("utf-8", "replace")
        )
        exit_code = (result.output or b"").strip().decode()
        if exit_code not in ("0", ""):
            # The run itself errored; record as unspecified failure.
            return AgentResult(
                total_input_tokens=input_tokens,
                total_output_tokens=output_tokens,
                failure_mode=FailureMode.UNKNOWN_AGENT_ERROR,
            )
        return AgentResult(
            total_input_tokens=input_tokens,
            total_output_tokens=output_tokens,
            failure_mode=FailureMode.NONE,
        )


def _summarize_usage(text: str) -> tuple[int, int]:
    last_per_model: dict[str, tuple[int, int]] = {}
    for line in text.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") != "usage":
            continue
        usage = event.get("usage", {})
        last_per_model[event.get("model", "?")] = (
            int(usage.get("input_tokens", 0)),
            int(usage.get("output_tokens", 0)),
        )
    return (
        sum(v[0] for v in last_per_model.values()),
        sum(v[1] for v in last_per_model.values()),
    )
