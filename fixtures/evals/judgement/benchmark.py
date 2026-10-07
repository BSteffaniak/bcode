"""Offline fixture tooling; never launches Bcode, providers, or evaluation runs."""
import argparse
import hashlib
import json
import math
from pathlib import Path
import random
import shlex
import sys

ROOT = Path(__file__).resolve().parent
FEATURES = (
    "tool_result_triage", "context_reranking", "recommendations",
    "progress_monitoring", "session_categorization", "eval_categorization",
)
ARMS = {
    "global-off": (),
    **{"only-" + feature: (feature,) for feature in FEATURES},
    "all-on": FEATURES,
    **{"without-" + feature: tuple(f for f in FEATURES if f != feature)
       for feature in FEATURES},
}
MAX_BYTES = 262144


def read_json(path):
    path = Path(path)
    if path.stat().st_size > MAX_BYTES:
        raise ValueError("JSON exceeds bounded fixture/capture limit")
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("duplicate JSON key: " + key)
            result[key] = value
        return result
    return json.loads(path.read_text(), object_pairs_hook=unique,
                      parse_constant=lambda value: (_ for _ in ()).throw(
                          ValueError("nonfinite JSON value: " + value)))


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def safe_name(value):
    return (isinstance(value, str) and value and len(value) <= 128
            and all(c.isascii() and (c.isalnum() or c in "-_.") for c in value)
            and value not in (".", ".."))


def load(split):
    filenames = {"development": "development.json", "held_out": "held-out.json",
                 "independent_public": "independent-public.json"}
    if split not in filenames:
        raise ValueError("unsupported dataset split")
    filename = filenames[split]
    graded = split in {"development", "independent_public"}
    data = read_json(ROOT / filename)
    if set(data) != {"schema_version", "split", "truth_provenance", "cases"}:
        raise ValueError("unknown dataset fields")
    if (data["schema_version"] != (3 if graded else 1) or data["split"] != split
            or data["truth_provenance"] != "authored_synthetic"):
        raise ValueError("unsupported dataset compatibility/provenance")
    frozen = read_json(ROOT / "freeze.json")
    if split == "held_out" and frozen["sha256"] != digest(ROOT / filename):
        raise ValueError("held-out digest changed; version and review a new freeze")
    ids = set()
    coverage = {feature: set() for feature in FEATURES}
    required = {"id", "feature", "control", "input", "expected", "rationale",
                "task", "files", "answer"}
    if graded:
        required |= {"family_id", "fault"}
    for case in data["cases"]:
        if set(case) != required or not safe_name(case["id"]) or case["id"] in ids:
            raise ValueError("invalid/duplicate case identity or unknown fields")
        ids.add(case["id"])
        if case["feature"] not in FEATURES or case["control"] not in {
                "positive", "negative", "adversarial"}:
            raise ValueError("unsupported feature/control")
        coverage[case["feature"]].add(case["control"])
        if not case["rationale"] or not case["task"] or not case["files"]:
            raise ValueError("missing independent rationale/task/fixture")
        if not isinstance(case["answer"], dict):
            raise ValueError("answer must be an object")
        for name, content in case["files"].items():
            if not safe_name(name) or name == "answer.json" or not isinstance(content, str):
                raise ValueError("fixture path/content invalid")
            if len(content.encode()) > 16384:
                raise ValueError("fixture content exceeds bounded input")
        expected = case["expected"]
        if expected["kind"] == "absent":
            if set(expected) != {"kind"}:
                raise ValueError("absence oracle fields")
        elif expected["kind"] == "labels":
            if set(expected) != {"kind", "values"} or not expected["values"]:
                raise ValueError("label oracle fields")
        elif expected["kind"] == "ranking":
            ranking_fields = {"kind", "prefix", "all_ids"}
            if graded:
                ranking_fields |= {"relevance", "costs", "budget", "k", "required",
                                   "protected_positions", "ideal_admission_gain"}
            if set(expected) != ranking_fields:
                raise ValueError("ranking oracle fields")
            candidates = case["input"]["candidates"]
            all_ids = expected["all_ids"]
            if (len(all_ids) != len(set(all_ids)) or set(all_ids) != {
                    c["id"] for c in candidates}
                    or not set(expected["prefix"]) <= set(all_ids)
                    or len(expected["prefix"]) != len(set(expected["prefix"]))):
                raise ValueError("ranking oracle incomplete/duplicate")
            if graded:
                import itertools
                if set(expected["relevance"]) != set(all_ids) or set(expected["costs"]) != set(all_ids):
                    raise ValueError("incomplete independent admission annotations")
                if any(type(v) not in (int, float) or not 0 <= v <= 4 for v in expected["relevance"].values()):
                    raise ValueError("invalid graded relevance")
                if any(type(v) is not int or v < 0 for v in expected["costs"].values()):
                    raise ValueError("invalid candidate sizes")
                if type(expected["budget"]) is not int or expected["budget"] < 0 or not 1 <= expected["k"] <= len(all_ids):
                    raise ValueError("invalid admission capacity/cutoff")
                protected = {c["id"] for c in candidates if c["protected"]}
                if set(expected["required"]) != protected:
                    raise ValueError("protected admission must be mandatory")
                recommendations = case["feature"] == "recommendations"
                for candidate in candidates:
                    cost = candidate["size_units"] if recommendations else (
                        0 if candidate["protected"] else candidate["size_units"])
                    if expected["costs"][candidate["id"]] != cost:
                        raise ValueError("candidate admission semantics mismatch")
                if expected["budget"] != case["input"]["budget_units"]:
                    raise ValueError("capacity mapping mismatch")
                total_cost = sum(expected["costs"].values())
                if recommendations:
                    if (expected["budget"] != total_cost
                            or expected["k"] != max(1, sum(r > 0 for r in expected["relevance"].values()))):
                        raise ValueError("recommendations require full admission and authored useful cutoff")
                elif total_cost <= expected["budget"]:
                    raise ValueError("optional capacity must supply real admission pressure")
                gains = []
                for bits in itertools.product((False, True), repeat=len(all_ids)):
                    admitted = {i for i, bit in zip(all_ids, bits) if bit}
                    if set(expected["required"]) <= admitted and sum(expected["costs"][i] for i in admitted) <= expected["budget"]:
                        gains.append(sum(2 ** expected["relevance"][i] - 1 for i in admitted))
                if not gains or max(gains) != expected["ideal_admission_gain"]:
                    raise ValueError("independent admission optimum mismatch")
        elif expected["kind"] == "progress" and graded:
            if (set(expected) != {"kind", "label", "onset_ms", "deadline_ms"}
                    or type(expected["onset_ms"]) is not int
                    or type(expected["deadline_ms"]) is not int
                    or not 0 <= expected["onset_ms"] <= expected["deadline_ms"]):
                raise ValueError("invalid progress detection window")
        else:
            raise ValueError("unsupported oracle kind")
    if split != "independent_public" and any("positive" not in c or not c & {"negative", "adversarial"}
           for c in coverage.values()):
        raise ValueError("each feature needs positive and negative/adversarial coverage")
    return data


def service_vocabulary_gaps(data):
    """Explicit triage-only producer mapping audit; never rewrite independent truth."""
    vocabulary = {"code", "environment", "auth", "transient", "partial", "inconclusive"}
    gaps = []
    for case in data["cases"]:
        expected = case["expected"]
        if case["feature"] == "tool_result_triage" and expected["kind"] == "labels":
            values = expected["values"]
            if set(values) != {"category"} or values["category"] not in vocabulary:
                gaps.append({"case_id": case["id"], "reason": "triage producer vocabulary mapping gap",
                             "independent_truth": expected})
    return gaps


def semantic_score(expected, output):
    """Score a full consumer result, never a raw provider answer."""
    if not isinstance(output, dict) or output.get("kind") != expected["kind"]:
        return False
    if expected["kind"] == "absent":
        return output == {"kind": "absent"}
    if expected["kind"] == "labels":
        return typed_equal(output, expected)
    if expected["kind"] == "progress":
        label = output.get("label")
        detected = output.get("detected_ms")
        return (set(output) == {"kind", "label", "detected_ms"}
                and label == expected["label"]
                and (detected is None if label is None else
                     type(detected) is int and expected["onset_ms"] <= detected <= expected["deadline_ms"]))
    ids = output.get("ids")
    return (set(output) == {"kind", "ids"} and isinstance(ids, list)
            and all(isinstance(i, str) for i in ids)
            and len(ids) == len(expected["all_ids"])
            and len(set(ids)) == len(ids) and set(ids) == set(expected["all_ids"])
            and ids[:len(expected["prefix"])] == expected["prefix"])


def typed_equal(actual, expected):
    # bool is an int subclass in Python: do not accept 1 as true or 0 as false.
    if type(actual) is not type(expected):
        return False
    if isinstance(expected, dict):
        return (actual.keys() == expected.keys()
                and all(typed_equal(actual[k], v) for k, v in expected.items()))
    if isinstance(expected, list):
        return (len(actual) == len(expected)
                and all(typed_equal(a, b) for a, b in zip(actual, expected)))
    return actual == expected


def check_workspace(case, workspace):
    workspace = Path(workspace).resolve(strict=True)
    for name, content in case["files"].items():
        path = workspace / name
        if path.is_symlink() or not path.is_file() or path.read_bytes() != content.encode():
            return False
    answer = workspace / "answer.json"
    if answer.is_symlink() or not answer.is_file():
        return False
    try:
        return typed_equal(read_json(answer), case["answer"])
    except (ValueError, OSError, TypeError):
        return False


def configuration(arm):
    enabled = ARMS[arm]
    return "[judgement]\nmode = \"" + ("off" if arm == "global-off" else "auto") + "\"\n" + "".join(
        feature + ' = "' + ("auto" if feature in enabled else "off") + '"\n'
        for feature in FEATURES)


def materialize(args):
    data = load(args.split)
    destination = Path(args.destination).resolve()
    # Only new directories: no overwriting prior captures or checkout fixtures.
    if destination.exists():
        raise ValueError("destination must not exist")
    if destination == ROOT or ROOT in destination.parents:
        raise ValueError("materialize outside the fixture checkout")
    if args.mode == "swarm" and (not args.subject_binary or not Path(args.subject_binary).is_absolute()
                                  or not Path(args.subject_binary).is_file()):
        raise ValueError("swarm requires an absolute coordinator-approved fresh subject binary")
    if args.repetitions < 1 or args.repetitions > 100:
        raise ValueError("repetitions outside bounded materialization limit")
    destination.mkdir(parents=True)
    schedule = []
    cases = list(data["cases"])
    for repetition in range(args.repetitions):
        rng = random.Random(f"{args.seed}:{args.split}:{args.mode}:{repetition}")
        rng.shuffle(cases)
        for case in cases:
            block = destination / f"r{repetition}-{case['id']}"
            block.mkdir()
            fixture = block / "workspace"
            fixture.mkdir()
            for name, content in case["files"].items():
                (fixture / name).write_text(content)
            # Consumer request is request-only adapter data, not visible truth.
            request = block / "request.json"
            request.write_text(json.dumps({"feature": case["feature"],
                                           "input": case["input"]}) + "\n")
            arms = list(ARMS)
            rng.shuffle(arms)
            text = ["schema_version = 1", 'id = ' + json.dumps(block.name),
                    'name = "Judgement paired block"', "[run]", "repetitions = 1",
                    "timeout_ms = 300000", 'isolation = "temp_copy"',
                    "randomize_case_order = false", "fail_fast = false"]
            for arm in arms:
                schedule.append({"case_id": case["id"], "repetition": repetition,
                                 "mode": args.mode, "arm": arm,
                                 "block": block.name})
                overlay = block / (arm + "-judgement.toml")
                overlay.write_text(configuration(arm))
                config = configuration(arm)
                if args.mode == "swarm":
                    config += '[workflows]\nrun_edit_plugins = ["bcode.workflow"]\nrun_publication_plugins = ["bcode.workflow"]\n'
                text += ["[[variants]]", "id = " + json.dumps(arm),
                         "executor = " + json.dumps("workflow" if args.mode == "swarm" else "agent"),
                         "config_toml = " + json.dumps(config)]
                if args.mode != "swarm":
                    text += ['profile = "build"',
                             'allowed_tools = ["filesystem.read", "filesystem.write", "filesystem.edit", "shell.run"]']
                text += ["[variants.env]", "BCODE_JUDGEMENT_REQUEST = " + json.dumps(str(request)),
                         "BCODE_JUDGEMENT_ARM = " + json.dumps(arm),
                         "BCODE_JUDGEMENT_CONFIG_OVERLAY = " + json.dumps(str(overlay)),
                         "BCODE_JUDGEMENT_MODE = " + json.dumps(args.mode)]
                if args.mode == "swarm":
                    text += ["[variants.metadata.workflow]",
                             "binary = " + json.dumps(args.subject_binary),
                             'strategy = "collaborate"',
                             'stop_condition = "Deliver answer.json satisfying the explicit task; preserve all input files."',
                             "max_iterations = 2", "concurrency = 1", "worker_attempts = 2",
                             'guidance = "Use bounded task evidence. Optional advice does not certify completion."']
            command = "python3 " + shlex.quote(str(ROOT / "benchmark.py")) + " oracle " + shlex.quote(args.split) + " " + shlex.quote(case["id"])
            text += ["[[cases]]", "id = " + json.dumps(case["id"]),
                     "fixture = " + json.dumps(str(fixture)),
                     "prompt = " + json.dumps(case["task"])]
            if args.mode == "swarm":
                text += ["[cases.metadata.workflow_oracle]", "oracle_command = " + json.dumps(command),
                         "preserve_files = " + json.dumps(list(case["files"]))]
            text += ["[[cases.judges]]", 'type = "command"',
                     "command = " + json.dumps(command), "timeout_ms = 5000", "required = true"]
            (block / "suite.toml").write_text("\n".join(text) + "\n")
    (destination / "schedule.json").write_text(json.dumps({"schema_version": 1,
        "seed": args.seed, "split": args.split, "truth_provenance": data["truth_provenance"],
        "entries": schedule}, indent=2) + "\n")
    print(json.dumps({"blocks": len(cases) * args.repetitions,
                      "scheduled_arms": len(schedule), "executed": False}))


def oracle_controls(args):
    """Emit supported command-executor calibration, explicitly not live arms."""
    data = load(args.split)
    destination = Path(args.destination).resolve()
    if destination.exists() or destination == ROOT or ROOT in destination.parents:
        raise ValueError("controls destination must be new and outside fixture checkout")
    destination.mkdir(parents=True)
    text = ["schema_version = 1", 'id = "judgement-oracle-controls"',
            'name = "Offline independent oracle discrimination"', "[run]",
            "repetitions = 1", "timeout_ms = 5000", 'isolation = "temp_copy"',
            "randomize_case_order = false", "fail_fast = false"]
    for control in ("correct", "wrong", "missing", "corrupt"):
        command = ("python3 " + shlex.quote(str(ROOT / "oracle_control.py")) + " "
                   + shlex.quote(args.split) + ' {{case_id}} ' + control)
        text += ["[[variants]]", "id = " + json.dumps(control),
                 'executor = "command"', "command = " + json.dumps(command),
                 "[variants.env]", 'PYTHONDONTWRITEBYTECODE = "1"']
    for case in data["cases"]:
        fixture = destination / case["id"]
        fixture.mkdir()
        for name, content in case["files"].items():
            (fixture / name).write_text(content)
        text += ["[[cases]]", "id = " + json.dumps(case["id"]),
                 "fixture = " + json.dumps(str(fixture)), 'prompt = "Offline oracle calibration only"',
                 "[[cases.judges]]", 'type = "metric_threshold"',
                 'metric = "command_exit_code"', "min = 0", "max = 0", "required = true"]
    (destination / "suite.toml").write_text("\n".join(text) + "\n")
    print(json.dumps({"calibration_attempts": len(data["cases"]) * 4, "executed": False}))


def summarize(args):
    """Conservative paired accuracy; no fabricated unknown telemetry or gain claims."""
    cases = {c["id"]: c for c in load(args.split)["cases"]}
    capture = read_json(args.capture)
    if set(capture) != {"schema_version", "observations"} or capture["schema_version"] != 1:
        raise ValueError("unsupported capture envelope")
    index = {}
    required = {"case_id", "arm", "repetition", "mode", "path", "trigger_observed",
                "consumer_applied", "terminal_status", "output", "measurements"}
    for obs in capture["observations"]:
        if set(obs) != required or obs["case_id"] not in cases or obs["arm"] not in ARMS:
            raise ValueError("unknown capture fields/case/arm")
        if obs["mode"] not in {"direct", "swarm", "editing"} or obs["path"] not in {
                "application_consumer", "domain_consumer", "scorer_fixture"}:
            raise ValueError("unsupported capture provenance")
        if (type(obs["repetition"]) is not int or obs["repetition"] < 0
                or type(obs["trigger_observed"]) is not bool
                or type(obs["consumer_applied"]) is not bool
                or obs["terminal_status"] not in {"completed", "failed", "cancelled", "unknown"}):
            raise ValueError("invalid capture facts")
        key = (obs["case_id"], obs["mode"], obs["repetition"], obs["arm"])
        if key in index:
            raise ValueError("duplicate/conflicting observation")
        if not isinstance(obs["measurements"], dict):
            raise ValueError("measurements must be an object; missing values remain null")
        for name, value in obs["measurements"].items():
            if name not in {"wall_time_ms", "judgement_latency_ms", "total_tokens", "cost_usd",
                            "discovery_calls", "verification_calls", "inference_calls", "provider_errors"}:
                raise ValueError("unknown measurement; normalize before export")
            if value is not None and (type(value) not in {int, float} or not math.isfinite(value) or value < 0):
                raise ValueError("measurement must be nonnegative finite number or null")
        index[key] = obs
    comparisons = []
    for feature in FEATURES:
        for off, on in [("global-off", "only-" + feature),
                        ("without-" + feature, "all-on")]:
            pairs = []
            for case_id, mode, repetition, arm in sorted(index):
                if arm != off or cases[case_id]["feature"] != feature:
                    continue
                baseline = index[(case_id, mode, repetition, off)]
                treatment = index.get((case_id, mode, repetition, on))
                if treatment is None:
                    continue
                eligible = all(o["path"] == "application_consumer" and o["trigger_observed"]
                               and o["terminal_status"] != "unknown" for o in (baseline, treatment))
                if not eligible:
                    continue
                expected = cases[case_id]["expected"]
                accuracy = [None if o["output"] is None else semantic_score(expected, o["output"])
                            for o in (baseline, treatment)]
                deltas = {}
                for metric in ("wall_time_ms", "judgement_latency_ms", "total_tokens", "cost_usd", "provider_errors"):
                    a, b = (o["measurements"].get(metric) for o in (baseline, treatment))
                    deltas[metric] = None if a is None or b is None else b - a
                pairs.append({"case_id": case_id, "mode": mode, "repetition": repetition,
                              "accuracy": accuracy, "deltas": deltas,
                              "consumer_applied": treatment["consumer_applied"]})
            scored = [p for p in pairs if None not in p["accuracy"]]
            comparisons.append({"feature": feature, "off": off, "on": on,
                "eligible_pairs": len(pairs), "scored_pairs": len(scored),
                "accuracy_delta": (sum(int(p["accuracy"][1]) - int(p["accuracy"][0])
                                       for p in scored) / len(scored)) if scored else None,
                "pairs": pairs})
    print(json.dumps({"schema_version": 1, "truth_provenance": "authored_synthetic",
                      "observed_arms": len(index), "comparisons": comparisons}, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    sub.add_parser("validate")
    p = sub.add_parser("oracle")
    p.add_argument("split", choices=("development", "held_out", "independent_public"))
    p.add_argument("case_id")
    p.add_argument("--workspace", default=".")
    p = sub.add_parser("materialize")
    p.add_argument("split", choices=("development", "held_out", "independent_public"))
    p.add_argument("mode", choices=("direct", "swarm", "editing"))
    p.add_argument("destination")
    p.add_argument("--subject-binary", help="absolute fresh approved workflow subject; not executed here")
    p.add_argument("--seed", type=int, default=20261007)
    p.add_argument("--repetitions", type=int, default=2)
    p = sub.add_parser("oracle-controls")
    p.add_argument("split", choices=("development", "held_out", "independent_public"))
    p.add_argument("destination")
    p = sub.add_parser("summarize")
    p.add_argument("split", choices=("development", "held_out", "independent_public"))
    p.add_argument("capture")
    args = parser.parse_args()
    if args.action == "validate":
        for split in ("development", "held_out", "independent_public"):
            print(split, len(load(split)["cases"]), "cases: valid")
    elif args.action == "materialize":
        materialize(args)
    elif args.action == "oracle-controls":
        oracle_controls(args)
    elif args.action == "summarize":
        summarize(args)
    else:
        case = next((c for c in load(args.split)["cases"] if c["id"] == args.case_id), None)
        if case is None:
            raise ValueError("unknown case")
        passed = check_workspace(case, args.workspace)
        print(json.dumps({"oracle_passed": passed}))
        raise SystemExit(0 if passed else 1)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, KeyError, TypeError) as error:
        print("fixture tooling failed: " + str(error), file=sys.stderr)
        raise SystemExit(2) from error
