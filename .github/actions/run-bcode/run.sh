#!/usr/bin/env bash
# Trusted-code adapter; neither auth mode is an OS sandbox.
set -euo pipefail
umask 077
fail() { echo 'Bcode run setup failed; verify trusted inputs and runner paths' >&2; exit 1; }
[[ "${BCODE_ACTION_BINARY:-}" = /* && "${RUNNER_TEMP:-}" = /* && "${GITHUB_WORKSPACE:-}" = /* && "${GITHUB_OUTPUT:-}" = /* ]] || fail
[[ -x "$BCODE_ACTION_BINARY" ]] || fail
[[ "${BCODE_ACTION_TIMEOUT:-}" =~ ^[0-9]{1,5}$ ]] || fail
(( 10#$BCODE_ACTION_TIMEOUT >= 1 && 10#$BCODE_ACTION_TIMEOUT <= 86400 )) || fail
[[ ! -L "$GITHUB_OUTPUT" && ( ! -e "$GITHUB_OUTPUT" || -f "$GITHUB_OUTPUT" ) ]] || fail
workspace=$(cd -- "$GITHUB_WORKSPACE" && pwd -P)
parent=$(cd -- "$RUNNER_TEMP" && pwd -P)
for path in "$parent" "$(cd -- "$(dirname -- "$BCODE_ACTION_BINARY")" && pwd -P)"; do
  [[ "$path" != "$workspace" && "$path" != "$workspace/"* && "$path" != *$'\n'* && "$path" != *$'\r'* ]] || fail
done
scratch=$(mktemp -d "$parent/bcode-run.XXXXXXXX")
cleanup() {
  # A lease is not a refresh credential. Remove private copies on every exit;
  # broker TTL reaps remote work after cancellation/unknown runner loss.
  rm -f -- "$scratch/broker-token" "$scratch/broker-expiry" "$scratch/broker.toml"
  unset BCODE_AUTH_BROKER_TOKEN BCODE_AUTH_BROKER_EXPIRES_AT
}
trap cleanup EXIT
mkdir "$scratch/home"
export HOME="$scratch/home" XDG_CONFIG_HOME="$scratch/home/config" XDG_DATA_HOME="$scratch/home/data" XDG_STATE_HOME="$scratch/home/state" CI=true BCODE_NO_ONBOARD=1
unset BCODE_CONFIG BCODE_CONTEXT BCODE_PROFILE GITHUB_TOKEN GH_TOKEN
case "${BCODE_ACTION_AUTH_MODE:-direct}" in
  direct)
    [[ -z "${BCODE_ACTION_BROKER_URL:-}" && -z "${BCODE_ACTION_BROKER_AUDIENCE:-}" && -z "${BCODE_AUTH_BROKER_TOKEN:-}" && -z "${BCODE_AUTH_BROKER_EXPIRES_AT:-}" ]] || fail
    [[ "${BCODE_ACTION_CONFIG:-}" = /* && -f "$BCODE_ACTION_CONFIG" && ! -L "$BCODE_ACTION_CONFIG" && -n "${BCODE_ACTION_PROFILE:-}" ]] || fail
    path=$(cd -- "$(dirname -- "$BCODE_ACTION_CONFIG")" && pwd -P)
    [[ "$path" != "$workspace" && "$path" != "$workspace/"* && "$path" != *$'\n'* && "$path" != *$'\r'* ]] || fail
    ;;
  broker-oidc)
    # Never mix caller auth/config with the sole broker-selected profile.
    [[ -z "${BCODE_ACTION_CONFIG:-}" && -z "${BCODE_ACTION_PROFILE:-}" ]] || fail
    [[ -n "${BCODE_ACTION_BROKER_URL:-}" && -n "${BCODE_ACTION_BROKER_AUDIENCE:-}" ]] || fail
    unset BCODE_AUTH_BROKER_TOKEN BCODE_AUTH_BROKER_EXPIRES_AT
    # Local plugin-owned command: trusted empty bootstrap config, no daemon or secret argv.
    ( set -o noclobber; : > "$scratch/bootstrap.toml" ) || fail
    "$BCODE_ACTION_BINARY" --config-only "$scratch/bootstrap.toml" \
      --state-root "$scratch/bootstrap-state" --no-credential-discovery --json-errors auth-broker prepare \
      --directory "$scratch" --broker-url "$BCODE_ACTION_BROKER_URL" \
      --audience "$BCODE_ACTION_BROKER_AUDIENCE" --timeout-secs "$BCODE_ACTION_TIMEOUT" \
      > /dev/null 2> "$scratch/setup-diagnostics.json" || fail
    # The provider reads private lease files; tool subprocesses never inherit the lease.
    BCODE_ACTION_CONFIG="$scratch/broker.toml"
    BCODE_ACTION_PROFILE=github-broker
    unset BCODE_ACTION_API_KEY OPENAI_API_KEY OPENAI_ACCESS_TOKEN OPENAI_REFRESH_TOKEN
    ;;
  *) fail ;;
esac
printf '%s' "$BCODE_ACTION_PROMPT" > "$scratch/prompt.txt"
unset BCODE_ACTION_PROMPT
unset ACTIONS_ID_TOKEN_REQUEST_TOKEN ACTIONS_ID_TOKEN_REQUEST_URL
args=()
set +e
"$BCODE_ACTION_BINARY" --config-only "$BCODE_ACTION_CONFIG" --profile "$BCODE_ACTION_PROFILE" \
  --state-root "$scratch/state" --no-credential-discovery --json-errors "${args[@]}" \
  run --cwd "$workspace" --file "$scratch/prompt.txt" --timeout-secs "$BCODE_ACTION_TIMEOUT" \
  > "$scratch/result.json" 2> "$scratch/diagnostics.json"
status=$?
set -e
printf 'result=%s\nexit-code=%s\n' "$scratch/result.json" "$status" >> "$GITHUB_OUTPUT"
printf 'Bcode exited with status %s; private result retained for this job\n' "$status"
"$BCODE_ACTION_BINARY" --config-only "$BCODE_ACTION_CONFIG" --profile "$BCODE_ACTION_PROFILE" \
  --state-root "$scratch/state" --no-credential-discovery server stop-if-idle \
  > /dev/null 2>&1 || true
exit "$status"
