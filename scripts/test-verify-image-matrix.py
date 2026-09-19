"""Offline behavior tests for the opt-in image matrix runner."""
import importlib.util
from pathlib import Path
import unittest
import io
import os
import sys

spec = importlib.util.spec_from_file_location("image_matrix", Path(__file__).with_name("verify-image-matrix.py"))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class ReportTests(unittest.TestCase):
    def report(self):
        return {"schema_version": 1, "dry_run": False, "results": {"model": {
            "status": "observed", "report": {"schema_version": 1, "cases": [
                {"name": name, "context": "passed", "transfer": "inconclusive"}
                for name in ("no_image_control", "image_acknowledgement", "inline_follow_up", "inline_repeat")
            ]}}}}

    def test_wrong_model_or_source_is_not_evidence(self):
        report = self.report()
        self.assertEqual(runner.verdict(report, "other", "user"), "inconclusive")
        self.assertEqual(runner.verdict(report, "model", "tool_result"), "inconclusive")
        self.assertEqual(runner.verdict(report, "model", "user"), "passed")

    def test_duplicate_scenarios_cannot_hide_failure(self):
        report = self.report()
        cases = report["results"]["model"]["report"]["cases"]
        cases.insert(0, dict(cases[0], context="failed"))
        self.assertEqual(runner.verdict(report), "inconclusive")

    def test_malformed_shapes_fail_closed(self):
        for value in (None, [], "report", 1, {"schema_version": True},
                      {"schema_version": 1, "dry_run": False, "results": []}):
            self.assertEqual(runner.verdict(value), "inconclusive")
        for value in (None, [], "future", {"name": []}, {"name": "test", "context": []}):
            report = self.report()
            report["results"]["model"]["report"]["cases"] = [value]
            self.assertEqual(runner.verdict(report), "inconclusive")

    def test_probe_process_output_is_bounded(self):
        output = io.BytesIO()
        result = runner.run_probe([sys.executable, '-c', 'print("X" * (2 * 1024 * 1024))'],
                                  os.environ.copy(), output, 5)
        self.assertEqual(result['error'], 'report_size_limit_remote_completion_unknown')
        self.assertEqual(len(output.getvalue()), runner.MAX_REPORT_BYTES)

    def test_probe_process_timeout_and_success(self):
        result = runner.run_probe([sys.executable, '-c', 'import time; time.sleep(5)'],
                                  os.environ.copy(), io.BytesIO(), 0.05)
        self.assertEqual(result['error'], 'process_timeout_remote_completion_unknown')
        output = io.BytesIO()
        result = runner.run_probe([sys.executable, '-c', 'print("{}");'], os.environ.copy(), output, 5)
        self.assertEqual(result, {'exit_code': 0})
        self.assertEqual(output.getvalue(), b'{}\n')

    def test_missing_evidence_never_passes(self):
        self.assertEqual(runner.verdict({"schema_version": 1, "results": {}}), "inconclusive")
        report = self.report()
        report["results"]["model"]["report"]["cases"].pop()
        self.assertEqual(runner.verdict(report), "inconclusive")

    def test_complete_inline_report_passes_but_failure_does_not(self):
        report = self.report()
        self.assertEqual(runner.verdict(report), "passed")
        report["results"]["model"]["report"]["cases"][0]["context"] = "failed"
        self.assertEqual(runner.verdict(report), "failed")

    def test_unknown_report_and_guessed_answers_are_inconclusive(self):
        report = self.report()
        report["schema_version"] = 2
        self.assertEqual(runner.verdict(report), "inconclusive")
        report = self.report()
        report["results"]["model"]["report"]["cases"][0]["context"] = "inconclusive"
        self.assertEqual(runner.verdict(report), "inconclusive")


if __name__ == "__main__":
    unittest.main()
