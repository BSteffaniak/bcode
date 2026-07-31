#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
profile="${BCODE_DAEMON_PERF_PROFILE:-release}"
samples="${BCODE_DAEMON_PERF_SAMPLES:-20}"
output_dir="${BCODE_DAEMON_PERF_OUTPUT:-${root}/target/daemon-startup-performance}"
suite_work_root="$(mktemp -d /tmp/bcode-perf.XXXXXX)"

case "$(uname -s)" in
  Darwin|Linux) ;;
  *)
    echo "measure-daemon-startup.sh currently requires a Unix socket host" >&2
    exit 2
    ;;
esac

case "${profile}" in
  debug)
    cargo_profile_args=()
    binary_dir="debug"
    ;;
  release)
    cargo_profile_args=(--release)
    binary_dir="release"
    ;;
  *)
    echo "BCODE_DAEMON_PERF_PROFILE must be debug or release" >&2
    exit 2
    ;;
esac

if ! [[ "${samples}" =~ ^[1-9][0-9]*$ ]]; then
  echo "BCODE_DAEMON_PERF_SAMPLES must be a positive integer" >&2
  exit 2
fi

cd "${root}"
cargo build --quiet "${cargo_profile_args[@]}" -p bcode --features distribution

bcode="${root}/target/${binary_dir}/bcode"
probe="${bcode}"
artifact_bytes="$(wc -c <"${bcode}" | tr -d ' ')"
mkdir -p "${output_dir}"

workdirs=()
cleanup() {
  for workdir in "${workdirs[@]}"; do
    if [[ -x "${bcode}" ]]; then
      BCODE_CONFIG="${workdir}/config.toml" \
        BCODE_STATE_DIR="${workdir}/state" \
        TMPDIR="${workdir}/tmp" \
        BCODE_SOCKET="${workdir}/bcode.sock" \
        BCODE_DAEMON_LOG="${workdir}/daemon.log" \
        "${bcode}" server stop --force >/dev/null 2>&1 || true
    fi
  done
  rm -rf "${suite_work_root}"
}
trap cleanup EXIT

new_environment() {
  local workdir
  workdir="$(mktemp -d "${suite_work_root}/environment.XXXXXX")"
  mkdir -p "${workdir}/tmp" "${workdir}/state"
  printf '[client]\nrequest_timeout_secs = 30\n' >"${workdir}/config.toml"
  printf '%s\n' "${workdir}"
}

run_bcode() {
  local workdir="$1"
  shift
  BCODE_CONFIG="${workdir}/config.toml" \
    BCODE_STATE_DIR="${workdir}/state" \
    TMPDIR="${workdir}/tmp" \
    BCODE_SOCKET="${workdir}/bcode.sock" \
    BCODE_DAEMON_LOG="${workdir}/daemon.log" \
    "$@"
}

summarize() {
  local scenario="$1"
  local command="$2"
  local p95_limit_us="${3:-}"
  local limit_args=()
  if [[ -n "${p95_limit_us}" ]]; then
    limit_args=(--p95-limit-us "${p95_limit_us}")
  fi
  python3 "${root}/scripts/summarize-daemon-startup-measurements.py" \
    --scenario "${scenario}" \
    --profile "${profile}" \
    --command "${command}" \
    --artifact-bytes "${artifact_bytes}" \
    "${limit_args[@]}" \
    --output "${output_dir}/${scenario}.json" \
    "${output_dir}/${scenario}.samples"
}

warm_workdir="$(new_environment)"
workdirs+=("${warm_workdir}")
run_bcode "${warm_workdir}" "${bcode}" server start >/dev/null

: >"${output_dir}/warm-handshake.samples"
: >"${output_dir}/warm-status.samples"
for ((iteration = 0; iteration < samples; iteration++)); do
  run_bcode "${warm_workdir}" "${probe}" server startup-probe >>"${output_dir}/warm-handshake.samples"
  started_ns="$(python3 -c 'import time; print(time.monotonic_ns())')"
  run_bcode "${warm_workdir}" "${bcode}" server status >/dev/null
  finished_ns="$(python3 -c 'import time; print(time.monotonic_ns())')"
  printf '%s\n' "$(((finished_ns - started_ns) / 1000))" >>"${output_dir}/warm-status.samples"
done
summarize warm-handshake "bcode server startup-probe" 5000
summarize warm-status "bcode server status"

: >"${output_dir}/process-baseline.samples"
for ((iteration = 0; iteration < samples; iteration++)); do
  started_ns="$(python3 -c 'import time; print(time.monotonic_ns())')"
  run_bcode "${warm_workdir}" "${bcode}" --version >/dev/null
  finished_ns="$(python3 -c 'import time; print(time.monotonic_ns())')"
  printf '%s\n' "$(((finished_ns - started_ns) / 1000))" >>"${output_dir}/process-baseline.samples"
done
summarize process-baseline "bcode --version"
run_bcode "${warm_workdir}" "${bcode}" server stop --force >/dev/null

cache_workdir="$(new_environment)"
workdirs+=("${cache_workdir}")
run_bcode "${cache_workdir}" "${probe}" server startup-probe >/dev/null
cp "${cache_workdir}/daemon.log" "${output_dir}/first-startup-trace.log"
run_bcode "${cache_workdir}" "${bcode}" server stop --force >/dev/null
: >"${output_dir}/cached-cold.samples"
for ((iteration = 0; iteration < samples; iteration++)); do
  run_bcode "${cache_workdir}" "${probe}" server startup-probe >>"${output_dir}/cached-cold.samples"
  run_bcode "${cache_workdir}" "${bcode}" server stop --force >/dev/null
done
summarize cached-cold "bcode server startup-probe"

: >"${output_dir}/first-cold.samples"
for ((iteration = 0; iteration < samples; iteration++)); do
  first_workdir="$(new_environment)"
  workdirs+=("${first_workdir}")
  run_bcode "${first_workdir}" "${probe}" server startup-probe >>"${output_dir}/first-cold.samples"
  run_bcode "${first_workdir}" "${bcode}" server stop --force >/dev/null
  rm -rf "${first_workdir}"
done
summarize first-cold "bcode server startup-probe"

concurrent_workdir="$(new_environment)"
workdirs+=("${concurrent_workdir}")
concurrent_clients="${BCODE_DAEMON_PERF_CONCURRENT_CLIENTS:-16}"
concurrent_started_ns="$(python3 -c 'import time; print(time.monotonic_ns())')"
probe_pids=()
for ((client = 0; client < concurrent_clients; client++)); do
  run_bcode "${concurrent_workdir}" "${probe}" server startup-probe >"${output_dir}/concurrent-${client}.sample" &
  probe_pids+=("$!")
done
for pid in "${probe_pids[@]}"; do
  wait "${pid}"
done
concurrent_finished_ns="$(python3 -c 'import time; print(time.monotonic_ns())')"
printf '%s\n' "$(((concurrent_finished_ns - concurrent_started_ns) / 1000))" >"${output_dir}/concurrent-cold.samples"
summarize concurrent-cold "${concurrent_clients} concurrent bcode server startup-probe"
run_bcode "${concurrent_workdir}" "${bcode}" server status --verbose >"${output_dir}/concurrent-status.txt"
cp "${concurrent_workdir}/daemon.log" "${output_dir}/concurrent-startup-trace.log"
run_bcode "${concurrent_workdir}" "${bcode}" server stop --force >/dev/null

printf 'daemon startup performance reports: %s\n' "${output_dir}"
