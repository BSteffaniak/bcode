# State Locations

A Bcode **state location** is one directory tree that owns durable Bcode state. Bcode resolves
exactly one *primary* state location per process, and that location owns new session creation and all
non-session durable state.

Configuration is deliberately not part of this: `bcode.toml` discovery already layers user, repo, and
`.bcode/` files plus `BCODE_CONFIG` / `BCODE_CONFIG_TOML`, so config works from anywhere. Durable
state is different, because it carries ownership, locking, and canonical-authority semantics.

## Selecting a location

Precedence, highest first:

1. `--state-root <DIR>` — an explicit absolute root for this invocation.
2. `--state-profile <NAME>` — a named `[state.profile.<name>]` entry.
3. `BCODE_STATE_DIR` — environment selection.
4. `[state] default_profile` — a named profile from configuration.
5. `[state] root` — a declared absolute root.
6. The XDG-derived default: `$XDG_STATE_HOME/bcode`, else `$HOME/.local/state/bcode`.

`--state-root` and `--state-profile` are mutually exclusive and are global flags, so they may appear
before or after a subcommand.

The canonical session store root is resolved separately, highest first:

1. `BCODE_SESSION_STORE_DIR`.
2. The selected profile's `sessions_root`.
3. `[state] sessions_root`.
4. `<primary root>/sessions`.

This split is the point of the feature: canonical session storage can live on a large or external
volume while sockets, daemon images, and logs stay local.

## Configuration

```toml
[state]
root = "/Volumes/big/bcode-state"
sessions_root = "/Volumes/big/bcode-sessions"
default_profile = "big"
readable_profiles = ["big", "local"]

[state.profile.big]
root = "/Volumes/big/bcode-state"
sessions_root = "/Volumes/big/bcode-sessions"

[state.profile.local]
root = "/Users/me/.local/state/bcode"
```

## What lives where

State-root-owned, and therefore **not** affected by `sessions_root`:

```text
<state-root>/
  daemons/                 daemon registry records and startup locks
  daemon-images/           cached daemon executables
  logs/                    daemon logs
  settings.db              interactive settings
  permissions.toml         runtime permission state
  tui.toml                 interactive TUI state
  workflows/workflow.db    workflow persistence
  traces/                  diagnostic traces
  derived/                 regenerable derived data, including search indexes
  ralph/                   loop state
```

Session-root-owned:

```text
<sessions-root>/
  <session-id>/
    session.db             canonical authoritative history
    manifest.json          derived discovery cache
  session-artifacts/
    <session-id>/          session-owned tool artifacts
  catalog.db               derived summary cache
  leases/                  live compatibility-owner metadata
  locks/                   cross-process coordination
```

Session artifacts are a *sibling* of the canonical `<session-id>/` directory rather than nested
inside it. Session-migration backup walks the canonical directory recursively, so nesting bulk
artifact bytes there would make every canonical backup copy them. Canonical discovery only accepts
directory names that parse as a session ID, so the named sibling is ignored by catalog scans.

Derived state is deliberately kept under the state root. It is regenerable, so it is rebuilt at a
destination rather than migrated.

## Failure behavior

Resolution validates that a location is absolute, reachable, a directory, and writable. On failure it
returns a typed error and **never substitutes a different location**. Falling back to the XDG default
when an external volume is unmounted would manufacture a second canonical storage path for the same
session IDs, which `A session has one canonical storage path` forbids.

## Daemon isolation

Each **runtime scope** gets its own daemon. A runtime scope is the pair
`(state root, config directory)`: the state root selects which canonical session storage the daemon
owns, and the config directory selects which plugins, permissions, and model policy it applies. A
digest of that canonicalized pair is folded into the IPC endpoint derivation, so two scopes never
share a socket or a daemon registry.

Config directory is part of the fingerprint for correctness, not tidiness: two config directories can
select different plugins and permission policy, so a shared daemon would apply the wrong policy to
one of them.

Because scoping is per daemon, a daemon never opens a session belonging to another scope. Opening a
session under a different state root or config directory starts a separate daemon that owns its own
leases and locks, so no cross-root coordination state is ever shared — the condition
`Daemon state locations are isolated` requires.

Both handshake directions verify it. The client refuses a daemon reporting a different
`state_location_id`, and the daemon refuses a client that reports one, because endpoint scoping alone
can be bypassed with an explicit `BCODE_IPC_ENDPOINT` or `BCODE_SOCKET` override. A peer that
advertises no identity is treated as unverifiable rather than compatible, per
`Sensitive ambiguity fails closed`.

Spawned daemons receive the resolved state root, session root, and config directory explicitly rather
than inheriting them, so a command-line selection is not silently lost across the process boundary.

`state_location_id` is carried by IPC protocol 31 and daemon record schema 5. Protocol 30 and record
schema 4 carried the narrower state-root-only identity; a stale peer computing it now mismatches and
is refused rather than silently sharing a daemon across config directories.

## Aggregated discovery

`[state] readable_profiles` lists locations whose sessions participate in aggregated discovery.
Aggregation is read-only: per `Aggregated session discovery does not confer authority`, a session's
mutations, ownership, and repair apply only to the location that owns its canonical storage. When more
than one location claims the same session ID, the conflict is surfaced and no location is opened as
authoritative until an explicit maintenance operation resolves it.
