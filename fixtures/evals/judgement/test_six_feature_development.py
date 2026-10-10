"""Behavioral oracle calibration, pair equivalence and fail-closed gates; no calls."""
import copy
import json
import os
from pathlib import Path
import tempfile
import unittest

import six_feature_development as tasks
import six_feature_oracle as oracle
import six_feature_plan as plans


CORRECT = {
    "checkout-dependency-repair": {"name": "checkout-api", "dependencies": {"express": "4.21.2"},
                                   "devDependencies": {"vitest": "2.1.9"}, "scripts": {"test": "vitest run"}},
    "payment-webhook-contract": {"route": "/hooks/payments", "events": ["payment.refunded", "payment.created"],
                                 "response_status": 204, "verify_signature": True, "health_route": "/healthz"},
    "inventory-pagination-stall": {"offset_formula": "(page - 1) * page_size", "order_by": ["created_at", "id"],
                                   "response_fields": ["items", "next_page"]},
    "cart-parser-tool-findability": {"selected_tool_id": "inspect-parser", "inspected_path": "cart.py"},
    "inventory-session-findability": {"session_id": "inventory"},
    "checkout-eval-findability": {"run_id": "checkout"},
}


def workspace():
    return tempfile.TemporaryDirectory(dir=os.environ.get("BCODE_EVAL_TMP", "/Volumes/ehdd/bcode-eval-artifacts/tmp"))


class DevelopmentTests(unittest.TestCase):
    def test_six_distinct_tasks_and_consumer_shapes(self):
        items = tasks.scenarios()
        self.assertEqual({i["feature"] for i in items}, set(tasks.FEATURES))
        self.assertEqual(len(items), 6)
        for item in items:
            self.assertEqual(item["split"], "development")
            self.assertTrue(item["trigger_contract"] and item["observable_outcome"] and item["api_gap"])
            self.assertNotIn("expected", item["producer_input"])
        triage = items[0]["producer_input"]["consumer"]["input"]
        self.assertEqual(set(triage), {"version", "invocation_id", "tool_name", "is_error", "partial", "excerpt_truncated", "output_excerpt"})
        self.assertIs(triage["is_error"], True)
        context = items[1]["producer_input"]["request"]
        self.assertGreater(sum(len(c["snippet"].encode()) for c in context["candidates"]), context["budget_bytes"])
        progress = items[2]["producer_input"]["consumer"]["input"]
        self.assertEqual([o["sequence"] for o in progress["observations"]], [1, 2, 3])
        self.assertEqual(len({o["failure_key"] for o in progress["observations"]}), 1)
        recommendation = items[3]["producer_input"]["consumer"]["input"]
        self.assertEqual(recommendation["purpose"], "recommendations")
        self.assertEqual(set(recommendation["candidates"][0]), {"id", "description", "protected"})
        items[0]["feature"] = "changed"
        self.assertEqual(tasks.scenarios()[0]["feature"], "tool_result_triage")

    def test_independent_correct_wrong_missing_corrupt_and_preservation(self):
        for identity, correct in CORRECT.items():
            with self.subTest(identity=identity), workspace() as root:
                tasks.materialize(identity, root)
                path = Path(root) / "result.json"
                self.assertEqual(oracle.grade(identity, root)["status"], "fail")
                for raw in ('{}', 'null', '{', '{"a":1,"a":2}', '{"a":NaN}', 'x' * 16385):
                    path.write_text(raw)
                    self.assertEqual(oracle.grade(identity, root)["status"], "fail")
                path.write_text(json.dumps(correct))
                result = oracle.grade(identity, root)
                self.assertEqual(result["status"], "pass")
                self.assertEqual(result["evidence_scope"], "artifact_only")
                self.assertTrue(all(result["tests"].values()))
                protected_name = next(iter(tasks.scenario(identity)["starter_files"]))
                protected = Path(root) / protected_name
                original_bytes = protected.read_bytes()
                protected.write_bytes(original_bytes + b"changed")
                self.assertEqual(oracle.grade(identity, root)["reason"], "input_preservation_failed")
                protected.write_bytes(original_bytes)
                bad = copy.deepcopy(correct)
                bad[next(iter(bad))] = "incorrect"
                path.write_text(json.dumps(bad))
                self.assertEqual(oracle.grade(identity, root)["status"], "fail")
        for identity, key, value in (("payment-webhook-contract", "verify_signature", False),
                                     ("inventory-pagination-stall", "response_fields", ["items"]),
                                     ("checkout-dependency-repair", "dependencies", {})):
            with workspace() as root:
                bad = copy.deepcopy(CORRECT[identity])
                bad[key] = value
                (Path(root) / "result.json").write_text(json.dumps(bad))
                self.assertEqual(oracle.grade(identity, root)["status"], "fail")

    def test_materialization_has_original_data_not_oracle(self):
        for item in tasks.scenarios():
            with workspace() as root:
                payload = tasks.materialize(item["scenario_id"], root)
                self.assertEqual(payload["producer_input"], item["producer_input"])
                self.assertEqual(set(p.name for p in Path(root).iterdir()), set(item["starter_files"]) | {"consumer-input.json"})
                for name, text in item["starter_files"].items():
                    self.assertEqual((Path(root) / name).read_bytes(), text.encode("utf-8"))
                with self.assertRaises(ValueError):
                    tasks.materialize(item["scenario_id"], root)

    def test_paired_inputs_are_deterministic_and_counterbalanced(self):
        selection, allowance = {"model": "test"}, {"turns": 1}
        first = plans.paired_plan(selection, allowance)
        self.assertEqual(first, plans.paired_plan(selection, allowance))
        self.assertEqual(len(first["pairs"]), 12)
        for pair in first["pairs"]:
            self.assertEqual(pair["shared_input_digest"], plans.digest(pair["shared"]))
            self.assertEqual({a["mode"] for a in pair["arms"]}, {"off", "on"})
            for arm in pair["arms"]:
                self.assertIs(arm["allow_live"], False)
                self.assertIsNone(arm["optional_calls"])
                auto = [f for f in tasks.FEATURES if arm["judgement_overlay"][f] == "auto"]
                self.assertEqual(auto, [pair["feature"]] if arm["mode"] == "on" else [])
        self.assertEqual([p["arms"][0]["mode"] for p in first["pairs"][:2]], ["off", "on"])
        selection["model"] = "changed"
        self.assertEqual(first["pairs"][0]["shared"]["selection"]["model"], "test")

    def test_unknown_scripted_historical_and_bool_counters_block(self):
        keys = ("subject_verified", "effective_selection_verified", "paired_inputs_verified",
                "feature_isolation_verified", "authority_preserved", "billing_known",
                "optional_ledger_complete", "normal_trigger_observed", "application_observed",
                "independent_artifact_observed", "producer_observed")
        off = dict.fromkeys(keys, True) | {"feature": "context_reranking", "evidence_scope": "current_normal_path", "optional_calls": {"discovery": 0, "verification": 0, "inference": 0}}
        on = copy.deepcopy(off)
        on["optional_calls"]["inference"] = 1
        self.assertTrue(plans.pair_evidence_gate(off, on)["complete_shape"])
        self.assertFalse(plans.pair_evidence_gate({}, {})["complete_shape"])
        for key in keys:
            bad = dict(on, **{key: None})
            self.assertFalse(plans.pair_evidence_gate(off, bad)["complete_shape"])
        for scope in ("scripted_control", "historical", "service_only"):
            self.assertFalse(plans.pair_evidence_gate(off, dict(on, evidence_scope=scope))["complete_shape"])
        on["optional_calls"]["inference"] = True
        self.assertFalse(plans.pair_evidence_gate(off, on)["complete_shape"])
        on["optional_calls"]["inference"] = 0
        self.assertFalse(plans.pair_evidence_gate(off, on)["complete_shape"])
        on["typed_outcome"] = "abstention"
        self.assertTrue(plans.pair_evidence_gate(off, on)["complete_shape"])
        self.assertFalse(plans.pair_evidence_gate(off, on)["benefit_claim"])

    def test_pagination_behavior_accepts_equivalence_rejects_defects_and_effects(self):
        identity = "inventory-pagination-stall"
        with workspace() as root:
            tasks.materialize(identity, root)
            artifact = Path(root) / "result.json"
            for formula, expected in (("page_size * (page + -1)", "pass"),
                                      ("page * page_size - page_size", "pass"),
                                      ("(page - 1) * page_size", "pass"),
                                      ("page - 1", "fail"), ("page * page_size", "fail"),
                                      ("page_size", "fail"), ("True", "fail"),
                                      ("(page - 1) * page_size % 1000", "fail"),
                                      ("__import__('os').system('false')", "fail"),
                                      ("[page for page in range(5)]", "fail"),
                                      ("2 ** 1000000", "fail"), ("1 // 0", "fail")):
                with self.subTest(formula=formula):
                    value = dict(CORRECT[identity], offset_formula=formula)
                    artifact.write_text(json.dumps(value))
                    self.assertEqual(oracle.grade(identity, root)["status"], expected)

    def test_recommendation_claim_is_only_artifact_quality(self):
        identity = "cart-parser-tool-findability"
        with workspace() as root:
            tasks.materialize(identity, root)
            (Path(root) / "result.json").write_text(json.dumps(CORRECT[identity]))
            artifact = oracle.grade(identity, root)
            self.assertEqual(artifact["status"], "pass")
            self.assertEqual(artifact["runtime_invocation"]["status"], "unavailable")
        self.assertEqual(oracle.corroborate_recommendation("session", "turn")["status"], "unavailable")
        self.assertEqual(oracle.corroborate_recommendation("session", "turn", CORRECT[identity])["status"], "unavailable")
        # Scripted envelopes only calibrate the adapter contract; not live evidence.
        projection = {"session_id": "session", "turn_id": "turn", "complete": True,
                      "evidence_scope": "current_normal_path", "public_reference": "test-only-receipt",
                      "invocations": [{"invocation_id": "read-1", "tool_name": "filesystem.read",
                                       "path": "cart.py", "settled_success": True}]}
        result = oracle.corroborate_recommendation("session", "turn", lambda *_: projection)
        self.assertEqual(result["status"], "corroborated")
        for key, value in (("complete", None), ("turn_id", "other"),
                           ("evidence_scope", "scripted_control"), ("public_reference", None)):
            bad = dict(projection, **{key: value})
            self.assertEqual(oracle.corroborate_recommendation("session", "turn", lambda *_: bad)["status"], "unavailable")
        projection["invocations"][0]["settled_success"] = False
        self.assertEqual(oracle.corroborate_recommendation("session", "turn", lambda *_: projection)["status"], "not_observed")
        projection["invocations"][0]["settled_success"] = True
        projection["invocations"][0]["path"] = "README.md"
        self.assertEqual(oracle.corroborate_recommendation("session", "turn", lambda *_: projection)["status"], "not_observed")
        facts = {"feature": "recommendations", "recommendation_runtime": {"status": "pass"}}
        gate = plans.pair_evidence_gate(facts, facts)
        self.assertIn("on:recommendation_public_invocation_uncorroborated", gate["blockers"])

    def test_symlink_parent_is_rejected(self):
        with workspace() as root, workspace() as outside:
            target = Path(outside) / "fresh"
            target.mkdir()
            alias = Path(root) / "alias"
            alias.symlink_to(outside, target_is_directory=True)
            with self.assertRaises(ValueError):
                tasks.materialize("checkout-dependency-repair", alias / "fresh")
            (target / "result.json").write_text(json.dumps(CORRECT["checkout-dependency-repair"]))
            self.assertEqual(oracle.grade("checkout-dependency-repair", alias / "fresh")["status"], "fail")


if __name__ == "__main__":
    unittest.main()
