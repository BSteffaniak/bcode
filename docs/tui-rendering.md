# TUI rendering configuration

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

## Draw cadence
`[tui.render]` controls terminal draw cadence.

* `max_fps = 60` is the default.
* Values from 1 through 240 are used directly; larger values are clamped to 240.
* `max_fps = 0` disables cadence limiting.
* Reloaded TUI configuration applies the new cadence without changing semantic event processing.

Semantic events continue to update application state immediately. The cadence limits terminal draws only; it does not delay cancellation, permission handling, execution state, checkpoint validation, or artifact decoding.
