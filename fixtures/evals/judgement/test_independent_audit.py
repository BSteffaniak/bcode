"""Independent audit controls. No feature implementation or provider execution is read."""
from pathlib import Path
import unittest

import benchmark as b


class IndependentAuditTests(unittest.TestCase):
    def test_reserved_families_have_explicit_nonlive_provenance(self):
        audit = b.read_json(Path(__file__).with_name("independent-audit.json"))
        self.assertEqual(audit["schema_version"], 1)
        self.assertEqual(audit["evidence_status"],
                         "preregistered_counterexamples_not_live_evidence")
        families = audit["families"]
        self.assertEqual(len({f["id"] for f in families}), len(families))
        self.assertTrue(all(f["feature"] in b.FEATURES for f in families))
        self.assertTrue(all(f["input"] and f["required_behavior"] for f in families))

    def test_public_cases_are_executable_with_current_consumer_adapter(self):
        import runpy
        import tempfile
        import json
        root = Path(__file__).resolve().parents[3]
        adapter = runpy.run_path(str(root / "packages/eval/src/judgement_capture/materialize.py"))
        cases = b.load("independent_public")["cases"]
        reserved = b.read_json(Path(__file__).with_name("independent-audit.json"))
        self.assertEqual({c["family_id"] for c in cases},
                         {f["id"] for f in reserved["families"]})
        for case in cases:
            with self.subTest(case=case["id"]):
                consumer = adapter["adapt"](case, "task")
                adapter["validate_service_input"](consumer)
                self.assertNotEqual(consumer["kind"], "unavailable")
                self.assertEqual(adapter["oracle"](case, "task")["kind"],
                                 {"labels": "classification", "absent": "classification"}.get(
                                     case["expected"]["kind"], case["expected"]["kind"]))
                with tempfile.TemporaryDirectory() as workspace:
                    for name, text in case["files"].items():
                        (Path(workspace) / name).write_text(text)
                    (Path(workspace) / "answer.json").write_text(json.dumps(case["answer"]))
                    self.assertTrue(b.check_workspace(case, workspace))
                    (Path(workspace) / "answer.json").write_text('{}')
                    self.assertFalse(b.check_workspace(case, workspace))

    def test_public_editing_materialization_is_offline_and_labeled(self):
        import contextlib
        import io
        import json
        import tempfile
        from types import SimpleNamespace
        with tempfile.TemporaryDirectory() as root:
            output = Path(root) / "public-audit"
            with contextlib.redirect_stdout(io.StringIO()):
                b.materialize(SimpleNamespace(split="independent_public", mode="editing",
                                              destination=str(output), repetitions=1, seed=7))
            schedule = json.loads((output / "schedule.json").read_text())
            self.assertEqual(schedule["split"], "independent_public")
            self.assertEqual(schedule["truth_provenance"], "authored_synthetic")
            self.assertEqual(len(schedule["entries"]), 8 * len(b.ARMS))

    def test_service_vocabulary_mismatch_is_an_explicit_oracle_gap(self):
        import copy
        data = b.load("independent_public")
        self.assertEqual(b.service_vocabulary_gaps(data), [])
        case = next(c for c in data["cases"] if c["id"] == "audit-authority-not-keywords")
        self.assertEqual(case["answer"]["category"], "test")
        self.assertEqual(case["expected"]["values"]["category"], "code")
        mappings = b.read_json(Path(__file__).with_name("service-vocabulary-mapping.json"))
        self.assertEqual(mappings["explicit_mappings"][0]["independent_semantic_label"], "test")
        for label in ("test", "permission", "invented"):
            mismatched = copy.deepcopy(data)
            mismatched["cases"][0]["expected"]["values"]["category"] = label
            gaps = b.service_vocabulary_gaps(mismatched)
            self.assertEqual(len(gaps), 1)
            self.assertEqual(gaps[0]["reason"], "triage producer vocabulary mapping gap")
            self.assertEqual(gaps[0]["independent_truth"]["values"]["category"], label)
        self.assertTrue(b.service_vocabulary_gaps(b.load("development")))

    def test_labels_do_not_accept_numeric_boolean_impostors(self):
        expected = {"kind": "labels", "labels": {"succeeded": True}}
        self.assertTrue(b.semantic_score(expected, expected))
        self.assertFalse(b.semantic_score(
            expected, {"kind": "labels", "labels": {"succeeded": 1}}))

    def test_progress_rejects_pre_onset_late_and_timestamp_without_advice(self):
        expected = {"kind": "progress", "label": "stall",
                    "onset_ms": 100, "deadline_ms": 200}
        for label, timestamp in (("stall", 99), ("stall", 201),
                                 ("drift", 100), ("stall", True)):
            with self.subTest(label=label, timestamp=timestamp):
                self.assertFalse(b.semantic_score(expected, {
                    "kind": "progress", "label": label, "detected_ms": timestamp}))
        negative = dict(expected, label=None)
        self.assertFalse(b.semantic_score(negative, {
            "kind": "progress", "label": None, "detected_ms": 100}))
        self.assertTrue(b.semantic_score(negative, {
            "kind": "progress", "label": None, "detected_ms": None}))


if __name__ == "__main__":
    unittest.main()
