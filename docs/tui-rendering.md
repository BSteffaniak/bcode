# TUI rendering configuration

## Session picker and search scope

The TUI keeps local canonical-summary filtering distinct from transcript search. Local filtering is
renderer-owned ephemeral state and may match title, session ID, working directory, import source,
and fork metadata already present in portable summaries. It does not invoke providers or imply
transcript coverage.

Ctrl-F explicitly starts bounded transcript search from the current picker query. The picker uses
the portable client contract through a 150 ms replaceable, generation-gated effect, so input and
catalog updates remain responsive and stale completions cannot replace newer results. A bounded
multi-result view exposes canonical summary title, canonical session/sequence, exact hydrated
timestamp when available, content kind, provider/rank, bounded preview/truncation, hydration state,
and separate query/corpus completeness plus provider/failure counts. Inline `deep:`, repeatable
`content:<kind>`, and repeatable `provider:<id>` controls map to typed portable policy and filters;
shell/tool output fails closed unless deep mode is explicit.

Enter navigates only an exact canonical hydration through the normal bounded `AroundSequence`
projection attach and canonical sequence anchor. Stale, deleted, damaged, unavailable, missing, or
mismatched hydration cannot navigate. Provider previews never become canonical history. Picker
input, result selection/viewport, key routing, rendering, and hit surfaces stay TUI-local, and search
state emits no canonical events or persistence writes. Provider deadlines and cancellation remain
application-owned; renderer task replacement is not reconnect-safe replay or durable transport
resume.

## Semantic transcript boundary

The TUI holds `SessionView` as its sole canonical transcript reducer. Durable and live session events
are applied to that reducer once, then `SessionViewTerminalAdapter` incrementally adapts the shared
`TranscriptViewDocument` by stable item ID and revision. Terminal code owns wrapping, Markdown
projection, plugin adapter instances, row caches, viewport anchors, scrolling, hit testing, and draw
cadence; it does not infer canonical transcript identity, ordering, lifecycle, fallback, or stream
integrity from raw events.

Ordinary item revisions invalidate only the changed terminal item. Membership or ordering changes
produce structural damage, and inconsistent source indexes escalate to a full reset. Viewport
preservation uses shared stable item identity plus the intra-item rendered row. Automatic movement is
reserved for accepted user submissions and the first visible content of a new assistant segment;
subsequent semantic updates preserve the current anchor, and manual scrolling detaches following.

Incomplete Markdown is renderer-local. Streaming projections visibly preserve accepted source
content, redact unsafe unfinished destinations, and converge to the normal projection once syntax is
complete. Semantic text remains unchanged in `SessionView`.

## Accepted Markdown projection scheduling

Transcript Markdown has one TUI-owned accepted projection per resident item. Rows, contribution
geometry, anchors, focus reconciliation, hit testing, and rich-presentation discovery all consume
that same retained result; consumers must not independently reparse transcript source. During a
stream, the previous accepted projection remains readable while a dedicated worker prepares the
latest item revision and complete render-options generation.

The worker mailbox contains at most one active request and one replaceable pending request. New
revisions, width changes, details state, finalization, or other option changes replace obsolete
pending work. Completion wakes the chat loop directly. The loop installs a result only when item ID,
item revision, and every render option still match, then marks only that transcript item dirty.
Stale results have no layout, focus, rich-presentation, or viewport effect. Renderer panics are
normalized without source text or private error details; retained content remains visible and a
later generation retries on the same worker.

Accepted row-count changes use the existing stable item/intra-item viewport anchor. Following and
manual detachment therefore remain application policies rather than worker behavior. Session switch,
shutdown, and resident eviction invalidate pending generations. The cache retains only one current
projection per resident item, so neither streaming revision count nor terminal resize history grows
retained state.

## Plugin visual adapters

The TUI resolves plugin visuals by stable `<plugin-id>/<adapter-id>` references. Configure explicit
selection under `[tui.visual_adapters]`:

```toml
[tui.visual_adapters]
preferred = ["example.shell/shell-card", "bcode.shell/shell-run-terminal-card"]
disabled = ["example.experimental/unstable-card"]
```

`preferred` is descending user preference. Compatible entries listed there precede unlisted
candidates. Remaining order is manifest priority, producer-default preference, then deterministic
plugin and adapter IDs. `disabled` removes an adapter before availability checks. Missing,
malformed, failed, or timed-out adapters advance to the next compatible candidate; the bounded
canonical tool fallback remains last.

Reloading composed configuration rebuilds the TUI presentation host. Adapter order, disabled state,
and plugin enablement therefore apply at the supported config reload boundary, and old adapter
registries, caches, artifact state, and pending dynamic work are disposed together. Active
invocations render through the rebuilt candidate hierarchy and may temporarily use a later adapter
or canonical fallback while dynamic output is recomputed. Replacing the bytes of an already loaded
native dynamic library is not hot-swapping: restart Bcode to load a new library artifact. This
reload behavior transfers current semantic state; it does not claim durable adapter replay or
resume semantics.

Dynamic adapters implement `bcode.tui-visual-adapter/v1` using the portable models in
`bcode_plugin_sdk::tui_visual`. Requests and responses are serialized; no `bmux_tui` value or Rust
trait object crosses the plugin ABI. Rendering executes outside frame drawing with a bounded queue,
2 s cancellation-propagating deadline, bounded response rows/spans/text, and a 512-entry cache
keyed by exact adapter, invocation, schema/version, semantic presentation revision, and width.
Artifact chunks use the same bounded service and invalidate only the affected invocation. The TUI
converts validated rows/styles and retains ownership of layout, lifecycle/timing chrome, viewport,
hit testing, and terminal paint.

Current visual adapters require rows, basic text styles, transcript title/timeout hints, render mode,
and artifact chunks. Interactive actions remain on the existing typed interaction/surface contracts;
the serialized visual contract intentionally does not generalize into an interactive surface ABI.

## Smooth stream scheduling

Configured live text smoothing remains semantic state in `SessionView`, not terminal rendering logic.
The TUI contributes the shared presentation deadline to its normal event-loop deadline set, advances
the view when due, and adapts only changed `SessionView` items through
`SessionViewTerminalAdapter`. It does not split provider chunks or evaluate interpolation curves.

`[presentation.streaming]` is frontend-independent: `enabled = false` or `max_lag_ms = 0` restores
immediate chunk presentation, while `curve` selects `linear`, `ease_in`, `ease_out`, or
`ease_in_out`. The default `max_lag_ms = 40` is bounded to 250 ms. Presentation advancement requests
a redraw but still respects `[tui.render].max_fps`; cancellation, permissions, execution, canonical
stream validation, and terminal flushing do not wait for terminal cadence.

## Draw cadence
`[tui.render]` controls terminal draw cadence.

* `max_fps = 60` is the default.
* Values from 1 through 240 are used directly; larger values are clamped to 240.
* `max_fps = 0` disables cadence limiting.
* Reloaded TUI configuration applies the new cadence without changing semantic event processing.

Semantic events continue to update application state immediately. The cadence limits terminal draws only; it does not delay cancellation, permission handling, execution state, checkpoint validation, or artifact decoding.

BMUX `Terminal` retains the previous frame and reports `DrawStats` from its ANSI cell-diff flush.
Bcode records `tui.frame.changed_cells` and `tui.frame.full_repaint_total`; repeated same-size draws
therefore use BMUX's retained-buffer changed-cell transport, while resize or explicit backend reset
may repaint fully. Custom dirty-region transport is not justified unless production telemetry shows
changed-cell amplification beyond the existing item-scoped layout and BMUX cell diff.

## Interactive tool presentation

`[tui.interactions]` controls terminal presentation for active plugin-owned interactions.

```toml
[tui.interactions]
placement = "transcript"
offscreen_focus = "retain"
```

* `placement = "transcript"` is the default. The active surface occupies its semantic interaction
  transcript item and scrolls with ordinary transcript content.
* `placement = "pinned"` pins the same surface above the composer. The overlay does not change
  transcript layout, and the host underpaints its complete rectangle before plugin rendering.
* `offscreen_focus = "retain"` is the default. Keyboard input continues to reach the active inline
  interaction while the user scrolls backward for context.
* `offscreen_focus = "suspend"` routes ordinary input back to the composer while the inline item is
  fully hidden. `tui.interaction.focusActive` restores the item into view; its default binding is
  Ctrl-I and it can be remapped under `[tui.keybindings.chat]`.

Placement and off-screen focus are renderer-owned and apply on configuration reload without changing
controller or exchange state. Interaction values and validation remain plugin-owned. Host-configured
composer edit, selection, newline, and submit bindings are adapted through the terminal plugin host.

Inline interaction rows use the existing indexed transcript layout and viewport. Partial visibility
is rendered through a scratch frame capped at 512 rows and 131,072 cells and clipped into terminal
coordinates, including cursor and mouse translation. Only one queued interaction is active at a
time; authoritative resolution removes active, opening, or queued state without reopening later.
Question requests are bounded to 32 questions, 100 options per question, 16 KiB question text, 4 KiB
option labels, and 64 KiB custom answers; custom inputs display at most six content rows at once.
Unknown or disabled adapters remain bounded static transcript rows with required/optional status.
