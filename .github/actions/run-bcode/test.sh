#!/usr/bin/env bash
# Offline adapter contract; no broker, daemon, credentials, or model spending.
set -euo pipefail
umask 077
root=$(mktemp -d)
trap 'rm -rf -- "$root"' EXIT
mkdir "$root/workspace" "$root/runner" "$root/bin"
export GITHUB_WORKSPACE="$root/workspace" RUNNER_TEMP="$root/runner" GITHUB_OUTPUT="$root/output"
export BCODE_ACTION_BINARY="$root/bin/bcode" BCODE_ACTION_AUTH_MODE=broker-oidc
export BCODE_ACTION_BROKER_URL=https://broker.example BCODE_ACTION_BROKER_AUDIENCE=bcode
export BCODE_ACTION_TIMEOUT=600 BCODE_ACTION_PROMPT='prompt as data'
export ACTIONS_ID_TOKEN_REQUEST_TOKEN=synthetic-oidc ACTIONS_ID_TOKEN_REQUEST_URL=https://x.actions.githubusercontent.com/token
export BCODE_ACTION_CONFIG='' BCODE_ACTION_PROFILE=''
export TEST_MARKER="$root/marker"
cat > "$BCODE_ACTION_BINARY" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
for arg in "$@"; do [[ "$arg" != *synthetic-oidc* && "$arg" != *synthetic-lease* ]] || exit 91; done
if [[ "$*" == *'auth-broker prepare'* ]]; then
  [[ "${ACTIONS_ID_TOKEN_REQUEST_TOKEN:-}" = synthetic-oidc ]] || exit 92
  while (( $# )); do
    case "$1" in --directory) directory=$2; shift 2 ;; *) shift ;; esac
  done
  printf 'synthetic-lease\n' > "$directory/broker-token"
  printf '1900\n' > "$directory/broker-expiry"
  printf '[model.profiles.github-broker]\n' > "$directory/broker.toml"
  exit 0
fi
[[ -z "${BCODE_AUTH_BROKER_TOKEN:-}" && -z "${BCODE_AUTH_BROKER_EXPIRES_AT:-}" && -z "${ACTIONS_ID_TOKEN_REQUEST_TOKEN:-}" && -z "${ACTIONS_ID_TOKEN_REQUEST_URL:-}" ]] || exit 93
if [[ "$*" == *'stop-if-idle'* ]]; then exit 0; fi
[[ "$*" == *'--profile github-broker'* ]] || exit 94
printf 'executed\n' > "$TEST_MARKER"
printf '{"outcome":"completed"}\n'
MOCK
chmod 700 "$BCODE_ACTION_BINARY"
bash "$(dirname -- "$0")/run.sh" > "$root/log"
[[ -f "$TEST_MARKER" ]]
for scratch in "$root/runner"/bcode-run.*; do
  [[ ! -e "$scratch/broker-token" && ! -e "$scratch/broker-expiry" && ! -e "$scratch/broker.toml" ]]
done
# Explicit direct/broker separation rejects mixed inputs before invoking the binary.
export BCODE_ACTION_CONFIG="$root/trusted.toml"
if bash "$(dirname -- "$0")/run.sh" > /dev/null 2>&1; then exit 95; fi
export BCODE_ACTION_CONFIG='' BCODE_ACTION_AUTH_MODE=direct
if bash "$(dirname -- "$0")/run.sh" > /dev/null 2>&1; then exit 96; fi
printf 'run-bcode Rust bootstrap adapter tests passed\n'
