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
  || ! grep -F 'Full content does not enter routine public diagnostics or metrics' "$architecture" >/dev/null; then
  echo "Composable workflow progress-document violation: approval, confinement, or diagnostic safety is incomplete." >&2
  violations=1
fi

if ! grep -F 'The TUI owns terminal canvas layout' "$architecture" >/dev/null \
  || ! grep -F 'Unrestricted JSON Patch is not the primary contract' "$architecture" >/dev/null \
  || ! grep -F 'cannot publish, activate, start, grant permission, persist secrets' "$architecture" >/dev/null; then
  echo "Composable workflow frontend violation: renderer-neutral editing or generated-draft safety is incomplete." >&2
  violations=1
fi

exit "$violations"
