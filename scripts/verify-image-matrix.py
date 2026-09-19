#!/usr/bin/env python3
"""Opt-in, bounded image verification across explicitly supplied provider configs.

No credentials are read by this runner. Bcode resolves authentication normally. Configs
must explicitly select the intended provider/model. Reports never claim unsupported or
inconclusive cases passed. This runner does not authorize persistent remote uploads.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import signal


MAX_REPORT_BYTES = 1024 * 1024


def run_probe(command, env, output, timeout):
    """Bound stdout, terminate/reap locally, and never imply remote cancellation."""
    try:
        process = subprocess.Popen(command, env=env, stdout=subprocess.PIPE,
                                   stderr=subprocess.DEVNULL, start_new_session=(os.name == 'posix'))
    except OSError:
        return {"error": "process_start_failed"}
    def terminate():
        try:
            if os.name == 'posix':
                os.killpg(process.pid, signal.SIGKILL)
            else:
                process.kill()
        except ProcessLookupError:
            pass

    fault = []
    captured = bytearray()

    def drain():
        size = 0
        try:
            while True:
                chunk = process.stdout.read(8192)
                if not chunk:
                    return
                remaining = MAX_REPORT_BYTES - size
                captured.extend(chunk[:remaining])
                size += min(len(chunk), remaining)
                if len(chunk) > remaining:
                    fault.append("report_size_limit_remote_completion_unknown")
                    terminate()
                    return
        except OSError:
            fault.append("report_write_failed_remote_completion_unknown")
            terminate()

    reader = threading.Thread(target=drain, daemon=True)
    reader.start()
    try:
        process.wait(timeout=timeout)
        reader.join(timeout=max(0.1, timeout))
        if reader.is_alive():
            terminate()
            reader.join(timeout=1)
            return {"error": "report_pipe_timeout_remote_completion_unknown"}
        output.write(captured)
        if fault:
            return {"error": fault[0]}
        return {"exit_code": process.returncode}
    except subprocess.TimeoutExpired:
        terminate()
        process.wait()
        reader.join(timeout=1)
        return {"error": "process_timeout_remote_completion_unknown"}
    finally:
        if process.poll() is None:
            terminate()
            process.wait()
        if not reader.is_alive():
            process.stdout.close()


def verdict(envelope, expected_model=None, expected_source=None):
    """Judge normalized reports; malformed or mismatched evidence fails closed."""
    if not isinstance(envelope, dict) or type(envelope.get("schema_version")) is not int:
        return "inconclusive"
    if envelope.get("schema_version") != 1 or envelope.get("dry_run") is not False:
        return "inconclusive"
    results = envelope.get("results")
    if not isinstance(results, dict) or not results:
        return "inconclusive"
    if expected_model is not None and set(results) != {expected_model}:
        return "inconclusive"
    outcomes = []
    allowed = {"passed", "failed", "inconclusive", "blocked", "unsupported"}
    for result in results.values():
        if not isinstance(result, dict):
            return "inconclusive"
        if result.get("status") != "observed":
            return "failed"
        report = result.get("report")
        if not isinstance(report, dict) or type(report.get("schema_version")) is not int or report["schema_version"] != 1:
            return "inconclusive"
        if expected_source is not None and report.get("source", "user") != expected_source:
            return "inconclusive"
        raw_cases = report.get("cases")
        if not isinstance(raw_cases, list):
            return "inconclusive"
        cases = {}
        for case in raw_cases:
            if not isinstance(case, dict) or not isinstance(case.get("name"), str):
                return "inconclusive"
            if case["name"] in cases:
                return "inconclusive"
            if not isinstance(case.get("context"), str) or case["context"] not in allowed:
                return "inconclusive"
            if not isinstance(case.get("transfer"), str) or case["transfer"] not in allowed:
                return "inconclusive"
            cases[case["name"]] = case
        if any(case["context"] == "failed" or case["transfer"] == "failed" for case in cases.values()):
            return "failed"
        required = ("no_image_control", "image_acknowledgement", "inline_follow_up", "inline_repeat")
        outcomes.append("passed" if all(cases.get(name, {}).get("context") == "passed"
                                       for name in required) else "inconclusive")
    return "passed" if all(value == "passed" for value in outcomes) else "inconclusive"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bcode", default="bcode")
    parser.add_argument("--config", type=Path, action="append", required=True,
                        help="Explicit provider/model config; repeat for a matrix (maximum eight)")
    parser.add_argument("--model", action="append", required=True, help="Exact catalog model ID, one per config in matching order")
    parser.add_argument("--seed", type=int, action="append", help="Generated fixture seed (maximum four)")
    parser.add_argument("--live", action="store_true", help="Authorize paid model requests using generated fixtures")
    parser.add_argument("--allow-conversation-storage", action="store_true")
    parser.add_argument("--timeout-seconds", type=int, default=60)
    args = parser.parse_args()
    seeds = args.seed if args.seed is not None else [726, 451]
    if not 1 <= len(args.config) <= 8 or not 1 <= len(seeds) <= 4:
        parser.error("matrix exceeds config/seed bounds")
    if not 1 <= args.timeout_seconds <= 600 or any(seed < 0 or seed > 2**64 - 1 for seed in seeds):
        parser.error("invalid timeout or seed")
    if len(args.model) != len(args.config) or any(not model or '*' in model for model in args.model):
        parser.error("provide one exact non-wildcard --model per config")
    configs = [config.resolve(strict=True) for config in args.config]
    if not args.live:
        print(json.dumps({"schema_version": 1, "dry_run": True,
                          "config_count": len(configs), "seed_count": len(seeds),
                          "source_count": 2, "maximum_started_turns": len(configs) * len(seeds) * 10,
                          "note": "No requests sent. --live opts in; provider retries may add requests."}, indent=2))
        return 0
    root = Path(tempfile.mkdtemp(prefix="bcode-image-matrix-"))
    os.chmod(root, 0o700)
    results = []
    for index, config in enumerate(configs):
        for seed in seeds:
            for source in ("user", "tool_result"):
                label = f"config-{index}-seed-{seed}-{source}"
                env = dict(os.environ)
                env["BCODE_CONFIG"] = str(config)
                command = [args.bcode, "model", "verify-images", "--generated-seed", str(seed),
                           "--id-pattern", args.model[index],
                           "--max-models", "1", "--timeout-seconds", str(args.timeout_seconds)]
                if source == "tool_result":
                    command.append("--tool-result")
                if args.allow_conversation_storage:
                    command.append("--allow-conversation-storage")
                entry = {"probe": label, "verdict": "inconclusive"}
                with (root / (label + ".json")).open("xb") as output:
                    entry.update(run_probe(command, env, output, args.timeout_seconds * 5 + 30))
                path = root / (label + ".json")
                if path.stat().st_size > 1024 * 1024:
                    entry["error"] = "report_size_limit_exceeded"
                if "error" not in entry:
                    try:
                        envelope = json.loads(path.read_text())
                        entry["verdict"] = verdict(envelope, args.model[index], source) if entry["exit_code"] == 0 else "failed"
                    except (ValueError, KeyError, TypeError):
                        entry["error"] = "invalid_report"
                results.append(entry)
                if "error" in entry:
                    # Do not launch additional work after timeout/ambiguous outcomes.
                    break
            if results[-1].get("error"):
                break
        if results[-1].get("error"):
            break
    report = {"schema_version": 1, "results": results,
              "scope": "inline visual verification; continuation verdicts remain in individual reports"}
    (root / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"Reports retained at {root}")
    return 0 if all(result["verdict"] == "passed" for result in results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
