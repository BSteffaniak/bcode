# Dynamic plugin packaging

Bcode plugin crates package themselves as dynamic libraries. No wrapper crate is
generated or checked in. The packaging runtime belongs to BMUX. Bcode owns the
product author layer (`bcode_plugin_sdk::define_plugin!`) and the plugin crates.

## How a crate packages itself

- The crate stays `[lib] crate-type = ["rlib"]` and declares the two role
  features. Workspace builds never enable them:

  ```toml
  [features]
  dynamic-backend  = []
  dynamic-frontend = []
  ```

- `define_plugin!` emits `plugin_definition()` and passes it to BMUX's
  `packaged_entry!`, which it reaches through `bcode_plugin_sdk`, so plugin
  crates never name `bmux_plugin_sdk`. The entry symbol exists only when BMUX's
  packager builds the crate. The packager runs
  `cargo rustc --lib --crate-type cdylib --no-default-features --features
  <component features>,dynamic-<role>` with `--cfg bmux_plugin_dylib`. The
  entry exports only that role, as BMUX's `plugin!` does. Crates never name the
  cfg.
- Component features come from `[package.metadata.bmux-plugin.components.<role>]`
  (for example `features = ["native"]`), because packaged builds disable
  default features.
- The packager links the one `bmux_plugin_sdk` in the crate's normal
  dependency graph, as its own Cargo.lock resolves it. For Bcode that is the
  git-pinned BMUX revision, reached through `bcode_plugin_sdk`.
- Dylib builds use `target/plugin-dylib`, so the cfg never rebuilds the
  workspace target.

## Scripts

All scripts need Python 3.11+. Run them from the repository development shell.

- `scripts/package-plugin.py`: BMUX's packager, from the checkout of the
  `bmux_plugin_sdk` this workspace resolves (`cargo metadata`).
  `BMUX_PLUGIN_PACKAGER` overrides it only to test other tooling.
- `scripts/package-plugin-fixtures.py`: packages the dynamic test fixtures and
  exports their paths (below).
- `scripts/stage-plugin.py`: packages one crate's backend and stages it with its
  verbatim `bcode-plugin.toml`, as the smoke scripts need.
- `scripts/install-bundled-plugins.py`: packages bundled crates and publishes
  immutable artifacts (`--install-root`).
- `scripts/package-plugin-acceptance.py`: runs BMUX's acceptance driver over
  this worktree.

Package one crate directly:

```sh
python3 scripts/package-plugin.py \
  --implementation-manifest examples/hello-plugin/Cargo.toml \
  --component backend --build
```

## Test fixtures

Tests that load real dynamic plugins read each artifact from an environment
variable:

- `BCODE_HELLO_BACKEND_ARTIFACT`: `examples/hello-plugin` backend (with `tui`)
- `BCODE_HELLO_FRONTEND_ARTIFACT`: `examples/hello-plugin` frontend
- `BCODE_QUESTION_BACKEND_ARTIFACT`: `plugins/question-plugin` backend
- `BCODE_FILESYSTEM_BACKEND_ARTIFACT`: `plugins/filesystem-plugin` backend
- `BCODE_SHELL_BACKEND_ARTIFACT`: `plugins/shell-plugin` backend

`cargo nextest run` sets them up automatically. A nextest setup script in
`.config/nextest.toml` runs `scripts/package-plugin-fixtures.py --nextest-env`
for exactly those tests. Packaging is incremental; the first run compiles the
fixtures' dependency graph once into `target/plugin-dylib`. Artifacts are copied
to `target/plugin-fixtures/<fixture>/`, and `target/plugin-fixtures/artifacts.json`
records the variables.

For `cargo test`, export the variables first:

```sh
eval "$(python3 scripts/package-plugin-fixtures.py --print-env)"
cargo test -p bcode_plugin
```

A missing variable fails the test with this instruction. Tests never fall back
to an implementation library.
