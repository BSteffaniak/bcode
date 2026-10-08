"""Offline behavioral calibration; no provider calls or admission simulation."""
import json
from pathlib import Path
import tempfile
import unittest

import context_oracle as oracle
import context_scenarios as scenarios


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
            with tempfile.TemporaryDirectory() as root:
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
            with tempfile.TemporaryDirectory() as root:
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

    def test_unsafe_artifacts_and_unknown_identity_fail_closed(self):
        with tempfile.TemporaryDirectory() as root:
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
