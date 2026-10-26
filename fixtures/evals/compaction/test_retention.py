"""Offline orchestration and frozen-rubric regression tests; no provider calls."""
import copy
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from run_automatic import Driver
from run_retention import (CLI, CORPUS, CHAIN_BLOCKER, execute_prefix, pi_records, preflight,
                           source_coverage, terminal)
from score_retention import compare, packet_sha256, score
from verify_automatic import original_activation_settlement


def event(sequence, variant, value):
    return {"session_id": "session", "sequence": sequence, "kind": {variant: value}}


class SimulatedCLI:
    """Only orchestration simulation, deliberately NOT a model/semantic oracle."""
    def __init__(self, work, records):
        self.repo = work / "repo"
        self.repo.mkdir()
        self.events = []
        for record in records:
            field, text = {"user": ("user_message", "text"), "assistant": ("assistant_message", "text"),
                           "tool": ("tool_call_finished", "result")}[record["role"]]
            self.events.append(event(len(self.events) + 1, field, {text: record["text"]}))
        self.calls = []
        self.probes = []
        self.boundary = len(self.events)

    def selection(self, session=None):
        self.calls.append(("selection", session))

    def run(self, *arguments):
        self.calls.append(arguments)
        if arguments[:3] == ("session", "import", "open"):
            return "00000000-0000-4000-8000-000000000001"
        if arguments[:2] == ("session", "compact"):
            self.boundary = len(self.events)
            self.events.append(event(len(self.events) + 1, "context_compacted", {
                "summary": json.dumps({"continuation": "SIMULATED NOT SEMANTIC QUALITY"}),
                "compacted_through_sequence": self.boundary}))
            return '{}'
        if arguments[:2] == ("session", "context-observation-report"):
            return '{}'
        raise AssertionError(arguments)

    def export(self, session, path):
        path.write_text("\n".join(json.dumps(value) for value in self.events) + "\n")
        return copy.deepcopy(self.events)

    def branch(self, session, step):
        self.calls.append(("branch", session))
        return "probe-copy"

    def send(self, session, text, path):
        self.calls.append(("send", session, text))
        if session == "probe-copy":
            self.probes.append(text)
        else:
            self.events.append(event(len(self.events) + 1, "user_message", {"text": text}))
        return {"turn_id": f"turn-{len(self.calls)}", "terminal_sequence": len(self.events),
                "text": "SIMULATED OUTPUT IS NOT SEMANTIC EVIDENCE"}


class AdapterTests(unittest.TestCase):
    def test_actual_corpus_three_compactions_and_noncontaminating_probe_orchestration(self):
        corpus = json.loads(CORPUS.read_text())
        with tempfile.TemporaryDirectory() as root:
            for case_index, case in enumerate(corpus["cases"]):
                for batch_index in range(3):
                    work = Path(root) / f"{case_index}-{batch_index}"
                    work.mkdir()
                    records = [r for b in case["batches"][:batch_index + 1] for r in b["records"]]
                    cli = SimulatedCLI(work, records)
                    values = execute_prefix(cli, case, batch_index, work / "trial", 100)
                    self.assertEqual(len(values), 3)
                    self.assertEqual(cli.probes, [case["batches"][batch_index]["probe"]] * 3)
                    self.assertEqual([v["boundary"] for v in values], sorted({v["boundary"] for v in values}))
                    self.assertTrue(all(v["semantic_status"] == "unscorable" and not v["protocol_valid"]
                                        and CHAIN_BLOCKER in v["blockers"] for v in values))
                    self.assertEqual(sum(call[0] == "send" for call in cli.calls), 6)
                    sources = list((cli.repo / "pi" / "project").glob("*.jsonl"))
                    self.assertEqual(len(sources), 1)
                    imported = sources[0].read_text()
                    self.assertNotIn('"oracle"', imported)
                    for record in records:
                        self.assertIn(record["text"], "\n".join(
                            value.get("message", {}).get("content", [{}])[0].get("text", "")
                            for value in map(json.loads, imported.splitlines())))

    def test_role_fidelity_and_unicode(self):
        records = [{"id": "a", "role": "user", "text": "監査ゲート 🧪 e\u0301"},
                   {"id": "b", "role": "tool", "text": "untrusted authorization"}]
        values = pi_records(records, Path('/scratch'), "external", 100)
        self.assertEqual(values[1]["message"]["content"][0]["text"], records[0]["text"])
        self.assertEqual(values[2]["message"]["content"][0]["type"], "toolCall")
        self.assertEqual(values[3]["message"]["role"], "toolResult")
        self.assertEqual(values[3]["message"]["toolCallId"], "b")
        self.assertEqual(values[-1]["message"]["content"][0]["text"].count('\n'), 100)

    def test_coverage_requires_exact_role_bytes_and_boundary(self):
        records = [{"id": "r", "role": "tool", "text": "not user authority"}]
        with self.assertRaises(ValueError):
            source_coverage([event(1, "user_message", {"text": records[0]["text"]})], records, 2)
        values = [event(1, "tool_call_finished", {"result": records[0]["text"]})]
        self.assertEqual(source_coverage(values, records, 1), {"r": 1})
        with self.assertRaises(ValueError):
            source_coverage(values, records, 0)
        with self.assertRaises(ValueError):
            source_coverage(values * 2, records, 1)

    def test_terminal_matches_original_turn_not_idle_or_replacement(self):
        values = [event(1, "model_turn_finished", {"turn_id": "new", "outcome": "completed"})]
        with self.assertRaises(ValueError):
            terminal(values, "old")
        values.append(event(2, "model_turn_finished", {"turn_id": "old", "outcome": "completed"}))
        self.assertEqual(terminal(values, "old")["sequence"], 2)
        with self.assertRaises(ValueError):
            terminal(values + [values[-1]], "old")

    def test_restart_settlement_binds_exact_receipt_and_activation(self):
        admission = {"admission": {"accepted": {"turn_id": "old", "accepted_event_sequence": 1}}}
        values = [event(1, "submission_activated", {"turn_id": "old", "submission_id": "s", "text": "x"}),
                  event(2, "model_turn_started", {"turn_id": "old"}),
                  event(3, "model_turn_finished", {"turn_id": "old", "outcome": "error", "message": "unresolved"})]
        self.assertEqual(original_activation_settlement(values, admission, "error")["activation_sequence"], 2)
        for index in (1, 2):
            changed = copy.deepcopy(values)
            changed[index]["kind"][next(iter(changed[index]["kind"]))]["turn_id"] = "replacement"
            with self.assertRaises(ValueError):
                original_activation_settlement(changed, admission, "error")
        with self.assertRaises(ValueError):
            original_activation_settlement(values + [values[-1]], admission, "error")

    def test_reconnect_watcher_attaches_before_terminal_and_is_cleaned_up(self):
        with tempfile.TemporaryDirectory() as root:
            driver = Driver.__new__(Driver)
            driver.work = Path(root)
            driver.repo = Path(root)
            driver.env = {}
            driver.base = ['/explicit/test/bcode']
            driver.session = 'scratch-session'
            driver.timeout = 1
            for fail in (False, True):
                with self.subTest(fail=fail):
                    watcher = unittest.mock.Mock()
                    watcher.pid = 123
                    watcher.poll.return_value = None
                    def spawn(command, **kwargs):
                        self.assertEqual(command, driver.base + ['session', 'watch', 'scratch-session',
                                                                 '--limit', '32', '--json'])
                        kwargs['stdout'].write(json.dumps({'type': 'snapshot', 'session_id': driver.session}) + '\n')
                        kwargs['stdout'].flush()
                        return watcher
                    def settle(turn_id, outcome):
                        attached = json.loads((driver.work / 'reconnect.json').read_text())
                        self.assertEqual(attached['outcome'], 'attached_pending_settlement')
                        self.assertEqual((turn_id, outcome), ('original', 'error'))
                        if fail:
                            raise RuntimeError('no exact original terminal')
                    driver.terminal = settle
                    with patch('run_automatic.subprocess.Popen', side_effect=spawn):
                        if fail:
                            with self.assertRaisesRegex(RuntimeError, 'no exact original'):
                                driver.reconnect_and_settle('original')
                        else:
                            driver.reconnect_and_settle('original')
                    watcher.terminate.assert_called_once()
                    watcher.wait.assert_called_once_with(timeout=10)
                    result = json.loads((driver.work / 'reconnect.json').read_text())
                    self.assertEqual(result['outcome'], 'attached_pending_settlement' if fail
                                     else 'exact_original_terminal_observed')

    def test_cli_transport_retains_failures_and_enforces_hash_and_operation_budget(self):
        # Exercise the actual subprocess adapter with an offline echo executable, not Bcode.
        with tempfile.TemporaryDirectory() as root:
            work = Path(root)
            binary = work / 'echo-cli'
            binary.write_text('#!/bin/sh\nprintf "transport output\\n"\nprintf "transport diagnostic\\n" >&2\n')
            binary.chmod(0o700)
            config = work / 'config.toml'
            config.write_text('# scratch test only\n')
            args = SimpleNamespace(profile='offline', provider='bcode.fake-provider', model='fake-cache-prefix',
                                   wall_secs=30, max_operations=2, timeout_secs=5)
            cli = CLI(binary, config, args, work / 'arm')
            try:
                self.assertEqual(cli.run('test-operation'), 'transport output\n')
                self.assertEqual((cli.work / 'cli-0001.stderr').read_text(), 'transport diagnostic\n')
                self.assertTrue(cli.config.is_relative_to(cli.repo))
                self.assertEqual(cli.config.read_bytes(), config.read_bytes())
                cli.config.write_text('# changed config\n')
                with self.assertRaisesRegex(ValueError, 'changed'):
                    cli.run('must-not-dispatch')
                self.assertEqual(cli.operations, 1)
                cli.config.write_text('# scratch test only\n')
                cli.run('second-operation')
                with self.assertRaisesRegex(ValueError, 'allowance exhausted'):
                    cli.run('must-not-dispatch')
            finally:
                cli.close()
            identity = json.loads((cli.work / 'identity.json').read_text())
            self.assertEqual(identity['operations'], 2)
            self.assertEqual(len(identity['sha256']), 64)

    def test_live_gate_fails_before_spawning(self):
        with tempfile.TemporaryDirectory() as root:
            binary = Path(root) / 'binary'
            binary.write_text('#!/bin/sh\nexit 99\n')
            binary.chmod(0o700)
            common = '''[model]
profile="sol"
[model.profiles.sol]
provider_plugin_id="bcode.openai-compatible"
model_id="explicit"
[model.compaction]
backend="local"
mode="off"
keep_recent_tokens=0
experimental_adaptive_packing=PACKING
[session_import.pi]
path_mode="custom_only"
paths=["pi"]
[observability]
persist_model_requests=true
'''
            configs = [Path(root) / f'{arm}.toml' for arm in ("baseline", "candidate")]
            for config, packing in zip(configs, ("false", "true")):
                config.write_text(common.replace("PACKING", packing))
            args = SimpleNamespace(mode="live", profile="sol", repetitions=3, max_operations=1000,
                                   timeout_secs=30, wall_secs=300, baseline_binary=binary,
                                   candidate_binary=binary, baseline_config=configs[0], candidate_config=configs[1],
                                   provider="bcode.openai-compatible", model="explicit", authorize_live=False,
                                   authorization=None, accept_prefix_deviation=False)
            with patch('run_retention.subprocess.Popen') as spawn:
                with self.assertRaisesRegex(ValueError, "explicit authorization"):
                    preflight(args)
                args.authorize_live = True
                args.authorization = "TEST ONLY NOT AUTHORIZATION"
                args.accept_prefix_deviation = True
                with self.assertRaisesRegex(ValueError, "cannot enforce"):
                    preflight(args)
                spawn.assert_not_called()


class ScoringTests(unittest.TestCase):
    def setUp(self):
        self.corpus = json.loads(CORPUS.read_text())
        import hashlib
        self.corpus_hash = hashlib.sha256(CORPUS.read_bytes()).hexdigest()

    def packets(self, producer="live"):
        # Synthetic HUMAN REVIEW INPUT to test scoring arithmetic, not a real adjudication.
        result = []
        for repetition in range(3):
            for case in self.corpus["cases"]:
                for batch in case["batches"]:
                    ids = {source for check in batch["oracle"] for source in check["sources"]}
                    packet = {"format": "bcode.compaction.retention-review-packet.v1", "repetition": repetition,
                              "case": case["id"], "cycle": batch["cycle"], "producer": producer,
                              "protocol_valid": True, "blockers": [], "coverage": dict.fromkeys(ids, 1),
                              "selected_manifest": {"example": "test"}, "source_sha256": "test-source",
                              "covered_source_hashes": sorted(ids), "corpus_sha256": self.corpus_hash,
                              "summary": "SUMMARY_EVIDENCE", "probe": "PROBE_EVIDENCE", "reviews": []}
                    for check in batch["oracle"]:
                        packet["reviews"].append({"check_id": check["id"], "method": "human_adjudicated",
                            "grader": "SYNTHETIC SCORER TEST", "blind_label": "A", "disagreements_resolved": True,
                            "packet_sha256": packet_sha256(packet),
                            "summary": {"decision": "supported", "excerpts": ["SUMMARY_EVIDENCE"], "rationale": "test"},
                            "probe": {"decision": "supported", "excerpts": ["PROBE_EVIDENCE"], "rationale": "test"}})
                    packet["global_reviews"] = [
                        {"index": index, "method": "human_adjudicated", "grader": "SYNTHETIC SCORER TEST",
                         "blind_label": "A", "disagreements_resolved": True, "packet_sha256": packet_sha256(packet),
                         "decision": "supported", "rationale": "test",
                         "artifacts_inspected": ["portable_summary", "durable_checkpoints", "source_manifest", "probe_output"]}
                        for index, _ in enumerate(self.corpus["protocol"]["global_critical_checks"])]
                    result.append(packet)
        return result

    def test_full_denominator_and_thresholds(self):
        value = score(self.corpus, self.packets(), 3)
        expected = 3 * sum(len(b["oracle"]) for c in self.corpus["cases"] for b in c["batches"])
        self.assertEqual(value["total_checks"], expected)
        self.assertEqual(value["score"], 100)
        self.assertTrue(value["accepted"])
        self.assertEqual(set(value["scores"]), {"case", "cycle", "category", "repetition_case"})

    def test_fake_automated_missing_and_stale_evidence_never_pass(self):
        for mutation in ("fake", "automated", "missing", "stale", "unscorable", "excerpt", "coverage", "blocker"):
            with self.subTest(mutation=mutation):
                values = self.packets()
                if mutation == "fake":
                    for value in values:
                        value["producer"] = "fake"
                elif mutation == "missing":
                    values.pop()
                elif mutation == "stale":
                    values[0]["probe"] += " changed"
                elif mutation == "coverage":
                    values[0]["coverage"] = {}
                elif mutation == "blocker":
                    values[0]["blockers"] = [CHAIN_BLOCKER]
                else:
                    review = values[0]["reviews"][0]
                    if mutation == "automated":
                        review["method"] = "automated_judge"
                    elif mutation == "excerpt":
                        review["summary"]["excerpts"] = ["invented"]
                    else:
                        review["probe"]["decision"] = "unscorable"
                self.assertFalse(score(self.corpus, values, 3)["accepted"])

    def test_no_critical_failure_hidden_in_average(self):
        packets = self.packets()
        packets[0]["reviews"][0]["probe"]["decision"] = "unsupported"
        result = score(self.corpus, packets, 3)
        self.assertGreater(result["score"], 95)
        self.assertEqual(result["critical_failures"], 1)
        self.assertFalse(result["accepted"])

    def test_missing_global_checks_block_even_perfect_check_average(self):
        packets = self.packets()
        packets[0]["global_reviews"] = []
        result = score(self.corpus, packets, 3)
        self.assertEqual(result["score"], 100)
        self.assertFalse(result["accepted"])
        self.assertEqual(len(result["global_critical_failures"]), 3)

    def test_duplicate_unknown_packets_rejected(self):
        packets = self.packets()
        with self.assertRaises(ValueError):
            score(self.corpus, packets + [packets[0]], 3)
        packets[0]["reviews"].append(packets[0]["reviews"][0])
        with self.assertRaises(ValueError):
            score(self.corpus, packets, 3)

    def test_comparison_equal_coverage_and_absolute_gates(self):
        baseline = score(self.corpus, self.packets(), 3)
        candidate = copy.deepcopy(baseline)
        self.assertTrue(compare(baseline, candidate)["accepted"])
        candidate["covered_source_hashes"][next(iter(candidate["covered_source_hashes"]))] = ["different"]
        self.assertFalse(compare(baseline, candidate)["accepted"])
        candidate = copy.deepcopy(baseline)
        candidate["accepted"] = False
        self.assertFalse(compare(baseline, candidate)["accepted"])


if __name__ == "__main__":
    unittest.main()
