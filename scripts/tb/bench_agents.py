"""Terminal-Bench adapters for an agent-vs-agent comparison (modron, omp).

Both agents run the same way so the numbers are comparable:

- one headless run per trial inside the task container, in the task's own
  shell (same user, cwd and dotfiles), approvals off;
- the same pinned model, reached through scripts/tb/relay.mjs, so the
  container never holds a credential and tokens are counted by the relay
  from the provider's usage fields, not by each agent's self-report;
- no retries, no second model, no per-agent timeout multiplier (the
  harness's task timeout applies to both).

Each trial gets a relay tag. The tag is written to ``trial.json`` in the
trial's agent-logs dir before the agent starts, so compare.py can join a
trial to its relay row even when the harness times the agent out.

Env (host side):
  BENCH_RELAY_URL       relay as seen from task containers
                        (default http://172.17.0.1:8787)
  BENCH_RELAY_HOST_URL  relay as seen from the host (default
                        http://127.0.0.1:8787)
  MODRON_TB_BINARY      musl-static modron binary
  BENCH_OMP_BUNDLE      tar with bin/bun and node_modules/ (see compare.sh)
  BENCH_CA_BUNDLE       optional PEM bundle to trust inside task containers,
                        for hosts whose outbound HTTPS is re-signed by an
                        inspecting proxy; applied to every agent alike
"""

from __future__ import annotations

import json
import os
import tempfile
import time
import urllib.request
import uuid
from pathlib import Path

from terminal_bench.agents.base_agent import AgentResult, BaseAgent, FailureMode
from terminal_bench.terminal.tmux_session import TmuxSession

POLL_SEC = 5
MAX_RUN_SEC = 120 * 60  # backstop only; the harness enforces the task timeout
INSTRUCTION = "/tmp/bench-instruction.txt"
RUN_LOG = "/tmp/bench-run.log"
EXIT_MARKER = "/tmp/bench-exit"
CA_IN_CONTAINER = "/etc/ssl/bench-ca-bundle.crt"
CA_ENV = "/etc/bench-ca.sh"
# Sourced by the agent's shell and by later interactive shells (the test
# step), so curl, pip, uv, node and git all trust the host's bundle.
CA_SETUP = (
    f"printf '%s\\n' "
    f"'export SSL_CERT_FILE={CA_IN_CONTAINER}' "
    f"'export REQUESTS_CA_BUNDLE={CA_IN_CONTAINER}' "
    f"'export CURL_CA_BUNDLE={CA_IN_CONTAINER}' "
    f"'export PIP_CERT={CA_IN_CONTAINER}' "
    f"'export NODE_EXTRA_CA_CERTS={CA_IN_CONTAINER}' "
    f"'export GIT_SSL_CAINFO={CA_IN_CONTAINER}' "
    f"'export UV_NATIVE_TLS=1' > {CA_ENV}; "
    f"[ -f /etc/ssl/certs/ca-certificates.crt ] && "
    f"cp -f {CA_IN_CONTAINER} /etc/ssl/certs/ca-certificates.crt; "
    f"for rc in /etc/bash.bashrc /root/.bashrc /etc/profile; do "
    f"echo '. {CA_ENV}' >> $rc; done"
)


def _relay_url() -> str:
    return os.environ.get("BENCH_RELAY_URL", "http://172.17.0.1:8787").rstrip("/")


def _relay_host_url() -> str:
    return os.environ.get("BENCH_RELAY_HOST_URL", "http://127.0.0.1:8787").rstrip("/")


def relay_stats(tag: str) -> dict:
    """The relay's row for one trial tag; empty when the relay is down."""
    try:
        with urllib.request.urlopen(f"{_relay_host_url()}/stats/{tag}", timeout=10) as resp:
            return json.loads(resp.read())
    except Exception:
        return {}


class RelayAgent(BaseAgent):
    """Shared trial flow; a subclass supplies install and the run command."""

    AGENT = ""
    DEFAULT_MODEL = "glm-5.3"

    def __init__(self, **kwargs):
        super().__init__(**kwargs)
        self._model_name = kwargs.get("model_name", self.DEFAULT_MODEL)

    @classmethod
    def name(cls) -> str:  # type: ignore[override]
        return cls.AGENT

    def install(self, session: TmuxSession) -> None:
        raise NotImplementedError

    def command(self, tag: str) -> str:
        """Shell text that runs the agent on INSTRUCTION, output to RUN_LOG."""
        raise NotImplementedError

    def diagnostics(self) -> list[str]:
        """Shell globs of extra files worth keeping from the container."""
        return []

    def perform_task(
        self,
        instruction: str,
        session: TmuxSession,
        logging_dir: Path | None = None,
    ) -> AgentResult:
        tag = f"{self.AGENT}-{uuid.uuid4().hex[:12]}"
        if logging_dir is None:
            logging_dir = Path(tempfile.mkdtemp(prefix=f"bench-{tag}-"))
        logging_dir.mkdir(parents=True, exist_ok=True)
        trial = {"tag": tag, "agent": self.AGENT, "model": self._model_name}
        (logging_dir / "trial.json").write_text(json.dumps(trial))

        prefix = ""
        ca = os.environ.get("BENCH_CA_BUNDLE", "")
        if ca:
            session.copy_to_container(
                [Path(ca).resolve()],
                container_dir=str(Path(CA_IN_CONTAINER).parent),
                container_filename=Path(CA_IN_CONTAINER).name,
            )
            session.container.exec_run(["sh", "-c", CA_SETUP], user="root")
            prefix = f". {CA_ENV}; "

        self.install(session)
        tmp = Path(tempfile.mkstemp(suffix=".txt")[1])
        tmp.write_text(instruction)
        session.copy_to_container(
            [tmp], container_dir="/tmp", container_filename=Path(INSTRUCTION).name
        )
        tmp.unlink(missing_ok=True)

        # ONE literal send ending in a newline; the exit marker is written
        # last so the poller never sees a half-finished run.
        cmd = (
            f"rm -f {EXIT_MARKER}; {prefix}{self.command(tag)} > {RUN_LOG} 2>&1; "
            f"echo $? > {EXIT_MARKER}\n"
        )
        started = time.monotonic()
        session.send_keys(cmd, block=False, min_timeout_sec=1.0)

        exit_code = ""
        deadline = started + MAX_RUN_SEC
        while time.monotonic() < deadline:
            result = session.container.exec_run(["cat", EXIT_MARKER], user="root")
            if result.exit_code == 0 and (result.output or b"").strip():
                exit_code = (result.output or b"").strip().decode()
                break
            time.sleep(POLL_SEC)
        seconds = round(time.monotonic() - started, 1)

        try:
            listed = session.container.exec_run(
                ["sh", "-c", "ls -t " + " ".join(self.diagnostics()) + " 2>/dev/null | head -3"],
                user="root",
            )
            extra = (listed.output or b"").decode().split()
            _copy_back(session, logging_dir, [RUN_LOG, *extra])
        except Exception:
            pass  # diagnostics are best-effort and never change the result

        stats = relay_stats(tag)
        trial.update({"exit_code": exit_code, "agent_seconds": seconds, "relay": stats})
        (logging_dir / "trial.json").write_text(json.dumps(trial))

        if exit_code == "":
            mode = FailureMode.AGENT_TIMEOUT
        elif exit_code != "0":
            mode = FailureMode.UNKNOWN_AGENT_ERROR
        else:
            mode = FailureMode.NONE
        return AgentResult(
            total_input_tokens=int(stats.get("input", 0)),
            total_output_tokens=int(stats.get("output", 0)),
            failure_mode=mode,
        )


class ModronAgent(RelayAgent):
    AGENT = "modron"
    BINARY = "/usr/local/bin/modron"
    HOME = "/tmp/modron-home"

    def install(self, session: TmuxSession) -> None:
        binary = Path(
            os.environ.get(
                "MODRON_TB_BINARY",
                "target/x86_64-unknown-linux-musl/release/modron",
            )
        ).resolve()
        if not binary.is_file():
            raise FileNotFoundError(
                f"modron Linux binary not found at {binary}; build it with "
                "cargo build --release -p modron-cli --target x86_64-unknown-linux-musl"
            )
        session.copy_to_container(
            [binary], container_dir="/usr/local/bin", container_filename="modron"
        )

    def command(self, tag: str) -> str:
        base = f"{_relay_url()}/t/{tag}/api/coding/paas/v4"
        # --provider pins the model: without it the router picks per-turn
        # tiers and the comparison would no longer be one model against one.
        return (
            f"chmod +x {self.BINARY}; mkdir -p {self.HOME}/sessions; "
            f"ZAI_API_KEY=relay ZAI_BASE_URL='{base}' "
            f"{self.BINARY} --provider zai --model {self._model_name} --yolo "
            f"--sessions-dir {self.HOME}/sessions --max-turns 150 "
            f'"$(cat {INSTRUCTION})"'
        )

    def diagnostics(self) -> list[str]:
        return [f"{self.HOME}/sessions/*.jsonl"]


class OmpAgent(RelayAgent):
    AGENT = "omp"
    ROOT = "/opt/omp"
    HOME = "/tmp/omp-home"

    def __init__(self, **kwargs):
        super().__init__(**kwargs)
        # omp speaks the Anthropic wire format to Z.AI for the GLM family.
        self._api_path = kwargs.get("api_path", "api/anthropic")

    def install(self, session: TmuxSession) -> None:
        bundle = Path(os.environ.get("BENCH_OMP_BUNDLE", "omp-bundle.tar")).resolve()
        if not bundle.is_file():
            raise FileNotFoundError(
                f"omp bundle not found at {bundle}; scripts/tb/compare.sh builds it"
            )
        session.copy_to_container(
            [bundle], container_dir="/opt", container_filename="omp-bundle.tar"
        )

    def command(self, tag: str) -> str:
        base = f"{_relay_url()}/t/{tag}/{self._api_path}"
        cli = f"{self.ROOT}/node_modules/@oh-my-pi/pi-coding-agent/dist/cli.js"
        return (
            f"mkdir -p {self.ROOT} {self.HOME}/agent {self.HOME}/sessions; "
            f"tar -xf /opt/omp-bundle.tar -C {self.ROOT} && rm -f /opt/omp-bundle.tar; "
            f"printf 'providers:\\n  zai:\\n    baseUrl: {base}\\n' "
            f"> {self.HOME}/agent/models.yml; "
            f"PI_CODING_AGENT_DIR={self.HOME}/agent ZAI_API_KEY=relay "
            f"{self.ROOT}/bin/bun {cli} -p --model zai/{self._model_name} "
            # --allow-home: several task images work in the home directory,
            # where omp would otherwise move itself to a temp dir.
            f"--auto-approve --allow-home --no-title "
            f"--session-dir {self.HOME}/sessions "
            f'"$(cat {INSTRUCTION})"'
        )

    def diagnostics(self) -> list[str]:
        return [f"{self.HOME}/sessions/*.jsonl", f"{self.HOME}/sessions/*/*.jsonl"]


def _copy_back(session: TmuxSession, logging_dir: Path, remote_paths: list[str]) -> None:
    """Copy files out of the task container into the trial log dir."""
    import tarfile
    from io import BytesIO

    for remote in remote_paths:
        try:
            bits, _stat = session.container.get_archive(remote)
        except Exception:
            continue  # the file does not exist (the agent never got that far)
        with tarfile.open(fileobj=BytesIO(b"".join(bits))) as tar:
            for member in tar.getmembers():
                if member.isfile():
                    data = tar.extractfile(member)
                    if data is not None:
                        (logging_dir / Path(remote).name).write_bytes(data.read())
