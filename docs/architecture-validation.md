# Architecture validation

Architecture is validated through types, domain-owned behavioral tests, and a small resolved
Cargo dependency check. Repository source text is not an executable contract. Renaming a helper,
reformatting a function, or moving a test should not require an architecture test update.

## Dependency checks

Run `cargo run -p xtask -- architecture [--target <triple>]` and
`cargo test -p xtask architecture::tests`. CI checks Linux, macOS, and Windows target graphs.
No cross-target compilation is needed for graph checks.

The command reads Cargo metadata, follows package IDs (including renamed dependencies), ignores
dev-only edges, includes build edges, and reports a dependency path for each violation. It checks:

* Model packages cannot reach their implementation parent, selected host/persistence/network
  implementations, or terminal renderers.
* The router engine cannot reach concrete router services or Bcode hosts.
* Portable session-view and HyperChad packages cannot reach terminal renderers.
* Minimal app and CLI profiles exclude heavyweight optional implementations; distribution and
  web-enabled profiles include their intended implementations.
* The bundled-plugin registry without features excludes search backends.
* Production/build graphs exclude nonportable `sha2-asm`.

Workspace-default resolution checks model/router boundaries. Portable frontend and feature profiles
use temporary consumer workspaces so unrelated workspace members cannot unify their features.
These probes resolve metadata only; they are not product crates or compiled fixtures. They seed
resolution with the repository lockfile and resolve offline after the workspace resolution. The
repository lockfile is never rewritten by the probes. Missing cached dependencies fail explicitly.
This is a selected configuration matrix, not proof for every possible feature combination.

Rules are ordinary Rust predicates, not a rule language or an exhaustive permitted-edge snapshot.
Tests exercise graph traversal with renamed, transitive, build-only, dev-only, and cyclic edges.
The graph cannot detect business logic hidden inside an otherwise permitted crate.

## Existing blocker surfaced by the checker

Isolated default-feature profiles currently expose:

* `bcode_session_view -> bcode_ipc -> bcode_plugin -> bmux_plugin -> bmux_tui`
* `bcode_hyperchad -> bcode_ipc -> bcode_plugin -> bmux_plugin -> bmux_tui`

These are reported failures, not allowed exceptions. Restoring portable boundaries requires a
separate IPC/plugin dependency refactor; source-scanner removal does not authorize weakening the
renderer-neutrality invariant. The new CI job therefore remains blocking until those paths are fixed.

The minimal app already uses the bundled-plugin registry and `zstd` through `bmux_codec`.
Registry plumbing and codec compression are not evidence that optional search providers are enabled;
the checker bans concrete optional implementations instead of those overbroad package exclusions.

## Behavioral and compiler protection

Keep tests in the domain that owns the behavior. Existing lease/process tests, artifact identity
matrix, offline prompt-cache evals, adapter fixtures, and theme lifecycle tests remain. Registrar
`Send` and `Sync` restrictions are checked with compile-fail doctests, independently for each trait.
The compiler's existing `unsafe_op_in_unsafe_fn` denial remains in plugin boundary crates.

For further coverage, test actual failure modes: permission decisions preceding effects, damaged
session reads remaining non-mutating, bounded history access, terminal outcome stability, and
unrelated capabilities working with plugins disabled. Do not create a replacement test for every
removed source assertion or a central test-name inventory.

Shell wrappers that actually build artifacts, run evals, or inspect resolved dependency output are
not source-contract tests. The existing Windows dependency wrapper remains usable from release CI;
its duplicate manifest-spelling assertion was removed.
