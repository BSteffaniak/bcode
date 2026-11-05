#!/usr/bin/env python3
"""Offline JSONL native-process double; never invokes a provider or Bcode.

The effect ledger is a test oracle, NOT recovery authority. It records only
fixture-chosen operation labels, never incoming request bodies or credentials.
All scripts, frames, waits, and output are bounded. Invalid protocol input is
reported with fixed diagnostics so even deliberately secret-bearing frames do
not leak through this fixture's own failure path.
"""
import argparse
import json
import os
from pathlib import Path
import re
import select
import sys
import time

MAX_BYTES = 65536
MAX_STEPS = 128
TIMEOUT = 5.0
LABEL = re.compile(r"[a-z0-9_-]{1,64}\Z")


def fail():
    print("fake native protocol mismatch", file=sys.stderr, flush=True)
    raise SystemExit(65)


def bounded_json(path):
    with open(path, "rb") as source:
        data = source.read(MAX_BYTES + 1)
    if len(data) > MAX_BYTES:
        fail()
    return json.loads(data)


def subset(expected, actual):
    if isinstance(expected, dict):
        return isinstance(actual, dict) and all(
            key in actual and subset(value, actual[key])
            for key, value in expected.items()
        )
    return expected == actual


def substitute(value, captured):
    if isinstance(value, str) and value.startswith("$"):
        return captured[value[1:]]
    if isinstance(value, dict):
        return {key: substitute(item, captured) for key, item in value.items()}
    if isinstance(value, list):
        return [substitute(item, captured) for item in value]
    return value


def read_frame():
    deadline = time.monotonic() + TIMEOUT
    frame = bytearray()
    # Use os.read rather than a buffered readline: the deadline also applies to
    # an unterminated frame and already-buffered subsequent frames are not lost.
    while len(frame) <= MAX_BYTES:
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([sys.stdin], [], [], remaining)[0]:
            fail()
        byte = os.read(sys.stdin.fileno(), 1)
        if not byte:
            fail()
        if byte == b"\n":
            return json.loads(frame)
        frame.extend(byte)
    fail()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scenario", required=True)
    parser.add_argument("--scripts", default=str(Path(__file__).with_name("scenarios.json")))
    parser.add_argument("--oracle", default=os.environ.get("BCODE_HARNESS_TEST_ORACLE"))
    args = parser.parse_args()
    script = bounded_json(args.scripts)[args.scenario]
    if not isinstance(script, list) or len(script) > MAX_STEPS:
        fail()
    if not args.oracle:
        fail()
    oracle = Path(args.oracle)
    oracle.mkdir(parents=True, exist_ok=True)
    captured = {}
    for step in script:
        if "expect" in step:
            frame = read_frame()
            if not subset(step["expect"], frame):
                fail()
            for name, key in step.get("capture", {}).items():
                captured[name] = frame[key]
        if "effect" in step:
            label = step["effect"]
            if not LABEL.fullmatch(label):
                fail()
            # Exclusive creation detects even a native retry with a changed ID.
            # A host must reconcile uncertainty, not resend the business action.
            try:
                with open(oracle / (label + ".effect"), "x", encoding="utf-8") as effect:
                    effect.write("observed\n")
                    effect.flush()
                    os.fsync(effect.fileno())
            except FileExistsError:
                (oracle / (label + ".duplicate")).touch()
                print("duplicate native effect attempted", file=sys.stderr, flush=True)
                raise SystemExit(73)
        if "gate" in step:
            label = step["gate"]
            if not LABEL.fullmatch(label):
                fail()
            (oracle / (label + ".waiting")).touch()
            deadline = time.monotonic() + TIMEOUT
            while not (oracle / (label + ".release")).exists():
                if time.monotonic() >= deadline:
                    fail()
                time.sleep(0.01)
        for event in step.get("emit", []):
            data = json.dumps(substitute(event, captured), separators=(",", ":"))
            if len(data.encode("utf-8")) > MAX_BYTES:
                fail()
            print(data, flush=True)
        if "raw" in step:
            data = step["raw"].encode("utf-8")
            if len(data) > MAX_BYTES:
                fail()
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()
        if "exit" in step:
            raise SystemExit(step["exit"])


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, TypeError, OSError):
        fail()
