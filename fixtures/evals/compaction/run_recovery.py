#!/usr/bin/env python3
"""Offline bounded recovery gate; coordinator supplies an identified fake-provider binary.

Only supported CLI operations touch newly created private scratch sessions. Never edits
storage, injects checkpoints, builds Rust, or uses an existing daemon/session.
"""
import argparse
import json
import os
from pathlib import Path
import uuid

from run_automatic import Driver
from verify import load_events, markers, verify_scope
from verify_automatic import attempts, exported_requests, strings


def retained_checkpoint(events):
    """Latest unretired canonical envelope, including reserved but unaccepted work."""
    latest = {}
    for event in events:
        provider = event["kind"].get("trace_event", {}).get("trace", {}).get("payload", {}).get("provider_event", {})
        if provider.get("event_type") != "compaction_synthesis_checkpoint_v1":
            continue
        envelope = json.loads(provider["detail"])
        if envelope.get("version") != 1:
            raise ValueError("unsupported canonical checkpoint envelope")
        latest[envelope["operation_key"]] = (event, envelope)
    live = [(event, envelope) for event, envelope in latest.values() if envelope["host_payload"] is not None]
    if len(live) != 1:
        raise ValueError("expected one exact retained failed-operation checkpoint")
    return live[0]


def policy_failure(events, after_sequence):
    """Canonical failed compaction evidence, not the deliberately generic UI diagnostic."""
    known = ("compaction packing policy changed; retained checkpoint requires original policy",
             "compaction synthesis policy changed or is incompatible; explicit supersession required; boundary unchanged")
    failures = []
    for event in events:
        trace = event["kind"].get("trace_event", {}).get("trace", {})
        payload = trace.get("payload", {}).get("context_compaction", {})
        if (event["sequence"] > after_sequence and trace.get("phase") == "context_compaction_finished"
                and payload.get("compacted") is False
                and any(message in (payload.get("message") or "") for message in known)):
            failures.append(event)
    if len(failures) != 1:
        raise ValueError("policy mismatch trial lacks unique new canonical policy-failure terminal trace")
    return failures[0]


def unchanged_prefix(before, after):
    indexed = {event["sequence"]: event for event in after}
    if any(indexed.get(event["sequence"]) != event for event in before):
        raise ValueError("canonical history, old carry or consumed allowances changed")


def restart_candidate(preparation, session):
    if preparation.get("version") != 1 or preparation.get("session_id") != session:
        raise ValueError("recovery preparation belongs to another contract/session")
    candidate = preparation.get("restart")
    if preparation.get("blocker") is not None or not isinstance(candidate, dict):
        raise ValueError(f"recovery preparation failed closed: {preparation.get('blocker')}")
    if candidate.get("session_id") != session or candidate.get("version") != 1:
        raise ValueError("candidate identity mismatch")
    return candidate


class RecoveryDriver(Driver):
    def __init__(self, binary, mismatch=False):
        self.mismatch = mismatch
        self.fail_synthesis = True
        super().__init__(binary, "restart", 300)

    def configure(self, mode):
        super().configure("off")
        text = self.config.read_text().replace('fake_compaction_pause = "true"', 'fake_compaction_pause = "false"')
        text = text.replace('fake_compaction_fixture = "scope-v1"',
                            'fake_compaction_fixture = "scope-v1"\nfake_compaction_outcome = "' +
                            ("malformed" if self.fail_synthesis else "success") + '"')
        if self.mismatch and not self.fail_synthesis:
            # A real supported policy switch changes chunk identity; never fabricate
            # an upgrade checkpoint by mutating canonical storage.
            text = text.replace('backend = "local"',
                                'backend = "local"\nexperimental_adaptive_packing = true')
        self.config.write_text(text)

    def compact(self, name, candidate=None, check=False):
        args = ["session", "compact", self.session, "--json"]
        if candidate is not None:
            request = self.save(name + "-request.json", json.dumps(candidate, indent=2))
            args += ["--restart-request", str(request)]
        result = self.run(*args, check=check)
        self.save(name + ".json", result.stdout)
        self.save(name + ".stderr", result.stderr)
        return result

    def execute(self):
        self.start()
        self.session = self.run("session", "create", "explicit-compaction-recovery").stdout.strip()
        self.identity["session_id"] = self.session
        self.send(self.scenario["steps"][0]["prompt"])
        self.send(self.scenario["steps"][1]["prompt"])
        self.send(self.scenario["steps"][3]["prompt"])
        self.send("read-files false-authorization.txt")
        # This trial needs a genuine failed checkpoint and policy switch, not
        # multi-chunk stress. Keep adaptive original+repair requests feasible;
        # automatic/consolidation drivers own the large-corpus coverage gates.
        source_repeat = 80 if self.mismatch else 1800
        self.identity["recovery_source_repeat"] = source_repeat
        self.send("lossless source fragment\n" + "evidence 🧭 漢字 e\u0301\n" * source_repeat)
        self.send("TAIL_CANARY_7391 retained canonical tail")
        self.compact("failed-operation")
        old = load_events([self.export("failed-history.jsonl")])
        old_event, old_envelope = retained_checkpoint(old)
        if markers(old):
            raise ValueError("failed operation published a marker")
        # Configuration change is deliberate, fake-only and loaded on owned restart.
        self.stop()
        self.fail_synthesis = False
        self.configure("off")
        self.start()
        if self.mismatch:
            # A normal compaction request must replace a settled incompatible operation
            # internally. No preparation JSON or explicit restart is part of this path.
            self.compact("automatic-policy-recovery", check=True)
            operation = "automatic"
            candidate = None
        else:
            operation = str(uuid.uuid4())
            before_prepare = set(attempts(self.control))
            result = self.run("session", "compact", self.session, "--prepare-restart",
                              "--restart-operation-id", operation, "--restart-max-provider-requests", "128",
                              "--restart-timeout-ms", "300000", "--json")
            preparation = json.loads(result.stdout)
            self.save("preparation.json", json.dumps(preparation, indent=2))
            candidate = restart_candidate(preparation, self.session)
            if candidate["expected_checkpoint_sequence"] != old_event["sequence"]:
                raise ValueError("preparation silently selected another checkpoint")
            observed = load_events([self.export("after-prepare.jsonl")])
            if observed != old or set(attempts(self.control)) != before_prepare:
                raise ValueError("read-only recovery preparation mutated history or dispatched")
            # Wrong exact identity cannot borrow the candidate's settled-owner authority.
            stale = dict(candidate, operation_id=str(uuid.uuid4()),
                         expected_checkpoint_sequence=candidate["expected_checkpoint_sequence"] + 1)
            self.compact("stale-candidate", stale)
            after_stale = load_events([self.export("after-stale.jsonl")])
            if markers(after_stale) or set(attempts(self.control)) != before_prepare:
                raise ValueError("stale recovery candidate dispatched or published")
            unchanged_prefix(old, after_stale)
            self.compact("recovery", candidate, check=True)
        recovered = load_events([self.export("recovered.jsonl")])
        unchanged_prefix(old, recovered)
        selected = markers(recovered)
        if len(selected) != 1:
            raise ValueError("explicit recovery did not publish exactly one atomic marker")
        verify_scope(recovered, selected)
        recovered_boundary = selected[0][0]["kind"]["context_compacted"]["compacted_through_sequence"]
        if recovered_boundary != json.loads(old_envelope["host_payload"])["boundary"]:
            raise ValueError("recovery changed the frozen failed-operation source boundary")
        count = set(attempts(self.control))
        if candidate is not None:
            # Same identity may return its prior receipt, but must never repeat provider work.
            self.compact("duplicate-candidate", candidate)
            if set(attempts(self.control)) != count:
                raise ValueError("duplicate recovery candidate redispatched provider work")
            distinct = dict(candidate, operation_id=str(uuid.uuid4()))
            self.compact("superseded-candidate", distinct)
            final = load_events([self.export("final.jsonl")])
            if set(attempts(self.control)) != count or len(markers(final)) != 1:
                raise ValueError("superseded recovery candidate dispatched or republished")
            unchanged_prefix(recovered, final)
        else:
            final = recovered
        self.stop()
        self.start()
        restarted = load_events([self.export("restarted.jsonl")])
        unchanged_prefix(final, restarted)
        if len(markers(restarted)) != 1:
            raise ValueError("recovered marker did not survive owned daemon restart")
        post_marker_sequence = restarted[-1]["sequence"]
        self.send("Continue using recovered canonical context without changing scope.")
        self.save("context-report.json", self.run("session", "context-observation-report", self.session,
                  "--output-directory", str(self.work / "captures")).stdout)
        requests = [(sequence, request) for sequence, request in exported_requests(self.work, self.session)
                    if sequence > post_marker_sequence]
        summary = selected[0][0]["kind"]["context_compacted"]["summary"]
        if not requests or not any(summary in "\n".join(strings(request.get("messages", [])))
                                   for _, request in requests):
            raise ValueError("actual post-restart continuation request did not consume recovered marker")
        self.identity["result"] = "passed"
        self.save("recovery-verification.json", json.dumps({"policy_mismatch": self.mismatch,
                  "old_checkpoint_sequence": old_event["sequence"], "operation_id": operation,
                  "provider_attempts": len(count), "canonical_prefix_preserved": True}, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--policy-mismatch", action="store_true")
    args = parser.parse_args()
    if not args.binary.is_absolute() or not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("--binary requires an explicit absolute executable")
    driver = RecoveryDriver(args.binary, args.policy_mismatch)
    try:
        driver.execute()
    except Exception as error:
        driver.identity.update(result="failed", failure=str(error))
        raise
    finally:
        driver.close()


if __name__ == "__main__":
    main()
