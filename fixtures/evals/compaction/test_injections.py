"""Synthetic delivery-verifier tests, not successful integration artifacts."""
import json
from pathlib import Path
import tempfile
import unittest

from verify_injections import verify_injections


class InjectionEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)

    def attempt(self, outcome, response, correction=False, source="same", name="initial"):
        path = self.root / f"attempt-{name}"
        path.mkdir()
        (path / "ready.json").write_text(json.dumps({"outcome": outcome, "correction": correction,
                                                    "source_sha256": source}))
        (path / "finished.json").write_text(json.dumps({"response_ok": True}))
        (path / "response.json").write_text(response)
        return path

    def test_missing_delivery_is_not_coverage(self):
        with self.assertRaisesRegex(ValueError, "no actual"):
            verify_injections(self.root, "repair")

    def test_malformed_repair_requires_same_source_and_parseable_correction(self):
        self.attempt("repair", "{malformed")
        path = self.attempt("repair", "{}", correction=True, source="different", name="correction")
        with self.assertRaisesRegex(ValueError, "same-source"):
            verify_injections(self.root, "repair")
        ready = json.loads((path / "ready.json").read_text())
        ready["source_sha256"] = "same"
        (path / "ready.json").write_text(json.dumps(ready))
        self.assertEqual(verify_injections(self.root, "repair")["attempts"], 2)
        (path / "response.json").write_text("{still malformed")
        with self.assertRaises(ValueError):
            verify_injections(self.root, "repair")

    def test_scope_omission_requires_observed_effect_not_just_setting(self):
        self.attempt("omit-scope-repair", '{"current_scope":[]}')
        path = self.attempt("omit-scope-repair", '{"current_scope":[]}', correction=True, name="correction")
        with self.assertRaisesRegex(ValueError, "no effective"):
            verify_injections(self.root, "omit-scope-repair")
        (path / "response.json").write_text('{"current_scope":[{"source_sequence":7,"statement":"read only"}]}')
        verify_injections(self.root, "omit-scope-repair")

    def test_uncorrected_new_scope_does_not_hide_required_effective_repair(self):
        self.attempt("omit-scope-repair", '{"current_scope":[{"statement":"new"}]}', source="first")
        with self.assertRaisesRegex(ValueError, "no effective"):
            verify_injections(self.root, "omit-scope-repair")
        self.attempt("omit-scope-repair", '{"current_scope":[]}', source="later", name="bad")
        self.attempt("omit-scope-repair", '{"current_scope":[{"statement":"retained"}]}',
                     correction=True, source="later", name="fixed")
        verify_injections(self.root, "omit-scope-repair")

    def test_near_limit_measures_complete_carry_not_just_continuation(self):
        value = {"current_scope": [{"statement": "只读 🧭"}], "continuation": "{}"}
        text = json.dumps(value, ensure_ascii=False)
        value["continuation"] += " " * (5924 - len(text))
        path = self.attempt("near-limit", json.dumps(value, ensure_ascii=False))
        verify_injections(self.root, "near-limit")
        (path / "response.json").write_text(json.dumps({"continuation": "x" * 5924}))
        with self.assertRaisesRegex(ValueError, "complete serialized"):
            verify_injections(self.root, "near-limit")
