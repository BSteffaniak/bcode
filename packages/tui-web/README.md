# Browser-local BMUX session adapter

`bcode_tui_web` runs shared BMUX components in browser-local WASM Canvas.
HyperChad's production Actix integration hosts assets and typed application
requests; it is not an alternative transcript/composer UI. There is no browser
terminal emulator, PTY, shell, host filesystem port, or arbitrary service port.
Plugins retain ownership of domain presentation and behavior.

## Build and packaged launch

```sh
bash scripts/build-tui-web.sh NEW_OUTPUT_DIRECTORY
```

The output must not already exist, including as a dangling symlink. The script
enters the Nix flake toolchain, resolves the remote `bmux_web_renderer` through
locked Cargo metadata, copies that revision's `interactive-surface.js`, and uses
the lockfile's exact wasm-bindgen CLI version. When the Bcode toolchain lacks
WASM std, it enters the resolved BMUX flake. It does not change Cargo's target
directory or use a sibling checkout. Generation occurs in a private sibling
staging directory; publication refuses to replace an existing output.

Manifest schema 2 identifies the bytes of six assets using SHA-256 and lengths,
and records the remote BMUX source, wasm-bindgen version and CLI executable hash.
Its compatibility tuple is `session_view: 1`, `browser_bridge: 1`, `canvas: 1`.
These hashes detect corruption/mixing; they are not publisher authentication.
The manifest is an asset inventory, not an asserted native-parity feature list.

For a checkout-built executable with the web-renderer feature, use:

```sh
/path/to/bcode web --tui-assets /absolute/path/to/NEW_OUTPUT_DIRECTORY
```

A web-enabled release places those same files in `bcode-runtimes/tui` beside
`bcode`. Its normal `bcode web` launch discovers them automatically; it does not
need checkout paths or an acceptance-only route. The command opens the private
launch URL through the platform browser opener; console output contains only a
redacted address, not a reusable capability URL. The browser receives a scoped
session launch token, not a daemon credential. Bootstrap removes the token from
browser history.

Static embedding additionally sets `backend` in the launch hash and supplies
trusted `globalThis.bcodeTuiConfig.allowedBackendOrigins` before bootstrap. The
backend independently needs explicit CORS policy. Never accept a URL-owned
allowlist. `src/browser-session.js` exports `mountBrowserSession` for hosts.

## Capability and evidence matrix

Source presence is not acceptance. In particular, neither a source module nor
an old built artifact establishes that the latest source passes browser E2E.

| Area | Implementation boundary | Required evidence |
| --- | --- | --- |
| Composer and transcript | Shared BMUX editor, `TranscriptViewport`, semantic Markdown and retained surface composition | Real WASM editing, rendering, resize, selection/copy and committed hit geometry |
| Plugin visuals and interaction | Typed plugin-owned presentation and question ports; no browser-side domain replacement | Plugin fixture rendering, focus, validation and authoritative resolution |
| Images | Browser artifact/image adapter and retained compositor | Real image load, clipping, resize, stale identity rejection and disposal |
| Policy, history and actions | Typed browser policy/session seams | Applied theme/keymap revisions, paging and action identity enforcement |
| Submission durability | Browser session transport and application receipts | Exact retry identity, durable receipt, restart and stale response rejection |
| Distribution | Cargo-resolved bridge, exact bindgen ABI, manifest and packaged asset discovery | Fresh build, archived/extracted byte equality, tamper/mixed-protocol rejection, rollback and normal packaged launch |

Full artifact download is a bounded complete Raw-byte response, with a 64 MiB
limit, not a server-streaming interface. Do not confuse that download operation
with bounded artifact preview ranges or claim unbounded streaming support.

This table specifies acceptance obligations, not a claim that all scenarios have
passed. Full generalized parity must not be inferred from safe rejection of a
required feature. Cells, hits, focus, selection and images must use one committed
compositor geometry. Unknown revisions/identities fail closed rather than being
reinterpreted as supported older state.

## Validation and rollback

Run all build/test commands under the Nix toolchain:

* `nix develop --command cargo test --locked -p bcode_tui_web`
* `nix develop --command cargo clippy -p bcode_tui_web --all-targets -- -D warnings`
* `nix develop --command cargo check --locked -p bcode_tui_web --target wasm32-unknown-unknown --features wasm`
* `node --test packages/tui-web/tests/browser-session.test.mjs`
* `python3 scripts/test-tui-web-e2e.py --self-test --bundle /absolute/real-bundle`

The production fixture requires explicit `BCODE_E2E_BUNDLE`,
`BCODE_E2E_CHROMIUM` and `BCODE_E2E_PYTHON`. No fake WASM is a substitute:

```sh
BCODE_E2E_BUNDLE=/absolute/extracted/bcode-browser-bundle \
BCODE_E2E_CHROMIUM=/absolute/chromium BCODE_E2E_PYTHON=/absolute/python3 \
  nix develop --command cargo test --locked -p bcode_hyperchad \
  --features renderer-html-actix --test browser_e2e \
  real_wasm_registered_router_durable_retry_and_restart -- --ignored --exact --nocapture
```

That fixture exercises production Actix with an isolated fake provider; by itself
it does not prove the extracted CLI's normal packaged launch. Release verification
currently exercises negative packaged discovery: a deliberately mismatched asset
must fail before daemon startup, rather than downgrade to another frontend.
Successful packaged launch needs separate browser evidence: invoke the extracted
executable's ordinary `web --session UUID` without `--tui-assets`, capture its
normal platform-opener URL privately, and verify the adjacent assets, scoped
session and authenticated Canvas load. A test must not print that capability URL.
The fixture must seed its session through the same extracted executable's matching
daemon and explicit test configuration, not an integration-test daemon.

Packaged-launch coverage must use the same archived assets and isolated test state,
never the active daemon/session. CI archives the fresh bundle before acceptance,
extracts it, compares bytes, and supplies the extracted path to distribution and
browser tests. Release jobs consume the accepted archive rather than rebuilding
assets.

Rollback selects a complete previously accepted executable/assets archive with
matching protocols and daemon artifact identity. Do not replace individual JS,
WASM or manifest files in a running distribution. Preserve existing state and
keep different executable artifact identities isolated. The xtask real-bundle
test covers extracted byte equality, rejected extra entries/links, asset tamper,
exact restoration and mixed protocols; it requires `BCODE_E2E_BUNDLE` explicitly.
