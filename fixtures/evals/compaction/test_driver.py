"""Driver behavior tests: process death is not a daemon restart authorization."""
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch

from run_automatic import Driver


class DriverTests(unittest.TestCase):
    def driver(self, directory):
        driver = Driver.__new__(Driver)
        driver.work = Path(directory)
        driver.repo = Path(directory)
        driver.base = ["/not-a-bcode-binary"]
        driver.env = {}
        driver.timeout = 1
        driver.identity = {}
        driver.server = Mock()
        return driver

    def test_dead_daemon_never_invokes_autostarting_cli(self):
        with tempfile.TemporaryDirectory() as directory:
            driver = self.driver(directory)
            driver.server.poll.return_value = -6
            with patch("run_automatic.subprocess.Popen") as spawn:
                with self.assertRaisesRegex(RuntimeError, "exited before CLI"):
                    driver.run("session", "export", "synthetic")
                spawn.assert_not_called()

    def test_daemon_death_during_cli_kills_client_and_retains_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            driver = self.driver(directory)
            driver.server.poll.side_effect = [None, -6]
            client = Mock()
            client.communicate.side_effect = [subprocess.TimeoutExpired("cli", .1),
                                               ("partial stdout", "transport loss")]
            client.returncode = -9
            with patch("run_automatic.subprocess.Popen", return_value=client):
                with self.assertRaisesRegex(RuntimeError, "exited during CLI"):
                    driver.run("cancel", "synthetic")
            client.kill.assert_called_once()
            retained = json.loads((Path(directory) / "failed-cli.json").read_text())
            self.assertEqual(retained["args"], ["cancel", "synthetic"])
            self.assertEqual(retained["stderr"], "transport loss")

    def test_wait_checks_daemon_before_observation_side_effect(self):
        with tempfile.TemporaryDirectory() as directory:
            driver = self.driver(directory)
            driver.server.poll.return_value = -6
            observation = Mock(return_value=True)
            with self.assertRaisesRegex(RuntimeError, "isolated daemon exited"):
                driver.until(observation, "terminal event")
            observation.assert_not_called()

    def test_stop_escalates_only_owned_child_and_preserves_primary_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            driver = self.driver(directory)
            owned = driver.server
            owned.pid = 123
            owned.poll.return_value = None
            owned.wait.side_effect = [subprocess.TimeoutExpired("owned", 30), -9]
            driver.logs = []
            driver.identity["failure"] = "original export handshake timeout"
            with self.assertRaisesRegex(ValueError, "original gate failure"):
                try:
                    raise ValueError("original gate failure")
                finally:
                    driver.close()
            owned.terminate.assert_called_once()
            owned.kill.assert_called_once()
            self.assertEqual([call.kwargs["timeout"] for call in owned.wait.call_args_list], [30, 10])
            self.assertIsNone(driver.server)
            retained = json.loads((Path(directory) / "artifact-identity.json").read_text())
            self.assertEqual(retained["failure"], "original export handshake timeout")
            self.assertEqual(retained["cleanup"], [{"pid": 123, "action": "kill_after_stop_timeout"}])

    def test_unreaped_child_blocks_replacement_without_masking_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            driver = self.driver(directory)
            owned = driver.server
            owned.poll.return_value = None
            owned.wait.side_effect = subprocess.TimeoutExpired("owned", 30)
            driver.stop()
            self.assertIs(driver.server, owned)
            with patch("run_automatic.subprocess.Popen") as spawn:
                with self.assertRaisesRegex(RuntimeError, "unreaped"):
                    driver.start()
                spawn.assert_not_called()
            self.assertEqual(driver.identity["cleanup"][-1]["reap"], "timed_out")

    def test_nonzero_cli_preserves_check_false(self):
        with tempfile.TemporaryDirectory() as directory:
            driver = self.driver(directory)
            driver.server.poll.return_value = None
            client = Mock()
            client.communicate.return_value = ("rejected", "stale candidate")
            client.returncode = 1
            client.args = driver.base
            with patch("run_automatic.subprocess.Popen", return_value=client):
                result = driver.run("session", "compact", check=False)
            self.assertEqual(result.returncode, 1)
            self.assertEqual(result.stderr, "stale candidate")


if __name__ == "__main__":
    unittest.main()
