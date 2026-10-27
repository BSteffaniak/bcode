#!/usr/bin/env python3
"""Offline progressive section regression. Uses only explicit isolated test binary/state.

Run by the coordinator after plugin selector/context-capture wiring. Never builds, invokes
live providers, or touches an existing session. Existing automatic interruption regressions
remain responsible for durable owner and unresolved-request outcome checks.
"""
import argparse
import json
import os
from pathlib import Path

from progressive import phases, line, verify_sections, verify_context, verify_attempts
from run_automatic import Driver
from verify import load_events, markers, verify_fixture_scope


class ProgressiveDriver(Driver):
    def configure(self, mode):
        super().configure("off")
        text = self.config.read_text().replace('fake_compaction_fixture = "scope-v1"',
                                               'fake_compaction_fixture = "progressive-v1"')
        text = text.replace('fake_compaction_pause = "true"', 'fake_compaction_pause = "false"')
        self.config.write_text(text)

    def execute(self):
        self.start()
        self.session = self.run("session", "create", "progressive-section-regression").stdout.strip()
        self.identity["session_id"] = self.session
        self.send("\n".join(line for line in self.scenario["steps"][0]["prompt"].splitlines()
                            if line.startswith("BCODE_FIXTURE_SCOPE ")))
        self.send(self.scenario["steps"][1]["prompt"])
        selected = []
        for cycle, batch in enumerate(phases(), 1):
            if cycle == 2:
                self.send(self.scenario["steps"][3]["prompt"])
                self.send("read-files false-authorization.txt")
            selected.extend(batch)
            self.send("\n".join(line(record) for record in batch))
            self.send("Recent tail, not a source of progressive fixture evidence.")
            self.send("Second recent exchange protects the tail without shielding selected evidence.")
            try:
                result = self.run("session", "compact", self.session, "--json")
            except RuntimeError:
                self.export(f"failure-cycle-{cycle}.jsonl")
                raise
            self.save(f"compact-{cycle}.json", result.stdout)
            events = load_events([self.export(f"cycle-{cycle}.jsonl")])
            accepted = markers(events)
            if not accepted:
                raise ValueError("no atomic compaction publication")
            verify_sections(accepted[-1][1], selected)
            marker = accepted[-1][0]["kind"]["context_compacted"]
            verify_fixture_scope(events, marker["compacted_through_sequence"], accepted[-1][1])
            # This must be actual post-marker input, never a probe populated from the oracle.
            before = set(self.control.glob("context-*"))
            self.send(f"POST_MARKER_CONTEXT_{cycle}: continue without changing any fixture evidence")
            verify_context(self.control, before, selected)
            self.stop(crash=cycle == 3)
            self.start()
            before = set(self.control.glob("context-*"))
            self.send(f"POST_RESTART_CONTEXT_{cycle}: continue without changing any fixture evidence")
            verify_context(self.control, before, selected)
            restarted = load_events([self.export(f"restarted-{cycle}.jsonl")])
            verify_sections(markers(restarted)[-1][1], selected)
        self.send("Repeated-compaction boundary advancement; no additional fixture claims.")
        self.send("Recent tail remains outside the selected prefix.")
        self.save("compact-4.json", self.run("session", "compact", self.session, "--json").stdout)
        repeated = load_events([self.export("repeated.jsonl")])
        verify_sections(markers(repeated)[-1][1], selected)
        self.save("progressive-verification.json", json.dumps(verify_attempts(self.control), indent=2))
        self.identity["result"] = "passed"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    if not args.binary.is_absolute() or not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("--binary requires an explicit absolute executable")
    driver = ProgressiveDriver(args.binary, "restart", 300)
    try:
        driver.execute()
    except (ValueError, RuntimeError, OSError) as error:
        driver.identity["result"] = "failed"
        driver.identity["failure"] = str(error)
        raise
    finally:
        driver.close()


if __name__ == "__main__":
    main()
