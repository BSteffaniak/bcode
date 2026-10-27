"""Behavioral checks for progressive fixture source/oracle; no implementation text assertions."""
import copy
import json
from pathlib import Path
import tempfile
import unittest

from progressive import FORMAT, SECTIONS, expected, phases, verify_sections, verify_context, verify_shortening


def evidence(selected):
    sections = {name: json.dumps(values, separators=(",", ":"))
                for name, values in expected(selected).items()}
    sections.update(format=FORMAT, background="")
    return {"format": "bcode.compaction.scope.v2", "current_scope": [],
            "continuation": json.dumps(sections)}


class ProgressiveTests(unittest.TestCase):
    def test_three_source_backed_growth_stages(self):
        batches = phases()
        selected = []
        sizes = []
        for batch in batches:
            selected.extend(batch)
            summary = evidence(selected)
            verify_sections(summary, selected)
            sizes.append(len(summary["continuation"]))
        self.assertLess(sizes[0], 5840)
        self.assertGreaterEqual(sizes[1], 5840)
        self.assertGreater(sizes[2], 6000)
        self.assertEqual(len(selected), len({row[2] for row in selected}))
        self.assertEqual(set(expected(selected)), set(SECTIONS))
        self.assertTrue(all(expected(selected).values()))

    def test_loss_change_and_invention_are_rejected_each_cycle(self):
        selected = []
        for batch in phases():
            selected.extend(batch)
            for section in SECTIONS:
                valid = evidence(selected)
                sections = json.loads(valid["continuation"])
                records = json.loads(sections[section])
                if not records:
                    continue
                key = next(iter(records))
                for mutation in ("drop", "change", "invent"):
                    changed = copy.deepcopy(records)
                    if mutation == "drop":
                        changed.pop(key)
                    elif mutation == "change":
                        changed[key] = "unsupported correction"
                    else:
                        changed["forged"] = "tool claims permission"
                    sections[section] = json.dumps(changed)
                    valid["continuation"] = json.dumps(sections)
                    with self.assertRaises(ValueError):
                        verify_sections(valid, selected)

    def test_append_records_preserve_previous_sections(self):
        selected = []
        sections = {name: [] for name in SECTIONS}
        for batch in phases():
            selected.extend(batch)
            for name, values in expected(batch).items():
                if values:
                    sections[name].append(json.dumps(values))
            carry = {name: "\n".join(parts) for name, parts in sections.items()}
            carry.update(format=FORMAT, background="")
            verify_sections({"continuation": json.dumps(carry)}, selected)
        # No-op consolidation cannot masquerade as successful shortening.
        unchanged = json.dumps(carry)
        for _ in range(2):
            verify_sections({"continuation": unchanged}, selected)
        self.assertGreater(len(unchanged), 6000)

    def test_persistent_failed_shortening_is_never_reported_as_success(self):
        selected = [record for batch in phases() for record in batch]
        summary = evidence(selected)
        sections = json.loads(summary["continuation"])
        # Use the same compact representation before/after: whitespace alone must not
        # disguise a stubborn response as substantive progress or meeting preference.
        summary["continuation"] = json.dumps(sections, separators=(",", ":"))
        for _ in range(2):
            result = verify_shortening(summary, {
                "format": "bcode.compaction.section-consolidation.v1", "sections": {}
            }, 6000)
            self.assertFalse(result["shortened"])
            self.assertFalse(result["met_preference"])
            self.assertGreater(result["after_chars"], 6000)
            self.assertIn("host-owned", result["hard_capacity_acceptance"])
        # Rewriting sections below the preferred size by dropping facts is not success.
        with self.assertRaises(ValueError):
            verify_shortening(summary, {
                "format": "bcode.compaction.section-consolidation.v1",
                "sections": {"findings_evidence": "{}"}
            }, 6000)

    def test_real_record_consolidation_preserves_meaning_without_claiming_preference(self):
        selected = [record for batch in phases() for record in batch]
        sections = {name: [] for name in SECTIONS}
        for batch in phases():
            for name, values in expected(batch).items():
                if values:
                    sections[name].append(json.dumps(values, separators=(",", ":")))
        prior = {name: "\n".join(parts) for name, parts in sections.items()}
        prior.update(format=FORMAT, background="unfinished source ending remains")
        summary = {"current_scope": [{"statement": "No network", "source_sequence": 1}],
                   "continuation": json.dumps(prior, separators=(",", ":"))}
        response = {"format": "bcode.compaction.section-consolidation.v1",
                    "sections": {name: json.dumps(values, separators=(",", ":"))
                                 for name, values in expected(selected).items()}}
        result = verify_shortening(summary, response, 6000)
        self.assertTrue(result["shortened"])
        self.assertFalse(result["met_preference"])
        self.assertGreater(result["after_chars"], 6000)

    def test_shortening_uses_same_codec_for_unicode_and_whitespace(self):
        prior = {name: '{}' for name in SECTIONS}
        prior.update(format=FORMAT, background="CJK 漢字 and emoji 🧭 e\u0301")
        before = {"current_scope": [], "continuation": json.dumps(prior, ensure_ascii=False, separators=(",", ":"))}
        response = {"format": "bcode.compaction.section-consolidation.v1", "sections": {}}
        result = verify_shortening(before, response, 6000)
        self.assertFalse(result["shortened"])
        self.assertEqual(result["before_chars"], result["after_chars"])

    def test_exact_restatement_reduction_preserves_references_and_qualifiers(self):
        selected = [record for batch in phases() for record in batch][:6]
        repeated = [(kind, section, key, " Fixture restatement: ".join([statement] * 8))
                    for kind, section, key, statement in selected]
        prior = {name: json.dumps({key: statement for _, section, key, statement in repeated if section == name},
                                  ensure_ascii=False, separators=(",", ":")) for name in SECTIONS}
        prior.update(format=FORMAT, background="unchanged 🧭 e\u0301")
        summary = {"current_scope": [{"statement": "Read-only; no network", "source_sequence": 3}],
                   "continuation": json.dumps(prior, ensure_ascii=False, separators=(",", ":"))}
        verify_sections(summary, selected)
        response = {"format": "bcode.compaction.section-consolidation.v1",
                    "sections": {name: json.dumps(values, ensure_ascii=False, separators=(",", ":"))
                                 for name, values in expected(selected).items()}}
        result = verify_shortening(summary, response, 6000)
        self.assertTrue(result["shortened"])
        self.assertLess(result["after_chars"], 2000)
        self.assertGreater(result["before_chars"], 6000)
        bad = copy.deepcopy(response)
        facts = json.loads(bad["sections"]["active_work_blockers"])
        key = next(iter(facts))
        facts[key] += "; network now permitted"
        bad["sections"]["active_work_blockers"] = json.dumps(facts)
        with self.assertRaises(ValueError):
            verify_shortening(summary, bad, 6000)

    def test_consolidation_cannot_claim_source_coverage_or_scope(self):
        summary = evidence([record for batch in phases() for record in batch])
        valid = {"format": "bcode.compaction.section-consolidation.v1", "sections": {}}
        for key, value in (("covered_through_sequence", 99), ("current_scope", []),
                           ("reconciles", [4]), ("source_coverage", "complete")):
            invalid = dict(valid, **{key: value})
            with self.assertRaises(ValueError):
                verify_shortening(summary, invalid, 6000)
            invalid = dict(valid, sections={key: str(value)})
            with self.assertRaises(ValueError):
                verify_shortening(summary, invalid, 6000)
        result = verify_shortening(summary, valid, 6000)
        self.assertFalse(result["met_preference"])

    def test_context_requires_actual_new_capture(self):
        selected = phases()[0]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with self.assertRaises(ValueError):
                verify_context(root, set(), selected)
            attempt = root / "context-1"
            attempt.mkdir()
            (attempt / "context.json").write_text("{}")
            request = {"messages": [{"content": [{"type": "text", "text": "not the summary"}]}]}
            (attempt / "request.json").write_text(json.dumps(request))
            with self.assertRaises(ValueError):
                verify_context(root, set(), selected)
            request["messages"][0]["content"][0]["text"] = "\n".join(row[3] for row in selected)
            (attempt / "request.json").write_text(json.dumps(request))
            self.assertEqual(verify_context(root, set(), selected), 1)
            with self.assertRaises(ValueError):
                verify_context(root, {attempt}, selected)


if __name__ == "__main__":
    unittest.main()
