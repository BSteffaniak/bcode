#!/usr/bin/env python3
"""Real proactive compaction interruption regression; fake provider only, never builds.

Uses the opt-in bcode.fake.compaction-control.v1 scratch protocol. Missing gates,
canonical terminal events, reuse or complete request captures are failures, not skips.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

from verify import load_events, markers
from verify_automatic import accepted_checkpoint, admitted_turn, attempts, verify_automatic


class Driver:
    def __init__(self, binary, interruption, timeout, adaptive_packing=False, source_repeat=1800):
        self.adaptive_packing = adaptive_packing
        self.source_repeat = source_repeat
        self.binary = binary
        self.timeout = timeout
        self.interruption = interruption
        self.work = Path(tempfile.mkdtemp(prefix="bcode-auto-compaction-", dir="/private/tmp")).resolve()
        self.repo = self.work / "repo"
        self.repo.mkdir()
        self.control = self.work / "control"
        self.control.mkdir()
        self.config = self.work / "offline.toml"
        self.server = None
        self.logs = []
        with binary.open("rb") as executable:
            binary_sha256 = hashlib.file_digest(executable, "sha256").hexdigest()
        self.identity = {"binary": str(binary), "sha256": binary_sha256,
                         "daemon_pids": [], "interruption": interruption, "result": "not_completed",
                         "adaptive_packing": adaptive_packing, "source_repeat": source_repeat}
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("BCODE_")}
        self.env["BCODE_CONFIG"] = str(self.config)
        self.env["BCODE_SOCKET"] = str(self.work / "daemon.sock")
        self.base = [str(binary), "--config-only", str(self.config), "--state-root", str(self.work / "state"),
                     "--profile", "offline", "--no-credential-discovery"]
        self.scenario = json.loads((Path(__file__).parent / "scenario.json").read_text())
        (self.repo / "false-authorization.txt").write_text(self.scenario["tool_files"]["false-authorization.txt"])
        self.configure("off")

    def configure(self, mode):
        packing_config = "experimental_adaptive_packing = true\n" if self.adaptive_packing else ""
        # Independent offline input, never a copy/overlay of the active configuration.
        self.config.write_text(f'''[plugins]
default = "none"
disabled = ["bcode.bedrock", "bcode.openai-compatible"]
enabled = ["bcode.fake-provider", "bcode.filesystem", "bcode.default-agents"]
[model]
profile = "offline"
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-cache-prefix"
[model.profiles.offline]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-cache-prefix"
[model.profiles.offline.settings]
fake_compaction_fixture = "scope-v1"
fake_compaction_control_dir = {json.dumps(str(self.control))}
fake_compaction_pause = "true"
fake_compaction_pause_timeout_ms = "{min(self.timeout * 1000, 300000)}"
[model.prompt_cache]
mode = "off"
[model.compaction]
mode = "{mode}"
backend = "local"
{packing_config}proactive_threshold_percent = 10
keep_recent_tokens = 512
[model.compaction.models."fake-cache-prefix"]
provider_plugin_id = "bcode.fake-provider"
mode = "{mode}"
proactive_threshold_tokens = 4000
[system_prompt]
mode = "replace"
text = "COMPACTION_REQUEST_ONLY_CANARY_7391"
[observability]
persist_model_requests = true
persist_tool_io = true
[skills]
enabled = false
''')

    def run(self, *args, check=True):
        result = subprocess.run(self.base + list(args), cwd=self.repo, env=self.env,
                                capture_output=True, text=True, timeout=self.timeout + 30)
        if check and result.returncode:
            raise RuntimeError(f"CLI {args[0]} failed ({result.returncode}): {result.stderr}\n{result.stdout}")
        return result

    def start(self):
        log = (self.work / f"daemon-{len(self.identity['daemon_pids'])}.log").open("w")
        self.logs.append(log)
        socket = Path(self.env["BCODE_SOCKET"])
        old_inode = socket.stat().st_ino if socket.exists() else None
        self.server = subprocess.Popen(self.base + ["server", "run"], cwd=self.repo, env=self.env,
                                       stdout=log, stderr=subprocess.STDOUT)
        self.identity["daemon_pids"].append(self.server.pid)
        self.until(lambda: socket.is_socket() and socket.stat().st_ino != old_inode, "new isolated daemon socket")
        server_status = self.run("server", "status", "--verbose").stdout
        self.save(f"server-{len(self.identity['daemon_pids'])}.txt", server_status)
        facts = dict(line.split(": ", 1) for line in server_status.splitlines() if ": " in line)
        if facts.get("executable identity") != self.identity["sha256"] or facts.get("pid") != str(self.server.pid):
            raise ValueError("isolated foreground daemon identity does not match launched executable/PID")
        # Readiness is not turn settlement. Also records exact offline selection via supported API.
        status = json.loads(self.run("model", "status", "--json").stdout)
        self.save("model-status.json", json.dumps(status, indent=2))
        if status.get("provider_plugin_id") != "bcode.fake-provider" or status.get("effective_model_id") != "fake-cache-prefix":
            raise ValueError("isolated daemon did not resolve the exact offline provider/model")

    def stop(self, crash=False):
        if self.server is not None and self.server.poll() is None:
            self.server.kill() if crash else self.server.terminate()
            self.server.wait(timeout=30)
        self.server = None

    def save(self, name, text):
        path = self.work / name
        path.write_text(text)
        return path

    def until(self, predicate, description):
        deadline = time.monotonic() + self.timeout
        while time.monotonic() < deadline:
            value = predicate()
            if value:
                return value
            if self.server is not None and self.server.poll() is not None:
                raise RuntimeError(f"isolated daemon exited while awaiting {description}")
            time.sleep(.05)
        raise RuntimeError(f"missing {description}; core/control may not implement resumable automatic compaction; inspect {self.work}")

    def export(self, name):
        return self.save(name, self.run("session", "export", self.session).stdout)

    def send(self, text):
        result = self.run("send", self.session, text,
                          "--wait", "--timeout-secs", str(self.timeout), check=True)
        with (self.work / "send-results.jsonl").open("a") as output:
            output.write(result.stdout + "\n")
        # This seed turn waits for its own authoritative terminal receipt.
        return result.stdout

    def gate(self, seen):
        def pending():
            values = [path for path in attempts(self.control) if str(path) not in seen]
            return values[0] if values else None
        return self.until(pending, "next synthesis gate (automatic threshold, no explicit compact)")

    def terminal(self, turn_id, outcome):
        def settled():
            events = load_events([self.export("current.jsonl")])
            return any(event["kind"].get("model_turn_finished", {}).get("turn_id") == turn_id
                       and event["kind"]["model_turn_finished"]["outcome"] == outcome for event in events)
        self.until(settled, f"canonical {outcome} for exact turn {turn_id}")

    def execute(self):
        self.start()
        self.session = self.run("session", "create", "automatic-compaction-regression").stdout.strip()
        self.identity["session_id"] = self.session
        # Seed long canonical records with policy OFF. Then reload through an isolated daemon
        # restart: the actual tested trigger is proactive request construction, never session compact.
        self.send(self.scenario["steps"][0]["prompt"])
        self.send(self.scenario["steps"][1]["prompt"])
        self.send(self.scenario["steps"][3]["prompt"])
        self.send("read-files false-authorization.txt")
        self.send("lossless source fragment\n" + "evidence 🧭 漢字 e\u0301\n" * self.source_repeat)
        self.send("TAIL_CANARY_7391 recent canonical tail")
        self.export("seed.jsonl")
        self.stop()
        self.configure("proactive")
        self.start()
        admission = json.loads(self.run("send", self.session, "Trigger automatic proactive compaction.", "--json").stdout)
        variant = admission.get("admission", {})
        receipt = next((variant[key] for key in ("accepted", "existing", "deferred") if key in variant), None)
        if receipt is None:
            raise ValueError(f"no admitted turn receipt: {admission}")
        turn_id = receipt["turn_id"]
        self.save("trigger-admission.json", json.dumps(admission, indent=2))
        seen = set()
        first = self.gate(seen)
        seen.add(str(first))
        (first / "release").touch()
        second = self.gate(seen)
        seen.add(str(second))
        # A later request demonstrates synthesis advancement, not by itself durable acceptance.
        before = self.export("before-interruption.jsonl")
        accepted_checkpoint(load_events([before]))
        if markers(load_events([before])):
            raise ValueError("final atomic marker was published before all synthesis gates completed")
        self.save("interruption.json", json.dumps({"first": first.name, "held": second.name,
                                                   "turn_id": turn_id, "mode": self.interruption}))
        if self.interruption == "cancel":
            self.save("cancel.json", self.run("cancel", self.session, "--turn-id", turn_id, "--json").stdout)
            self.terminal(turn_id, "cancelled")
        else:
            self.stop(crash=self.interruption == "crash")
            self.start()
            # Restart does not authorize replaying the interrupted turn. Explicitly settle
            # that exact turn through the supported cancellation boundary, retaining carry.
            self.save("post-restart-cancel.json", self.run("cancel", self.session,
                      "--turn-id", turn_id, "--json").stdout)
            self.terminal(turn_id, "cancelled")
        self.export("interrupted.jsonl")
        # Ordinary new turn admission exercises the real same-session automatic path.
        followup = self.run("send", self.session,
                            "Continue the requested work without changing the corrected scope.", "--json").stdout
        self.save("followup-admission.jsonl", followup)
        target_turn = admitted_turn(json.loads(followup))
        self.identity["continuation_turn_id"] = target_turn
        resumed = self.gate(seen)
        seen.add(str(resumed))
        self.save("resumed-attempt.json", json.dumps({"attempt": resumed.name}))
        # Fail early if accepted prefix is requested again; never release a known replay as success.
        from verify_automatic import coverage_keys, check_no_replay
        check_no_replay(coverage_keys(first), coverage_keys(resumed))
        (resumed / "release").touch()
        deadline = time.monotonic() + self.timeout
        while time.monotonic() < deadline:
            for attempt in attempts(self.control):
                if str(attempt) not in seen:
                    seen.add(str(attempt))
                    (attempt / "release").touch()
            final = self.export("final.jsonl")
            events = load_events([final])
            if markers(events) and any(event["kind"].get("model_turn_finished", {}).get("outcome") == "completed"
                                       and event["kind"]["model_turn_finished"]["turn_id"] == target_turn
                                       and event["sequence"] > markers(events)[-1][0]["sequence"] for event in events):
                break
            time.sleep(.1)
        else:
            raise ValueError("no final atomic compaction marker followed by canonical terminal continuation")
        self.save("context-report.json", self.run("session", "context-observation-report", self.session,
                                                   "--output-directory", str(self.work / "captures")).stdout)
        report = verify_automatic(self.work)
        self.save("verification.json", json.dumps(report, indent=2))
        self.identity["result"] = "passed"

    def close(self):
        self.stop()
        for log in self.logs:
            log.close()
        self.save("artifact-identity.json", json.dumps(self.identity, indent=2))
        print(f"Artifacts retained: {self.work}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--interruption", choices=("cancel", "restart", "crash"), default="cancel")
    parser.add_argument("--timeout-secs", type=int, default=60)
    parser.add_argument("--adaptive-packing", action="store_true", help="Opt into real experimental adaptive packing")
    parser.add_argument("--source-repeat", type=int, default=1800, help="Bounded canonical source size for paired packing trials")
    args = parser.parse_args()
    if not args.binary.is_absolute() or not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("--binary requires an explicit absolute executable path")
    if not 1 <= args.timeout_secs <= 300:
        parser.error("timeout must be 1..300 seconds")
    if not 1800 <= args.source_repeat <= 6000:
        parser.error("source repeat must be 1800..6000")
    driver = Driver(args.binary, args.interruption, args.timeout_secs, args.adaptive_packing, args.source_repeat)
    try:
        driver.execute()
    except (ValueError, RuntimeError, OSError, subprocess.SubprocessError) as error:
        driver.identity["result"] = "failed"
        driver.identity["failure"] = str(error)
        if hasattr(driver, "session") and driver.server is not None and driver.server.poll() is None:
            try:
                driver.export("failure-history.jsonl")
            except (RuntimeError, OSError, subprocess.SubprocessError) as export_error:
                driver.identity["failure_export"] = str(export_error)
        raise SystemExit(f"FAIL: {error}") from error
    finally:
        driver.close()


if __name__ == "__main__":
    main()
