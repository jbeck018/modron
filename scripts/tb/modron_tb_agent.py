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
BINARY_IN_CONTAINER = "/usr/local/bin/modron"
POLL_SEC = 5
MAX_RUN_SEC = 30 * 60


def _provider_for(model: str) -> str:
    """Provider id for the pinned run, from the model family."""
    if model.startswith("gpt-"):
        return "chatgpt"
    if model.startswith("claude-"):
        return "anthropic"
    return "zai"


class ModronTbAgent(BaseAgent):
    def __init__(self, **kwargs):
        super().__init__(**kwargs)
        self._model_name = kwargs.get("model_name", "glm-5.3")
        # Pin provider+model together: without --provider the router picks
        # per-turn tiers (e.g. the flash cheap tier), and a gated/overloaded
        # flash model kills the trial even when the pinned model works.
        self._provider = (
            kwargs.get("provider_name") or _provider_for(self._model_name)
        )

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
        home = "/tmp/modron-home"
        cmd = (
            f"chmod +x {BINARY_IN_CONTAINER}; "
            # NO HOME override: task images legitimately ship dotfiles in
            # the home dir (aws creds, jupyter config, .ssh) and every TB
            # trial is already a fresh container — isolation is free.
            # Sessions still go to an explicit dir.
            f"export MODRON_SESSIONS_DIR={home}/sessions "
            f"ZAI_API_KEY='{api_key}' ZAI_BASE_URL='{base_url}'; "
            f"mkdir -p {home}/sessions; rm -f /tmp/modron-exit; "
            f"{BINARY_IN_CONTAINER} --provider {self._provider} "
            f"--model {self._model_name} --yolo "
            f"--sessions-dir {home}/sessions --max-turns 80 "
            f'"$(cat /tmp/modron-instruction.txt)" > /tmp/modron-run.log 2>&1; '
            "echo $? > /tmp/modron-exit; "
            "cp -f /tmp/modron-run.log /logs/modron-run.log 2>/dev/null; "
            "cp -f $(ls -t /tmp/modron-home/sessions/*.jsonl 2>/dev/null "
            "| head -1) /logs/modron-session.jsonl 2>/dev/null; true\n"
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

        # 5. Diagnostics into the trial's logging_dir: modron's stdout log
        #    and its session transcript — without these a failed trial is
        #    undissectable (the container is destroyed after the run).
        if logging_dir is None:
            # tb's custom-agent path passes None: fall back to a host-side
            # scrape dir so failures stay dissectable.
            logging_dir = Path("/tmp/modron-trial-logs") / time.strftime("%H%M%S")
        try:
            if logging_dir is not None:
                _copy_back(session, logging_dir, [
                    "/tmp/modron-run.log",
                    "/tmp/modron-exit",
                ])
                newest_out = session.container.exec_run(
                    ["sh", "-c",
                     f"ls -t {home}/sessions/*.jsonl 2>/dev/null | head -1"],
                    user="root",
                )
                newest = (newest_out.output or b"").decode().strip()
                if newest:
                    _copy_back(session, logging_dir, [newest])
        except Exception:
            tracing = None  # capture is best-effort, never poison the result

        # 6. Token usage: the LAST Usage event per model, parsed from the
        #    /logs copy (guaranteed by the run command's cp) — the old
        #    in-container grep raced the transcript flush and under-
        #    reported several trials.
        out = session.container.exec_run(
            ["sh", "-c",
             "cat /logs/modron-session.jsonl 2>/dev/null; "
             f"cat {home}/sessions/*.jsonl 2>/dev/null"],
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



def _copy_back(session: TmuxSession, logging_dir: Path, remote_paths: list[str]) -> None:
    """Copy files out of the task container into the trial log dir."""
    import tarfile
    from io import BytesIO

    logging_dir.mkdir(parents=True, exist_ok=True)
    for remote in remote_paths:
        try:
            bits, _stat = session.container.get_archive(remote)
        except Exception:
            continue  # missing file (e.g. no exit marker on timeout)
        tar = tarfile.open(fileobj=BytesIO(b"".join(bits)))
        for member in tar.getmembers():
            if not member.isfile():
                continue
            out = logging_dir / Path(remote).name
            out.write_bytes(tar.extractfile(member).read())

def _summarize_usage(text: str) -> tuple[int, int]:
    """Sum EVERY usage event: the engine logs per-turn deltas, not
    cumulative totals (last-event logic under-reported long runs by ~20x).
    Subagent spend arrives as its own events tagged "subagent (tool)" —
    included, honest accounting."""
    total_in = total_out = 0
    for line in text.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") != "usage":
            continue
        usage = event.get("usage", {})
        total_in += int(usage.get("input_tokens", 0))
        total_out += int(usage.get("output_tokens", 0))
    return total_in, total_out
