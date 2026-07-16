#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

violations=0

if rg -n 'SESSION_STATUS_POLL_INTERVAL|PERMISSION_POLL_INTERVAL|maybe_start_(session_status|permission)_poll|PermissionPollSchedule' packages/tui/src --glob '*.rs' >/tmp/bcode-tui-sync-polling.txt; then
  echo "Runtime architecture violation: TUI state synchronization must be snapshot/event-driven, not polling-driven." >&2
  cat /tmp/bcode-tui-sync-polling.txt >&2
  violations=1
fi

if rg -n 'RecvError::Lagged\(_\) => continue' packages/server/src packages/client/src --glob '*.rs' >/tmp/bcode-silent-event-lag.txt; then
  echo "Runtime architecture violation: event-stream lag must trigger explicit resynchronization, not silent continuation." >&2
  cat /tmp/bcode-silent-event-lag.txt >&2
  violations=1
fi

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
  rg -l 'PluginAutomation|plugin_automation|PluginStatusNote|clone_session_at_generation|automation_hold' \
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

for removed_symbol in HostModelNativeWebSearchRequest cancellation_path invocation_action_path ToolSchedulingContract ToolResourceClaim ToolResourceAccess; do
  if rg -n "\\b${removed_symbol}\\b" packages plugins examples --glob '*.rs' >/tmp/bcode-removed-runtime-symbol.txt; then
    echo "Runtime architecture violation: removed symbol ${removed_symbol} was reintroduced." >&2
    cat /tmp/bcode-removed-runtime-symbol.txt >&2
    violations=1
  fi
done

if rg -n 'definition\.side_effect == ToolSideEffect::ReadOnly|!definition\.requires_permission' packages/server/src/lib.rs >/tmp/bcode-server-parallel-heuristic.txt; then
  echo "Runtime architecture violation: server concurrency was tied to side-effect or permission metadata." >&2
  cat /tmp/bcode-server-parallel-heuristic.txt >&2
  violations=1
fi

if rg -n -i 'bcode\.(shell|filesystem|question|vim-edit|web-search)|shell-plugin|filesystem-plugin|question-plugin|vim-edit-plugin|web-search-plugin' \
  packages/agent-runtime/src packages/tool/src/contracts.rs >/tmp/bcode-core-test-domain-leakage.txt; then
  echo "Runtime architecture violation: tool-domain assumptions appeared in core runtime/contracts." >&2
  cat /tmp/bcode-core-test-domain-leakage.txt >&2
  violations=1
fi

if rg -n 'default_tool_execution_max_concurrency|max_concurrency: NonZeroUsize::new\(4\)|tool_execution\.max_concurrency\.get\(\)' \
  packages/config/src/lib.rs packages/tool/src/contracts.rs packages/server/src/lib.rs \
  >/tmp/bcode-default-concurrency-limit.txt; then
  echo "Runtime architecture violation: an artificial default tool concurrency limit was reintroduced." >&2
  cat /tmp/bcode-default-concurrency-limit.txt >&2
  violations=1
fi

legacy_executor_invocations="$(rg -n 'self\.executor\.execute_tool\(' packages/agent-runtime/src/lib.rs | wc -l | tr -d ' ')"
if [[ "$legacy_executor_invocations" != "1" ]]; then
  echo "Runtime architecture violation: expected one executor invocation confined to LegacyToolInvoker, found $legacy_executor_invocations." >&2
  violations=1
fi

if rg -n '\b(PathBuf|cwd|artifact_dir|cancellation_path|invocation_action_path)\b' packages/tool/src/contracts.rs >/tmp/bcode-preparation-transport-leakage.txt; then
  echo "Runtime architecture violation: transport/path fields appeared in canonical tool contracts." >&2
  cat /tmp/bcode-preparation-transport-leakage.txt >&2
  violations=1
fi

provider_tool_definition="$(
  awk '/^pub struct ToolDefinition \{/{capture=1} capture{print} capture && /^\}/{exit}' packages/model/src/lib.rs
)"
expected_provider_tool_definition="$(cat <<'EOF'
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}
EOF
)"
if [[ "$provider_tool_definition" != "$expected_provider_tool_definition" ]]; then
  echo "Runtime architecture violation: provider-visible tool definition gained policy/presentation metadata." >&2
  diff -u <(printf '%s\n' "$expected_provider_tool_definition") <(printf '%s\n' "$provider_tool_definition") >&2 || true
  violations=1
fi

prepared_invocation_fields="$(
  awk '/^pub struct PreparedToolInvocation \{/{capture=1; next} capture && /^\}/{exit} capture && /^    pub /{print}' packages/tool/src/contracts.rs
)"
expected_prepared_invocation_fields="$(cat <<'EOF'
    pub invocation: ToolInvocationDescriptor,
    pub preparation: ToolPreparationResponse,
EOF
)"
if [[ "$prepared_invocation_fields" != "$expected_prepared_invocation_fields" ]]; then
  echo "Runtime architecture violation: prepared invocation representation gained transport or adapter fields." >&2
  diff -u <(printf '%s\n' "$expected_prepared_invocation_fields") <(printf '%s\n' "$prepared_invocation_fields") >&2 || true
  violations=1
fi

runtime_production="$(mktemp)"
awk '/^#\[cfg\(test\)\]/{exit} {print}' packages/agent-runtime/src/lib.rs >"$runtime_production"
for primitive in 'invoker.prepare_tool(' 'authorization.authorize_batch(' '.invoke_tool(&prepared.tool'; do
  count="$(grep -F -c "$primitive" "$runtime_production")"
  if [[ "$count" != "1" ]]; then
    echo "Runtime architecture violation: canonical primitive '$primitive' has $count production call sites; expected one." >&2
    violations=1
  fi
done
rm -f "$runtime_production"

artifact_request_fields="$(
  awk '/^pub struct ToolArtifactWriteRequest \{/{capture=1; next} capture && /^\}/{exit} capture && /^    pub /{print}' packages/tool/src/contracts.rs
)"
expected_artifact_request_fields="$(cat <<'EOF'
    pub invocation_id: String,
    pub artifact_id: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
    pub metadata: serde_json::Value,
EOF
)"
if [[ "$artifact_request_fields" != "$expected_artifact_request_fields" ]]; then
  echo "Runtime architecture violation: bounded atomic artifact request shape changed unexpectedly." >&2
  diff -u <(printf '%s\n' "$expected_artifact_request_fields") <(printf '%s\n' "$artifact_request_fields") >&2 || true
  violations=1
fi
if rg -n 'Artifact(Allocate|Finalize)|artifact_(allocate|finalize)|ArtifactWriteChunk' packages/tool/src packages/agent-runtime/src packages/plugin-sdk/src >/tmp/bcode-artifact-v1-streaming.txt; then
  echo "Runtime architecture violation: unversioned allocation/finalize state was added to bounded artifact ABI v1." >&2
  cat /tmp/bcode-artifact-v1-streaming.txt >&2
  violations=1
fi

if rg -n 'stream::iter\(cancellations\)|for_each_concurrent\(' packages/server/src/runtime_work.rs \
  >/tmp/bcode-awaited-runtime-cleanup.txt; then
  echo "Runtime architecture violation: registered runtime cleanup is awaited at the local cancellation boundary." >&2
  cat /tmp/bcode-awaited-runtime-cleanup.txt >&2
  violations=1
fi
if ! rg -U 'for cancellation in cancellations \{\n[[:space:]]+tokio::spawn\(async move \{\n[[:space:]]+cancellation\.cancel\(\)\.await;' packages/server/src/runtime_work.rs >/dev/null; then
  echo "Runtime architecture violation: registered runtime cleanup handles are not detached after capture." >&2
  violations=1
fi

if ! rg -U 'let current_turn = close_session_turn\(state, session_id\)\.await;[\s\S]*acknowledge_cancel_command\(command, cancelled\);[\s\S]*finish_session_turn_cancellation\(' packages/server/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: cancellation acknowledgement no longer precedes durable bookkeeping/cleanup." >&2
  violations=1
fi

if ! awk '
  /^\[concurrency\]$/ { in_concurrency = 1; next }
  /^\[/ { in_concurrency = 0 }
  in_concurrency && $0 ~ /^type[[:space:]]*=[[:space:]]*"exclusive"/ { found = 1 }
  END { exit found ? 0 : 1 }
' examples/hello-plugin/bcode-plugin.toml; then
  echo "Runtime architecture violation: the non-reentrant hello ABI fixture must declare exclusive execution." >&2
  violations=1
fi

if (( violations != 0 )); then
  exit 1
fi

echo "loop/runtime domain-isolation guard passed"
