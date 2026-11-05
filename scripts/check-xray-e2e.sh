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
    "BCODE_XRAY_EVAL_BINARY": artifacts[0]["path"],
    "BCODE_XRAY_EVAL_SHA256": artifacts[0]["sha256"],
    "BCODE_XRAY_ACCEPTANCE_ARTIFACTS": str(work),
    # Child fixtures retain their private roots on either outcome and print only
    # locators to this log; do not inline stderr (it may contain captured content).
    "RUST_BACKTRACE": "1",
}
results = []
def interrupted(signum, _frame):
    # Convert termination into the same evidence-preserving unwind as Ctrl-C.
    # Do not restart the current exact case, even if admission is unresolved.
    signal.signal(signum, signal.SIG_IGN)
    raise KeyboardInterrupt
signal.signal(signal.SIGTERM, interrupted)
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
    # Inventory once, then supervise each exact case independently. An aggregate
    # suite deadline must not erase later cases or cause admission to be replayed.
    suite = {"name": artifact["name"], "cases": [], "exit_code": None}
    results.append(suite)
    def retain_results():
        (work / "results.json").write_text(json.dumps({"scope": scope, "artifacts": artifacts, "results": results}, indent=2) + "\n")
    retain_results()
    try:
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
        names = re.findall(rb"^(.+): test\r?$", list_path.read_bytes(), re.MULTILINE)
        if inventory.returncode != 0 or not names:
            suite["inventory_failed"] = True
            suite["exit_code"] = 1
            retain_results()
            continue
        suite["listed_tests"] = len(names)
        for index, raw_name in enumerate(names):
            name = raw_name.decode("utf-8")
            case = {"name": name, "state": "starting", "attempts": 1,
                    "admission_replayed": False, "exit_code": None}
            suite["cases"].append(case)
            retain_results()
            log_path = work / f'{artifact["name"]}-{index:04}.log'
            case["log"] = log_path.name
            with log_path.open("wb") as log:
                process = None
                try:
                    process = subprocess.Popen(
                        [artifact["path"], name, "--exact", "--include-ignored", "--test-threads=1", "--nocapture"],
                        cwd=work / "work", env=env, stdin=subprocess.DEVNULL,
                        stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
                    case["state"] = "running"
                    retain_results()
                    case["exit_code"] = process.wait(timeout=360)
                    case["state"] = "finished"
                except subprocess.TimeoutExpired:
                    case["state"] = "timed_out"
                except (KeyboardInterrupt, InterruptedError):
                    case["state"] = "interrupted"
                    raise
                except OSError:
                    case["state"] = "launch_failed"
                finally:
                    # Persist the primary result BEFORE teardown. Restrict reaping
                    # to this newly owned process group; never consult registries.
                    retain_results()
                    case["cleanup"] = "started"
                    retain_results()
                    if process is not None:
                        try:
                            os.killpg(process.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        process.wait()
                    case["cleanup"] = "completed"
                    retain_results()
            if case["exit_code"] == 0:
                summaries = re.findall(rb"test result: ok\. ([0-9]+) passed; ([0-9]+) failed; ([0-9]+) ignored;", log_path.read_bytes())
                if len(summaries) != 1 or summaries[0] != (b"1", b"0", b"0"):
                    case["acceptance_error"] = "exact_case_not_passed_unignored"
                    case["exit_code"] = 1
            retain_results()
        suite["exit_code"] = int(any(case["exit_code"] != 0 for case in suite["cases"]))
    except subprocess.TimeoutExpired:
        suite["inventory_timeout"] = True
        suite["exit_code"] = 1
    except OSError:
        suite["launch_failed"] = True
        suite["exit_code"] = 1
    finally:
        retain_results()
if any(result["exit_code"] != 0 for result in results):
    print("Selected cases failed or timed out; inspect private per-case evidence. No case retried.", file=sys.stderr)
    raise SystemExit(1)
print(f"Selected offline fixture suites passed: {', '.join(scope['selected_suites'])}. No suite was retried. Inspect results.json for scope; subset runs are not full-matrix acceptance. Fake-provider mechanics do not certify live provider quality or complete v1 coverage.")
PY
