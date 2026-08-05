# Source-defined workflow authoring

Bcode accepts two explicit source profiles through `bcode workflow author`:

* **Concise pipeline profile** uses `workflow_source_version: 1` and ordered `steps`. It is intended for ordinary source-controlled automation.
* **Canonical profile** is the complete versioned `WorkflowAuthoringDocument`. Use it for uncommon graph, binding, retry, routing, agent, and presentation capabilities.

Both profiles lower to one canonical `WorkflowAuthoringDocument`. Only that canonical document is persisted, published, and executed. Source paths and raw source text remain client-local.

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
