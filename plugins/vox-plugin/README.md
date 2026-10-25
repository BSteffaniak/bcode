# Frontend adapter follow-up

This supersedes the earlier integration document's statement that a generic
frontend command SDK contract is unavailable. Only plugin-owned sources were
changed in this follow-up; host wiring and manifests remain coordinator-owned.

## Coordinator wiring

* Enable `bcode_plugin_sdk`'s `tui` feature for `static-bundled` compilation
  (`static-bundled = ["bcode_plugin_sdk/tui"]` is sufficient if not already enabled).
* Register `bcode_vox_plugin::tui_registry` (also available as
  `bcode_vox_plugin::tui::tui_registry`) using existing static TUI registration.
  This function is compiled under `static-bundled`, like the native static entry.
* Keep one returned registry per local frontend lifetime. It registers exact
  command IDs `vox.start`, `vox.stop`, `vox.toggle`, `vox.cancel`, `vox.status`
  through `register_frontend_command`; all share one lazy runtime handle.
* Generic keymap declarations use the same IDs in the existing `chat` scope.
  All defaults are deliberately unbound: users may explicitly bind them without
  installing surprise microphone shortcuts. Route generic registered actions to
  the matching local command rather than inventing a Vox-specific dispatch table.
* Invoke `PluginFrontendCommand::invoke()` only from explicit local user actions.
  Initialization reserves local ownership and starts model/adapters on the Vox
  worker on the first Start/Toggle; recognition and capture stay on that worker.
  Registration, status, stop, cancel, text
  polling, and shutdown never initialize the runtime or read configuration.
* Poll `drain_text()` on a bounded/coalesced completion cadence after Stop (and
  during operation, since maximum-duration capture can finish independently).
  Any registered command can drain the shared slot; only the first call returns
  the final string. Insert literal text once into the currently focused editor.
  Never submit, log, serialize, retry insertion, or send it through the daemon.
  Continue polling beyond initial action acceptance until final completion.
* Call `shutdown()` on frontend detach/exit; it is idempotent across all five
  command handles, closes admission permanently and requests runtime shutdown.
  Shutdown is cooperative, not a claim that backend worker threads have joined.

## Configuration and boundaries

`VOX_CONFIG` must contain an explicit nonempty configuration path. No guessed
file, default model, auto-download, or cloud/clipboard fallback is selected.
Relative paths are resolved by ordinary explicit file access in the client
process. The configured recognition backend remains unchanged: an explicitly
selected cloud backend is not silently rewritten to local, nor used as fallback.

The adapter forces `DeliveryMode::Host` on the loaded configuration. It never
selects external typing, stdout, or clipboard sinks. It consumes actual
`Handle::take_final_text()` values (moving the string, without cloning a per-command
result). Error/status diagnostics are normalized, including backend failure text.
Status invocation checks availability/failure; today's `invoke() -> Result<(),
String>` contract cannot return a rich Recording/Transcribing state. The separate
typed status service remains available for consumers needing structured status.

This is an explicit **in-process frontend opt-in**, not the standalone service
singleton. It does not acquire the standalone IPC socket or exclude a separately
running Vox service, a second frontend process, or a second registry. Do not claim
machine-wide single-operation exclusivity. Within this one registry all commands
share one `Handle`, and that runtime enforces one active operation. The existing
`LocalService` adapter remains useful for explicit standalone service consumers;
the daemon native plugin still has no microphone binding and fails actions closed.

New side-effect-free unit tests are in `src/tui/tests.rs`: lazy initialization,
shared handle/actions, consumptive final-text delivery, idempotent shutdown,
normalized failures/no retries, missing path rejection, Host delivery override,
and keymap-to-command registration agreement. No tests, formatting, compilation,
runtime probes, checks, or builds have run. All plugin edits are settled.
