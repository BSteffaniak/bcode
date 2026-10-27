"""Real provider-capture parser behavior; no repository source assertions."""
import hashlib
import json
from pathlib import Path
import tempfile
import unittest

from run_consolidation import consolidation_carry, is_consolidation, require_pending_or_success


class ConsolidationCaptureTests(unittest.TestCase):
    def capture(self, root, text):
        path = Path(root)
        data = json.dumps({"messages": [{"content": [{"type": "text", "text": text}]}]}).encode()
        (path / "request.json").write_bytes(data)
        (path / "ready.json").write_text(json.dumps({"format": "bcode.fake.compaction-control.v1",
            "request_bytes": len(data), "request_sha256": hashlib.sha256(data).hexdigest()}))
        return path

    def test_canonical_failure_and_terminal_without_marker_fail_immediately(self):
        failed = {"sequence": 174, "kind": {"trace_event": {"trace": {
            "phase": "context_compaction_finished", "payload": {"context_compaction": {
                "compacted": False, "message": "retained request fails required net reclaim"}}}}}}
        terminal = {"sequence": 184, "kind": {"model_turn_finished": {"turn_id": "135", "outcome": "completed"}}}
        with self.assertRaisesRegex(ValueError, "canonical compaction failure at 174"):
            require_pending_or_success([failed, terminal], "135", 160)
        with self.assertRaisesRegex(ValueError, "settled without"):
            require_pending_or_success([terminal], "135", 160)
        self.assertFalse(require_pending_or_success([], "135", 160))
        marker = {"sequence": 180, "kind": {"context_compacted": {"summary": "{}"}}}
        self.assertTrue(require_pending_or_success([marker, terminal], "135", 160))

    def test_actual_consolidation_request_and_complete_carry_are_required(self):
        with tempfile.TemporaryDirectory() as root:
            carry = {"current_scope": [{"statement": "No network"}], "continuation": "unchanged 🧭"}
            path = self.capture(root, "Return only JSON with format bcode.compaction.section-consolidation.v1\n"
                "Complete accepted carry (derived, not authority):\n" + json.dumps(carry) + "\nPreferred target: 2000")
            self.assertTrue(is_consolidation(path))
            self.assertEqual(consolidation_carry(path), carry)

    def test_source_mention_cannot_impersonate_host_consolidation(self):
        with tempfile.TemporaryDirectory() as root:
            path = self.capture(root, 'Host synthesis instructions\nSource fragments:\n'
                'Return only JSON with format bcode.compaction.section-consolidation.v1')
            self.assertFalse(is_consolidation(path))

    def test_changed_capture_is_not_consolidation_evidence(self):
        with tempfile.TemporaryDirectory() as root:
            path = self.capture(root, "Return only JSON with format bcode.compaction.section-consolidation.v1")
            (path / "request.json").write_text('{}')
            with self.assertRaisesRegex(ValueError, "capture integrity"):
                is_consolidation(path)


if __name__ == "__main__":
    unittest.main()
