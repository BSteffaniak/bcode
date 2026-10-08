"""Behavioral calibration, no Bcode processes/provider calls/dependency installs."""
import contextlib
import io
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest

import benchmark as b


class DatasetTests(unittest.TestCase):
    def test_categorization_prompt_defines_vocabulary_independent_of_truth(self):
        case = next(c for c in b.load("development")["cases"]
                    if c["id"] == "dev-session-frontend-doc")
        prompt = b.task_prompt(case)
        self.assertIn("documentation", prompt)
        self.assertIn("debugging", prompt)
        self.assertIn("unknown", prompt)
        changed_truth = dict(case, answer={"task": "debugging"}, expected={})
        self.assertEqual(prompt, b.task_prompt(changed_truth))
        self.assertIn("exactly these top-level answer.json keys", prompt)
        self.assertIn("no extra keys: task", prompt)

    def test_prompt_schemas_are_truth_value_independent_for_all_splits(self):
        for split in ("development", "held_out", "independent_public"):
            for case in b.load(split)["cases"]:
                prompt = b.task_prompt(case)
                changed = dict(case, answer={key: "not truth" for key in case["answer"]},
                               expected={"kind": "absent"})
                self.assertEqual(prompt, b.task_prompt(changed), case["id"])
                for key in case["answer"]:
                    self.assertIn(key + ": " + b.ANSWER_TYPES[key], prompt)
                self.assertIn("no extra keys", prompt)
        with self.assertRaises(KeyError):
            b.task_prompt({"task": "Read evidence", "answer": {"unreviewed": True}})

    def test_trigger_contracts_cover_six_features_without_claiming_execution(self):
        self.assertEqual(set(b.FEATURES), set(b.TRIGGER_CONTRACTS))
        self.assertEqual(len({c["operation"] for c in b.TRIGGER_CONTRACTS.values()}), 6)
        for contract in b.TRIGGER_CONTRACTS.values():
            self.assertTrue(contract["prerequisite"])
            self.assertTrue(contract["application"])

    def test_supported_operation_preflight_off_on_and_unknown_fail_closed(self):
        for feature in b.FEATURES:
            contract = b.TRIGGER_CONTRACTS[feature]
            self.assertTrue(contract["supported_operation"])
            self.assertFalse(contract["normal_turn_supported"])
            facts = dict.fromkeys(b.PREFLIGHT_FACTS, True)
            facts["optional_calls"] = 0
            self.assertTrue(b.operation_preflight(feature, "global-off", facts)["ready"])
            facts["optional_calls"] = 1
            self.assertFalse(b.operation_preflight(feature, "global-off", facts)["ready"])
            self.assertTrue(b.operation_preflight(feature, "only-" + feature, facts)["ready"])
            facts["optional_calls"] = 0
            self.assertFalse(b.operation_preflight(feature, "only-" + feature, facts)["ready"])
            facts["explicit_fallback_or_abstention"] = True
            self.assertTrue(b.operation_preflight(feature, "only-" + feature, facts)["ready"])
            for key in b.PREFLIGHT_FACTS:
                incomplete = dict(facts, **{key: None})
                self.assertFalse(b.operation_preflight(feature, "global-off", incomplete)["ready"])
            for value in (None, False, -1, 0.0):
                self.assertFalse(b.operation_preflight(feature, "global-off",
                    dict(facts, optional_calls=value))["ready"])
            self.assertFalse(b.operation_preflight(feature, "global-off", facts)["normal_turn_ready"])

    def test_repetition_schedule_does_not_depend_on_prior_shuffle(self):
        root = Path(tempfile.mkdtemp(prefix="judgement-reproducible-"))
        schedules = []
        for count in (1, 2):
            dest = root / str(count)
            with contextlib.redirect_stdout(io.StringIO()):
                b.materialize(SimpleNamespace(split="development", mode="direct",
                    destination=str(dest), repetitions=count, seed=7))
            schedule = json.loads((dest / "schedule.json").read_text())
            schedules.append([e for e in schedule["entries"] if e["repetition"] == 0])
            for repetition in range(count):
                import random
                cases = list(b.load("development")["cases"])
                rng = random.Random(f"7:development:direct:{repetition}")
                rng.shuffle(cases)
                observed = [e["case_id"] for e in schedule["entries"]
                            if e["repetition"] == repetition]
                expected = [case["id"] for case in cases for _ in b.ARMS]
                self.assertEqual(observed, expected)
            self.assertFalse(schedule["direct_editing_distinct_execution"])
            for entry in schedule["entries"]:
                self.assertEqual(entry["trigger_evidence"], "unverified")
                contract = json.loads((dest / entry["block"] / "trigger-contract.json").read_text())
                self.assertFalse(contract["ordinary_task_success_is_trigger_evidence"])
            self.assertEqual(schedule["corpus_sha256"], b.digest(b.ROOT / "development.json"))
        self.assertEqual(*schedules)

    def test_all_features_disjoint_splits(self):
        development = b.load("development")["cases"]
        held = b.load("held_out")["cases"]
        self.assertFalse({c["id"] for c in development} & {c["id"] for c in held})
        self.assertEqual(set(b.FEATURES), {c["feature"] for c in held})

    def test_development_family_coverage_and_controls(self):
        cases = b.load("development")["cases"]
        for feature in b.FEATURES:
            group = [c for c in cases if c["feature"] == feature]
            self.assertGreaterEqual(len({c["family_id"] for c in group}), 6)
            self.assertGreaterEqual(sum(c["control"] == "positive" for c in group), 2)
            self.assertGreaterEqual(sum(c["control"] != "positive" for c in group), 2)
            self.assertTrue(any(c["fault"] != "none" for c in group))

    def test_optional_admission_matches_production_budget_semantics(self):
        import itertools
        cases = [c for c in b.load("development")["cases"]
                 if c["feature"] == "context_reranking"]
        for case in cases:
            expected = case["expected"]
            candidates = {c["id"]: c for c in case["input"]["candidates"]}
            ids = list(candidates)
            best = 0
            for order in itertools.permutations(ids):
                remaining_bytes = expected["budget"] * 128
                admitted = []
                for id in order:
                    candidate = candidates[id]
                    physical_bytes = candidate["size_units"] * 128
                    if candidate["protected"]:
                        admitted.append(id)
                    elif physical_bytes <= remaining_bytes:
                        admitted.append(id)
                        remaining_bytes -= physical_bytes
                self.assertTrue(set(expected["required"]) <= set(admitted))
                self.assertLessEqual(sum(expected["costs"][id] for id in admitted), expected["budget"])
                gain = sum(2 ** expected["relevance"][id] - 1 for id in admitted)
                best = max(best, gain)
                # All optional candidates cannot fit; protected candidates don't
                # remove optional capacity even when ranked first or last.
                self.assertLess(len(admitted), len(ids))
            self.assertEqual(best, expected["ideal_admission_gain"], case["id"])
        timezone = next(c for c in cases if c["id"] == "dev-context-timezone")
        e = timezone["expected"]
        self.assertEqual(e["costs"], {"theme": 1, "offset": 2, "policy": 0})
        self.assertEqual(e["budget"], 2)
        self.assertEqual(e["ideal_admission_gain"], 16)
        self.assertEqual(timezone["input"]["candidates"][2]["size_units"], 3)

    def test_recommendation_full_permutation_has_no_admission_penalty(self):
        import itertools
        import math
        cases = [c for c in b.load("development")["cases"]
                 if c["feature"] == "recommendations"]
        for case in cases:
            e = case["expected"]
            ids = e["all_ids"]
            self.assertEqual(e["budget"], sum(e["costs"].values()))
            self.assertEqual(e["ideal_admission_gain"], sum(2 ** r - 1 for r in e["relevance"].values()))
            for order in itertools.permutations(ids):
                # Captured admitted is the full returned permutation in any order.
                self.assertEqual(set(order), set(ids))
                self.assertLessEqual(sum(e["costs"][id] for id in order), e["budget"])
                gain = sum(2 ** e["relevance"][id] - 1 for id in order)
                self.assertEqual(gain, e["ideal_admission_gain"])
            if case["id"] == "dev-recommend-read":
                self.assertEqual(e["k"], 1)
                def dcg(order):
                    return sum((2 ** e["relevance"][id] - 1) / math.log2(i + 2)
                               for i, id in enumerate(order[:e["k"]]))
                ideal = dcg(["read", "write"])
                self.assertGreater(ideal, 0)
                self.assertEqual(dcg(["read", "write"]) / ideal, 1)
                self.assertEqual(dcg(["write", "read"]) / ideal, 0)
                self.assertTrue(b.semantic_score(e, {"kind": "ranking", "ids": ["read", "write"]}))
                self.assertFalse(b.semantic_score(e, {"kind": "ranking", "ids": ["read", "read"]}))

    def test_failure_truth_not_relabeled_after_pilot(self):
        cases = {c["id"]: c for c in b.load("development")["cases"]}
        triage = cases["dev-triage-denied"]["expected"]
        self.assertFalse(b.semantic_score(triage, {"kind": "labels", "values": {"category": "auth"}}))
        progress = cases["dev-progress-stall"]["expected"]
        self.assertEqual(progress["onset_ms"], 3000)
        self.assertFalse(b.semantic_score(progress, {"kind": "progress", "label": "stall", "detected_ms": 2000}))

    def test_semantic_oracle_controls(self):
        for split in ("development", "held_out"):
            for case in b.load(split)["cases"]:
                expected = case["expected"]
                if expected["kind"] == "ranking":
                    prefix = expected["prefix"]
                    valid = {"kind": "ranking", "ids": prefix + [
                        i for i in expected["all_ids"] if i not in prefix]}
                    self.assertTrue(b.semantic_score(expected, valid), case["id"])
                    self.assertFalse(b.semantic_score(expected, {"kind": "ranking", "ids": []}))
                    self.assertFalse(b.semantic_score(expected, {
                        "kind": "ranking", "ids": valid["ids"] + valid["ids"][:1]}))
                    self.assertFalse(b.semantic_score(expected, {
                        "kind": "ranking", "ids": valid["ids"] + ["injected"]}))
                    if prefix:
                        self.assertFalse(b.semantic_score(expected, {
                            "kind": "ranking", "ids": list(reversed(valid["ids"]))}))
                elif expected["kind"] == "progress":
                    output = {"kind": "progress", "label": expected["label"],
                              "detected_ms": expected["onset_ms"] if expected["label"] else None}
                    self.assertTrue(b.semantic_score(expected, output))
                    if expected["label"]:
                        output["detected_ms"] = expected["deadline_ms"] + 1
                        self.assertFalse(b.semantic_score(expected, output))
                else:
                    if expected["kind"] == "labels":
                        semantic = {"kind": "labels", "values": expected["values"]}
                    else:
                        semantic = expected
                    self.assertTrue(b.semantic_score(expected, semantic))
                    wrong = {"kind": "labels", "values": {"fabricated": "success"}}
                    self.assertFalse(b.semantic_score(expected, wrong))

    def test_artifact_oracle_preservation_wrong_missing_and_types(self):
        # Leave inspectable artifacts in authorized TMPDIR; no deletion.
        root = Path(tempfile.mkdtemp(prefix="judgement-oracle-calibration-"))
        for split in ("development", "held_out"):
            for case in b.load(split)["cases"]:
                workspace = root / case["id"]
                workspace.mkdir()
                for name, text in case["files"].items():
                    (workspace / name).write_text(text)
                answer = workspace / "answer.json"
                self.assertFalse(b.check_workspace(case, workspace))
                answer.write_text(json.dumps(case["answer"]))
                self.assertTrue(b.check_workspace(case, workspace), case["id"])
                answer.write_text('{"model_claim":"success"}')
                self.assertFalse(b.check_workspace(case, workspace))
                answer.write_text(json.dumps(case["answer"]))
                first = next(iter(case["files"]))
                (workspace / first).write_text("corrupted input")
                self.assertFalse(b.check_workspace(case, workspace))
        self.assertFalse(b.typed_equal({"x": True}, {"x": 1}))
        self.assertFalse(b.typed_equal({"x": None}, {"x": 0}))
        self.assertFalse(b.typed_equal({"x": 1, "extra": 2}, {"x": 1}))

    def test_bounded_json_duplicate_and_nonfinite_rejected(self):
        root = Path(tempfile.mkdtemp(prefix="judgement-json-calibration-"))
        path = root / "bad.json"
        for content in ('{"x":1,"x":2}', '{"x":NaN}', ' ' * (b.MAX_BYTES + 1)):
            path.write_text(content)
            with self.assertRaises(ValueError):
                b.read_json(path)

    def test_materialized_paired_schedule_and_toml(self):
        import tomllib
        root = Path(tempfile.mkdtemp(prefix="judgement-materialize-calibration-"))
        schedules = []
        for name in ("first", "second"):
            dest = root / name
            args = SimpleNamespace(split="development", mode="editing", destination=str(dest),
                                   adapter=None, repetitions=2, seed=7)
            with contextlib.redirect_stdout(io.StringIO()):
                b.materialize(args)
            schedule = json.loads((dest / "schedule.json").read_text())
            schedules.append(schedule)
            self.assertEqual(len(schedule["entries"]), 36 * 14 * 2)
            for manifest in dest.glob("*/suite.toml"):
                parsed = tomllib.loads(manifest.read_text())
                self.assertEqual(len(parsed["variants"]), 14)
                self.assertEqual(parsed["run"]["repetitions"], 1)
                for variant in parsed["variants"]:
                    config = tomllib.loads(variant["config_toml"])["judgement"]
                    self.assertEqual({f for f in b.FEATURES if config[f] == "auto"},
                                     set(b.ARMS[variant["id"]]))
            with self.assertRaises(ValueError):
                b.materialize(args)
        self.assertEqual(schedules[0], schedules[1])
        orders = [e["arm"] for e in schedules[0]["entries"]]
        self.assertNotEqual(orders[:14], orders[14:28])

    def test_direct_swarm_supported_executor_configs(self):
        import tomllib
        root = Path(tempfile.mkdtemp(prefix="judgement-executor-calibration-"))
        for mode in ("direct", "swarm"):
            args = SimpleNamespace(split="held_out", mode=mode,
                                   destination=str(root / mode), subject_binary=None,
                                   repetitions=1, seed=9)
            if mode == "swarm":
                with self.assertRaises(ValueError):
                    b.materialize(args)
                self.assertFalse(Path(args.destination).exists())
                # Structural offline validation only; never execute the test subject.
                args.subject_binary = str(Path(__file__).resolve())
            with contextlib.redirect_stdout(io.StringIO()):
                b.materialize(args)
            for manifest in Path(args.destination).glob("*/suite.toml"):
                suite = tomllib.loads(manifest.read_text())
                for variant in suite["variants"]:
                    self.assertEqual(variant["executor"], "workflow" if mode == "swarm" else "agent")
                    if mode == "swarm":
                        self.assertEqual(variant["metadata"]["workflow"]["concurrency"], 1)

    def test_runnable_offline_controls(self):
        import subprocess
        import tomllib
        root = Path(tempfile.mkdtemp(prefix="judgement-runnable-controls-"))
        dest = root / "materialized"
        with contextlib.redirect_stdout(io.StringIO()):
            b.oracle_controls(SimpleNamespace(split="development", destination=str(dest)))
        suite = tomllib.loads((dest / "suite.toml").read_text())
        self.assertEqual(len(suite["variants"]), 4)
        # Execute each existing-format command template in a fresh copied workspace.
        for case in suite["cases"]:
            for variant in suite["variants"]:
                workspace = root / (case["id"] + "-" + variant["id"])
                workspace.mkdir()
                for source in Path(case["fixture"]).iterdir():
                    (workspace / source.name).write_bytes(source.read_bytes())
                import shlex
                command = variant["command"].replace("{{case_id}}", shlex.quote(case["id"]))
                result = subprocess.run(shlex.split(command), cwd=workspace,
                                        capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, 0, result.stderr)
                output = json.loads(result.stdout)
                self.assertTrue(output["calibration_passed"])
                self.assertEqual(output["source"], "scorer_calibration")
                self.assertEqual(output["oracle_passed"], variant["id"] == "correct")

    def test_unknown_empty_capture_not_zero_gain(self):
        root = Path(tempfile.mkdtemp(prefix="judgement-capture-calibration-"))
        path = root / "empty.json"
        path.write_text('{"schema_version":1,"observations":[]}')
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            b.summarize(SimpleNamespace(split="development", capture=str(path)))
        report = json.loads(output.getvalue())
        self.assertEqual(report["observed_arms"], 0)
        self.assertTrue(all(c["accuracy_delta"] is None for c in report["comparisons"]))
        self.assertFalse(report["promotion_eligible"])
        self.assertEqual(len(report["coverage"]), len(b.FEATURES) * 3)
        self.assertTrue(all(c["scheduled_attempts"] is None and c["missing_attempts"] is None
                            and c["normal_path_gate"] == "unknown" for c in report["coverage"]))

    def test_task_success_with_unknown_telemetry_cannot_promote(self):
        case = b.load("development")["cases"][0]
        observations = [{"case_id": case["id"], "arm": arm, "repetition": 0,
            "mode": "direct", "path": "application_consumer", "trigger_observed": True,
            "consumer_applied": arm != "global-off", "terminal_status": "completed",
            "output": case["expected"], "measurements": {}}
            for arm in ("global-off", "only-" + case["feature"])]
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "capture.json"
            path.write_text(json.dumps({"schema_version": 1, "observations": observations}))
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                b.summarize(SimpleNamespace(split="development", capture=str(path)))
        report = json.loads(output.getvalue())
        self.assertFalse(report["promotion_eligible"])
        coverage = next(c for c in report["coverage"]
                        if c["feature"] == case["feature"] and c["mode"] == "direct")
        self.assertEqual(coverage["unknown_cost"], 2)
        self.assertEqual(coverage["observed_attempts"], 2)
        self.assertEqual(coverage["normal_path_gate"], "unknown")


if __name__ == "__main__":
    unittest.main()
