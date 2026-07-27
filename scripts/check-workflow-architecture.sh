#!/usr/bin/env bash
set -euo pipefail

violations=0

if ! rg -q 'state_dir\.join\("workflows"\)\.join\(DATABASE_FILE\)' packages/workflow-store/src/lib.rs; then
  echo "Workflow store ownership violation: workflow-store must own the canonical workflow database path." >&2
  violations=1
fi

if rg -n 'workflow\.db|CREATE TABLE.*workflow_(runs|attempts|activations|outputs)' \
  packages/session plugins/loop-plugin --glob '*.rs' >/tmp/bcode-workflow-architecture-violations 2>/dev/null; then
  echo "Workflow persistence ownership violation: session/loop code must not own workflow database paths or tables." >&2
  cat /tmp/bcode-workflow-architecture-violations >&2
  violations=1
fi
rm -f /tmp/bcode-workflow-architecture-violations

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

exit "$violations"
