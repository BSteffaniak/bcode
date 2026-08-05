# Source-defined workflow authoring

Bcode accepts three explicit source profiles through `bcode workflow author`:

* **Concise pipeline profile v1** uses `workflow_source_version: 1` and ordered `steps`. It remains fully compatible for ordinary source-controlled automation.
* **Structured profile v2** uses `workflow_source_version: 2`, requires stable step IDs, and adds explicit typed prior-step input/condition references, durable input/approval gates, bounded repeats, bounded retry and homogeneous fan-out declarations, fixed two-branch parallel joins, exact immutable workflow calls, and complete canonical agent configurations over the same plugin-owned actions. Agents lower to canonical `WorkflowAgentConfiguration`, exact input/output schemas, resources, tools, skills, models, prompts, timeouts, and context targets. Input and approval declarations lower to canonical typed durable gate nodes with exact schemas and atomic resource claims. A repeat deterministically inserts `<step-id>__repeat`, redirects successors through that controller, and emits the existing bounded back edge; its maximum must not exceed the run cycle cap and its body boundary schemas must match. `fail` exhaustion preserves the body schema, while `emit_outcome` produces the exact versioned typed repeat-outcome boundary and requires every successor to accept that schema. A fixed parallel declaration names exactly two distinct prior dependency exits, derives a closed ordered two-member schema, and lowers to the existing canonical parallel join with explicit `wait_all` or `fail_fast` policy. Workflow calls accept only the existing versioned `WorkflowCallConfiguration`, resolve an exact immutable definition identity from the authoring catalog, and copy its typed boundaries; unavailable or identity-mismatched children fail closed. `input_from` references lower explicit field/index selectors to canonical `WorkflowTransform` values; selected conditions compose the same selector into canonical selector-based predicates. References may target only prior dependencies and traverse only unambiguous exact object/array schemas; unknown fields, union/combinator traversal, missing fields/indices, and schema mismatches fail closed. Retry declarations validate bounded attempts/backoff and only owner-safe failure classes. Homogeneous fan-out declarations validate exact array/member/result schemas, a leaf operation, member/concurrency bounds, and sibling failure policy. Both remain capability-gated until their durable scheduling is production-admitted. Unknown future structured constructs are rejected rather than approximated.
* **Canonical profile** is the complete versioned `WorkflowAuthoringDocument`. Use it for uncommon graph, binding, retry, routing, agent, and presentation capabilities.

Both profiles lower to one canonical `WorkflowAuthoringDocument`. Only that canonical document is persisted, published, and executed. Source paths and raw source text remain client-local.

## Workflow packages

A portable package manifest uses `version: 1`, one stable `package_id`, a bounded non-empty `exports` map, and at most 64 source members. Each member carries a package-local ID, a confined relative diagnostic name with an explicit JSON/YAML/TOML suffix, the matching source format, a bounded source payload, and explicit package-local dependencies. The transported name is never interpreted as a host path; clients must confine and read local files before constructing this contract.

Validation rejects duplicate member IDs or source names, missing exports/dependencies, duplicate dependency entries, cycles, dependency depth beyond eight, excessive members/dependencies/aggregate bytes, path traversal or absolute names, malformed IDs, and unknown future versions. Package source remains an authoring/reproducibility input and is not canonical runtime state.

A version-1 package lock/result records the validated package source digest and, for each deterministically ordered member, its source digest, exact content-derived definition identity, optional successful published revision, and bounded package-local dependency closure. Locks reject malformed/future/duplicate/inconsistent state and never authorize publication or execution. They may be generated or updated only from successful canonical daemon outcomes; their presence is not proof that canonical publication still exists or is valid.

Pure package planning validates the manifest, topologically compiles children before parents, and rewrites `package_call: { member: ... }` only when the target is a declared direct dependency that has compiled successfully. The rewrite uses the exact content-derived canonical definition identity and then runs ordinary source-v2 lowering and recursive catalog preview. The result contains child-before-parent lowering facts plus a deterministic unsigned lock candidate; planning performs no persistence and publication remains a separate transaction.

Portable IPC and client boundaries expose bounded package validation/planning as a daemon-owned computation with the existing deadline/cancellation control. The request transports the manifest and client-read source payloads, never trusted local paths; the response returns the pure typed plan.

Version-1 apply and publish contracts carry exact plans/lock candidates plus optimistic per-member generations. Apply generation facts identify only existing members; omitted members must not already exist. Publication requires one exact generation for every locked member. Typed results distinguish applied, published, conflict, and rejected outcomes: success must contain complete identity-ordered canonical member facts and a matching lock, while conflict/rejection is forbidden from carrying partial mutation facts. These contracts define fail-closed transaction semantics but do not by themselves implement durable atomic storage.

## Formats

JSON, YAML, and TOML are equivalent adapters selected explicitly or by `.json`, `.yaml`, `.yml`, or `.toml`. Bcode never retries another parser after failure. YAML is restricted to bounded JSON-compatible YAML 1.2 values: mapping keys must be strings, and custom tags, anchors, aliases, merge keys, non-finite numbers, and ambiguous unsupported values are rejected. TOML uses the documented `$bcode_null = true` marker where a JSON null is required.

## Concise shell steps

```yaml
workflow_source_version: 1
workflow_id: project/check
title: Check
steps:
  - run: cargo fmt --check
  - run: cargo test --workspace
```

The shell plugin owns `run@1` and `shell.script@1`. On Unix the default interpreter argv is `sh -c`; on Windows it is `cmd /C`. Exit code `0` is accepted by default, timeout defaults to 300,000 ms, output preview defaults to 8,192 bytes with artifact spill enabled, and an unaccepted exit fails the node so later sequential steps do not run.

Advanced form:

```yaml
- id: probe
  run:
    script: ./probe.sh
    shell: [bash, -euo, pipefail, -c]
    cwd: .
    environment:
      MODE: ci
    timeout_ms: 120000
    accepted_exit_codes: [0, 2]
    continue_on_unaccepted_exit: false
    output:
      preview_bytes: 8192
      artifact_spill: true
```

Secret-bearing environment names are rejected; use runtime-owned secret delivery instead.

## Generic typed actions

An exact block may be selected without shorthand:

```yaml
- uses: bcode.example/example.read@1
  with:
    query: status
```

The exact block input schema, effects, resources, authorization, cancellation, and reconciliation remain authoritative. Missing or ambiguous actions and disabled owners fail closed with source-addressed diagnostics.

## Ordering and identity

Omitted step IDs are generated deterministically as `step_0001`, `step_0002`, and so on. Omitted dependencies make each step depend on the immediately preceding step. `needs` may reference only earlier step IDs. Lowering emits a source map from every concise step path to its canonical node.

## CLI lifecycle

```text
bcode workflow author validate workflow.yaml
bcode workflow author preview workflow.yaml
bcode workflow author apply workflow.yaml
```

`apply` uses draft identity `source` unless `--draft-id` is supplied. It creates a new workflow when absent; for an existing source draft it reads one generation and performs exactly one optimistic replacement. A concurrent replacement returns a typed conflict and is never retried or overwritten. If a workflow exists without that draft identity, apply fails and requires an explicit draft choice.

Apply never publishes, activates, starts, or grants authority. Continue through the existing `publish`, `start`, and inspection commands. Preview exposes the lowered canonical digest, exact requirements, effects, permissions, and run limits before publication.
