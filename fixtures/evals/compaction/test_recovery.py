"""Behavioral recovery oracle tests; canonical synthetic event values are product inputs."""
import copy
import json
from pathlib import Path
import tempfile
import tomllib
import unittest

from run_consolidation import ConsolidationDriver
from run_recovery import RecoveryDriver, policy_failure, retained_checkpoint, restart_candidate, unchanged_prefix


def checkpoint(sequence, payload, key="exact-operation"):
    return {"sequence": sequence, "kind": {"trace_event": {"trace": {"payload": {
        "provider_event": {"event_type": "compaction_synthesis_checkpoint_v1", "detail": json.dumps({
            "version": 1, "operation_key": key, "host_payload": payload})}}}}}}


class RecoveryOracleTests(unittest.TestCase):
    def test_fake_only_configuration_and_real_policy_pressure(self):
        with tempfile.TemporaryDirectory() as root:
            driver = RecoveryDriver.__new__(RecoveryDriver)
            driver.config = Path(root) / "offline.toml"
            driver.control = Path(root)
            driver.timeout = 300
            driver.adaptive_packing = False
            driver.mismatch = True
            driver.fail_synthesis = False
            driver.configure("off")
            config = tomllib.loads(driver.config.read_text())
            self.assertEqual(config["model"]["profiles"]["offline"]["settings"]["fake_compaction_fixture"], "progressive-v1")
            self.assertNotIn("experimental_adaptive_packing", config["model"]["compaction"])
            self.assertEqual(config["model"]["provider_plugin_id"], "bcode.fake-provider")
            self.assertEqual(config["model"]["profiles"]["offline"]["settings"]["fake_compaction_outcome"], "success")
            pressure = ConsolidationDriver.__new__(ConsolidationDriver)
            pressure.__dict__.update(driver.__dict__)
            pressure.mismatch = False
            pressure.pressure = True
            pressure.threshold = 3500
            pressure.configure("proactive")
            config = tomllib.loads(pressure.config.read_text())
            self.assertEqual(config["model"]["compaction"]["mode"], "proactive")
            self.assertEqual(config["model"]["compaction"]["models"]["fake-cache-prefix"]["proactive_threshold_tokens"], 3500)
            self.assertEqual(config["model"]["profiles"]["offline"]["settings"]["fake_compaction_fixture"], "progressive-v1")

    def test_policy_failure_requires_new_canonical_failed_terminal(self):
        event = {"sequence": 9, "kind": {"trace_event": {"trace": {
            "phase": "context_compaction_finished", "payload": {"context_compaction": {
                "compacted": False, "message": "compaction packing policy changed; retained checkpoint requires original policy"}}}}}}
        self.assertEqual(policy_failure([event], 8), event)
        for events, boundary in (([event], 9), ([event, event], 8),
                                  ([{"sequence": 10, "kind": {"system_message": {
                                      "text": "compaction packing policy changed; retained checkpoint requires original policy"}}}], 8)):
            with self.assertRaises(ValueError):
                policy_failure(events, boundary)
        for phase, compacted in (("context_compaction_started", False), ("context_compaction_finished", True)):
            changed = copy.deepcopy(event)
            trace = changed["kind"]["trace_event"]["trace"]
            trace["phase"] = phase
            trace["payload"]["context_compaction"]["compacted"] = compacted
            with self.assertRaises(ValueError):
                policy_failure([changed], 8)

    def test_mismatch_corpus_is_small_without_bypassing_real_failure_gate(self):
        from unittest.mock import Mock
        for mismatch, repeats in ((True, 80), (False, 1800)):
            driver = RecoveryDriver.__new__(RecoveryDriver)
            driver.mismatch = mismatch
            driver.identity = {}
            driver.scenario = {"steps": [{"prompt": "scope"}, {"prompt": "fact"}, {}, {"prompt": "correction"}]}
            driver.start = Mock()
            driver.run = Mock(return_value=type("Result", (), {"stdout": "scratch-session"})())
            driver.send = Mock()
            driver.compact = Mock(side_effect=RuntimeError("stop after real failed-operation command"))
            with self.assertRaisesRegex(RuntimeError, "real failed-operation"):
                driver.execute()
            source = driver.send.call_args_list[-2].args[0]
            self.assertEqual(source, "lossless source fragment\n" + "evidence 🧭 漢字 e\u0301\n" * repeats)
            driver.compact.assert_called_once_with("failed-operation")
            self.assertEqual(driver.identity["recovery_source_repeat"], repeats)

    def test_retirement_never_exposes_older_checkpoint(self):
        with self.assertRaisesRegex(ValueError, "one exact retained"):
            retained_checkpoint([checkpoint(1, "accepted"), checkpoint(2, None)])

    def test_reserved_unaccepted_checkpoint_remains_inspectable(self):
        event = checkpoint(2, json.dumps({"next_chunk": 0, "reserved": 1}))
        self.assertEqual(retained_checkpoint([event])[0], event)

    def test_ambiguous_multiple_operations_fail_closed(self):
        with self.assertRaises(ValueError):
            retained_checkpoint([checkpoint(1, "a"), checkpoint(2, "b", "other")])

    def test_canonical_prefix_detects_rewriting_and_deleted_allowances(self):
        before = [checkpoint(1, "consumed=3"), checkpoint(2, "carry")]
        unchanged_prefix(before, before + [checkpoint(3, None)])
        for after in (before[1:], copy.deepcopy(before)):
            if len(after) == 2:
                after[0]["kind"] = {"replacement": "reset"}
            with self.assertRaisesRegex(ValueError, "canonical history"):
                unchanged_prefix(before, after)

    def test_candidate_requires_exact_session_and_no_blocker(self):
        good = {"version": 1, "session_id": "scratch", "blocker": None,
                "restart": {"version": 1, "session_id": "scratch"}}
        self.assertEqual(restart_candidate(good, "scratch"), good["restart"])
        for mutated in (dict(good, blocker="ambiguous_dispatch"),
                        dict(good, session_id="other"), dict(good, restart=None),
                        dict(good, restart={"version": 1, "session_id": "other"})):
            with self.assertRaises(ValueError):
                restart_candidate(mutated, "scratch")


if __name__ == "__main__":
    unittest.main()
