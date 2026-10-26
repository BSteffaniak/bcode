#!/usr/bin/env bash
set -euo pipefail

# Exercise actual local compaction, canonical persistence, automatic admission,
# cancellation and isolated daemon restart/crash using the static fake provider.
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
binary="${BCODE_COMPACTION_EVAL_BINARY:-}"
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
python3 "${root}/fixtures/evals/compaction/run.py" --binary "${binary}"
for interruption in cancel restart crash; do
    python3 "${root}/fixtures/evals/compaction/run_automatic.py" \
        --binary "${binary}" --interruption "${interruption}"
done

# Compare identical larger inputs through both production packing paths. Preserve
# full run logs alongside each driver's retained scratch evidence.
comparison="$(mktemp -d "${TMPDIR:-/tmp}/bcode-compaction-packing.XXXXXX")"
python3 "${root}/fixtures/evals/compaction/run_automatic.py" \
    --binary "${binary}" --source-repeat 5500 | tee "${comparison}/off.log"
python3 "${root}/fixtures/evals/compaction/run_automatic.py" \
    --binary "${binary}" --source-repeat 5500 --adaptive-packing | tee "${comparison}/on.log"
off="$(python3 -c 'import sys; print(next(line.strip().removeprefix("Artifacts retained: ") for line in open(sys.argv[1]) if line.startswith("Artifacts retained: ")))' "${comparison}/off.log")"
on="$(python3 -c 'import sys; print(next(line.strip().removeprefix("Artifacts retained: ") for line in open(sys.argv[1]) if line.startswith("Artifacts retained: ")))' "${comparison}/on.log")"
python3 "${root}/fixtures/evals/compaction/verify_automatic.py" --compare-packing "${off}" "${on}"
