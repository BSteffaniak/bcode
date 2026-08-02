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
skip_build="${BCODE_DAEMON_PERF_SKIP_BUILD:-0}"
if [[ "${skip_build}" != "0" && "${skip_build}" != "1" ]]; then
  echo "BCODE_DAEMON_PERF_SKIP_BUILD must be 0 or 1" >&2
  exit 2
fi
build_features="${BCODE_DAEMON_PERF_FEATURES:-distribution,static-bundled-tantivy-session-search-plugin}"
if [[ "${skip_build}" == "0" ]]; then
  cargo build --quiet "${cargo_profile_args[@]}" -p bcode --features "${build_features}"
fi

bcode="${BCODE_DAEMON_PERF_BINARY:-${root}/target/${binary_dir}/bcode}"
if [[ ! -x "${bcode}" ]]; then
  echo "daemon performance binary is not executable: ${bcode}" >&2
  exit 2
fi
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
  local mode="${1:-default}"
  local workdir
  workdir="$(mktemp -d "${suite_work_root}/environment.XXXXXX")"
  mkdir -p "${workdir}/tmp" "${workdir}/state"
  printf '[client]\nrequest_timeout_secs = 30\n' >"${workdir}/config.toml"
  case "${mode}" in
    search-disabled)
      printf '\n[session_search]\nenabled = false\n' >>"${workdir}/config.toml"
      ;;
    search-enabled-empty)
      mkdir -p "${workdir}/search-index"
      cat >>"${workdir}/config.toml" <<EOF

[session_search]
enabled = true

[plugins]
enabled = ["bcode.tantivy-session-search"]

[plugins.config."bcode.tantivy-session-search"]
storage_root = "${workdir}/search-index"
EOF
      ;;
    search-enabled-large|search-rebuilding)
      local source_root
      if [[ "${mode}" == "search-enabled-large" ]]; then
        source_root="${BCODE_DAEMON_PERF_LARGE_INDEX_ROOT:-}"
      else
        source_root="${BCODE_DAEMON_PERF_REBUILDING_INDEX_ROOT:-}"
      fi
      if [[ -z "${source_root}" || ! -d "${source_root}" ]]; then
        echo "${mode} requires its configured provider fixture root" >&2
        return 2
      fi
      cp -R "${source_root}" "${workdir}/search-index"
      cat >>"${workdir}/config.toml" <<EOF

[session_search]
enabled = true

[plugins]
enabled = ["bcode.tantivy-session-search"]

[plugins.config."bcode.tantivy-session-search"]
storage_root = "${workdir}/search-index"
EOF
      ;;
    default) ;;
    *)
      echo "unknown startup environment mode: ${mode}" >&2
      return 2
      ;;
  esac
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

enforce_budgets="${BCODE_DAEMON_PERF_ENFORCE_BUDGETS:-1}"
if [[ "${enforce_budgets}" != "0" && "${enforce_budgets}" != "1" ]]; then
  echo "BCODE_DAEMON_PERF_ENFORCE_BUDGETS must be 0 or 1" >&2
  exit 2
fi

summarize() {
  local scenario="$1"
  local command="$2"
  local p95_limit_us="${3:-}"
  local limit_args=()
  if [[ -n "${p95_limit_us}" && "${enforce_budgets}" == "1" ]]; then
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
summarize cached-cold "bcode server startup-probe" 500000

: >"${output_dir}/first-cold.samples"
for ((iteration = 0; iteration < samples; iteration++)); do
  first_workdir="$(new_environment)"
  workdirs+=("${first_workdir}")
  run_bcode "${first_workdir}" "${probe}" server startup-probe >>"${output_dir}/first-cold.samples"
  run_bcode "${first_workdir}" "${bcode}" server stop --force >/dev/null
  rm -rf "${first_workdir}"
done
summarize first-cold "bcode server startup-probe"

startup_modes=(search-disabled search-enabled-empty)
if [[ -n "${BCODE_DAEMON_PERF_LARGE_INDEX_ROOT:-}" ]]; then
  startup_modes+=(search-enabled-large)
fi
if [[ -n "${BCODE_DAEMON_PERF_REBUILDING_INDEX_ROOT:-}" ]]; then
  startup_modes+=(search-rebuilding)
fi
for mode in "${startup_modes[@]}"; do
  : >"${output_dir}/${mode}-cold.samples"
  for ((iteration = 0; iteration < samples; iteration++)); do
    mode_workdir="$(new_environment "${mode}")"
    workdirs+=("${mode_workdir}")
    run_bcode "${mode_workdir}" "${probe}" server startup-probe >>"${output_dir}/${mode}-cold.samples"
    run_bcode "${mode_workdir}" "${bcode}" server metrics --json >"${output_dir}/${mode}-${iteration}-metrics.json"
    run_bcode "${mode_workdir}" "${bcode}" server stop --force >/dev/null
    rm -rf "${mode_workdir}"
  done
  summarize "${mode}-cold" "bcode server startup-probe"
done

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
