#!/usr/bin/env python3
"""Drive a terminal program in a PTY through scripted steps and capture styled screens.

Usage: drive.py PLAN.json OUT_DIR PROBE -- COMMAND...

PLAN: {"size": [cols, rows], "env": {...}, "cwd": "...",
       "accept_prompts": {"marker": "text", "keys": "1", "label": "permission"},
       "steps": [{"expect": ["text"], "timeout": 30, "settle": 0.5, "snapshot": "name",
                  "frames": 0, "interval": 0.2, "type": "slow text", "send": "keys"}]}
Each step waits until every `expect` string is on the rendered screen (BMUX terminal grid, with
non-breaking spaces read as spaces), waits `settle` seconds for the screen to stop changing,
records a snapshot when named plus `frames` further snapshots `interval` seconds apart, then
types `type` one character at a time and sends `send` as is. While waiting, a screen showing the
`accept_prompts` marker is snapshotted and answered with its keys. Terminal queries are answered
like a typical true-color terminal without the kitty keyboard protocol.

The child gets only PATH, USER, LOGNAME, SHELL and TMPDIR from this environment, plus the plan's.
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

QUERIES = [
    (re.compile(rb"\x1b\[c"), lambda m, s: b"\x1b[?62;22c"),
    (re.compile(rb"\x1b\[>0?c"), lambda m, s: b"\x1b[>1;10;0c"),
    (re.compile(rb"\x1b\]11;\?(\x07|\x1b\\)"), lambda m, s: b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
    (re.compile(rb"\x1b\]10;\?(\x07|\x1b\\)"), lambda m, s: b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
    (re.compile(rb"\x1b\[6n"), lambda m, s: b"\x1b[1;1R"),
    (re.compile(rb"\x1b\[>q"), lambda m, s: b"\x1bP>|xterm(390)\x1b\\"),
]


class Session:
    def __init__(self, command, size, env, cwd, out_dir, probe):
        self.size = size
        self.out_dir = out_dir
        self.probe = probe
        self.capture = bytearray()
        self.capture_path = os.path.join(out_dir, "live.capture")
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(cwd)
            os.execvpe(command[0], command, env)
        self.pid, self.fd = pid, fd
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", size[1], size[0], 0, 0))
        os.kill(pid, signal.SIGWINCH)

    def pump(self, seconds):
        deadline = time.time() + seconds
        while True:
            remaining = deadline - time.time()
            if remaining <= 0:
                return
            ready, _, _ = select.select([self.fd], [], [], remaining)
            if not ready:
                return
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                raise RuntimeError("program exited")
            if not data:
                raise RuntimeError("program exited")
            self.capture += data
            for pattern, reply in QUERIES:
                for match in pattern.finditer(data):
                    os.write(self.fd, reply(match, self))

    def screen(self, styled=False):
        with open(self.capture_path, "wb") as handle:
            handle.write(self.capture)
        args = [self.probe, self.capture_path, str(self.size[0]), str(self.size[1])]
        if styled:
            args.append("--styled")
        return subprocess.run(args, capture_output=True, check=True, timeout=30).stdout.decode()

    def wait(self, expect, timeout, settle, accept=None):
        deadline = time.time() + timeout
        while True:
            self.pump(0.2)
            text = self.screen()
            if all(item in text.replace("\u00a0", " ") for item in expect):
                break
            if accept and accept(text):
                deadline = time.time() + timeout
                continue
            if time.time() > deadline:
                raise TimeoutError(f"timed out waiting for {expect!r}; screen:\n{text}")
        last = self.screen()
        stable_since = time.time()
        while time.time() - stable_since < settle:
            self.pump(0.1)
            now = self.screen()
            if now != last:
                last, stable_since = now, time.time()
        return last

    def send(self, keys):
        os.write(self.fd, keys.encode())

    def type(self, text):
        """Type one character at a time; programs treat a fast multi-character write as a paste."""
        for character in text:
            os.write(self.fd, character.encode())
            self.pump(0.03)


def main():
    separator = sys.argv.index("--")
    plan_path, out_dir, probe = sys.argv[1:separator]
    command = sys.argv[separator + 1:]
    with open(plan_path) as handle:
        plan = json.load(handle)
    os.makedirs(out_dir, exist_ok=True)
    keep = ("PATH", "USER", "LOGNAME", "SHELL", "TMPDIR")
    env = {name: os.environ[name] for name in keep if name in os.environ}
    env.update(plan.get("env", {}))
    session = Session(command, plan.get("size", [120, 40]), env, plan.get("cwd", os.getcwd()),
                      out_dir, probe)
    counter = [0]

    def snapshot(label, text):
        name = f"{counter[0]:02d}-{label}"
        counter[0] += 1
        with open(os.path.join(out_dir, name + ".txt"), "w") as handle:
            handle.write(text)
        with open(os.path.join(out_dir, name + ".json"), "w") as handle:
            handle.write(session.screen(styled=True))
        print(f"snapshot {name}", flush=True)

    prompts = plan.get("accept_prompts")

    def accept(text):
        if not prompts or prompts["marker"] not in text.replace("\u00a0", " "):
            return False
        stable = session.wait([prompts["marker"]], 30, prompts.get("settle", 1.0))
        snapshot(prompts.get("label", "prompt"), stable)
        session.send(prompts.get("keys", "\r"))
        session.pump(1.0)
        return True

    try:
        for step in plan["steps"]:
            text = session.wait(step.get("expect", []), step.get("timeout", 30),
                                step.get("settle", 0.5), accept)
            if step.get("snapshot"):
                snapshot(step["snapshot"], text)
            for frame in range(step.get("frames", 0)):
                session.pump(step.get("interval", 0.2))
                snapshot(f"{step.get('snapshot', 'frame')}-{frame + 1}", session.screen())
            if step.get("type"):
                session.type(step["type"])
            if step.get("send"):
                session.send(step["send"])
    finally:
        with open(os.path.join(out_dir, "final.txt"), "w") as handle:
            try:
                handle.write(session.screen())
            except Exception as error:  # noqa: BLE001
                handle.write(f"<no screen: {error}>")
        try:
            os.kill(session.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


if __name__ == "__main__":
    main()
