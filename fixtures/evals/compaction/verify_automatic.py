#!/usr/bin/env python3
"""Verify real automatic-driver artifacts; fixture controls are not canonical markers."""
import argparse
import hashlib
import json
from pathlib import Path

from verify import load_events, markers, verify_scope

CANARY = "COMPACTION_REQUEST_ONLY_CANARY_7391"


def digest(data):
    return hashlib.sha256(data).hexdigest()


def attempts(root):
    """Ready publication is atomic; incomplete attempt directories are not gates."""
    return sorted((path.parent for path in Path(root).glob("attempt-*/ready.json")),
                  key=lambda path: (path.joinpath("ready.json").stat().st_mtime_ns, path.name))


def capture(path):
    ready = json.loads((path / "ready.json").read_text())
    if ready.get("format") != "bcode.fake.compaction-control.v1":
        raise ValueError("unsupported provider control format")
    request_bytes = (path / "request.json").read_bytes()
    source_bytes = (path / "sources.jsonl").read_bytes()
    if digest(request_bytes) != ready["request_sha256"] or len(request_bytes) != ready["request_bytes"]:
        raise ValueError("provider request capture integrity mismatch")
    if digest(source_bytes) != ready["source_sha256"] or len(source_bytes) != ready["source_bytes"]:
        raise ValueError("provider source capture integrity mismatch")
    request = json.loads(request_bytes)
    if "provider_context" in request:
        raise ValueError("provider control leaked excluded credential/settings boundary")
    sources = [json.loads(line) for line in source_bytes.decode().splitlines() if line.strip()]
    coverage = json.loads((path / "coverage.json").read_text())
    expected = [{"sequence": item["sequence"], "provenance": item["provenance"],
                 "byte_offset": item["byte_offset"], "final_fragment": item["final_fragment"],
                 "text_bytes": len(item["text"].encode()), "text_sha256": digest(item["text"].encode())}
                for item in sources]
    if expected != coverage or not coverage:
        raise ValueError("missing or mismatched exact provider source coverage")
    if CANARY in source_bytes.decode():
        raise ValueError("request-only context entered portable synthesis source records")
    # Parse the actual provider message; separate coverage files are not sufficient evidence.
    prompts = [text for text in strings(request["messages"])
               if "\n\nSource fragments (JSON lines):\n" in text]
    if len(prompts) != 1:
        raise ValueError("missing actual attributed provider synthesis prompt")
    section = prompts[0].split("\n\nSource fragments (JSON lines):\n", 1)[1]
    actual = []
    for line in section.splitlines():
        if not line.strip() or line.startswith("Repair:"):
            continue
        actual.append(json.loads(line))
    if actual != sources:
        raise ValueError("provider request and source evidence differ")
    return request, sources


def strings(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for child in value.values():
            yield from strings(child)
    elif isinstance(value, list):
        for child in value:
            yield from strings(child)


def coverage_keys(path):
    _, sources = capture(path)
    # Identity is the interval, not its digest: altered bytes at the same offset are also replay.
    return {(item["sequence"], item["provenance"], item["byte_offset"], len(item["text"].encode()))
            for item in sources}


def check_no_replay(accepted, resumed):
    """Reject overlapping coverage even if fragment sizes differ across the interruption."""
    for seq, role, offset, length in accepted:
        for other_seq, other_role, other_offset, other_length in resumed:
            if (seq, role) == (other_seq, other_role) and max(offset, other_offset) < min(offset + length, other_offset + other_length):
                raise ValueError("accepted prefix coverage requested again after interruption")


def check_lossless(paths, events, through):
    fragments = {}
    for path in paths:
        _, sources = capture(path)
        for item in sources:
            if item["sequence"] is None:
                continue
            identity = (item["sequence"], item["provenance"], item["byte_offset"])
            if identity in fragments and fragments[identity] != item:
                raise ValueError("conflicting source fragment bytes")
            fragments[identity] = item
    by_sequence = {event["sequence"]: event for event in events}
    for sequence, event in by_sequence.items():
        user = event["kind"].get("user_message", event["kind"].get("submission_activated"))
        if user is None or sequence > through:
            continue
        pieces = sorted((item for key, item in fragments.items() if key[:2] == (sequence, "user")),
                        key=lambda item: item["byte_offset"])
        if not pieces:
            raise ValueError(f"covered canonical user source #{sequence} missing provider fragments")
        offset = 0
        for index, piece in enumerate(pieces):
            if piece["byte_offset"] != offset or piece["final_fragment"] != (index == len(pieces) - 1):
                raise ValueError("source fragment gap, overlap, or premature final fragment")
            offset += len(piece["text"].encode())
        if "".join(piece["text"] for piece in pieces) != f"#{sequence} user:\n{user['text']}":
            raise ValueError(f"canonical user source #{sequence} was clipped or rewritten")


def exported_requests(work, session):
    root = work / "captures" / session
    document = json.loads((root / "context-observations.captures.json").read_text())
    if document.get("schema") != "bcode.eval.trace_exports.v1":
        raise ValueError("unsupported public capture export format")
    requests = []
    for receipt in document["receipts"]:
        if receipt.get("status") != "exported" or receipt.get("completeness") != "complete":
            raise ValueError("request capture unavailable, unverified or truncated")
        name = receipt["file"]
        if Path(name).name != name:
            raise ValueError("unconfined public export artifact filename")
        data = (root / "context-observations.captures" / name).read_bytes()
        if len(data) != receipt["byte_len"] or digest(data) != receipt["sha256"]:
            raise ValueError("exported request bytes failed integrity verification")
        value = json.loads(data)
        if "messages" in value and "model_id" in value:
            requests.append((receipt["sequence"], value))
    if not requests:
        raise ValueError("no complete actual provider request exports")
    return requests


def accepted_checkpoint(events):
    """Read the implemented versioned host checkpoint from public canonical export only."""
    accepted = []
    for event in events:
        trace = event["kind"].get("trace_event", {}).get("trace", {})
        provider = trace.get("payload", {}).get("provider_event", {})
        if provider.get("event_type") != "compaction_synthesis_checkpoint_v1":
            continue
        envelope = json.loads(provider["detail"])
        if envelope.get("version") != 1 or trace.get("turn_id") is not None or trace.get("phase") != "model_provider_event":
            raise ValueError("unsupported or wrongly attributed canonical compaction checkpoint")
        if envelope["host_payload"] is None:
            continue
        payload = json.loads(envelope["host_payload"])
        if payload.get("format") != "bcode.local-compaction.v1":
            raise ValueError("unsupported host compaction carry format")
        if CANARY in envelope["host_payload"]:
            raise ValueError("request-only canary persisted in checkpoint carry")
        if payload["next_chunk"] >= 1:
            accepted.append((event, envelope, payload))
    if not accepted:
        raise ValueError("no canonical durable acceptance of first synthesis chunk; provider response_ready is insufficient")
    return accepted[-1]


def admitted_turn(value):
    """Resolve a real ordinary admission receipt, never infer target from session activity."""
    variant = value.get("admission", {})
    receipt = next((variant[key] for key in ("accepted", "existing", "deferred") if key in variant), None)
    if receipt is None or not isinstance(receipt.get("turn_id"), str):
        raise ValueError("missing accepted ordinary continuation turn receipt")
    return receipt["turn_id"]


def verify_automatic(work):
    work = Path(work)
    events = load_events([work / "seed.jsonl", work / "before-interruption.jsonl",
                          work / "interrupted.jsonl", work / "final.jsonl"])
    selected = markers(events)
    verify_scope(events, selected)
    if markers(load_events([work / "interrupted.jsonl"])):
        raise ValueError("original history authority replaced before synthesis completion")
    checkpoint_event, _, checkpoint = accepted_checkpoint(load_events([work / "before-interruption.jsonl"]))
    interruption = json.loads((work / "interruption.json").read_text())
    if checkpoint_event["sequence"] >= selected[-1][0]["sequence"]:
        raise ValueError("acceptance checkpoint must precede final atomic marker")
    resumed = json.loads((work / "resumed-attempt.json").read_text())["attempt"]
    control = work / "control"
    ordered = attempts(control)
    resumed_index = next(index for index, path in enumerate(ordered) if path.name == resumed)
    prefix = coverage_keys(control / interruption["first"])
    for path in ordered[resumed_index:]:
        check_no_replay(prefix, coverage_keys(path))
    # Sequential request advancement and carry-in are observed at the provider boundary.
    # This is an end-to-end reuse assertion, not an inference from response_ready alone.
    request, _ = capture(control / resumed)
    prior_prompts = [text for text in strings(request["messages"]) if "Prior structured synthesis: " in text]
    if len(prior_prompts) != 1:
        raise ValueError("no observable resumed carry-in synthesis")
    prior = json.loads(prior_prompts[0].split("Prior structured synthesis: ", 1)[1].split("\n\nSource fragments", 1)[0])
    # Supersedes is a per-response delta; the host clears it before each next prompt.
    expected_carry = json.loads(json.dumps(checkpoint["evidence"]))
    for scope in expected_carry["current_scope"]:
        scope["supersedes"] = []
    if prior != expected_carry:
        raise ValueError("resumed provider carry differs from durably accepted canonical checkpoint")
    if json.loads(prior["continuation"])["facts"].get("early") != "violet-7391":
        raise ValueError("accepted synthesis was not carried into resumed provider request")
    final_event, _ = selected[-1]
    kind = final_event["kind"][next(iter(final_event["kind"]))]
    check_lossless(ordered, events, kind["compacted_through_sequence"])
    requests = exported_requests(work, events[0]["session_id"])
    later = [request for sequence, request in requests if sequence > final_event["sequence"]]
    if not later:
        raise ValueError("missing actual post-marker provider continuation request")
    message_text = "\n".join(strings(later[-1]["messages"]))
    if "TAIL_CANARY_7391" not in message_text or "violet-7391" not in message_text:
        raise ValueError("actual continuation request lost recent tail or retained fact")
    if "Read-only analysis; do not edit files" not in message_text or "No network access" not in message_text:
        raise ValueError("actual continuation request lost corrected scope or independent restriction")
    target_turn = admitted_turn(json.loads((work / "followup-admission.jsonl").read_text()))
    if not any(event["sequence"] > final_event["sequence"] and
               event["kind"].get("model_turn_finished", {}).get("turn_id") == target_turn and
               event["kind"]["model_turn_finished"]["outcome"] == "completed" for event in events):
        raise ValueError("no canonical completed continuation after final marker")
    print(f"PASS: automatic multi-chunk {interruption['mode']}; accepted prefix reused, lossless user fragments, real continuation request")
    return {"synthesis_requests": len(ordered), "planned_chunks": len(checkpoint["manifest"])}


def canonical_user_workload(events):
    """Exact ordered user bytes, not session-local event offsets interleaved with traces.

    Each run's attribution/coverage is verified separately against its own canonical
    sequence numbers. Cross-run sequence equality is not a source-content contract.
    No source text (including paths, whitespace or Unicode) is normalized here.
    """
    return [user["text"] for event in events
            if (user := event["kind"].get("user_message", event["kind"].get("submission_activated"))) is not None]


def compare_packing(disabled, enabled):
    """Reverify two real isolated runs and compare identical canonical user workload coverage."""
    roots = [Path(disabled), Path(enabled)]
    identities = [json.loads((root / "artifact-identity.json").read_text()) for root in roots]
    if [identity.get("adaptive_packing") for identity in identities] != [False, True]:
        raise ValueError("comparison requires packing-off then packing-on artifacts")
    for field in ("sha256", "interruption", "source_repeat"):
        if identities[0].get(field) != identities[1].get(field):
            raise ValueError(f"packing comparison mismatch: {field}")
    reports = [verify_automatic(root) for root in roots]
    workloads = []
    for root in roots:
        seed = load_events([root / "seed.jsonl"])
        workloads.append(canonical_user_workload(seed))
    if workloads[0] != workloads[1]:
        raise ValueError("packing comparison canonical user workloads differ")
    if reports[1]["planned_chunks"] < 2:
        raise ValueError("packing-on workload did not retain multiple real chunks")
    if reports[1]["synthesis_requests"] >= reports[0]["synthesis_requests"]:
        raise ValueError("adaptive packing did not reduce real synthesis request count")
    report = {"schema": "bcode.compaction.packing_comparison.v1", "sha256": identities[0]["sha256"],
              "canonical_user_sources_equal": True, "off": reports[0], "on": reports[1],
              "requests_saved": reports[0]["synthesis_requests"] - reports[1]["synthesis_requests"]}
    print(json.dumps(report, indent=2))
    return report


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("work", nargs="?", type=Path)
    parser.add_argument("--compare-packing", nargs=2, type=Path, metavar=("OFF", "ON"))
    arguments = parser.parse_args()
    if (arguments.work is None) == (arguments.compare_packing is None):
        parser.error("supply one run directory or --compare-packing OFF ON")
    try:
        if arguments.compare_packing:
            compare_packing(*arguments.compare_packing)
        else:
            verify_automatic(arguments.work)
    except (ValueError, KeyError, TypeError, OSError, IndexError) as error:
        raise SystemExit(f"FAIL: {error}") from error
