# TUI rendering configuration

## Component ownership and direct primitive boundary

Bcode's normal terminal shell composes generic controls from `bmux_tui_components` and coding-agent
recipes from `bcode_tui_components`. Production consumers request exact component features; the
repository checks those requests with `scripts/check-tui-component-features.sh` and prohibits the
`all` convenience bundle in production manifests.

Direct `bmux_tui` primitives remain valid only for thin terminal-boundary adaptation that shared
components cannot own coherently: full-canvas underpaint, bounded scratch-frame clipping, terminal
image placement, domain-specific map/diff drawing, and component internals. Two Bcode picker list
adapters remain nested inside shared `PickerFrame`/`ModalFrame` composition, while Eval and Metrics
adapt their domain rows into BMUX tables. `scripts/check-loop-runtime-architecture.sh` mechanically
rejects new raw reusable controls outside these classified adapters.

The generic component crate intentionally retains `bmux_keyboard` and `bmux_text_edit` through
`bmux_tui`'s baseline. The primitive crate's public event, focus, list, viewport, picker, palette,
history, and text-input APIs expose those types directly; splitting them would be a separate
primitive-API redesign, not component feature isolation. Component-owned optional dependencies such
as terminal-grid and Unicode helpers remain feature-gated and are checked by BMUX's complete feature
matrix.

## Theme ownership

Terminal presentation is derived from the active versioned theme definition described in
[`tui-themes.md`](tui-themes.md). Bcode owns coding-agent semantic roles and resolves them into
renderer styles; BMUX owns terminal primitives and generic component style structures. Portable
frontend/session contracts do not contain terminal colors, viewport data, or theme selection.

Opaque themes apply their resolved `canvas` style to the complete normal terminal frame before
content, overlays, and rich presentations are drawn. Existing interactive depth is renderer-owned:
`surface.raised` covers palettes, pickers, and the composer; `surface.overlay` covers opaque modal
panels; and `control.focused` covers focused controls. These optional schema-v1 roles fall back to
canvas and `border.focused`, respectively, so existing external themes remain compatible.
`terminal-native` preserves `Color::Default`: its full-frame fill therefore means the terminal's
background, not black. Opaque themes opt into backgrounds through semantic canvas, surface,
container, source, and diff roles. Raw terminal/program output keeps its own ANSI colors and is not
reinterpreted as application chrome.

Bcode's bundled `/loop` modal demonstrates the same boundary for plugin-owned application chrome:
the host supplies typed canvas, text, muted, border, focus, and selection presentation through
`PluginTuiTheme`; the plugin maps those styles into BMUX modal and input components without changing
loop workflow semantics. Theme-less compatibility rendering remains available only when no host
presentation is supplied.

The `/theme` comparison pane follows the same renderer boundary. Its catalog metadata comes from the
canonical discovered theme catalog, while representative transcript, tool, source, selection, and
focus examples are bounded semantic fixtures rendered through existing presentation adapters. The
picker never reads raw session events, reinterprets tool state, appends preview rows to live history,
or mutates canonical session state. Responsive layout only chooses between a list and a list-plus-
preview arrangement; keyboard preview/apply/cancel and scrolling-aware mouse hit testing continue
through the existing picker state and user-state persistence effects.

Configuration startup and reload continue through `runtime.rs` and the app's existing config
application path. The app stores only fully resolved presentation. Markdown options include the
resolved Markdown and syntax palettes, transcript layout fingerprints include the resolved theme
fingerprint, and native plugin visual contexts carry renderer-owned source/diff/syntax presentation
plus that fingerprint. Theme changes therefore invalidate retained styled rows without replaying or
mutating canonical session history.

The mechanical boundary is checked by `scripts/check-tui-theme-architecture.sh`. Its exceptions are
narrow: declarative theme definitions/resolution, raw ANSI conversion, compatibility defaults, and
focused tests may contain concrete colors; migrated application chrome must consume semantic styles.

## BMUX runtime ownership

Bcode's normal terminal entry point in `packages/tui/src/runtime.rs` constructs one
`bmux_tui_runtime` owner for terminal input, bounded application admission, commands,
subscriptions, timers, redraw coalescing, and presentation cadence. Bcode continues to own
`ActiveChat`, session-view adaptation, permissions, client effects, transcript layout, image
composition, navigation, and all other product semantics. BMUX runtime types do not enter portable
frontend or plugin contracts.

Chat, pickers, dialogs, palettes, Ralph flows, and plugin surfaces are serialized by that root
program. Standalone onboarding is a separate product entry point, but it uses the same BMUX managed
input/runtime/presenter boundary rather than a handwritten input or draw loop. Terminal drawing is
confined to the chat and onboarding presenters. Plugin-owned surfaces retain generic structured
fallbacks when rich adapters are unavailable or disabled.

Reliable session and terminal updates use bounded admission. Request-draft paint handoff, hit maps,
cursor state, and image-scene ordering remain Bcode-owned and are acknowledged only after successful
presentation. Bcode resolves the BMUX TUI crates from `master`, currently locked to commit
`e4ba5252a77ea54cf02a56e028b57fdb95e38928`, with no local path override. Render cadence limits
presentation only and never delays semantic updates, authorization, cancellation dispatch, or
canonical execution.

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

## Projection and presentation boundedness

Markdown projections are cached by item identity, canonical revision, and complete render options.
Each resident item keeps the current projection and at most eight variants; resident reconciliation,
revision replacement, and generation checks prevent stale or incompatible results from being used.

Bcode owns semantic invalidation. Only cursor-blink paint over a known composer region is localized;
unknown, structural, transcript, plugin, theme, resize, reset, route, and onboarding changes use full
damage. BMUX clips and bounds generic terminal regions and transactionally commits cells, hit regions,
cursor state, and image state only after output succeeds. This retained presentation is process-local
and does not define transport retention, acknowledgment, replay, conflict handling, reconnect safety,
or durable resume.

## Performance verification

Use `scripts/capture-tui-product-latency.sh` for the Bcode latency/RSS gate and
`bmux-perf-tools run-benchmark --manifest perf/tui-runtime-render.toml --profile normal` in BMUX for
the generic runtime/render gate. Both enforce the documented 100 ms p99 ceiling; the Bcode probe also
enforces its RSS budget. Generated artifacts contain workload, environment, revision, distribution,
and gate metadata.

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

Inline interaction rows use the existing indexed transcript layout and viewport. Their complete
bounded logical extent is retained as a sparse span, while each frame directly renders only the
visible logical slice. There is no interaction-owned whole-question viewport or full-height scratch
buffer. The host caps logical extent at 4,096 rows and visible work at 512 rows and 131,072 cells.
Committed geometry is shared by drawing, cursor projection, hit testing, and mouse translation.
Pinned mode uses a host-owned viewport keyed by interaction identity and the renderer's focused-row
hint. Only one queued interaction is active at a time; authoritative resolution removes active,
opening, or queued state without reopening later.
Question requests are bounded to 32 questions, 100 options per question, 16 KiB question text, 4 KiB
option labels, and 64 KiB custom answers; custom inputs display at most six content rows at once.
Unknown or disabled adapters remain bounded static transcript rows with required/optional status.
