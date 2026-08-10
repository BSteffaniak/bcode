# Reusable source-defined workflow packages

These product-facing examples are ordinary workflow package manifests and source-v3 workflows. They are not loaded from Rust fixtures or interpreted by specialized host code.

## Typed command execution

`command/package.workflow-package.yaml` exports `run-and-assert`. Its public input is the shell owner's versioned `bcode.shell.exec/v1` command-plan schema, including argv arrays, accepted exit codes, environment policy, sequencing, timeouts, and output retention. Its public output is `bcode.shell.exec-result/v1`; callers can use the typed `passed` and per-command facts in deterministic conditions.

## Typed prompt and deterministic verification

`prompt-verification.workflow-package.yaml` exports a bounded mutating prompt followed by a visible source-authored shell verifier. The prompt has an exact typed task input and structured result, optional skills are requested only through instruction text, and the verifier runs through the same shell-owner authorization boundary as ordinary command packages.

## Non-repository data quality

`data-quality.workflow-package.yaml` imports the command and bounded-remediation exports. It runs deterministic inspection before a fresh isolated, tool-free typed assessment, branches through a typed operator decision, and conditionally enters bounded remediation. The workflow contains no repository or version-control assumptions.

## Progress-driven delivery composition

`delivery.workflow-package.yaml` is a product-facing multi-package composition over exact planning, bounded implementation, isolated review, and completion exports. Generic named transforms construct child inputs from typed predecessor state, and an explicit typed operator gate remains available for policy, cancellation, or repair decisions. The broader checkpoint, validation, and synchronization packages remain independently callable as the composition grows toward full release proof.

## Bounded synchronization recovery

`sync-recovery.workflow-package.yaml` imports exactly two named exports: normal synchronization and repository recovery. It conditionally invokes recovery from the shell owner's canonical typed `passed` fact and has no unbounded retry path.

## Normal synchronization

`synchronization.workflow-package.yaml` accepts a complete typed argv plan containing normal pull/push remote and ref policy plus a deterministic postcondition command. The example contains no force-push mode, disables interactive Git prompting, and fails closed on any unaccepted command result.

## Configured checkpoints

`checkpoint.workflow-package.yaml` accepts the complete typed argv plan used for checkpoint creation. Commit messages and exclusion pathspecs are ordinary bounded argv values; neither the package nor generic host embeds a progress-document path.

## Planning and completion evaluation

`planning.workflow-package.yaml` exports a typed path/prompt planning operation and a separate read-only completion evaluator. Planning skills are optional prompt instructions, while completion runs in a fresh isolated context against an exact stop condition and bounded evidence.

## Bounded remediation

`remediation.workflow-package.yaml` carries one exact typed state through a maximum of three mutating prompt executions. The repeat back-edge is durable, bounded by both source and run limits, and fails explicitly on exhaustion.

## Repository recovery

`repository-recovery.workflow-package.yaml` requests the optional `resolve-conflicts` skill only through ordinary prompt text, then verifies both unmerged-index protocols with a source-visible shell check. Skill availability never changes authority or the verifier.

## Isolated adversarial review

`review.workflow-package.yaml` exports two fresh, read-only typed review prompts and a deterministic `wait_all` parallel aggregation. Both reviewers use the ordinary review profile and bounded filesystem tools.

## Validation and formatting composition

`validation.workflow-package.yaml` imports only the command package's `run-and-assert` export. A caller supplies the same typed plan, so formatting and validation commands and their order remain source-defined rather than hardcoded by the workflow host.

Explicit manifest paths work independently of discovery:

```text
bcode workflow package validate examples/workflows/packages/command/package.workflow-package.yaml
bcode workflow package preview examples/workflows/packages/validation.workflow-package.yaml
```

After applying and publishing a package, its named export can be started through `bcode workflow start package-export` with a typed JSON input file. Authorization remains owned by the shell plugin and the selected agent policy.
