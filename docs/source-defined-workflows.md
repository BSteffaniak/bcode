# Source-defined workflow authoring

Bcode accepts one structurally explicit source profile through `bcode workflow author` and package operations. It uses `workflow_source_version: 3`, requires stable step IDs, and supports plugin-owned shorthand actions alongside explicit typed prior-step input/condition references, durable input/approval gates, bounded repeats, bounded retry and homogeneous fan-out declarations, fixed two-branch parallel joins, exact immutable workflow calls, and complete canonical prompt configurations. Source workflows may declare exact versioned top-level `input` and `output` interfaces. Lowering verifies every graph entry accepts the input interface and every successful exit produces the output interface. Existing source-v3 documents without those fields derive them from their graph boundaries, but new reusable workflows should declare them explicitly. Prompt nodes lower to canonical `WorkflowPromptConfiguration` with exact input/output schemas, resources, tools, models, prompts, timeouts, and context targets. Skills, when useful, are requested in prompt text rather than selected by the workflow contract.

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

Package-local calls use `package_call: {member: child}`. Exact external package exports selected by a resolver or lock use `package_call: {external: shared-name}` and the calling member must list that name in `external_dependencies`; the manifest supplies its exact immutable `WorkflowCallTarget`. Both forms lower to the same `WorkflowCallConfiguration` before compilation, so runtime scheduling never distinguishes local from imported source and never re-resolves a target after publication.

`imports` records exact imported package ID, export name, immutable target, and imported lock digest; these facts are copied into the generated package lock. Planning rejects missing target definitions, duplicate import/export bindings, undeclared member use, and unsupported/cyclic definition closure. Source or imported-lock drift changes the package digest rather than silently relocking.

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

## CLI lifecycle

```text
bcode workflow author validate workflow.yaml
bcode workflow author preview workflow.yaml
bcode workflow author apply workflow.yaml
```

`apply` uses draft identity `source` unless `--draft-id` is supplied. It creates a new workflow when absent; for an existing source draft it reads one generation and performs exactly one optimistic replacement. A concurrent replacement returns a typed conflict and is never retried or overwritten. If a workflow exists without that draft identity, apply fails and requires an explicit draft choice.

Apply never publishes, activates, starts, or grants authority. Continue through the existing `publish`, `start`, and inspection commands. Preview exposes the lowered canonical digest, exact requirements, effects, permissions, and run limits before publication.
