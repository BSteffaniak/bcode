#!/usr/bin/env bash
set -euo pipefail
root=$(mktemp -d)
trap 'rm -rf -- "$root"' EXIT
mkdir "$root/workspace" "$root/runner"
printf '[model]\n' > "$root/config.toml"
cat > "$root/bcode" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
if [[ " $* " == *' server stop-if-idle '* ]]; then exit 0; fi
[[ -z ${GITHUB_TOKEN:-} && -z ${GH_TOKEN:-} && -z ${ACTIONS_ID_TOKEN_REQUEST_TOKEN:-} ]]
[[ -z ${BCODE_ACTION_PROMPT:-} ]]
while (( $# )); do
  case "$1" in
    --file) file=$2; shift 2 ;;
    --state-root) state=$2; shift 2 ;;
    *) shift ;;
  esac
done
[[ ! -e "$state" ]]
[[ $(cat "$file") == '$(touch SHOULD_NOT_EXIST); arbitrary "prompt"' ]]
printf '{"schema_version":1,"outcome":"completed"}\n'
exit "${TEST_STATUS:-0}"
SH
chmod 700 "$root/bcode"
adapter=$(cd -- "$(dirname -- "$0")/.." && pwd)/.github/actions/run-bcode/run.sh
for status in 0 7; do
  set +e
  BCODE_ACTION_BINARY="$root/bcode" BCODE_ACTION_CONFIG="$root/config.toml" \
    BCODE_ACTION_PROFILE=test BCODE_ACTION_TIMEOUT=60 \
    BCODE_ACTION_PROMPT='$(touch SHOULD_NOT_EXIST); arbitrary "prompt"' \
    RUNNER_TEMP="$root/runner" GITHUB_WORKSPACE="$root/workspace" GITHUB_OUTPUT="$root/output-$status" \
    GITHUB_TOKEN=secret GH_TOKEN=secret ACTIONS_ID_TOKEN_REQUEST_TOKEN=secret TEST_STATUS="$status" \
    bash "$adapter"
  observed=$?
  set -e
  [[ "$observed" = "$status" ]]
  grep -q "^exit-code=$status$" "$root/output-$status"
  result=$(grep '^result=' "$root/output-$status"); result=${result#result=}
  grep -q '"schema_version":1' "$result"
  [[ ! -e "$root/workspace/SHOULD_NOT_EXIST" ]]
done
printf 'Bcode run adapter behavior tests passed\n'
