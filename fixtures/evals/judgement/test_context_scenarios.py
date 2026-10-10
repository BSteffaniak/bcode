"""Offline behavioral calibration; no provider calls or admission simulation."""
import json
import os
from pathlib import Path
import tempfile
import unittest

import context_oracle as oracle
import context_scenarios as scenarios


def workspace():
    return tempfile.TemporaryDirectory(dir=os.environ.get("BCODE_EVAL_TMP", "/Volumes/ehdd/bcode-eval-artifacts/tmp"))


class ContextScenarioTests(unittest.TestCase):
    def test_pair_equivalence_and_deterministic_order(self):
        selection = {"provider": "test", "model": "test", "profile": "test"}
        allowance = {"max_turns": 2, "timeout_seconds": 10}
        first = scenarios.equivalent_pairs(selection, allowance, 4)
        self.assertEqual(first, scenarios.equivalent_pairs(selection, allowance, 4))
        self.assertEqual(len(first["pairs"]), 12)
        self.assertEqual(first["corpus_digest"], scenarios.digest(scenarios.corpus()))
        for pair in first["pairs"]:
            item = scenarios.scenario(pair["scenario_id"])
            self.assertEqual(pair["shared_input_digest"], scenarios.digest({
                "scenario": item, "source_order": pair["source_order"],
                "selection": selection, "allowance": allowance}))
            self.assertEqual(set(pair["arms"]), {"off", "on"})
        self.assertGreater(len({tuple(p["source_order"]) for p in first["pairs"]}), 1)
        selection["model"] = "mutated"
        self.assertEqual(first["pairs"][0]["selection"]["model"], "test")

    def test_physical_budget_and_materialization(self):
        for item in scenarios.corpus()["scenarios"]:
            optional = [c for c in item["candidates"] if not c["required"]]
            self.assertTrue(all(len(c["content"].encode()) == 256 for c in optional))
            self.assertEqual(item["optional_budget_bytes"], 256)
            order = [c["id"] for c in reversed(item["candidates"])]
            with workspace() as root:
                payload = scenarios.materialize(item["scenario_id"], root, order)
                self.assertEqual([c["id"] for c in payload["candidates"]], order)
                self.assertEqual(set(p.name for p in Path(root).iterdir()),
                                 {"solution.py", "consumer-input.json"})
                self.assertFalse({"expected", "answer", "tests"} & set(payload))
                self.assertEqual(json.loads((Path(root) / "consumer-input.json").read_text()), payload)
                with self.assertRaises(ValueError):
                    scenarios.materialize(item["scenario_id"], root, order)

    def test_correct_wrong_missing_and_corrupt_artifacts(self):
        answers = {
            "retry-window-v1": ("retry_delay(attempt)", "min(75 * 2 ** attempt, 900)", "min(100 + 100 * attempt, 500)"),
            "local-minute-v1": ("utc_minute(local_minute)", "(local_minute - 345) % 1440", "(local_minute + 240) % 1440"),
            "bucket-route-v1": ("bucket_for(key)", "(key * 7 + 3) % 11", "key % 8"),
        }
        for identity, (signature, correct, wrong) in answers.items():
            with workspace() as root:
                path = Path(root) / "solution.py"
                self.assertEqual(oracle.grade(identity, root)["status"], "fail")
                for expression, expected in [(correct, "pass"), (wrong, "fail"), ("0", "fail")]:
                    path.write_text("def " + signature + ":\n    return " + expression + "\n")
                    scored = oracle.grade(identity, root)
                    self.assertEqual(scored["status"], expected)
                    self.assertEqual(set(scored["tests"]), set(scenarios.test_ids(identity)))
                    self.assertEqual(all(scored["tests"].values()), expected == "pass")
                for content in ["not python!", "import os\n", "x" * 16385,
                                "def " + signature + ":\n    return __import__('os').system('false')\n"]:
                    path.write_text(content)
                    self.assertEqual(oracle.grade(identity, root)["status"], "fail")

    def test_typed_adapter_physical_budget_and_pinned_identity(self):
        for item in scenarios.corpus()["scenarios"]:
            order = [c["id"] for c in item["candidates"]]
            origins = {identity: {"source": "memory", "provenance": "test-producer:" + identity}
                       for identity in order}
            request = scenarios.context_selection_request(item["scenario_id"], order, origins)
            self.assertEqual(set(request), {"task", "candidates", "budget_bytes"})
            self.assertEqual(request["budget_bytes"], 256)
            for authored, mapped in zip(item["candidates"], request["candidates"]):
                self.assertEqual(set(mapped), {"id", "source", "provenance", "snippet", "protected"})
                self.assertEqual(mapped["snippet"], authored["content"])
                self.assertEqual(mapped["protected"], authored["required"])
                self.assertEqual(mapped["id"], item["scenario_id"] + ":" + authored["id"])
                self.assertEqual(mapped["provenance"], origins[authored["id"]]["provenance"])
            optional_bytes = [len(c["snippet"].encode()) for c in request["candidates"] if not c["protected"]]
            self.assertEqual(optional_bytes, [256, 256, 256])
            self.assertGreater(sum(optional_bytes), request["budget_bytes"])
            self.assertEqual(sum(c["protected"] for c in request["candidates"]), 1)
            self.assertEqual(request, scenarios.context_selection_request(item["scenario_id"], order, origins))
            with self.assertRaises(ValueError):
                scenarios.context_selection_request(item["scenario_id"], order, {})
        manifest = scenarios.equivalent_pairs({"model": "test"}, {"turns": 1}, 4)
        self.assertEqual(manifest["adapter_binding"], "unbound_not_execution_ready")
        identities = {item["scenario_id"]: {
            c["id"]: {"source": "memory", "provenance": "test-producer:" + c["id"]}
            for c in item["candidates"]} for item in scenarios.corpus()["scenarios"]}
        bound = scenarios.equivalent_pairs({"model": "test"}, {"turns": 1}, 4, identities)
        self.assertEqual(bound["adapter_binding"], "producer_bound")
        self.assertNotEqual(bound["pairs"][0]["shared_input_digest"], manifest["pairs"][0]["shared_input_digest"])
        identities["retry-window-v1"]["contract-current"]["provenance"] = "test-producer:changed"
        changed = scenarios.equivalent_pairs({"model": "test"}, {"turns": 1}, 4, identities)
        self.assertNotEqual(bound["pairs"][0]["shared_input_digest"], changed["pairs"][0]["shared_input_digest"])
        tasks = scenarios.comparator_tasks(manifest)
        self.assertEqual(len({t["task_id"] for t in tasks}), 12)
        self.assertEqual(sum(t["repetitions"] for t in tasks), 12)
        self.assertEqual(len({t["family_id"] for t in tasks}), 3)
        for task, pair in zip(tasks, manifest["pairs"]):
            self.assertEqual(task["pairing_id"], pair["shared_input_digest"])
            self.assertEqual(task["test_ids"], scenarios.test_ids(pair["scenario_id"]))

    def test_extended_independent_domains_and_controls(self):
        self.assertEqual(len(scenarios.corpus()["scenarios"]), 3)
        self.assertEqual(len(scenarios.corpus(extended=True)["scenarios"]), 6)
        extended = scenarios.corpus(extended=True)
        negative = next(s for s in extended["scenarios"] if s["scenario_id"] == "sensor-clamp-v1")
        self.assertEqual(negative["control"], "negative_redundant_contract")
        self.assertIn("-40 through 125", negative["prompt"])
        for item in extended["scenarios"]:
            self.assertEqual([len(c["content"].encode()) for c in item["candidates"] if not c["required"]], [256] * 3)
        controls = {
            "storage-blocks-v1": ("blocks_for(byte_count)", "(byte_count + 4095) // 4096", "byte_count // 4096"),
            "sensor-clamp-v1": ("sanitize_reading(reading)", "max(-40, min(125, reading))", "max(0, min(100, reading))"),
            "invoice-rounding-v1": ("tax_cents(net_cents)", "(net_cents * 7 + 50) // 100", "net_cents * 7 // 100"),
        }
        for identity, (signature, correct, misleading) in controls.items():
            with workspace() as root:
                path = Path(root) / "solution.py"
                for expression, expected in [(correct, "pass"), (misleading, "fail"), ("0", "fail")]:
                    path.write_text("def " + signature + ":\n    return " + expression + "\n")
                    self.assertEqual(oracle.grade(identity, root)["status"], expected)
                path.write_text("def " + signature + ":\n    return 1 / 0\n")
                self.assertEqual(oracle.grade(identity, root)["status"], "fail")

    def test_symlinked_parent_cannot_supply_artifact_or_workspace(self):
        with workspace() as root, workspace() as outside:
            target = Path(outside) / "fresh"
            target.mkdir()
            alias = Path(root) / "alias"
            alias.symlink_to(outside, target_is_directory=True)
            item = scenarios.scenario("retry-window-v1")
            order = [c["id"] for c in item["candidates"]]
            with self.assertRaises(ValueError):
                scenarios.materialize(item["scenario_id"], alias / "fresh", order)
            (target / "solution.py").write_text("def retry_delay(attempt):\n    return min(900, 75 * 2 ** attempt)\n")
            self.assertEqual(oracle.grade(item["scenario_id"], alias / "fresh")["status"], "fail")

    def test_materialized_snippets_are_exact_utf8_and_oracle_not_copied(self):
        for item in scenarios.corpus(extended=True)["scenarios"]:
            with workspace() as root:
                payload = scenarios.materialize(item["scenario_id"], root,
                                                [c["id"] for c in item["candidates"]])
                recorded = json.loads((Path(root) / "consumer-input.json").read_text(encoding="utf-8"))
                self.assertEqual(recorded, payload)
                self.assertEqual([c["content"].encode("utf-8") for c in recorded["candidates"]],
                                 [c["content"].encode("utf-8") for c in item["candidates"]])
                self.assertEqual(set(p.name for p in Path(root).iterdir()),
                                 {"solution.py", "consumer-input.json"})

    def test_live_plan_history_shape_and_counterbalance(self):
        import context_live_plan as plans
        manifest = scenarios.equivalent_pairs({"model": "test"}, {"turns": 1}, 2, extended=True)
        bindings = {}
        for index, pair in enumerate(manifest["pairs"]):
            item = scenarios.scenario(pair["scenario_id"])
            bindings[pair["pair_id"]] = {}
            for arm_index, arm in enumerate(["off", "on"]):
                bindings[pair["pair_id"]][arm] = {
                    "session_id": "00000000-0000-0000-0000-" + str(index * 2 + arm_index + 1).zfill(12),
                    "turn_sequence": 20,
                    "excerpts": {c["id"]: {"sequence": i + 1, "snippet": c["content"]}
                                 for i, c in enumerate(item["candidates"]) if not c["required"]}}
        plan = plans.live_development_plan({"model": "test"}, {"turns": 1}, bindings)
        self.assertEqual(len(plan["jobs"]), 24)
        self.assertEqual(len({t["family_id"] for t in plan["tasks"]}), 6)
        self.assertEqual(sum(t["repetitions"] for t in plan["tasks"]), 12)
        self.assertEqual([j["arm"] for j in plan["jobs"][:4]], ["off", "on", "on", "off"])
        for job in plan["jobs"]:
            self.assertEqual(len(job["historical_context"]), 3)
            self.assertEqual(job["repetition"], 0)
            self.assertNotIn("tests", job)
            self.assertNotIn("expected", job)
        import copy
        duplicate = copy.deepcopy(bindings)
        first_pair = manifest["pairs"][0]["pair_id"]
        duplicate[first_pair]["on"]["session_id"] = duplicate[first_pair]["off"]["session_id"].replace("-", "")
        with self.assertRaises(ValueError):
            plans.live_development_plan({"model": "test"}, {"turns": 1}, duplicate)
        altered = copy.deepcopy(bindings)
        altered[first_pair]["on"]["excerpts"]["contract-current"]["snippet"] += " "
        with self.assertRaises(ValueError):
            plans.live_development_plan({"model": "test"}, {"turns": 1}, altered)
        first = bindings[first_pair]["off"]
        first["excerpts"]["contract-current"]["sequence"] = 20
        with self.assertRaises(ValueError):
            plans.live_development_plan({"model": "test"}, {"turns": 1}, bindings)

    def test_unsafe_artifacts_and_unknown_identity_fail_closed(self):
        with workspace() as root:
            target = Path(root) / "external.py"
            target.write_text("def bucket_for(key):\n    return (key * 7 + 3) % 11\n")
            (Path(root) / "solution.py").symlink_to(target)
            self.assertEqual(oracle.grade("bucket-route-v1", root)["status"], "fail")
        with self.assertRaises(ValueError):
            oracle.grade("future-case", ".")
        with self.assertRaises(ValueError):
            scenarios.equivalent_pairs({}, {"turns": 1})


if __name__ == "__main__":
    unittest.main()
