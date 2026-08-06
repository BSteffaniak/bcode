#!/usr/bin/env bash
set -euo pipefail

violations=0
architecture="docs/composable-coding-workflows.md"

for boundary in \
  'Generic shell and prompt composition' \
  'Shell authorization' \
  'Prompt-based skill use' \
  'Typed dynamic bindings' \
  'Reusable source components'; do
  if [[ ! -f "$architecture" ]] || ! grep -F "$boundary" "$architecture" >/dev/null; then
    echo "Composable workflow architecture violation: missing generic boundary: $boundary" >&2
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

for manifest in \
  plugins/git-plugin/bcode-plugin.toml \
  plugins/code-review-plugin/bcode-plugin.toml \
  plugins/workflow-plugin/bcode-plugin.toml; do
  if rg -n 'bcode\.workflow-block/v1|workflow_blocks|workflow_authoring_actions' "$manifest" \
    >/tmp/bcode-specialized-workflow-services.txt 2>/dev/null; then
    echo "Composable workflow specialization violation: product plugin contributes workflow behavior: $manifest" >&2
    cat /tmp/bcode-specialized-workflow-services.txt >&2
    violations=1
  fi
done
rm -f /tmp/bcode-specialized-workflow-services.txt

if [[ -e plugins/progress-doc-plugin ]] \
  || [[ -e plugins/workflow-plugin/skills ]] \
  || [[ -e plugins/workflow-plugin/templates ]]; then
  echo "Composable workflow specialization violation: retired progress, skill, or template assets remain." >&2
  violations=1
fi

if rg -n 'workflow_plugin_skill_roots|plugin:bcode\.workflow|workflow-plugin/skills' \
  packages/server plugins/workflow-plugin --glob '*.rs' \
  >/tmp/bcode-workflow-skill-coupling.txt 2>/dev/null; then
  echo "Composable workflow skill violation: workflow-specific skill discovery remains." >&2
  cat /tmp/bcode-workflow-skill-coupling.txt >&2
  violations=1
fi
rm -f /tmp/bcode-workflow-skill-coupling.txt

if ! rg -q 'action_key[[:space:]]*=[[:space:]]*"run"' plugins/shell-plugin/bcode-plugin.toml \
  || ! rg -q 'bcode\.shell/shell\.script@1' plugins/shell-plugin/bcode-plugin.toml; then
  echo "Composable workflow shell violation: generic source run ownership drifted." >&2
  violations=1
fi

if ! rg -q 'format_skill_catalog_for_prompt' packages/server/src/lib.rs \
  || ! rg -q 'skill_source_roots_from_config' packages/server/src/lib.rs; then
  echo "Composable workflow prompt-skill violation: ordinary prompt skill catalog discovery drifted." >&2
  violations=1
fi

if ! rg -q 'WorkflowValueSelector' packages/workflow/src/lib.rs \
  || ! rg -q 'WorkflowTransformExpression' packages/workflow/src/lib.rs \
  || ! rg -q 'WorkflowStructuredSourceCondition' packages/workflow/src/lib.rs; then
  echo "Composable workflow dataflow violation: typed selectors, transforms, or conditions were removed." >&2
  violations=1
fi

if ! rg -q 'WorkflowCallConfiguration' packages/workflow/src/lib.rs \
  || ! rg -q 'workflow_run_links' packages/workflow-store/src/lib.rs \
  || ! rg -q 'WorkflowTerminalOutputInspection' packages/ipc/src/lib.rs packages/server/src/lib.rs; then
  echo "Composable workflow lifecycle violation: exact child calls or canonical terminal output were removed." >&2
  violations=1
fi

if ! grep -F 'The TUI owns terminal canvas layout' "$architecture" >/dev/null \
  || ! grep -F 'Unrestricted JSON Patch is not the primary contract' "$architecture" >/dev/null; then
  echo "Composable workflow frontend violation: renderer-neutral editing boundaries drifted." >&2
  violations=1
fi

exit "$violations"
