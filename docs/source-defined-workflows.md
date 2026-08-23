# Source-defined workflow authoring

Bcode accepts one structurally explicit source profile through `bcode workflow author` and package operations. It uses `workflow_source_version: 3`, requires stable step IDs, and supports plugin-owned shorthand actions alongside explicit typed prior-step input/condition references, durable input/approval gates, bounded repeats, bounded retry and homogeneous fan-out declarations, fixed two-branch parallel joins, exact immutable workflow calls, and complete canonical prompt configurations. Source workflows may declare exact versioned top-level `input` and `output` interfaces. Lowering verifies every graph entry accepts the input interface and every successful exit produces the output interface. Existing source-v3 documents without those fields derive them from their graph boundaries, but new reusable workflows should declare them explicitly. Prompt nodes lower to canonical `WorkflowPromptConfiguration` with exact input/output schemas, resources, tools, models, prompts, timeouts, and context targets. A prompt output policy is either `structured`, whose canonical schema must equal the node output, or `preserve_input`, whose node output must equal its input and which performs no structured-result finalization. Provider-native and adapter-mediated result mechanisms are selected below this source contract. Skills, when useful, are requested in prompt text rather than selected by the workflow contract.

Versions 1 and 2 are intentionally unsupported. They are rejected as old contracts rather than migrated, guessed, or interpreted as version 3. Canonical `WorkflowAuthoringDocument` input remains a separate explicit profile selected by `schema_version`; a source document must declare exactly one profile marker.

### Explicit workflow interfaces

Reusable source workflows should declare exact versioned interfaces:

```yaml
input:
  type_name: example.request/v1
  schema:
    type: object
    additionalProperties: false
    required: [message]
    properties:
      message: {type: string}
output:
  type_name: example.result/v1
  schema:
    type: object
    additionalProperties: false
    required: [accepted]
    properties:
      accepted: {type: boolean}
```

Every graph entry must accept the declared input exactly, and every successful graph exit must emit the declared output exactly. Multiple entries or exits are valid only when all respective boundary schemas agree. A mismatch fails during source lowering and canonical definition validation rather than being selected by graph order. Source-v3 documents that omit these fields retain their existing derived-boundary semantics; explicit interfaces are required for maintainable callable workflows.

Input and approval declarations lower to canonical typed durable gate nodes with exact schemas and atomic resource claims. A repeat deterministically inserts `<step-id>__repeat`, redirects successors through that controller, and emits the bounded back edge; its maximum must not exceed the run cycle cap and its body boundary schemas must match. `fail` exhaustion preserves the body schema, while `emit_outcome` produces the exact versioned typed repeat-outcome boundary. Parallel declarations name exactly two distinct prior dependency exits and lower to canonical joins with explicit policy. Workflow calls target exact immutable identities. Selectors traverse only unambiguous exact schemas. Retry and fan-out declarations validate fully but remain capability-gated until their durable schedulers are production-admitted. Unknown future constructs are rejected rather than approximated.

## Formats

JSON, YAML, and TOML are equivalent adapters selected explicitly or by `.json`, `.yaml`, `.yml`, or `.toml`. Bcode never retries another parser after failure. YAML is restricted to bounded JSON-compatible YAML 1.2 values: mapping keys must be strings, and custom tags, anchors, aliases, merge keys, non-finite numbers, and ambiguous unsupported values are rejected. TOML uses the documented `$bcode_null = true` marker where a JSON null is required.

### Generic input expressions

A step may construct its exact typed input with `input_expression`. Expressions use the existing bounded transform evaluator and can combine constants, arrays, objects, merge/default/increment operations, immutable root-run input (`state`), persisted authored configuration (`configuration`), and exact named predecessor outputs (`dependency.<step-id>`). Every named predecessor must be a prior declared dependency, and the expression output interface must exactly equal the target node input interface.

```yaml
- id: assess
  needs: [inspect]
  input_expression:
    version: 2
    expression:
      operation: object
      fields:
        mode: {operation: input, source: configuration, path: mode}
        request: {operation: input, source: state, path: request}
        inspection:
          operation: selected_input
          source: dependency.inspect
          selector:
            version: 1
            segments: [{kind: field, name: result}]
        attempt:
          operation: increment
          value: {operation: constant, value: 0}
          by: 1
    output:
      type_name: example.assessment-request/v1
      schema: {type: object}
  prompt: # ... exact prompt contract
```

Named dependency values come from canonical persisted activation outputs. Restarts therefore evaluate the same expression from the same root input, configuration, and predecessor values; source lowering rejects unknown, forward, or undeclared dependency names before publication.

Package-local calls use `package_call: {member: child}`. Imported calls use `package_call: {external: shared-name}` and the calling member lists that name in `external_dependencies`. A source import declares `import_id`, `package_id`, `export`, and a confined relative `manifest` path; a resolver recursively loads that package and replaces the source import with its exact immutable `WorkflowCallTarget` and generated lock digest. Exact external dependencies may instead provide `target` plus `package_lock_digest_sha256` directly. Both forms lower to the same `WorkflowCallConfiguration`, so runtime scheduling never distinguishes local from imported source and never re-resolves a target after publication.

Explicit manifest paths always work. `bcode workflow package discover --workspace <path>` searches roots anchored to that canonical workspace in deterministic order: `.bcode/workflows`, `workflows`, configured `[workflows].paths`, user config workflows, then user state workflows. Lower numeric precedence wins; duplicate package IDs at one precedence fail as ambiguous instead of being selected silently. `[workflows]` can independently disable repository or user roots. Durable typed waits are resolved through `bcode workflow provide-input` and `bcode workflow resolve-approval`; the workflow plugin exposes the same input, approve, deny, and existing mutation-approval operations through portable client APIs.

The portable closure request carries the explicit entry package plus its complete bounded transitive source inventory. Explicit manifest paths work from any caller-authorized root, and every canonicalized manifest/member must remain below that root. Daemon planning rejects absent or unreachable packages, duplicate identities or source labels, cycles, missing exports, excessive depth/packages/edges/bytes, and exact import facts that drift from resolved source. Dependencies are planned before importers, and exact package ID, export, target, and imported-lock digest facts are copied into every generated package lock without silent relocking. Package lock v4 also records each named export's exact member and definition identity; publication atomically fills exact authored revisions and persists a bounded lock-digest publication receipt with those export facts. `bcode workflow start package-export --package-id <id> --export <name>` resolves that receipt (optionally pinned by `--package-lock-digest-sha256`) and starts only its exact authored revision through the ordinary application authorization and run path.

A `workflow_call` uses the same expression contract for an optional exact child `input` mapping and an optional child-result `output` mapping. Input mappings must produce the published child input interface and may use declared predecessor, configuration, and root sources. Output mappings may use only `current`, the canonical validated child terminal result, and their declared output becomes the parent call node's exact output interface. Both mappings are embedded in the immutable compiled call configuration and therefore survive restart without re-resolution.

## Concise shell steps

```yaml
workflow_source_version: 3
workflow_id: project/check
title: Check
steps:
  - id: format
    run: cargo fmt --check
  - id: test
    run: cargo test --workspace
```

Each step has a stable `id`. The shell plugin owns `run@1` and `exec@1`. On Unix the default interpreter argv is `sh -c`; on Windows it is `cmd /C`. Exit code `0` is accepted by default, timeout defaults to 300,000 ms, output preview defaults to 8,192 bytes with artifact spill enabled, and an unaccepted exit fails the node so later sequential steps do not run.

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

## Package and example composition

Product-facing packages live under `examples/workflows/packages`. Each package declares complete source-visible schemas, prompts, argv, limits, resources, conditions, and failure policy. The delivery example composes exact planning, remediation, review, and completion exports through named typed bindings. The data-quality example demonstrates the same architecture without repository or version-control assumptions.

For discovery precedence, confinement, package locks, publication, restart, cancellation, waits, repair, and the complete public lifecycle, see [`workflow-package-lifecycle.md`](workflow-package-lifecycle.md).

Supported source files placed directly under configured workflow roots are standalone launch-catalog
entries unless a package manifest owns them as members. JSON, YAML, YML, and TOML suffixes use the
same source-v3 lowering and diagnostics as explicit authoring commands. Outside-root files are never
found by broad scanning: clients must request exact explicit-path inspection and then separately
choose whether to apply/import, publish, or start through canonical lifecycle operations.

`bcode workflow package discover` exposes the same bounded renderer-neutral launch catalog used by
`/workflow`, including package exports, standalone sources, and plugin templates. It is a read-only
parity entry point, not a second discovery implementation.

## CLI lifecycle

```text
bcode workflow author validate workflow.yaml
bcode workflow author preview workflow.yaml
bcode workflow author apply workflow.yaml
```

`apply` uses draft identity `source` unless `--draft-id` is supplied. It creates a new workflow when absent; for an existing source draft it reads one generation and performs exactly one optimistic replacement. A concurrent replacement returns a typed conflict and is never retried or overwritten. If a workflow exists without that draft identity, apply fails and requires an explicit draft choice.

Apply never publishes, activates, starts, or grants authority. Continue through the existing `publish`, `start`, and inspection commands. Preview exposes the lowered canonical digest, exact requirements, effects, permissions, and run limits before publication.
