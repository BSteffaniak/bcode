# TUI rendering configuration

## Session picker and search scope

The initial TUI session-search slice is intentionally limited to local filtering of the canonical
session-summary picker. That filter remains renderer-owned interaction state and may match title,
session ID, working directory, import source, and fork metadata already present in portable
summaries. It does not invoke optional search providers, claim transcript coverage, expose deep or
content-category controls, or render provider documents as canonical history.

A later dedicated transcript-search mode must consume the portable session-search request/result/
status contracts asynchronously, expose partial/degraded coverage, and navigate through canonical
locator hydration. Its reusable effect waits 150 ms before dispatch, aborts superseded renderer
work, advances a renderer-local generation on replacement/cancel, and accepts terminal aggregates
only for the latest generation. Provider deadlines and cancellation remain application-owned; this
is not a reconnect-safe replay protocol. It must not reuse local picker filtering as evidence that
transcript search or provider coverage exists. Picker text input, selection/viewport, key routing,
rendering, and hit surfaces stay in TUI modules, and picker filter state is ephemeral: rebuilding from catalog summaries
must not emit canonical events or persistence writes.

The presentation adapter keeps content kind, canonical session locator/ID, canonical catalog title,
provider/rank, bounded preview and truncation, canonical hydrated timestamp/outcome, query/corpus
completeness, provider/failure counts, and explicit degraded state. Provider documents never become
title or history authority.

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
500 ms cancellation-propagating deadline, bounded response rows/spans/text, and a 512-entry cache
keyed by exact adapter, invocation, schema/version, semantic presentation revision, and width.
Artifact chunks use the same bounded service and invalidate only the affected invocation. The TUI
converts validated rows/styles and retains ownership of layout, lifecycle/timing chrome, viewport,
hit testing, and terminal paint.

Current visual adapters require rows, basic text styles, transcript title/timeout hints, render mode,
and artifact chunks. Interactive actions remain on the existing typed interaction/surface contracts;
the serialized visual contract intentionally does not generalize into an interactive surface ABI.

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
