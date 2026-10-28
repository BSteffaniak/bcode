"""Fault delivery must be backed by captured carry, not incremental-update shape."""
import json
import tempfile
import unittest
from pathlib import Path

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

    def carry(self, path, scopes):
        prompt = "Prior structured synthesis: " + json.dumps({"current_scope": scopes})
        prompt += "\n\nSource fragments (JSON lines):\n"
        (path / "request.json").write_text(json.dumps({
            "messages": [{"content": [{"text": prompt}]}]}))

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
        initial = self.attempt("omit-scope-repair", '{"current_scope":[]}')
        path = self.attempt("omit-scope-repair", '{"current_scope":[]}', correction=True, name="correction")
        with self.assertRaisesRegex(ValueError, "no effective"):
            verify_injections(self.root, "omit-scope-repair")
        scopes = [{"source_sequence": 7, "statement": "read only"}]
        self.carry(initial, scopes)
        (path / "response.json").write_text(json.dumps({"current_scope": scopes}))
        verify_injections(self.root, "omit-scope-repair")

    def test_uncorrected_new_scope_does_not_hide_required_effective_repair(self):
        self.attempt("omit-scope-repair", '{"current_scope":[{"statement":"new"}]}', source="first")
        with self.assertRaisesRegex(ValueError, "no effective"):
            verify_injections(self.root, "omit-scope-repair")
        initial = self.attempt("omit-scope-repair", '{"current_scope":[]}', source="later", name="bad")
        self.carry(initial, [{"statement": "retained"}])
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


class InjectionTests(unittest.TestCase):
    def test_scope_fault_requires_effective_change_to_captured_carry(self):
        carried = [{"statement": "No network access", "source_sequence": 2, "supersedes": []}]
        for outcome in ("omit-scope-repair", "paraphrase-scope-repair"):
            with self.subTest(outcome=outcome), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                bad = [] if outcome == "omit-scope-repair" else [
                    dict(carried[0], statement="No network access (paraphrased)")]
                for name, correction, scope in (("initial", False, bad), ("repair", True, [])):
                    attempt = root / f"attempt-{name}"
                    attempt.mkdir()
                    (attempt / "ready.json").write_text(json.dumps({
                        "outcome": outcome, "correction": correction, "source_sha256": "same-source"}))
                    (attempt / "finished.json").write_text(json.dumps({"response_ok": True}))
                    (attempt / "response.json").write_text(json.dumps({"current_scope": scope}))
                # An empty incremental update alone is not evidence of lost scope.
                with self.assertRaises(ValueError):
                    verify_injections(root, outcome)
                request = root / "attempt-initial" / "request.json"
                prompt = "Prior structured synthesis: " + json.dumps({"current_scope": carried})
                prompt += "\n\nSource fragments (JSON lines):\n"
                request.write_text(json.dumps({"messages": [{"content": [{"text": prompt}]}]}))
                self.assertTrue(verify_injections(root, outcome)["fault_delivery_only"])
                (root / "attempt-initial" / "response.json").write_text(
                    json.dumps({"current_scope": carried}))
                with self.assertRaises(ValueError):
                    verify_injections(root, outcome)

    def test_near_limit_section_update_requires_intact_fixture_state(self):
        with tempfile.TemporaryDirectory() as directory:
            attempt = Path(directory) / "attempt-initial"
            attempt.mkdir()
            (attempt / "ready.json").write_text(json.dumps({"outcome": "near-limit"}))
            (attempt / "finished.json").write_text(json.dumps({"response_ok": True}))
            for background, accepted in (("BCODE_FIXTURE_STATE_V1 {}", True), ("lost state", False)):
                response = {"format": "bcode.compaction.section-update.v2",
                            "sections": {"background": background}}
                response["sections"]["background"] += " " * (5924 - len(json.dumps(response)))
                (attempt / "response.json").write_text(json.dumps(response))
                if accepted:
                    self.assertTrue(verify_injections(directory, "near-limit")["fault_delivery_only"])
                else:
                    with self.assertRaises(ValueError):
                        verify_injections(directory, "near-limit")
