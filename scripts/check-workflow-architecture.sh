#!/usr/bin/env bash
set -euo pipefail

violations=0

if ! rg -q 'state_dir\.join\("workflows"\)\.join\(DATABASE_FILE\)' packages/workflow-store/src/lib.rs; then
  echo "Workflow store ownership violation: workflow-store must own the canonical workflow database path." >&2
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
  || ! rg -q 'Active-run pinning' docs/runtime-workflow-authoring.md \
  || ! rg -q 'Producer-neutral authoring' docs/runtime-workflow-authoring.md \
  || ! rg -q 'Import and export' docs/runtime-workflow-authoring.md; then
  echo "Workflow authoring architecture violation: source/compile/run, pinning, producer, and import boundaries must remain documented." >&2
  violations=1
fi

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
  || ! rg -q 'explicit_grant_required = true' plugins/git-plugin/bcode-plugin.toml \
  || ! rg -q 'reconciliation[[:space:]]*=[[:space:]]*"repair_required"' plugins/git-plugin/bcode-plugin.toml; then
  echo "Workflow mutation contract violation: mutating shell/Git blocks require exact grants and safe reconciliation." >&2
  violations=1
fi

exit "$violations"
