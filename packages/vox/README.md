# Vox

Vox is a standalone local-first Rust dictation library and utility. It does not require a Bcode daemon. The Bcode plugin is a separate frontend integration.

## Build

```sh
cargo build -p bcode_vox --features linux-desktop
cargo install --path packages/vox --features linux-desktop --bin vox
# Optional GPU build:
cargo build -p bcode_vox --features linux-desktop,vulkan
```

Default Cargo features are empty. `native-audio`, `local-inference`, `wayland`, and `x11` are independently selectable; unsupported capabilities return explicit errors. Vulkan requires its development toolchain and a real driver at runtime; CPU operation does not require Vulkan or a graphical toolkit.

## Workflow

```sh
vox models list
vox models install whisper-tiny.en
vox doctor
vox --config ~/.config/vox/config.toml serve
# Other terminal / compositor bindings:
vox start
vox stop
vox toggle
vox cancel
vox status --json
```

Use `vox --help` for the exact installed command syntax and model IDs. Model installation is explicit and verifies a catalogued SHA-256 before publication. Downloaded model weights have their own licenses, separate from Vox and the inference library. Nothing uploads to a cloud service unless cloud recognition is explicitly selected.

See `examples/config.toml` and `examples/hyprland.conf`. Start the service inside the graphical session with its Wayland, DBus, and audio environment. Do not run it as root. A service started over SSH normally lacks the required local desktop environment.

## Delivery policy

Typing targets whichever field is focused when recognition finishes. It never silently switches to clipboard delivery. Wayland support requires a compositor exposing virtual-keyboard-v1; GNOME/KDE coverage must not be assumed. Clipboard mode is an explicit alternative and changes clipboard contents; clipboard managers may retain them.

No focus restoration or automatic Enter/submit. External typing defaults to single-line text. Bracketed paste is not a global desktop API: Bcode and cooperating terminal hosts should use semantic insertion instead. A failed delivery may have partially typed text and is never automatically replayed.

## Privacy and lifetime

Microphone capture starts on explicit recording actions. A single operation owns capture/transcription. Cancellation prevents subsequent delivery, including late recognition results. Audio/transcripts are not intentionally logged or persisted by the normal workflow. Backend/OS logging and terminal stdout are separate disclosure surfaces; opt into stdout only deliberately. Optional cloud credentials are environment references, never emitted by doctor.

## Platform scope

Linux-first implementation; portable public contracts do not contain Wayland/X11, CPAL, or inference-library types. macOS/Windows adapters require their own implementation and hardware acceptance before support is claimed. Native injection cannot bypass compositor security or fix another compositor plugin's input handling.

## Validation

Run unit tests with default features and native feature configurations; run real microphone and current-desktop smoke tests only with explicit user opt-in. Fixture transcription, silence rejection, model integrity, control bounds, Unicode receipt, cancellation, and plugin disable behavior are acceptance requirements—not implied by compiling successfully.
