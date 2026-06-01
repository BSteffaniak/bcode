# Bcode Agent Guidelines

This file defines required project conventions and validation expectations for coding agents working in this repository.

## Product Architecture Direction

Bcode is a Rust-native, TUI-first, plugin-driven coding agent with a local client/server architecture.

### Hard Rules

- Do not create speculative empty crates.
- Add crates only when implementation needs them.
- Do not create generic `core`, `shared`, `common`, or similarly vague crates.
- Crates must be domain-driven and named for the capability/domain they own.
- Domain behavior should live in plugins whenever practical.
- Plugins are first-class and may implement critical product behavior.
- Prefer plugin/service extension points over hardcoded behavior.
- Bundled plugins should be enabled by default, but fully disableable.
- Keep Bcode independently usable even as BMUX integration grows.

## Workspace Organization

- Root workspace configuration lives in `Cargo.toml`.
- Package crates belong under `packages/<domain>/` when they are needed.
- Bundled plugins belong under `plugins/<domain>-plugin/` when they are needed.
- Do not add placeholder package directories.
- Do not create a crate just because a future roadmap item may need it.

## `_models` Crate Pattern

When a package defines types that other packages need without depending on the full implementation crate, extract the shared types into a sibling `models/` crate.

Example:

- `packages/session/`
- `packages/session/models/`

`_models` crates may contain:

- structs
- enums
- type aliases
- serialization/deserialization derives
- `From`/`Into` implementations
- simple parsing or utility functions on owned types

`_models` crates must not contain:

- business logic
- service orchestration
- plugin loading
- model-provider implementation
- TUI logic
- database queries
- network clients
- heavy dependencies

`_models` crates should be leaves in the dependency graph. They may depend on other `_models` crates, but must not depend on their parent implementation crate.

Never create generic shared type crates. Types belong in the domain-specific `_models` crate for the package that owns them.

## Rust Code Style

### Collections

- Prefer `BTreeMap`/`BTreeSet` over `HashMap`/`HashSet`.

### Dependencies

- Add new dependencies to the root workspace `Cargo.toml`.
- Package manifests should use `workspace = true` for dependency versions.
- Avoid inline dependency versions in package manifests.
- Use full versions including patch numbers.
- Prefer `default-features = false` for new dependencies where practical.
- Verify new dependency versions are current stable releases before adding them.

### Crate Features

Every crate should include:

```toml
[features]
default = []
fail-on-warnings = []
```

### Clippy

Every Rust crate should include these crate-level attributes unless there is a documented reason not to:

```rust
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
```

### `#[must_use]`

- Add `#[must_use]` to constructors and getters returning types other than `Result` or `Option`.
- Do not add `#[must_use]` to functions returning `Result` or `Option`; those types are already marked must-use.

### Documentation

- Document public APIs.
- Error documentation should clearly list error conditions.
- Use asterisks (`*`) for Rustdoc bullet lists.

## Session Persistence Architecture

- Follow [`docs/session-persistence-architecture.md`](docs/session-persistence-architecture.md) for session store changes.
- Catalog discovery must be best-effort, bounded, and non-mutating for damaged sessions.
- Catalog code may do first-event discovery for display metadata, but must not write partial indexes for logs with unknown tails.
- Normal catalog/open/attach/history/model-context paths must not full-replay event logs or run repair rebuilds.
- Full replay/rebuild behavior belongs behind explicit repair, doctor, reindex, or migration commands.
- Missing, stale, corrupt, or inconsistent indexes should surface degraded or repair-required state unless they can be caught up incrementally from trustworthy sidecars.

## Plugin Architecture Expectations

- Initial plugin runtime is native Rust dynamic libraries.
- Plugin discovery should be manifest-driven.
- Plugin interfaces should be versioned.
- Plugin services should use typed, serializable request/response payloads.
- Permission, provider, tool, command, UI contribution, and integration behavior should be plugin-owned wherever practical.
- Host/runtime crates should provide plumbing and routing, not product-specific behavior.

## Validation Rules

If code changes are made, run relevant checks before finishing and report exactly what ran and whether it passed.

Minimum expected validation once Rust crates exist:

- `cargo fmt`
- `cargo check --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `scripts/check-session-architecture.sh` for session persistence changes
- relevant `cargo test` commands for changed packages
- relevant plugin rebuild/check commands when plugin crates exist

Prefer `cargo fmt` over `cargo fmt --check` during normal implementation work. Agents should apply formatting directly and leave changed Rust files formatted. Use `cargo fmt --check` only for read-only/plan-only sessions, CI-style verification, or when explicitly asked not to modify files.

Treat clippy warnings as blocking. Fix root causes rather than adding broad suppressions.

For docs-only changes, no runtime validation is required.

If a required command cannot be run, explain why in the final response.

