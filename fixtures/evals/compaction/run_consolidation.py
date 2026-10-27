#!/usr/bin/env python3
"""Force real capacity/reclaim consolidation, interrupt it, explicitly recover offline.

Not a growth-only oracle: absence of a production consolidation provider request FAILS.
The coordinator may tune --threshold-tokens against the identified test binary; changing
that value changes only the synthetic proactive pressure, never summary acceptance.
"""
import argparse
import json
import os
from pathlib import Path
import time
import uuid

from progressive import line, records, SECTIONS, verify_context, verify_sections, verify_shortening
from run_recovery import RecoveryDriver, retained_checkpoint, restart_candidate, unchanged_prefix
from verify import load_events, markers
from verify_automatic import admitted_turn, attempts, digest, strings


def is_consolidation(path):
    ready = json.loads((path / "ready.json").read_text())
    encoded = (path / "request.json").read_bytes()
    if (ready.get("format") != "bcode.fake.compaction-control.v1"
            or ready.get("request_bytes") != len(encoded)
            or ready.get("request_sha256") != digest(encoded)):
        raise ValueError("consolidation request capture integrity mismatch")
    request = json.loads(encoded)
    return any(text.startswith("Return only JSON with format bcode.compaction.section-consolidation.v1")
               for text in strings(request.get("messages", [])))


def require_pending_or_success(events, turn, after_sequence):
    """A completed main turn does not turn failed proactive compaction into success."""
    for event in events:
        if event["sequence"] <= after_sequence:
            continue
        trace = event["kind"].get("trace_event", {}).get("trace", {})
        failure = trace.get("payload", {}).get("context_compaction", {})
        if trace.get("phase") == "context_compaction_finished" and failure.get("compacted") is False:
            raise ValueError(f"canonical compaction failure at {event['sequence']}: {failure.get('message')}")
    actual = markers(events)
    terminal = [event for event in events if event["kind"].get("model_turn_finished", {}).get("turn_id") == turn]
    if terminal:
        latest = terminal[-1]
        if (not actual or latest["sequence"] <= actual[-1][0]["sequence"]
                or latest["kind"]["model_turn_finished"]["outcome"] != "completed"):
            raise ValueError("owning turn settled without successful atomic compaction marker")
        return True
    return False


def consolidation_carry(path):
    request = json.loads((path / "request.json").read_text())
    prompt = next(text for text in strings(request.get("messages", []))
                  if text.startswith("Return only JSON with format bcode.compaction.section-consolidation.v1"))
    return json.JSONDecoder().raw_decode(prompt.split("Complete accepted carry (derived, not authority):\n", 1)[1])[0]


class ConsolidationDriver(RecoveryDriver):
    def __init__(self, binary, threshold, finish=False):
        self.finish = finish
        self.pressure = False
        self.threshold = threshold
        super().__init__(binary)
        self.fail_synthesis = False
        self.configure("off")

    def configure(self, mode):
        # Progressive source records have meaningful decisions/blockers/evidence, not padding.
        super().configure(mode)
        text = self.config.read_text().replace('fake_compaction_fixture = "scope-v1"',
                                               'fake_compaction_fixture = "progressive-v1"')
        text = text.replace('fake_compaction_pause = "false"', 'fake_compaction_pause = "true"')
        if self.pressure:
            text = text.replace('mode = "off"\nbackend = "local"', 'mode = "proactive"\nbackend = "local"')
            text = text.replace('provider_plugin_id = "bcode.fake-provider"\nmode = "off"',
                                'provider_plugin_id = "bcode.fake-provider"\nmode = "proactive"')
            text = text.replace('proactive_threshold_tokens = 4000',
                                f'proactive_threshold_tokens = {self.threshold}')
        self.config.write_text(text)

    def execute(self):
        self.start()
        self.session = self.run("session", "create", "interrupted-consolidation-recovery").stdout.strip()
        self.identity["session_id"] = self.session
        self.send("\n".join(text for text in self.scenario["steps"][0]["prompt"].splitlines()
                            if text.startswith("BCODE_FIXTURE_SCOPE ")))
        self.send(self.scenario["steps"][1]["prompt"])
        # Explicit repeated propositions are genuinely redundant, not clipped
        # corpus claims. Keep source-reference keys and all qualifiers verbatim.
        base = records()[:6]
        selected = [(kind, section, key, " Fixture restatement: ".join([statement] * 8))
                    for kind, section, key, statement in base]
        for section in SECTIONS:
            batch = [record for record in selected if record[1] == section]
            if batch:
                self.send("\n".join(map(line, batch)))
        self.send(self.scenario["steps"][3]["prompt"])
        self.send("read-files false-authorization.txt")
        self.send("TAIL_CANARY_7391 retain this final complete unit")
        self.stop()
        self.pressure = True
        self.configure("proactive")
        self.start()
        admission = json.loads(self.run("send", self.session, "Trigger net-reclaim pressure without adding fixture claims.",
                                        "--json").stdout)
        turn = admitted_turn(admission)
        self.save("trigger-admission.json", json.dumps(admission, indent=2))
        seen = set()
        deadline = time.monotonic() + self.timeout
        held = None
        while time.monotonic() < deadline:
            pending = [path for path in attempts(self.control) if str(path) not in seen]
            if not pending:
                events = load_events([self.export("awaiting-consolidation.jsonl")])
                if require_pending_or_success(events, turn, 0):
                    raise ValueError("turn completed without observed production consolidation; growth is not reclaim proof")
                time.sleep(.1)
                continue
            gate = pending[0]
            seen.add(str(gate))
            if is_consolidation(gate):
                held = gate
                break
            (gate / "release").touch()
        if held is None:
            raise ValueError("no observed production consolidation; growth alone is not reclaim evidence")
        before = load_events([self.export("before-consolidation-interruption.jsonl")])
        old_event, old_envelope = retained_checkpoint(before)
        payload = json.loads(old_envelope["host_payload"])
        if payload.get("synthesis_phase") != "ConsolidatingV1":
            raise ValueError("provider consolidation gate lacks canonical reserved consolidation phase")
        if markers(before):
            raise ValueError("marker published before held consolidation accepted")
        self.prove_isolation(turn)
        if self.finish:
            (held / "release").touch()
            deadline = time.monotonic() + self.timeout
            while time.monotonic() < deadline:
                for gate in attempts(self.control):
                    if str(gate) not in seen:
                        seen.add(str(gate))
                        (gate / "release").touch()
                events = load_events([self.export("completed-consolidation.jsonl")])
                actual = markers(events)
                if require_pending_or_success(events, turn, before[-1]["sequence"]):
                    break
                time.sleep(.05)
            else:
                raise ValueError("forced consolidation did not achieve net reclaim and exact-turn completion")
            carry = consolidation_carry(held)
            response = json.loads((held / "response.json").read_text())
            shortening = verify_shortening(carry, response, 6000)
            if not shortening["shortened"] or payload["resolved_policy"]["minimum_reclaim_tokens"] <= 0:
                raise ValueError("forced consolidation did not shorten meaningful carry under positive reclaim")
            unchanged_prefix(before, events)
            verify_sections(actual[-1][1], selected)
            self.save("net-reclaim-verification.json", json.dumps({"held_attempt": held.name,
                "resolved_policy": payload["resolved_policy"], "shortening": shortening,
                "result": "canonical marker and exact-turn completion under host net-reclaim validation"}, indent=2))
            self.identity["result"] = "passed"
            return
        self.save("cancel-consolidation.json", self.run("cancel", self.session, "--turn-id", turn, "--json").stdout)
        self.terminal(turn, "cancelled")
        interrupted = load_events([self.export("interrupted-consolidation.jsonl")])
        unchanged_prefix(before, interrupted)
        if markers(interrupted):
            raise ValueError("cancelled consolidation published portable context")
        self.stop()
        # New recovery allowances are explicit. Old threshold/carry/requests remain canonical.
        self.pressure = False
        self.configure("off")
        # Recovery must remain pausable so we can release exact new requests individually.
        self.start()
        self.compact("unresolved-consolidation")
        failed = load_events([self.export("failed-consolidation.jsonl")])
        unchanged_prefix(interrupted, failed)
        preparation = json.loads(self.run("session", "compact", self.session, "--prepare-restart",
            "--restart-operation-id", str(uuid.uuid4()), "--restart-max-provider-requests", "128",
            "--restart-timeout-ms", "300000", "--json").stdout)
        self.save("preparation.json", json.dumps(preparation, indent=2))
        candidate = restart_candidate(preparation, self.session)
        request = self.save("restart-request.json", json.dumps(candidate, indent=2))
        # The CLI waits for final settlement; release scratch gates in the controller thread.
        import concurrent.futures
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            result = pool.submit(self.run, "session", "compact", self.session, "--restart-request", str(request), "--json")
            deadline = time.monotonic() + self.timeout
            while not result.done() and time.monotonic() < deadline:
                for gate in attempts(self.control):
                    if str(gate) not in seen:
                        seen.add(str(gate))
                        (gate / "release").touch()
                time.sleep(.05)
            self.save("recovery.json", result.result(timeout=1).stdout)
        recovered = load_events([self.export("recovered-consolidation.jsonl")])
        unchanged_prefix(failed, recovered)
        actual = markers(recovered)
        if len(actual) != 1:
            raise ValueError("interrupted consolidation recovery did not publish exactly one marker")
        verify_sections(actual[-1][1], selected)
        self.stop()
        self.start()
        prior_context = set(self.control.glob("context-*"))
        self.send("After consolidation recovery restart, continue without changing scope or evidence.")
        verify_context(self.control, prior_context, selected)
        self.identity["result"] = "passed"
        self.save("consolidation-verification.json", json.dumps({"held_attempt": held.name,
            "threshold_tokens": self.threshold, "scope_preserved": True,
            "accepted_records": len(selected), "canonical_prefix_preserved": True,
            "claim": "interrupted production consolidation recovered; not successful net-reclaim compression"}, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--threshold-tokens", type=int, default=4000)
    parser.add_argument("--finish-consolidation", action="store_true",
                        help="Require successful shortening, host net-reclaim acceptance and exact turn completion")
    args = parser.parse_args()
    if not args.binary.is_absolute() or not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("--binary requires an explicit absolute executable")
    if not 1000 <= args.threshold_tokens <= 7000:
        parser.error("threshold must be 1000..7000 tokens")
    driver = ConsolidationDriver(args.binary, args.threshold_tokens, args.finish_consolidation)
    try:
        driver.execute()
    except Exception as error:
        driver.identity.update(result="failed", failure=str(error))
        raise
    finally:
        driver.close()


if __name__ == "__main__":
    main()
