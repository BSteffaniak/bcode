#!/usr/bin/env python3
"""Offline PTY acceptance, without builds or the caller's configuration/state.

Usage (after the coordinator grants runtime capacity):
  bash scripts/smoke-continuous-tui-pty.sh --binary /absolute/bcode \
    --grid-probe /absolute/bcode_terminal_grid_probe \
    --fake-library /absolute/libbcode_fake_provider_plugin.dylib \
    --case quick-advanced-cancel

Built-in cases assert only the named journey, not the entire product acceptance.
Custom --plan JSON contract:
  {"name":"review-ask-live-feed", "model":true, "args":["review"],
   "steps":[{"expect":["Code reviews"], "send":"1"},
            {"expect":["Working tree and index"], "send":"..."},
            {"expect":["Code reviews"], "send":"q"}], "exit_code":0}
Expectations are case-insensitive substrings of the *current rendered grid*, not
source assertions or historical terminal output. A step may have forbid:[...],
resize:[columns,rows], timeout:seconds, hold:seconds, and send:text. Use hold to
keep a delayed initializer visible while repeatedly checking raw mode/containment.
Default hold is 0.5 seconds so a transition does not consume the next screen's key.
Review steps may use mouse:{kind,column,row|row_text,end_text?,count?}; kinds
are click (default), move, drag, wheel_up and wheel_down. row_text/end_text must
match exactly one visible row; column is an explicit terminal-cell coordinate.
Do not use Python string indexes as Unicode terminal widths. A mouse action
cannot share a step with send/resize. screen_change:none|some compares this grid
with the preceding step. no_rewrite:true checks output since that step's action
for display erase or near-screen-sized printable payload; use only after settled
no-op pointer events, not resize/scroll/transitions. Evidence records the counts.
handoff:true lets the application hand the terminal to a child program (or take it
back) while that step waits: the alternate screen may be left and entered again,
and the step still only matches the grid while an alternate screen is active.
Review-only restart_steps runs a second real TUI process against the same isolated
fixture/state after clean exit; first-* artifacts preserve the original journey.
This proves saved-workspace restoration rather than retained in-memory navigation.
--cold-start lets the product start its own isolated daemon; cleanup uses its
public server stop against the explicit test artifact and isolated state only.
blims-service-timeout creates company/world with public CLI, suspends only an
unreaped direct daemon child, checks visible unknown-outcome recovery and resumes
that child before cleanup. It never retries ambiguous changes.
Every step must expect
visible content; reject:[...] fails immediately on forbidden side effects, whereas
forbid:[...] waits for stale transition content to disappear. Completion cases
exercise plugin-owned path discovery and keep parents open after accept/Escape;
they do not claim submission or workflow execution acceptance.
Custom plans use fake-only fixtures. The dedicated xai-model-chat case enables
bundled compatible local recommendation with a fake XAI_API_KEY, disables remote
catalogs and sends no prompt. Darwin sandbox-exec denies IP networking for both
client and daemon (Unix IPC remains allowed); other platforms fail closed for this
case. This is not auth verification/provider response
coverage. Successful enrollment still needs a separate safe fixture.

Captures, final grid, daemon log and JSON evidence stay in a private temporary
folder (reported on completion/failure). Fake/review cases enable only fake-provider
and local default-agent/code-review plugins (review cases add the harness plugin,
since questions about the code run in real sessions). xAI case uses bundled compatible only
for local recommendation, with a fake key and zero chat submissions. No session is fabricated
for setup. Cleanup signals only direct children we started and have not reaped.
--self-test tests harness mechanics only, never application acceptance.
"""

import argparse
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

MAX_CAPTURE = 8 * 1024 * 1024
MODE = re.compile(rb"\x1b\[\?([0-9;]+)([hl])")
ALT_MODES = {47, 1047, 1049}
MARKER = b"BCODE_CONTINUOUS_PTY_SHELL_SENTINEL"
def completion_steps(parent, exit_steps):
    """Keep the focused parent open through accept, Escape, and ordinary edits."""
    guard = ["COMPLETION_PARENT_SUBMITTED"]
    return [
        {"expect": [parent], "hold": 2, "send": "@completion-t"},
        {"expect": ["Suggestions", "completion-target.txt"], "send": "\r"},
        {"expect": [parent, "@completion-target.txt"], "forbid": ["Suggestions"],
         "reject": guard, "hold": 1, "send": " ordinary"},
        {"expect": [parent, "@completion-target.txt ordinary"], "reject": guard,
         "send": "\x15@completion-t"},
        {"expect": ["Suggestions", "completion-target.txt"], "reject": guard,
         "send": "\x1b"},
        {"expect": [parent, "@completion-t"],
         "forbid": ["Suggestions", "completion-target.txt"], "reject": guard,
         "send": " ordinary"},
        {"expect": [parent, "@completion-t ordinary"], "reject": guard,
         "send": "\x1b"},
    ] + exit_steps


CASES = {
    "completion-chat": {"fixture": "completion", "model": True,
        "steps": completion_steps("Message", [
            {"expect": ["Message"], "send": "\x04"},
            {"expect": ["input cleared", "Message"], "send": "\x04"}])},
    "completion-loop-goal": {"fixture": "completion", "model": True,
        "steps": [{"expect": ["Message"], "send": "/goal\r"},
                  {"expect": ["Message", "/goal"], "send": "\r"}] +
        completion_steps("Goal (required)", [
            {"expect": ["Ask Bcode"], "send": "\x04"}])},
    "completion-question": {"fixture": "completion", "model": True,
        "steps": [{"expect": ["Message", "ready"], "hold": 2, "send": 'tool-call question {"questions":[{"header":"Completion","question":"COMPLETION_QUESTION","options":[],"custom":true,"control":"radio","selection_mode":"single","required":true}]}\r'}] +
        completion_steps("COMPLETION_QUESTION", [
            {"expect": ["Ask Bcode"], "send": "\x04"}])},

    # The review shows the fixture's files, so suggestions are told apart by their header and
    # by what the composer's text becomes.
    "completion-review": {"fixture": "completion-review", "model": True, "args": ["review"],
        "steps": [
        {"expect": ["Code reviews", "Staged changes"], "send": "1"},
        {"expect": ["CONTINUOUS_REVIEW_FILE"], "forbid": ["Code reviews"], "send": "jj"},
        {"expect": ["CONTINUOUS_REVIEW_FILE"], "send": "a"},
        {"expect": ["Ask Bcode about"], "hold": 1, "send": "@completion-t"},
        {"expect": ["Suggestions", "@completion-t"], "send": "\r"},
        {"expect": ["Ask Bcode about", "@completion-target.txt"], "forbid": ["Suggestions"],
         "reject": ["COMPLETION_PARENT_SUBMITTED"], "hold": 1, "send": " ordinary"},
        {"expect": ["@completion-target.txt ordinary"], "reject": ["COMPLETION_PARENT_SUBMITTED"],
         "send": "\x15@completion-t"},
        {"expect": ["Suggestions", "@completion-t"], "reject": ["COMPLETION_PARENT_SUBMITTED"],
         "send": "\x1b"},
        {"expect": ["Ask Bcode about", "@completion-t"], "forbid": ["Suggestions"],
         "reject": ["COMPLETION_PARENT_SUBMITTED"], "send": " ordinary"},
        {"expect": ["@completion-t ordinary"], "reject": ["COMPLETION_PARENT_SUBMITTED"],
         "send": "\x1b"},
        {"expect": ["CONTINUOUS_REVIEW_FILE"], "forbid": ["Ask Bcode about"], "send": "q"},
        {"expect": ["Code reviews"], "send": "q"}]},

    "blims-service-timeout": {"fixture": "blims", "model": True,
        "args": ["blims", "enter"], "steps": [
        {"expect": ["Pixel office"], "daemon_signal": "suspend", "send": "r"},
        {"expect": ["Outcome unknown", "changes paused"], "hold": 0.5,
         "daemon_signal": "resume", "send": "q", "timeout": 60}]},
    "xai-model-chat": {"fixture": "xai", "model": False, "args": [], "steps": [
        {"expect": ["Welcome to Bcode", "xAI", "grok"], "send": "\r"},
        {"expect": ["Ask Bcode"], "hold": 1, "send": "\x04"}]},
    "quick-cancel": {"model": False, "args": [], "steps": [
        {"expect": ["Welcome to Bcode", "No usable account"], "send": "\x1b"}]},
    "quick-advanced-cancel": {"model": False, "args": [], "steps": [
        {"expect": ["No usable account"], "send": "\x1bOQ"},
        {"expect": ["Set up your workspace"], "send": "\x1b"}]},
    "advanced-cancel": {"model": True, "args": ["--onboard"], "steps": [
        {"expect": ["Set up your workspace"], "send": "\x1b"}]},
    "advanced-enrollment-cancel": {"model": True, "args": ["--onboard"], "steps": [
        {"expect": ["Set up your workspace"], "send": "p"},
        {"expect": ["Choose a provider", "Fake provider"], "send": "\x1b"},
        {"expect": ["Set up your workspace"], "send": "\x1b"}]},
    "advanced-chat": {"model": True, "args": ["--onboard"], "steps": [
        {"expect": ["Set up your workspace"], "send": "l"},
        {"expect": ["Ask Bcode"], "send": "CONTINUOUSPTYECHO\r"},
        {"expect": ["CONTINUOUSPTYECHO", "fake"], "send": "\x04"}]},
}


# Review plans use observed row anchors and explicit cell columns, so component
# geometry can change; a missing or ambiguous anchor fails rather than falling
# back to guessed row numbers. `q` leaves the review for home and quits home.
CASES.update({
    "review-home-cancel": {"fixture": "review", "model": True,
        "args": ["review"], "steps": [
        {"expect": ["Code reviews", "Uncommitted changes"], "send": "q"}]},
    "review-invalid-repository": {"fixture": "fake", "model": True,
        "args": ["review"], "steps": [
        {"expect": ["Code reviews", "New review"], "send": "1"},
        {"expect": ["What do you want to review?"], "send": "\r"},
        {"expect": ["Which changes?"], "send": "\r"},
        {"expect": ["Create review"], "send": "\r"},
        {"expect": ["Could not open the review", "not a git repository"], "send": "\x1b"},
        {"expect": ["Code reviews"], "forbid": ["Could not open the review"], "send": "q"}]},
    "review-home-new-workspace": {"fixture": "review", "model": True,
        "args": ["review"], "steps": [
        {"expect": ["Code reviews"], "send": "n"},
        {"expect": ["What do you want to review?", "Working tree"], "send": "\r"},
        {"expect": ["Which changes?", "Staged changes"], "send": "\x1b[B"},
        {"expect": ["\u258c Staged changes"], "send": "\r"},
        # Typing edits the title while the action list keeps its selection.
        {"expect": ["\u258c Create review", "Sources (1)"], "send": "_PTY"},
        {"expect": ["Staged changes_PTY", "\u258c Create review"], "send": "\r"},
        {"expect": ["Staged changes_PTY", "CONTINUOUS_REVIEW_FILE"],
         "forbid": ["Code reviews", "Create review"], "send": "q"},
        {"expect": ["Code reviews", "In progress", "Staged changes_PTY"], "send": "q"}],
        "restart_steps": [
        {"expect": ["Code reviews", "Staged changes_PTY"],
         "mouse": {"row_text": "0/4", "column": 6}},
        {"expect": ["Staged changes_PTY", "CONTINUOUS_REVIEW_FILE"],
         "forbid": ["Code reviews"], "send": "q"},
        {"expect": ["Code reviews"], "send": "q"}]},
    "review-ask-live-feed": {"fixture": "review", "model": True,
        "args": ["review"], "steps": [
        {"expect": ["Code reviews", "Uncommitted changes"], "send": "1"},
        {"expect": ["Working tree and index", "CONTINUOUS_REVIEW_FILE"],
         "forbid": ["Code reviews"], "send": "jj"},
        {"expect": ["CONTINUOUS_REVIEW_FILE"], "send": "a"},
        {"expect": ["Ask Bcode about continuous-review-fixture.txt:1"],
         "send": "CONTINUOUS_REVIEW_QUESTION\r"},
        # The answer streams into the card below the line; the review keeps the terminal.
        {"expect": ["Bcode answered", "\u258dyou", "CONTINUOUS_REVIEW_QUESTION",
                    "CONTINUOUS_REVIEW_ANSWER", "Open in Bcode", "CONTINUOUS_REVIEW_FILE"],
         "forbid": ["You are helping with a local code review"], "timeout": 60, "send": "o"},
        # `o` on the card opens the linked session in Bcode, which takes over the terminal.
        {"expect": ["You are helping with a local code review", "CONTINUOUS_REVIEW_ANSWER"],
         "handoff": True, "hold": 1, "send": "\x04"},
        {"expect": ["returned from linked Bcode session", "CONTINUOUS_REVIEW_ANSWER"],
         "handoff": True, "send": "q"},
        # Leaving an unsaved review with notes asks for a second exit.
        {"expect": ["this review is not saved"], "send": "q"},
        {"expect": ["Code reviews"], "send": "q"}]},
    "review-click-save-resume": {"fixture": "review", "model": True,
        "args": ["review"], "steps": [
        {"expect": ["Code reviews", "Uncommitted changes"],
         "mouse": {"row_text": "Uncommitted changes  1 file", "column": 8}},
        {"expect": ["Working tree and index", "CONTINUOUS_REVIEW_FILE"],
         "forbid": ["Code reviews"], "send": "\x13"},
        {"expect": ["Working tree and index", "Review saved"], "send": "q"},
        {"expect": ["Code reviews", "In progress"], "send": "q"}],
        "restart_steps": [
        {"expect": ["Code reviews", "In progress", "Uncommitted changes  "],
         "mouse": {"row_text": "0/4", "column": 6}},
        {"expect": ["Working tree and index", "CONTINUOUS_REVIEW_FILE"],
         "forbid": ["Code reviews"], "send": "q"},
        {"expect": ["Code reviews"], "send": "q"}]},
    "review-wizard-unstaged": {"fixture": "review", "model": True,
        "args": ["review"], "steps": [
        {"expect": ["Code reviews"], "send": "n"},
        {"expect": ["What do you want to review?", "Working tree"], "send": "\r"},
        {"expect": ["Which changes?", "\u258c Unstaged changes  1 file"], "send": "\r"},
        {"expect": ["\u258c Create review", "Sources (1)"], "send": "\r"},
        {"expect": ["REVIEW_UNSTAGED_CONTENT"],
         "forbid": ["Code reviews", "CONTINUOUS_REVIEW_FILE"], "send": "q"},
        {"expect": ["Code reviews"], "send": "q"}]},
    "review-tree-scroll-resize": {"fixture": "review", "model": True,
        "args": ["review"], "steps": [
        {"expect": ["Code reviews"], "send": "1"},
        {"expect": ["CONTINUOUS_REVIEW_FILE", "▾ alpha/", "▾ beta/", "▾ 日本語/"],
         "mouse": {"row_text": "▾ nested/", "column": 4}},
        {"expect": ["▸ nested/"], "forbid": ["▾ alpha/", "▾ beta/", "▾ 日本語/"],
         "mouse": {"row_text": "▸ nested/", "column": 4}},
        {"expect": ["▾ alpha/", "▾ beta/", "▾ 日本語/"], "send": "f"},
        {"expect": ["Files"], "send": "scroll-target"},
        {"expect": ["1/4 \u2500 \u00ab"], "send": "\r"},
        {"expect": ["REVIEW_SCROLL_LINE_001"], "send": "\x1b"},
        {"expect": ["REVIEW_SCROLL_LINE_001"], "resize": [100, 24]},
        {"expect": ["REVIEW_SCROLL_LINE_001"],
         "mouse": {"kind": "wheel_down", "column": 75, "row": 12, "count": 6}},
        {"expect": ["REVIEW_SCROLL_LINE_"], "forbid": ["REVIEW_SCROLL_LINE_001"],
         "screen_change": "some", "send": "G"},
        {"expect": ["REVIEW_UNICODE_CONTENT"], "send": "g"},
        {"expect": ["CONTINUOUS_REVIEW_FILE"],
         "mouse": {"kind": "wheel_down", "column": 8, "row": 10, "count": 2}},
        # A wheel over the file tree does not scroll the review.
        {"expect": ["CONTINUOUS_REVIEW_FILE", "REVIEW_SCROLL_LINE_001"], "resize": [48, 16]},
        {"expect": ["Uncommitted changes"], "resize": [120, 32]},
        {"expect": ["Uncommitted changes", "CONTINUOUS_REVIEW_FILE"], "send": "q"},
        {"expect": ["Code reviews"], "send": "q"}]},
    "review-range-edit-thread": {"fixture": "review", "model": True,
        "args": ["review"], "steps": [
        {"expect": ["Code reviews"], "send": "1"},
        {"expect": ["CONTINUOUS_REVIEW_FILE"], "send": "f"},
        {"expect": ["Files"], "send": "scroll-target"},
        {"expect": ["1/4 \u2500 \u00ab"], "send": "\r"},
        {"expect": ["REVIEW_SCROLL_LINE_001"], "send": "\x1b"},
        {"expect": ["REVIEW_SCROLL_LINE_001", "REVIEW_SCROLL_LINE_003"],
         "mouse": {"kind": "drag", "row_text": "REVIEW_SCROLL_LINE_001",
                   "end_text": "REVIEW_SCROLL_LINE_003", "column": 60}},
        # Dragging across lines opens a comment on the range.
        {"expect": ["Comment on nested/alpha/scroll-target.txt:1-3"], "send": "PTY_RANGE_DRAFT\r"},
        {"expect": ["PTY_RANGE_DRAFT", "1 draft", "draft saved"], "hold": 1, "send": ")i"},
        {"expect": ["Edit comment", "PTY_RANGE_DRAFT"], "send": "\x05_EDITED\r"},
        {"expect": ["PTY_RANGE_DRAFT_EDITED", "comment updated"], "hold": 1, "send": "\x13"},
        {"expect": ["PTY_RANGE_DRAFT_EDITED", "Review saved"], "send": "q"},
        {"expect": ["Code reviews", "In progress"], "send": "q"}],
        "restart_steps": [
        {"expect": ["Code reviews", "In progress"],
         "mouse": {"row_text": "0/4", "column": 6}},
        {"expect": ["Uncommitted changes", "1 draft"], "forbid": ["Code reviews"], "send": "f"},
        {"expect": ["Files"], "send": "scroll-target"},
        {"expect": ["1/4 \u2500 \u00ab"], "send": "\r"},
        {"expect": ["PTY_RANGE_DRAFT_EDITED"], "send": "\x1b"},
        {"expect": ["PTY_RANGE_DRAFT_EDITED"], "send": "q"},
        {"expect": ["Code reviews"], "send": "q"}]},
    "review-pointer-no-rewrite": {"fixture": "review", "model": True,
        "args": ["review"], "steps": [
        {"expect": ["Code reviews"], "send": "1"},
        # The first move shows the hovered line's comment affordance; moves within the
        # same line change nothing and repaint nothing.
        {"expect": ["CONTINUOUS_REVIEW_FILE"], "hold": 1,
         "mouse": {"kind": "move", "column": 80, "row": 12}},
        {"expect": ["CONTINUOUS_REVIEW_FILE"], "hold": 1,
         "mouse": {"kind": "move", "column": 81, "row": 12}},
        {"expect": ["CONTINUOUS_REVIEW_FILE"], "screen_change": "none",
         "no_rewrite": True, "hold": 1,
         "mouse": {"kind": "move", "column": 82, "row": 12}},
        {"expect": ["CONTINUOUS_REVIEW_FILE"], "screen_change": "none",
         "no_rewrite": True, "hold": 1, "send": "q"},
        {"expect": ["Code reviews"], "send": "q"}]},
})


def digest(path):
    with open(path, "rb") as source:
        checksum = hashlib.sha256()
        for block in iter(lambda: source.read(1024 * 1024), b""):
            checksum.update(block)
        return checksum.hexdigest()


def stop(child):
    """Only signal an unreaped direct child (no pid discovery or process sweeps)."""
    if child is not None and child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=15)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=30)


class Interval:
    def __init__(self):
        self.enters = 0
        self.leaves = 0
        self.active = False
        self.end = 0
        # Set while a handoff step waits; entries made then are handoffs.
        self.handoff = False
        self.handoff_enters = 0

    def scan(self, capture):
        # Revisit an unfinished escape sequence, never recount a complete one.
        for match in MODE.finditer(capture, self.end):
            self.end = match.end()
            if not ALT_MODES.intersection(map(int, match[1].split(b";"))):
                continue
            if match[2] == b"h":
                self.enters += 1
                if self.enters != 1:
                    if not self.handoff or self.active:
                        raise AssertionError("alternate screen re-entered during journey")
                    self.handoff_enters += 1
                self.active = True
            else:
                self.leaves += 1
                if not self.active:
                    raise AssertionError("alternate screen left without entry")
                self.active = False

    def finished(self):
        expected = 1 + self.handoff_enters
        if (self.enters, self.leaves, self.active) != (expected, expected, False):
            raise AssertionError(f"expected {expected} screen interval(s), got {self.enters}/{self.leaves}")


def mouse_bytes(mouse, grid, columns, rows):
    """Resolve row anchors from the observed grid, never from source layout.

    Columns are explicit terminal cells: string indexes cannot measure Unicode.
    Ambiguous or clipped anchors fail instead of clicking an unrelated control.
    """
    def anchor(text):
        matches = [index + 1 for index, line in enumerate(grid.splitlines())
                   if text.lower() in line.lower()]
        if len(matches) != 1:
            raise AssertionError(f"mouse row anchor {text!r} matched {len(matches)} rows")
        return matches[0]

    row = anchor(mouse["row_text"]) if "row_text" in mouse else mouse["row"]
    column = mouse["column"]
    end = anchor(mouse["end_text"]) if "end_text" in mouse else row
    if not (1 <= column <= columns and 1 <= row <= rows and 1 <= end <= rows):
        raise AssertionError("mouse target outside current terminal")

    def sgr(button, y, suffix="M"):
        return f"\x1b[<{button};{column};{y}{suffix}".encode()

    kind = mouse.get("kind", "click")
    if kind == "click":
        return sgr(0, row) + sgr(0, row, "m")
    if kind == "drag":
        return sgr(0, row) + sgr(32, end) + sgr(0, end, "m")
    button = {"move": 35, "wheel_up": 64, "wheel_down": 65}[kind]
    return sgr(button, row) * mouse.get("count", 1)


def terminal_output_evidence(data, columns, rows):
    """Conservative wire evidence, not a terminal emulator or frame detector.

    An unchanged grid alone hides clear/repaint flicker. Count explicit whole
    display erases and printable payload large enough to rewrite most cells.
    Small differential paints are allowed; passing does not prove zero flicker.
    """
    csi = rb"\x1b\[[0-?]*[ -/]*[@-~]"
    without_strings = re.sub(rb"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)", b"", data)
    plain = re.sub(csi, b"", without_strings).decode("utf-8", "replace")
    printable = sum(character.isprintable() for character in plain)
    clears = len(re.findall(rb"\x1b\[(?:2|3)J", without_strings))
    return {"bytes": len(data), "display_erases": clears,
            "printable_characters": printable,
            "rewrite_suspected": bool(clears or printable >= columns * rows * 0.75)}


def check_step_observation(step, previous_grid, grid, output):
    if "screen_change" in step:
        if previous_grid is None:
            raise AssertionError("screen_change requires a preceding step")
        changed = grid != previous_grid
        if changed != (step["screen_change"] == "some"):
            raise AssertionError(f"expected screen_change={step['screen_change']}")
    if step.get("no_rewrite") and output["rewrite_suspected"]:
        raise AssertionError(f"unexpected whole-screen erase/rewrite after pointer event: {output}")


def pump(master, capture, interval, timeout=0.05):
    readable, _, _ = select.select([master], [], [], timeout)
    if readable:
        try:
            chunk = os.read(master, 65536)
        except OSError as error:
            if error.errno != errno.EIO:
                raise
            chunk = b""
        capture.extend(chunk)
        if len(capture) > MAX_CAPTURE:
            raise AssertionError("bounded PTY capture exceeded 8 MiB")
        interval.scan(capture)


def raw(slave):
    return not termios.tcgetattr(slave)[3] & (termios.ICANON | termios.ECHO)


def run_pty(command, env, work, steps, probe, timeout, exit_code=0, daemon=None,
            failure_status_command=None):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 80, 120, 0, 0))
    initial = termios.tcgetattr(slave)
    # Model a pre-existing shell prompt in the primary buffer. A real alternate
    # screen must hide it for every screen, including intermediate transitions.
    os.write(slave, MARKER + b"\r\n")
    child = None
    capture = bytearray()
    interval = Interval()
    evidence = []
    deadline = time.monotonic() + timeout
    capture_path = work / "terminal.capture"
    last_grid = ""
    previous_grid = None
    action_offset = 0
    failure = None

    def screen():
        nonlocal last_grid
        capture_path.write_bytes(capture)
        if probe is None:  # Self-test fixture only; never production rendering.
            last_grid = bytes(capture).decode("utf-8", "replace")
        else:
            rows, cols, _, _ = struct.unpack(
                "HHHH", fcntl.ioctl(slave, termios.TIOCGWINSZ, b"\0" * 8))
            result = subprocess.run([probe, str(capture_path), str(cols), str(rows)],
                                    capture_output=True, check=True, timeout=15, env=env)
            if len(result.stdout) > MAX_CAPTURE:
                raise AssertionError("grid probe exceeded output bound")
            last_grid = result.stdout.decode("utf-8", "replace")
            if MARKER.decode() in last_grid:
                raise AssertionError("primary-buffer shell sentinel exposed during journey")
        (work / "last-grid.txt").write_text(last_grid)
        return last_grid.lower()

    try:
        # Establish a controlling tty, like an interactive foreground shell child.
        # This single-threaded harness is the only place we use preexec_fn.
        def child_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        child = subprocess.Popen(command, cwd=work / "repo" if (work / "repo").is_dir() else work,
                                 env=env, stdin=slave,
                                 stdout=slave, stderr=slave, preexec_fn=child_terminal)
        for index, step in enumerate(steps):
            end = min(deadline, time.monotonic() + step.get("timeout", 45))
            matched = False
            hold_until = None
            while time.monotonic() < end:
                interval.handoff = bool(step.get("handoff"))
                pump(master, capture, interval)
                if interval.leaves and not interval.active and not interval.handoff:
                    raise AssertionError(f"screen left before step {index + 1} completed")
                if interval.active:
                    if not raw(slave):
                        raise AssertionError("raw input released before final exit")
                    grid = screen()
                    rejected = [value for value in step.get("reject", []) if value.lower() in grid]
                    if rejected:
                        raise AssertionError(f"step {index + 1} forbidden side effect: {rejected}")
                    forbidden = any(value.lower() in grid for value in step.get("forbid", []))
                    # Prior-screen text may legitimately remain until the next paint.
                    # Do not treat it as a match or advance the journey prematurely.
                    if not forbidden and all(value.lower() in grid for value in step["expect"]):
                        if hold_until is None:
                            hold_until = time.monotonic() + step.get("hold", 0.5)
                        if time.monotonic() >= hold_until:
                            matched = True
                            break
                    else:
                        hold_until = None
                if child.poll() is not None:
                    raise AssertionError(f"application exited before step {index + 1}")
            if not matched:
                raise AssertionError(f"timeout waiting for rendered step {index + 1}: {step['expect']}")
            cols, rows = struct.unpack(
                "HHHH", fcntl.ioctl(slave, termios.TIOCGWINSZ, b"\0" * 8))[1::-1]
            output = terminal_output_evidence(bytes(capture[action_offset:]), cols, rows)
            check_step_observation(step, previous_grid, last_grid, output)
            (work / f"step-{index + 1:02d}-grid.txt").write_text(last_grid)
            evidence.append({"step": index + 1, "expect": step["expect"], "raw": True,
                             "terminal_output_since_action": output,
                             "screen_changed": previous_grid is not None and last_grid != previous_grid,
                             "mouse": step.get("mouse"),
                             "reject": step.get("reject", []),
                             "daemon_signal": step.get("daemon_signal"),
                             "alternate_screen_enters": interval.enters})
            previous_grid = last_grid
            action_offset = len(capture)
            if step.get("mouse"):
                os.write(master, mouse_bytes(step["mouse"], last_grid, cols, rows))
            if step.get("daemon_signal"):
                if daemon is None or daemon.poll() is not None:
                    raise AssertionError("daemon fault injection requires unreaped harness-created child")
                daemon.send_signal(signal.SIGSTOP if step["daemon_signal"] == "suspend" else signal.SIGCONT)
            if "resize" in step:
                cols, rows = step["resize"]
                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
                # Child is still our unreaped direct process.
                if child.poll() is None:
                    child.send_signal(signal.SIGWINCH)
            if step.get("send"):
                os.write(master, step["send"].encode())
        end = min(deadline, time.monotonic() + 10)
        while child.poll() is None and time.monotonic() < end:
            pump(master, capture, interval)
        if child.poll() is None:
            raise AssertionError("application failed to exit; cleanup is not acceptance")
        # Drain after child termination, before checking leave sequence/restoration.
        while select.select([master], [], [], 0.05)[0]:
            before = len(capture)
            pump(master, capture, interval, 0)
            if len(capture) == before:
                break
        interval.finished()
        if child.returncode != exit_code:
            raise AssertionError(f"exit status {child.returncode}, expected {exit_code}")
        if termios.tcgetattr(master) != initial:
            raise AssertionError("application did not restore exact original tty attributes")
        # After the controlling session exits Darwin revokes slave ioctl access;
        # tty attributes remain observable through the master. Shell output can
        # be tested while the child is alive, but not by writing a revoked slave.
    except (AssertionError, OSError, subprocess.SubprocessError) as error:
        failure = str(error)
        # Read public bounded catalog status while the failing foreground and
        # its exact isolated daemon are still alive. Never retry the model turn.
        capture_path.write_bytes(capture)
        if failure_status_command is not None:
            try:
                result = subprocess.run(failure_status_command, env=env, cwd=work,
                                        capture_output=True, timeout=20)
                (work / "failure-session-status.txt").write_bytes(
                    result.stdout + result.stderr)
                (work / "failure-session-status-exit.txt").write_text(str(result.returncode))
            except subprocess.SubprocessError as diagnostic_error:
                (work / "failure-session-status.txt").write_text(str(diagnostic_error))
        raise
    finally:
        # Check restoration before forced cleanup above; never conceal a defect by
        # restoring termios ourselves on the private PTY.
        cleanup_error = None
        try:
            stop(child)
        except subprocess.SubprocessError as error:
            cleanup_error = str(error)
        capture_path.write_bytes(capture)
        (work / "evidence.json").write_text(json.dumps({
            "cleanup_error": cleanup_error,
            "command": command, "steps": evidence, "enters": interval.enters,
            "leaves": interval.leaves, "failure": failure,
            "step_timeout_seconds": timeout,
            "tty_restored": termios.tcgetattr(master) == initial,
            "exit_code": child.returncode if child else None}, indent=2))
        if cleanup_error and failure is None:
            raise AssertionError(f"owned child cleanup failed: {cleanup_error}")
        os.close(master)
        os.close(slave)


def run_restart(command, env, work, steps, probe, timeout, exit_code=0, daemon=None):
    """Start a fresh process on the same fixture after preserving first-leg evidence."""
    for name in ("terminal.capture", "last-grid.txt", "evidence.json"):
        (work / name).rename(work / ("first-" + name))
    for path in work.glob("step-*-grid.txt"):
        path.rename(path.with_name("first-" + path.name))
    run_pty(command, env, work, steps, probe, timeout, exit_code, daemon)


def isolated_fixture(work, library, model, fixture="fake"):
    home = work / "home"
    config_dir = home / ".config"
    state = work / "state"
    tmp = work / "tmp"
    for directory in (home, config_dir, state, tmp):
        directory.mkdir(parents=True, exist_ok=True)
    if fixture != "xai":
        manifest_dir = config_dir / "bcode/plugins/fake-provider"
        manifest_dir.mkdir(parents=True)
        template = Path(__file__).resolve().parent.parent / "plugins/fake-provider-plugin/bcode-plugin.toml"
        manifest, count = re.subn(r'(?m)^library\s*=\s*"[^"]+"$',
                                  "library = " + json.dumps(library), template.read_text())
        if count != 1:
            raise AssertionError("fake manifest must have one runtime library")
        (manifest_dir / "bcode-plugin.toml").write_text(manifest)
    config = work / "bcode.toml"
    text = '''[plugins]
default = "none"
enabled = ["bcode.fake-provider", "bcode.default-agents", "bcode.code_review"]
disabled = ["bcode.openai-compatible", "bcode.bedrock"]
[model.prompt_cache]
mode = "off"
[daemon]
idle_shutdown = false
'''
    if fixture in ("completion", "completion-review"):
        text = text.replace('"bcode.code_review"',
                            '"bcode.code_review", "bcode.file-references", "bcode.filesystem", '
                            '"bcode.question", "bcode.loop", "bcode.workflow"')
        text += '[tui.interactions]\nplacement = "pinned"\n'
        # Debug nested IPC needs headroom; keep the production search budget bounded.
        text += '[file_references]\ntimeout_ms = 1000\n'
        (work / "completion-target.txt").write_text("offline path completion fixture\n")
    if fixture == "review":
        # Questions about the code run in real Bcode sessions.
        text = text.replace('"bcode.code_review"', '"bcode.code_review", "bcode.harness"')
    if fixture == "blims":
        text = text.replace('"bcode.code_review"', '"bcode.code_review", "bcode.blims"')
        text += '[client]\nrequest_timeout_secs = 2\n'
    if fixture == "xai":
        text = text.replace('"bcode.fake-provider", ', '"bcode.openai-compatible", ')
        text = text.replace('disabled = ["bcode.openai-compatible", "bcode.bedrock"]',
                            'disabled = ["bcode.bedrock"]')
    if model:
        text += '''[model]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"
'''
    if fixture in ("completion", "completion-review"):
        text += '''
[model.profiles.pty-completion]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"
'''
    if fixture in ("review", "completion", "completion-review"):
        # A submitted parent emits a visible sentinel in completion fixtures.
        # Initial question tool requests still use the normal fake tool directive.
        answer = "CONTINUOUS_REVIEW_ANSWER" if fixture == "review" else "COMPLETION_PARENT_SUBMITTED"
        script = {"steps": [
            {"event": {"type": "text_delta", "text": answer}},
            {"event": {"type": "turn_finished", "stop_reason": "end_turn"}}]}
        if fixture == "review":
            text += "[model.settings]\nfake_event_script = " + json.dumps(json.dumps(script)) + "\n"
        else:
            scripts = {"@completion": script}
            text += "[model.profiles.pty-completion.settings]\nfake_prompt_tool_directives = \"true\"\nfake_event_scripts_by_prompt_marker = " + json.dumps(json.dumps(scripts)) + "\n"
    config.write_text(text)
    # Allowlist, not an inherited environment with an incomplete secret blacklist.
    env = {"HOME": str(home), "XDG_CONFIG_HOME": str(config_dir),
           "XDG_STATE_HOME": str(home / ".local/state"), "XDG_CACHE_HOME": str(home / ".cache"),
           "TMPDIR": str(tmp), "PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "TERM": "xterm-256color",
           "LANG": "en_US.UTF-8", "BCODE_CONFIG": str(config), "BCODE_STATE_DIR": str(state),
           "RUST_MIN_STACK": "16777216", "BCODE_DISABLE_REMOTE_MODEL_CATALOG": "1"}
    if fixture == "xai":
        env["XAI_API_KEY"] = "bcode-pty-fake-not-a-real-credential"
        env.update({"HTTP_PROXY": "http://127.0.0.1:1", "HTTPS_PROXY": "http://127.0.0.1:1",
                    "ALL_PROXY": "http://127.0.0.1:1", "NO_PROXY": "localhost,127.0.0.1"})
    if fixture in ("review", "completion", "completion-review"):
        # Keep search workspace separate from harness state/config and build artifacts.
        git_env = {**env, "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null"}
        repo = work / "repo"
        repo.mkdir()
        subprocess.run(["/usr/bin/git", "init", "--quiet", str(repo)], env=git_env,
                       check=True, timeout=5, capture_output=True)
        (repo / "continuous-review-fixture.txt").write_text("CONTINUOUS_REVIEW_FILE\n")
        if fixture in ("completion", "completion-review"):
            (repo / "completion-target.txt").write_text("offline path completion fixture\n")
        tracked = ["continuous-review-fixture.txt"]
        if fixture == "review":
            # Preserve the original first file/first line for existing journeys.
            # Enough content to scroll independently of the tree, plus sibling
            # folders, Unicode and a path wider than a normal sidebar.
            review_files = {
                "nested/alpha/scroll-target.txt": "".join(
                    f"REVIEW_SCROLL_LINE_{line:03d}\n" for line in range(1, 161)),
                "nested/beta/sibling.txt": "REVIEW_SIBLING_CONTENT\n",
                "nested/日本語/長い名前-" + "review-path-" * 7 + "🦀.txt":
                    "REVIEW_UNICODE_CONTENT café 日本語 🦀 e\u0301\n",
            }
            for name, content in review_files.items():
                target = repo / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(content)
                tracked.append(name)
            # Also give the unstaged preset a distinct, truthful tracked edit.
            # The index remains the staged fixture, so old staged cases are intact.
        if fixture in ("completion", "completion-review"):
            tracked.append("completion-target.txt")
        subprocess.run(["/usr/bin/git", "-C", str(repo), "add", "--", *tracked],
                       env=git_env, check=True, timeout=5, capture_output=True)
        if fixture == "review":
            (repo / "nested/beta/sibling.txt").write_text(
                "REVIEW_SIBLING_CONTENT\nREVIEW_UNSTAGED_CONTENT\n")
    return env, config, state


def plan_checked(plan):
    if not isinstance(plan, dict) or set(plan) - {"name", "fixture", "model", "args", "steps", "restart_steps", "exit_code"}:
        raise ValueError("unknown plan fields")
    if plan.get("fixture", "fake") not in ("fake", "review", "xai", "blims", "completion", "completion-review"):
        raise ValueError("unsupported fixture")
    if plan.get("fixture") == "xai" and plan != CASES["xai-model-chat"]:
        raise ValueError("xAI fixture restricted to fixed no-prompt model-choice journey")
    args = plan.get("args", [])
    if args not in ([], ["--onboard"], ["review"], ["blims", "enter"]):
        raise ValueError("plan args must select startup, --onboard, review, or blims enter; no config/state overrides")
    steps = plan.get("steps", [])
    restart = plan.get("restart_steps", [])
    if "restart_steps" in plan and (not restart or plan.get("fixture") != "review" or args != ["review"]):
        raise ValueError("restart_steps requires a review fixture/journey")
    if not isinstance(steps, list) or not isinstance(restart, list) or not steps or len(steps) + len(restart) > 32:
        raise ValueError("plan requires 1..32 total steps")
    for step in steps + restart:
        if not isinstance(step, dict) or set(step) - {"expect", "forbid", "reject", "send", "resize", "timeout", "hold", "daemon_signal", "mouse", "screen_change", "no_rewrite", "handoff"}:
            raise ValueError("unknown step fields")
        for field in ("expect", "forbid", "reject"):
            values = step.get(field, [])
            if not isinstance(values, list) or any(not isinstance(v, str) or not v or len(v) > 256 for v in values):
                raise ValueError("expect/forbid must contain bounded nonempty strings")
        if not step.get("expect") or len(step["expect"]) > 16:
            raise ValueError("each step requires 1..16 visible expectations")
        if not isinstance(step.get("send", ""), str) or len(step.get("send", "")) > 4096:
            raise ValueError("send must be bounded text")
        if not 0 < step.get("timeout", 45) <= 60:
            raise ValueError("step timeout must be in (0,60]")
        if not 0 <= step.get("hold", 0) < step.get("timeout", 45):
            raise ValueError("hold must be nonnegative and shorter than step timeout")
        if step.get("daemon_signal") not in (None, "suspend", "resume"):
            raise ValueError("unsupported daemon signal")
        if "screen_change" in step and step["screen_change"] not in ("none", "some"):
            raise ValueError("screen_change must be none or some")
        if "no_rewrite" in step and not isinstance(step["no_rewrite"], bool):
            raise ValueError("no_rewrite must be boolean")
        if "handoff" in step and not isinstance(step["handoff"], bool):
            raise ValueError("handoff must be boolean")
        if "mouse" in step:
            mouse = step["mouse"]
            if not isinstance(mouse, dict) or set(mouse) - {"kind", "column", "row", "row_text", "end_text", "count"}:
                raise ValueError("unknown mouse fields")
            if mouse.get("kind", "click") not in ("click", "move", "drag", "wheel_up", "wheel_down"):
                raise ValueError("unsupported mouse kind")
            if ("row" in mouse) == ("row_text" in mouse):
                raise ValueError("mouse requires exactly one row or row_text")
            for field in ("column", "row", "count"):
                value = mouse.get(field, 1)
                if type(value) is not int or not 1 <= value <= (20 if field == "count" else 500):
                    raise ValueError("mouse coordinates/count must be bounded integers")
            if "column" not in mouse:
                raise ValueError("mouse requires explicit terminal-cell column")
            for field in ("row_text", "end_text"):
                if field in mouse and (not isinstance(mouse[field], str) or not 1 <= len(mouse[field]) <= 256):
                    raise ValueError("mouse anchors must be bounded nonempty strings")
            if ("end_text" in mouse) != (mouse.get("kind") == "drag"):
                raise ValueError("drag requires end_text; other mouse kinds must not have it")
            if "count" in mouse and mouse.get("kind") not in ("wheel_up", "wheel_down", "move"):
                raise ValueError("count is only for wheel/move")
            if "send" in step or "resize" in step:
                raise ValueError("mouse must be the sole input action in its step")
        if "resize" in step:
            size = step["resize"]
            if not isinstance(size, list) or len(size) != 2 or any(not isinstance(v, int) or not 1 <= v <= 500 for v in size):
                raise ValueError("resize requires [columns,rows] in 1..500")
    if not isinstance(plan.get("model", False), bool) or not isinstance(plan.get("exit_code", 0), int):
        raise ValueError("model must be boolean; exit_code must be integer")
    return plan


def self_test():
    interval = Interval()
    interval.scan(b"\x1b[?1049")
    interval.scan(b"\x1b[?1049h")
    interval.scan(b"\x1b[?1049h\x1b[?1049l")
    interval.finished()
    try:
        interval.scan(b"\x1b[?1049h\x1b[?1049l\x1b[?1049h")
    except AssertionError:
        pass
    else:
        raise AssertionError("self-test failed to reject terminal reentry")
    handoff = Interval()
    handoff.scan(b"\x1b[?1049h")
    handoff.handoff = True
    handoff.scan(b"\x1b[?1049h\x1b[?1049l\x1b[?1049h\x1b[?1049l\x1b[?1049h")
    handoff.handoff = False
    handoff.scan(b"\x1b[?1049h\x1b[?1049l\x1b[?1049h\x1b[?1049l\x1b[?1049h\x1b[?1049l")
    handoff.finished()
    try:
        handoff.scan(b"\x1b[?1049h\x1b[?1049l\x1b[?1049h\x1b[?1049l\x1b[?1049h\x1b[?1049l"
                     b"\x1b[?1049h")
    except AssertionError:
        pass
    else:
        raise AssertionError("self-test accepted reentry outside a handoff")
    for plan in CASES.values():
        plan_checked(plan)
    # A workspace title also appears in its preset and details. Include row
    # health so a resume click cannot accidentally create another review.
    for health in ("active", "drafts"):
        home_grid = (
            " Bcode Reviews\n"
            "  Staged changes  [s]\n"
            f"> {health}   Staged changes  · just now  · {health}\n"
            "                                         Staged changes\n"
            "                                         sources: 1/1 included\n"
        )
        assert mouse_bytes({"row_text": f"{health}   Staged changes", "column": 12},
                           home_grid, 120, 80) == b"\x1b[<0;12;3M\x1b[<0;12;3m"
    grid = "界🦀 heading\n target leaf\n range end\n"
    assert mouse_bytes({"row_text": "target leaf", "column": 5}, grid, 20, 3) == b"\x1b[<0;5;2M\x1b[<0;5;2m"
    assert mouse_bytes({"kind": "drag", "row_text": "target leaf", "end_text": "range end", "column": 9}, grid, 20, 3) == b"\x1b[<0;9;2M\x1b[<32;9;3M\x1b[<0;9;3m"
    assert mouse_bytes({"kind": "wheel_down", "row": 2, "column": 5, "count": 2}, grid, 20, 3) == b"\x1b[<65;5;2M" * 2
    assert mouse_bytes({"kind": "wheel_up", "row": 2, "column": 5}, grid, 20, 3) == b"\x1b[<64;5;2M"
    assert mouse_bytes({"kind": "move", "row": 2, "column": 5}, grid, 20, 3) == b"\x1b[<35;5;2M"
    for mouse, text in (({"row_text": "target", "column": 1}, "target\ntarget"),
                        ({"row_text": "clipped", "column": 1}, grid),
                        ({"row": 4, "column": 1}, grid)):
        try:
            mouse_bytes(mouse, text, 20, 3)
        except AssertionError:
            pass
        else:
            raise AssertionError("mouse accepted ambiguous/missing/offscreen target")
    quiet = terminal_output_evidence(b"\x1b[?2026h\x1b[?2026l", 120, 80)
    check_step_observation({"screen_change": "none", "no_rewrite": True}, grid, grid, quiet)
    check_step_observation({"screen_change": "some"}, grid, grid + "changed", quiet)
    assert terminal_output_evidence(b"\x1b]0;title\x07", 120, 80)["printable_characters"] == 0
    for step, before, after, output in (
            ({"screen_change": "some"}, grid, grid, quiet),
            ({"screen_change": "none"}, grid, "changed", quiet),
            ({"screen_change": "none"}, None, grid, quiet),
            ({"no_rewrite": True}, grid, grid, terminal_output_evidence(b"\x1b[2J", 120, 80)),
            ({"no_rewrite": True}, grid, grid, terminal_output_evidence(b"x" * 7200, 120, 80))):
        try:
            check_step_observation(step, before, after, output)
        except AssertionError:
            pass
        else:
            raise AssertionError("observation guard accepted no-op/clear/whole-screen rewrite")
    for extra in ({"mouse": {"kind": "drag", "row": 1, "column": 2}},
                  {"mouse": {"row": 1, "row_text": "a", "column": 2}},
                  {"mouse": {"row": 1, "column": 0}},
                  {"mouse": {"row": 1, "column": 2}, "send": "x"},
                  {"no_rewrite": "yes"}, {"screen_change": "maybe"}):
        try:
            plan_checked({"steps": [{"expect": ["ready"], **extra}]})
        except ValueError:
            pass
        else:
            raise AssertionError("invalid generic step fields accepted")
    work = Path(tempfile.mkdtemp(prefix="bcode-continuous-pty-selftest-"))
    fixture_work = work / "review-fixture"
    env, _, _ = isolated_fixture(fixture_work, "/offline/not-loaded.dylib", True, "review")
    repo = fixture_work / "repo"
    staged = subprocess.run(["/usr/bin/git", "diff", "--cached", "--stat"],
                            cwd=repo, env=env, check=True, capture_output=True, timeout=5)
    assert b"4 files changed" in staged.stdout
    unstaged = subprocess.run(["/usr/bin/git", "diff", "--", "nested/beta/sibling.txt"],
                              cwd=repo, env=env, check=True, capture_output=True, timeout=5)
    assert b"+REVIEW_UNSTAGED_CONTENT" in unstaged.stdout
    assert (repo / "continuous-review-fixture.txt").read_text() == "CONTINUOUS_REVIEW_FILE\n"
    assert "REVIEW_SCROLL_LINE_160" in (repo / "nested/alpha/scroll-target.txt").read_text()
    assert len(list((repo / "nested/日本語").glob("*🦀.txt"))) == 1
    fixture = (
        "import os,termios,tty; old=termios.tcgetattr(0); tty.setraw(0); "
        "os.write(1,b'\\x1b[?1049hHARNESS_READY'); os.read(0,1); "
        "os.write(1,b'\\x1b[?1049l'); termios.tcsetattr(0,termios.TCSANOW,old)"
    )
    run_pty([sys.executable, "-c", fixture], os.environ.copy(), work,
            [{"expect": ["HARNESS_READY"], "hold": 0.2, "send": "q"}], None, 5)
    first_evidence = (work / "evidence.json").read_bytes()
    run_restart([sys.executable, "-c", fixture.replace("HARNESS_READY", "RESTART_READY")],
                os.environ.copy(), work,
                [{"expect": ["RESTART_READY"], "hold": 0.1, "send": "q"}], None, 5)
    assert (work / "first-evidence.json").read_bytes() == first_evidence
    assert "HARNESS_READY" in (work / "first-step-01-grid.txt").read_text()
    assert "RESTART_READY" in (work / "step-01-grid.txt").read_text()
    # Exercise generic input + since-action accounting through a real private PTY.
    pointer_work = work / "pointer"
    pointer_work.mkdir()
    pointer_fixture = fixture.replace(
        "os.read(0,1);",
        "assert os.read(0,4096)==b'\\x1b[<35;80;12M'; os.read(0,1);")
    run_pty([sys.executable, "-c", pointer_fixture], os.environ.copy(), pointer_work,
            [{"expect": ["HARNESS_READY"], "hold": 0.1,
              "mouse": {"kind": "move", "column": 80, "row": 12}},
             {"expect": ["HARNESS_READY"], "screen_change": "none", "no_rewrite": True,
              "hold": 0.2, "send": "q"}], None, 5)
    pointer_evidence = json.loads((pointer_work / "evidence.json").read_text())
    assert pointer_evidence["steps"][1]["terminal_output_since_action"]["bytes"] == 0
    for name, payload in (("pointer-clear", b"\x1b[2J"),
                          ("pointer-rewrite", b"x" * 7200)):
        rewrite_work = work / name
        rewrite_work.mkdir()
        source = fixture.replace("os.read(0,1);", f"os.read(0,4096); os.write(1,{payload!r}); os.read(0,1);")
        try:
            run_pty([sys.executable, "-c", source], os.environ.copy(), rewrite_work,
                    [{"expect": ["HARNESS_READY"], "hold": 0.1,
                      "mouse": {"kind": "move", "column": 80, "row": 12}},
                     {"expect": ["HARNESS_READY"], "no_rewrite": True,
                      "hold": 0.2, "send": "q"}], None, 5)
        except AssertionError as error:
            if "whole-screen erase/rewrite" not in str(error):
                raise AssertionError(f"wrong rewrite failure: {error}") from error
        else:
            raise AssertionError("PTY rewrite guard failed")
    # Prove failures are not hidden by forced cleanup or historical captures.
    failures = {
        "reentry": (fixture.replace("os.read(0,1);", "os.write(1,b'\x1b[?1049l\x1b[?1049h'); os.read(0,1);"),
                    "alternate screen re-entered"),
        "raw-release": (fixture.replace("os.read(0,1);", "termios.tcsetattr(0,termios.TCSANOW,old); os.read(0,1);"),
                        "raw input released"),
        "unrestored": (fixture.replace("termios.tcsetattr(0,termios.TCSANOW,old)", "None"),
                       "did not restore exact original tty"),
        "missing-screen": (fixture.replace("HARNESS_READY", "NOT_READY"),
                           "timeout waiting"),
    }
    for name, (source, expected) in failures.items():
        case_work = work / name
        case_work.mkdir()
        try:
            run_pty([sys.executable, "-c", source], os.environ.copy(), case_work,
                    [{"expect": ["HARNESS_READY"], "hold": 0, "send": "q", "timeout": 0.5}], None, 2)
        except AssertionError as error:
            if expected not in str(error):
                raise AssertionError(f"wrong {name} failure: {error}") from error
        else:
            raise AssertionError(f"self-test failed to reject {name}")
    reject_work = work / "rejected-side-effect"
    reject_work.mkdir()
    try:
        run_pty([sys.executable, "-c", fixture], os.environ.copy(), reject_work,
                [{"expect": ["HARNESS_READY"], "reject": ["HARNESS_READY"],
                  "hold": 0, "send": "q"}], None, 2)
    except AssertionError as error:
        if "forbidden side effect" not in str(error):
            raise AssertionError(f"wrong reject failure: {error}") from error
    else:
        raise AssertionError("self-test failed to reject a forbidden side effect")
    error_work = work / "initializer-error"
    error_work.mkdir()
    run_pty([sys.executable, "-c", fixture + "; raise SystemExit(1)"],
            os.environ.copy(), error_work,
            [{"expect": ["HARNESS_READY"], "send": "q"}], None, 5, exit_code=1)
    print(f"harness self-test PASS (not product acceptance); artifacts: {work}")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary")
    parser.add_argument("--grid-probe")
    parser.add_argument("--fake-library")
    parser.add_argument("--binary-sha256", help="Coordinator-admitted executable digest")
    parser.add_argument("--grid-probe-sha256", help="Coordinator-admitted grid probe digest")
    parser.add_argument("--fake-library-sha256", help="Coordinator-admitted fake plugin digest")
    cases = parser.add_mutually_exclusive_group()
    cases.add_argument("--case", choices=sorted(CASES), default=None)
    cases.add_argument("--plan", type=Path)
    parser.add_argument("--timeout", type=float, default=90)
    parser.add_argument("--cold-start", action="store_true", help="Let the TUI own isolated daemon startup")
    parser.add_argument("--self-test", action="store_true")
    options = parser.parse_args()
    if options.self_test:
        self_test()
        return
    if not 0 < options.timeout <= 300:
        parser.error("timeout must be in (0,300]")
    if options.plan and options.plan.stat().st_size > 65536:
        parser.error("plan exceeds 64 KiB")
    plan = plan_checked(json.loads(options.plan.read_text()) if options.plan else CASES[options.case or "quick-cancel"])
    for field in ("binary", "grid_probe") + (() if plan.get("fixture") == "xai" else ("fake_library",)):
        value = getattr(options, field)
        if not value or not Path(value).is_absolute() or not Path(value).is_file():
            parser.error(f"--{field.replace('_', '-')} requires an explicit absolute built artifact")
    for field in ("binary", "grid_probe") + (() if plan.get("fixture") == "xai" else ("fake_library",)):
        expected = getattr(options, field + "_sha256")
        actual = digest(getattr(options, field))
        if expected and actual != expected.lower():
            parser.error(f"{field} SHA256 differs from coordinator admission")
    if not os.access(options.binary, os.X_OK) or not os.access(options.grid_probe, os.X_OK):
        parser.error("binary and grid probe must be executable")
    # Enforce no IP contact, not just a fake credential or proxy preference. IPC
    # Unix sockets remain allowed. Never silently weaken this on another platform.
    sandbox = []
    if sys.platform == "darwin" and Path("/usr/bin/sandbox-exec").is_file():
        sandbox = ["/usr/bin/sandbox-exec", "-p",
                   '(version 1)(allow default)(deny network* (remote ip "*:*") (local ip "*:*") )']
        subprocess.run(sandbox + ["/usr/bin/true"], check=True, timeout=5,
                       capture_output=True)
    elif plan.get("fixture") == "xai":
        parser.error("xAI no-contact case requires Darwin sandbox-exec IP confinement")
    work = Path(tempfile.mkdtemp(prefix="bcode-continuous-pty-"))
    print(f"isolated artifacts: {work}", flush=True)
    env, config, state = isolated_fixture(work, options.fake_library, plan.get("model", False),
                                          plan.get("fixture", "fake"))
    base = [options.binary, "--config-only", str(config), "--state-root", str(state)]
    if plan.get("fixture") in ("completion", "completion-review"):
        base += ["--profile", "pty-completion"]
    if plan.get("fixture") != "xai":
        base.append("--no-credential-discovery")
    identity = subprocess.run(sandbox + base + ["artifact-id"], env=env, cwd=work,
                              check=True, capture_output=True, timeout=60).stdout.decode().strip()
    (work / "artifacts.json").write_text(json.dumps({
        "artifact_id": identity, "binary_sha256": digest(options.binary),
        "grid_probe_sha256": digest(options.grid_probe),
        "fake_library_sha256": digest(options.fake_library) if plan.get("fixture") != "xai" else None,
        "cold_start": options.cold_start,
        "case": plan.get("name", options.case or "custom"),
        "network_confinement": "Darwin sandbox denies IP network" if sandbox else "fake-only provider"}, indent=2))
    daemon = None
    try:
        with (work / "daemon.log").open("wb") as log:
            if not options.cold_start:
                daemon = subprocess.Popen(sandbox + base + ["server", "run"],
                                          cwd=work / "repo" if (work / "repo").is_dir() else work, env=env,
                                          stdin=subprocess.DEVNULL, stdout=log, stderr=log)
                end = time.monotonic() + min(60, options.timeout)
                while time.monotonic() < end:
                    if daemon.poll() is not None:
                        raise AssertionError("isolated daemon exited before readiness; see daemon.log")
                    if (work / "daemon.log").stat().st_size > MAX_CAPTURE:
                        raise AssertionError("daemon startup log exceeded bound")
                    if b"server ready; accepting clients" in (work / "daemon.log").read_bytes():
                        break
                    time.sleep(0.1)
                else:
                    raise AssertionError("isolated daemon readiness timeout")
            if plan.get("fixture") == "blims":
                if options.cold_start:
                    raise ValueError("Blims fault fixture requires harness-owned daemon")
                result = subprocess.run(sandbox + base + ["blims", "create", "--json"],
                                        env=env, cwd=work, capture_output=True, timeout=60)
                (work / "blims-create.json").write_bytes(result.stdout)
                if result.returncode:
                    raise AssertionError("isolated Blims company.create failed")
                result = subprocess.run(sandbox + base + ["blims", "select-world", "cozy-startup-loft", "--json"],
                                        env=env, cwd=work, capture_output=True, timeout=60)
                (work / "blims-world.json").write_bytes(result.stdout)
                if result.returncode:
                    raise AssertionError("isolated Blims select-world failed")
                # Startup healthchecks keep their normal timeout; the short
                # operation deadline is selected only after isolated creation.
                config.write_text(config.read_text().replace("request_timeout_secs = 2", "request_timeout_secs = 5"))
            command = sandbox + base + plan.get("args", [])
            if plan.get("fixture") == "completion":
                status = subprocess.run(sandbox + base + ["model", "status"], env=env, cwd=work,
                                        check=True, capture_output=True, timeout=60)
                (work / "model-status.txt").write_bytes(status.stdout)
                if b"bcode.fake-provider" not in status.stdout or b"fake-echo" not in status.stdout:
                    raise AssertionError("completion fixture did not select the offline fake provider")
                created = subprocess.run(sandbox + base + ["session", "create", "completion-pty"],
                                         env=env, cwd=work / "repo", check=True, capture_output=True, timeout=60)
                session_id = created.stdout.decode().strip()
                command = sandbox + base + ["tui", session_id]
            run_pty(command, env, work, plan["steps"], options.grid_probe,
                    options.timeout, plan.get("exit_code", 0), daemon,
                    sandbox + base + ["--request-timeout-secs", "10", "session", "list",
                                      "--cwd", str(work / "repo"), "--json", "--with-status"])
            if plan.get("restart_steps"):
                # A second real process against the same isolated repo/state proves
                # persistence and clickable resume, not merely retained memory.
                run_restart(command, env, work, plan["restart_steps"], options.grid_probe,
                            options.timeout, plan.get("exit_code", 0), daemon)
    finally:
        if plan.get("fixture") == "blims" and daemon is not None and daemon.poll() is None:
            daemon.send_signal(signal.SIGCONT)
        stop(daemon)
        if options.cold_start:
            # Public application stop is pinned to this explicit artifact/state;
            # never inspect or signal an inferred daemon PID.
            result = subprocess.run(sandbox + base + ["server", "stop"],
                                    env=env, cwd=work, capture_output=True, timeout=60)
            (work / "cold-daemon-stop.txt").write_bytes(result.stdout + result.stderr)
            if result.returncode:
                raise AssertionError("isolated cold-start daemon cleanup failed")
    print(f"PASS: {plan.get('name', options.case or 'quick-cancel')} (only specified steps); artifacts: {work}")


if __name__ == "__main__":
    try:
        main()
    except (AssertionError, ValueError, OSError, subprocess.SubprocessError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        sys.exit(1)
