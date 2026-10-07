"""Offline corpus -> v1 shared plan and CaptureJob materializer. Never executes subjects."""
import argparse
import hashlib
import json
from pathlib import Path
import uuid

MAX_BYTES = 262144
FEATURES = ("tool_result_triage", "context_reranking", "recommendations",
            "progress_monitoring", "session_categorization", "eval_categorization")


def read_json(path):
    with Path(path).open("rb") as stream:
        raw = stream.read(MAX_BYTES + 1)
    if len(raw) > MAX_BYTES:
        raise ValueError("input exceeds materializer bound")
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("duplicate JSON field")
            result[key] = value
        return result
    return json.loads(raw, object_pairs_hook=unique,
                      parse_constant=lambda _: (_ for _ in ()).throw(ValueError("nonfinite input")))


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def identifier(value):
    if not isinstance(value, str) or not value or len(value) > 128 or value in (".", ".."):
        raise ValueError("invalid identifier")
    if not all(c.isascii() and (c.isalnum() or c in "-_.") for c in value):
        raise ValueError("invalid identifier")
    return value


def adapt(case, dimension):
    """Translate inputs, never expected labels, into producer-owned request schemas."""
    feature, source = case["feature"], case["input"]
    case_id = identifier(case["id"])
    if feature == "context_reranking" and "relevance" not in case["expected"]:
        raise ValueError("legacy context lacks graded byte-budget admission truth; explicit gap")
    if feature == "progress_monitoring" and any("at_ms" not in item for item in source["observations"]):
        raise ValueError("legacy progress lacks trajectory timestamps; explicit gap")
    if feature == "tool_result_triage":
        request = {"version": 1, "invocation_id": case_id, "tool_name": source["tool"],
                   "is_error": not source["settled_success"], "partial": source.get("partial", False),
                   "excerpt_truncated": False, "output_excerpt": source["excerpt"]}
        return {"kind": "service", "consumer": {"service": feature, "input": request}}
    if feature == "progress_monitoring":
        observations = []
        for item in source["observations"]:
            sequence = item["sequence"]
            observations.append({"sequence": sequence, "action": item.get("action", "settled operation"),
                                 "failure_key": item.get("failure_id"),
                                 "evidence_key": str(sequence) if item["fresh_evidence"] else None,
                                 "summary": item.get("summary") or item.get("failure_id") or "settled observation"})
        request = {"version": 1, "scope": {"kind": "session", "id": case_id},
                   "objective": source["objective"], "observations": observations}
        times = [item.get("at_ms") for item in source["observations"]]
        if times and all(isinstance(time, int) and not isinstance(time, bool) for time in times):
            return {"kind": "service", "consumer": {"service": "progress_trajectory", "input": request,
                                                       "observation_times_ms": times}}
        return {"kind": "service", "consumer": {"service": feature, "input": request}}
    if feature == "recommendations":
        return {"kind": "service", "consumer": {"service": feature, "input": {
            "purpose": "recommendations", "task": source["task"], "candidates": [
                {k: c[k] for k in ("id", "description", "protected")} for c in source["candidates"]]}}}
    if feature == "context_reranking":
        unit = 128  # Fixed byte unit preserves semantic descriptions without truncation.
        candidates = []
        for c in source["candidates"]:
            snippet = c["description"]
            if "budget_units" in source:
                size = c["size_units"] * unit
                if len(snippet.encode()) > size or size > 2048:
                    raise ValueError("context snippet cannot represent corpus byte units")
                snippet += " " * (size - len(snippet.encode()))
            candidates.append({"id": c["id"], "source": "file", "provenance": "synthetic:" + c["id"],
                               "snippet": snippet, "protected": c["protected"]})
        request = {"task": source["task"], "candidates": candidates,
                   "budget_bytes": source["budget_units"] * unit if "budget_units" in source else None}
        return {"kind": "service", "consumer": {"service": "context_selection", "input": request}}
    if feature == "session_categorization":
        # Deterministic request identity is fixture-local, never an alternate canonical session root.
        request = {"session_id": str(uuid.uuid5(uuid.NAMESPACE_URL, "bcode-judgement:" + case_id)),
                   "source_sequence": 1, "newest_request": source["newest_request"]}
        return {"kind": "service", "consumer": {"service": feature, "input": request, "dimension": dimension}}
    if feature == "eval_categorization":
        # The same deterministic classifier consumed by eval capture; no expected answer is injected.
        oracle = source["oracle"]
        labels = []
        for key, value, label in (("capture_complete", False, "incomplete_execution_evidence"),
                                  ("status", "failed", "workflow_terminal_failure"),
                                  ("timed_out", True, "deadline_exhaustion"),
                                  ("false_completion", True, "false_completion"),
                                  ("preservation_passed", False, "preservation_violation"),
                                  ("artifact_observed", False, "delivered_artifact_unobservable")):
            if key in oracle and type(oracle[key]) is type(value) and oracle[key] == value:
                labels.append(label)
        if oracle.get("artifact_observed") is not False and oracle.get("artifact_correct") is False:
            labels.append("delivered_artifact_incorrect_or_missing")
        return {"kind": "eval_diagnosis", "oracle": oracle, "diagnosis": {"labels": labels}}
    raise ValueError("unsupported feature")


def validate_service_input(consumer):
    """Strict wire-shape preflight; producer typed decoding remains the authoritative check."""
    if consumer["kind"] != "service":
        return
    service, data = consumer["consumer"]["service"], consumer["consumer"]["input"]
    if service == "tool_result_triage":
        if set(data) != {"version", "invocation_id", "tool_name", "is_error", "partial", "excerpt_truncated", "output_excerpt"}:
            raise ValueError("triage wire fields do not match version one")
        if data["version"] != 1 or not isinstance(data["output_excerpt"], str):
            raise ValueError("invalid triage wire types")
    if service in ("progress_monitoring", "progress_trajectory"):
        if set(data) != {"version", "scope", "objective", "observations"} or data["version"] != 1:
            raise ValueError("invalid progress envelope")
        if set(data["scope"]) != {"kind", "id"} or data["scope"]["kind"] != "session":
            raise ValueError("invalid progress scope")
        previous = 0
        for item in data["observations"]:
            if set(item) != {"sequence", "action", "failure_key", "evidence_key", "summary"}:
                raise ValueError("invalid progress observation fields")
            if not isinstance(item["sequence"], int) or item["sequence"] <= previous:
                raise ValueError("progress observations must be strictly ordered")
            previous = item["sequence"]
            if not all(isinstance(item[key], str) and item[key] for key in ("action", "summary")):
                raise ValueError("invalid progress text")


def feasible_gain(truth):
    required = set(truth["required"])
    remaining = truth["budget"] - sum(truth["costs"][key] for key in required)
    if remaining < 0:
        raise ValueError("mandatory evidence exceeds shared admission budget")
    states = {0: 0.0}
    for key, relevance in truth["relevance"].items():
        if key in required:
            continue
        for used, gain in list(states.items()):
            cost = used + truth["costs"][key]
            if cost <= remaining:
                states[cost] = max(states.get(cost, 0.0), gain + 2 ** relevance - 1)
    return max(states.values()) + sum(2 ** truth["relevance"][key] - 1 for key in required)


def oracle(case, dimension):
    expected = case["expected"]
    if expected["kind"] == "progress":
        return {k: expected[k] for k in ("kind", "label", "onset_ms", "deadline_ms")}
    if expected["kind"] == "ranking" and "relevance" in expected:
        truth = {k: expected[k] for k in ("kind", "relevance", "costs", "budget", "k",
                                          "required", "protected_positions", "ideal_admission_gain")}
        if case["feature"] == "context_reranking":
            truth["costs"] = {key: value * 128 for key, value in truth["costs"].items()}
            truth["budget"] *= 128
            # Required evidence is outside optional pressure in the actual consumer.
            # Zero-cost protected truth is required; do not silently rewrite authored ground truth.
            if any(truth["costs"][key] != 0 for key in truth["required"]):
                raise ValueError("context protected costs must be zero in optional-budget truth")
        return truth
    if case["feature"] == "progress_monitoring":
        # Service capture has no detection timestamp: positive timing remains unmeasured.
        return {"kind": "progress", "label": expected.get("values", {}).get("state"),
                "onset_ms": 0, "deadline_ms": 300000}
    elif case["feature"] == "session_categorization":
        label = expected.get("values", {}).get(dimension)
    else:
        label = next(iter(expected.get("values", {}).values()), None)
    if expected["kind"] != "ranking":
        return {"kind": "classification", "labels": {"unsupported_advice": 0.0} if label is None else {label: 1.0},
                "abstention_credit": 1.0 if label is None else 0.0}
    candidates = case["input"]["candidates"]
    prefix = expected["prefix"]
    protected = {c["id"]: i for i, c in enumerate(candidates) if c["protected"]}
    # Protected evidence has fixed slots. Its credit must not demand moving it to a prefix.
    movable = [x for x in prefix if x not in protected]
    relevance = {c["id"]: (4.0 if c["id"] in movable else (1.0 if not prefix else 0.0))
                 for c in candidates}
    return {"kind": "ranking", "relevance": relevance, "k": len(candidates),
            "costs": {c["id"]: 1 for c in candidates}, "budget": len(candidates),
            "ideal_admission_gain": sum(2 ** v - 1 for v in relevance.values()),
            "required": list(protected), "protected_positions": protected}


def oracle_legacy_gap(case, dimension):
    # Retain scheduled denominator without executing unsupported legacy arm inputs.
    if case["feature"] == "context_reranking":
        ids = [candidate["id"] for candidate in case["input"]["candidates"]]
        return {"kind": "ranking", "relevance": {key: 0.0 for key in ids}, "k": len(ids),
                "costs": {key: 0 for key in ids}, "budget": 0, "ideal_admission_gain": 0.0,
                "required": [], "protected_positions": {}}
    if case["feature"] == "progress_monitoring":
        return {"kind": "progress", "label": case["expected"].get("values", {}).get("state"),
                "onset_ms": 0, "deadline_ms": 300000}
    return oracle(case, dimension)


def materialize(corpus, template, gates, repetitions, dimension, output):
    if corpus.get("schema_version") not in (1, 3) or not 1 <= repetitions <= 100:
        raise ValueError("unsupported corpus/repetition policy")
    cases = corpus["cases"]
    if not 1 <= len(cases) <= 128:
        raise ValueError("corpus case bound exceeded")
    preregistration = template["preregistration_id"]
    output = Path(output)
    if output.exists():
        raise ValueError("output must be a new directory; prior evidence is never overwritten")
    tasks, jobs, gaps = [], [], []
    seen = set()
    for case in cases:
        case_id = identifier(case["id"])
        if case_id in seen or case["feature"] not in FEATURES:
            raise ValueError("duplicate case/unsupported feature")
        seen.add(case_id)
        try:
            consumer = adapt(case, dimension)
            validate_service_input(consumer)
            truth = oracle(case, dimension)
        except ValueError as gap:
            consumer = {"kind": "unavailable"}
            truth = oracle_legacy_gap(case, dimension)
            gaps.append({"task_id": case_id, "gap": str(gap)})
        tasks.append({"task_id": case_id, "feature": case["feature"], "repetitions": repetitions,
                      "oracle": truth})
        pairing = digest({"case_input": case["input"], "dimension": dimension})
        for repetition in range(repetitions):
            for arm in ("off", "on"):
                request = dict(template)
                request.update(task_id=case_id, repetition=repetition, feature=case["feature"], arm=arm,
                               capture_id=f"{case_id}-{repetition}-{arm}", pairing_id=pairing)
                # Generation is not proof that the subject enacted isolated configuration.
                request["isolated_feature"] = None
                jobs.append((request["capture_id"], {"schema_version": 1, "request": request,
                                                     "consumer": consumer, "allow_live": False}))
    plan = {"schema_version": 1, "oracle_id": digest(corpus), "preregistration_id": preregistration,
            "tasks": tasks, "gates": gates}
    # Validate all serialized job bounds before publishing any file.
    for _, job in jobs:
        if len(json.dumps(job).encode()) > 128 * 1024:
            raise ValueError("CaptureJob exceeds bound")
    output.mkdir(parents=True)
    with (output / "plan.json").open("x") as stream:
        json.dump(plan, stream, indent=2)
    with (output / "materialization-gaps.json").open("x") as stream:
        json.dump(gaps, stream, indent=2)
    for name, job in jobs:
        with (output / (name + ".job.json")).open("x") as stream:
            json.dump(job, stream, indent=2)
    return plan, jobs


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corpus", required=True)
    parser.add_argument("--request-template", required=True, help="CaptureRequest with verified subject metadata")
    parser.add_argument("--gates", required=True, help="preregistered shared Gates JSON")
    parser.add_argument("--output", required=True)
    parser.add_argument("--feature", choices=FEATURES, help="materialize one independent feature only")
    parser.add_argument("--repetitions", type=int, default=1)
    parser.add_argument("--session-dimension", choices=("task", "domain"), default="task")
    args = parser.parse_args()
    try:
        corpus = read_json(args.corpus)
        if args.feature:
            corpus["cases"] = [case for case in corpus["cases"] if case["feature"] == args.feature]
        plan, jobs = materialize(corpus, read_json(args.request_template), read_json(args.gates),
                                 args.repetitions, args.session_dimension, args.output)
    except (OSError, ValueError, KeyError, TypeError):
        parser.exit(2, "invalid or unsupported bounded materialization input\n")
    print(f"materialized {len(plan['tasks'])} tasks and {len(jobs)} disabled-live jobs; no execution performed")


if __name__ == "__main__":
    main()
