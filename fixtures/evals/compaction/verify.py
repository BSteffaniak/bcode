#!/usr/bin/env python3
"""Check real exported canonical events; never manufacture compaction evidence.

Usage: verify.py first.jsonl second.jsonl restarted.jsonl
Inputs are actual eval transcript / public session export artifacts from one session.
This is an evidence verifier, not a daemon-driving harness or semantic LLM evaluation.
"""
import json
import pathlib
import sys


def verify(paths):
    events = {}
    for path in paths:
        for line in pathlib.Path(path).read_text().splitlines():
            if not line.strip():
                continue
            event = json.loads(line)
            key = (event["session_id"], event["sequence"])
            if key in events and events[key] != event:
                raise ValueError("conflicting canonical event identity")
            events[key] = event
    if len({key[0] for key in events}) != 1:
        raise ValueError("expected exactly one canonical session across restart")
    markers = []
    for event in sorted(events.values(), key=lambda event: event["sequence"]):
        name, kind = next(iter(event["kind"].items()))
        if name == "context_compacted":
            markers.append((event, json.loads(kind["summary"])))
        elif name == "provider_context_compacted":
            markers.append((event, json.loads(kind["snapshot"]["portable_summary"])))
    if len(markers) < 2:
        raise ValueError("need at least two real canonical compaction markers")
    if len({event["kind"][next(iter(event["kind"]))]["compacted_through_sequence"] for event, _ in markers}) < 2:
        raise ValueError("compaction coverage did not advance")
    for _, evidence in markers:
        if evidence["format"] != "bcode.compaction.scope.v2":
            raise ValueError("not structured scope evidence")
        if "COMPACTION_REQUEST_ONLY_CANARY_7391" in json.dumps(evidence):
            raise ValueError("request-only canary became portable content")
    first = json.loads(markers[0][1]["continuation"])
    latest = markers[-1][1]
    state = json.loads(latest["continuation"])
    if first["facts"].get("early") != "violet-7391" or state["facts"].get("early") != "violet-7391":
        raise ValueError("early fact did not survive repeated real compaction")
    statements = {scope["statement"] for scope in latest["current_scope"]}
    expected = {"No network access", "Read-only analysis; do not edit files"}
    if statements != expected:
        raise ValueError(f"incorrect final independent scope: {statements}")
    for scope in latest["current_scope"]:
        source = next((event for event in events.values() if event["sequence"] == scope["source_sequence"]), None)
        user = None if source is None else source["kind"].get("user_message", source["kind"].get("submission_activated"))
        if user is None:
            raise ValueError("scope must cite a real canonical user event or activated submission")
        if scope["statement"] not in user["text"]:
            raise ValueError("scope was not derived from the cited fixture directive")
    print(f"PASS: {len(markers)} advancing real markers; early fact, corrected role, independent restriction")
    print("LIMIT: caller must verify distinct daemon process/artifact identity and restarted request completion.")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    try:
        verify(sys.argv[1:])
    except (ValueError, KeyError, TypeError, OSError) as error:
        raise SystemExit(f"FAIL: {error}") from error
