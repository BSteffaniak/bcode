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
    observation_pages = sorted(workflow.glob("workflow-observations-*.json"))
    assert observation_pages, "missing transactional observation coverage"
    previous = 0
    through = None
    for path in observation_pages:
        page = load(path)
        assert page["version"] == 1 and page["origin"] == "admission"
        assert page["run_id"] == inspection["run"]["run_id"]
        assert page["sources"] and page["limitations"], "finite coverage must declare scope"
        if through is None:
            through = page["through_sequence"]
        assert through == page["through_sequence"]
        for row in page["observations"]:
            assert previous < row["sequence"] <= through
            previous = row["sequence"]
        assert page["next_after_sequence"] in (None, previous)
    assert page["next_after_sequence"] is None and previous > 0
    diagnostic_files = list(workflow.glob("diagnostics-*/artifact-*.json"))
    diagnostics = {load(path)["boundary"]: load(path) for path in diagnostic_files}
    assert {"input", "request", "response", "outcome"} <= diagnostics.keys(), "judgement diagnostic boundaries missing"
    assert len({diagnostics[key]["dispatch_identity"] for key in ("input", "request", "response", "outcome")}) == 1
    assert diagnostics["input"]["payload"]["evaluator_kind"] == "loop_completion_judgement"
    assert diagnostics["input"]["payload"]["input_role"] == "normalized_iteration"
    answer = diagnostics["response"]["payload"]["answers"]["completion"]
    assert answer["kind"] == "yes_no" and answer["probability"] == 0.75
    outcome = diagnostics["outcome"]["payload"]
    assert outcome["decision"]["condition_met"] is True and outcome["fallback_selected"] is False
    for index_path in workflow.glob("diagnostics-*/index.json"):
        index = load(index_path)
        for position, artifact in enumerate(index["artifacts"]):
            path = index_path.parent / f"artifact-{position:03}.json"
            assert not path.is_symlink() and path.stat().st_size <= MAX_BYTES
            content = path.read_bytes()
            assert len(content) == artifact["byte_len"]
            assert hashlib.sha256(content).hexdigest() == artifact["sha256"]
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


def audit_controlled(root, require_complete=False):
    """Report every retained trial without repairing or rewriting any artifact."""
    trials = sorted((root / "cases").glob("*/variants/*/repetitions/*"))
    trials = [path for path in trials if path.is_dir() and not path.is_symlink()]
    assert len(trials) == 36, "controlled audit requires all 36 trials"
    reports = []
    for trial in trials:
        boundaries = {}
        summaries = sorted(trial.glob("*.introspection.json"))
        gaps = []
        for summary in summaries:
            try:
                index = summary.with_suffix(".jsonl")
                assert not index.is_symlink() and index.stat().st_size <= MAX_BYTES
                rows = [json.loads(line) for line in index.read_text().splitlines()]
                import contextlib
                import io
                with contextlib.redirect_stdout(io.StringIO()):
                    verify_exports(summary, rows, True, gaps)
            except (AssertionError, ValueError, KeyError, OSError) as error:
                # Do not print exception text: malformed artifacts can contain secrets.
                gaps.append(type(error).__name__)
        boundaries["model_and_tool_exports"] = {
            "status": "missing" if not summaries else "incomplete" if gaps else "verified_observed",
            "sessions_observed": len(summaries),
            "scope": "retained request boundaries only; not full-session coverage",
        }
        manifest = trial / "capture-completeness.json"
        if manifest.exists():
            document = load(manifest)
            assert document["schema"] == "bcode.eval.capture_completeness.v1"
            boundaries.update(document["boundaries"])
        else:
            for boundary in ("events", "attempts", "judgement", "provider_wire"):
                boundaries[boundary] = {"status": "missing_manifest"}
        reports.append({"trial": str(trial.relative_to(root)), "boundaries": boundaries,
                        "complete": False})
    print(json.dumps({"schema": "bcode.eval.capture_audit.v1", "trials": reports,
                      "complete": False}, sort_keys=True))
    if require_complete:
        raise SystemExit("full causal capture is not certified")


if __name__ == "__main__":
    if "--controlled-audit" in sys.argv[2:]:
        audit_controlled(pathlib.Path(sys.argv[1]), "--require-complete" in sys.argv[2:])
    else:
        verify(pathlib.Path(sys.argv[1]), "--require-complete" in sys.argv[2:])
