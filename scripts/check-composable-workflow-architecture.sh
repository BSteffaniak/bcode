#!/usr/bin/env bash
set -euo pipefail

violations=0
architecture="docs/composable-coding-workflows.md"

for required in \
  'State-preserving operation dataflow' \
  'Deterministic predicates' \
  'Repeat outcomes' \
  'Canonical terminal workflow output' \
  'Exact child workflow calls' \
  'Project instructions and reviewed command plans' \
  'Repository snapshot and verification authority' \
  'Exact Git checkpointing' \
  'Progress-document interaction and persistence' \
  'Semantic graph editing and frontend adaptation' \
  'Confined external template sources'; do
  if [[ ! -f "$architecture" ]] || ! grep -F "$required" "$architecture" >/dev/null; then
    echo "Composable workflow architecture violation: missing locked section: $required" >&2
    violations=1
  fi
done

for boundary in \
  'composable-coding-workflows.md' \
  'state_envelope_v1' \
  'block_and_review' \
  'four workflow nesting' \
  '64 descendants per root run'; do
  if ! grep -F "$boundary" "$architecture" docs/runtime-workflow-authoring.md \
    docs/workflow-persistence-architecture.md docs/workflow-plugins-and-templates.md \
    docs/workflow-operations.md docs/git-workflow-blocks.md docs/shell-workflow-block.md \
    docs/frontend-contracts.md >/dev/null; then
    echo "Composable workflow architecture violation: missing boundary marker: $boundary" >&2
    violations=1
  fi
done

if rg -n 'CodingWorkflowState|ProgressWorkState|ProgressDocument|RepositorySnapshot|Clippier|clippier|bcode\.progress-doc' \
  packages/workflow packages/workflow-store --glob '*.rs' \
  >/tmp/bcode-composable-product-leaks.txt 2>/dev/null; then
  echo "Composable workflow ownership violation: product or integration types leaked into generic workflow packages." >&2
  cat /tmp/bcode-composable-product-leaks.txt >&2
  violations=1
fi
rm -f /tmp/bcode-composable-product-leaks.txt

if rg -n 'std::process::Command|tokio::process::Command|git2::|pulldown_cmark|AGENTS\.md' \
  packages/workflow packages/workflow-store --glob '*.rs' \
  >/tmp/bcode-composable-io-leaks.txt 2>/dev/null; then
  echo "Composable workflow ownership violation: generic workflow packages own external operation or instruction I/O." >&2
  cat /tmp/bcode-composable-io-leaks.txt >&2
  violations=1
fi
rm -f /tmp/bcode-composable-io-leaks.txt

if ! grep -F 'retained state or presentation' "$architecture" >/dev/null \
  || ! grep -F 'Derive permission facts from the unwrapped owner operation' "$architecture" >/dev/null; then
  echo "Composable workflow authorization violation: state-envelope authorization boundary is not explicit." >&2
  violations=1
fi

if ! grep -F 'Active-revision lookup is forbidden during dispatch' "$architecture" >/dev/null \
  || ! grep -F 'deterministic child run identity' "$architecture" >/dev/null \
  || ! grep -F 'not abandon children' "$architecture" >/dev/null; then
  echo "Composable workflow child-call violation: exact identity or lifecycle boundary is incomplete." >&2
  violations=1
fi

if ! grep -F 'workspace_snapshot' "$architecture" >/dev/null \
  || ! grep -F 'Git plugin owns a versioned `RepositorySnapshot`' "$architecture" >/dev/null \
  || ! grep -F 'Any later included mutation invalidates it' "$architecture" >/dev/null; then
  echo "Composable workflow verification violation: workspace and verified content authority are conflated." >&2
  violations=1
fi

if ! grep -F 'Normal mutation authorization still precedes writing' "$architecture" >/dev/null \
  || ! grep -F 'Writes use a safe temporary file and' "$architecture" >/dev/null \
  || ! grep -F 'Full content does not enter routine public diagnostics or metrics' "$architecture" >/dev/null \
  || ! rg -q 'id[[:space:]]*=[[:space:]]*"bcode\.progress-doc"' plugins/progress-doc-plugin/bcode-plugin.toml \
  || ! rg -q 'MAX_WORKFLOW_MUTATION_APPROVAL_INPUT_BYTES' packages/workflow/src/lib.rs \
  || ! rg -q 'scope\.input_checksum_sha256 == input_checksum_sha256' packages/server/src/lib.rs \
  || ! rg -q 'scope\.input_summary == \*owner_input' packages/server/src/lib.rs \
  || ! rg -q 'progress_document_approval_provenance' packages/server/src/lib.rs \
  || ! rg -q 'workflow_interaction_provenance_exists' packages/server/src/lib.rs \
  || ! rg -q 'progress_document_interaction_action' packages/server/src/lib.rs \
  || ! rg -q 'PROGRESS_DOCUMENT_INTERACTION_SCHEMA' packages/server/src/lib.rs \
  || ! rg -q 'progress_document_interaction_request_matches' packages/server/src/lib.rs \
  || ! rg -q 'progress_document_interaction_events_have_apply' packages/server/src/lib.rs \
  || ! rg -q 'MAX_PROGRESS_DOCUMENT_INTERACTION_HISTORY_EVENTS' packages/server/src/lib.rs \
  || ! rg -q 'workflow_plugin_skill_roots' packages/server/src/lib.rs \
  || ! rg -q 'required skills fail closed when the registry is unavailable' packages/server/src/lib.rs \
  || ! rg -q 'disabled workflow plugin host' packages/server/src/lib.rs \
  || ! rg -q 'malformed_encoding_is_rejected_without_content_disclosure' plugins/progress-doc-plugin/src/lib.rs \
  || ! rg -q 'cancellation_precedes_progress_document_mutation' plugins/progress-doc-plugin/src/lib.rs \
  || ! rg -q 'workflow_read_only_skill_is_compatible' packages/server/src/lib.rs \
  || ! rg -q 'interaction:<durable-id>' packages/server/src/lib.rs \
  || ! rg -q 'progress-doc\.inspect' plugins/progress-doc-plugin/src/lib.rs \
  || ! rg -q 'progress-doc\.create' plugins/progress-doc-plugin/src/lib.rs \
  || ! rg -q 'progress-doc\.replace' plugins/progress-doc-plugin/src/lib.rs \
  || ! rg -q 'progress-doc\.reconcile' plugins/progress-doc-plugin/src/lib.rs \
  || ! rg -q 'persist_noclobber' plugins/progress-doc-plugin/src/lib.rs \
  || ! rg -q 'explicit_grant_required = true' plugins/progress-doc-plugin/bcode-plugin.toml \
  || ! rg -q 'reconciliation[[:space:]]*=[[:space:]]*"repair_required"' plugins/progress-doc-plugin/bcode-plugin.toml; then
  echo "Composable workflow progress-document violation: ownership, approval, confinement, or diagnostic safety is incomplete." >&2
  violations=1
fi

if ! rg -q 'git\.repository-snapshot' plugins/git-plugin/bcode-plugin.toml \
  || ! rg -q 'git\.verification-receipt' plugins/git-plugin/bcode-plugin.toml \
  || ! rg -q 'struct RepositorySnapshot' plugins/git-plugin/src/lib.rs \
  || ! rg -q 'parse_porcelain_v2_entries' plugins/git-plugin/src/lib.rs \
  || ! rg -q 'progress_document_path' plugins/git-plugin/src/lib.rs \
  || ! rg -q 'MAX_SNAPSHOT_MANIFEST_BYTES' plugins/git-plugin/src/lib.rs \
  || ! rg -q 'instruction_drift_receipt' plugins/workflow-plugin/src/lib.rs \
  || ! rg -q 'InstructionDriftReceipt::Blocked' plugins/workflow-plugin/src/lib.rs \
  || ! rg -q 'ReviewedReplacement' plugins/workflow-plugin/src/lib.rs \
  || ! rg -q 'VerificationStage::PostFormat' plugins/git-plugin/src/lib.rs \
  || ! rg -Uq 'stage = \{ type = "string", enum = \[\s*"pre_format",\s*"post_format",?\s*\]' plugins/git-plugin/bcode-plugin.toml \
  || ! rg -q 'plan_sha256' plugins/shell-plugin/src/contracts.rs \
  || ! rg -q 'canonical_command_plan_sha256' plugins/shell-plugin/src/lib.rs \
  || ! rg -q 'max_uses' packages/workflow-store/src/lib.rs \
  || ! rg -q 'uses_consumed' packages/workflow-store/src/lib.rs \
  || ! rg -q 'pub fn consume_grant' packages/workflow-store/src/lib.rs \
  || ! rg -q 'use budget is exhausted' packages/workflow-store/src/lib.rs \
  || ! rg -q 'grant is expired' packages/workflow-store/src/lib.rs \
  || ! rg -q 'required_artifacts_complete' plugins/git-plugin/src/lib.rs \
  || ! rg -q 'pre_snapshot\.aggregate_sha256 != request\.post_snapshot\.aggregate_sha256' plugins/git-plugin/src/lib.rs \
  || ! rg -q 'project_instruction_fingerprint_sha256' plugins/git-plugin/src/lib.rs; then
  echo "Composable workflow verification-authority violation: Git-owned snapshot or unchanged-state receipt enforcement is incomplete." >&2
  violations=1
fi

if ! rg -q 'expected_snapshot_sha256' plugins/git-plugin/src/lib.rs \
  || ! rg -q 'VerifiedCheckpointManifest' plugins/git-plugin/src/lib.rs \
  || ! rg -q 'staged_object_identities' plugins/git-plugin/src/lib.rs \
  || ! rg -q 'committed checkpoint parent is not the verified HEAD' plugins/git-plugin/src/lib.rs \
  || ! rg -q 'committed path set differs from the verified manifest' plugins/git-plugin/src/lib.rs \
  || ! rg -q 'committed_objects' plugins/git-plugin/src/lib.rs \
  || ! rg -q 'expected_repository_identity_sha256' plugins/git-plugin/bcode-plugin.toml \
  || ! rg -q 'explicit_grant_required = true' plugins/git-plugin/bcode-plugin.toml; then
  echo "Composable workflow verified-checkpoint violation: exact verified Git identity is incomplete." >&2
  violations=1
fi

if ! rg -q 'WorkflowTemplateDocumentSource' packages/plugin/src/lib.rs \
  || ! rg -q 'MAX_WORKFLOW_TEMPLATE_AUTHORING_DOCUMENT_BYTES' packages/plugin/src/lib.rs \
  || ! rg -q 'canonical_path\.starts_with\(&canonical_root\)' packages/plugin/src/lib.rs \
  || ! rg -q 'source digest mismatch' packages/plugin/src/lib.rs \
  || ! rg -q 'InstantiateWorkflowTemplate' packages/ipc/src/lib.rs packages/server/src/lib.rs \
  || ! rg -q 'template instantiation requires a successful compilation preview' packages/server/src/lib.rs \
  || ! rg -q 'flagship_template_instantiates_and_publishes_as_an_exact_mutable_draft' packages/server/src/lib.rs \
  || ! rg -q 'templates/implementation-batch\.workflow\.json' plugins/workflow-plugin/bcode-plugin.toml \
  || ! rg -q 'IMPLEMENTATION_BATCH_ITERATION_LIMIT: u32 = 20' plugins/workflow-plugin/src/lib.rs \
  || ! rg -q 'WorkflowRepeatExhaustionPolicy::EmitOutcome|"emit_outcome"' plugins/workflow-plugin/src/lib.rs plugins/workflow-plugin/templates/implementation-batch.workflow.json \
  || ! rg -q 'ImplementationBatchOutcome' plugins/workflow-plugin/src/lib.rs \
  || ! rg -q 'git\.repository-snapshot' plugins/workflow-plugin/templates/implementation-batch.workflow.json \
  || ! rg -q 'shell\.command-plan' plugins/workflow-plugin/templates/implementation-batch.workflow.json \
  || ! rg -q 'git\.prepare' plugins/workflow-plugin/templates/implementation-batch.workflow.json \
  || ! rg -q 'git\.verification-receipt' plugins/workflow-plugin/templates/implementation-batch.workflow.json \
  || ! rg -q 'git\.compose-commit' plugins/workflow-plugin/templates/implementation-batch.workflow.json \
  || ! rg -q 'git\.commit' plugins/workflow-plugin/templates/implementation-batch.workflow.json \
  || ! rg -q 'delivery-tranche\.workflow\.json' plugins/workflow-plugin/bcode-plugin.toml \
  || ! rg -q 'delivery_tranche_pins_exact_batch_and_runtime_owns_five_batches' plugins/workflow-plugin/src/lib.rs \
  || ! rg -q '"max_iterations":5' plugins/workflow-plugin/templates/delivery-tranche.workflow.json \
  || ! rg -q 'progress-driven-delivery\.workflow\.json' plugins/workflow-plugin/bcode-plugin.toml \
  || ! rg -q 'progress_driven_parent_pins_tranche_and_enforces_all_product_budgets' plugins/workflow-plugin/src/lib.rs \
  || ! rg -q 'local-<workflow-slug>-progress\.md' plugins/workflow-plugin/templates/progress-driven-delivery.workflow.json \
  || ! rg -q '"max_iterations":10' plugins/workflow-plugin/templates/progress-driven-delivery.workflow.json \
  || ! rg -q '"from":"validation","to":"classify"' plugins/workflow-plugin/templates/implementation-batch.workflow.json \
  || ! rg -q '"from":"post_format_validation","to":"classify"' plugins/workflow-plugin/templates/implementation-batch.workflow.json \
  || ! rg -q 'WorkflowDescendantRunSummary' packages/workflow-store/src/lib.rs \
  || ! rg -q 'descendant_run_summaries' packages/server/src/lib.rs \
  || ! rg -q 'Runtime-owned counters' plugins/workflow-plugin/src/tui.rs \
  || ! rg -q 'workflow_authoring_catalog' packages/plugin-sdk/src/tui.rs packages/tui/src/plugin_surface_host.rs \
  || ! rg -q 'apply_workflow_authoring_edits' packages/plugin-sdk/src/tui.rs packages/tui/src/plugin_surface_host.rs \
  || ! rg -q 'WorkflowSchemaFormDescription' plugins/workflow-plugin/src/authoring_tui.rs \
  || ! rg -q 'PluginStructuredGenerationRequest' packages/plugin-sdk/src/tui.rs plugins/workflow-plugin/src/authoring_tui.rs \
  || ! rg -q 'MAX_GENERATION_REPAIR_ATTEMPTS: u32 = 3' plugins/workflow-plugin/src/authoring_tui.rs \
  || ! rg -q 'accept_generated_workflow_candidate' packages/plugin-sdk/src/tui.rs packages/tui/src/plugin_surface_host.rs \
  || ! rg -q 'TurnToolPolicy::Disabled' packages/tui/src/plugin_surface_host.rs \
  || ! rg -q 'WorkflowTerminalOutputInspection' packages/ipc/src/lib.rs packages/server/src/lib.rs \
  || ! rg -q 'drive_workflow_run_and_parents\(state, &run_id\)' packages/server/src/lib.rs \
  || ! rg -q 'later_options\["terminal_output"\]' scripts/release-flagship-workflow-lifecycle.sh \
  || ! rg -q 'WorkflowAuthoringDocument' plugins/workflow-plugin/src/authoring_tui.rs \
  || ! rg -q 'flagship_restart_preserves_wait_approval_repeat_and_composed_status' packages/workflow-store/src/lib.rs \
  || ! rg -q 'durable_input_gate_waits_validates_and_activates_successor' packages/workflow-store/src/lib.rs \
  || ! rg -q 'mutation_approval_wait_is_exact_persisted_and_restart_safe' packages/workflow-store/src/lib.rs \
  || ! rg -q 'repeat_restart_cannot_duplicate_or_skip_generation' packages/workflow-store/src/lib.rs; then
  echo "Composable workflow template violation: confined external authoring documents or mutable draft instantiation are incomplete." >&2
  violations=1
fi

if ! grep -F 'The TUI owns terminal canvas layout' "$architecture" >/dev/null \
  || ! grep -F 'Unrestricted JSON Patch is not the primary contract' "$architecture" >/dev/null \
  || ! grep -F 'cannot publish, activate, start, grant permission, persist secrets' "$architecture" >/dev/null; then
  echo "Composable workflow frontend violation: renderer-neutral editing or generated-draft safety is incomplete." >&2
  violations=1
fi

exit "$violations"
