#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

violations=0

if rg -n -i 'bcode\.shell|shell[_ .-]?recording|terminal[_ .-]?pty|pty[_ .-]?stream' packages/tui/src/artifact_stream.rs >/tmp/bcode-artifact-stream-domain-leak.txt; then
  echo "Runtime architecture violation: generic TUI artifact transport contains shell-domain knowledge." >&2
  cat /tmp/bcode-artifact-stream-domain-leak.txt >&2
  violations=1
fi

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

if rg -n '\bPluginAutomation[A-Za-z0-9_]*\b|\bplugin_automation_[A-Za-z0-9_]*\b|automation_hold' \
  packages plugins --glob '*.rs' \
  | rg -v '"plugin_automation_turn_(started|finished)"' \
  >/tmp/bcode-removed-plugin-automation.txt; then
  echo "Loop architecture violation: removed specialized PluginAutomation machinery was reintroduced." >&2
  cat /tmp/bcode-removed-plugin-automation.txt >&2
  violations=1
fi

if rg -n 'turn_tool_policies|FollowupCommand::(UserMessage|AdmittedTurn|ContinueFromUserEvent)' \
  packages/server/src/lib.rs >/tmp/bcode-parallel-turn-admission-paths.txt; then
  echo "Runtime architecture violation: turn execution policy or ordinary messages bypass durable admitted events." >&2
  cat /tmp/bcode-parallel-turn-admission-paths.txt >&2
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
EOF
)"
if [[ "$clone_files" != "$expected_clone_files" ]]; then
  echo "Loop architecture violation: generation-specific cloning spread to unexpected files." >&2
  diff -u <(printf '%s\n' "$expected_clone_files") <(printf '%s\n' "$clone_files") >&2 || true
  violations=1
fi

loop_default_clients="$(rg -n 'BcodeClient::default_endpoint' plugins/loop-plugin/src/lib.rs | wc -l | tr -d ' ')"
if [[ "$loop_default_clients" != "4" ]]; then
  echo "Loop architecture violation: expected four recorded direct loop daemon-client constructions, found $loop_default_clients." >&2
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

if rg -n '\b(ToolExecutor|LegacyToolInvoker)\b|self\.executor\.execute_tool\(' packages/agent-runtime/src/lib.rs >/tmp/bcode-legacy-tool-executor.txt; then
  echo "Runtime architecture violation: legacy executor compatibility reappeared in AgentRuntime." >&2
  cat /tmp/bcode-legacy-tool-executor.txt >&2
  violations=1
fi

if rg -n 'legacy_side_effect|legacy_policy_metadata|automation_policy_allows_tool' \
  packages/server/src/lib.rs packages/agent-profile/src/lib.rs >/tmp/bcode-legacy-policy-projection.txt; then
  echo "Runtime architecture violation: server policy reintroduced legacy side-effect projection." >&2
  cat /tmp/bcode-legacy-policy-projection.txt >&2
  violations=1
fi

if rg -n 'request\.(arguments|policy|side_effect)|\bToolArgumentKind\b|\bToolSideEffect\b' \
  packages/agent-policy/src/lib.rs >/tmp/bcode-agent-policy-argument-inference.txt; then
  echo "Runtime architecture violation: agent policy reintroduced raw argument or side-effect inference." >&2
  cat /tmp/bcode-agent-policy-argument-inference.txt >&2
  violations=1
fi

if rg -U 'SkillToolPolicyRequest \{[\s\S]{0,120}tool: (definition|tool\.clone\(\))' \
  packages/server/src/lib.rs packages/skill/src/lib.rs >/tmp/bcode-skill-definition-policy.txt; then
  echo "Runtime architecture violation: skill policy reintroduced full ToolDefinition evaluation." >&2
  cat /tmp/bcode-skill-definition-policy.txt >&2
  violations=1
fi

if rg -n '\b(PathBuf|cwd|artifact_dir|cancellation_path|invocation_action_path)\b' packages/tool/src/contracts.rs >/tmp/bcode-preparation-transport-leakage.txt; then
  echo "Runtime architecture violation: transport/path fields appeared in canonical tool contracts." >&2
  cat /tmp/bcode-preparation-transport-leakage.txt >&2
  violations=1
fi

runtime_permission_context_fields="$(
  awk '/^pub struct RuntimePermissionContext \{/{capture=1; next} capture && /^\}/{exit} capture && /^    pub /{print}' packages/agent-runtime/src/lib.rs
)"
expected_runtime_permission_context_fields="$(cat <<'EOF'
    pub session_id: SessionId,
    pub agent_id: String,
EOF
)"
if [[ "$runtime_permission_context_fields" != "$expected_runtime_permission_context_fields" ]]; then
  echo "Runtime architecture violation: canonical permission context gained path or domain-policy fields." >&2
  diff -u <(printf '%s\n' "$expected_runtime_permission_context_fields") <(printf '%s\n' "$runtime_permission_context_fields") >&2 || true
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

for legacy_sdk_loop in 'run_provider_tool_loop_in_scope' 'append_provider_tool_calls' 'append_tool_results' 'ToolRoundState::new(request.max_tool_rounds)' 'ScopedAgentEventSink' 'unbounded_channel();'; do
  if grep -F "$legacy_sdk_loop" packages/bcode/src/lib.rs >/dev/null; then
    echo "Runtime architecture violation: SDK reintroduced duplicate provider/tool loop fragment '$legacy_sdk_loop'." >&2
    violations=1
  fi
done
if ! rg -U 'fn run_provider_tool_loop<P>\([\s\S]*\.run_provider_tool_loop\(' packages/bcode/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: SDK provider/tool orchestration no longer delegates to AgentRuntime." >&2
  violations=1
fi
if ! rg -U 'pub async fn run_provider_tool_loop_in_scope[\s\S]*run_planned_provider_round[\s\S]*execute_prepared_tool_batch_with_host_context' packages/agent-runtime/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: canonical provider planning/tool continuation loop was removed from AgentRuntime." >&2
  violations=1
fi
if ! rg -U 'provider_round_planner: Arc<dyn ProviderRoundPlanner>[\s\S]*\.run_provider_tool_loop\([\s\S]*self\.provider_round_planner\.as_ref\(\)' packages/bcode/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: SDK provider recovery no longer routes through the canonical planner seam." >&2
  violations=1
fi

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
if ! rg -U 'for \(cleanup_work_id, kind, cancellation\) in cancellations \{[\s\S]{0,160}tokio::spawn\(async move \{[\s\S]{0,160}let result = cancellation\.cancel\(\)\.await;' packages/server/src/runtime_work.rs >/dev/null; then
  echo "Runtime architecture violation: registered runtime cleanup handles are not detached after capture." >&2
  violations=1
fi

if ! grep -F 'parallel_group_cancellation_returns_exactly_one_outcome_per_invocation' packages/agent-runtime/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: mixed active/queued cancellation cardinality proof was removed." >&2
  violations=1
fi

if ! rg -U 'let service = invoke_host_provider_native_search_response\([\s\S]*tokio::select! \{[\s\S]*cancel_state\.cancelled\(\)[\s\S]*ToolInvocationServiceResolution::Cancelled' packages/server/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: server nested service routing is not cancellation-bounded." >&2
  violations=1
fi
if ! rg -U 'after_publish\(\);[\s\S]*cancel_state\.is_cancelled\(\)[\s\S]*remove_file\(&destination\)[\s\S]*"cancelled"' packages/server/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: cancelled artifact publication is not rolled back." >&2
  violations=1
fi

if ! rg -U 'let result = cancellation\.cancel\(\)\.await;[\s\S]*detached runtime cleanup completed[\s\S]*detached runtime cleanup failed' packages/server/src/runtime_work.rs >/dev/null; then
  echo "Runtime architecture violation: detached runtime cleanup completion/failure diagnostics were removed." >&2
  violations=1
fi
if ! rg -U 'let result = plugins[\s\S]*OP_CANCEL_TURN[\s\S]*detached provider cleanup completed[\s\S]*detached provider cleanup failed' packages/server/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: detached provider cleanup completion/failure diagnostics were removed." >&2
  violations=1
fi

if ! rg -U 'fn dispatch_provider_turn_cleanup\([\s\S]*tokio::spawn\(async move[\s\S]*OP_CANCEL_TURN' packages/server/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: provider cleanup is no longer detached from local cancellation." >&2
  violations=1
fi

if ! rg -U 'let current_turn = close_session_turn\(state, session_id\)\.await;[\s\S]*acknowledge_cancel_command\(command, cancelled\);[\s\S]*finish_session_turn_cancellation\(' packages/server/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: cancellation acknowledgement no longer precedes durable bookkeeping/cleanup." >&2
  violations=1
fi

if ! rg -U 'persisted results must retain provider order despite reverse completion' packages/server/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: production overlap test no longer proves provider-ordered persistence." >&2
  violations=1
fi

if ! rg -U 'SlashCommandOutcome::CancelTurn[\s\S]*TuiEffect::CancelTurn[\s\S]*set_cancelling\(\)' packages/tui/src/composer_flow.rs >/dev/null; then
  echo "Runtime architecture violation: composer cancellation does not enter immediate Cancelling UI state." >&2
  violations=1
fi
if ! rg -U 'Ok\(true\)[\s\S]*set_cancelling\(\)[\s\S]*turn cancellation requested' packages/tui/src/chat_loop.rs >/dev/null; then
  echo "Runtime architecture violation: positive cancellation acknowledgement does not preserve Cancelling UI state." >&2
  violations=1
fi

if ! grep -F 'runtime_status_tracks_plugin_local_queueing' packages/plugin/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: exclusive adapter queueing/serialization proof was removed." >&2
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

if rg -n 'bcode_parallel_tool_calls' packages plugins examples --glob '*.rs' >/tmp/bcode-parallel-tool-metadata.txt; then
  echo "Runtime architecture violation: provider parallel intent regressed to transitional metadata." >&2
  cat /tmp/bcode-parallel-tool-metadata.txt >&2
  violations=1
fi
if ! rg -U 'pub struct ModelTurnRequest \{[\s\S]*pub tool_call_policy: ToolCallRequestPolicy' packages/model/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: model turn requests lost typed tool-call policy." >&2
  violations=1
fi
if ! rg -U 'parallel_tool_calls:[\s\S]{0,180}request\.tool_call_policy\.parallel' plugins/openai-compatible-provider-plugin/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: provider request mapping bypasses typed tool-call policy." >&2
  violations=1
fi

if ! rg -U 'pub struct PermissionSummary \{[\s\S]{0,400}pub tool_call_id: String,[\s\S]{0,400}pub batch: Option<PermissionBatchCorrelation>' packages/ipc/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: permission summaries lost call/batch correlation." >&2
  violations=1
fi
if ! rg -U 'PermissionBatchCorrelation \{[\s\S]{0,220}batch_id:[\s\S]{0,220}call_index: request\.index,[\s\S]{0,220}call_count: self\.call_count' packages/server/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: server authorization no longer correlates permission checkpoints with complete batches." >&2
  violations=1
fi

if ! rg -U 'ResolvePermissionBatch \{[\s\S]{0,120}batch_id: String,[\s\S]{0,120}approved: bool' packages/ipc/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: safe batch permission resolution request was removed." >&2
  violations=1
fi
if ! rg -U 'batch_decision = batch\.decision\.lock\(\)\.await;[\s\S]{0,220}\*batch_decision = Some\(approved\)[\s\S]{0,900}batch\.batch_id == batch_id' packages/server/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: batch permission resolution is not latched and batch-scoped." >&2
  violations=1
fi

if ! rg -U 'close_session_turn\(state, session_id\)\.await;[\s\S]{0,160}cancel_pending_permissions_for_session\(state, session_id\)\.await;[\s\S]{0,500}acknowledge_cancel_command' packages/server/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: turn cancellation no longer closes permission checkpoints before acknowledgement." >&2
  violations=1
fi
if ! rg -U 'PermissionResolved[\s\S]{0,500}snapshot\.permissions\.remove' packages/session-view/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: resolved permission checkpoints remain active in renderer-neutral session view state." >&2
  violations=1
fi

if ! rg -U 'runtime_work\.cleanup_total[\s\S]{0,500}runtime_work\.cleanup_duration_ms' packages/server/src/runtime_work.rs >/dev/null ||
   ! rg -U 'provider\.cleanup_total[\s\S]{0,500}provider\.cleanup_duration_ms' packages/server/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: detached runtime/provider cleanup diagnostics are incomplete." >&2
  violations=1
fi

if rg -U 'labels\.insert\([\s\S]{0,120}"(tool_call_id|call_id|batch_id|invocation_id|permission_id)"' packages plugins --glob '*.rs' >/dev/null; then
  echo "Runtime architecture violation: aggregate metric labels contain unique call or batch identity." >&2
  violations=1
fi

if rg -n 'tool_call_policy\.parallel = options\.parallel|tool_call_policy\.parallel = parallel_tool_calls' packages/agent-runtime/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: canonical runtime upgrades negotiated provider parallel capability from scheduler configuration." >&2
  violations=1
fi
if ! rg -n 'tool_call_policy\.parallel &= options\.parallel' packages/agent-runtime/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: canonical runtime lost sequential fallback for negotiated parallel policy." >&2
  violations=1
fi

if ! grep -F 'completed_tool_calls_preserve_provider_order_and_exact_ids' plugins/openai-compatible-provider-plugin/src/lib.rs >/dev/null ||
   ! grep -F 'completed_tool_calls_preserve_bedrock_order_and_exact_ids' plugins/bedrock-provider-plugin/src/lib.rs >/dev/null ||
   ! grep -F 'canonical_loop_runs_provider_batch_and_ordered_continuation' packages/agent-runtime/src/lib.rs >/dev/null ||
   ! grep -F 'server_same_batch_shell_calls_overlap_after_complete_authorization' packages/server/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: provider order/call identity conformance coverage is incomplete." >&2
  violations=1
fi

if ! grep -F '# Scheduler invariants' packages/agent-runtime/src/lib.rs >/dev/null ||
   ! grep -F '# Scope invariants' packages/agent-runtime/src/turn.rs >/dev/null ||
   ! grep -F '# Channel invariants' packages/agent-runtime/src/turn.rs >/dev/null; then
  echo "Runtime architecture violation: canonical scheduler/scope/channel invariants are no longer documented next to code." >&2
  violations=1
fi

if ! grep -F 'runtime_work_status_label_preserves_semantic_activity' packages/session-view/models/src/tests.rs >/dev/null ||
   ! grep -F 'authoritative_runtime_work_snapshot_drives_tui_activity' packages/tui/src/app.rs >/dev/null ||
   ! grep -F 'runtime_work_terminal_state_leaves_sibling_active_and_rejects_late_revival' packages/session-view/src/lib.rs >/dev/null ||
   ! grep -F 'terminal_runtime_work_without_visible_start_is_history_only' packages/session-view/src/lib.rs >/dev/null ||
   ! grep -F 'web_projection_keeps_active_sibling_and_does_not_revive_terminal_work' packages/web-render/src/lib.rs >/dev/null ||
   ! grep -F 'runtime_work_activity_is_excluded_from_model_context' packages/server/src/lib.rs >/dev/null ||
   ! grep -F 'late_stream_events_cannot_revive_finished_tool_projection' packages/session/models/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: grouped activity or terminal late-event suppression coverage was removed." >&2
  violations=1
fi

if ! grep -F 'transient_contribution_bypasses_persistence_but_remains_observable_and_published' packages/agent-runtime/src/turn.rs >/dev/null ||
   ! grep -F 'durable_contribution_requires_persistence_admission' packages/agent-runtime/src/turn.rs >/dev/null ||
   ! grep -F 'sdk_builder_persists_only_durable_contributions' packages/bcode/tests/builder_adapters.rs >/dev/null; then
  echo "Runtime architecture violation: contribution persistence boundary coverage was removed." >&2
  violations=1
fi

if ! grep -F 'presentation_and_exchange_payloads_are_excluded_from_model_context' packages/server/src/lib.rs >/dev/null ||
   ! grep -F 'legacy_stream_presentation_payload_is_excluded_from_model_context' packages/server/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: presentation/exchange model-context exclusion coverage was removed." >&2
  violations=1
fi

if ! grep -F 'server_question_exchange_completes_original_plugin_invocation' packages/server/src/lib.rs >/dev/null ||
   ! grep -F 'exchange_lifecycle_projects_opaque_active_state_and_terminal_resolution' packages/session-view/src/lib.rs >/dev/null ||
   ! grep -F 'ToolExchangeRequested' packages/session/src/persisted.rs >/dev/null ||
   ! grep -F 'ToolExchangeResolved' packages/session/src/persisted.rs >/dev/null; then
  echo "Runtime architecture violation: neutral durable exchange lifecycle coverage was removed." >&2
  violations=1
fi

if ! grep -F 'server_persists_filesystem_progress_as_neutral_lifecycle_only' packages/server/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: production neutral plugin progress persistence coverage was removed." >&2
  violations=1
fi

if rg -n 'ToolInvocationStreamEvent' \
  plugins/ocr-plugin/src plugins/filesystem-plugin/src plugins/document-plugin/src \
  plugins/web-search-plugin/src --glob '*.rs' >/tmp/bcode-migrated-progress-streams.txt; then
  echo "Runtime architecture violation: migrated OCR/filesystem/document progress reintroduced legacy stream events." >&2
  cat /tmp/bcode-migrated-progress-streams.txt >&2
  violations=1
fi

for plugin_source in \
  plugins/ocr-plugin/src/lib.rs \
  plugins/filesystem-plugin/src/lib.rs \
  plugins/document-plugin/src/lib.rs \
  plugins/web-search-plugin/src/lib.rs; do
  if ! grep -F 'progress_uses_neutral_invocation_lifecycle_contract' "$plugin_source" >/dev/null; then
    echo "Runtime architecture violation: neutral progress lifecycle coverage missing from $plugin_source." >&2
    violations=1
  fi
done

if rg -n 'ToolInvocationStreamEvent::(Started|Status|Finished)|emit_tool_status' \
  plugins/shell-plugin/src --glob '*.rs' >/tmp/bcode-shell-legacy-lifecycle.txt; then
  echo "Runtime architecture violation: shell plugin reintroduced legacy invocation lifecycle stream events." >&2
  cat /tmp/bcode-shell-legacy-lifecycle.txt >&2
  violations=1
fi

if ! grep -F 'static_and_dynamic_shell_contributions_are_observable_headlessly' packages/bcode/tests/embedded_scoped_plugin.rs >/dev/null ||
   ! grep -F 'server_persists_shell_owned_contribution_opaquely' packages/server/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: shell generic lifecycle/contribution conformance coverage was removed." >&2
  violations=1
fi

if ! grep -F 'orchestration_emits_exactly_one_started_and_terminal_lifecycle_per_invocation' packages/agent-runtime/src/lib.rs >/dev/null ||
   ! grep -F 'tool_owned_started_and_terminal_lifecycle_stages_are_rejected' packages/agent-runtime/src/turn.rs >/dev/null ||
   ! grep -F 'neutral_batch_cancellation_prevents_queued_start' packages/agent-runtime/src/lib.rs >/dev/null ||
   ! grep -F 'server_tool_cancellation_persists_exact_generic_lifecycle' packages/server/src/lib.rs >/dev/null ||
   ! grep -F 'server_tool_error_persists_failed_generic_lifecycle' packages/server/src/lib.rs >/dev/null ||
   ! grep -F 'projection_history_pages_cross_real_ipc_bidirectionally' packages/server/src/lib.rs >/dev/null ||
   ! grep -F 'generic_lifecycle_drives_tui_activity_until_terminal_event' packages/tui/src/app.rs >/dev/null ||
   ! grep -F 'web_preserves_compact_single_tool_activity_until_terminal_event' packages/web-render/src/lib.rs >/dev/null ||
   ! grep -F 'legacy_tool_stream_lifecycle_events_are_not_newly_persisted' packages/server/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: orchestration-owned lifecycle coverage was removed." >&2
  violations=1
fi

if rg -n '\bPluginInvocationAction\b|PluginInvocationActionAccepted|send_plugin_invocation_action|invocation_action_file|cancellation_file' \
  packages plugins --glob '*.rs' >/tmp/bcode-legacy-invocation-action.txt; then
  echo "Runtime architecture violation: legacy plugin invocation action transport was reintroduced." >&2
  cat /tmp/bcode-legacy-invocation-action.txt >&2
  violations=1
fi

input_model_declarations="$(rg -l 'pub (struct ToolInvocationInput|enum ToolInvocationInputResolution)' packages --glob '*.rs' | sort)"
if [[ "$input_model_declarations" != "packages/tool/models/src/lib.rs" ]]; then
  echo "Runtime architecture violation: invocation input DTOs must be declared only in the tool-models leaf crate." >&2
  printf '%s\n' "$input_model_declarations" >&2
  violations=1
fi

if ! grep -F 'invocation_input_request_round_trips_with_opaque_payload' packages/ipc/src/lib.rs >/dev/null ||
   ! grep -F 'generic_invocation_inputs_enqueue_opaque_bounded_payloads' packages/server/src/lib.rs >/dev/null ||
   ! grep -F 'shell_visual_adapter_owns_resize_input_payload_and_identity' plugins/shell-plugin/src/shell_run_tui.rs >/dev/null; then
  echo "Runtime architecture violation: neutral invocation input transport coverage was removed." >&2
  violations=1
fi

if ! grep -F 'batch_size = calls.len()' packages/agent-runtime/src/lib.rs >/dev/null ||
   ! grep -F 'provider_round,' packages/agent-runtime/src/lib.rs >/dev/null ||
   ! grep -F 'configured_max_concurrency = ?options.max_concurrency.map(NonZeroUsize::get)' packages/agent-runtime/src/lib.rs >/dev/null ||
   ! grep -F 'observed_concurrency = execution.observed_concurrency' packages/agent-runtime/src/lib.rs >/dev/null ||
   ! grep -F 'batch_concurrency_observation_tracks_peak_and_releases_active_work' packages/agent-runtime/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: canonical batch concurrency observability was removed." >&2
  violations=1
fi

if ! grep -F 'Some("sequential_mode")' packages/agent-runtime/src/lib.rs >/dev/null ||
   ! grep -F 'Some("concurrency_bound")' packages/agent-runtime/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: canonical scheduler serialization reason tracing was removed." >&2
  violations=1
fi

if ! grep -F 'plugin_serialization_reason(PluginConcurrency::Exclusive)' packages/plugin/src/lib.rs >/dev/null ||
   ! grep -F 'plugin service invocation serialized by host' packages/plugin/src/lib.rs >/dev/null ||
   ! grep -F 'plugin_serialization_reason_is_only_reentrancy_exclusivity' packages/plugin/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: plugin-host reentrancy serialization tracing was removed." >&2
  violations=1
fi

if ! grep -F 'queued_cancellations = queued' packages/agent-runtime/src/lib.rs >/dev/null ||
   ! grep -F 'running_cancellations = running' packages/agent-runtime/src/lib.rs >/dev/null ||
   ! grep -F 'discarded_late_events = scope.control().discarded_normal_event_count()' packages/agent-runtime/src/lib.rs >/dev/null ||
   ! grep -F 'assert_eq!(control.queued_cancellation_count(), 1)' packages/agent-runtime/src/lib.rs >/dev/null ||
   ! grep -F 'assert_eq!(control.running_cancellation_count(), 1)' packages/agent-runtime/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: neutral cancellation/discard accounting was removed." >&2
  violations=1
fi

if ! grep -F 'RuntimePhaseDuration::start("preparation", Some(provider_round))' packages/agent-runtime/src/lib.rs >/dev/null ||
   ! grep -F 'RuntimePhaseDuration::start("authorization", Some(provider_round))' packages/agent-runtime/src/lib.rs >/dev/null ||
   ! grep -F 'RuntimePhaseDuration::start("batch", Some(provider_round))' packages/agent-runtime/src/lib.rs >/dev/null ||
   ! grep -F 'RuntimePhaseDuration::start("invocation", None)' packages/agent-runtime/src/lib.rs >/dev/null ||
   ! grep -F 'InvocationOperationDuration::start("exchange")' packages/agent-runtime/src/turn.rs >/dev/null ||
   ! grep -F 'InvocationOperationDuration::start("input_wait")' packages/agent-runtime/src/turn.rs >/dev/null ||
   ! grep -F 'InvocationOperationDuration::start("service")' packages/agent-runtime/src/turn.rs >/dev/null ||
   ! grep -F 'InvocationOperationDuration::start("artifact")' packages/agent-runtime/src/turn.rs >/dev/null ||
   ! grep -F '"neutral turn cancellation signalled"' packages/agent-runtime/src/turn.rs >/dev/null ||
   ! grep -F '"plugin.queue_wait.duration_ms"' packages/plugin/src/lib.rs >/dev/null ||
   ! grep -F '"plugin.resource_wait.duration_ms"' packages/plugin/src/lib.rs >/dev/null ||
   ! grep -F '"runtime_work.cleanup_duration_ms"' packages/server/src/runtime_work.rs >/dev/null ||
   ! grep -F '"provider.cleanup_duration_ms"' packages/server/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: neutral runtime phase duration diagnostics were removed." >&2
  violations=1
fi

if ! grep -F 'pub struct ParallelToolCallCapabilities' packages/model/src/lib.rs >/dev/null ||
   ! grep -F 'requested && self.provider && self.model && self.canonical_runtime' packages/model/src/lib.rs >/dev/null ||
   ! grep -F 'parallel_tool_policy_requires_intent_provider_model_and_canonical_runtime' packages/model/src/lib.rs >/dev/null ||
   ! grep -F 'provider_registry_negotiates_parallel_only_when_provider_and_model_support_it' packages/bcode/tests/provider_defaults.rs >/dev/null ||
   ! grep -F 'sdk_parallel_signal_falls_back_when_one_capability_is_missing' packages/bcode/tests/provider_tool_loop.rs >/dev/null ||
   ! grep -F 'changing_model_after_capability_resolution_invalidates_parallel_signal' packages/bcode/tests/provider_tool_loop.rs >/dev/null ||
   ! grep -F 'duplicate_server_loop_never_advertises_parallel_before_canonical_delegation' packages/server/src/lib.rs >/dev/null ||
   ! grep -F 'unknown_model_is_not_upgraded_to_parallel_tool_calls' packages/model-catalog/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: parallel tool-call capability negotiation was weakened." >&2
  violations=1
fi

if rg -n 'tool_call_policy: bcode_model::ToolCallRequestPolicy \{[[:space:]]*$' \
  packages/bcode/src/lib.rs packages/server/src/lib.rs >/tmp/bcode-direct-parallel-policy.txt; then
  echo "Runtime architecture violation: production request builders bypass typed parallel capability negotiation." >&2
  cat /tmp/bcode-direct-parallel-policy.txt >&2
  violations=1
fi

if ! grep -F 'parallel_tool_calls: bool' packages/model-catalog/models/src/lib.rs >/dev/null ||
   ! grep -F 'ModelCapability::ParallelToolCalls' packages/model-catalog/src/lib.rs >/dev/null ||
   ! grep -F 'ProviderCapability::ParallelToolCalls' plugins/fake-provider-plugin/src/lib.rs >/dev/null ||
   ! grep -F 'known_parallel_tool_provider' plugins/openai-compatible-provider-plugin/src/lib.rs >/dev/null ||
   ! grep -F 'parallel_tool_provider_capability_requires_known_backend' plugins/openai-compatible-provider-plugin/src/lib.rs >/dev/null ||
   ! grep -F 'ProviderCapability::ParallelToolCalls' plugins/bedrock-provider-plugin/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: provider/model parallel capability advertisements were removed." >&2
  violations=1
fi

if ! grep -F 'static_provider_adapter_conforms_for_multiple_calls_and_sequential_fallback' packages/bcode/tests/provider_plugin_conformance.rs >/dev/null ||
   ! grep -F 'static_provider_adapter_conforms_for_malformed_calls_and_cancellation' packages/bcode/tests/provider_plugin_conformance.rs >/dev/null ||
   ! grep -F 'completed_tool_calls_preserve_provider_order_and_exact_ids' plugins/openai-compatible-provider-plugin/src/lib.rs >/dev/null ||
   ! grep -F 'malformed_provider_tool_call_is_rejected_without_partial_completion' plugins/openai-compatible-provider-plugin/src/lib.rs >/dev/null ||
   ! grep -F 'openai_provider_cancel_turn_signals_active_adapter_state' plugins/openai-compatible-provider-plugin/src/lib.rs >/dev/null ||
   ! grep -F 'completed_tool_calls_preserve_bedrock_order_and_exact_ids' plugins/bedrock-provider-plugin/src/lib.rs >/dev/null ||
   ! grep -F 'malformed_bedrock_tool_call_is_rejected_without_partial_completion' plugins/bedrock-provider-plugin/src/lib.rs >/dev/null ||
   ! grep -F 'bedrock_provider_cancel_turn_signals_active_adapter_state' plugins/bedrock-provider-plugin/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: provider parallel-tool conformance coverage was removed." >&2
  violations=1
fi

if ! grep -F 'generic_lifecycle_drives_tui_activity_until_terminal_event' packages/tui/src/app.rs >/dev/null ||
   ! grep -F 'web_preserves_compact_single_tool_activity_until_terminal_event' packages/web-render/src/lib.rs >/dev/null ||
   ! grep -F 'web_uses_grouped_heading_only_for_multiple_active_invocations' packages/web-render/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: single-tool UX regression coverage was removed." >&2
  violations=1
fi

if ! grep -F 'batched_actions_keep_single_call_and_apply_to_all_distinct' packages/tui/src/permission_dialog.rs >/dev/null ||
   ! grep -F 'batched_remember_actions_never_apply_to_all' packages/tui/src/permission_dialog.rs >/dev/null ||
   ! grep -F 'grouped_permission_renders_per_call_and_apply_to_all_actions' packages/web-render/ui/src/pages/home.rs >/dev/null ||
   ! grep -F 'resolve_permission_batch(form.batch_id, form.approved)' packages/web-render/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: grouped permission adapter behavior was removed." >&2
  violations=1
fi

if ! grep -F 'permission_batch_correlation_survives_session_view_projection' packages/session-view/src/lib.rs >/dev/null ||
   ! grep -F 'batch: policy_context.batch.clone()' packages/server/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: live permission batch correlation was removed." >&2
  violations=1
fi

if ! grep -F 'transient_contribution_is_published_live_only_with_verified_identity' packages/server/src/lib.rs >/dev/null ||
   ! grep -F 'transient_contribution_projects_live_and_remove_is_terminal' packages/session-view/src/lib.rs >/dev/null ||
   ! grep -F 'transient_contribution_updates_and_removes_one_live_fallback' packages/tui/src/app.rs >/dev/null; then
  echo "Runtime architecture violation: transient contribution live-only routing coverage was removed." >&2
  violations=1
fi

if ! scripts/check-plugin-presentation-manifests.sh; then
  violations=1
fi

if ! grep -F 'generic_live_contribution_description_preserves_opaque_identity_and_payload' packages/cli/src/lib.rs >/dev/null ||
   ! grep -F 'SessionWatchEvent::ResyncRequired' packages/cli/src/lib.rs >/dev/null ||
   ! grep -F 'Event::Session(event) | Event::RuntimeWork(event)' packages/client/src/lib.rs >/dev/null; then
  echo "Runtime architecture violation: generic client/CLI session event handling was removed." >&2
  violations=1
fi

if (( violations != 0 )); then
  exit 1
fi

echo "loop/runtime domain-isolation guard passed"
