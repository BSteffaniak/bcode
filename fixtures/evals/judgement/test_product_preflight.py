"""Offline executable product-job preparation; no Bcode/provider execution."""
import copy
import json
from pathlib import Path
import tempfile
import unittest

import benchmark as b
import product_preflight as p


def provenance():
    subject = {"executable": "/approved/subject/bcode", "executable_sha256": "a" * 64,
        "artifact_id": "approved-artifact", "configuration_sha256": "b" * 64,
        "configuration_complete": True, "state_id": "approved-state", **p.SOL}
    return {"schema_version": 1,
        "launch": {"config_file": "/approved/providers/sol.toml", "state_root": "/approved/state",
                   "context": None, "endpoint_id": "approved-endpoint", "inline_overrides_accounted": True},
        "selection": p.SOL, "jev_selection": {"provider_plugin_id": "bcode.jev",
            "model_id": "jev-1.13.0", "auth_profile": "jev"},
        "tool_policy": {"approved": True, "allowed_tools": p.TOOLS, "workflow_tools_approved": False},
        "subjects": {feature: {"off": dict(subject), "on": dict(subject, configuration_sha256="c" * 64)}
                     for feature in b.FEATURES}}


class ProductPreflightTests(unittest.TestCase):
    def test_six_real_adapter_trigger_inputs_are_truth_independent(self):
        adapter = p.capture_adapter()
        for feature in b.FEATURES:
            with self.subTest(feature=feature):
                case = next(c for c in b.load("development")["cases"] if c["feature"] == feature)
                consumer = adapter.adapt(case, "task")
                adapter.validate_service_input(consumer)
                altered = copy.deepcopy(case)
                # Keep structural context/progress requirements; replace independent oracle values.
                altered["answer"] = {key: "wrong" for key in case["answer"]}
                if feature not in {"context_reranking", "progress_monitoring"}:
                    altered["expected"] = {"kind": "absent"}
                self.assertEqual(consumer, adapter.adapt(altered, "task"))
                self.assertIn(consumer["kind"], {"service", "eval_diagnosis"})

    def test_serial_suite_contains_supported_jobs_overlays_and_exact_launch(self):
        template = {"preregistration_id": "offline-calibration", "source": "scorer_calibration",
                    "subject": {}, "timeout_ms": 1000, "cache_condition": "unknown"}
        with tempfile.TemporaryDirectory() as root:
            output = Path(root) / "suite"
            manifest = p.prepare(provenance(), template, {}, output)
            self.assertEqual(len(manifest["entries"]), 12)
            self.assertEqual(manifest["concurrency"], 1)
            self.assertFalse(manifest["normal_turn_gain_eligible"])
            self.assertFalse(manifest["direct_editing_distinct_execution"])
            for feature in b.FEATURES:
                entries = [e for e in manifest["entries"] if e["feature"] == feature]
                self.assertEqual([e["arm"] for e in entries], ["off", "on"])
                for entry in entries:
                    job = json.loads((output / entry["job"]).read_text())
                    self.assertFalse(job["allow_live"])
                    self.assertIsNone(job["request"]["isolated_feature"])
                    self.assertEqual(job["request"]["subject"], entry["subject"])
                    self.assertEqual(entry["argv"][0], "/approved/subject/bcode")
                    self.assertIn("benchmark-capture", entry["argv"])
                    self.assertIn('model_id = "jev-1.13.0"', entry["env"]["BCODE_CONFIG_TOML"])
                    self.assertEqual(entry["env"]["BCODE_CONFIG"], "/approved/providers/sol.toml")
                    self.assertEqual(b.digest(output / entry["job"]), entry["job_sha256"])
            self.assertEqual(json.loads((output / "materialization-gaps.json").read_text()), [])

    def test_exact_provenance_rejects_unknown_or_mismatched_inputs(self):
        p.validate_provenance(provenance())
        for key in ("subjects", "tool_policy", "selection", "launch", "jev_selection"):
            altered = provenance()
            del altered[key]
            with self.assertRaises(ValueError):
                p.validate_provenance(altered)
        altered = provenance()
        altered["selection"] = dict(p.SOL, model="")
        with self.assertRaises(ValueError):
            p.validate_provenance(altered)
        altered = provenance()
        altered["subjects"][b.FEATURES[0]]["on"]["state_id"] = "foreign"
        with self.assertRaises(ValueError):
            p.validate_provenance(altered)

    def test_live_generation_rejected_without_user_authorization(self):
        with tempfile.TemporaryDirectory() as root:
            with self.assertRaises(ValueError):
                p.prepare(provenance(), {}, {}, Path(root) / "suite", allow_live=True)
            self.assertFalse((Path(root) / "suite").exists())
        declared = provenance()
        declared["tool_policy"]["approved"] = False
        declared["jev_selection"]["model_id"] = "catalog-verified-concrete-version"
        p.validate_provenance(declared)

    def test_off_requires_explicit_zero_observations_not_silence(self):
        request = {"isolated_feature": True}
        job = {"request": {"isolated_feature": None}}
        capture = {"request": request, "scope": "service_microbenchmark", "consumer_executed": True,
            "status": "captured", "telemetry": {"optional_counts_incomplete": False,
                "discovery_calls": 0, "verification_calls": 0, "inference_calls": 0,
                "optional_operations": [{"outcome": "disabled"}], "calls_complete": True, "calls": []}}
        self.assertTrue(p.check_capture({"arm": "off"}, job, capture)["ready"])
        silent = copy.deepcopy(capture)
        silent["telemetry"]["optional_operations"] = []
        self.assertFalse(p.check_capture({"arm": "off"}, job, silent)["ready"])
        for key in ("discovery_calls", "verification_calls", "inference_calls"):
            for value in (None, False, 1):
                bad = copy.deepcopy(capture)
                bad["telemetry"][key] = value
                self.assertFalse(p.check_capture({"arm": "off"}, job, bad)["ready"])
        bad = copy.deepcopy(capture)
        bad["telemetry"]["calls_complete"] = False
        self.assertFalse(p.check_capture({"arm": "off"}, job, bad)["ready"])


if __name__ == "__main__":
    unittest.main()
