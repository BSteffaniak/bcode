#!/usr/bin/env python3
"""Offline bounded overload/opt-out/restart regression, using only isolated supported CLI APIs."""
import argparse
import json
import os
from pathlib import Path
import uuid

from run_automatic import Driver
from verify import load_events, markers
from verify_automatic import attempts
from run_recovery import retained_checkpoint, unchanged_prefix


class RetryDriver(Driver):
    def __init__(self, binary, case):
        self.case = case
        self.outcome = "overload"
        super().__init__(binary, "cancel", 60, source_repeat=1800)

    def configure(self, mode):
        super().configure(mode)
        text = self.config.read_text().replace('fake_compaction_pause = "true"',
                                               'fake_compaction_pause = "false"')
        text = text.replace('fake_compaction_control_dir = ',
                            f'fake_compaction_outcome = "{self.outcome}"\nfake_compaction_control_dir = ')
        text += '\n[model.retry]\noverload_initial_delay_ms = 1\noverload_max_delay_ms = 1\n'
        if self.case == "disabled":
            text += 'enabled = false\n'
        elif self.case == "finite":
            text += 'overload_retry_forever = false\nmax_overload_retries = 2\n'
        self.config.write_text(text)

    def execute(self):
        self.start()
        self.session = self.run("session", "create", "bounded-compaction-retry").stdout.strip()
        self.identity["session_id"] = self.session
        self.send("Preserve the user restriction: do not modify files.")
        self.send("historical facts\n" + "evidence\n" * 80)
        self.send("protected current tail")
        result = self.run("session", "compact", self.session, "--json", check=False)
        self.save("first-compaction.json", result.stdout)
        if result.returncode == 0:
            raise ValueError("persistent overload unexpectedly compacted")
        old = load_events([self.export("failed.jsonl")])
        event, envelope = retained_checkpoint(old)
        checkpoint = json.loads(envelope["host_payload"])
        expected = {"disabled": 0, "finite": 2, "bounded": 8}[self.case]
        if checkpoint["request_state"]["extra_requests"] != expected:
            raise ValueError("wrong cumulative retry charge")
        count = len(attempts(self.control))
        if count != expected + 1 or markers(old):
            raise ValueError("wrong dispatch count or failed operation published")
        self.run("session", "compact", self.session, "--json", check=False)
        if len(attempts(self.control)) != count:
            raise ValueError("ordinary request replayed exhausted work")
        self.stop()
        self.outcome = "success"
        self.configure("off")
        self.start()
        self.run("session", "compact", self.session, "--json", check=False)
        if len(attempts(self.control)) != count:
            raise ValueError("daemon replacement renewed retry allowance")
        preparation = json.loads(self.run("session", "compact", self.session, "--prepare-restart",
            "--restart-operation-id", str(uuid.uuid4()), "--restart-max-provider-requests", "64",
            "--restart-timeout-ms", "300000", "--json").stdout)
        self.save("preparation.json", json.dumps(preparation, indent=2))
        if preparation.get("blocker") or not preparation.get("restart"):
            raise ValueError(f"settled provider recovery blocked: {preparation.get('blocker')}")
        request = self.save("request.json", json.dumps(preparation["restart"]))
        self.run("session", "compact", self.session, "--restart-request", str(request), "--json")
        final = load_events([self.export("recovered.jsonl")])
        unchanged_prefix(old, final)
        if len(markers(final)) != 1:
            raise ValueError("explicit settled restart did not publish")
        self.identity.update(result="passed", case=self.case, provider_attempts=count)
        print(f"PASS: {self.case} retry policy; retained charges, no repeated dispatch, settled explicit recovery")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--case", choices=("disabled", "finite", "bounded"), required=True)
    args = parser.parse_args()
    if not args.binary.is_absolute() or not os.access(args.binary, os.X_OK):
        parser.error("--binary requires an explicit absolute executable")
    driver = RetryDriver(args.binary, args.case)
    try:
        driver.execute()
    finally:
        driver.close()
