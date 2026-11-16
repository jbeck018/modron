#!/usr/bin/env python3
"""Drive the terminal app under a pty for e2e T1 (run by run.mjs, python3 -I).

Config (JSON file path in argv[1]): bin, serve_bin, url, home, cwd, out,
mode (t1 default, t2, t3, t4) and marks (a directory for progress markers the
Node side waits on in t4).
Starts `modron-app-tui --provider mock` with MODRON_BIN and MODRON_URL set and
HOME private, then: waits for "connected", types a task, waits for the
approval prompt, presses Esc, presses Ctrl+C twice, and records what it saw.
Writes a JSON result to cfg["out"]. Kills anything it left running.
"""
import fcntl
import json
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import termios
import time

cfg = json.load(open(sys.argv[1]))
port = cfg["url"].rsplit(":", 1)[1]
# Checks read the RENDERED screen, not the byte stream: ratatui writes only the
# cells that changed, so "connecting" -> "connected" never shows up as one
# contiguous string in the raw output (FINDINGS F14). pyte is a pure-Python
# terminal emulator kept outside the repo (see e2e/README.md, Prerequisites).
PYTE_DIR = cfg.get("pyte") or os.environ.get("E2E_PYTE_DIR") or "/tmp/claude-0/e2e/pyte"
sys.path.insert(0, PYTE_DIR)
try:
    import pyte
except ImportError:
    sys.exit("tui_pty.py: pyte is not importable from " + PYTE_DIR + " (see e2e/README.md)")
buf = b""
screen = pyte.Screen(120, 40)
stream = pyte.ByteStream(screen)


def text():
    return "\n".join(screen.display)


def strip(s):
    # CSI and charset escapes; ratatui redraws move the cursor instead of spaces.
    return re.sub(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b[()][A-Za-z0-9]|\x1b[=>78]", "", s)


def squash(s):
    return re.sub(r"[^a-z0-9]", "", strip(s).lower())


def pump(seconds):
    global buf
    end = time.time() + seconds
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.1)
        if r:
            try:
                data = os.read(fd, 65536)
            except OSError:
                return
            if not data:
                return
            buf += data
            # crossterm's supports_keyboard_enhancement() writes CSI ? u and
            # CSI c and waits for an answer. Answer as a kitty-capable terminal
            # would (flags 1 = disambiguate; DA1 ends the wait), so the app
            # enables the kitty keyboard protocol and Shift+Enter is reported.
            if b"\x1b[?u" in data:
                os.write(fd, b"\x1b[?1u")
            if b"\x1b[c" in data:
                os.write(fd, b"\x1b[?62c")
            stream.feed(data)


def wait_for(pred, timeout):
    end = time.time() + timeout
    while time.time() < end:
        pump(0.2)
        if pred():
            return True
    return False


def wait_approval(timeout):
    """Polls every 20 ms until the approval prompt shows; returns when it did."""
    end = time.time() + timeout
    while time.time() < end:
        pump(0.02)
        if "allowonce" in squash(text()):
            return True
    return False


def mark(name):
    if cfg.get("marks"):
        open(os.path.join(cfg["marks"], name), "w").close()


def denied_text():
    return bool(re.search(r"deny|denied|refus", squash(text())))


pid, fd = pty.fork()
if pid == 0:
    os.chdir(cfg["cwd"])
    env = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": cfg["home"],
        "TERM": "xterm-256color",
        "LANG": "C.UTF-8",
        "MODRON_BIN": cfg["serve_bin"],
        "MODRON_URL": cfg["url"],
        "XDG_STATE_HOME": os.path.join(cfg["home"], "state"),
    }
    os.execve(cfg["bin"], [cfg["bin"], "--provider", "mock"], env)

fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))

res = {
    "mode": cfg.get("mode", "t1"), "connected": False, "approval": False, "sent": [],
    "deny_seen": False, "exit_code": None, "still_running": False, "resume_line": False,
    "serve_left": [], "tail": "",
}

def recent():
    return squash(text())


res["connected"] = wait_for(lambda: bool(re.search(r"(?<!dis)connected", squash(text()))), 45)
# A fresh HOME opens the first-run provider list ("not configured", Nav::first_run).
# Esc backs out one screen; press it until the list is gone, and record whether it went.
res["first_run_screen"] = False
if res["connected"]:
    for _ in range(5):
        if not wait_for(lambda: "notconfigured" in recent(), 2):
            break
        res["first_run_screen"] = True
        os.write(fd, b"\x1b")
        pump(1.5)
    res["first_run_dismissed"] = res["first_run_screen"] and "notconfigured" not in recent()

mode = res["mode"]
reaped = None
if res["connected"] and mode in ("t1", "t2", "t3"):
    os.write(fd, b"e2e tui task\r")
    res["sent"].append("task")
    res["approval"] = wait_approval(60)

if mode == "t1" and res["approval"]:
    # The type-ahead guard ignores a decision key for 300 ms after the prompt
    # first shows, so the user (here: the driver) waits past it before Esc.
    pump(0.5)
    os.write(fd, b"\x1b")  # Esc denies the pending approval
    res["sent"].append("esc")
    pump(3)
    res["deny_seen"] = denied_text()
    res["after_esc_tail"] = text()[-600:]

if mode == "t2" and res["approval"]:
    # Inside the window: Esc is typed before the prompt was read. It is
    # consumed, so the prompt stays up.
    os.write(fd, b"\x1b")
    res["sent"].append("esc-early")
    pump(0.6)
    res["early_ignored"] = "allowonce" in recent()
    res["early_tail"] = text()[-600:]
    # Past the window, the same key denies, and the screen shows the result.
    os.write(fd, b"\x1b")
    res["sent"].append("esc")
    # The mock's script asks for its next tool at once, so the prompt changes
    # (to "write demo-output.txt") rather than clears: the first call was
    # answered, and the run went on.
    res["prompt_cleared"] = wait_for(lambda: "allowonce" not in recent() or "demooutput" in recent(), 15)
    pump(1.0)
    res["deny_seen"] = "demooutput" in recent() or "allowonce" not in recent()
    res["after_esc_tail"] = text()[-600:]

if mode == "t3" and res["approval"]:
    # One Ctrl+C during the running turn (the prompt waits on it) interrupts
    # it; the app stays open. The second press (next) quits.
    os.write(fd, b"\x03")
    res["sent"].append("ctrl-c")
    res["interrupted"] = wait_for(lambda: "allowonce" not in recent(), 15)
    pump(0.3)
    done, reaped_status = os.waitpid(pid, os.WNOHANG)
    if done:
        reaped = reaped_status  # the app exited on its own: keep its status
    res["alive_after_first"] = done == 0
    res["interrupt_tail"] = text()[-600:]

if mode == "t4" and res["connected"]:
    # Ctrl+J inside a draft adds a line; the draft is submitted as one task.
    os.write(fd, b"line one")
    res["typed1"] = wait_for(lambda: "lineone" in recent(), 10)
    os.write(fd, b"\x0a")  # Ctrl+J
    os.write(fd, b"line two")
    res["typed2"] = wait_for(lambda: "linetwo" in recent(), 10)
    # Shift+Enter in kitty encoding (CSI 13;2u) adds the third line.
    os.write(fd, b"\x1b[13;2u")
    os.write(fd, b"line three")
    res["typed3"] = wait_for(lambda: "linethree" in recent(), 10)
    os.write(fd, b"\r")
    res["sent"].append("task")
    res["approval"] = wait_approval(60)
    if res["approval"]:
        mark("m1")

if mode == "t4b" and res["connected"]:
    # A trailing backslash continues the line; Enter then submits both lines.
    os.write(fd, b"alpha\\")
    res["typed1"] = wait_for(lambda: "alpha" in recent(), 10)
    os.write(fd, b"\r")
    os.write(fd, b"omega")
    res["typed2"] = wait_for(lambda: "omega" in recent(), 10)
    os.write(fd, b"\r")
    res["sent"].append("task")
    res["approval"] = wait_approval(60)
    if res["approval"]:
        mark("m1")

# Ctrl+C: interrupts a running turn; a second press within 2 s (or idle) quits.
os.write(fd, b"\x03")
res["sent"].append("ctrl-c")
pump(0.4)
os.write(fd, b"\x03")
res["sent"].append("ctrl-c")

status = reaped
end = time.time() + 15
while status is None and time.time() < end:
    pump(0.2)
    done, st = os.waitpid(pid, os.WNOHANG)
    if done:
        status = st
        break
if status is not None:
    res["exit_code"] = os.waitstatus_to_exitcode(status)
else:
    res["still_running"] = True
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
pump(1.0)

res["resume_line"] = "--session" in text()
res["tail"] = text()[-2500:]

time.sleep(2.5)  # the app stops its own server child on exit (SIGTERM, then SIGKILL after 2 s)
ps = subprocess.run(["ps", "-eo", "pid,args"], capture_output=True, text=True).stdout
left = [line.strip() for line in ps.splitlines() if "serve" in line and f"127.0.0.1:{port}" in line]
res["serve_left"] = left
for line in left:  # clean up so the run leaves nothing behind
    try:
        os.kill(int(line.split()[0]), signal.SIGKILL)
    except (ValueError, OSError):
        pass

with open(cfg["out"], "w") as f:
    json.dump(res, f, indent=2)
