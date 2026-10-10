#!/usr/bin/env bash
# Trusted-code automation adapter. It does not sandbox tools or hide the API key from them.
set -euo pipefail
umask 077
fail() { echo 'Bcode run setup failed; verify trusted inputs and runner paths' >&2; exit 1; }
[[ "${BCODE_ACTION_BINARY:-}" = /* && "${BCODE_ACTION_CONFIG:-}" = /* && "${RUNNER_TEMP:-}" = /* && "${GITHUB_WORKSPACE:-}" = /* && "${GITHUB_OUTPUT:-}" = /* ]] || fail
[[ -x "$BCODE_ACTION_BINARY" && -f "$BCODE_ACTION_CONFIG" && ! -L "$BCODE_ACTION_CONFIG" ]] || fail
[[ "${BCODE_ACTION_TIMEOUT:-}" =~ ^[0-9]{1,5}$ ]] || fail
(( 10#$BCODE_ACTION_TIMEOUT >= 1 && 10#$BCODE_ACTION_TIMEOUT <= 86400 )) || fail
[[ -n "${BCODE_ACTION_PROFILE:-}" ]] || fail
[[ ! -L "$GITHUB_OUTPUT" && ( ! -e "$GITHUB_OUTPUT" || -f "$GITHUB_OUTPUT" ) ]] || fail
workspace=$(cd -- "$GITHUB_WORKSPACE" && pwd -P)
parent=$(cd -- "$RUNNER_TEMP" && pwd -P)
for path in "$parent" "$(cd -- "$(dirname -- "$BCODE_ACTION_CONFIG")" && pwd -P)" "$(cd -- "$(dirname -- "$BCODE_ACTION_BINARY")" && pwd -P)"; do
  [[ "$path" != "$workspace" && "$path" != "$workspace/"* && "$path" != *$'\n'* && "$path" != *$'\r'* ]] || fail
done
scratch=$(mktemp -d "$parent/bcode-run.XXXXXXXX")
printf '%s' "$BCODE_ACTION_PROMPT" > "$scratch/prompt.txt"
unset BCODE_ACTION_PROMPT
# State, home and config live outside the checkout. Credential values remain in
# the environment only when explicitly supplied by trusted workflow policy.
mkdir "$scratch/home"
export HOME="$scratch/home" XDG_CONFIG_HOME="$scratch/home/config" XDG_DATA_HOME="$scratch/home/data" XDG_STATE_HOME="$scratch/home/state" CI=true BCODE_NO_ONBOARD=1
unset BCODE_CONFIG BCODE_CONTEXT BCODE_PROFILE GITHUB_TOKEN GH_TOKEN ACTIONS_ID_TOKEN_REQUEST_TOKEN ACTIONS_ID_TOKEN_REQUEST_URL
set +e
"$BCODE_ACTION_BINARY" --config-only "$BCODE_ACTION_CONFIG" --profile "$BCODE_ACTION_PROFILE" \
  --state-root "$scratch/state" --no-credential-discovery --json-errors \
  run --cwd "$workspace" --file "$scratch/prompt.txt" --timeout-secs "$BCODE_ACTION_TIMEOUT" \
  > "$scratch/result.json" 2> "$scratch/diagnostics.json"
status=$?
set -e
# Do not print raw diagnostics, model text or workflow commands into the CI log.
printf 'result=%s\nexit-code=%s\n' "$scratch/result.json" "$status" >> "$GITHUB_OUTPUT"
printf 'Bcode exited with status %s; private result retained for this job\n' "$status"
# Only stop the matching idle daemon in this private state; never kill/replay an
# unknown active turn. Ephemeral runner disposal handles final cleanup.
"$BCODE_ACTION_BINARY" --config-only "$BCODE_ACTION_CONFIG" --profile "$BCODE_ACTION_PROFILE" \
  --state-root "$scratch/state" --no-credential-discovery server stop-if-idle \
  > /dev/null 2>&1 || true
exit "$status"
