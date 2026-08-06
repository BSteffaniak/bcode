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

if rg -n 'SHELL_COMMAND_PLAN_VERSION_1|command-plan/v1|command-plan-result/v1|continue_on_nonzero' \
  plugins/shell-plugin packages/plugin --glob '*.rs' --glob '*.toml' \
  >/tmp/bcode-legacy-shell-workflow-contracts.txt 2>/dev/null; then
  echo "Composable workflow shell violation: retired shell workflow contracts remain." >&2
  cat /tmp/bcode-legacy-shell-workflow-contracts.txt >&2
  violations=1
fi
rm -f /tmp/bcode-legacy-shell-workflow-contracts.txt

if ! rg -q 'block_id[[:space:]]*= "exec"' plugins/shell-plugin/bcode-plugin.toml \
  || ! rg -q 'block_version[[:space:]]*= 1' plugins/shell-plugin/bcode-plugin.toml \
  || ! rg -q 'target_block[[:space:]]*= "bcode.shell/exec@1"' plugins/shell-plugin/bcode-plugin.toml; then
  echo "Composable workflow shell violation: current argv plan or run shorthand is missing." >&2
  violations=1
fi

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

if rg -n 'workflow_plugin_skill_roots|plugin:bcode\.workflow|workflow-plugin/skills|AgentSkillSelection|AgentSkillActivationMode|SkillSelection|required_skills|compilation_bindings|workflow_agent_skill' \
  packages/workflow packages/plugin packages/server plugins/workflow-plugin --glob '*.rs' \
  >/tmp/bcode-workflow-skill-coupling.txt 2>/dev/null; then
  echo "Composable workflow skill violation: workflow-specific skill discovery remains." >&2
  cat /tmp/bcode-workflow-skill-coupling.txt >&2
  violations=1
fi
rm -f /tmp/bcode-workflow-skill-coupling.txt

if rg -n 'WORKFLOW_SOURCE_V1_DOCUMENT_VERSION|WorkflowSourceDocument|WorkflowSourceStep|WorkflowSourceProfile::Concise' \
  packages plugins --glob '*.rs' \
  >/tmp/bcode-workflow-legacy-source.txt 2>/dev/null \
  || rg -n 'workflow_source_version[[:space:]]*[:=][[:space:]]*[12]' \
    fixtures --glob '*.json' --glob '*.yaml' --glob '*.toml' \
    >>/tmp/bcode-workflow-legacy-source.txt 2>/dev/null; then
  echo "Composable workflow source violation: retired workflow source contracts remain." >&2
  cat /tmp/bcode-workflow-legacy-source.txt >&2
  violations=1
fi
rm -f /tmp/bcode-workflow-legacy-source.txt

if ! rg -q 'pub const WORKFLOW_SOURCE_DOCUMENT_VERSION: u32 = 3;' packages/workflow/src/lib.rs; then
  echo "Composable workflow source violation: clean source-v3 boundary drifted." >&2
  violations=1
fi

if ! rg -q 'missing_mutation_verification' packages/workflow/src/lib.rs \
  || ! rg -q 'WorkflowBlockEffect::ReadOnly' packages/workflow/src/lib.rs \
  || ! rg -q 'workflow_prompt_catalog_omits_disabled_and_model_disabled_skills' packages/server/src/lib.rs \
  || ! rg -q 'workflow_prompt_planning_marks_mutating_turns_without_skill_requirements' packages/server/src/lib.rs \
  || ! rg -q 'startup_redispatch_reuses_execution_session_and_turn_admission' packages/server/src/lib.rs \
  || ! rg -q 'fixed_generation_workflow_start_requires_and_pins_exact_parent_generation' packages/server/src/lib.rs \
  || ! rg -q 'shared_parent_workflow_runs_each_agent_node_in_the_initiating_session' packages/server/src/lib.rs \
  || ! rg -q 'active_workflow_prompt_cancellation_reaches_turn_and_durable_attempt' packages/server/src/lib.rs \
  || ! rg -q 'workflow_prompt_completion_maps_provider_cancellation_and_schema_failures' packages/server/src/lib.rs; then
  echo "Composable workflow prompt violation: deterministic post-mutation verification admission is missing." >&2
  violations=1
fi

for contract in \
  'WORKFLOW_DEFINITION_SCHEMA_VERSION: u32 = 2' \
  'WORKFLOW_AUTHORING_DOCUMENT_VERSION: u32 = 2' \
  'WORKFLOW_PACKAGE_MANIFEST_VERSION: u32 = 2' \
  'WORKFLOW_PACKAGE_LOCK_VERSION: u32 = 2' \
  'WORKFLOW_PROMPT_CONFIGURATION_VERSION: u32 = 2'; do
  if ! rg -q "$contract" packages/workflow/src/lib.rs; then
    echo "Composable workflow schema violation: missing clean contract $contract." >&2
    violations=1
  fi
done

if rg -n 'WorkflowAgentConfiguration|AgentStructuredOutputPolicy|WORKFLOW_AGENT_CONFIGURATION_VERSION|WorkflowStructuredSourceAgent' \
  packages plugins --glob '*.rs' >/tmp/bcode-workflow-legacy-prompt.txt 2>/dev/null; then
  echo "Composable workflow prompt violation: retired agent-shaped workflow contracts remain." >&2
  cat /tmp/bcode-workflow-legacy-prompt.txt >&2
  violations=1
fi
rm -f /tmp/bcode-workflow-legacy-prompt.txt

if ! rg -q 'action_key[[:space:]]*=[[:space:]]*"run"' plugins/shell-plugin/bcode-plugin.toml \
  || ! rg -q 'bcode\.shell/exec@1' plugins/shell-plugin/bcode-plugin.toml; then
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
