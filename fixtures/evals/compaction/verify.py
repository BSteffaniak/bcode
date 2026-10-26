#!/usr/bin/env python3
"""Check real exported canonical events; never manufacture compaction evidence.

Usage: verify.py first.jsonl second.jsonl restarted.jsonl
Inputs are actual eval transcript / public session export artifacts from one session.
This is an evidence verifier, not a daemon-driving harness or semantic LLM evaluation.
"""
import json
import pathlib
import sys


def load_events(paths):
    """Merge real canonical exports, rejecting conflicting identities and sequence gaps."""
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
    if not events or len({key[0] for key in events}) != 1:
        raise ValueError("expected exactly one nonempty canonical session across restart")
    ordered = sorted(events.values(), key=lambda event: event["sequence"])
    if [event["sequence"] for event in ordered] != list(range(ordered[0]["sequence"], ordered[-1]["sequence"] + 1)):
        raise ValueError("canonical export sequence gap")
    return ordered


def markers(events):
    """Select only actual canonical local/provider compaction markers."""
    selected = []
    for event in events:
        if "context_compacted" in event["kind"]:
            selected.append((event, json.loads(event["kind"]["context_compacted"]["summary"])))
        elif "provider_context_compacted" in event["kind"]:
            selected.append((event, json.loads(event["kind"]["provider_context_compacted"]["snapshot"]["portable_summary"])))
    return selected


def verify_fixture_scope(events, through, evidence):
    """Check one accepted marker against canonical fixture directives, not model claims."""
    if evidence.get("format") != "bcode.compaction.scope.v2":
        raise ValueError("not structured scope evidence")
    if "COMPACTION_REQUEST_ONLY_CANARY_7391" in json.dumps(evidence):
        raise ValueError("request-only canary became portable content")
    current = {}
    ancestors = {}
    for event in events:
        if event["sequence"] > through:
            continue
        user = event["kind"].get("user_message", event["kind"].get("submission_activated"))
        if user is None:
            continue
        for line in user["text"].splitlines():
            if not line.startswith("BCODE_FIXTURE_SCOPE "):
                continue
            directive = json.loads(line.removeprefix("BCODE_FIXTURE_SCOPE "))
            key, statement = directive["key"], directive["statement"]
            if key in current:
                previous, previous_statement = current[key]
                if previous_statement == statement:
                    continue
                ancestors.setdefault(key, set()).add(previous)
            current[key] = (event["sequence"], statement)
    scopes = evidence["current_scope"]
    actual = [(scope["source_sequence"], scope["statement"]) for scope in scopes]
    if len(actual) != len(set(actual)) or set(actual) != set(current.values()):
        raise ValueError("scope does not reflect chronological latest canonical user directives")
    for scope in scopes:
        key = next(key for key, value in current.items()
                   if value == (scope["source_sequence"], scope["statement"]))
        supersedes = scope.get("supersedes", [])
        if len(supersedes) != len(set(supersedes)) or not set(supersedes) <= ancestors.get(key, set()):
            raise ValueError("supersession lacks same-key canonical user support")


def verify_scope(events, selected):
    """Verify retention, chronology, user attribution and request-only isolation."""
    if not selected:
        raise ValueError("need a real canonical compaction marker")
    for event, evidence in selected:
        payload = event["kind"].get("context_compacted", event["kind"].get("provider_context_compacted"))
        verify_fixture_scope(events, payload["compacted_through_sequence"], evidence)
    latest = selected[-1][1]
    state = json.loads(latest["continuation"])
    if state["facts"].get("early") != "violet-7391":
        raise ValueError("early fact did not survive real compaction")
    statements = {scope["statement"] for scope in latest["current_scope"]}
    expected = {"No network access", "Read-only analysis; do not edit files"}
    if statements != expected:
        raise ValueError(f"incorrect final independent scope: {statements}")


def verify(paths):
    events = load_events(paths)
    selected = markers(events)
    if len(selected) < 2:
        raise ValueError("need at least two real canonical compaction markers")
    if len({event["kind"][next(iter(event["kind"]))]["compacted_through_sequence"] for event, _ in selected}) < 2:
        raise ValueError("compaction coverage did not advance")
    verify_scope(events, selected)
    first = json.loads(selected[0][1]["continuation"])
    if first["facts"].get("early") != "violet-7391":
        raise ValueError("early fact did not survive first real compaction")
    print(f"PASS: {len(selected)} advancing real markers; early fact, corrected role, independent restriction")
    print("LIMIT: caller must verify distinct daemon process/artifact identity and restarted request completion.")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    try:
        verify(sys.argv[1:])
    except (ValueError, KeyError, TypeError, OSError) as error:
        raise SystemExit(f"FAIL: {error}") from error
