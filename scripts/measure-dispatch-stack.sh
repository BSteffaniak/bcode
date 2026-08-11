#!/usr/bin/env bash
# Measure the actual runtime stack high-water requirement of an IPC dispatch path.
#
# Why this exists: `clippy::large_stack_frames` analyses pre-optimization MIR and reports the same
# frame sizes in debug and release, so it cannot distinguish a real stack cost from a debug-only
# artifact. This script bisects the real requirement by running a representative test under
# decreasing `RUST_MIN_STACK` values until it overflows.
#
# Usage:
#   scripts/measure-dispatch-stack.sh [TEST_FILTER] [--release]
#
# Defaults to the permission IPC dispatch flow in debug.
set -euo pipefail

TEST_FILTER="${1:-permission_resolution_crosses_real_ipc}"
PROFILE_FLAG=""
PROFILE_LABEL="debug"
if [ "${2:-}" = "--release" ]; then
  PROFILE_FLAG="--release"
  PROFILE_LABEL="release"
fi

# Candidate stack sizes in KiB, descending. The reported requirement is the smallest passing size.
CANDIDATES=(4096 2048 1024 512 256 128 64)

echo "measuring stack requirement: filter='${TEST_FILTER}' profile=${PROFILE_LABEL}"

# Build once so compilation noise does not affect the per-size runs.
cargo test ${PROFILE_FLAG} -q -p bcode_server --lib "${TEST_FILTER}" --no-run >/dev/null 2>&1

smallest_pass=""
for kb in "${CANDIDATES[@]}"; do
  if RUST_MIN_STACK=$((kb * 1024)) cargo test ${PROFILE_FLAG} -q -p bcode_server --lib \
      "${TEST_FILTER}" 2>&1 | grep -qE "^test result: ok"; then
    printf '  %5s KiB: PASS\n' "${kb}"
    smallest_pass="${kb}"
  else
    printf '  %5s KiB: OVERFLOW\n' "${kb}"
    break
  fi
done

if [ -z "${smallest_pass}" ]; then
  echo "result: requires more than ${CANDIDATES[0]} KiB (or the test failed for another reason)" >&2
  exit 1
fi

echo "result: ${PROFILE_LABEL} requires <= ${smallest_pass} KiB for '${TEST_FILTER}'"
