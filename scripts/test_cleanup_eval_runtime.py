"""Behavioral tests for harness cleanup routing and fail-closed retention."""
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("cleanup", Path(__file__).with_name("cleanup-eval-runtime.py"))
cleanup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cleanup)


class CleanupTests(unittest.TestCase):
    def test_prune_refusal_preserves_images_and_routes_only_private_root(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "state"
            images = root / "daemon-images"
            images.mkdir(parents=True)
            image = images / "fixture"
            image.write_bytes(b"image")
            calls = []

            def invoke(args, **kwargs):
                calls.append((args, kwargs["env"]))
                return subprocess.CompletedProcess(args, 1)

            with patch.object(cleanup.subprocess, "run", side_effect=invoke):
                self.assertFalse(cleanup.cleanup("/identified/bcode", directory))
            self.assertEqual(image.read_bytes(), b"image")
            self.assertEqual(len(calls), 2)
            for args, env in calls:
                self.assertEqual(args[1:3], ["--state-root", str(root)])
                self.assertNotIn("BCODE_SOCKET", env)
                self.assertEqual(env["BCODE_STATE_DIR"], str(root))

    def test_stop_failure_does_not_claim_release_pruning_decides_independently(self):
        with tempfile.TemporaryDirectory() as directory:
            (Path(directory) / "state" / "daemon-images").mkdir(parents=True)
            with patch.object(cleanup.subprocess, "run", side_effect=[
                subprocess.TimeoutExpired("stop", 40), subprocess.CompletedProcess("prune", 1)
            ]):
                self.assertFalse(cleanup.cleanup("/identified/bcode", directory))


if __name__ == "__main__":
    unittest.main()
