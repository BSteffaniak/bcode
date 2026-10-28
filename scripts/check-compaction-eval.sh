#!/usr/bin/env bash
set -euo pipefail

# Exercise actual local compaction, canonical persistence, automatic admission,
# cancellation and isolated daemon restart/crash using the static fake provider.
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
binary="${BCODE_COMPACTION_EVAL_BINARY:-}"
if [[ "${1:-}" == "--python-only" ]]; then
    PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s "${root}/fixtures/evals/compaction" -p 'test_*.py'
    exit
fi
if [[ $# -ne 0 ]]; then
    echo 'usage: check-compaction-eval.sh [--python-only]' >&2
    exit 2
fi
if [[ -z "${binary}" ]]; then
    cargo build --locked -p bcode --bin bcode --features app,static-bundled-plugins,static-bundled-fake-provider-plugin
    binary="$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/debug/bcode"
fi
if [[ "${binary}" != /* || ! -x "${binary}" ]]; then
    echo 'BCODE_COMPACTION_EVAL_BINARY must identify an executable absolute path' >&2
    exit 1
fi
printf 'Compaction regression binary: %s\n' "${binary}"
shasum -a 256 "${binary}"
python3 -m unittest discover -s "${root}/fixtures/evals/compaction" -p 'test_*.py'
# Each driver owns an isolated state root, socket and fake-provider daemon. Run a bounded
# number together; retain every log and wait for all jobs in a batch before reporting failure.
logs="$(mktemp -d "${TMPDIR:-/tmp}/bcode-compaction-gate.XXXXXX")"
printf 'Compaction gate logs: %s\n' "${logs}"
pids=()
names=()
wait_batch() {
    local failed=0 index
    for index in "${!pids[@]}"; do
        if ! wait "${pids[index]}"; then
            printf 'FAILED: %s\n' "${names[index]}" >&2
            failed=1
        fi
        cat "${logs}/${names[index]}.log"
    done
    pids=()
    names=()
    return "${failed}"
}
run_case() {
    local name="$1"
    shift
    python3 "${root}/fixtures/evals/compaction/$1" --binary "${binary}" "${@:2}" >"${logs}/${name}.log" 2>&1 &
    pids+=("$!")
    names+=("${name}")
    if [[ "${#pids[@]}" -ge 4 ]]; then
        wait_batch
    fi
}
run_case explicit run.py
run_case progressive run_progressive.py
run_case recovery run_recovery.py
run_case policy-mismatch run_recovery.py --policy-mismatch
run_case consolidation-interrupted run_consolidation.py
run_case consolidation-reclaim run_consolidation.py --finish-consolidation
for outcome in repair oversize-repair omit-scope-repair paraphrase-scope-repair near-limit; do
    run_case "${outcome}" run.py --outcome "${outcome}"
done
for interruption in cancel restart crash; do
    run_case "automatic-${interruption}" run_automatic.py --interruption "${interruption}"
done
wait_batch

# Compare identical multi-request inputs; the retired packing switch must be a no-op.
run_case packing-off run_automatic.py --source-repeat 18000 --timeout-secs 300
run_case packing-on run_automatic.py --source-repeat 18000 --adaptive-packing
wait_batch
off="$(python3 -c 'import sys; print(next(line.strip().removeprefix("Artifacts retained: ") for line in open(sys.argv[1]) if line.startswith("Artifacts retained: ")))' "${logs}/packing-off.log")"
on="$(python3 -c 'import sys; print(next(line.strip().removeprefix("Artifacts retained: ") for line in open(sys.argv[1]) if line.startswith("Artifacts retained: ")))' "${logs}/packing-on.log")"
python3 "${root}/fixtures/evals/compaction/verify_automatic.py" --compare-packing "${off}" "${on}"
