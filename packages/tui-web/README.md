# Browser-local BMUX session adapter

`bcode_tui_web` is an explicit TUI adapter, not a portable frontend contract.
It consumes full semantic session-view observations and renders shared BMUX
composer and native Markdown components through `LocalSurface` directly to Canvas.
The real BMUX editor and Bcode `TranscriptViewport` are shared with native.

## Executable package

Run `bash scripts/build-tui-web.sh NEW_OUTPUT_DIRECTORY` in the Bcode checkout.
The script enters its Nix environment, checks wasm-bindgen CLI/lockfile ABI,
resolves the BMUX bridge through Cargo metadata and writes a fresh bundle with
SHA-256 manifest. Existing output directories are never overwritten.

Serve the bundle through HyperChad's `--tui-assets DIR` integration. Launch
`index.html#session=UUID&token=LAUNCH_TOKEN`; bootstrap removes the token from
browser history. Static embedding additionally sets `backend` in the hash and
supplies trusted `globalThis.bcodeTuiConfig.allowedBackendOrigins` before bootstrap.
The backend independently needs an explicit CORS policy. Never supply a URL-owned
allowlist. `src/browser-session.js` also exports `mountBrowserSession` for hosts.

Fetches use no-store, omit credentials, reject redirects and suppress referrers.
Responses are incrementally bounded to 2 MiB, requests to 64 KiB. Original JSON is
passed directly to Rust to preserve u64 revisions. Unresolved submissions retain
exact identity and payload; only a matching durable receipt clears the exact
draft. Epoch changes require remounting; no durable transport resume is claimed.
All async WASM operations are serialized. Disposal aborts fetches, removes event
listeners, waits for presentation and frees retained state.

## Current capability boundary

Shared `TranscriptMarkdown` projections retain canonical source, item ID/revision,
contributions, anchors and selection provenance. Preparation happens on semantic
or width changes, outside paint. The native `append_markdown_block` heading/body
recipe and `MarkdownBody` component render accepted rows, source fragments and
action hits. BMUX selection resolves canonical source and code-fence ranges for
explicit copy. Application key overrides enter through the shared binding seam.
The browser projection preview is bounded to 64 KiB; complete semantic source is
retained and omitted projection is explicitly labeled.

This is **not full native parity**. Remaining gaps: plugin visual adapter selection
and complete retained plugin surfaces, tool/permission/interaction controls,
artifact images, history paging, native theme/config transport, contributed native
keymaps/chords, projection worker scheduling and segment reveal policy. Link/action
hits are emitted but browser activation is not implemented. Native platform
services require injected ports, not daemon/filesystem dependencies in WASM.
Opaque plugin snapshots are retained atomically, never flattened into fake parity.

Next extraction: shared native semantic-document item dispatch/cache acceptance,
plugin surface acceptance, indexed resident layout and action adapters. Both hosts
must compose that same transcript child through `session_ui::compose`.

## Validation

* `cargo test -p bcode_tui_web`
* `cargo clippy -p bcode_tui_web --all-targets -- -D warnings`
* `cargo check -p bcode_tui_web --target wasm32-unknown-unknown --features wasm`
* `node --test packages/tui-web/tests/browser-session.test.mjs`

`tests/chromium-smoke.html` exercises a locally served `artifacts/consumer-3`
bundle: real WASM Canvas rendering, grapheme editing, explicit Enter/Ctrl+Enter
bindings, zero/tiny resize and disposal. Build that distribution before running
this harness; it is not a full backend or native-parity acceptance suite.

The root Cargo patches track BMUX and MoosicBox `master`, including their shared
dependency closure; `Cargo.lock` records the resolved commits. No sibling worktrees
are needed. The build script
uses the Cargo-resolved BMUX bridge and, when the native shell lacks WASM std, its
locked Nix toolchain. Assets and tool binaries remain ignored build artifacts.
