# Vox CLI handoff

The executable calls `bcode_vox::cli::run() -> bcode_vox::Result<()>`.
`cli::diagnostic(&Error) -> &'static str` supplies secret-safe public messages.
The CLI uses public library APIs only and never launches another process.

## Commands

```sh
vox config example
vox model list
vox model install tiny.en       # explicit public model download
vox model verify tiny.en       # hash the installed catalog artifact
vox --config /absolute/config.toml doctor --json
vox --config /absolute/config.toml serve
vox start
vox stop
vox toggle
vox cancel
vox status --watch --json
vox --config /absolute/config.toml transcribe input.wav
vox transcribe input.wav --model /absolute/model.gguf --json
vox shortcuts hyprland
vox shortcuts sway
vox model remove tiny.en
```

`serve` is foreground-only. Start it separately before sending actions; action
clients never start a service implicitly. A service uses its startup configuration;
client `--config` does not reconfigure an existing service. Stop means stop the
recording and process/deliver it, not terminate the service. Cancel discards the
active operation. Each action is submitted once, never automatically retried after
an ambiguous transport error. Service shutdown remains available to embedding
callers through the public service API, not a CLI command in this first surface.

An explicit config must exist. Otherwise the CLI reads
`$XDG_CONFIG_HOME/vox/config.toml`, falling back to
`$HOME/.config/vox/config.toml` for missing/relative/empty XDG config roots.
An absent default config uses built-in defaults; malformed/unreadable config
fails. `vox config example` generates TOML from the frozen Config schema without
writing files or credentials. Set a local model path explicitly before serving.
The model library owns XDG/HOME model storage; there is no CLI-only directory
policy or environment mutation. Listing is metadata-only, never proof of integrity.

File transcription uses configured recognition, speech gating and text processing,
with delivery forced to stdout: it never acquires a desktop sink or microphone.
`--model` is a local path and conflicts with cloud recognition; `--backend cloud`
is explicit and requires cloud settings in config. There is no fallback. The gate
runs before recognizer construction; silence produces empty final text. JSON is a
single final `{ "text": ... }` record. Plain output is final text plus a newline.
WAV loading has the audio module's bounded allocation policy, followed by the
configured recording-duration limit. Ctrl-C cancels file recognition and model
install/verification cooperatively; blocking backend calls may take time to return.
Cancellation/error never emits a partial transcript. A successful model publication
is a commit point; a subsequent interrupt does not undo it. Ctrl-C handling uses a
scoped helper thread; Tokio's underlying OS signal registration remains global.
An embedding host must account for that process-level behavior when calling run().

`--json` is global (accepted before or after a command). Status watch emits one
JSON line per changed state, polling every 250 ms; service failures terminate the
watch, with no reconnect or action retry. Errors go to stderr and are redacted.
Exit codes: 0 success, 2 invalid input/config, 130 cancellation, 1 other failure.
Doctor reports build flags, configured policy and model metadata only. It does not
open a microphone, connect to a desktop, type, read/write the clipboard, run
inference, verify a model hash, contact cloud endpoints, or test the service.

## Shortcuts

`shortcuts::Bindings::from_config(&Config) -> Result<Bindings>` maps opaque key
identifiers to hold/toggle policies. `insert(String, BindingMode) -> Result<()>`
adds an independent identifier; duplicates/blank keys fail without replacement.
`event(&str, KeyEvent) -> Option<Action>` suppresses repeats, duplicate presses,
unmatched releases and unknown identifiers. Hold emits Start then Stop; toggle
emits Toggle once per press and rearms on release. `reset() -> Option<Action>`
clears key state and requests Cancel if a hold binding was pressed (lost input
ownership must not commit a recording). Callers send returned actions themselves;
this module does not acquire global keyboard resources or invoke the service.

`snippet(Compositor) -> &'static str` supplies Hyprland/Sway example bindings:
press starts, release stops, separate toggle/cancel. No repeat-enabled binding is
used. These examples use fixed illustrative keys and `vox` on PATH. Adjust them
and start the service explicitly. Vox config shortcut strings alone do not install
compositor bindings. Portal global-shortcut support is **not implemented**.

## Coordinator integration

Keep root/manifest/shared-module ownership with the coordinator. The CLI's private
`interrupt` and `tests` declarations are already in cli/mod.rs. Public lib module
declarations required: `pub mod cli; pub mod shortcuts;`. Binary name `vox` should
point at src/main.rs in package `bcode_vox`.

Existing dependencies used: clap derive/help/usage/error-context, serde derive,
serde_json, toml. **Add `tokio = { workspace = true, features = ["rt", "signal",
"sync", "macros"] }`** for scoped interrupt handling; this reuses the workspace
version. Tests use the existing tempfile dev dependency. No contracts/config
changes requested. Model calls match the shared module's public list/data_dir,
install(id, cancel), verify_with_cancel(id, cancel), and remove(id) APIs.

Please integrate user-facing installation/feature requirements, model licensing,
config placement, foreground service setup, shortcut snippets, unsupported portal
status and doctor limitations into package/root docs. This local handoff does not
claim desktop, microphone, backend or cross-platform acceptance.

Behavioral tests are implemented in cli/tests.rs and shortcuts/mod.rs, including
argument parsing, config precedence/errors, schema-round-tripped examples, explicit
backend/model policy, stdout-only file policy, silence, final-only processing,
cancellation, no retry/fallback, JSON escaping, truthful doctor metadata and shortcut
edge state. **No formatting, compilation, checks, tests, Nix builds or runtime probes
were run: verification is explicitly deferred until all wave-1 writers settle.**
