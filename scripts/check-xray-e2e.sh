#!/usr/bin/env bash
# Deferred offline acceptance runner. No builds or ambient daemon management.
# Whole-app tests own a fresh foreground server and matching clients in private state.
# Supply deliberately selected, matching test artifacts.
set -euo pipefail

selection=all
if [[ ${1:-} == --suite ]]; then
    if [[ $# -lt 2 ]]; then
        echo "--suite requires all, acceptance, server, or recursive_server" >&2
        exit 2
    fi
    selection=$2
    shift 2
fi
case "$selection" in
    all|acceptance|server|recursive_server) ;;
    *) echo "unknown suite selection: $selection" >&2; exit 2 ;;
esac

if [[ $# != 8 && $# != 10 ]]; then
    echo "usage: bash $0 [--suite all|acceptance|server|recursive_server] /absolute/bcode sha256 /absolute/xray-acceptance-test sha256 /absolute/server-xray-test sha256 /absolute/fake-provider-library sha256 [/absolute/server-lib-test sha256]" >&2
    exit 2
fi
if [[ $selection == recursive_server && $# != 10 ]]; then
    echo "recursive_server requires the explicit server-lib-test artifact and hash" >&2
    exit 2
fi

# Python owns the entire isolated subprocess environment and bounded execution.
# No caller credentials, BCODE_* overrides, proxy settings, or config are inherited.
exec python3 - "$selection" "$@" <<'PY'
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile

selection = sys.argv[1]
args = sys.argv[2:]
artifacts = []
selections = [("bcode", 0), ("acceptance", 2), ("server", 4), ("provider_library", 6)]
if len(args) == 10:
    selections.append(("recursive_server", 8))
for name, offset in selections:
    path = Path(args[offset])
    expected = args[offset + 1]
    if not path.is_absolute() or not path.is_file() or (name != "provider_library" and not os.access(path, os.X_OK)):
        raise SystemExit(f"{name}: absolute existing artifact required (executable for test/client)")
    if not re.fullmatch(r"[0-9a-f]{64}", expected):
        raise SystemExit(f"{name}: explicit lowercase SHA-256 required")
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    if digest.hexdigest() != expected:
        raise SystemExit(f"{name}: artifact identity mismatch; no executable invoked")
    artifacts.append({"name": name, "path": str(path), "sha256": expected})

os.umask(0o077)
work = Path(tempfile.mkdtemp(prefix="bcode-xray-e2e-"))
print(f"private acceptance artifacts retained: {work}", flush=True)
for directory in ("home", "config", "state", "tmp", "work"):
    (work / directory).mkdir(mode=0o700)
config = work / "config" / "test.toml"
config.write_text('[plugins]\nenabled = []\n[observability]\npersist_model_requests = false\npersist_tool_io = false\n')
(work / "artifact-identity.json").write_text(json.dumps(artifacts, indent=2) + "\n")
env = {
    "PATH": os.defpath,
    "HOME": str(work / "home"),
    "TMPDIR": str(work / "tmp"),
    "XDG_CONFIG_HOME": str(work / "config"),
    "XDG_STATE_HOME": str(work / "state"),
    "BCODE_CONFIG": str(config),
    "BCODE_STATE_DIR": str(work / "state"),
    "BCODE_XRAY_TEST_BINARY": artifacts[0]["path"],
    "BCODE_XRAY_TEST_PROVIDER_LIBRARY": artifacts[3]["path"],
    # Child fixtures retain their private roots on either outcome and print only
    # locators to this log; do not inline stderr (it may contain captured content).
    "RUST_BACKTRACE": "1",
}
results = []
suites = artifacts[1:3] + artifacts[4:]
if selection != "all":
    suites = [artifact for artifact in suites if artifact["name"] == selection]
scope = {
    "selection": selection,
    "selected_suites": [artifact["name"] for artifact in suites],
    "all_supplied_suites_selected": selection == "all",
    "recursive_server_supplied": len(artifacts) == 5,
}
(work / "scope.json").write_text(json.dumps(scope, indent=2) + "\n")
for artifact in suites:
    command = [artifact["path"]]
    if artifact["name"] == "recursive_server":
        command.append("xray_execution_acceptance_tests::")
    list_command = command + ["--list"]
    command += ["--include-ignored", "--test-threads=1", "--nocapture"]
    try:
        # Recheck before each suite: mutable local artifact paths must not silently
        # become a different client or provider after the initial selection.
        for selected in artifacts:
            digest = hashlib.sha256()
            with Path(selected["path"]).open("rb") as source:
                for chunk in iter(lambda: source.read(1024 * 1024), b""):
                    digest.update(chunk)
            if digest.hexdigest() != selected["sha256"]:
                raise SystemExit("selected artifact changed; no further suite invoked")
        list_path = work / f'{artifact["name"]}.list'
        with list_path.open("wb") as listing:
            inventory = subprocess.run(
                list_command, cwd=work / "work", env=env, stdin=subprocess.DEVNULL,
                stdout=listing, stderr=subprocess.STDOUT, timeout=30, check=False,
            )
        listed_tests = len(re.findall(rb"^.+: test\r?$", list_path.read_bytes(), re.MULTILINE))
        if inventory.returncode != 0 or listed_tests == 0:
            raise SystemExit(f'{artifact["name"]}: missing nonempty test inventory; suite not run')
        with (work / f'{artifact["name"]}.log').open("wb") as log:
            process = subprocess.Popen(command, cwd=work / "work", env=env,
                                       stdin=subprocess.DEVNULL, stdout=log,
                                       stderr=subprocess.STDOUT, start_new_session=True)
            try:
                # The serial server suite includes six whole-app cases and a
                # deliberate observer crash with up to 75s for verified reclamation.
                # The 360s aggregate expired during its last production case despite
                # preceding cases passing; keep per-operation/test bounds unchanged.
                suite_timeout = 600 if artifact["name"] == "server" else 360
                code = process.wait(timeout=suite_timeout)
                results.append({"name": artifact["name"], "exit_code": code})
            except subprocess.TimeoutExpired:
                results.append({"name": artifact["name"], "timeout": True})
            finally:
                # Only this new process group: reap foreground fixture children even
                # if the test process aborts before its RAII teardown. Never use a
                # daemon registry, ambient PID lookup, or stop-all command.
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait()
    except subprocess.TimeoutExpired:
        results.append({"name": artifact["name"], "inventory_timeout": True})
    except OSError:
        results.append({"name": artifact["name"], "launch_failed": True})
    if results[-1].get("exit_code") == 0:
        log_bytes = (work / f'{artifact["name"]}.log').read_bytes()
        summaries = re.findall(rb"test result: ok\. ([0-9]+) passed; ([0-9]+) failed; ([0-9]+) ignored;", log_bytes)
        results[-1]["listed_tests"] = listed_tests
        passed_tests = sum(int(passed) for passed, _, _ in summaries)
        results[-1]["passed_tests"] = passed_tests
        if not summaries or any(int(failed) or int(ignored) for _, failed, ignored in summaries) or passed_tests != listed_tests:
            results[-1]["acceptance_error"] = "not_all_listed_tests_passed_unignored"
            results[-1]["exit_code"] = 1
    (work / "results.json").write_text(json.dumps({"scope": scope, "artifacts": artifacts, "results": results}, indent=2) + "\n")
    if results[-1].get("exit_code") != 0:
        print(f'{artifact["name"]} failed or timed out; see private log', file=sys.stderr)
        raise SystemExit(1)
print(f"Selected offline fixture suites passed: {', '.join(scope['selected_suites'])}. No suite was retried. Inspect results.json for scope; subset runs are not full-matrix acceptance. Fake-provider mechanics do not certify live provider quality or complete v1 coverage.")
PY
