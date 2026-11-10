# Developing Bcode against a local BMUX checkout

Bcode consumes BMUX through workspace `[patch.crates-io]` git entries on BMUX `master`, and
`Cargo.lock` pins the verified revision. To change BMUX and Bcode together, temporarily point
every patched `bmux_*` crate at a local BMUX checkout with `scripts/bmux-dev.sh`.

## Loop

```sh
scripts/bmux-dev.sh on [path]   # default: $BMUX_DEV_PATH or ~/GitHub/wt-bmux/robust-themes
scripts/bmux-dev.sh status      # override on/off, BMUX path and HEAD, lockfile state
# edit BMUX and Bcode; cargo check/test/clippy resolve BMUX from the local checkout
scripts/bmux-dev.sh off         # restore the git patches and Cargo.lock
```

After the BMUX change is pushed to BMUX `master`:

```sh
scripts/bmux-dev.sh update      # off, then `cargo update -p bmux_tui`; reports the new revision
```

Commit the resulting `Cargo.lock` revision bump with the Bcode change that needs it. `update`
warns when the local BMUX `HEAD` is not the newly locked revision (for example, when it was not
pushed yet). `cargo update` may also re-resolve unrelated lock entries; review the lock diff.

## How it works

* `on` maps each crate named in the workspace `[patch.crates-io]` table to its BMUX package
  directory by reading every BMUX manifest's `[package] name`; a missing or duplicated name is an
  error. It rewrites only those `bmux_*` patch lines to absolute `path` patches, each marked with a
  trailing `# bmux-dev:` comment, then re-resolves `Cargo.lock` and verifies every `bmux_*` package
  resolves from the local path. Running `on` again regenerates the lines (also for a new path).
* The original patch lines, a pre-`on` copy of `Cargo.lock`, a fingerprint of uncommitted
  `Cargo.toml` edits, and a fingerprint of the re-resolved `Cargo.lock` without its `bmux_*`
  entries are kept in git-ignored files under `.cargo/bmux-dev.*`.
* `off` restores only the marked lines (recorded lines first, `HEAD`'s otherwise), leaving other
  manifest edits untouched. It then restores `Cargo.lock` from `HEAD`, or from the pre-`on` copy
  when the lock already had uncommitted changes. It refuses to restore the lock when manifests
  changed while the override was on, when lock entries other than `bmux_*` packages changed while
  it was on (for example `cargo update -p <crate>`, or a dependency the local BMUX checkout
  added), or when no record exists and the lock differs from `HEAD` beyond BMUX sources.
  `off --force` restores it from `HEAD` anyway and keeps the discarded lock at
  `.cargo/bmux-dev.Cargo.lock.discarded`. Redo intended lock changes after `off`.

The override is in the tracked manifest rather than in an untracked Cargo config file on purpose.
A config `[patch]` does take precedence over manifest patches, but Cargo discovers configuration
from the working directory of every invocation, including nested builds below `target/`.
`bmux_mermaid_render`'s build script runs a `--locked` guest workspace build from its `OUT_DIR`;
it would inherit the config patches as unused patches, require a lockfile update, and fail.
Manifest patches apply only to the Bcode workspace.

## Guard

`cargo xtask bmux-lock-check` fails when a workspace `bmux_*` patch is not a git patch, or when
`Cargo.lock` holds a `bmux_*` package whose source is not the git source that patch declares
(path-patched, registry, unpinned, or another repository/reference). `cargo xtask architecture`
runs the same check first and reports its failures even when `--locked` metadata resolution then
fails, so CI rejects a commit made while the override was on.
