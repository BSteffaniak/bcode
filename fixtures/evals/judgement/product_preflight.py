"""Serial public-producer study preparation and captured-result preflight; never executes Bcode."""
import argparse
import importlib.util
import json
from pathlib import Path

import benchmark as b

CAPTURE_PATH = b.ROOT.parents[2] / "packages/eval/src/judgement_capture/materialize.py"
TOOLS = ["filesystem.read", "filesystem.write", "filesystem.edit", "shell.run"]
SOL = {"provider": "bcode.openai-compatible", "model": "gpt-6.1-sol",
       "model_profile": "sol", "auth_profile": "openai-2"}


def capture_adapter():
    spec = importlib.util.spec_from_file_location("capture_materializer", CAPTURE_PATH)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def sha(value):
    return isinstance(value, str) and len(value) == 64 and all(c in "0123456789abcdef" for c in value)


def validate_provenance(data):
    """Require declared exact inputs; this is not independent runtime verification."""
    if set(data) != {"schema_version", "launch", "selection", "jev_selection", "tool_policy", "subjects"}:
        raise ValueError("unsupported provenance fields")
    if data["schema_version"] != 1 or set(data["selection"]) != set(SOL):
        raise ValueError("explicit normalized selection required")
    if not all(isinstance(value, str) and value for value in data["selection"].values()):
        raise ValueError("selection references must be nonempty")
    jev = data["jev_selection"]
    if (set(jev) != {"provider_plugin_id", "model_id", "auth_profile"}
            or not all(b.safe_name(value) for value in jev.values())):
        raise ValueError("explicit catalog-verified judgement selection required at admission")
    launch = data["launch"]
    if (set(launch) != {"config_file", "state_root", "context", "endpoint_id", "inline_overrides_accounted"}
            or not Path(launch["config_file"]).is_absolute()
            or not Path(launch["state_root"]).is_absolute()
            or not isinstance(launch["endpoint_id"], str) or not launch["endpoint_id"]
            or launch["inline_overrides_accounted"] is not True
            or launch["context"] is not None and not b.safe_name(launch["context"])):
        raise ValueError("incomplete endpoint/config provenance")
    policy = data["tool_policy"]
    if (set(policy) != {"approved", "allowed_tools", "workflow_tools_approved"}
            or type(policy["approved"]) is not bool or policy["allowed_tools"] != TOOLS
            or policy["workflow_tools_approved"] is not False):
        raise ValueError("expected approved four-tool policy, no workflow tools")
    if set(data["subjects"]) != set(b.FEATURES):
        raise ValueError("six per-feature off/on subjects required")
    fields = {"executable", "executable_sha256", "artifact_id", "configuration_sha256",
              "configuration_complete", "provider", "model", "auth_profile", "model_profile", "state_id"}
    first = None
    for feature in b.FEATURES:
        arms = data["subjects"][feature]
        if set(arms) != {"off", "on"}:
            raise ValueError("off/on subject pair required")
        if arms["off"]["configuration_sha256"] == arms["on"]["configuration_sha256"]:
            raise ValueError("off/on resolved configurations must differ")
        for subject in arms.values():
            if (set(subject) != fields or not Path(subject["executable"]).is_absolute()
                    or not sha(subject["executable_sha256"]) or not sha(subject["configuration_sha256"])
                    or subject["configuration_complete"] is not True
                    or any(subject[key] != value for key, value in data["selection"].items())
                    or not subject["state_id"] or not subject["artifact_id"]):
                raise ValueError("incomplete subject identity")
            identity = tuple(subject[key] for key in ("executable", "executable_sha256", "artifact_id", "state_id"))
            if first is None:
                first = identity
            elif identity != first:
                raise ValueError("subject artifact/state must be consistent across serial suite")


def prepare(provenance, template, gates, output, allow_live=False):
    """Generate twelve supported jobs using capture lane's adapter; no provider calls."""
    validate_provenance(provenance)
    if allow_live is not False:
        raise ValueError("live job generation requires separate verified user authorization; unsupported here")
    adapter = capture_adapter()
    corpus = b.load("development")
    # Fixed first-authored case per feature, independent of observed provider answers.
    cases = [next(c for c in corpus["cases"] if c["feature"] == feature) for feature in b.FEATURES]
    selected = dict(corpus, cases=cases)
    for case in cases:
        consumer = adapter.adapt(case, "task")
        adapter.validate_service_input(consumer)
        if consumer["kind"] == "unavailable":
            raise ValueError("pilot operation unavailable")
    output = Path(output)
    plan, jobs = adapter.materialize(selected, template, gates, 1, "task", output)
    schedule = []
    for number, (name, job) in enumerate(jobs):
        feature, arm = job["request"]["feature"], job["request"]["arm"]
        subject = provenance["subjects"][feature][arm]
        job["request"]["subject"] = subject
        job["request"]["isolated_feature"] = None
        job["allow_live"] = allow_live
        path = output / (name + ".job.json")
        path.write_text(json.dumps(job, indent=2) + "\n")
        policy = b.configuration("global-off" if arm == "off" else "only-" + feature)
        policy += "[judgement.selection]\n" + "".join(
            key + " = " + json.dumps(value) + "\n" for key, value in provenance["jev_selection"].items())
        policy_path = output / (name + ".judgement.toml")
        policy_path.write_text(policy)
        argv = [subject["executable"], "--state-root", provenance["launch"]["state_root"],
                "--profile", provenance["selection"]["model_profile"]]
        if provenance["launch"]["context"] is not None:
            argv += ["--context", provenance["launch"]["context"]]
        argv += ["judgement", "benchmark-capture", "--file", str(path.resolve())]
        schedule.append({"serial_index": number, "job": path.name, "feature": feature, "arm": arm,
            "case_id": job["request"]["task_id"], "scope": "service_microbenchmark",
            "argv": argv, "env": {"BCODE_CONFIG": provenance["launch"]["config_file"],
                                    "BCODE_CONFIG_TOML": policy},
            "endpoint_id": provenance["launch"]["endpoint_id"],
            "subject": subject, "job_sha256": b.digest(path),
            "overlay_sha256": b.digest(policy_path),
            "admission_required": ["require_matching_running_daemon", "resolved_configuration_matches_subject",
                                   "effective_model_and_judgement_pin_verified", "explicit_user_budget_and_tool_policy_verified",
                                   "serial_budget_admission"],
            "isolation_verified": False, "normal_turn_gain_eligible": False})
    manifest = {"schema_version": 1, "concurrency": 1, "executed": False,
        "allow_live": allow_live, "corpus_sha256": b.digest(b.ROOT / "development.json"),
        "selection_rule": "first authored development case per feature; no outcome selection",
        "provenance": provenance, "entries": schedule,
        "direct_editing_distinct_execution": False, "normal_turn_gain_eligible": False}
    (output / "serial-suite.json").write_text(json.dumps(manifest, indent=2) + "\n")
    return manifest


def check_capture(entry, job, capture):
    """Validate observable producer execution/absence; never treat missing calls as zero."""
    gaps = []
    request = capture.get("request", {})
    scheduled_request = dict(job["request"])
    observed_request = dict(request)
    # Isolation is an observed hosting fact; generation deliberately leaves it unknown.
    scheduled_request.pop("isolated_feature", None)
    observed_request.pop("isolated_feature", None)
    if observed_request != scheduled_request:
        gaps.append("capture_request_mismatch")
    if capture.get("scope") != "service_microbenchmark":
        gaps.append("not_service_study")
    if capture.get("consumer_executed") is not True:
        gaps.append("consumer_not_executed")
    if capture.get("status") not in {"captured", "fallback_reason_unknown"}:
        gaps.append("operation_not_captured")
    if request.get("isolated_feature") is not True:
        gaps.append("isolation_unverified")
    telemetry = capture.get("telemetry", {})
    if telemetry.get("optional_counts_incomplete") is not False:
        gaps.append("optional_counts_incomplete")
    counters = [telemetry.get(key) for key in ("discovery_calls", "verification_calls", "inference_calls")]
    if any(type(value) is not int or value < 0 for value in counters):
        gaps.append("optional_calls_unknown")
    operations = telemetry.get("optional_operations")
    if not isinstance(operations, list) or not operations:
        gaps.append("no_explicit_optional_observation")
    elif any(not isinstance(operation, dict) for operation in operations):
        gaps.append("invalid_optional_observation")
    elif entry["arm"] == "off" and any(
            operation.get("outcome") != "disabled" for operation in operations):
        gaps.append("off_observation_not_disabled")
    if entry["arm"] == "off" and counters != [0, 0, 0]:
        gaps.append("off_arm_calls_not_zero")
    if entry["arm"] == "on" and capture.get("status") == "fallback_reason_unknown":
        gaps.append("fallback_reason_unknown")
    if telemetry.get("calls_complete") is not True:
        gaps.append("billing_ledger_incomplete")
    calls = telemetry.get("calls")
    if not isinstance(calls, list) or any(call.get("cost_usd") is None for call in calls):
        gaps.append("billing_unknown")
    return {"ready": not gaps, "gaps": gaps, "normal_turn_gain_eligible": False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    p = sub.add_parser("prepare")
    for name in ("provenance", "request-template", "gates", "output"):
        p.add_argument("--" + name, required=True)
    p.add_argument("--allow-live", action="store_true", help="rejected: no live authorization is established")
    p = sub.add_parser("check")
    p.add_argument("--suite", required=True)
    p.add_argument("--captures", required=True, help="directory containing capture-id.capture.json")
    args = parser.parse_args()
    if args.action == "prepare":
        manifest = prepare(b.read_json(args.provenance), b.read_json(args.request_template),
                           b.read_json(args.gates), args.output, args.allow_live)
        print(json.dumps({"jobs": len(manifest["entries"]), "executed": False}))
    else:
        suite = b.read_json(args.suite)
        results = []
        if not isinstance(suite.get("entries"), list) or len(suite["entries"]) != 12 or suite.get("concurrency") != 1:
            raise ValueError("expected complete serial twelve-job suite")
        for entry in suite["entries"]:
            if not b.safe_name(entry.get("job")):
                raise ValueError("unconfined scheduled job")
            job = b.read_json(Path(args.suite).parent / entry["job"])
            if not sha(entry.get("job_sha256")) or b.digest(Path(args.suite).parent / entry["job"]) != entry["job_sha256"]:
                raise ValueError("scheduled job identity mismatch")
            if not b.safe_name(job["request"].get("capture_id")):
                raise ValueError("unconfined capture identity")
            path = Path(args.captures) / (job["request"]["capture_id"] + ".capture.json")
            result = check_capture(entry, job, b.read_json(path)) if path.is_file() else {
                "ready": False, "gaps": ["missing_capture"], "normal_turn_gain_eligible": False}
            results.append(dict(result, feature=entry["feature"], arm=entry["arm"]))
        print(json.dumps({"ready": all(r["ready"] for r in results), "results": results}, indent=2))


if __name__ == "__main__":
    main()
