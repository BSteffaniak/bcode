# Vox: Bcode integration handoff

## Implemented plugin boundary

`plugins/vox-plugin` owns native discovery, five user commands, a versioned
control service, and a direct client-local adapter. There are **no tools**, model
calls, transcript responses, device payloads, clipboard effects, process launches,
or implicit fallback paths in this plugin.

Public Rust APIs:

* `PLUGIN_ID = "bcode.vox"`; `commands() -> Vec<CommandContribution>` contributes
  `vox.start`, `vox.stop`, `vox.toggle`, `vox.cancel`, `vox.status` to palette and
  slash discovery (`/vox-start`, `/vox-stop`, `/vox-toggle`, `/vox-cancel`,
  `/vox-status`). Arguments are rejected. No active session is required.
* `VoxPlugin: RustPlugin` registers these commands. Its native service export
  always calls `service::dispatch(request, None)`. Thus daemon actions fail with
  `local_frontend_required`; daemon status says unavailable, never synthetic Idle.
* `service::INTERFACE_ID = "bcode.vox.control/v1"`, `VERSION = 1`.
  `action` takes `ActionRequest { version, action: contracts::Action }`, returns
  `ActionResponse { version, accepted }`. Acceptance does **not** mean successful
  transcription/delivery. `status` takes `StatusRequest { version }`, returns
  `StatusResponse { version, local_frontend, status: Option<contracts::Status> }`.
  Typed control requests deny unknown fields, reject unsupported versions and
  operations, and enforce a 4096-byte bound. Never retry mutations automatically.
* `service::LocalControl` has `action(&mut self, Action) -> bcode_vox::Result<()>`
  and `status(&self) -> bcode_vox::Result<Status>`. All failures and Failed status
  diagnostics are normalized rather than reflecting backend strings.
* `frontend::LocalFrontend<C: LocalControl>::new(control)` binds an explicitly
  local resource without capture. `invoke(&mut self, &ServiceRequest)` dispatches
  either the control interface or existing `bcode.command/v1:invoke` payloads.
  `shutdown()` sends Cancel, once, and reports failure. The adapter is !Send/!Sync;
  create and consume it on a dedicated **client-local** worker, not the daemon or
  the UI event loop. Drop alone does not claim successful cancellation.
* `frontend::LocalService: Default + LocalControl` consumes the real
  `bcode_vox::service::{action,status}` IPC APIs. It connects only to Vox's local
  owner-checked endpoint and never starts the standalone service. IPC owns peer,
  framing, and version checks. A transport error may follow an executed mutation;
  do not resubmit it. This adapter deliberately does not offer automatic recovery.
* `LocalControl for bcode_vox::runtime::Handle` is also implemented using its
  actual `action(&self, Action) -> Result<Status>` and `status(&self) -> Status`
  APIs. This supports a deliberately frontend-owned embedded runtime. The plugin
  never constructs a runtime implicitly. The frontend remains responsible for
  the runtime handle's full shutdown/lifetime after adapter cancellation.

Prefer `LocalFrontend::new(LocalService)` to consume an explicitly started local
Vox service. The standalone user's config determines delivery (typing by default,
clipboard only by explicit configuration); Bcode does not override it. Embedded
Handle mode must use an explicit validated local config. Host delivery/final-text
composer insertion is **not implemented** here; do not select Host delivery unless
the frontend also consumes the runtime's final result. No transcript goes into
status, command messages, logs, artifacts, or daemon IPC.

## Coordinator manifest changes (not made by this writer)

Create `plugins/vox-plugin/Cargo.toml` with package name `bcode_vox_plugin`, normal
workspace package metadata, `[lib] crate-type = ["cdylib", "rlib"]`, and:

```toml
[dependencies]
bcode_command = { workspace = true }
bcode_plugin_sdk = { workspace = true }
bcode_vox = { workspace = true }
serde = { workspace = true, features = ["derive"] }
serde_json = { workspace = true }

[features]
default = []
fail-on-warnings = []
static-bundled = []
```

Add the workspace member/path dependency and follow bundled usage-plugin wiring:
`packages/bundled-plugins/Cargo.toml` optional dependency and default-enabled,
fully disableable `static-bundled-vox-plugin` feature enabling
`bcode_vox_plugin/static-bundled`; register `bcode_vox_plugin::static_plugin()` in
the existing bundled static entry inventory. Inspect that inventory's current
location before editing; do not duplicate one. No dependency versions are newly
selected here. `bcode-plugin.toml` is present with ABI 5 and the Linux `.so` name;
non-Linux packaging must select its actual artifact filename by the established
packaging convention. No shared module declaration changes are needed in Vox.

## Generic frontend routing: required coordinator patch

Existing `RustPlugin::invoke_service` runs in the daemon. `PluginApplication`
methods in `packages/plugin-sdk/src/application.rs` describe daemon application
routing and do not prove locality. Neither payload fields, workspace hints, nor
SSH presence may select microphone location. Do not install this adapter in
`BcodeClient`'s daemon transport or forward actions as a fallback.

Add a generic local frontend provider registry/registration contract, owned by
the SDK/client frontend boundary, not a Vox-specific branch. Minimal contract:

* registration has enabled plugin identity, exact interface identities and
  command IDs, a local provider factory, and cancellation/shutdown lifecycle;
* request entry requires an explicit frontend user-command origin (not a public
  model-callable service/tool route); the host chooses provider from enabled
  discovery, not from request data; duplicate ownership rejects registration;
* run a provider factory on a bounded client-local worker; retain that instance
  on its worker for all requests; bounded single-flight commands, no retries;
* distinguish NotOwned from selected-provider failure: only NotOwned may use
  ordinary daemon routing; unavailable/failed local providers never fall back;
* cancellation and frontend detach explicitly close the local provider. Do not
  broadcast controls to other attached clients or assume session ID grants access.

Exact existing integration points inspected:

1. `packages/plugin-sdk/src/lib.rs`: `StaticPluginVtable` and static registration
   plumbing beside CLI/TUI lifecycle registrations can carry an optional generic
   local provider registration. Native dynamic registration needs an equally
   explicit client-local lifecycle API; do not serialize Rust trait objects or
   reuse the daemon native instance. An initial static implementation may reject
   dynamic local actions safely until this is available.
2. `packages/tui/src/effects.rs`: `async fn invoke_plugin_command` (near line 32)
   and the `TuiEffect::InvokePluginCommand` execution arm (near line 2385) currently
   invoke through `BcodeClient`. Select an enabled generic local provider before
   daemon transport; execute on its bounded worker and preserve the existing
   `InvokeCommandResponse` / `PluginCommandInvoked` result path. Do not call
   `LocalService` synchronously in the BMUX update/render thread.
3. `packages/tui/src/root_program.rs`: the `PluginTuiAction::InvokePluginCommand`
   and slash-command effect construction paths already converge on that effect;
   retain that convergence. Bind local lifecycle at frontend startup/shutdown,
   not per session and not per command. Status polling must be bounded/coalesced.
4. Plugin-owned registration factory for this registry should construct
   `LocalFrontend::new(LocalService)` on the selected client worker and declare
   `bcode.vox.control/v1` plus the five `commands()` IDs. No host-side parsing of
   Vox action names is needed. The factory entry cannot be implemented against
   today's SDK because this generic registry contract does not exist yet.

Until that plumbing exists, native discovery and fail-closed errors work as
source; local adapter calls are real but are not automatically wired into the
running Bcode UI. Do not advertise completed end-to-end dictation integration.

## Tests and verification boundary

`tests/control.rs` writes behavior tests for absent-local-host failure, real
adapter dispatch against a side-effect-free fake, all five command mappings,
shutdown cancellation, single invocation on failure, privacy redaction, payload
bounds, unknown versions/fields/operations, and argument rejection. These tests
do not open devices or call the actual IPC service.

No compilation, checks, tests, formatting, runtime probes, or Nix builds were
run. Verification is explicitly deferred until every wave-1 writer settles.
The Cargo manifest, bundle registration, generic frontend plumbing and subsequent
verification remain coordinator work. No frozen contract changes are requested.
All plugin/document edits in this handoff are settled.
