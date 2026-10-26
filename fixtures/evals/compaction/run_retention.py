#!/usr/bin/env python3
"""Corpus CLI executor. Prefix trials are diagnostic, NOT frozen-protocol quality passes.

No build, active-session access, database access, or implicit live-provider selection.
The CLI currently cannot append attributed assistant/tool corpus batches to an existing
session. Independent prefixes preserve roles, but do not implement the corpus's summary
carry chain. This blocker is retained in every observation and cannot be graded away.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import tomllib
import uuid

from verify import load_events, markers
from verify_automatic import admitted_turn

CORPUS = Path(__file__).with_name("retention_cases.json")
FORMAT = "bcode.compaction.retention-run.v1"
CHAIN_BLOCKER = "unsupported_cli_attributed_batch_append: independent prefixes are not summary carry"


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write_json(path, value):
    Path(path).write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def terminal(events, turn_id):
    """Require one authoritative exact-turn terminal; idle and text are not receipts."""
    values = [event for event in events
              if event["kind"].get("model_turn_finished", {}).get("turn_id") == turn_id]
    if len(values) != 1 or values[0]["kind"]["model_turn_finished"]["outcome"] != "completed":
        raise ValueError("exact admitted turn lacks unique completed terminal")
    return values[0]


def pi_records(records, cwd, external_id, padding_lines):
    """Use supported Pi import, retaining canonical roles and Unicode byte content."""
    result = [{"type": "session", "version": 3, "id": external_id, "cwd": str(cwd),
               "timestamp": "2026-01-01T00:00:00.000Z"}]
    for record in records:
        role = record["role"]
        if role not in ("user", "assistant", "tool"):
            raise ValueError("unsupported corpus role")
        if role == "tool":
            result.append({"type": "message", "id": record["id"] + "-call", "message": {
                "role": "assistant", "content": [{"type": "toolCall", "id": record["id"],
                "name": "corpus_observation", "arguments": {}}]}})
        message = {"role": "toolResult" if role == "tool" else role,
                   "content": [{"type": "text", "text": record["text"]}]}
        if role == "tool":
            message.update(toolCallId=record["id"], toolName="corpus_observation", isError=False)
        result.append({"type": "message", "id": record["id"], "message": message})
    if padding_lines:
        padding = {"id": "neutral-padding", "role": "tool",
                   "text": "neutral calibration sample 0123456789\n" * padding_lines}
        result.extend(pi_records([padding], cwd, external_id, 0)[1:])
    return result


def source_coverage(events, records, boundary):
    """Exact canonical delivery/coverage check; this is NOT a semantic judgment."""
    fields = {"user": ("user_message", "text"), "assistant": ("assistant_message", "text"),
              "tool": ("tool_call_finished", "result")}
    coverage = {}
    for record in records:
        variant, field = fields[record["role"]]
        found = [event["sequence"] for event in events
                 if event["kind"].get(variant, {}).get(field) == record["text"]]
        if len(found) != 1 or found[0] > boundary:
            raise ValueError(f"invalid canonical coverage for {record['id']}")
        coverage[record["id"]] = found[0]
    return coverage


class CLI:
    """One identified binary/config and foreground daemon, in private scratch only."""
    def __init__(self, binary, config, args, work):
        self.binary, self.args, self.work = binary, args, work
        self.repo = work / "repo"
        self.repo.mkdir(parents=True)
        # Offline inputs are deliberate synthetic configs, copied byte-for-byte into
        # the session's authorized root so recovery can verify their real provenance.
        self.config = self.repo / "offline.toml"
        self.config.write_bytes(config.read_bytes())
        config = self.config
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("BCODE_")}
        self.env.update(BCODE_CONFIG=str(config), BCODE_SOCKET=str(work / "daemon.sock"))
        self.base = [str(binary), "--config-only", str(config), "--state-root", str(work / "state"),
                     "--profile", args.profile, "--no-credential-discovery", "--disable-all-tools"]
        self.started = time.monotonic()
        self.operations = 0
        self.process = None
        self.log = None
        self.identity = {"binary": str(binary), "sha256": digest(binary),
                         "config": str(config), "config_sha256": digest(config), "profile": args.profile,
                         "provider": args.provider, "model": args.model, "operations": 0}

    def run(self, *arguments):
        if digest(self.binary) != self.identity["sha256"] or digest(self.config) != self.identity["config_sha256"]:
            raise ValueError("binary/config changed after preflight")
        remaining = self.args.wall_secs - (time.monotonic() - self.started)
        if self.operations >= self.args.max_operations or remaining <= 0:
            raise ValueError("explicit operation/wall allowance exhausted")
        self.operations += 1
        result = subprocess.run(self.base + list(arguments), cwd=self.repo, env=self.env,
                                text=True, capture_output=True,
                                timeout=min(self.args.timeout_secs, remaining))
        # Record outputs, including failures, without copying config/authentication contents.
        prefix = self.work / f"cli-{self.operations:04}"
        prefix.with_suffix(".stdout").write_text(result.stdout)
        prefix.with_suffix(".stderr").write_text(result.stderr)
        write_json(prefix.with_suffix(".json"), {"arguments": arguments, "exit_code": result.returncode})
        if result.returncode:
            raise RuntimeError(f"CLI failed: {arguments[0]}; inspect {prefix}")
        return result.stdout

    def start(self):
        self.log = (self.work / "daemon.log").open("w")
        self.process = subprocess.Popen(self.base + ["server", "run"], cwd=self.repo, env=self.env,
                                        stdout=self.log, stderr=subprocess.STDOUT)
        deadline = time.monotonic() + self.args.timeout_secs
        while not Path(self.env["BCODE_SOCKET"]).is_socket():
            if self.process.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError("scratch daemon failed readiness")
            time.sleep(.05)
        facts = dict(line.split(": ", 1) for line in self.run("server", "status", "--verbose").splitlines()
                     if ": " in line)
        if facts.get("pid") != str(self.process.pid) or facts.get("executable identity") != self.identity["sha256"]:
            raise ValueError("scratch daemon PID/artifact mismatch")
        self.identity["pid"] = self.process.pid
        self.selection()

    def selection(self, session=None):
        status = json.loads(self.run("model", "status", *([session] if session else []), "--json"))
        if (status.get("provider_plugin_id") != self.args.provider
                or status.get("effective_model_id") != self.args.model):
            raise ValueError("effective provider/model differ from authorized selection")
        # SessionModelStatus has no profile-name field. Exact profile provenance is
        # pinned by preflight plus immutable config hash, not guessed from model aliases.
        write_json(self.work / f"selection-{session or 'draft'}.json", status)

    def export(self, session, path):
        path.write_text(self.run("session", "export", session))
        return load_events([path])

    def send(self, session, text, path):
        source = self.work / f"prompt-{self.operations + 1}.txt"
        source.write_text(text)
        receipt = json.loads(self.run("send", session, "--file", str(source), "--json"))
        write_json(path.with_suffix(".admission.json"), receipt)
        turn = admitted_turn(receipt)
        deadline = time.monotonic() + self.args.timeout_secs
        while time.monotonic() < deadline:
            events = self.export(session, path)
            if any(event["kind"].get("model_turn_finished", {}).get("turn_id") == turn for event in events):
                finished = terminal(events, turn)
                messages = [event["kind"]["assistant_message"]["text"] for event in events
                            if "assistant_message" in event["kind"] and
                            event["sequence"] < finished["sequence"] and
                            event["sequence"] > max((e["sequence"] for e in events if
                                e["kind"].get("model_turn_started", {}).get("turn_id") == turn), default=10**30)]
                if not messages:
                    raise ValueError("exact completed activation has no downstream assistant output")
                return {"turn_id": turn, "terminal_sequence": finished["sequence"], "text": "\n".join(messages)}
            time.sleep(.2)
        raise ValueError("exact activation did not settle within allowance")

    def branch(self, session, directory):
        snapshot = json.loads(self.run("session", "derivation-snapshot", session))
        operation = str(uuid.uuid4())
        request = {"version": 1, "operation_id": operation, "idempotency_key": operation,
                   "source": snapshot, "source_policy": "exact_generation",
                   "cutoff_sequence": snapshot["latest_sequence"],
                   "destination_working_directory": str(self.repo),
                   "lineage": {"producer": "bcode.compaction.retention-eval", "operation_kind": "probe-copy"}}
        path = directory / "branch-request.json"
        write_json(path, request)
        result = json.loads(self.run("session", "derive", "--request", str(path)))
        deadline = time.monotonic() + self.args.timeout_secs
        while result.get("outcome") is None and time.monotonic() < deadline:
            time.sleep(.1)
            result = json.loads(self.run("session", "derivation-status", operation))
        write_json(directory / "branch-result.json", result)
        if result.get("outcome", {}).get("status") != "succeeded":
            raise ValueError("probe copy did not authoritatively succeed")
        return result["outcome"]["session"]["id"]

    def close(self):
        if self.process is not None and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=10)
        if self.log:
            self.log.close()
        self.identity.update(operations=self.operations, elapsed_seconds=time.monotonic() - self.started)
        write_json(self.work / "identity.json", self.identity)


def execute_prefix(cli, case, batch_index, directory, padding_lines):
    """Execute actual corpus prefix, advancing real compactions and independent probes.

    Diagnostic deviation is explicit: later prefixes are fresh imports, not retained carry.
    Later compactions within each trial DO carry the actual previous portable summary.
    """
    directory.mkdir()
    records = [record for batch in case["batches"][:batch_index + 1] for record in batch["records"]]
    external = str(uuid.uuid4())
    imports = cli.repo / "pi" / "project"
    imports.mkdir(parents=True, exist_ok=True)
    source = imports / f"{external}.jsonl"
    source.write_text("\n".join(json.dumps(value, ensure_ascii=False) for value in
                                 pi_records(records, cli.repo, external, padding_lines)) + "\n")
    session = cli.run("session", "import", "open", "--source", "pi", external).strip()
    uuid.UUID(session)  # Import warnings or partial import must never become an authority locator.
    cli.selection(session)
    observations = []
    previous_boundary = -1
    for cycle in range(1, 4):
        step = directory / f"compaction-{cycle}"
        step.mkdir()
        before = cli.export(session, step / "before.jsonl")
        write_json(step / "compact-result.json", json.loads(cli.run("session", "compact", session, "--json")))
        after = cli.export(session, step / "after.jsonl")
        added = [(event, summary) for event, summary in markers(after)
                 if event["sequence"] > before[-1]["sequence"]]
        if len(added) != 1:
            raise ValueError("need one new real compaction marker")
        event, summary = added[0]
        boundary = event["kind"]["context_compacted"]["compacted_through_sequence"]
        if boundary <= previous_boundary:
            raise ValueError("real compaction boundary did not advance")
        previous_boundary = boundary
        coverage = source_coverage(after, records, boundary)
        write_json(step / "portable-summary.json", summary)
        branch = cli.branch(session, step)
        cli.selection(branch)
        response = cli.send(branch, case["batches"][batch_index]["probe"], step / "probe.jsonl")
        unchanged = cli.export(session, step / "unchanged.jsonl")
        if unchanged != after:
            raise ValueError("probe contaminated source session")
        downstream = cli.send(session, "Acknowledge receipt without performing any action.", step / "downstream.jsonl")
        captures = step / "captures"
        (step / "context-report.json").write_text(cli.run("session", "context-observation-report", session,
                                                          "--output-directory", str(captures)))
        observation = {"format": "bcode.compaction.retention-observation.v1", "case": case["id"],
                       "corpus_cycle": case["batches"][batch_index]["cycle"], "compaction": cycle,
                       "session_id": session, "probe_session_id": branch,
                       "marker_sequence": event["sequence"], "boundary": boundary, "coverage": coverage,
                       "summary": summary, "probe": response, "downstream": downstream,
                       "source_sha256": digest(source), "protocol_valid": False,
                       "blockers": [CHAIN_BLOCKER, "selected_manifest_requires_manual_capture_review",
                                    "request_only_context_not_injected"],
                       "semantic_status": "unscorable", "claim": "orchestration_only"}
        write_json(step / "observation.json", observation)
        observations.append(observation)
    return observations


def preflight(args):
    if args.profile != "sol" and args.mode == "live":
        raise ValueError("live evaluation is authorized only for explicit sol profile")
    if not 3 <= args.repetitions <= 10 or not 1 <= args.max_operations <= 100000:
        raise ValueError("require 3..10 paired repetitions and explicit bounded operations")
    if not 1 <= args.timeout_secs <= 300 or not args.timeout_secs <= args.wall_secs <= 86400:
        raise ValueError("invalid timeout/wall budget")
    configs = []
    for binary, config in [(args.baseline_binary, args.baseline_config), (args.candidate_binary, args.candidate_config)]:
        if not binary.is_absolute() or not binary.is_file() or not os.access(binary, os.X_OK):
            raise ValueError("explicit absolute executable required for each arm")
        if not config.is_absolute() or not config.is_file():
            raise ValueError("explicit existing absolute configuration required for each arm")
        document = tomllib.loads(config.read_text())
        model = document.get("model", {})
        selection = model.get("profiles", {}).get(args.profile, {})
        if (model.get("profile") != args.profile or selection.get("provider_plugin_id") != args.provider
                or selection.get("model_id") != args.model):
            raise ValueError("explicit profile configuration must pin exact authorized provider/model")
        compaction = document.get("model", {}).get("compaction", {})
        if compaction.get("backend") != "local" or compaction.get("mode") != "off" or compaction.get("keep_recent_tokens") != 0:
            raise ValueError("prefix trials require local/off/keep_recent_tokens=0; never truncate corpus")
        if compaction.get("models"):
            raise ValueError("model-specific compaction overrides are unsupported in paired trials")
        pi = document.get("session_import", {}).get("pi", {})
        # Scratch root is deliberately deterministic relative to each arm's cwd.
        if pi.get("path_mode") != "custom_only" or pi.get("paths") != ["pi"]:
            raise ValueError('require session_import.pi path_mode="custom_only", paths=["pi"]')
        if not document.get("observability", {}).get("persist_model_requests"):
            raise ValueError("complete model request captures required")
        configs.append(document)
    if [c["model"]["compaction"].get("experimental_adaptive_packing", False) for c in configs] != [False, True]:
        raise ValueError("baseline packing must be false and candidate true")
    configs[1]["model"]["compaction"]["experimental_adaptive_packing"] = False
    configs[0]["model"]["compaction"]["experimental_adaptive_packing"] = False
    if configs[0] != configs[1]:
        raise ValueError("paired configurations must differ only in adaptive packing")
    if args.mode == "offline":
        if args.provider != "bcode.fake-provider" or args.model != "fake-cache-prefix":
            raise ValueError("offline mode only permits exact fake provider/model")
        plugins = configs[0].get("plugins", {})
        if (plugins.get("default") != "none"
                or not set(plugins.get("enabled", [])) <= {"bcode.fake-provider", "bcode.default-agents", "bcode.pi-session-import"}
                or not {"bcode.openai-compatible", "bcode.bedrock"} <= set(plugins.get("disabled", []))):
            raise ValueError("offline configuration must disable live provider discovery")
    elif not args.authorize_live or not args.authorization or not args.accept_prefix_deviation:
        raise ValueError("live requires explicit authorization record, --authorize-live and --accept-prefix-deviation")
    if args.mode == "live":
        raise ValueError("live blocked: CLI operation/wall allowances cannot enforce provider request/token/spend caps; "
                         "no live execution is permitted by this adapter version")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for arm in ("baseline", "candidate"):
        parser.add_argument(f"--{arm}-binary", type=Path, required=True)
        parser.add_argument(f"--{arm}-config", type=Path, required=True)
    parser.add_argument("--mode", choices=("offline", "live"), default="offline")
    for name in ("profile", "provider", "model"):
        parser.add_argument(f"--{name}", required=True)
    parser.add_argument("--authorize-live", action="store_true")
    parser.add_argument("--authorization", help="Non-secret approval reference; never credentials")
    parser.add_argument("--accept-prefix-deviation", action="store_true")
    parser.add_argument("--repetitions", type=int, required=True)
    parser.add_argument("--max-operations", type=int, required=True)
    parser.add_argument("--wall-secs", type=int, required=True)
    parser.add_argument("--timeout-secs", type=int, default=120)
    parser.add_argument("--padding-lines", type=int, default=1800)
    args = parser.parse_args()
    try:
        preflight(args)
        if not 100 <= args.padding_lines <= 6000:
            raise ValueError("padding must be 100..6000 lines")
    except (ValueError, OSError) as error:
        parser.error(str(error))
    work = Path(tempfile.mkdtemp(prefix="bcode-retention-", dir="/private/tmp")).resolve()
    corpus = json.loads(CORPUS.read_text())
    report = {"format": FORMAT, "corpus_sha256": digest(CORPUS), "mode": args.mode,
              "authorization": args.authorization, "repetitions": args.repetitions,
              "padding": {"recipe": "neutral calibration sample 0123456789\\n", "lines": args.padding_lines},
              "budgets": {"operations_per_arm": args.max_operations, "wall_seconds_per_arm": args.wall_secs,
                          "warning": "CLI operations/time are not hard provider token, request or monetary caps"},
              "claim": "orchestration_only", "semantic_status": "unscorable", "blockers": [CHAIN_BLOCKER],
              "arms": {}, "result": "not_completed"}
    try:
        for label in ("baseline", "candidate"):
            cli = CLI(getattr(args, label + "_binary"), getattr(args, label + "_config"), args, work / label)
            report["arms"][label] = []
            try:
                cli.start()
                for repetition in range(args.repetitions):
                    for case in corpus["cases"]:
                        for index, _ in enumerate(case["batches"]):
                            directory = cli.work / f"{repetition}-{case['id']}-{index + 1}"
                            report["arms"][label].extend(execute_prefix(cli, case, index, directory, args.padding_lines))
            finally:
                cli.close()
        report["result"] = "diagnostic_execution_completed_not_quality_pass"
    except (ValueError, RuntimeError, OSError, subprocess.SubprocessError) as error:
        report.update(result="failed", failure=str(error))
        raise
    finally:
        write_json(work / "run.json", report)
        print(f"Artifacts retained: {work}")


if __name__ == "__main__":
    main()
