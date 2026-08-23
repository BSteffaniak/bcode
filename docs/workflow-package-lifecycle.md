# Workflow package lifecycle and runtime semantics

This document describes the public, bounded package and execution path used by source-defined workflows. The portable source contract is documented in [`source-defined-workflows.md`](source-defined-workflows.md); generic ownership boundaries are documented in [`composable-coding-workflows.md`](composable-coding-workflows.md).

## Discovery, precedence, and confinement

Explicit manifest paths are always supported. Repository discovery checks `.bcode/workflows` before `workflows`; configured roots follow repository roots, then user configuration and state roots. Equal-precedence duplicate package identities are ambiguous and fail closed. Discovery is bounded by the requested result limit.

Filesystem discovery is owned by `bcode_workflow_discovery` and orchestrated through workflow
application operations. CLI, TUI, and future clients consume the same versioned launch-catalog
contract. The bounded catalog includes package exports, standalone `*.workflow.{json,yaml,yml,toml}`
sources not owned as package members, and available plugin templates. Discovery itself is read-only:
it does not apply drafts, publish revisions, grant authority, repair state, or start runs.

An explicit source path is resolved through the same application operation but is not added to the
automatic catalog. The exact file is canonicalized, must be a supported workflow source, and is
confined as one requested source rather than used as a root for adjacent traversal. This keeps
outside-root workflows usable without turning a path selection into repository-wide discovery or a
persistent import. Apply/import remains a separate explicit lifecycle operation.

The CLI's `bcode workflow package discover` command and `/workflow` both request this same catalog;
they do not maintain independent precedence, package-member suppression, ambiguity, or confinement
rules. CLI output is the renderer-neutral catalog JSON, including stable source identity, readiness,
diagnostics, schemas, requirements, effects, permissions, and pagination cursor.

Manifests and members are canonicalized and confined to their authorized package root. Import paths are relative normal-component JSON, YAML, or TOML paths. Absolute paths, parent traversal, symlink escape, duplicate canonical manifests, cycles, excessive closure depth, and unreachable closure members are rejected.

## Imports, locks, publication, and drift

An import names only `package_id`, `export`, and an import-local identity. Closure planning resolves dependencies before importers and replaces each source import with an exact immutable definition identity and package-lock digest. Runtime calls never resolve manifests or mutable export names.

Validation and preview do not persist. Apply atomically stages every member with optimistic generation facts. Publish atomically verifies all staged generations and the expected lock before creating immutable revisions and a publication receipt. Any conflict rolls back the whole transaction. Later source or lock disagreement is reported as drift; normal reads never silently relock or republish.

Updates are explicit: validate and preview the new closure, apply with exact expected generations, review the new lock, then publish it. Existing runs retain their published definition, child targets, configuration, workspace, authorization profile, and limits.

## Public lifecycle

The public CLI path is:

```text
bcode workflow package discover --workspace PATH
bcode workflow package validate MANIFEST
bcode workflow package preview MANIFEST
bcode workflow package apply MANIFEST
bcode workflow package publish --lock LOCK_JSON --expected-generation MEMBER=GENERATION
bcode workflow start package-export --package-id ID --export NAME --parent-session-id SESSION --input INPUT.json
bcode workflow inspect-run --run-id RUN
bcode workflow provide-input ...
bcode workflow resolve-approval ...
bcode workflow cancel ...
```

Plugin and frontend surfaces call the same portable client/application operations. They do not open the workflow database or depend on private daemon types.

## Descendants, waits, cancellation, and terminal state

Child calls use deterministic run identities derived from the root, parent activation, attempt, and exact target. Depth, descendant count, node execution, cycles, retries, concurrency, duration, and retained data are bounded. Restart restores persisted child links and loop generations rather than creating replacement children.

Input, approval, mutation-approval, and authorization waits are durable typed records. Duplicate resolution with the same value is safe; conflicting duplicate resolution fails closed.

Cancellation intent is persisted before propagation. It reaches scheduling, prompt turns, shell/plugin owners, prepared attempts, and descendants. Owner propagation is retryable after transient failure. Once authoritative terminal state is recorded, stale delivery cannot reopen it.

## Failure, ambiguity, and repair

Read-only and explicitly idempotent work can be safely reconciled according to owner contracts. A mutation with an unprovable commit, rebase, pull, push, or external side effect becomes `repair_required`; it is never blindly replayed. Doctor, reconciliation, repair, and incompatible-store reset are explicit maintenance operations. Normal discover, status, inspect, history, attach, and rendering paths remain bounded and non-mutating.

## Skills, permissions, and frontend boundaries

Skills are optional prompt instructions. Missing or disabled skills do not alter workflow topology and never grant tools or authority. Prompt turns use ordinary agent profiles, model resolution, tool allowlists, and permission policy. Shell execution is prepared and authorized from canonical command facts by the shell owner before side effects.

Public snapshots expose portable run state, typed waits, descendants, and terminal output. Renderers may adapt those semantics but do not own canonical state, execution policy, or plugin workflows.
