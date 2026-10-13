"""Behavioral regressions for the external operations oracle (no live providers).

Run: python3 -B -m unittest discover -s fixtures/evals/swarm/oracles -p 'test_*.py'
"""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ORACLE_PATH = Path(__file__).with_name("operations.py")
spec = importlib.util.spec_from_file_location("operations_oracle", ORACLE_PATH)
oracle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(oracle)
CORRECT = "def total(values): return sum(values)\ndef unique_sorted(values): return sorted(set(values))\n"


class OperationsOracleTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.workspace = Path(self.temp.name)

    def check(self, source, timeout=0.5):
        (self.workspace / "operations.py").write_text(source)
        return oracle.check(self.workspace, timeout_seconds=timeout)

    def test_correct_subject(self):
        self.assertTrue(self.check(CORRECT))

    def test_faulty_subjects_and_wrong_types(self):
        for source in (
            "def total(v): return 0\ndef unique_sorted(v): return []\n",
            "def total(v): return sum(v)\ndef unique_sorted(v): return sorted(v)\n",
            "def total(v): return float(sum(v))\ndef unique_sorted(v): return sorted(set(v))\n",
            "def total(v): return False\ndef unique_sorted(v): return []\n",
            "def total(v): return sum(v)\ndef unique_sorted(v): return tuple(sorted(set(v)))\n",
            "def total(v): return sum(v)\ndef unique_sorted(v): return [True, 2, 3] if v else []\n",
        ):
            with self.subTest(source=source):
                self.assertFalse(self.check(source))

    def test_termination_and_exceptions_fail_closed(self):
        for source in (
            "import sys; sys.exit(0)\n",
            "import os; os._exit(0)\n",
            "import os, signal; os.kill(os.getpid(), signal.SIGKILL)\n",
            "raise RuntimeError('subject crashed')\n",
            "def total(v): raise RuntimeError('call crashed')\n",
            "def total(v):\n import sys\n sys.exit(0)\n",
            "def total(v):\n import os\n os._exit(0)\n",
            "def total(v):\n import os, signal\n os.kill(os.getpid(), signal.SIGKILL)\n",
        ):
            with self.subTest(source=source):
                self.assertFalse(self.check(source))

    def test_import_and_function_timeouts(self):
        for source in ("while True: pass\n", "def total(v):\n while True: pass\n"):
            with self.subTest(source=source):
                self.assertFalse(self.check(source, timeout=0.1))

    def test_pass_reports_cannot_certify_functions(self):
        for report in (
            "hidden functional checks passed",
            '{"passed":true}',
            '{"schema_version":1,"result":0,"passed":true}',
            '{"schema_version":1,"result":0}',
            '{"schema_version":1,"result":0,"result":6}',
        ):
            source = f"import os\nprint({report!r}, flush=True)\nos._exit(0)\n"
            with self.subTest(report=report):
                self.assertFalse(self.check(source))

    def test_noisy_and_oversized_output_is_not_protocol(self):
        self.assertFalse(self.check("print('passed')\n" + CORRECT))
        self.assertFalse(self.check("print('x' * 10000)\n" + CORRECT))

    def test_protocol_contract(self):
        accepted = [{"schema_version": 1, "result": 6},
                    {"schema_version": 1, "result": [1, 2, 3]}]
        self.assertTrue(oracle._valid_result(accepted[0], 6))
        self.assertTrue(oracle._valid_result(accepted[1], [1, 2, 3]))
        for payload in (None, [], {}, {"schema_version": True, "result": 6},
                        {"schema_version": 2, "result": 6},
                        {"schema_version": 1, "result": 6.0},
                        {"schema_version": 1, "result": True},
                        {"schema_version": 1, "result": 6, "passed": True}):
            with self.subTest(payload=payload):
                self.assertFalse(oracle._valid_result(payload, 6))
        with self.assertRaises(ValueError):
            json.loads('{"result":0,"result":6}',
                       object_pairs_hook=oracle._unique_object)

    def test_missing_and_symlink_subjects(self):
        self.assertFalse(oracle.check(self.workspace))
        original = self.workspace / "original.py"
        original.write_text(CORRECT)
        (self.workspace / "operations.py").symlink_to(original)
        self.assertFalse(oracle.check(self.workspace))

    def test_cli_exit_and_public_report(self):
        for source, code, message in (
            (CORRECT, 0, "hidden functional checks passed\n"),
            ("import sys; sys.exit(0)\n", 1, "hidden functional checks failed\n"),
        ):
            (self.workspace / "operations.py").write_text(source)
            result = subprocess.run(
                [sys.executable, "-I", "-B", str(ORACLE_PATH)],
                env={**os.environ, "BCODE_EVAL_WORKSPACE": str(self.workspace)},
                capture_output=True, text=True, timeout=15,
            )
            self.assertEqual(result.returncode, code)
            self.assertEqual(result.stdout, message)
            self.assertEqual(result.stderr, "")


if __name__ == "__main__":
    unittest.main()
