#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
output="${1:-${root}/target/tui-product-latency.json}"
process_samples="${BCODE_TUI_PROCESS_SAMPLES:-5}"
locked_peak_rss_budget_bytes="${BCODE_TUI_PEAK_RSS_BUDGET_BYTES:-33554432}"
if [[ ! "${process_samples}" =~ ^[1-9][0-9]*$ ]]; then
    echo "BCODE_TUI_PROCESS_SAMPLES must be a positive integer" >&2
    exit 1
fi
if [[ ! "${locked_peak_rss_budget_bytes}" =~ ^[1-9][0-9]*$ ]]; then
    echo "BCODE_TUI_PEAK_RSS_BUDGET_BYTES must be a positive integer" >&2
    exit 1
fi
mkdir -p "$(dirname "${output}")"
tmp="$(mktemp -d /tmp/bcode-tui-product-latency.XXXXXX)"
cleanup() {
    rm -rf "${tmp}"
}
trap cleanup EXIT

cd "${root}"

# Build once, then execute the exact test binary directly so sample timing never
# includes Cargo dependency checks, compilation, or lock contention.
cargo test -p bcode_tui product_input_and_timer_to_committed_presentation_latency_report \
    --lib --no-run --message-format=json \
    | python3 -c 'import json, sys
for line in sys.stdin:
    try:
        message = json.loads(line)
    except json.JSONDecodeError:
        continue
    executable = message.get("executable")
    target = message.get("target", {})
    if executable and target.get("name") == "bcode_tui" and "lib" in target.get("kind", []):
        print(executable)' > "${tmp}/binary"

probe_binary="$(tail -n 1 "${tmp}/binary")"
if [[ -z "${probe_binary}" || ! -x "${probe_binary}" ]]; then
    echo "failed to locate built bcode_tui test probe" >&2
    exit 1
fi
git_revision="$(git rev-parse HEAD)"
git_dirty=false
if [[ -n "$(git status --porcelain)" ]]; then
    git_dirty=true
fi
bmux_revision="$(git -C "${root}/../bmux" rev-parse HEAD 2>/dev/null || true)"
bmux_dirty=false
if [[ -n "${bmux_revision}" && -n "$(git -C "${root}/../bmux" status --porcelain)" ]]; then
    bmux_dirty=true
fi
rustc_version="$(rustc --version)"
cargo_version="$(cargo --version)"

probe_output="$(${probe_binary} \
    --ignored --exact \
    root_program::tests::terminal_and_timer_latency_stay_within_flood_acceptance_budget \
    --nocapture)"
printf '%s\n' "${probe_output}" >&2

: > "${tmp}/runs.jsonl"
: > "${tmp}/rss.txt"
for sample in $(seq 1 "${process_samples}"); do
    /usr/bin/time -l "${probe_binary}" \
        --ignored --exact \
        root_program::tests::product_input_and_timer_to_committed_presentation_latency_report \
        --nocapture > "${tmp}/run-${sample}.out" 2> "${tmp}/run-${sample}.err"
    cat "${tmp}/run-${sample}.out" >&2
    cat "${tmp}/run-${sample}.err" >&2
    sed -n 's/^BCODE_PERF_CASE //p' "${tmp}/run-${sample}.out" | tail -n 1 \
        >> "${tmp}/runs.jsonl"
    awk '/maximum resident set size/ {print $1}' "${tmp}/run-${sample}.err" \
        >> "${tmp}/rss.txt"
done

python3 - "${output}" "${tmp}/runs.jsonl" "${tmp}/rss.txt" "${probe_binary}" \
    "${locked_peak_rss_budget_bytes}" "${git_revision}" "${git_dirty}" \
    "${bmux_revision}" "${bmux_dirty}" "${rustc_version}" "${cargo_version}" <<'PY'
import json
import os
import platform
import sys

(
    output,
    runs_path,
    rss_path,
    binary,
    rss_budget_text,
    git_revision,
    git_dirty_text,
    bmux_revision,
    bmux_dirty_text,
    rustc_version,
    cargo_version,
) = sys.argv[1:]
with open(runs_path, encoding="utf-8") as source:
    runs = [json.loads(line) for line in source if line.strip()]
with open(rss_path, encoding="utf-8") as source:
    rss_samples = [int(line) for line in source if line.strip()]
if not runs or len(runs) != len(rss_samples):
    raise SystemExit("probe did not emit one latency and RSS sample per process")
first = runs[0]
for run in runs:
    if (
        run.get("domain") != "product_committed_presentation_latency"
        or run.get("profile") != first.get("profile")
        or run.get("sample_count") != first.get("sample_count")
        or run.get("locked_p99_budget_ms") != first.get("locked_p99_budget_ms")
        or len(run.get("terminal", {}).get("samples_ms", [])) != run.get("sample_count")
        or len(run.get("timer", {}).get("samples_ms", [])) != run.get("sample_count")
    ):
        raise SystemExit("probe processes emitted inconsistent latency records")

def summary(values):
    ordered = sorted(values)
    def percentile(percent):
        rank = max(1, (len(ordered) * percent + 99) // 100)
        return ordered[min(rank - 1, len(ordered) - 1)]
    return {
        "samples": ordered,
        "min": ordered[0],
        "p50": percentile(50),
        "p95": percentile(95),
        "p99": percentile(99),
        "max": ordered[-1],
    }

terminal = summary([
    value
    for run in runs
    for value in run["terminal"]["samples_ms"]
])
timer = summary([
    value
    for run in runs
    for value in run["timer"]["samples_ms"]
])
rss = summary(rss_samples)
rss_budget = int(rss_budget_text)
record = {
    "schema_version": 2,
    "kind": "bcode_tui_product_latency",
    "domain": "product_committed_presentation_latency",
    "profile": first["profile"],
    "probe_binary": os.path.realpath(binary),
    "machine": platform.platform(),
    "git_revision": git_revision,
    "git_dirty": git_dirty_text == "true",
    "bmux_revision": bmux_revision or None,
    "bmux_dirty": bmux_dirty_text == "true" if bmux_revision else None,
    "rustc_version": rustc_version,
    "cargo_version": cargo_version,
    "process_sample_count": len(runs),
    "latency_sample_count_per_process": first["sample_count"],
    "latency_sample_count": len(terminal["samples"]),
    "locked_p99_budget_ms": first["locked_p99_budget_ms"],
    "locked_peak_rss_budget_bytes": rss_budget,
    "latency_gate_passed": (
        terminal["p99"] <= first["locked_p99_budget_ms"]
        and timer["p99"] <= first["locked_p99_budget_ms"]
    ),
    "peak_rss_gate_passed": rss["p95"] <= rss_budget,
    "terminal": {f"{key}_ms" if key != "samples" else "samples_ms": value
                 for key, value in terminal.items()},
    "timer": {f"{key}_ms" if key != "samples" else "samples_ms": value
              for key, value in timer.items()},
    "peak_rss": {f"{key}_bytes" if key != "samples" else "samples_bytes": value
                 for key, value in rss.items()},
}
with open(output, "w", encoding="utf-8") as destination:
    json.dump(record, destination, indent=2, sort_keys=True)
    destination.write("\n")

print(json.dumps({
    "artifact": output,
    "terminal_p99_ms": record["terminal"]["p99_ms"],
    "timer_p99_ms": record["timer"]["p99_ms"],
    "peak_rss_p95_bytes": record["peak_rss"]["p95_bytes"],
    "locked_p99_budget_ms": record["locked_p99_budget_ms"],
    "locked_peak_rss_budget_bytes": rss_budget,
    "latency_gate_passed": record["latency_gate_passed"],
    "peak_rss_gate_passed": record["peak_rss_gate_passed"],
}, sort_keys=True))
if not record["latency_gate_passed"]:
    raise SystemExit(
        "terminal/timer p99 exceeded locked latency budget "
        f"{record['locked_p99_budget_ms']}"
    )
if not record["peak_rss_gate_passed"]:
    raise SystemExit(
        f"peak RSS p95 {rss['p95']} exceeded locked budget {rss_budget}"
    )
PY
