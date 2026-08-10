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

if rg -n -i 'progress[-_ ]document|conflict[-_ ]resolution|feature[-_ ]delivery|adversarial[-_ ]review|checkpoint_message|synchronization[-_ ]and[-_ ]push' \
  packages/workflow packages/workflow-store packages/server packages/client packages/cli --glob '*.rs' \
  >/tmp/bcode-composable-procedure-leaks.txt 2>/dev/null; then
  echo "Composable workflow ownership violation: source-authored procedures leaked into generic workflow hosts." >&2
  cat /tmp/bcode-composable-procedure-leaks.txt >&2
  violations=1
fi
rm -f /tmp/bcode-composable-procedure-leaks.txt

if rg -n 'local-composable-workflows-progress\.md|git add --all -- .*local-composable' \
  examples/workflows --glob '*.rs' \
  >/tmp/bcode-composable-example-host-leaks.txt 2>/dev/null; then
  echo "Composable workflow ownership violation: product-facing example policy leaked into Rust." >&2
  cat /tmp/bcode-composable-example-host-leaks.txt >&2
  violations=1
fi
rm -f /tmp/bcode-composable-example-host-leaks.txt

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

if rg -n 'bcode\.(git|review|progress-doc)/|GitWorkflow|ReviewWorkflow|ProgressDocumentWorkflow|workflow_(git|review|progress)' \
  packages/workflow packages/workflow-store packages/server plugins/workflow-plugin \
  --glob '*.rs' --glob '*.toml' \
  >/tmp/bcode-product-workflow-blocks.txt 2>/dev/null; then
  echo "Composable workflow specialization violation: product-specific workflow blocks were reintroduced." >&2
  cat /tmp/bcode-product-workflow-blocks.txt >&2
  violations=1
fi
rm -f /tmp/bcode-product-workflow-blocks.txt

if rg -n 'fn migrate\(|ALTER TABLE workflow_|UPDATE workflow_store_contract SET schema_version' \
  packages/workflow-store/src/lib.rs >/tmp/bcode-workflow-store-compatibility.txt 2>/dev/null; then
  echo "Composable workflow compatibility violation: obsolete store migration paths remain." >&2
  cat /tmp/bcode-workflow-store-compatibility.txt >&2
  violations=1
fi
rm -f /tmp/bcode-workflow-store-compatibility.txt

if ! rg -q 'pub const WORKFLOW_SOURCE_DOCUMENT_VERSION: u32 = 3;' packages/workflow/src/lib.rs; then
  echo "Composable workflow source violation: clean source-v3 boundary drifted." >&2
  violations=1
fi

if ! rg -q 'workflow_prompt_catalog_omits_disabled_and_model_disabled_skills' packages/server/src/lib.rs \
  || ! rg -q 'workflow_prompt_planning_marks_mutating_turns_without_skill_requirements' packages/server/src/lib.rs \
  || ! rg -q 'startup_redispatch_reuses_execution_session_and_turn_admission' packages/server/src/lib.rs \
  || ! rg -q 'fixed_generation_workflow_start_requires_and_pins_exact_parent_generation' packages/server/src/lib.rs \
  || ! rg -q 'shared_parent_workflow_runs_each_agent_node_in_the_initiating_session' packages/server/src/lib.rs \
  || ! rg -q 'active_workflow_prompt_cancellation_reaches_turn_and_durable_attempt' packages/server/src/lib.rs \
  || ! rg -q 'workflow_prompt_completion_maps_provider_cancellation_and_schema_failures' packages/server/src/lib.rs; then
  echo "Composable workflow prompt violation: generic durable prompt execution contracts are missing." >&2
  violations=1
fi

if rg -q 'missing_mutation_verification|validate_mutating_prompt_verification' packages/workflow/src/lib.rs; then
  echo "Composable workflow prompt violation: production admission prescribes post-mutation workflow topology." >&2
  violations=1
fi

for contract in \
  'WORKFLOW_DEFINITION_SCHEMA_VERSION: u32 = 2' \
  'WORKFLOW_AUTHORING_DOCUMENT_VERSION: u32 = 2' \
  'WORKFLOW_PACKAGE_MANIFEST_VERSION: u32 = 3' \
  'WORKFLOW_PACKAGE_LOCK_VERSION: u32 = 4' \
  'WORKFLOW_PACKAGE_CLOSURE_VERSION: u32 = 1' \
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

if ! rg -q 'plan_workflow_package_closure' packages/workflow/src/lib.rs packages/server/src/lib.rs \
  || ! rg -q 'read_workflow_package_closure' packages/cli/src/lib.rs \
  || ! rg -q 'discover_workflow_packages' packages/cli/src/lib.rs \
  || ! rg -q 'WorkflowsConfig' packages/config/src/lib.rs \
  || ! rg -q 'workflow_package_publications' packages/workflow-store/src/lib.rs \
  || ! rg -q 'WorkflowPackagePublicationReceipt' packages/workflow/src/lib.rs \
  || ! rg -q 'StartWorkflowPackageExport' packages/ipc/src/lib.rs packages/server/src/lib.rs; then
  echo "Composable workflow package violation: recursive closure resolution drifted from public package planning." >&2
  violations=1
fi

if ! rg -q 'WorkflowCallConfiguration' packages/workflow/src/lib.rs \
  || ! rg -q 'workflow_run_links' packages/workflow-store/src/lib.rs \
  || ! rg -q 'WorkflowTerminalOutputInspection' packages/ipc/src/lib.rs packages/server/src/lib.rs \
  || ! rg -q 'ResolveApproval' packages/cli/src/lib.rs \
  || ! rg -q 'workflow.approve' plugins/workflow-plugin/src/lib.rs; then
  echo "Composable workflow lifecycle violation: exact child calls or canonical terminal output were removed." >&2
  violations=1
fi

if [[ ! -f examples/workflows/packages/command/package.workflow-package.yaml ]] \
  || [[ ! -f examples/workflows/packages/validation.workflow-package.yaml ]] \
  || [[ ! -f examples/workflows/packages/prompt-verification.workflow-package.yaml ]] \
  || [[ ! -f examples/workflows/packages/review.workflow-package.yaml ]] \
  || [[ ! -f examples/workflows/packages/remediation.workflow-package.yaml ]] \
  || [[ ! -f examples/workflows/packages/repository-recovery.workflow-package.yaml ]] \
  || [[ ! -f examples/workflows/packages/planning.workflow-package.yaml ]] \
  || [[ ! -f examples/workflows/packages/checkpoint.workflow-package.yaml ]] \
  || [[ ! -f examples/workflows/packages/synchronization.workflow-package.yaml ]] \
  || [[ ! -f examples/workflows/packages/sync-recovery.workflow-package.yaml ]] \
  || [[ ! -f examples/workflows/packages/delivery.workflow-package.yaml ]] \
  || ! rg -q 'external_dependencies: \[checkpoint, completion, planning, remediation, review, synchronization, validation\]' examples/workflows/packages/delivery.workflow-package.yaml \
  || ! rg -q 'source: dependency\.request' examples/workflows/packages/delivery/feature-delivery.workflow.yaml \
  || ! rg -Fq 'package_call: {external: checkpoint}' examples/workflows/packages/delivery/feature-delivery.workflow.yaml \
  || ! rg -Fq 'package_call: {external: synchronization}' examples/workflows/packages/delivery/feature-delivery.workflow.yaml \
  || ! rg -Fq 'package_call: {external: validation}' examples/workflows/packages/delivery/feature-delivery.workflow.yaml \
  || ! rg -q 'fixed-generation prompt contexts' examples/workflows/README.md \
  || [[ ! -f examples/workflows/packages/data-quality.workflow-package.yaml ]] \
  || ! rg -q 'external_dependencies: \[inspect, remediate\]' examples/workflows/packages/data-quality.workflow-package.yaml \
  || ! rg -q 'primary_cli_discovers_and_plans_hermetic_external_data_quality_package' packages/cli/src/lib.rs \
  || ! rg -q 'bcode\.shell/exec@1' examples/workflows/packages/command/run-and-assert.workflow.yaml \
  || ! rg -q 'external_dependencies: \[recover, synchronize\]' examples/workflows/packages/sync-recovery.workflow-package.yaml \
  || ! rg -q 'manifest: synchronization\.workflow-package\.yaml' examples/workflows/packages/sync-recovery.workflow-package.yaml \
  || ! rg -q 'manifest: repository-recovery\.workflow-package\.yaml' examples/workflows/packages/sync-recovery.workflow-package.yaml \
  || ! rg -q 'synchronize_after_recovery' examples/workflows/packages/sync-recovery/synchronize-with-recovery.workflow.yaml \
  || ! rg -q 'source: dependency\.request' examples/workflows/packages/sync-recovery/synchronize-with-recovery.workflow.yaml \
  || ! rg -q 'post-recovery synchronization attempt' examples/workflows/README.md \
  || ! rg -q 'GIT_TERMINAL_PROMPT' examples/workflows/packages/synchronization/synchronize.workflow.yaml \
  || ! rg -q 'example-pathspec-replaced-by-typed-input' examples/workflows/packages/checkpoint/checkpoint.workflow.yaml \
  || ! rg -q 'bcode\.completion\.request/v1' examples/workflows/packages/planning/evaluate-completion.workflow.yaml \
  || ! rg -q 'resolve-conflicts' examples/workflows/packages/repository-recovery/resolve.workflow.yaml \
  || ! rg -q 'max_iterations: 3' examples/workflows/packages/remediation/bounded-remediation.workflow.yaml \
  || ! rg -q 'failure_policy: wait_all' examples/workflows/packages/review/review.workflow.yaml \
  || ! rg -q 'bcode\.prompt-verification\.prompt-result/v1' examples/workflows/packages/prompt-verification/prompt-and-verify.workflow.yaml \
  || ! rg -q '^    run:$' examples/workflows/packages/prompt-verification/prompt-and-verify.workflow.yaml \
  || ! rg -q 'bcode\.shell\.exec/v1' examples/workflows/README.md \
  || ! rg -q 'bcode\.shell\.exec-result/v1' examples/workflows/README.md \
  || ! rg -q 'manifest: command/package\.workflow-package\.yaml' examples/workflows/packages/validation.workflow-package.yaml \
  || ! rg -q 'external_dependencies: \[command\]' examples/workflows/packages/validation.workflow-package.yaml; then
  echo "Composable workflow source-package violation: product-facing typed command/validation packages drifted." >&2
  violations=1
fi

if rg -n 'local-composable-workflows-progress\.md' fixtures/workflow-components/checkpoint.workflow.yaml \
  >/tmp/bcode-checkpoint-hardcoded-exclusion.txt 2>/dev/null; then
  echo "Composable workflow checkpoint violation: generic checkpoint fixture hardcodes a progress-document path." >&2
  cat /tmp/bcode-checkpoint-hardcoded-exclusion.txt >&2
  violations=1
fi
rm -f /tmp/bcode-checkpoint-hardcoded-exclusion.txt

if [[ ! -f fixtures/workflow-components/package.workflow-package.yaml ]] \
  || ! rg -q 'run-command-and-assert' fixtures/workflow-components/package.workflow-package.yaml \
  || ! rg -q 'prompt-and-verify' fixtures/workflow-components/package.workflow-package.yaml \
  || ! rg -q 'completion-evaluation' fixtures/workflow-components/package.workflow-package.yaml \
  || ! rg -q 'non-git-data-quality' fixtures/workflow-components/package.workflow-package.yaml \
  || ! rg -q 'feature-delivery' fixtures/workflow-components/package.workflow-package.yaml \
  || ! rg -q 'package_call' fixtures/workflow-components/feature-delivery.workflow.yaml; then
  echo "Composable workflow source-component violation: generic source package inventory drifted." >&2
  violations=1
fi

if rg -ni '(^|[^a-z])git([[:space:]]|[-_./])' fixtures/workflow-components/non-git-data-quality.workflow.yaml \
  >/tmp/bcode-non-git-workflow-leaks.txt 2>/dev/null; then
  echo "Composable workflow generality violation: non-Git example acquired Git semantics." >&2
  cat /tmp/bcode-non-git-workflow-leaks.txt >&2
  violations=1
fi
rm -f /tmp/bcode-non-git-workflow-leaks.txt

if rg -ni '(^|[^a-z])git([[:space:]]|[-_./])' examples/workflows/packages/data-quality \
  examples/workflows/packages/data-quality.workflow-package.yaml \
  >/tmp/bcode-product-non-git-workflow-leaks.txt 2>/dev/null; then
  echo "Composable workflow generality violation: product-facing non-Git package acquired Git semantics." >&2
  cat /tmp/bcode-product-non-git-workflow-leaks.txt >&2
  violations=1
fi
rm -f /tmp/bcode-product-non-git-workflow-leaks.txt

if rg -n 'force-with-lease|force[[:space:]]+push|push[[:space:]]+--force|--force' \
  fixtures/workflow-components examples/workflows --glob '*.yaml' --glob '*.json' \
  >/tmp/bcode-flagship-force-push.txt 2>/dev/null; then
  echo "Composable workflow flagship violation: source package contains force-push behavior." >&2
  cat /tmp/bcode-flagship-force-push.txt >&2
  violations=1
fi
rm -f /tmp/bcode-flagship-force-push.txt

if rg -n 'bcode\.(git|code-review|progress-doc|workflow-product)/' \
  fixtures/workflow-components --glob '*.yaml' --glob '*.json' \
  >/tmp/bcode-flagship-specialized-actions.txt 2>/dev/null; then
  echo "Composable workflow flagship violation: source package uses specialized product blocks." >&2
  cat /tmp/bcode-flagship-specialized-actions.txt >&2
  violations=1
fi
rm -f /tmp/bcode-flagship-specialized-actions.txt

if ! grep -F 'The TUI owns terminal canvas layout' "$architecture" >/dev/null \
  || ! grep -F 'Unrestricted JSON Patch is not the primary contract' "$architecture" >/dev/null; then
  echo "Composable workflow frontend violation: renderer-neutral editing boundaries drifted." >&2
  violations=1
fi

exit "$violations"
