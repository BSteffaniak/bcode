#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
output="${1:-${root}/target/live-shell-tui-performance-baseline.jsonl}"
mkdir -p "$(dirname "${output}")"
tmp="$(mktemp -d /tmp/bcode-live-shell-baseline.XXXXXX)"
cleanup() {
    rm -rf "${tmp}"
}
trap cleanup EXIT

cd "${root}"

run_probe() {
    local package="$1"
    local test_name="$2"
    shift 2
    local log="${tmp}/${package}-${test_name//::/-}.log"
    cargo test -p "${package}" "${test_name}" --lib "$@" -- --ignored --exact --nocapture \
        | tee "${log}" >&2
    grep '^BCODE_PERF_CASE ' "${log}" | sed 's/^BCODE_PERF_CASE //'
}

{
    printf '%s\n' '{"schema_version":1,"kind":"live_shell_tui_performance_baseline"}'
    run_probe bcode_shell_plugin tests::live_shell_output_chunk_baseline_report
    run_probe bcode_shell_plugin shell_run_tui::tests::incremental_replay_work_baseline_report --features static-bundled
    run_probe bcode_tui telemetry::tests::telemetry_enabled_disabled_overhead_baseline_report
    run_probe bcode_server tests::transient_streaming_fixed_workload_baseline_report
} >"${output}"

python3 - "${output}" <<'PY'
import json
import sys

path = sys.argv[1]
with open(path, encoding="utf-8") as source:
    records = [json.loads(line) for line in source if line.strip()]

shell = [record for record in records if record.get("domain") == "shell_output"]
replay = [record for record in records if record.get("domain") == "shell_replay"]
telemetry = [record for record in records if record.get("domain") == "telemetry_overhead"]
publisher = [
    record for record in records if record.get("domain") == "pending_live_fanout"
]
if len(shell) != 9:
    raise SystemExit(f"expected 9 shell matrix cases, found {len(shell)}")
if any(
    record["published_updates"]
    > (record["recording_bytes"] + 65535) // 65536
    + (record["wall_us"] + 15999) // 16000
    + 2
    for record in shell
):
    raise SystemExit("shell publication count exceeded byte-bound policy")
if len(replay) != 9 or any(
    record["emulate_bytes"] != record["output_bytes"]
    or record["retained_frame_bytes"] != record["output_bytes"]
    for record in replay
):
    raise SystemExit("shell replay-work matrix is incomplete or amplified")
if {record["enabled"] for record in telemetry} != {False, True}:
    raise SystemExit("telemetry enabled/disabled control is incomplete")
if len(publisher) != 5 or any(
    record["rejected_updates"] != 0 or record["pass_through_updates"] != 0
    for record in publisher
):
    raise SystemExit("server pending fan-out baseline is incomplete")
expected_volumes = {64 * 1024, 1024 * 1024, 8 * 1024 * 1024}
expected_chunks = {17, 4 * 1024, 16 * 1024}
if {record["output_bytes"] for record in shell} != expected_volumes:
    raise SystemExit("shell output-volume matrix is incomplete")
if {record["chunk_bytes"] for record in shell} != expected_chunks:
    raise SystemExit("shell chunk-size matrix is incomplete")
print(path)
PY
