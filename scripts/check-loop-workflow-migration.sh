#!/usr/bin/env bash
set -euo pipefail

violations=0
source_file="plugins/loop-plugin/src/lib.rs"

for required in \
  'struct LoopWorkflowInput' \
  'workflow_definition: Option<bcode_workflow::WorkflowDefinition>' \
  'workflow_initial_value: Option<LoopWorkflowIteration>' \
  'host\.start_workflow\(request\)\.await' \
  'fn prepare_legacy_resume' \
  'fn is_legacy_loop_state'; do
  if ! rg -q "$required" "$source_file"; then
    echo "Loop workflow migration violation: missing required marker: $required" >&2
    violations=1
  fi
done

if rg -n 'host\.spawn\(Box::pin\(run_loop\(state\)\)\)' "$source_file" >/tmp/bcode-loop-new-start-legacy.txt; then
  echo "Loop workflow migration violation: the start surface routes new loops to the legacy scheduler." >&2
  cat /tmp/bcode-loop-new-start-legacy.txt >&2
  violations=1
fi
rm -f /tmp/bcode-loop-new-start-legacy.txt

if rg -n 'workflow\.db|CREATE TABLE.*workflow_(runs|attempts|activations|outputs)' \
  plugins/loop-plugin --glob '*.rs' >/tmp/bcode-loop-workflow-persistence.txt; then
  echo "Loop workflow migration violation: loop plugin owns generic workflow persistence." >&2
  cat /tmp/bcode-loop-workflow-persistence.txt >&2
  violations=1
fi
rm -f /tmp/bcode-loop-workflow-persistence.txt

exit "$violations"
