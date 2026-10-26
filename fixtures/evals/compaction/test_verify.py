#!/usr/bin/env python3
"""Pure verifier tests with synthetic vectors, NOT passing integration artifacts."""
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
import tomllib

from run_automatic import Driver

from verify import load_events, markers, verify_scope
from verify_automatic import accepted_checkpoint, capture, check_lossless, check_no_replay, coverage_keys, admitted_turn


class VerifierTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)

    def test_cross_run_workload_ignores_only_local_event_offsets(self):
        from verify_automatic import canonical_user_workload

        def event(sequence, text):
            return {"sequence": sequence, "kind": {"submission_activated": {"text": text}}}

        expected = canonical_user_workload([event(1, "directive 🧭"), event(7, "read-files relative.txt")])
        self.assertEqual(expected, canonical_user_workload([event(2, "directive 🧭"), event(9, "read-files relative.txt")]))
        for changed in ([event(2, "directive 🧭 "), event(9, "read-files relative.txt")],
                        [event(2, "directive 🧭"), event(9, "read-files other.txt")],
                        [event(2, "read-files relative.txt"), event(9, "directive 🧭")],
                        [event(2, "directive 🧭")],
                        [event(2, "directive 🧭"), event(9, "read-files relative.txt"), event(10, "directive 🧭")]):
            self.assertNotEqual(expected, canonical_user_workload(changed))

    def test_adaptive_config_opt_in_does_not_change_defaults(self):
        driver = object.__new__(Driver)
        driver.config = self.root / "config.toml"
        driver.control = self.root
        driver.timeout = 30
        driver.adaptive_packing = False
        driver.configure("proactive")
        config = tomllib.loads(driver.config.read_text())
        self.assertNotIn("experimental_adaptive_packing", config["model"]["compaction"])
        driver.adaptive_packing = True
        driver.configure("proactive")
        config = tomllib.loads(driver.config.read_text())
        self.assertIs(config["model"]["compaction"]["experimental_adaptive_packing"], True)
        self.assertEqual(config["model"]["compaction"]["models"]["fake-cache-prefix"]["proactive_threshold_tokens"], 4000)

    def attempt(self, text="#1 user:\n漢字 🧭 e\u0301", offset=0):
        path = self.root / "attempt-test"
        path.mkdir(exist_ok=True)
        source = {"sequence": 1, "provenance": "user", "byte_offset": offset,
                  "final_fragment": True, "text": text}
        raw = (json.dumps(source, ensure_ascii=False) + "\n").encode()
        request = json.dumps({"messages": [{"content": [{"text":
            "Prior structured synthesis: {}\n\nSource fragments (JSON lines):\n" + raw.decode()}]}]}).encode()
        (path / "sources.jsonl").write_bytes(raw)
        (path / "request.json").write_bytes(request)
        (path / "ready.json").write_text(json.dumps({"format": "bcode.fake.compaction-control.v1",
            "request_bytes": len(request), "request_sha256": hashlib.sha256(request).hexdigest(),
            "source_bytes": len(raw), "source_sha256": hashlib.sha256(raw).hexdigest()}))
        coverage = {key: source[key] for key in ("sequence", "provenance", "byte_offset", "final_fragment")}
        coverage.update(text_bytes=len(text.encode()), text_sha256=hashlib.sha256(text.encode()).hexdigest())
        (path / "coverage.json").write_text(json.dumps([coverage]))
        return path

    def test_exact_provider_source_and_byte_coverage(self):
        path = self.attempt()
        _, sources = capture(path)
        self.assertEqual(sources[0]["text"], "#1 user:\n漢字 🧭 e\u0301")
        self.assertEqual(next(iter(coverage_keys(path)))[-1], len(sources[0]["text"].encode()))

    def test_request_digest_tamper(self):
        path = self.attempt()
        (path / "request.json").write_text("{}")
        with self.assertRaisesRegex(ValueError, "integrity"):
            capture(path)

    def test_coverage_claim_is_not_evidence(self):
        path = self.attempt()
        (path / "coverage.json").write_text("[]")
        with self.assertRaisesRegex(ValueError, "coverage"):
            capture(path)

    def test_request_only_canary_in_source_rejected(self):
        path = self.attempt("COMPACTION_REQUEST_ONLY_CANARY_7391")
        with self.assertRaisesRegex(ValueError, "request-only"):
            capture(path)

    def test_overlap_with_different_fragment_boundary_is_replay(self):
        with self.assertRaisesRegex(ValueError, "requested again"):
            check_no_replay({(1, "user", 0, 10)}, {(1, "user", 5, 20)})
        check_no_replay({(1, "user", 0, 10)}, {(1, "user", 10, 20)})

    def test_lossless_unicode_source_matches_real_canonical_header(self):
        path = self.attempt()
        event = {"session_id": "s", "sequence": 1, "kind": {"user_message": {"text": "漢字 🧭 e\u0301"}}}
        check_lossless([path], [event], 1)
        event["kind"]["user_message"]["text"] += "lost tail"
        with self.assertRaisesRegex(ValueError, "clipped or rewritten"):
            check_lossless([path], [event], 1)

    def test_gap_rejected(self):
        path = self.attempt(offset=1)
        event = {"sequence": 1, "kind": {"user_message": {"text": "漢字 🧭 e\u0301"}}}
        with self.assertRaisesRegex(ValueError, "gap"):
            check_lossless([path], [event], 1)

    def test_export_conflicting_duplicate_and_sequence_gap(self):
        event = {"session_id": "s", "sequence": 0, "kind": {"user_message": {"text": "a"}}}
        path = self.root / "export.jsonl"
        path.write_text(json.dumps(event) + "\n" + json.dumps({**event, "sequence": 2}))
        with self.assertRaisesRegex(ValueError, "sequence gap"):
            load_events([path])
        path.write_text(json.dumps(event) + "\n" + json.dumps({**event, "kind": {}}))
        with self.assertRaisesRegex(ValueError, "conflicting"):
            load_events([path])

    def test_no_fabricated_marker_from_provider_status(self):
        event = {"sequence": 0, "kind": {"trace_event": {"status": "response_ready"}}}
        self.assertEqual(markers([event]), [])
        with self.assertRaisesRegex(ValueError, "real canonical"):
            verify_scope([event], [])

    def test_durable_checkpoint_required_and_privacy_checked(self):
        with self.assertRaisesRegex(ValueError, "durable acceptance"):
            accepted_checkpoint([])
        payload = {"format": "bcode.local-compaction.v1", "next_chunk": 1, "evidence": {}}
        envelope = {"version": 1, "operation_key": "test", "host_payload": json.dumps(payload)}
        event = {"sequence": 5, "kind": {"trace_event": {"trace": {"turn_id": None,
                 "phase": "model_provider_event", "payload": {"provider_event": {
                     "event_type": "compaction_synthesis_checkpoint_v1", "detail": json.dumps(envelope)}}}}}}
        self.assertEqual(accepted_checkpoint([event])[2], payload)
        payload["private"] = "COMPACTION_REQUEST_ONLY_CANARY_7391"
        envelope["host_payload"] = json.dumps(payload)
        event["kind"]["trace_event"]["trace"]["payload"]["provider_event"]["detail"] = json.dumps(envelope)
        with self.assertRaisesRegex(ValueError, "request-only"):
            accepted_checkpoint([event])

    def test_ordinary_continuation_requires_exact_admission_receipt(self):
        for kind in ("accepted", "existing", "deferred"):
            self.assertEqual(admitted_turn({"admission": {kind: {"turn_id": "exact"}}}), "exact")
        with self.assertRaisesRegex(ValueError, "ordinary continuation"):
            admitted_turn({"admission": {"rejected": "no"}})

    def test_latest_source_required_not_just_matching_statement(self):
        events = []
        for sequence, key, statement in [(1, "network", "No network access"),
                                         (2, "role", "Read-only analysis; do not edit files"),
                                         (3, "role", "Read-only analysis; do not edit files")]:
            text = 'BCODE_FIXTURE_SCOPE ' + json.dumps({"key": key, "statement": statement})
            events.append({"sequence": sequence, "kind": {"user_message": {"text": text}}})
        evidence = {"format": "bcode.compaction.scope.v2", "continuation": json.dumps({"facts": {"early": "violet-7391"}}),
                    "current_scope": [{"source_sequence": 1, "statement": "No network access"},
                                      {"source_sequence": 2, "statement": "Read-only analysis; do not edit files"}]}
        with self.assertRaisesRegex(ValueError, "chronological latest"):
            verify_scope(events, [({"kind": {"context_compacted": {"compacted_through_sequence": 3}}}, evidence)])
        evidence["current_scope"][1]["source_sequence"] = 3
        verify_scope(events, [({"kind": {"context_compacted": {"compacted_through_sequence": 3}}}, evidence)])


if __name__ == "__main__":
    unittest.main()
