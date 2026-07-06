#!/usr/bin/env bash
set -euo pipefail

cargo run -p bcode -- eval validate fixtures/evals/edit-tools/suite.toml
cargo run -p bcode -- eval run fixtures/evals/edit-tools/suite.toml --run-id ci-edit-tools --fail-under-pass-rate 1.0
cargo run -p bcode -- eval compare target/bcode-evals/runs/ci-edit-tools --markdown target/bcode-evals/runs/ci-edit-tools/comparison.md --fail-under-pass-rate 1.0
