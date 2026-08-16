#!/usr/bin/env bash
set -euo pipefail

violations=0

if ! rg -q 'state_dir\.join\("workflows"\)\.join\(DATABASE_FILE\)' packages/workflow-store/src/lib.rs; then
  echo "Workflow store ownership violation: workflow-store must own the canonical workflow database path." >&2
  violations=1
fi

if rg -n 'fn migrate\(|ALTER TABLE workflow_|UPDATE workflow_store_contract SET schema_version' \
  packages/workflow-store/src/lib.rs >/tmp/bcode-workflow-store-migrations 2>/dev/null; then
  echo "Workflow store compatibility violation: clean-break storage must not retain migrations." >&2
  cat /tmp/bcode-workflow-store-migrations >&2
  violations=1
fi
rm -f /tmp/bcode-workflow-store-migrations

if ! rg -q 'pub struct WorkflowBlockPreparationRequest' packages/workflow/src/lib.rs \
  || ! rg -q 'pub struct WorkflowBlockPreparationResponse' packages/workflow/src/lib.rs \
  || ! rg -q 'preparation_required[[:space:]]*= true' plugins/shell-plugin/bcode-plugin.toml \
  || ! rg -q 'invoke_service_json_scoped_cancellable' packages/server/src/lib.rs \
  || ! rg -q '"preparation": preparation' packages/server/src/lib.rs \
  || ! rg -q 'input: canonical_input' packages/server/src/lib.rs \
  || ! rg -q 'MAX_WORKFLOW_BLOCK_PREPARATION_REQUEST_BYTES' packages/workflow/src/lib.rs \
  || ! rg -q 'cancellation_requested_at_ms.is_some' packages/server/src/lib.rs \
  || ! rg -q 'grant_scope == scope' packages/server/src/lib.rs \
  || ! rg -q 'preparation_descriptor_sha256' packages/workflow/src/lib.rs \
  || ! rg -q 'prepared_mutation_approval_facts_are_persisted_and_survive_restart' packages/workflow-store/src/lib.rs \
  || ! rg -q 'duplicate preparation must be stable' plugins/shell-plugin/src/lib.rs \
  || ! rg -q 'duplicate approval' packages/workflow-store/src/lib.rs \
  || ! rg -q 'cancelled_receipt_backed_mutation_is_terminal_not_repair_required' packages/workflow-store/src/lib.rs \
  || ! rg -q 'unknown_receipt_backed_mutation_becomes_repair_required' packages/workflow-store/src/lib.rs \
  || ! rg -q 'workflow_command_policy_allow_ask_and_deny_fail_closed_consistently' packages/server/src/lib.rs \
  || ! rg -q 'prepared_read_only_redispatch_reuses_identity_and_persists_receipt' packages/workflow-store/src/lib.rs \
  || ! rg -q 'workflow_preparation_binding_rejects_malformed_and_future_responses' packages/server/src/lib.rs \
  || ! rg -q 'workflow_owner_preparation_observes_durable_run_cancellation' packages/server/src/lib.rs \
  || ! rg -q 'workflow_plugin_dispatch_rejects_persisted_contract_or_input_drift' packages/server/src/lib.rs \
  || ! rg -q 'validate_workflow_preparation_binding' packages/server/src/lib.rs \
  || ! rg -q 'WorkflowPluginBlockPlan::AwaitingAuthorization' packages/server/src/lib.rs \
  || ! rg -q 'workflow_output_preview_preserves_binary_identity_without_claiming_utf8' plugins/shell-plugin/src/lib.rs \
  || ! rg -q 'descriptor.input_sha256 != input_sha256' plugins/shell-plugin/src/lib.rs; then
  echo "Workflow owner-preparation violation: typed contracts or manifest requirement drifted." >&2
  violations=1
fi

if rg -n 'operation_facts\.(get|as_object)|operation_facts\[[^]]+\]' \
  packages/workflow packages/workflow-store packages/server --glob '*.rs' \
  >/tmp/bcode-workflow-preparation-interpretation-violations 2>/dev/null; then
  echo "Workflow owner-preparation isolation violation: generic layers must persist and compare opaque owner facts without interpreting them." >&2
  cat /tmp/bcode-workflow-preparation-interpretation-violations >&2
  violations=1
fi
rm -f /tmp/bcode-workflow-preparation-interpretation-violations

if rg -n 'ShellWorkflowPreparationDescriptor|shell_workflow_command|shell_command_plan' \
  packages/workflow packages/workflow-store --glob '*.rs' \
  >/tmp/bcode-workflow-preparation-domain-violations 2>/dev/null; then
  echo "Workflow owner-preparation isolation violation: shell preparation types or semantics leaked into generic workflow domains." >&2
  cat /tmp/bcode-workflow-preparation-domain-violations >&2
  violations=1
fi
rm -f /tmp/bcode-workflow-preparation-domain-violations

if ! rg -q 'evaluate_workflow_plugin_block_policy' packages/server/src/lib.rs \
  || ! rg -q 'evaluate_agent_tool_policy_with_metadata' packages/server/src/lib.rs \
  || ! rg -q 'ToolPolicyAuthorizationMetadata' plugins/shell-plugin/src/lib.rs; then
  echo "Workflow command-policy routing violation: prepared shell facts must use the normal agent-policy evaluation path." >&2
  violations=1
fi

if ! rg -q 'pub enum WorkflowValueAssertion' packages/workflow/src/lib.rs \
  || ! rg -q 'SelectedAssertion' packages/workflow/src/lib.rs \
  || ! rg -q 'typed_assertions_enforce_utf8_truncation_byte_length_sha_and_artifact_boundaries' packages/workflow/src/lib.rs; then
  echo "Workflow assertion contract violation: typed UTF-8, byte-length, SHA-256, or artifact-boundary assertions drifted." >&2
  violations=1
fi

if ! rg -q 'x-bcode-dynamic-binding-paths' plugins/shell-plugin/bcode-plugin.toml \
  || ! rg -q 'workflow_dynamic_binding_path_allowed' packages/workflow/src/lib.rs \
  || ! rg -q 'workflow_prompt_input_remains_structured_json_without_text_interpolation' packages/server/src/lib.rs \
  || ! rg -q 'Source-authored exit protocols' docs/shell-workflow-block.md; then
  echo "Workflow safe-binding violation: owner-confined dynamic values, structured prompt JSON, or exit protocols drifted." >&2
  violations=1
fi

if ! rg -q 'pub struct WorkflowStructuredSourceConcisePrompt' packages/workflow/src/lib.rs \
  || ! rg -q 'workflow_skill_catalog_instruction' packages/server/src/lib.rs \
  || ! rg -q 'format_skill_catalog_for_prompt' packages/server/src/lib.rs \
  || ! rg -q 'concise_prompt_source_expands_to_complete_safe_configuration' packages/workflow/src/lib.rs; then
  echo "Workflow prompt-first violation: concise prompt expansion or ordinary skill catalog inclusion drifted." >&2
  violations=1
fi

if ! rg -q 'pub const WORKFLOW_STORE_SCHEMA_VERSION: u32 = 15' packages/workflow-store/src/lib.rs \
  || ! rg -q 'UnsupportedStore' packages/workflow-store/src/lib.rs \
  || ! rg -q 'reset_incompatible_store_in_state_dir' packages/workflow-store/src/lib.rs \
  || ! rg -q 'WorkflowStoreResetReceipt' packages/workflow-store/src/lib.rs \
  || ! rg -q 'reset_incompatible_workflow_store_offline' packages/server/src/lib.rs \
  || ! rg -q 'ResetIncompatibleWorkflowStore' packages/ipc/src/lib.rs \
  || ! rg -q 'workflow_store_reset_requires_offline_maintenance' packages/server/src/lib.rs \
  || ! rg -q 'execution_session_link_survives_retry_and_cancellation_terminalization' packages/workflow-store/src/lib.rs \
  || ! rg -q 'restored.authorization_ceiling' packages/workflow-store/src/lib.rs \
  || ! rg -q 'restored active lease' packages/workflow-store/src/lib.rs \
  || ! rg -q 'restored checkpoint' packages/workflow-store/src/lib.rs \
  || ! rg -q 'WorkflowExecutionAuthority' packages/workflow-store/src/lib.rs \
  || ! rg -q 'transfer_execution_authority' packages/workflow-store/src/lib.rs \
  || ! rg -q 'verify_execution_authority' packages/workflow-store/src/lib.rs \
  || ! rg -q 'workflow_execution_authority' packages/server/src/lib.rs \
  || ! rg -q 'workflow recovery deferred without mutation' packages/server/src/lib.rs \
  || ! rg -q 'deferred_receipt_backed_mutation_remains_running' packages/workflow-store/src/lib.rs \
  || ! rg -q 'foreign_artifact_workflow_receipt_is_deferred_before_observation' packages/server/src/lib.rs \
  || ! rg -q 'workflow_store_unavailable' packages/server/src/lib.rs \
  || ! rg -q 'workflow capability is unavailable; daemon startup will continue' packages/server/src/lib.rs \
  || ! rg -q 'workflow restoration skipped while the workflow domain is unavailable' packages/server/src/lib.rs \
  || ! rg -q 'DELETE-INCOMPATIBLE-WORKFLOW-STATE' packages/cli/src/lib.rs; then
  echo "Workflow store clean-break violation: fail-closed schema detection or explicit reset plumbing is missing." >&2
  violations=1
fi

if rg -n 'workflow\.db|CREATE TABLE.*(workflow_(runs|attempts|activations|outputs|drafts|revisions|presets|authoring_events)|authored_workflows)' \
  packages/session packages/tui packages/hyperchad plugins --glob '*.rs' >/tmp/bcode-workflow-architecture-violations 2>/dev/null; then
  echo "Workflow persistence ownership violation: session, loop, frontend, and plugin code must not own workflow database paths or tables." >&2
  cat /tmp/bcode-workflow-architecture-violations >&2
  violations=1
fi
rm -f /tmp/bcode-workflow-architecture-violations

if [[ ! -f docs/runtime-workflow-authoring.md ]] \
  || ! rg -q 'WorkflowAuthoringDocument' docs/runtime-workflow-authoring.md \
  || ! rg -q 'WorkflowApplicationOperationFacts' docs/runtime-workflow-authoring.md \
  || ! rg -q 'Active-run pinning' docs/runtime-workflow-authoring.md \
  || ! rg -q 'Producer-neutral authoring' docs/runtime-workflow-authoring.md \
  || ! rg -q 'Import and export' docs/runtime-workflow-authoring.md \
  || ! rg -q 'Producer workflows for AI and UI clients' docs/runtime-workflow-authoring.md; then
  echo "Workflow authoring architecture violation: source/compile/run, pinning, producer, and import boundaries must remain documented." >&2
  violations=1
fi

if ! rg -q 'pub struct WorkflowExecutableAuthoringSemantics' packages/workflow/src/lib.rs \
  || ! rg -q 'normalized\.executable_semantics\(\)' packages/workflow/src/lib.rs \
  || ! rg -q 'authoring_digest_is_stable_and_presentation_is_not_executable_identity' packages/workflow/src/lib.rs \
  || ! rg -q 'pub struct WorkflowRequirementAvailabilityReport' packages/workflow/src/lib.rs \
  || ! rg -q 'WorkflowRevisionRequirementInspection' packages/ipc/src/lib.rs \
  || ! rg -q 'AuthoredWorkflowInspection' packages/ipc/src/lib.rs \
  || ! rg -q 'workflow_authoring_events' packages/workflow-store/src/lib.rs \
  || ! rg -q 'diagnose_authored_workflow' packages/workflow-store/src/lib.rs \
  || ! rg -q 'typed_client_completes_authored_workflow_lifecycle_over_ipc' packages/server/src/lib.rs \
  || ! rg -q 'portable_json_catalog_fixture_composes_agent_control_and_exact_block_safety' packages/workflow/src/lib.rs \
  || ! rg -q 'repair_authored_workflow' packages/workflow-store/src/lib.rs \
  || ! rg -q 'TransactionBehavior::Exclusive' packages/workflow-store/src/lib.rs \
  || ! rg -q 'workflow maintenance backup must be in the canonical workflow directory' packages/workflow-store/src/lib.rs \
  || ! rg -q 'explicit_authored_repair_requires_confined_backup_and_repairs_only_safe_state' packages/workflow-store/src/lib.rs \
  || ! rg -q 'ImportWorkflowRevisionRequest' packages/ipc/src/lib.rs \
  || ! rg -q 'import_workflow_revision' packages/workflow-store/src/lib.rs \
  || ! rg -q 'RequireExistingWorkflowNextRevision' packages/ipc/src/lib.rs \
  || ! rg -q 'exact_revision_import_is_atomic_collision_safe_and_restart_safe' packages/workflow-store/src/lib.rs \
  || ! rg -q 'WorkflowImportCollisionPolicy' packages/ipc/src/lib.rs \
  || ! rg -q 'export_preview_import_preserves_semantics_and_enforces_collision_policy' packages/server/src/lib.rs \
  || ! rg -q 'producer_kind_does_not_change_compilation_or_local_authorization' packages/workflow/src/lib.rs \
  || ! rg -q 'local_application_authorization_ignores_untrusted_producer_kind' packages/server/src/lib.rs; then
  echo "Workflow authoring derived-state violation: executable identity, producer neutrality, current availability, and AI/UI workflow coverage must remain explicit." >&2
  violations=1
fi
if ! rg -q 'pub struct WorkflowDraftInspectionSummary' packages/ipc/src/lib.rs \
  || ! rg -q 'pub struct WorkflowRevisionInspectionSummary' packages/ipc/src/lib.rs \
  || ! rg -q 'pub struct WorkflowPresetInspectionSummary' packages/ipc/src/lib.rs \
  || rg -n 'pub struct AuthoredWorkflowInspection' -A7 packages/ipc/src/lib.rs \
      | rg -q 'Workflow(Draft|Revision|Preset)Snapshot|serde_json::Value'; then
  echo "Workflow diagnostic-data violation: aggregate inspection must use content-minimized summaries without documents, schemas, configuration, secrets, or prose." >&2
  violations=1
fi
if ! rg -q 'pub fn validate_persistable_authoring_value' packages/workflow/src/lib.rs \
  || ! rg -Fq 'validate_persistable_authoring_value("configuration_defaults"' packages/workflow/src/lib.rs \
  || ! rg -Fq 'validate_persistable_authoring_value("preset.configuration"' packages/workflow-store/src/lib.rs \
  || ! rg -q '"authored_run.configuration"' packages/workflow-store/src/lib.rs; then
  echo "Workflow persistence-safety violation: authored documents, presets, and run provenance must reject inline secrets and request-scoped secret references." >&2
  violations=1
fi

if ! rg -q 'record_histogram_with_exact_labels' packages/server/src/lib.rs \
  || ! rg -q 'add_counter_with_exact_labels' packages/server/src/lib.rs \
  || ! rg -q 'workflow.authoring.validation.duration_ms' packages/server/src/lib.rs \
  || ! rg -q 'workflow.authoring.compilation.duration_ms' packages/server/src/lib.rs \
  || ! rg -q 'workflow.authoring.publication.duration_ms' packages/server/src/lib.rs \
  || ! rg -q 'workflow.authoring.conflicts_total' packages/server/src/lib.rs \
  || ! rg -q 'workflow.authoring.import_preview.duration_ms' packages/server/src/lib.rs \
  || ! rg -q 'workflow.authoring.start_resolution.duration_ms' packages/server/src/lib.rs \
  || rg -n 'workflow\.authoring\.(validation|compilation|publication|conflicts|import_preview|start_resolution)' packages/server/src/lib.rs \
      | rg -q '(workflow_id|draft_id|preset_id|document|prompt|schema|secret|message)'; then
  echo "Workflow observability violation: authored lifecycle metrics and bounded labels must remain present and content-free." >&2
  violations=1
fi

if ! rg -q 'pub struct WorkflowApplicationOperationFacts' packages/workflow/src/lib.rs \
  || ! rg -q 'WORKFLOW_APPLICATION_OPERATION_FACTS_VERSION' packages/workflow/src/lib.rs \
  || ! rg -q 'authorize_workflow_application_operation' packages/server/src/lib.rs \
  || ! rg -q 'authorize_local_workflow_application_operation' packages/server/src/lib.rs; then
  echo "Workflow authorization boundary violation: normalized workflow-owned operation facts and the daemon authorization gate must remain present." >&2
  violations=1
fi

if rg -n 'ToolPolicyOperation|EvaluateToolCallRequest|RuntimePermissionRequest|PermissionPolicyContext' \
  packages/workflow/src/lib.rs >/tmp/bcode-workflow-authorization-leaks 2>/dev/null; then
  echo "Workflow authorization boundary violation: application operation facts must not depend on tool-call/session permission types." >&2
  cat /tmp/bcode-workflow-authorization-leaks >&2
  violations=1
fi
rm -f /tmp/bcode-workflow-authorization-leaks

# Workflow authoring models live with the renderer-neutral workflow contracts. Keep implementation
# packages and third-party frontend/database frameworks out of that owner before authoring types land,
# so future additions fail mechanically at the package boundary rather than during review.
if rg -n '^[[:space:]]*(bcode_(client|daemon|hyperchad|ipc|model_provider_runtime|plugin_runtime|server|tui|workflow_store)|ratatui|rusqlite|sqlx)[[:space:]]*=' \
  packages/workflow/Cargo.toml \
  >/tmp/bcode-workflow-authoring-dependency-violations 2>/dev/null; then
  echo "Workflow authoring contract violation: portable workflow contracts must not depend on frontend, daemon, persistence, provider implementation, or plugin runtime packages." >&2
  cat /tmp/bcode-workflow-authoring-dependency-violations >&2
  violations=1
fi
rm -f /tmp/bcode-workflow-authoring-dependency-violations

if rg -n '(^|[^[:alnum:]_])(bcode_(client|daemon|hyperchad|ipc|model_provider_runtime|plugin_runtime|server|tui|workflow_store)|ratatui|rusqlite|sqlx)::' \
  packages/workflow/src --glob '*.rs' \
  >/tmp/bcode-workflow-authoring-source-violations 2>/dev/null; then
  echo "Workflow authoring contract violation: workflow-owned source imports frontend, daemon, database, provider-private, or plugin-runtime implementation types." >&2
  cat /tmp/bcode-workflow-authoring-source-violations >&2
  violations=1
fi
rm -f /tmp/bcode-workflow-authoring-source-violations

if ! rg -q 'pub struct WorkflowAuthoringDocument' packages/workflow/src/lib.rs \
  || ! rg -q 'pub struct WorkflowAuthoringCatalogSnapshot' packages/workflow/src/lib.rs \
  || ! rg -q 'pub struct WorkflowCompilationPreview' packages/workflow/src/lib.rs; then
  echo "Workflow authoring contract violation: required portable authoring contracts are missing." >&2
  violations=1
fi

# The authored lifecycle snapshot/request declarations precede the legacy exact-definition request.
# Keep this public authoring contract region independent of persistence implementation types.
awk '/pub struct AuthoredWorkflowSnapshot/{capture=1} /pub struct WorkflowDefinitionRegistrationRequest/{capture=0} capture' \
  packages/ipc/src/lib.rs \
  | rg -n 'bcode_workflow_store|rusqlite|ratatui|bcode_tui|bcode_server' \
  >/tmp/bcode-workflow-authoring-ipc-violations 2>/dev/null && {
  echo "Workflow authoring IPC violation: portable authoring snapshots or requests expose implementation types." >&2
  cat /tmp/bcode-workflow-authoring-ipc-violations >&2
  violations=1
}
rm -f /tmp/bcode-workflow-authoring-ipc-violations

if ! rg -q 'pub fn production_admission' packages/workflow/src/lib.rs \
  || ! rg -q 'WorkflowProductionCapabilities::current' packages/server/src/lib.rs \
  || ! rg -q 'validate_workflow_definition_for_production' packages/server/src/lib.rs; then
  echo "Workflow admission violation: durable registration/start must use the production capability contract." >&2
  violations=1
fi

for required in \
  'pub const ALL_NODE_KINDS' \
  'pub const ALL_WORKFLOW_EDGE_KINDS' \
  'pub const fn node_kind_name' \
  'pub const fn workflow_edge_kind_name'; do
  if ! rg -q "$required" packages/workflow/src/lib.rs; then
    echo "Workflow capability coverage violation: missing exhaustive enum coverage marker: $required" >&2
    violations=1
  fi
done

if ! rg -q 'let node_kinds = BTreeMap::from' packages/workflow/src/lib.rs \
  || ! rg -q 'let edge_kinds = BTreeMap::from' packages/workflow/src/lib.rs; then
  echo "Workflow capability coverage violation: current capability maps must explicitly classify node and edge kinds." >&2
  violations=1
fi

for required in \
  'pub fn prepare_attempt' \
  'pub fn persist_dispatch_receipt' \
  'pub fn persist_validated_output' \
  'pub fn reconcile_prepared_attempts' \
  'pub fn active_attempt_cancellations' \
  'pub async fn propagate_cancellation' \
  'pub fn doctor_run' \
  'pub fn repair_attempt' \
  'RepairRequired'; do
  if ! rg -q "$required" packages/workflow-store/src/lib.rs; then
    echo "Workflow durability violation: missing required invariant marker: $required" >&2
    violations=1
  fi
done

if ! rg -q 'Persist prepared intent before an external operation is dispatched' packages/workflow-store/src/lib.rs \
  || ! rg -q 'Persist validated node output before marking its activation complete' packages/workflow-store/src/lib.rs; then
  echo "Workflow transaction-order violation: prepared-intent/output boundaries must remain explicit." >&2
  violations=1
fi

if rg -n 'bcode_workflow|workflow-store|workflow_store|WorkflowDefinition|WorkflowRun' \
  packages/agent-runtime --glob '*.rs' >/tmp/bcode-workflow-agent-runtime-violations 2>/dev/null; then
  echo "Workflow domain-isolation violation: agent-runtime must not own workflow coordination." >&2
  cat /tmp/bcode-workflow-agent-runtime-violations >&2
  violations=1
fi
rm -f /tmp/bcode-workflow-agent-runtime-violations

{
  awk '/^#\[cfg\(test\)\]/{exit} {print}' packages/server/src/lib.rs
  find packages/agent-runtime/src -name '*.rs' -type f -exec cat {} +
} | rg -n 'code_review\.bundle|security\.(scan|audit)|git\.(commit|push)|workflow_template' \
  >/tmp/bcode-workflow-domain-policy-violations 2>/dev/null && {
  echo "Workflow domain-isolation violation: generic runtimes contain product workflow policy." >&2
  cat /tmp/bcode-workflow-domain-policy-violations >&2
  violations=1
}
rm -f /tmp/bcode-workflow-domain-policy-violations

if rg -n 'std::process::Command|tokio::process::Command|git2::' \
  packages/workflow packages/workflow-store --glob '*.rs' \
  >/tmp/bcode-workflow-external-owner-violations 2>/dev/null; then
  echo "Workflow domain-isolation violation: generic workflow packages must not execute shell or Git operations." >&2
  cat /tmp/bcode-workflow-external-owner-violations >&2
  violations=1
fi
rm -f /tmp/bcode-workflow-external-owner-violations

if ! rg -q 'WorkflowTemplateContribution' packages/plugin/src/lib.rs \
  || ! rg -q 'WORKFLOW_TEMPLATE_CONTRIBUTION_VERSION' packages/plugin/src/lib.rs \
  || ! rg -q 'template\.validate\(\)' packages/plugin/src/lib.rs; then
  echo "Workflow template contract violation: manifest templates must remain versioned and discovery-validated." >&2
  violations=1
fi

if ! rg -q 'explicit_grant_required = true' plugins/shell-plugin/bcode-plugin.toml \
  || ! rg -q 'reconciliation[[:space:]]*=[[:space:]]*"repair_required"' plugins/shell-plugin/bcode-plugin.toml \
  || rg -q 'bcode\.workflow-block/v1|workflow_blocks' plugins/git-plugin/bcode-plugin.toml; then
  echo "Workflow mutation contract violation: shell requires safe authorization/reconciliation and Git must not own workflow operations." >&2
  violations=1
fi

if ! rg -q 'pub enum WorkflowNodeDataflowPolicy' packages/workflow/src/lib.rs \
  || ! rg -q 'StateEnvelopeV1' packages/workflow/src/lib.rs \
  || ! rg -q 'prepare_workflow_node_dataflow' packages/server/src/lib.rs \
  || ! rg -q 'owner_input = prepared\.owner_input' packages/server/src/lib.rs \
  || ! rg -q 'complete_workflow_node_dataflow' packages/server/src/lib.rs; then
  echo "Workflow state-envelope violation: explicit adaptation, owner-only authorization, or result rewrapping was removed." >&2
  violations=1
fi

if ! rg -q 'FieldsEqual' packages/workflow/src/lib.rs \
  || ! rg -q 'NumericCompare' packages/workflow/src/lib.rs \
  || ! rg -q 'WorkflowValueSelectorSegment' packages/workflow/src/lib.rs \
  || ! rg -q 'SelectedInput' packages/workflow/src/lib.rs \
  || ! rg -q 'MAX_VALUE_SELECTOR_SEGMENTS' packages/workflow/src/lib.rs \
  || ! rg -q 'MAX_PREDICATE_DEPTH' packages/workflow/src/lib.rs \
  || ! rg -q 'WORKFLOW_PREDICATE_MIN_VERSION' packages/workflow/src/lib.rs; then
  echo "Workflow predicate violation: bounded versioned compositional predicates and generic selectors were removed." >&2
  violations=1
fi

if rg -n 'shell|command-plan|exit_code' packages/workflow/src/lib.rs \
  | rg 'WorkflowValueSelector|SelectedEquals|SelectedValuesEqual|SelectedNumericCompare|SelectedInput' \
  >/tmp/bcode-workflow-selector-violations 2>/dev/null; then
  echo "Workflow selector ownership violation: generic selectors must not encode shell-specific behavior." >&2
  cat /tmp/bcode-workflow-selector-violations >&2
  violations=1
fi
rm -f /tmp/bcode-workflow-selector-violations

if ! rg -q 'pub const WORKFLOW_SOURCE_DOCUMENT_VERSION: u32 = 3;' packages/workflow/src/lib.rs \
  || ! rg -q 'pub struct WorkflowStructuredSourceDocument' packages/workflow/src/lib.rs \
  || ! rg -q 'WorkflowSourceProfile::Structured' packages/workflow/src/lib.rs \
  || ! rg -q 'structured_source_v3_lowers_deterministically_and_rejects_previous_versions' packages/workflow/src/lib.rs \
  || ! rg -q 'workflow_source_version: 3' docs/source-defined-workflows.md \
  || rg -q 'WORKFLOW_SOURCE_V1_DOCUMENT_VERSION|WorkflowSourceProfile::Concise' packages/workflow/src/lib.rs; then
  echo "Workflow source-v3 violation: the clean source contract, old-version rejection, lowering, or documentation drifted." >&2
  violations=1
fi

if ! rg -q 'pub enum WorkflowSourceFormat' packages/workflow/src/lib.rs \
  || ! rg -q 'pub fn decode_workflow_authoring_source' packages/workflow/src/lib.rs \
  || ! rg -q 'checked_in_workflow_sources_have_identical_compiled_semantics' packages/workflow/src/lib.rs \
  || ! rg -q 'decode_workflow_authoring_source' packages/bcode/src/workflow.rs \
  || ! rg -q 'decode_workflow_authoring_source' packages/cli/src/lib.rs \
  || ! rg -q 'decode_workflow_authoring_source' plugins/workflow-plugin/src/cli.rs \
  || ! rg -q 'name = "workflow-ui"' plugins/workflow-plugin/src/cli.rs \
  || rg -q 'std::fs|tokio::fs|bcode_workflow_store|rusqlite' packages/workflow/src/lib.rs; then
  echo "Workflow source-format violation: one workflow-owned decoder must serve SDK and frontend adapters." >&2
  violations=1
fi

if ! rg -q 'distinct_activations_of_one_node_recover_distinct_sessions' packages/server/src/lib.rs \
  || ! rg -q 'execution_session_link_survives_retry_and_cancellation_terminalization' packages/workflow-store/src/lib.rs \
  || ! rg -q 'damaged_execution_session_link_fails_closed_on_read' packages/workflow-store/src/lib.rs; then
  echo "Workflow execution-session recovery violation: activation, retry/restart, cancellation, and damage coverage drifted." >&2
  violations=1
fi

if ! rg -q 'schema_nine_is_rejected_without_mutation' packages/workflow-store/src/lib.rs \
  || ! rg -q 'execution_session_rejects_unknown_future_provenance_version' packages/session/src/lib.rs \
  || ! rg -q 'pinned_generation_execution_excludes_later_parent_events' packages/session/src/lib.rs; then
  echo "Workflow execution-session schema violation: clean-break rejection and future/pinned context coverage drifted." >&2
  violations=1
fi

if ! rg -q 'shared_execution_admission_serializes_one_parent' packages/session/src/lib.rs \
  || ! rg -q 'admit_shared_execution_session' packages/server/src/lib.rs \
  || ! rg -q 'SharedParentSequential' packages/workflow/src/lib.rs; then
  echo "Workflow shared-parent context violation: shared execution must remain parent-scoped and serialized." >&2
  violations=1
fi

if ! rg -q 'background sessions' docs/workflow-persistence-architecture.md \
  || ! rg -q 'list_sessions_with_background' packages/session/src/lib.rs \
  || ! rg -q 'SessionVisibility::Background' packages/session/src/lib.rs; then
  echo "Workflow execution-session lifecycle violation: background visibility and retention boundaries must remain explicit." >&2
  violations=1
fi

if ! rg -q 'FixedGenerationFork' packages/workflow/src/lib.rs \
  || ! rg -q 'parent_session_generation' packages/ipc/src/lib.rs packages/workflow-store/src/lib.rs \
  || ! rg -q 'create_pinned_generation_execution_session' packages/session/src/lib.rs packages/server/src/lib.rs \
  || ! rg -q 'fixed_generation_workflow_start_requires_and_pins_exact_parent_generation' packages/server/src/lib.rs; then
  echo "Workflow fixed-generation context violation: exact start admission and pinned child context support drifted." >&2
  violations=1
fi

if ! rg -q 'workflow_execution_sessions' docs/workflow-persistence-architecture.md \
  || ! rg -q 'WORKFLOW_EXECUTION_SESSION_LINK_VERSION' packages/workflow-store/src/lib.rs \
  || ! rg -q 'workflow_execution_sessions' packages/workflow-store/src/lib.rs \
  || ! rg -q 'execution_session_link' packages/workflow-store/src/lib.rs \
  || ! rg -q 'EXECUTION_SESSION_PROVENANCE_VERSION' packages/session/models/src/lib.rs \
  || ! rg -q 'activation_id: Option<String>' packages/session/models/src/lib.rs \
  || ! rg -q 'workflow_child_session_for_activation' packages/server/src/lib.rs \
  || rg -q 'workflow_child_session_for_attempt' packages/server/src/lib.rs; then
  echo "Workflow execution-session identity violation: workflow sessions must be recovered by exact activation and attempt." >&2
  violations=1
fi

if [[ ! -f fixtures/workflows/concise-run.workflow.json ]] \
  || [[ ! -f fixtures/workflows/concise-run.workflow.yaml ]] \
  || [[ ! -f fixtures/workflows/concise-run.workflow.toml ]] \
  || [[ ! -f docs/source-defined-workflows.md ]] \
  || ! rg -q 'pub struct WorkflowStructuredSourceDocument' packages/workflow/src/lib.rs \
  || ! rg -q 'pub fn lower_workflow_authoring_source' packages/workflow/src/lib.rs \
  || ! rg -q 'WorkflowAuthoringActionDescriptor' packages/plugin/src/lib.rs \
  || ! rg -q 'block_id[[:space:]]*=[[:space:]]*"exec"' plugins/shell-plugin/bcode-plugin.toml; then
  echo "Usable workflow source violation: format-neutral lowering, plugin-owned actions, or shell isolation drifted." >&2
  violations=1
fi

if [[ ! -f fixtures/workflows/source-defined-input.workflow.json ]] \
  || [[ ! -f fixtures/workflows/source-defined-input.workflow.toml ]] \
  || [[ ! -f fixtures/workflows/shell-v2-exit-routing.workflow.json ]] \
  || ! rg -q 'Source-controlled JSON and TOML' docs/runtime-workflow-authoring.md \
  || ! rg -q '\$bcode_null' docs/runtime-workflow-authoring.md; then
  echo "Workflow source-format documentation violation: paired fixtures and TOML null semantics must remain documented." >&2
  violations=1
fi

if [[ $(rg -c '^\[\[services\.workflow_blocks\]\]' plugins/shell-plugin/bcode-plugin.toml) -ne 1 ]] \
  || ! rg -q 'block_id[[:space:]]*=[[:space:]]*"exec"' plugins/shell-plugin/bcode-plugin.toml \
  || ! rg -q 'block_version[[:space:]]*=[[:space:]]*1' plugins/shell-plugin/bcode-plugin.toml \
  || ! rg -q 'bcode\.shell\.exec/v1' plugins/shell-plugin/bcode-plugin.toml \
  || ! rg -q 'bcode\.shell\.exec-result/v1' plugins/shell-plugin/bcode-plugin.toml \
  || ! rg -q 'exit_accepted' plugins/shell-plugin/bcode-plugin.toml \
  || ! rg -q 'pub enum ProcessTermination' packages/tool-runtime/src/lib.rs; then
  echo "Shell workflow contract violation: the single exec@1 block or normalized termination semantics drifted." >&2
  violations=1
fi

if rg -q 'fn read_nearest_agent_instructions' packages/server/src/lib.rs \
  || ! rg -q 'bcode_project_instructions::discover_project_instructions' packages/server/src/lib.rs \
  || ! rg -q 'pub fn discover_project_instructions' packages/project-instructions/src/lib.rs; then
  echo "Project-instruction ownership violation: discovery must remain domain-owned and shared." >&2
  violations=1
fi

if ! rg -q 'workflow_definitions: BTreeMap' packages/workflow/src/lib.rs \
  || ! rg -q 'resolve_authoring_workflow_call' packages/workflow/src/lib.rs \
  || ! rg -q 'authorization_ceiling' packages/workflow-store/src/lib.rs \
  || ! rg -q 'workflow child authorization ceiling exceeds its parent' \
    packages/workflow-store/src/lib.rs \
  || ! rg -q 'Child runs never inherit parent' packages/workflow-store/src/lib.rs \
  || ! rg -q 'SHELL_COMMAND_PLAN_VERSION: u32 = 2' plugins/shell-plugin/src/contracts.rs \
  || ! rg -q 'accepted_exit_codes' plugins/shell-plugin/src/contracts.rs \
  || ! rg -q 'continue_on_unaccepted_exit' plugins/shell-plugin/src/contracts.rs; then
  echo "Workflow composition violation: recursive preview, authorization ceilings, or shell plan contracts were removed." >&2
  violations=1
fi

if ! rg -q 'WorkflowDependencyManifestEntry' packages/workflow/src/lib.rs \
  || ! rg -q 'workflow_dependency_manifest' packages/workflow/src/lib.rs \
  || ! rg -q 'workflow_dependency_manifest' packages/server/src/lib.rs; then
  echo "Workflow dependency-manifest violation: portable export/import no longer binds exact child targets." >&2
  violations=1
fi

if ! rg -q 'NodeKind::WorkflowCall' packages/workflow/src/lib.rs \
  || ! rg -q 'WorkflowCallTarget' packages/workflow/src/lib.rs \
  || ! rg -q 'workflow call nodes must not retain resource leases' packages/workflow/src/lib.rs \
  || ! rg -q 'create_child_run_idempotent' packages/workflow-store/src/lib.rs \
  || ! rg -q '"child_run_id".*request\.link\.child_run_id' packages/workflow-store/src/lib.rs \
  || ! rg -q 'observe_child_attempt' packages/workflow-store/src/lib.rs \
  || ! rg -q 'workflow_run_links' packages/workflow-store/src/lib.rs \
  || ! rg -q 'reject_recursive_child_target' packages/workflow-store/src/lib.rs \
  || ! rg -q 'dispatch_workflow_child' packages/server/src/lib.rs \
  || ! rg -q 'MAX_WORKFLOW_RUN_DESCENDANTS' packages/workflow-store/src/lib.rs; then
  echo "Workflow child-composition violation: exact call targets, atomic links, or admission bounds were removed." >&2
  violations=1
fi

if ! rg -q 'terminal_output_id' packages/workflow-store/src/lib.rs \
  || ! rg -q 'terminal_output_checksum_sha256' packages/workflow-store/src/lib.rs \
  || ! rg -q 'terminal_output_survives_restart_without_duplicate_materialization' \
    packages/workflow-store/src/lib.rs; then
  echo "Workflow terminal-output violation: canonical successful output identity or restart coverage was removed." >&2
  violations=1
fi

if ! rg -q 'WorkflowRepeatExhaustionPolicy' packages/workflow/src/lib.rs \
  || ! rg -q 'WorkflowRepeatOutcome' packages/workflow/src/lib.rs \
  || ! rg -q 'repeat_outcomes' packages/workflow-store/src/lib.rs \
  || ! rg -q 'repeat_iteration_limit_exhausted' packages/workflow-store/src/lib.rs; then
  echo "Workflow repeat outcome violation: typed exhaustion outcomes or fail-default compatibility were removed." >&2
  violations=1
fi

if ! bash scripts/check-composable-workflow-architecture.sh; then
  echo "Workflow composability architecture violation: coding-workflow product boundaries drifted." >&2
  violations=1
fi

exit "$violations"
