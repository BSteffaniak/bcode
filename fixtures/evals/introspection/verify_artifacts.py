"""Bounded verification of retained eval artifacts; never open canonical stores."""
import hashlib
import json
import pathlib
import sys

MAX_BYTES = 8 * 1024 * 1024


def load(path):
    if path.is_symlink() or path.stat().st_size > MAX_BYTES:
        raise ValueError("unsafe or oversized artifact")
    return json.loads(path.read_text())


def verify_exports(summary, rows, enabled, gaps):
    stem = summary.name.removesuffix(".introspection.json")
    receipt_path = summary.parent / (stem + ".captures.json")
    receipt_document = load(receipt_path)
    assert receipt_document["schema"] == "bcode.eval.trace_exports.v1"
    assert receipt_document["sensitive"] is True
    receipts = receipt_document["receipts"]
    assert len(receipts) <= len(rows), "unexpected receipt count"
    canonical_path = summary.parent / (stem + ".jsonl")
    assert not canonical_path.is_symlink() and canonical_path.stat().st_size <= MAX_BYTES
    canonical = [json.loads(line) for line in canonical_path.read_text().splitlines()]
    indexed = {(r["session_id"], r["sequence"]): r for r in rows}
    retained = {(r["session_id"], r["sequence"]): r["kind"].get("trace_event", {}).get("trace", {}).get("payload", {}) for r in canonical}
    seen = set()
    exported_requests = 0
    for receipt in receipts:
        identity = (receipt["session_id"], receipt["sequence"])
        assert identity in indexed and identity not in seen, "unjoined/duplicate capture receipt"
        seen.add(identity)
        payload = retained[identity]
        reference = None
        for event, field in (("model_request_built", "request"),
                             ("tool_invocation_requested", "arguments"),
                             ("tool_invocation_started", "arguments"),
                             ("tool_invocation_finished", "output")):
            if event in payload:
                reference = payload[event].get(field)
                break
        assert reference is not None, "receipt without request-time trace reference"
        status = receipt["status"]
        assert status in ("exported", "unavailable", "verification_failed")
        if status != "exported":
            assert not receipt.get("file"), "failed export claims a blob file"
            gaps.append(stem + ": " + status + " capture receipt")
            print("CAPTURE GAP: " + stem + ": " + status + " capture receipt")
            continue
        assert enabled, "capture-off exported a sensitive blob"
        name = f"{identity[0]}-{identity[1]}.blob"
        assert receipt["file"] == name and pathlib.Path(name).name == name
        directory = summary.parent / (stem + ".captures")
        assert not directory.is_symlink(), "symlinked capture directory"
        blob = directory / name
        assert not blob.is_symlink() and blob.stat().st_size <= MAX_BYTES
        data = blob.read_bytes()
        assert len(data) == receipt["byte_len"] == reference["byte_len"]
        assert hashlib.sha256(data).hexdigest() == receipt["sha256"] == reference["sha256"]
        assert receipt["completeness"] == reference["completeness"]
        assert receipt["completeness"] in ("complete", "retained", "truncated")
        assert receipt["redaction"] == reference["redaction"]
        assert receipt["redaction"] in ("none", "automatic", "manual_required")
        if receipt["completeness"] != "complete":
            gaps.append(stem + ": " + receipt["completeness"] + " blob content")
            print("CAPTURE GAP: " + stem + ": " + receipt["completeness"] + " blob content")
        if "model_request_built" in payload:
            assert receipt["completeness"] == "complete", "fixture request capture is incomplete"
            assert receipt["redaction"] != "manual_required", "fixture request needs manual redaction"
            # Validate actual captured request content without printing it.
            request = json.loads(data)
            assert request.get("messages"), "export is not a populated model request"
            exported_requests += 1
    requests = [row for row in rows if "model_request_built" in
                row["observation"].get("payload", {})]
    if enabled:
        assert exported_requests == len(requests), \
            "capture-on must export every observed request successfully"
        finished = [row for row in rows if "tool_invocation_finished" in
                    row["observation"].get("payload", {})]
        assert all((r["session_id"], r["sequence"]) in seen for r in finished), \
            "missing tool output export receipts"
        assert all(r["status"] == "exported" for r in receipts), \
            "fixture capture has failed export receipts (see CAPTURE GAP evidence)"
    else:
        assert not receipts, "capture-off unexpectedly retained export receipts"
    return exported_requests


def verify(root, require_complete=False):
    controls = root / "oracle-controls"
    for case in ("tiny", "separable", "handoff"):
        for variant in ("positive", "wrong", "missing"):
            result = load(controls / f"cases/{case}/variants/{variant}/repetitions/0001/result.json")
            assert result["passed"] is (variant == "positive"), "oracle control mismatch"
    assert load(controls / "summary.json")["passed"] is False
    gaps = []
    request_counts = {}
    for variant in ("capture-on", "capture-off"):
        rep = root / f"offline-direct/cases/read-loop/variants/{variant}/repetitions/0001"
        assert load(rep / "result.json")["passed"] is True
        summaries = list(rep.glob("*.introspection.json"))
        assert summaries, "missing direct introspection summary"
        for summary in summaries:
            info = load(summary)
            assert info["schema_version"] == 2, "unsupported introspection version"
            rows_path = summary.with_suffix(".jsonl")
            assert not rows_path.is_symlink() and rows_path.stat().st_size <= MAX_BYTES
            rows = [json.loads(line) for line in rows_path.read_text().splitlines()]
            assert rows and all(row["schema_version"] == 2 for row in rows)
            assert all(row["session_id"] == info["session_id"] for row in rows)
            sequences = [row["sequence"] for row in rows]
            assert sequences == sorted(set(sequences)), "noncanonical index ordering"
            observations = [row["observation"] for row in rows]
            payloads = [o.get("payload", {}) for o in observations if o["kind"] == "trace"]
            requests = [p for p in payloads if "model_request_built" in p]
            assert len(requests) == info["model_requests_observed"] and len(requests) >= 3
            assert any(o["kind"] == "context" for o in observations), "missing context occupancy"
            assert any("provider_round" in p for p in payloads), "missing provider timing"
            assert sum("tool_invocation_started" in p for p in payloads) == 2
            assert sum("tool_invocation_finished" in p for p in payloads) == 2
            assert all("metadata" not in p["model_request_built"] for p in requests)
            references = info["request_blob_references"]
            assert references + info["missing_request_capture"] == len(requests)
            if variant == "capture-on":
                assert references == len(requests), "explicit capture did not retain request references"
                assert info["complete_model_inputs_exported"] is True
                assert all(p["output_capture"]["status"] == "referenced_unverified"
                           for p in payloads if "tool_invocation_finished" in p)
            else:
                assert info["complete_model_inputs_exported"] is False
                assert references == 0, "capture-off unexpectedly persisted requests"
                assert all(p["output_capture"]["status"] == "not_captured"
                           for p in payloads if "tool_invocation_finished" in p)
            request_counts[variant] = verify_exports(summary, rows, variant == "capture-on", gaps)
            # Export receipts, bytes and hashes are authoritative evidence; do not
            # infer availability from the older index summary's export flag.
            if variant == "capture-on" and info["missing_request_capture"]:
                gaps.append(variant + ": request capture intentionally unavailable")
    workflow = root / "offline-workflow/cases/scripted-loop/variants/single/repetitions/0001"
    assert load(workflow / "result.json")["passed"] is True
    inspection = load(workflow / "inspection.json")
    assert inspection.get("attempts"), "missing workflow attempt provenance"
    assert load(workflow / "timing.json")["schema_version"] == 1
    correlations = load(workflow / "workflow-session-correlations.json")
    assert correlations["schema"] == "bcode.eval.workflow_session_correlations.v1"
    observed = {attempt["node_id"]: attempt for attempt in correlations["attempts"]}
    for node in ("loop.implementation", "loop.evaluation"):
        assert observed[node]["availability"] == "observed", "missing persisted agent turn correlation"
        assert len(observed[node]["session_turns"]) == 1, "ambiguous agent turn correlation"
    assert observed["loop.judgement.evaluate"]["availability"] == "not_observed", \
        "plugin judgement must not fabricate an agent turn"
    workflow_summaries = list(workflow.glob("*.introspection.json"))
    assert workflow_summaries, "missing workflow session indexes"
    for summary in workflow_summaries:
        assert load(summary)["schema_version"] == 2
        index_path = summary.with_suffix(".jsonl")
        assert not index_path.is_symlink() and index_path.stat().st_size <= MAX_BYTES
        rows = [json.loads(line) for line in index_path.read_text().splitlines()]
        assert all(row["schema_version"] == 2 for row in rows)
        verify_exports(summary, rows, True, gaps)
    oracle = load(workflow / "oracle.json")
    if not oracle["capture_complete"]:
        gaps.append("workflow: descendant capture incomplete")
    # Current inspection is not a complete semantic handoff history. Never equate it
    # with exported handoff inputs, judgement requests or provider queue attribution.
    gaps.append("workflow: semantic handoff/judgement/queue attribution not certified by this smoke")
    print("Independent positive/wrong/missing controls verified; real direct/workflow indexes retained.")
    for gap in gaps:
        print("CAPTURE GAP: " + gap)
    if require_complete and gaps:
        raise SystemExit("complete introspection not demonstrated")


if __name__ == "__main__":
    verify(pathlib.Path(sys.argv[1]), "--require-complete" in sys.argv[2:])
