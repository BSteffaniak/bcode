#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

violations=0

expected_feature_files="$(cat <<'EOF'
packages/cli/src/lib.rs
packages/client/src/lib.rs
packages/ipc/src/lib.rs
packages/server/src/lib.rs
packages/session/models/src/lib.rs
packages/session/src/db.rs
packages/session/src/lib.rs
packages/session/src/persisted.rs
packages/tui/src/app.rs
packages/tui/src/chat_loop.rs
packages/tui/src/palette_flow.rs
packages/tui/src/transcript.rs
plugins/loop-plugin/src/lib.rs
EOF
)"
actual_feature_files="$(
  rg -l 'PluginAutomation|plugin_automation|PluginStatusNote|clone_session_at_generation|SESSION_STATUS_POLL_INTERVAL|automation_hold' \
    packages plugins --glob '*.rs' | sort
)"
if [[ "$actual_feature_files" != "$expected_feature_files" ]]; then
  echo "Loop architecture violation: specialized automation machinery spread to unexpected files." >&2
  diff -u <(printf '%s\n' "$expected_feature_files") <(printf '%s\n' "$actual_feature_files") >&2 || true
  violations=1
fi

expected_pascal_symbols="$(cat <<'EOF'
PluginAutomation
PluginAutomationExecutionPolicy
PluginAutomationHold
PluginAutomationHoldRequest
PluginAutomationHoldResponse
PluginAutomationOperation
PluginAutomationOperationLookupRequest
PluginAutomationOrigin
PluginAutomationSnapshot
PluginAutomationSnapshotRequest
PluginAutomationTurn
PluginAutomationTurnCompletion
PluginAutomationTurnDisposition
PluginAutomationTurnFinished
PluginAutomationTurnRequest
PluginAutomationTurnStarted
EOF
)"
actual_pascal_symbols="$(
  rg -o '\bPluginAutomation[A-Za-z0-9_]*\b' packages plugins --glob '*.rs' \
    | sed 's/.*://' | sort -u
)"
if [[ "$actual_pascal_symbols" != "$expected_pascal_symbols" ]]; then
  echo "Loop architecture violation: the temporary PluginAutomation symbol set changed." >&2
  diff -u <(printf '%s\n' "$expected_pascal_symbols") <(printf '%s\n' "$actual_pascal_symbols") >&2 || true
  violations=1
fi

expected_snake_symbols="$(cat <<'EOF'
plugin_automation_active
plugin_automation_generation
plugin_automation_holds
plugin_automation_lock
plugin_automation_locks
plugin_automation_operation_events
plugin_automation_origin_labels_only_the_matching_user_turn
plugin_automation_policies
plugin_automation_preflight_disposition
plugin_automation_snapshot
plugin_automation_turn_finished
plugin_automation_turn_started
EOF
)"
actual_snake_symbols="$(
  rg -o '\bplugin_automation_[A-Za-z0-9_]*\b' packages plugins --glob '*.rs' \
    | sed 's/.*://' | sort -u
)"
if [[ "$actual_snake_symbols" != "$expected_snake_symbols" ]]; then
  echo "Loop architecture violation: the temporary plugin_automation symbol set changed." >&2
  diff -u <(printf '%s\n' "$expected_snake_symbols") <(printf '%s\n' "$actual_snake_symbols") >&2 || true
  violations=1
fi

production_core_sources="$(mktemp)"
trap 'rm -f "$production_core_sources"' EXIT
for file in \
  packages/agent-runtime/src/lib.rs \
  packages/client/src/lib.rs \
  packages/ipc/src/lib.rs \
  packages/plugin-sdk/src/lib.rs \
  packages/server/src/lib.rs \
  packages/session/models/src/lib.rs \
  packages/session/src/lib.rs \
  packages/tui/src/app.rs \
  packages/tui/src/chat_loop.rs \
  packages/tui/src/composer_flow.rs \
  packages/tui/src/slash_palette.rs \
  packages/tui/src/transcript.rs
do
  awk '/^#\[cfg\(test\)\]/{exit} {print FILENAME ":" FNR ":" $0}' "$file" >> "$production_core_sources"
done
if rg -n 'bcode\.(loop|filesystem|shell|question|worktree|vim-edit|web-search|ocr|document)|LoopPhase|EvaluatorPhase|IterationPhase' "$production_core_sources" \
  >/tmp/bcode-loop-domain-leakage.txt; then
  echo "Loop architecture violation: loop-domain knowledge appeared in generic production code." >&2
  cat /tmp/bcode-loop-domain-leakage.txt >&2
  violations=1
fi

# These three provider-ID branches are existing domain leaks recorded in the migration ledger.
# Freeze them until provider capabilities replace them; do not permit another concrete-ID branch.
provider_branch_count="$(
  rg -n 'Some\("bcode\.(openai-compatible|bedrock)"\) =>' packages/server/src/lib.rs \
    | awk -F: '$1 < 18000 {count += 1} END {print count + 0}'
)"
if [[ "$provider_branch_count" != "3" ]]; then
  echo "Runtime architecture violation: expected exactly three recorded provider-ID branches, found $provider_branch_count." >&2
  violations=1
fi

clone_files="$(rg -l 'clone_session_at_generation' packages plugins --glob '*.rs' | sort)"
expected_clone_files="$(cat <<'EOF'
packages/client/src/lib.rs
packages/server/src/lib.rs
packages/session/src/lib.rs
plugins/loop-plugin/src/lib.rs
EOF
)"
if [[ "$clone_files" != "$expected_clone_files" ]]; then
  echo "Loop architecture violation: generation-specific cloning spread to unexpected files." >&2
  diff -u <(printf '%s\n' "$expected_clone_files") <(printf '%s\n' "$clone_files") >&2 || true
  violations=1
fi

loop_default_clients="$(rg -n 'BcodeClient::default_endpoint' plugins/loop-plugin/src/lib.rs | wc -l | tr -d ' ')"
if [[ "$loop_default_clients" != "8" ]]; then
  echo "Loop architecture violation: expected eight recorded direct loop daemon-client constructions, found $loop_default_clients." >&2
  violations=1
fi

native_search_implementations="$(
  rg -l 'fn (native_web_search|native_web_search_inner)\b' packages plugins --glob '*.rs' \
    | sort
)"
if [[ -n "$native_search_implementations" ]] && grep -Ev '^plugins/[^/]*provider-plugin/src/' <<<"$native_search_implementations" >/tmp/bcode-native-search-domain-leakage.txt; then
  echo "Runtime architecture violation: provider-native search implementation escaped provider plugins." >&2
  cat /tmp/bcode-native-search-domain-leakage.txt >&2
  violations=1
fi

for removed_symbol in HostModelNativeWebSearchRequest cancellation_path invocation_action_path; do
  if rg -n "\\b${removed_symbol}\\b" packages plugins examples --glob '*.rs' >/tmp/bcode-removed-runtime-symbol.txt; then
    echo "Runtime architecture violation: removed symbol ${removed_symbol} was reintroduced." >&2
    cat /tmp/bcode-removed-runtime-symbol.txt >&2
    violations=1
  fi
done

if (( violations != 0 )); then
  exit 1
fi

echo "loop/runtime domain-isolation guard passed"
