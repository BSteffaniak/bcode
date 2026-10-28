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
    def __init__(self, binary, interruption, timeout, adaptive_packing=False, source_repeat=18000,
                 legacy_restart_cancel=False):
        self.legacy_restart_cancel = legacy_restart_cancel
        self.adaptive_packing = adaptive_packing
        self.source_repeat = source_repeat
        self.binary = binary
        self.timeout = timeout
        self.interruption = interruption
        self.work = Path(tempfile.mkdtemp(prefix="bcode-auto-compaction-")).resolve()
        self.repo = self.work / "repo"
        self.repo.mkdir()
        self.control = self.work / "control"
        self.control.mkdir()
        self.config = self.repo / "offline.toml"
        self.server = None
        self.logs = []
        with binary.open("rb") as executable:
            binary_sha256 = hashlib.file_digest(executable, "sha256").hexdigest()
        self.identity = {"binary": str(binary), "sha256": binary_sha256,
                         "daemon_pids": [], "interruption": interruption, "result": "not_completed",
                         "adaptive_packing": adaptive_packing, "source_repeat": source_repeat,
                         "legacy_restart_cancel": legacy_restart_cancel}
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
        # The CLI may autostart a daemon. Once this driver owns one, a dead
        # foreground process is failure, never permission to spawn a replacement.
        if self.server is not None and self.server.poll() is not None:
            raise RuntimeError(f"owned isolated daemon exited before CLI {args[0]}")
        client = subprocess.Popen(self.base + list(args), cwd=self.repo, env=self.env,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        deadline = time.monotonic() + self.timeout + 30
        try:
            while True:
                try:
                    stdout, stderr = client.communicate(timeout=.1)
                    break
                except subprocess.TimeoutExpired:
                    if self.server is not None and self.server.poll() is not None:
                        raise RuntimeError(f"owned isolated daemon exited during CLI {args[0]}")
                    if time.monotonic() >= deadline:
                        raise RuntimeError(f"CLI {args[0]} exceeded bounded driver deadline")
        except BaseException:
            client.kill()
            stdout, stderr = client.communicate()
            self.save("failed-cli.json", json.dumps({"args": list(args), "stdout": stdout,
                                                    "stderr": stderr, "returncode": client.returncode}))
            raise
        result = subprocess.CompletedProcess(client.args, client.returncode, stdout, stderr)
        if check and result.returncode:
            raise RuntimeError(f"CLI {args[0]} failed ({result.returncode}): {result.stderr}\n{result.stdout}")
        return result

    def start(self):
        if self.server is not None:
            raise RuntimeError("cannot start another daemon while an owned child remains unreaped")
        log = (self.work / f"daemon-{len(self.identity['daemon_pids'])}.log").open("w")
        self.logs.append(log)
        socket = Path(self.env["BCODE_SOCKET"])
        self.server = subprocess.Popen(self.base + ["server", "run"], cwd=self.repo, env=self.env,
                                       stdout=log, stderr=subprocess.STDOUT)
        self.identity["daemon_pids"].append(self.server.pid)
        def owns_socket():
            if not socket.is_socket():
                return False
            try:
                import socket as socket_api
                import struct
                with socket_api.socket(socket_api.AF_UNIX, socket_api.SOCK_STREAM) as probe:
                    probe.settimeout(.2)
                    probe.connect(str(socket))
                    if hasattr(socket_api, "SO_PEERCRED"):
                        peer, _, _ = struct.unpack("3i", probe.getsockopt(socket_api.SOL_SOCKET, socket_api.SO_PEERCRED, struct.calcsize("3i")))
                    else:  # macOS LOCAL_PEERPID on a connected Unix socket.
                        peer, = struct.unpack("i", probe.getsockopt(0, 2, struct.calcsize("i")))
                return peer == self.server.pid
            except OSError:
                return False
        self.until(owns_socket, "owned isolated daemon socket")
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
        server = self.server
        if server is not None and server.poll() is None:
            server.kill() if crash else server.terminate()
            try:
                server.wait(timeout=30)
            except subprocess.TimeoutExpired:
                # Cleanup must not mask the original gate/transport failure. Only
                # the exact Popen-owned isolated child is eligible for escalation.
                self.identity.setdefault("cleanup", []).append({"pid": server.pid,
                    "action": "kill_after_stop_timeout"})
                server.kill()
                try:
                    server.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    self.identity["cleanup"][-1]["reap"] = "timed_out"
                    return  # retain the handle; never start another owned daemon
        self.server = None

    def save(self, name, text):
        path = self.work / name
        path.write_text(text)
        return path

    def until(self, predicate, description):
        deadline = time.monotonic() + self.timeout
        while time.monotonic() < deadline:
            if self.server is not None and self.server.poll() is not None:
                raise RuntimeError(f"isolated daemon exited while awaiting {description}")
            value = predicate()
            if value:
                return value
            time.sleep(.05)
        raise RuntimeError(f"missing {description}; core/control may not implement resumable automatic compaction; inspect {self.work}")

    def export(self, name):
        return self.save(name, self.run("session", "export", self.session).stdout)

    def send(self, text):
        prompt = self.save("seed-prompt.txt", text)
        result = self.run("send", self.session, "--file", str(prompt),
                          "--wait", "--timeout-secs", str(self.timeout), check=True)
        with (self.work / "send-results.jsonl").open("a") as output:
            output.write(result.stdout + "\n")
        # This seed turn waits for its own authoritative terminal receipt.
        return result.stdout

    def prove_isolation(self, blocked_turn):
        """A held provider request must not monopolize another session's actor/turn."""
        unrelated = self.run("session", "create", "compaction-isolation-canary").stdout.strip()
        admission = json.loads(self.run("send", unrelated, "UNRELATED_SESSION_CANARY_7391", "--json").stdout)
        target = admitted_turn(admission)
        def completed():
            events = load_events([self.save("unrelated.jsonl", self.run("session", "export", unrelated).stdout)])
            return any(event["kind"].get("model_turn_finished", {}).get("turn_id") == target
                       and event["kind"]["model_turn_finished"]["outcome"] == "completed" for event in events)
        self.until(completed, "unrelated exact-turn completion while synthesis is held")
        blocked = load_events([self.export("isolation-blocked.jsonl")])
        if any(event["kind"].get("model_turn_finished", {}).get("turn_id") == blocked_turn for event in blocked):
            raise ValueError("blocked compaction settled before isolation was demonstrated")
        self.save("isolation.json", json.dumps({"session_id": unrelated, "turn_id": target,
                    "blocked_session_id": self.session, "blocked_turn_id": blocked_turn,
                    "result": "completed_while_compaction_held"}, indent=2))

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

    def reconnect_and_settle(self, turn_id):
        """Normal bounded attachment triggers recovery; history reads stay non-mutating."""
        stdout = (self.work / "reconnect-watch.jsonl").open("w")
        stderr = (self.work / "reconnect-watch.stderr").open("w")
        watcher = subprocess.Popen(self.base + ["session", "watch", self.session,
                                   "--limit", "32", "--json"], cwd=self.repo, env=self.env,
                                   stdout=stdout, stderr=stderr)
        try:
            # Wait for a real initial snapshot so a slow/failed attach cannot be
            # confused with reconnect recovery. Only the watcher owns attachment;
            # subsequent exports remain passive observations.
            def attached():
                if watcher.poll() is not None:
                    raise ValueError("normal reconnect watcher exited before initial snapshot")
                path = self.work / "reconnect-watch.jsonl"
                with path.open() as stream:
                    line = stream.readline(4 * 1024 * 1024)
                if not line.endswith("\n"):
                    return False
                value = json.loads(line)
                if value.get("type") != "snapshot" or value.get("session_id") != self.session:
                    raise ValueError("normal reconnect did not yield exact-session snapshot")
                return True
            self.until(attached, "normal watcher initial snapshot")
            self.save("reconnect.json", json.dumps({"format": "bcode.compaction.reconnect.v1",
                       "operation": "session watch", "initial_event_limit": 32,
                       "pid": watcher.pid, "turn_id": turn_id,
                       "outcome": "attached_pending_settlement"}, indent=2))
            self.terminal(turn_id, "error")
            if watcher.poll() is not None:
                raise ValueError("normal reconnect watcher exited before settlement observation")
            self.save("reconnect.json", json.dumps({"format": "bcode.compaction.reconnect.v1",
                       "operation": "session watch", "initial_event_limit": 32,
                       "pid": watcher.pid, "turn_id": turn_id,
                       "outcome": "exact_original_terminal_observed"}, indent=2))
        finally:
            if watcher.poll() is None:
                watcher.terminate()
                try:
                    watcher.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    watcher.kill()
                    watcher.wait(timeout=10)
            stdout.close()
            stderr.close()

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
        self.prove_isolation(turn_id)
        self.save("interruption.json", json.dumps({"first": first.name, "held": second.name,
                                                   "turn_id": turn_id, "mode": self.interruption}))
        if self.interruption == "cancel":
            self.save("cancel.json", self.run("cancel", self.session, "--turn-id", turn_id, "--json").stdout)
            self.terminal(turn_id, "cancelled")
        else:
            self.stop(crash=self.interruption == "crash")
            self.start()
            if self.legacy_restart_cancel:
                self.save("post-restart-cancel.json", self.run("cancel", self.session,
                          "--turn-id", turn_id, "--json").stdout)
                self.terminal(turn_id, "cancelled")
            else:
                # Readiness is not settlement: require the original activation's terminal
                # outcome without a cancellation request or replacement admission.
                self.reconnect_and_settle(turn_id)
                settled = load_events([self.export("restart-settlement-history.jsonl")])
                from verify_automatic import original_activation_settlement
                evidence = original_activation_settlement(settled, admission, "error")
                if evidence["message"] != "interrupted compaction provider outcome is unresolved; no request replayed":
                    raise ValueError("restart error did not explicitly preserve unresolved provider outcome")
                self.save("original-activation-settlement.json", json.dumps(evidence, indent=2))
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
        self.save("artifact-identity.json", json.dumps(self.identity, indent=2))
        report = verify_automatic(self.work)
        self.save("verification.json", json.dumps(report, indent=2))
        self.identity["result"] = "legacy_workaround_only" if self.legacy_restart_cancel else "passed"

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
    parser.add_argument("--legacy-restart-cancel", action="store_true",
                        help="Historical workaround only; does NOT prove automatic recovery")
    parser.add_argument("--timeout-secs", type=int, default=60)
    parser.add_argument("--adaptive-packing", action="store_true", help="Opt into real experimental adaptive packing")
    parser.add_argument("--source-repeat", type=int, default=18000, help="Bounded source size; default exceeds one capacity-first synthesis request")
    args = parser.parse_args()
    if not args.binary.is_absolute() or not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("--binary requires an explicit absolute executable path")
    if not 1 <= args.timeout_secs <= 300:
        parser.error("timeout must be 1..300 seconds")
    if not 1800 <= args.source_repeat <= 24000:
        parser.error("source repeat must be 1800..24000")
    if args.legacy_restart_cancel and args.interruption == "cancel":
        parser.error("legacy restart cancellation applies only to restart/crash")
    driver = Driver(args.binary, args.interruption, args.timeout_secs, args.adaptive_packing,
                    args.source_repeat, args.legacy_restart_cancel)
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
