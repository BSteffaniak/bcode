# Shared-wrapper acceptance (reserved validation slot only)

The implementation crates are export-free rlibs. Never recover old tests by
building implementation cdylibs, enabling legacy exports, or skipping a missing
artifact. The selected BMUX packager owns wrapper generation and ABI negotiation.

Generation-only preparation, from this Bcode worktree (no Cargo/native load):

```sh
OUT=$(mktemp -d /tmp/bcode-x1-acceptance.XXXXXX)
export OUT
BMUX_PLUGIN_PACKAGER=/Volumes/ehdd/GitHub/wt-bmux/shared-plugin-architecture/scripts/package-plugin.py \
  python3 -B scripts/package-plugin-acceptance.py --output "$OUT"
```

After reserving the compiler slot, run the same driver with explicit build:

```sh
BMUX_PLUGIN_PACKAGER=/Volumes/ehdd/GitHub/wt-bmux/shared-plugin-architecture/scripts/package-plugin.py \
  nix develop --command python3 -B scripts/package-plugin-acceptance.py \
  --output "$OUT" --build --run-acceptance
nix develop --command cargo test -p bcode_bundled_plugins --lib
```

The driver builds separate scoped-loader implementation/wrapper packages, BMUX
hello, and the REAL `bcode_file_references_plugin` wrapper. It records exact Cargo
JSON artifact paths and SHA-256 identity evidence. Its independent expected IDs
catch file-references accidentally embedding another plugin's declaration. No
artifact is installed, selected in an active daemon, or invoked by this driver.
Both supported ABI versions are checked explicitly (v2/bits15, v3/bits31);
missing v3 declaration callbacks, unknown versions and mismatched bits fail.

Run the static/native scoped-loader commands from the BMUX example's
`ACCEPTANCE.md` using the recorded `BMUX_ACCEPTANCE_NATIVE_ARTIFACT`. Native tests
must run with `--ignored`, not disappear into the default suite's skip count.

## Filesystem/shell scheduler-conformance artifacts

The same build also emits real filesystem/shell wrappers. After an assigned
slot completes the build above, select their exact Cargo JSON paths:

```sh
export BCODE_FILESYSTEM_WRAPPER_ARTIFACT="$(python3 -c \
  'import json,os; print(json.load(open(os.environ["OUT"]+"/acceptance-artifacts.json"))["environment"]["BCODE_FILESYSTEM_WRAPPER_ARTIFACT"])')"
export BCODE_SHELL_WRAPPER_ARTIFACT="$(python3 -c \
  'import json,os; print(json.load(open(os.environ["OUT"]+"/acceptance-artifacts.json"))["environment"]["BCODE_SHELL_WRAPPER_ARTIFACT"])')"
nix develop --command cargo test -p bcode --features embedded-plugins --test embedded_scoped_plugin
```

The embedded fixture now uses `PluginHost::load_native_backend_components` with
unchanged declarations and separate explicit artifact paths. Static hello/shell
cases select the same real shared factories; native filesystem/shell cases use
normal host contribution publication and scheduling. The coordinator supplied
the SDK/runtime dev-dependencies. Run the full embedded test binary above,
including scheduler parity, real shell contributions and filesystem mutation,
denial, cancellation and artifact-retention cases; no missing artifact is skipped.
Source integration is ready; these statements are not a passing execution claim.

Coverage still required before production completion:

- Mixed static/dynamic co-residence through the production host.
- Bcode native file-references activation and real invocation, not just metadata.
- Migrating old assumptions in `packages/bcode/tests/cli_process.rs` and
  `packages/server/src/lib.rs` with their owners. Existing filenames copied into
  test roots do not themselves prove the original artifacts were wrappers.
- Repository-wide fmt/check/clippy/tests per AGENTS.md, in the reserved slot.

These commands are prepared, not compiler-validated. Source-only packaging tests
live in the selected BMUX `scripts/test-package-plugin.py`; they exercise this
Bcode delegate too. Host/loader execution coverage belongs with those lanes;
metadata success is not a substitute. No kernel-authority fallback, provider
call, global runtime change, or managed installation is authorized here.
