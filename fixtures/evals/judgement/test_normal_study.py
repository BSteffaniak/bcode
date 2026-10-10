"""Offline behavioral tests and wire-shape preparation; no provider/daemon calls."""
import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import normal_study as study
import normal_study_oracle as oracle


def workspace():
    return tempfile.TemporaryDirectory(dir=os.environ.get("BCODE_EVAL_TMP", "/Volumes/ehdd/bcode-eval-artifacts/tmp"))


def correct(delimiter):
    return ('def encode_row(row):\n    return ' + repr(delimiter) +
            '.join([\'"\' + str(cell).replace(\'"\', \'""\') + \'"\' for cell in row]) + "\\n"\n')


def template():
    return {"schema_version": 1, "allow_live": False,
            "request": {"preregistration_id": "test", "task_id": "test", "repetition": 0,
                        "feature": "tool_result_triage", "arm": "off", "source": "live_consumer",
                        "capture_id": "test", "pairing_id": "test", "isolated_feature": None,
                        "subject": {"executable": "/test/bcode", "executable_sha256": "0" * 64,
                                    "artifact_id": "test-only", "configuration_sha256": "1" * 64,
                                    "configuration_complete": False, "provider": "test", "model": "test",
                                    "auth_profile": None, "model_profile": None, "state_id": "test-only"},
                        "cache_condition": "unknown", "timeout_ms": 60000},
            "consumer": {"kind": "normal_turn", "request": {
                "session_id": "00000000-0000-0000-0000-000000000001", "prompt": "test",
                "admission": {"execution": {"schema_version": 8, "agent_profile": "build",
                    "provider_plugin_id": "test", "model_id": "test",
                    "tool_allowlist": ["filesystem.read", "filesystem.write", "filesystem.edit", "shell.run"]}},
                "max_events": 1000, "categorization_dimension": "task"}}}


class NormalStudyTests(unittest.TestCase):
    def test_actual_triage_starter_failure_and_independent_repair(self):
        identity = "dev-tool-result-triage-positive"
        with workspace() as root:
            study.materialize(identity, root)
            # Execute only trusted authored starter/smoke, never model-generated code.
            before = subprocess.run([sys.executable, "smoke.py"], cwd=root, capture_output=True, timeout=3,
                                    env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1"))
            self.assertNotEqual(before.returncode, 0)
            self.assertIn(b"TypeError", before.stderr)
            self.assertEqual(oracle.grade(identity, root)["status"], "fail")
            (Path(root) / "exporter.py").write_text(correct("|"))
            after = oracle.grade(identity, root)
            self.assertEqual(after["status"], "pass")
            self.assertTrue(all(after["tests"].values()))
            self.assertEqual(after["runtime"]["status"], "unavailable")
            (Path(root) / "smoke.py").write_text("print('fake success')\n")
            self.assertEqual(oracle.grade(identity, root)["reason"], "preservation_failed")

    def test_correct_wrong_missing_corrupt_effects_and_negative_controls(self):
        for split, delimiter in (("development", "|"), ("new-heldout", ";")):
            for item in study.cases(split):
                if item["oracle"] != "csv-behavior-v1":
                    continue
                identity = item["case_id"]
                with self.subTest(identity=identity), workspace() as root:
                    study.materialize(identity, root)
                    starter = oracle.grade(identity, root)
                    self.assertEqual(starter["status"], "pass" if item["control"] == "negative" else "fail")
                    artifact = Path(root) / "exporter.py"
                    for source in ("", "import os\n", "def encode_row(row):\n    return __import__('os').system('false')\n",
                                   "def encode_row(row):\n    return 'wrong'\n",
                                   "def encode_row(row):\n    return 'x' * 100000000\n"):
                        artifact.write_text(source)
                        self.assertEqual(oracle.grade(identity, root)["status"], "fail")
                    artifact.write_text(correct(delimiter).replace("[", "(").replace("]", ")"))
                    self.assertEqual(oracle.grade(identity, root)["status"], "pass")
                    artifact.write_text(correct(","))
                    self.assertEqual(oracle.grade(identity, root)["status"], "fail")

    def test_materialization_exact_bytes_and_symlink_confinement(self):
        identity = "dev-tool-result-triage-positive"
        with workspace() as root, workspace() as outside:
            study.materialize(identity, root)
            self.assertEqual(set(p.name for p in Path(root).iterdir()), set(study.case(identity)["files"]))
            for name, content in study.case(identity)["files"].items():
                self.assertEqual((Path(root) / name).read_bytes(), content.encode())
            alias = Path(outside) / "alias"
            alias.symlink_to(root, target_is_directory=True)
            self.assertEqual(oracle.grade(identity, alias)["status"], "fail")
            with self.assertRaises(ValueError):
                study.materialize(identity, root)

    def test_exact_job_shape_pair_equivalence_and_history_gap(self):
        identity = "dev-tool-result-triage-positive"
        source = template()
        off = study.capture_job(source, identity, "off", "00000000-0000-0000-0000-000000000001")
        on = study.capture_job(source, identity, "on", "00000000-0000-0000-0000-000000000002")
        self.assertEqual(off["request"]["pairing_id"], on["request"]["pairing_id"])
        self.assertEqual(off["consumer"]["request"]["admission"], on["consumer"]["request"]["admission"])
        self.assertEqual(off["consumer"]["request"]["prompt"], on["consumer"]["request"]["prompt"])
        self.assertEqual(off["consumer"]["kind"], "normal_turn")
        self.assertFalse(on["allow_live"])
        self.assertIsNone(on["request"]["isolated_feature"])
        self.assertEqual(source, template())
        with self.assertRaises(ValueError):
            study.capture_job(source, "dev-context-reranking-positive", "on", on["consumer"]["request"]["session_id"])
        session = on["consumer"]["request"]["session_id"]
        refs = [{"session_id": session, "sequence": i} for i in (1, 2, 3)]
        context = study.capture_job(source, "dev-context-reranking-positive", "on", session, history_references=refs)
        self.assertEqual(context["consumer"]["request"]["admission"]["execution"]["historical_context"], refs)
        malformed = template()
        malformed["request"]["invented"] = True
        with self.assertRaises(ValueError):
            study.capture_job(malformed, identity, "off", session)

    def test_eval_diagnosis_cannot_be_substituted_with_normal_turn(self):
        with self.assertRaises(ValueError):
            study.capture_job(template(), "dev-eval-categorization-positive", "on",
                              "00000000-0000-0000-0000-000000000001")

    def test_public_triage_needs_actual_failure_and_repair_receipts(self):
        identity = "dev-tool-result-triage-positive"
        facts = {"case_id": identity, "session_id": "session", "turn_id": "turn", "scope": "current_normal_path",
                 "complete": True, "public_reference": "test-only", "observed_hook": True,
                 "application_observed": True, "terminal": "completed", "receipts": []}
        self.assertEqual(oracle.public_outcome(identity, "session", "turn", lambda *_: facts)["status"], "unavailable")
        facts["receipts"] = [{"sequence": i + 1, "invocation_id": "smoke-" + str(i),
                              "operation": "shell.run", "command": "python3 smoke.py", "settled_success": success}
                             for i, success in enumerate((False, True))]
        self.assertEqual(oracle.public_outcome(identity, "session", "turn", lambda *_: facts)["status"], "corroborated")
        facts["case_id"] = "dev-progress-monitoring-positive"
        self.assertEqual(oracle.public_outcome(facts["case_id"], "session", "turn", lambda *_: facts)["status"], "unavailable")
        facts["receipts"] = [{"sequence": i + 1, "invocation_id": "smoke-" + str(i),
                              "operation": "shell.run", "command": "python3 smoke.py", "settled_success": success}
                             for i, success in enumerate((False, False, False, True))]
        self.assertEqual(oracle.public_outcome(facts["case_id"], "session", "turn", lambda *_: facts)["status"], "corroborated")

    def test_freeze_controls_prerandomization_and_combined_scope(self):
        schedule = study.frozen_schedule()
        self.assertEqual(schedule, study.frozen_schedule())
        self.assertEqual(len(schedule["cases"]), 45)
        self.assertEqual(len(schedule["pairs"]), 135)
        self.assertEqual(schedule["corpus_sha256"], study.digest(schedule["cases"]))
        for feature in study.FEATURES:
            self.assertEqual({i["control"] for i in schedule["cases"] if i["feature"] == feature},
                             {"positive", "negative", "outage"})
        self.assertGreater(len({tuple(p["arms"]) for p in schedule["pairs"]}), 1)
        combined = study.combined_schedule()
        self.assertFalse(combined["isolated_feature"])
        self.assertEqual(len(combined["blocks"]), 3)
        self.assertNotIn("context_reranking", combined["case"]["hooks"])
        self.assertNotIn("eval_categorization", combined["case"]["hooks"])

    def test_three_distinct_families_behavior_and_genuine_smoke_failures(self):
        families = {i["family_id"] for i in study.repair_cases()}
        self.assertEqual(families, {"record-ids", "checkout-validation", "confined-path"})
        for item in study.repair_cases():
            with self.subTest(case=item["case_id"]), workspace() as root:
                study.materialize(item["case_id"], root)
                if item["control"] == "negative":
                    self.assertEqual(oracle.grade(item["case_id"], root)["status"], "pass")
                    continue
                # Trusted authored smoke only; never executes generated submissions.
                before = subprocess.run([sys.executable, "smoke.py"], cwd=root, capture_output=True, timeout=3,
                                        env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1"))
                self.assertNotEqual(before.returncode, 0)
                self.assertEqual(oracle.grade(item["case_id"], root)["status"], "fail")
                negative = next(i for i in study.repair_cases() if i["family_id"] == item["family_id"]
                                and i["feature"] == item["feature"] and i["control"] == "negative")
                (Path(root) / "repair.py").write_text(negative["files"]["repair.py"])
                self.assertEqual(oracle.grade(item["case_id"], root)["status"], "pass")
                (Path(root) / "repair.py").write_text("import os\n")
                self.assertEqual(oracle.grade(item["case_id"], root)["status"], "fail")
        with workspace() as root:
            item = study.repair_cases()[0]
            study.materialize(item["case_id"], root)
            (Path(root) / "repair.py").write_text("def selected_ids(records):\n    return __import__('os').system('false')\n")
            self.assertEqual(oracle.grade(item["case_id"], root)["status"], "fail")

    def test_workflow_suite_materializes_current_schema_without_execution(self):
        import tomllib
        with workspace() as root:
            result = study.workflow_failure_suite(root, "/immutable/bcode", "[judgement]\nmode = 'off'\n",
                                                  state_root="/external/state", profile="sol", config_file="/external/sol.toml")
            suite = tomllib.loads((Path(root) / "suite.toml").read_text())
            self.assertEqual(suite["schema_version"], 1)
            self.assertEqual(suite["variants"][0]["executor"], "workflow")
            self.assertEqual(suite["variants"][0]["metadata"]["workflow"]["binary"], "/immutable/bcode")
            self.assertEqual(suite["cases"][0]["judges"][0]["type"], "command")
            self.assertEqual(suite["variants"][0]["metadata"]["daemon_isolation"], "isolated")
            self.assertEqual(suite["variants"][0]["env"]["BCODE_MODEL_PROFILE"], "sol")
            self.assertNotIn("BCODE_CONFIG_TOML", suite["variants"][0]["env"])
            import ast
            tree = ast.parse((Path(root) / "oracle.py").read_text())
            byte_literals = [n.value for n in ast.walk(tree) if isinstance(n, ast.Constant) and isinstance(n.value, bytes)]
            self.assertEqual(byte_literals, [b"keep-this-exact\n"])
            self.assertIn("no failure is guaranteed", result["gap"])
            run = subprocess.run([sys.executable, str(Path(root) / "oracle.py")], cwd=Path(root) / "workspace",
                                 capture_output=True, timeout=3)
            self.assertEqual(run.returncode, 1)
            (Path(root) / "workspace" / "operations.py").write_text("def total(values):\n    return sum(values) + 0\n")
            run = subprocess.run([sys.executable, str(Path(root) / "oracle.py")], cwd=Path(root) / "workspace",
                                 capture_output=True, timeout=3)
            self.assertEqual(run.returncode, 0)
            (Path(root) / "workspace" / "protected.txt").write_bytes(b"keep-this-exact\\n")
            run = subprocess.run([sys.executable, str(Path(root) / "oracle.py")], cwd=Path(root) / "workspace",
                                 capture_output=True, timeout=3)
            self.assertEqual(run.returncode, 1)
            (Path(root) / "workspace" / "protected.txt").write_bytes(b"keep-this-exact\n")
            (Path(root) / "workspace" / "operations.py").write_text("def total(values):\n    return sum(values) + 1\n")
            run = subprocess.run([sys.executable, str(Path(root) / "oracle.py")], cwd=Path(root) / "workspace",
                                 capture_output=True, timeout=3)
            self.assertEqual(run.returncode, 1)
            (Path(root) / "workspace" / "protected.txt").write_text("changed")
            run = subprocess.run([sys.executable, str(Path(root) / "oracle.py")], cwd=Path(root) / "workspace",
                                 capture_output=True, timeout=3)
            self.assertEqual(run.returncode, 1)
            with self.assertRaises(ValueError):
                study.workflow_failure_suite(root, "/immutable/bcode", "", state_root="/external/state",
                                             profile="sol", config_file="/external/sol.toml")

    def test_workflow_shared_environment_is_explicit_and_overlay_not_duplicated(self):
        import tomllib
        overlay = "[judgement]\nmode = 'off'\n"
        env = {"BCODE_STATE_DIR": "/external/state", "BCODE_SESSION_STORE_DIR": "/external/state/sessions",
               "BCODE_CONFIG": "/external/sol.toml", "BCODE_MODEL_PROFILE": "sol", "BCODE_CONFIG_TOML": overlay}
        with workspace() as root:
            study.workflow_failure_suite(root, "/immutable/bcode", overlay, state_root="/external/state",
                                         profile="sol", config_file="/external/sol.toml", daemon_isolation="shared", variant_env=env)
            variant = tomllib.loads((Path(root) / "suite.toml").read_text())["variants"][0]
            self.assertNotIn("config_toml", variant)
            self.assertNotIn("profile", variant)
            self.assertEqual(variant["env"]["BCODE_CONFIG_TOML"], overlay)
            self.assertEqual(variant["metadata"]["daemon_isolation"], "shared")
        for kwargs in ({"daemon_isolation": "shared"},
                       {"daemon_isolation": "shared", "variant_env": dict(env, BCODE_MODEL_PROFILE="other")},
                       {"daemon_isolation": "isolated", "variant_env": env}):
            with workspace() as root, self.assertRaises(ValueError):
                study.workflow_failure_suite(root, "/immutable/bcode", overlay, state_root="/external/state",
                                             profile="sol", config_file="/external/sol.toml", **kwargs)
        with workspace() as root, self.assertRaises(ValueError):
            study.workflow_failure_suite(root, "/immutable/bcode", overlay, state_root="relative",
                                         profile="sol", config_file="/external/sol.toml")

    def test_driver_envelope_preserves_policy_and_oracle_identity(self):
        outer = {"capture_template": template(), "oracle_module": "/outside/oracle.py", "oracle_sha256": "a" * 64,
                 "oracle_dependencies": {"/outside/study.py": "b" * 64}, "arm": {"overlay_toml": "unchanged"}}
        item = study.repair_cases()[0]
        job = study.driver_job(outer, item["case_id"], "off", "00000000-0000-0000-0000-000000000001")
        self.assertEqual(job["starter_files"], item["files"])
        self.assertEqual(job["artifact"], "repair.py")
        self.assertEqual(job["oracle_sha256"], outer["oracle_sha256"])
        self.assertEqual(job["arm"], outer["arm"])
        self.assertEqual(job["capture_template"]["consumer"]["request"]["admission"],
                         outer["capture_template"]["consumer"]["request"]["admission"])

    def test_navigation_claims_do_not_create_actual_retrieval(self):
        identity = "dev-session-categorization-positive"
        with workspace() as root:
            study.materialize(identity, root)
            (Path(root) / "result.json").write_text('{"selected_id":"orders"}')
            result = oracle.grade(identity, root)
            self.assertEqual(result["status"], "unavailable")
            self.assertIsNone(result["tests"]["behavior"])
        self.assertEqual(oracle.public_outcome(identity, "session", "turn")["status"], "unavailable")
        # Calibration only: no live evidence is manufactured by a passing unit test.
        facts = {"case_id": identity, "session_id": "session", "turn_id": "turn", "scope": "current_normal_path",
                 "complete": True, "public_reference": "test-only", "observed_hook": True,
                 "application_observed": True, "terminal": "completed", "receipts": []}
        self.assertEqual(oracle.public_outcome(identity, "session", "turn", lambda *_: facts)["status"], "fail")
        facts["receipts"] = [{"sequence": 1, "invocation_id": "test-only", "operation": "session.open",
                              "record_id": "orders", "settled_success": True}]
        self.assertEqual(oracle.public_outcome(identity, "session", "turn", lambda *_: facts)["status"], "corroborated")
        facts["complete"] = None
        self.assertEqual(oracle.public_outcome(identity, "session", "turn", lambda *_: facts)["status"], "unavailable")


if __name__ == "__main__":
    unittest.main()
