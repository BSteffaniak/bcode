# Progress-driven feature-delivery package

`package.workflow-package.yaml` exports `feature-delivery`, a flagship workflow composed only from
portable source-v3 primitives and exact package-local child calls. It is an editable example, not a
host-owned template or specialized workflow service.

## Configuration

The source declares bounded configuration for the progress-document path, implementation prompt,
validation-command inventory, checkpoint message, checkpoint exclusion paths, and optional model/provider selection. Defaults
are visible in `feature-delivery.workflow.yaml`. Dynamic values are not interpolated into shell
source; the shipped commands use fixed source text and bounded environment values.

## Flow

1. A generic `run` step verifies the progress document and repository instructions.
2. A mutating prompt performs one coherent implementation tranche. It requests
   `resume-progress-doc` through prompt text and uses ordinary tools and permissions.
3. A generic shell step verifies the progress document remains untracked, checks conflicts, and
   runs `git diff --check`.
4. Exact package-local calls run validation/formatting, isolated adversarial review, and completion
   evaluation components.
5. A durable typed interaction asks the operator whether to continue, remediate, resolve conflicts,
   refocus, checkpoint, synchronize, stop, cancel, or require repair.
6. Typed conditional approval branches surface the selected policy-sensitive action. The reusable
   checkpoint, synchronization, conflict-resolution, and refocus child components remain available
   in the same package for the next bounded run/tranche.

## Models, tools, skills, and permissions

Prompt nodes select the ordinary `build` or `review` profiles. Provider and model use normal catalog
resolution; the example does not hardcode provider wire details. Tool allowlists are explicit and
skills are requested only in prompt text. Skills never grant authority. Shell commands go through
owner preparation and normal command policy; mutating prompts and repository writes remain subject
to ordinary authorization. Operator gates do not bypass those decisions.

## Limits and isolation

The root source caps duration, node executions, concurrency, cycles, and retries. Mutating work uses
a fixed-generation fork. Reviews use fresh isolated contexts. Component-local timeouts and resource
claims are visible in their YAML sources. Exhaustion, ambiguity, authorization waits, cancellation,
and explicit policy choices surface through durable status/interactions rather than blind retries.

## Status, cancellation, and repair

Package validation, preview, apply, publish, start, status, interaction, cancellation, and repair use
Bcode's public workflow commands and canonical store. Cancellation propagates through prompt, shell,
and child-call boundaries. Ambiguous mutation results fail closed. Restart resumes persisted waits
and child identities. Repair and incompatible-store reset remain explicit maintenance operations;
normal reads never migrate or rebuild history.

## Checkpoint and synchronization safety

`checkpoint.workflow.yaml` is a legacy test fixture for a concrete no-exclusion checkpoint. Product-facing checkpoint composition receives its complete argv inventory, including exclusion pathspecs, through typed input; the generic runtime does not select or interpret any progress-document path.
`synchronization-and-push.workflow.yaml` uses normal `git pull --rebase --autostash` and `git push`;
it contains no history-rewriting push mode. Command failures return through typed shell results and
require a later bounded conflict/synchronization decision rather than unbounded retry.
