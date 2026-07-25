#!/usr/bin/env bash
set -euo pipefail

violations=0
source_file="plugins/loop-plugin/src/lib.rs"

for required in \
  'struct LoopWorkflowInput' \
  'WorkflowSpec<LoopWorkflowIteration>' \
  'PluginWorkflowStartRequest::typed' \
  'host\.start_workflow\(request\.clone\(\)\)\.await' \
  'associated_workflow_run' \
  'control_associated_workflow_run' \
  'fn legacy_state_exists_at' \
  'unsupported_legacy_message'; do
  if ! rg -q "$required" "$source_file"; then
    echo "Loop workflow migration violation: missing required marker: $required" >&2
    violations=1
  fi
done

for forbidden in \
  'struct PendingOperation' \
  'enum OperationKind' \
  'enum OperationStatus' \
  'fn run_loop' \
  'fn run_ordinary_turn' \
  'LegacyRecoveryDisposition' \
  'fn reconcile_pending_operation' \
  'fn save_state' \
  'fn decode_state' \
  'fn load_state_result' \
  'SessionWatchEvent' \
  'ModelTurnOutcome'; do
  if rg -n "$forbidden" "$source_file" >/tmp/bcode-loop-legacy-runtime.txt; then
    echo "Loop workflow migration violation: legacy runtime marker remains: $forbidden" >&2
    cat /tmp/bcode-loop-legacy-runtime.txt >&2
    violations=1
  fi
done
rm -f /tmp/bcode-loop-legacy-runtime.txt

if rg -n 'fs::(write|rename|remove_file|create_dir_all)' "$source_file" \
  | grep -v 'write fixture' >/tmp/bcode-loop-state-mutation.txt; then
  echo "Loop workflow migration violation: loop plugin mutates local loop state." >&2
  cat /tmp/bcode-loop-state-mutation.txt >&2
  violations=1
fi
rm -f /tmp/bcode-loop-state-mutation.txt

if rg -n 'workflow\.db|CREATE TABLE.*workflow_(runs|attempts|activations|outputs)' \
  plugins/loop-plugin --glob '*.rs' >/tmp/bcode-loop-workflow-persistence.txt; then
  echo "Loop workflow migration violation: loop plugin owns generic workflow persistence." >&2
  cat /tmp/bcode-loop-workflow-persistence.txt >&2
  violations=1
fi
rm -f /tmp/bcode-loop-workflow-persistence.txt

exit "$violations"
