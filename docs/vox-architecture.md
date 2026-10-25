# Vox implementation boundaries

Vox's portable library owns the recording pipeline, adapters, and standalone runtime, without a Bcode daemon dependency. The independently installable executable is `vox`. Its crate's default features are empty; a Linux native build selects `linux-desktop`, optionally `vulkan`. C++ inference is optional and explicitly pinned. Text delivery is owned by Vox, not Enigo or an external typing utility.

The Vox plugin contributes user commands and frontend callbacks. A generic frontend command registry invokes local adapters and consumes final literal text. The TUI adapts text to its existing composer paste behavior; it does not own recognition or recording policy. Plugin callbacks must never capture a remote daemon's microphone. Native frontend resources require explicit configuration and stop on frontend shutdown.

Platform contracts do not expose Wayland, CPAL, or transcribe-cpp handles. Linux-specific code is cfg-gated, and unsupported platforms/features report errors instead of pretending delivery succeeded. Future macOS/Windows implementations belong behind the same contracts; no support claim is implied by an interface.

Cancellation prevents subsequent pipeline delivery; bytes already typed cannot be undone. Control protocol replies distinguish admission from completion. Clients do not retry ambiguous mutations or switch sinks after a partial delivery. A recognition failure never selects cloud or clipboard automatically.

The initial model-free speech gate is an energy baseline, not a reliable speech/noise classifier. Explicit Silero model configuration selects the neural gate. Recognition and speech models require separate explicit downloads and licenses. Live accuracy/latency and compositor compatibility require acceptance on the actual target environment.

## Verification

Implementation wave source is not completion evidence. Validate portable and native features, runtime/control races, mock cloud transport, WAV conversion, silence rejection, download integrity, and real Unicode receipt. Independently verify Bcode plugin disable behavior and semantic editing. Real microphone capture, current desktop injection, paid cloud usage, and deployment are explicit opt-in operations.
