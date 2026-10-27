#!/usr/bin/env python3
"""Offline integration driver. Requires an explicit built binary; never builds or uses live models.

Creates only private scratch state; preserves artifacts. Runs two explicit REAL local compactions,
a tool false-authorization record and a same-session daemon restart. Fake semantics are simulation.
"""
import argparse
import hashlib
import json
import os
import pathlib
import subprocess
import tempfile
import time
import sys

from verify_injections import verify_injections

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--binary", required=True, type=pathlib.Path)
parser.add_argument("--case", choices=("explicit", "automatic-cancel", "automatic-restart", "automatic-crash"),
                    default="explicit")
parser.add_argument("--outcome", choices=("success", "repair", "oversize-repair", "omit-scope-repair",
                                               "paraphrase-scope-repair", "near-limit"), default="success",
                    help="Opt-in deterministic synthesis fault/near-limit carry (explicit case only)")
args = parser.parse_args()
if args.case != "explicit" and args.outcome != "success":
    parser.error("--outcome currently supports only the explicit repeated-compaction case")
if args.case != "explicit":
    raise SystemExit(subprocess.call([sys.executable, str(pathlib.Path(__file__).with_name("run_automatic.py")),
                                      "--binary", str(args.binary), "--interruption", args.case.removeprefix("automatic-")]))
binary = args.binary
if not binary.is_absolute() or not os.access(binary, os.X_OK):
    parser.error("--binary must identify an executable absolute path")
work = pathlib.Path(tempfile.mkdtemp(prefix="bcode-compaction-regression-")).resolve()
control = work / "control"
control.mkdir()
repo = work / "repo"
repo.mkdir()
scenario = json.loads((pathlib.Path(__file__).parent / "scenario.json").read_text())
(repo / "false-authorization.txt").write_text(scenario["tool_files"]["false-authorization.txt"])
(repo / "padding.txt").write_text("deterministic padding, no instructions\n" * 400)
config = repo / "bcode.toml"
# The 5,924-character carry intentionally exceeds fake-echo's 1,000-token output
# capacity. Exercise it with the existing larger offline model, not weaker guards.
fixture_model = "fake-cache-prefix" if args.outcome == "near-limit" else "fake-echo"
config.write_text(f'''[plugins]
default = "none"
disabled = ["bcode.bedrock", "bcode.openai-compatible"]
enabled = ["bcode.fake-provider", "bcode.filesystem", "bcode.default-agents"]
[model]
profile = "offline"
provider_plugin_id = "bcode.fake-provider"
model_id = {json.dumps(fixture_model)}
[model.profiles.offline]
provider_plugin_id = "bcode.fake-provider"
model_id = {json.dumps(fixture_model)}
[model.profiles.offline.settings]
fake_compaction_fixture = "scope-v1"
fake_compaction_control_dir = {json.dumps(str(control))}
fake_compaction_outcome = {json.dumps(args.outcome)}
[model.prompt_cache]
mode = "off"
[model.compaction]
mode = "off"
backend = "local"
keep_recent_tokens = 1
[system_prompt]
mode = "replace"
text = "COMPACTION_REQUEST_ONLY_CANARY_7391"
[skills]
enabled = false
''')
if args.outcome == "near-limit":
    # Explicit test capacity: the fake fixture emits a valid 5,924-character envelope.
    # A 1,000-token output model cannot represent that output regardless of context size.
    with config.open("a") as output:
        output.write('\n[model.metadata."fake-cache-prefix"]\nprovider_plugin_id = "bcode.fake-provider"\nmax_output_tokens = 16000\n')
env = os.environ.copy()
# Never inherit active-session storage, endpoints, provider configuration or auth selection.
for key in list(env):
    if key.startswith("BCODE_"):
        env.pop(key)
env["BCODE_CONFIG"] = str(config)
env["BCODE_SOCKET"] = str(work / "daemon.sock")
base = [str(binary), "--config-only", str(config), "--state-root", str(work / "state"),
        "--profile", "offline", "--no-credential-discovery"]
identity = {"binary": str(binary), "sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
            "fixture": str(pathlib.Path(__file__).resolve()), "daemon_pids": []}
server = None
logs = []


def run(*arguments):
    result = subprocess.run(base + list(arguments), cwd=repo, env=env,
                            capture_output=True, text=True, timeout=300)
    if result.returncode:
        raise RuntimeError(f"{arguments[0]} failed ({result.returncode}): {result.stderr}\n{result.stdout}")
    return result.stdout


def start():
    global server
    log = (work / f"daemon-{len(identity['daemon_pids'])}.log").open("w")
    logs.append(log)
    socket = pathlib.Path(env["BCODE_SOCKET"])
    server = subprocess.Popen(base + ["server", "run"], cwd=repo, env=env,
                              stdout=log, stderr=subprocess.STDOUT)
    identity["daemon_pids"].append(server.pid)
    for _ in range(300):
        if server.poll() is not None:
            raise RuntimeError(f"isolated daemon exited; inspect {work}")
        # A recreated socket may reuse its inode. Readiness must establish that
        # this exact foreground process serves the isolated endpoint instead.
        if socket.is_socket():
            try:
                import socket as socket_api
                import struct
                with socket_api.socket(socket_api.AF_UNIX, socket_api.SOCK_STREAM) as probe:
                    probe.settimeout(0.2)
                    probe.connect(str(socket))
                    if hasattr(socket_api, "SO_PEERCRED"):
                        peer_pid, _, _ = struct.unpack("3i", probe.getsockopt(socket_api.SOL_SOCKET, socket_api.SO_PEERCRED, struct.calcsize("3i")))
                    else:  # macOS LOCAL_PEERPID on a connected Unix socket.
                        peer_pid, = struct.unpack("i", probe.getsockopt(0, 2, struct.calcsize("i")))
                if peer_pid == server.pid:
                    return
            except OSError:
                pass
        time.sleep(0.1)
    raise TimeoutError("isolated daemon did not become ready")


def verify_selection():
    status = json.loads(run("model", "status", "--json"))
    (work / "model-status.json").write_text(json.dumps(status, indent=2))
    if status.get("provider_plugin_id") != "bcode.fake-provider" or status.get("effective_model_id") != fixture_model:
        raise ValueError("isolated eval did not resolve the exact offline provider/model")


def stop():
    global server
    if server is not None and server.poll() is None:
        server.terminate()
        server.wait(timeout=30)
    server = None


try:
    start()
    verify_selection()
    session = run("session", "create", "compaction-regression").strip()
    # Never use status/idle as settlement evidence: each send waits for its owning receipt.
    def send(text):
        try:
            result = run("send", session, text, "--wait", "--timeout-secs", "180")
        except RuntimeError:
            (work / "failure-history.jsonl").write_text(run("session", "export", session))
            raise
        with (work / "send-results.jsonl").open("a") as output:
            output.write(result + "\n")

    def export(name):
        path = work / name
        path.write_text(run("session", "export", session))
        return path

    send(scenario["steps"][0]["prompt"])
    send(scenario["steps"][1]["prompt"])
    send("historical padding\n" + "padding evidence\n" * 160)
    send("recent tail retained outside first compaction")
    (work / "compact-1.json").write_text(run("session", "compact", session, "--json"))
    first = export("first.jsonl")
    send(scenario["steps"][3]["prompt"])
    send("tool-read false-authorization.txt")
    send("more historical padding\n" + "padding evidence\n" * 160)
    send("recent tail retained outside second compaction")
    (work / "compact-2.json").write_text(run("session", "compact", session, "--json"))
    second = export("second.jsonl")
    stop()
    start()
    verify_selection()
    send("Reply after restart without changing scope.")
    restarted = export("restarted.jsonl")
    identity["restarted_request_completed"] = True
    (work / "injection-verification.json").write_text(json.dumps(verify_injections(control, args.outcome), indent=2))
    identity["outcome"] = args.outcome
    subprocess.run(["python3", str(pathlib.Path(__file__).parent / "verify.py"),
                    str(first), str(second), str(restarted)], check=True)
finally:
    stop()
    for log in logs:
        log.close()
    (work / "artifact-identity.json").write_text(json.dumps(identity, indent=2))
    print(f"Artifacts retained: {work}")
