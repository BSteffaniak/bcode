# TUI V2 transcript architecture

This document is the implementation contract for the second terminal transcript engine ("V2").
Every phase and every change to V2 code implements against it. Rules are numbered; tables are
normative. Where this document and an older TUI document disagree about V2, this document wins;
V1 continues to follow [`tui-rendering.md`](tui-rendering.md) until it is deleted.

Related documents: [`tui-rendering.md`](tui-rendering.md),
[`renderer-architecture.md`](renderer-architecture.md),
[`presentation-architecture.md`](presentation-architecture.md),
[`session-persistence-architecture.md`](session-persistence-architecture.md),
[`tui-themes.md`](tui-themes.md), [`tui-keybindings.md`](tui-keybindings.md), and BMUX's
`docs/tui-framework.md` and `docs/tui-runtime.md`. [`INVARIANTS.md`](../INVARIANTS.md) applies
unchanged.

Contents:

1. Purpose and scope
2. Diagnosis summary
3. Layered architecture
4. Position and navigation model
5. Resident window and paging
6. Focus model
7. Engine selection
8. BMUX upstream work
9. Frame pipeline and performance budgets
10. Acceptance criteria
11. Ambiguity resolutions
12. Phases
13. Development loop

---

## 1. Purpose and scope

### 1.1 Purpose

V2 replaces the terminal transcript pipeline: everything between the semantic `SessionView`
transcript document and the painted transcript region, including the resident history window,
item layout, the viewport and scrolling, navigation and automatic reveal, focus inside the
transcript, the latest-content bar, transcript selection and hit regions, and the chat-screen
frame composition that places the transcript, latest bar, composer and status row.

### 1.2 Relation to V1

| Rule | Statement |
|---|---|
| S1 | V2 is a separate engine. It does not call V1-private transcript modules in `packages/tui/src` (for example `app.rs`, `chat_loop.rs`, `render.rs`, `transcript_*.rs`, `older_history.rs`, `structure.rs`, `indexed_transcript_layout.rs`). |
| S2 | V1 stays behaviorally untouched. No V1 bug fixes. The only V1 edits permitted before phase 7 are (a) the engine-selection seam (section 7) and (b) `#[cfg(test)]`-only observation and driving seams used by the scenario suite (for example the `*_for_test` methods on `BcodeRuntimeModel` and `BmuxApp`), which add no production code. Each such edit must leave V1 behavior identical when V1 is selected. |
| S3 | Both engines remain selectable from phase 4 until phase 7. Phase 7 makes V2 the default, deletes V1 and its seam, and updates documentation and invariants. |
| S4 | V2 is not a port. Where V2 needs behavior that V1 implements, it re-derives it from this specification and the acceptance criteria, not from V1 code structure. |

### 1.3 Shared code

V2 may use only genuinely generic or semantic layers:

| Shared layer | Crate | Use in V2 |
|---|---|---|
| Semantic transcript | `bcode_session_view`, `bcode_session_view_models` | Sole transcript reducer and document (A4). V2 adds incremental window operations (section 3.1). |
| Structural presentation policy | `bcode_presentation_policy`, `bcode_presentation_policy_models` | Rule resolution, grouping, visibility floor, summary templates. |
| Theme | `bcode_tui_theme` | Resolved roles, chrome, motion, density, icons. |
| Pure content recipes | `bcode_tui_components` | Only producers that are pure functions of explicit inputs (no thread-locals, no application state), such as Markdown projection adapters, tool cards, diff/source viewers, composer recipe. |
| Terminal mechanics | `bmux_tui`, `bmux_tui_components`, `bmux_tui_runtime`, `bmux_markdown` | Layout, measurement, virtualization, anchoring, scrolling, animation, selection, hits, focus, damage, test harness. |
| Host shell | `bcode_tui` | Daemon connection, effects/IPC, composer state, overlays/pickers/modals, plugin hosting, configuration. V2 is embedded by the host; it does not own I/O. |

A V1 module that V2 needs but that relies on thread-locals or V1 application state is not
shared; V2 implements its own explicit-input equivalent (rule L4). The resulting temporary
duplication is removed when V1 is deleted in phase 7.

---

## 2. Diagnosis summary

Paths are relative to `packages/tui/src` unless noted. Rule identifiers refer to sections 3–6.

| # | V1 root cause | Evidence | Preventing V2 rule(s) |
|---|---|---|---|
| D-1 | About 15 independent top-row authorities (viewport mode, scroll animation, pending/presented anchors, 14-field navigation checkpoint, manual grace, overflow, reveal and admission state, history reveal requests, latest-bar height) mutated by about 22 functions; at least 8 sites decide which fields to reset. | `app.rs` (`BmuxApp`, 109 fields), `packages/tui-components/src/transcript_viewport.rs` | N1 (single `NavState`), N2 (one reducer), N3 (rows derived) |
| D-2 | Lost correspondence on page arrival: a prepend changes the transcript immediately but layout resyncs only at the next frame; a wheel tick in between clears both anchors, and the fallback maps old layout index *k* to new item *k*. | `app.rs` `prepend_older_history`, `mark_manual_transcript_scroll`; capture fallback in transcript projection | P1 (identity anchors), W3 (window, units and index updated in the same update), N6 (manual input materializes from the keyed anchor) |
| D-3 | Paging stalls and leaps: loads start only at row 0; tiny pages (4 items / 24 rows); wheel input accumulates while frozen; zero-row pages chain stalls; Home pages through the whole session. | `effects.rs:331-339`, `older_history.rs` | W5 (prefetch distance), W6 (page sizing), N8 (pending movement spent only against measured page rows), W10 (session start/latest load directly) |
| D-4 | Trim after prepend uses stale layout indices; a row count is passed as an event count. Latent: the phase-0 scenario `bug_a3_…` did not reproduce a visible failure. | `transcript_resident_window.rs:132` | W7 (eviction by item key, protected band), W3 |
| D-5 | History banner disappears at exhaustion, shifting content one row. | older-history banner rendering | U3 (boundary unit has constant height) |
| D-6 | Timeline window equals the resident cap (1024 events), so the first newer page evicts about 500 events in the same mutation as the append. | `history_flow.rs:67`, `app.rs:63` | W8 (timeline window ≤ target, aligned), W7 |
| D-7 | No fast return to live: jump-to-latest scrolls to the bottom of the historical window. | `app.rs` jump handling, `replace_latest_transcript_window` | W10 (`JumpToLatest` loads the tail window), W2 (explicit `Live`/`Detached` modes) |
| D-8 | Resident jump sets a follow anchor without clearing presented/pending anchors; next frame may restore the old position. Unconfirmed: the phase-0 scenario `bug_b4_…` did not reproduce it in four variants; BUG-B4 stays as a guard scenario. | `app.rs:1591` `jump_to_transcript_index` | N1, N2 (every intent goes through one reducer; there are no presented/pending anchors) |
| D-9 | Raw-event window edges fall mid-turn; later pages complete edge items in place. | `history_flow.rs` timeline windows | W4 (windows aligned to whole turns) |
| D-10 | History paging rebuilds the whole `SessionView` and adapter from all resident events, possibly twice. | `app.rs` `rebuild_transcript_from_history`, which calls `SessionView::rebuild_history_window` (`packages/session-view/src/lib.rs`) | S-ops (incremental window operations, section 3.1) |
| D-11 | Position-keyed layout cache; signatures include item id, so a prepend re-renders every item; Fenwick tree rebuilt from scratch, duplicating BMUX `MeasuredListIndex`. | `indexed_transcript_layout.rs`, `packages/tui-components/src/transcript/indexed_transcript_layout.rs:113-200` | L1 (identity-keyed cache), BMUX 5 (incremental index) |
| D-12 | Render configuration in thread-locals (`DIFF_VIEWER_CONFIG`, `MARKDOWN_DETAILS_OPEN`, `PLUGIN_VISUAL_THEME`, `TRANSCRIPT_THEME`, `FOCUS_PHASE`, `FOCUS_GRADIENT_PAINTED`, `STRUCTURE`, plus a static generation). | `render.rs:14`, `structure.rs:1089,1146` | L2 (explicit `ItemRenderContext`), L4 |
| D-13 | Cache keys embed pointer addresses and `SystemTime::now()`; transcript length and history-loading flags are in the structural fingerprint. | layout fingerprints | L3 (fingerprint contents) |
| D-14 | `transcript_top_row` samples `Instant::now()` about six times per frame plus commit and selection scene. | `app.rs` | F2 (one clock sample per frame) |
| D-15 | Paint mutates application state (reconciling footnotes, fragments, details, interactions, cache retention); hit regions and selection scene computed in separate passes from re-walked lines. | `render_prepared_damage`, `commit_root_presentation` | F3 (pure paint), F4 (one snapshot feeds every channel) |
| D-16 | Every wheel tick marks structural + full damage (42 `Damage::Full` sites). | `root_program.rs` around 4283-4300 | F5 (regional damage), D4 |
| D-17 | Focus highlight splits each visible row into one span per grapheme every frame. | paint decorate closure | L5 (focus is a paint-only overlay) |
| D-18 | Latest bar visibility simulated on a cloned viewport, duplicating sync logic. | `needs_latest_bar_at_height` | F6 (bar decision is a pure function of the frame inputs) |
| D-19 | Navigation hidden in activity code (submission anchoring inside latest-activity recording). | `record_latest_hidden_activity` | N9 (automatic navigation is an explicit policy emitting intents) |
| D-20 | Tests render one frame per input; no test interleaves input with page arrival or eviction. | `frame_sequence_harness.rs` | Section 10 scenario suite on BMUX harness (BMUX 15) |

---

## 3. Layered architecture

### 3.0 Overview and placement

```text
 daemon/client (bcode_tui host effects)
        | SessionEvent pages, live events                      ^ PageRequest / WindowRequest
        v                                                      |
 [3.1 Session data]  bcode_session_view (+ window_ops)  --> TranscriptChanges
        v
 [3.2 Resident window manager]  bcode_tui_transcript::window
        v
 [3.3 Units + item layout]  bcode_tui_transcript::{units, layout}
        v
 [3.4 Viewport]  bcode_tui_transcript::viewport  over bmux_tui_components::VirtualList
        ^
 [3.5 Navigation controller]  bcode_tui_transcript::navigation  (NavState, reduce)
        v
 [3.6 Frame]  bcode_tui_transcript::frame  (FrameSnapshot -> PaintCx)
```

#### Crate and module placement

| Item | Placement | Justification |
|---|---|---|
| V2 engine | **New crate** `packages/tui-transcript/` → `bcode_tui_transcript` | A crate boundary mechanically enforces S1 (V2 cannot reach V1 internals of `bcode_tui`), lets the engine be tested headlessly without the host, and survives V1 deletion as the transcript engine. Created in phase 2 when its first code lands. Domain-named. |
| V2 modules | `window`, `units`, `layout` (with `layout::producers`, `layout::context`), `viewport`, `navigation` (with `navigation::auto`), `focus`, `latest_bar`, `selection`, `frame`, `engine` | One module per layer; `engine` is the façade the host calls. |
| V2 scenario support | `bcode_tui_transcript` feature `testing` (module `scenario`): deterministic engine fixtures (fixture producers, `LayoutWorkCounters` counted by the item layout cache and read with `TranscriptEngine::take_layout_work`; a scripted history source and BMUX `ProgramHarness` glue are added, together with the `bmux_tui_runtime/testing` dependency, only when a phase needs them). The V2 driver itself is `bcode_tui::transcript_scenarios::v2` (`cfg(test)`); it drives the engine façade directly in phases 2–3 and through `transcript_v2_host` from phase 4. | Keeps one scenario definition set for both engines; the engine crate never depends on `bcode_tui`. |
| Incremental window operations | `bcode_session_view` module `window_ops`; change types in `bcode_session_view_models` (`TranscriptChanges`, `TranscriptChange`) | Semantic, renderer-neutral (A1, A4). No terminal data. |
| Engine enum and config key | `bcode_config`: `TuiEngine` enum, `TuiConfig::engine` | Config crate owns the schema. |
| Engine resolution, V2 host wiring | `bcode_tui` modules `engine_selection` and `transcript_v2_host` | Host owns I/O (client calls for pages), effects and startup. |
| CLI flag | `bcode_cli` root `Cli` struct | Existing global flags live there. |
| Engine-neutral scenario suite | `bcode_tui` `cfg(test)` module `transcript_scenarios` (definitions, runner, V1 driver; V2 driver from phase 2). Status manifest: [`tui-v2-scenarios.md`](tui-v2-scenarios.md). | The V1 driver needs crate-internal access, and host-owned scenarios (composer typing, clipboard, configuration reload) need the host. Until phase 4 wires V2 into the host, such scenarios are ignored for V2 with `#[ignore = "V2 pending phase 4: <capability>"]`. |
| BMUX patch and Cargo.lock guard | `xtask/src/bmux_lock.rs`, run by `cargo xtask bmux-lock-check` and first by `cargo run -p xtask -- architecture` | Existing repository validation home. |

No `_models` crate is created for V2: no other package needs V2 types without the
implementation. `ReadingPosition` (saved per-session reading positions, restored on session
resurrection) lives in `bcode_tui_transcript::navigation`.

`bcode_tui_transcript` dependencies: `bcode_session_view`, `bcode_session_view_models`,
`bcode_presentation_policy`, `bcode_tui_theme`, `bcode_tui_components` (pure recipes only),
`bmux_tui`, `bmux_tui_components` (features `virtual-list`, `scroll-view`, `disclosure`, and
others as used), `bmux_tui_runtime` (types only), `bmux_markdown`. It must not depend on
`bcode_tui`, `bcode_client` or `bcode_ipc` other than through `bcode_session_view` (A5).

### 3.1 Session data

| Aspect | Contract |
|---|---|
| Owned state | One `SessionView` per attached session per engine instance (the engine owns it). |
| Inputs | Bounded durable pages, around-sequence windows, head/tail windows, live events, hydrated non-transcript state (permissions, interactions, runtime work, usage, composer). |
| Outputs | `TranscriptViewDocument` items keyed by `TranscriptViewItemId` + `ViewRevision`; `TranscriptChanges` per operation. |

New operations in `bcode_session_view::window_ops` (phase 5; before phase 5 V2 uses
`rebuild_history_window` plus a keyed diff, see rule SD5):

```rust
impl SessionView {
    /// Reduce a page of complete older turns and splice its items before the window.
    pub fn prepend_history_page(&mut self, events: &[SessionEvent]) -> Result<TranscriptChanges, WindowOpError>;
    /// Reduce a page of newer events after the window (detached windows only).
    pub fn append_history_page(&mut self, events: &[SessionEvent]) -> Result<TranscriptChanges, WindowOpError>;
    /// Remove whole turns whose items precede `first_kept` (exclusive) from the projection.
    pub fn evict_before(&mut self, first_kept: &TranscriptViewItemId) -> TranscriptChanges;
    /// Remove whole turns whose items follow `last_kept` (exclusive) from the projection.
    pub fn evict_after(&mut self, last_kept: &TranscriptViewItemId) -> TranscriptChanges;
    /// Replace the window (jump, head, tail); equivalent to today's `rebuild_history_window`.
    pub fn replace_history_window(&mut self, events: &[SessionEvent]) -> TranscriptChanges;
    /// Drain item-level changes accumulated by live/durable event application.
    pub fn drain_transcript_changes(&mut self) -> TranscriptChanges;
}

// bcode_session_view_models
pub struct TranscriptChanges { pub base_revision: ViewRevision, pub revision: ViewRevision, pub changes: Vec<TranscriptChange> }
pub enum TranscriptChange {
    Inserted { after: Option<TranscriptViewItemId>, ids: Vec<TranscriptViewItemId> },
    Removed { ids: Vec<TranscriptViewItemId> },
    Revised { id: TranscriptViewItemId, revision: ViewRevision },
    Reset,
}
pub enum WindowOpError { UnalignedBoundary, Overlap, SessionMismatch, Bounds }
```

| Rule | Statement |
|---|---|
| SD1 | `SessionView` remains the only transcript reducer; each durable event is applied once per residency (A4). Page and window ranges are disjoint from resident ranges; overlaps are rejected (`Overlap`), never re-applied. |
| SD2 | `prepend_history_page` accepts only pages whose last event precedes a safe window start of the resident window (turn-aligned, rule W4). It reduces the page in an isolated bounded sub-projection and splices items; no resident event is replayed. On `UnalignedBoundary` the caller falls back to `replace_history_window`; identity anchors (P1) keep the reader in place because item ids are deterministic. |
| SD3 | Eviction removes whole turns and every aggregate (tool invocations, streams, reasoning) owned only by evicted items. Head-side eviction never removes live-only items (sequence `None`) or streaming items. Tail-side eviction in `Live` mode happens only as the W9 `Live → Detached` transition, which discards the live-only and streaming items together with the evicted tail turns; `Window(Tail)` rehydrates them on return. No other operation evicts them. |
| SD4 | All operations are bounded by the page caps (D1) and are non-mutating with respect to durable state (A11). |
| SD5 | Before phase 5 V2 derives `TranscriptChanges` from a keyed id/revision diff of the document, run only when the document revision changed. Phase 5 replaces that diff with the operations above. |
| SD6 | Change types carry no terminal, viewport, frame or damage concepts (A1, E9). |
| SD7 | `TranscriptChanges` is an in-process, id-only notification for renderers in the same process: no item payloads, no schema version, never serialized. `SessionViewPatch` (`TranscriptViewPatchOp`) stays the only portable change contract (E7). To prevent the two from drifting, `SessionView` derives patch operations from each operation's `TranscriptChanges` (`Revised` → `Replace`, `Removed` → `Remove`, `Inserted` at the tail → `Append`, any other insertion or `Reset` → bounded `Reset`), and a unit test checks that applying the derived patch to the previous snapshot yields the new document. |

### 3.2 Resident window manager (`window`)

| Aspect | Contract |
|---|---|
| Owned state | `ResidentWindow { mode: WindowMode, first_sequence, last_sequence, event_count, has_older, has_newer, in_flight: { older, newer, replace }, generation, first_available_sequence, last_available_sequence, rows_per_event_estimate }` |
| Inputs | `TranscriptChanges`, page/window results, navigation demands (rows beyond each viewport edge), session bounds. |
| Outputs | `PageRequest` / `WindowRequest` effects (data; executed by the host), `EvictionPlan`, `PositionEstimate`. |
| Invariants | W1–W13 and W7a (section 5). |

```rust
pub enum WindowMode { Live, Detached }
pub enum PageRequest { Older { before_sequence: u64 }, Newer { after_sequence: u64 } }
pub enum WindowRequest { Around { sequence: u64 }, Head, Tail, LiveState } // LiveState: W11 rehydration only
pub struct PageCaps { pub max_items: usize, pub max_events_scanned: usize, pub max_bytes: usize }
pub struct PositionEstimate { pub rows_before_window: u64, pub resident_rows: u64, pub rows_after_window: u64 }
```

Every request carries the window `generation`; results for an older generation are dropped. The
generation is bumped by every window replacement and by an eviction that cancels an in-flight
page on the trimmed side, so a cancelled page's late result can never match a later request in
the same direction.

### 3.3 Units and item layout (`units`, `layout`)

#### Units

A *unit* is one VirtualList item. Units are derived from the document plus resolved structure.

```rust
pub enum UnitKey {
    Item(TranscriptViewItemId),           // one transcript item (or member of an expanded group)
    GroupHeader(TranscriptViewItemId),    // keyed by first member id
    HiddenMarker(TranscriptViewItemId),   // "▸ N hidden", keyed by first hidden id
    Boundary(BoundaryEdge),               // Older | Newer resident-window boundary row
}
```

| Rule | Statement |
|---|---|
| U1 | Unit order equals document order; grouping never reorders or crosses turns (A14). |
| U2 | A collapsed group contributes only its header unit; expanding inserts member units after it. An anchor on a member that disappears remaps to its group header (or hidden marker), keeping the row offset clamped to that unit. |
| U3 | `Boundary` units have a constant height of 1 row in every state, and state changes are paint-only revisions. `Boundary(Older)` always exists and shows "earlier messages available", "loading" or "start of session". `Boundary(Newer)` exists only in `Detached` mode ("newer messages available" or "loading"); the `Detached → Live` transition removes it as a membership change below the reader, which never moves content at or above the anchor. |
| U4 | Unit derivation is incremental from `TranscriptChanges`; a membership change touches only affected units (D3). |
| U5 | Structure resolution (presentation-policy rules, plugin facts, visibility floor, disclosure overrides) runs per changed unit and is memoized (D11, 256 entries). Disclosure overrides are ephemeral, bounded, oldest-first, reconciled on residency (D12). |

#### Item layout

| Aspect | Contract |
|---|---|
| Owned state | `ItemLayoutCache` keyed by `(UnitKey, item revision, LayoutFingerprint, width)`; retained `ItemLayout { height, rows: retained BMUX layout node, source_map, focus_targets, hit_regions, selection_fragments, content_keys }`. |
| Inputs | Unit, `ItemRenderContext`, width. |
| Outputs | Exact height at the current width plus all per-item geometry from one layout definition (B3). |

```rust
pub struct ItemRenderContext<'a> {
    pub width: u64,
    pub theme: &'a ResolvedTheme,            // bcode_tui_theme; includes chrome, density, icons, motion
    pub structure: &'a ResolvedUnitPresentation, // presentation-policy result for this unit
    pub disclosure: DisclosureState,
    pub markdown: &'a MarkdownOptions,
    pub diff_viewer: &'a DiffViewerOptions,
    pub plugin_visuals: &'a PluginVisualState,  // accepted adapter rows (computed outside drawing)
}
pub struct LayoutFingerprint(u64); // hash of every layout-affecting input above, except width
pub struct PaintInputs<'a> { pub clock: FrameClock, pub focus: Option<&'a FocusedTarget>, pub selection: &'a SelectionState, pub activity: &'a ActivityPhase }
```

| Rule | Statement |
|---|---|
| L1 | The layout cache is identity-keyed. Inserting, removing or reordering units never invalidates other units. Heights feed BMUX `MeasuredListIndex` incrementally (BMUX 5); V2 has no Fenwick tree of its own. |
| L2 | All render configuration reaches producers through `ItemRenderContext` and `PaintInputs` parameters. V2 code declares no `thread_local!` and no mutable statics. |
| L3 | `LayoutFingerprint` contains only layout-affecting values (theme layout roles, density, chrome borders, structure, disclosure, Markdown and diff options, accepted plugin visual revision). It never contains pointer addresses, wall-clock time, transcript length, history-loading state, focus, selection, clock phase or elapsed time. |
| L4 | Producers are pure functions `fn(&Unit, &ItemRenderContext) -> impl Component` (in code: `UnitProducer::produce(UnitView, &ItemRenderContext) -> ProducedItem`, whose owned `ItemContent` component also derives the item geometry from its own retained layout node). Shared recipes from `bcode_tui_components` are used only when they satisfy this signature; otherwise V2 implements them in `layout::producers`. |
| L5 | Focus highlight, selection highlight, spinner/elapsed labels, glow phases and boundary-unit state are paint-only: they change paint revision, not layout revision, and are applied by BMUX style overlays, not span splitting. |
| L6 | Rows, source mappings, focus targets, hit regions and selection fragments come from the same retained layout (B3, B6, B12). Nothing is re-wrapped or re-measured after measurement. |
| L7 | Markdown: one accepted projection per resident item, consumed by rows, anchors, focus, hits and images; installed only on exact id/revision/options match (D2). See ambiguity 1. |
| L8 | Asynchronous visuals (Mermaid, images, plugin adapters) reserve explicit placeholder rows; acceptance is an item revision. Plugin adapters run outside drawing (D9). |
| L9 | Every resident unit has an exact height at the current width before the update that made it resident returns (rule W3). Width change re-measures resident units; anchors survive via P1/P3. |

How the session's presentation features plug in:

| Feature | Plug-in point |
|---|---|
| Structure v2 (groups, sections, visibility floor, disclosure targets) | `units` (group/marker units) + `ItemRenderContext::structure` + `DisclosureState`; disclosure targets are focus targets (BMUX 12). |
| Themes and profiles | `ItemRenderContext::theme`; theme change = new fingerprint for every unit (full damage, structural). |
| Chrome (borders via `LineFrame`, speaker, surfaces, separators, density, icons, motion) | Producers wrap content with BMUX `LineFrame` using theme chrome; motion feeds `ScrollMotion` duration (0 under reduce-motion). |
| Plugin visuals | Accepted rows in `PluginVisualState`, renderer-neutral fallback otherwise (A8, E1–E5). |

### 3.4 Viewport (`viewport`)

| Aspect | Contract |
|---|---|
| Owned state | No navigation state. A BMUX `VirtualListState<UnitKey>` in controlled mode (VP5) that is a derived projection of `NavState`, plus scrollbar drag interaction state. |
| Inputs | Unit keys and retained layouts (via `VirtualItemSource`), viewport rect, `ListPosition` from navigation. |
| Outputs | Visible range, per-item offsets, scrollbar geometry, list-level events (wheel, scrollbar drag, selection autoscroll) translated into navigation intents. |

| Rule | Statement |
|---|---|
| VP1 | The viewport stores no navigation policy. It resolves a `ListPosition` (section 4) into offsets; policy lives in `navigation`. |
| VP2 | Tail space is BMUX overscroll (BMUX 3) configured with `viewport_height - 1` rows (ambiguity 8). |
| VP3 | Only visible, correctly positioned units paint and register interaction metadata (B7). |
| VP4 | The scrollbar is optional (chrome `scrollbar`, default `off`), positioned within the whole session using `PositionEstimate` (BMUX 7); dragging emits `JumpToFraction`. |
| VP5 | Controlled list. Before every layout and paint the viewport writes the BMUX position derived from `NavState` (`set_position`), and it never reads the list's anchor, offset or follow flag back. The list never changes its own position: its wheel, page, scrollbar and autoscroll handling only returns `VirtualListAction`s (BMUX 1), which the viewport translates into `NavIntent`s. Anchor preservation across membership and height changes is computed by navigation from `ListPosition` (P1, P4), not by `VirtualListState::sync` or `restore_anchor`. BMUX 2 adds this controlled mode; until it lands V2 does not use the list's own scroll state. |
| VP6 | One measurement cache. `ItemLayoutCache` (L1) is the only source of unit heights and retained layouts. The list's `MeasuredListIndex` is fed from it incrementally (BMUX 5) through `VirtualItemSource` (BMUX 6); the list's internal `LayoutCache` is not used for transcript units. |
| VP7 | Type mapping, one way, per frame. Bcode's `ListPosition`, `ReadingAnchor`, `AnchorOffset`, `ScrollTarget` and `Placement` are navigation policy types; BMUX `ListPosition<K>`, `ListAnchor<K, S>`, `AnchorOffset<S>`, `ScrollTarget<K>` and `AnchorPolicy` (BMUX 2, 4) are mechanics. `FollowBottom` → `FollowBottom`, `Anchored` → `Anchored`, `TailSpace` → `Overscrolled`; `ItemTop(k)` → `Key(k, ItemTopAtTop)`, `ItemBottom(k)` → `Key(k, ItemBottomAtBottom)`, `LatestBottom` → `Bottom`, `Anchor(a)` → `Anchor(a)`. `NavState.motion` stores the Bcode `ScrollMotion`; BMUX 4 supplies only its sampling function. |

### 3.5 Navigation controller (`navigation`)

| Aspect | Contract |
|---|---|
| Owned state | `NavState` (section 4.3) — the single position authority. |
| Inputs | `NavInput` (intents, content changes, page/window results, timers, geometry). |
| Outputs | Updated `NavState` and `Vec<NavEffect>` (page/window requests, timer schedules, invalidations). |
| Invariants | N1–N12 (section 4). |

`navigation::auto` holds the automatic navigation policy (submission acceptance reveal,
assistant segment reveal, overflow handoff); it emits intents into the same reducer.

### 3.6 Frame (`frame`)

| Aspect | Contract |
|---|---|
| Owned state | None between frames except the last committed `FrameSnapshot` id and its interaction scenes (BMUX-owned). |
| Inputs | Engine state (read-only), one `FrameClock`, chat-screen rect, composer/status components from the host. |
| Outputs | `FrameSnapshot`, painted cells and metadata, damage regions. |
| Invariants | F1–F7 (section 9). |

---

## 4. Position and navigation model

### 4.1 Position authority

```rust
pub struct ReadingAnchor { pub unit: UnitKey, pub offset: AnchorOffset }
pub enum AnchorOffset {
    Row(u64),                                         // row within the unit at the viewport top
    Source { position: SourcePosition, row_delta: u64 }, // mapped source position + rows below it
}
pub enum ListPosition {
    FollowBottom,                                 // last content row on the viewport bottom row
    Anchored { anchor: ReadingAnchor },           // anchor row on the viewport top row
    TailSpace { anchor: ReadingAnchor },          // top-origin, blank rows below content derived
}
```

`SourcePosition` is a Bcode type in `bcode_tui_transcript::layout` (Markdown byte position, diff
line, shell range, plugin content key). BMUX sees it only as the opaque type parameter `S` of
BMUX 2 and 10 and never interprets it.

| Rule | Statement |
|---|---|
| P1 | The only stored position is a `ListPosition`. Top row, blank tail rows and scrollbar offset are derived from it and the current geometry every time they are needed. No numeric top row is stored. |
| P2 | `Row` offsets are clamped to the surviving unit's height (C23). |
| P3 | `Source` offsets are used when the row maps to source (Markdown byte position, diff line, shell range, plugin content key) and resolve against the new projection after reflow; unmapped rows use `Row` and never snap to earlier source (C24). |
| P4 | If the anchor unit is removed: member → group header/marker (U2); otherwise the nearest surviving unit in document order *after* it, row 0; if none, the nearest before it at its last row. Missing correspondence never implies `FollowBottom` (C17). |

### 4.2 Navigation intents

```rust
pub enum NavIntent {
    Follow,                                        // explicit attach to latest bottom
    ScrollBy { rows: i64, source: InputSource },   // wheel, arrows, selection autoscroll
    PageBy { pages: i32 },                         // Page Up/Down (10 rows per D1 unless viewport-based, see 11-P2)
    RevealItem { item: UnitKey, sticky: bool },    // automatic reveal (submission: false, assistant: true)
    JumpToItem { item: UnitKey, placement: Placement },
    JumpToSequence { sequence: u64, placement: Placement }, // timeline
    JumpToFraction { fraction: f64 },              // scrollbar drag; maps to a sequence estimate
    JumpToSessionStart,
    JumpToLatest,
    FocusTarget { target: FocusTargetId, edge: ApproachEdge },
    ReleaseHold,                                   // stop following/holding at the current visual position
    RestorePosition { position: ReadingPosition }, // session resurrection
}
pub enum InputSource { Wheel, Key, SelectionAutoScroll, Programmatic }
pub enum Placement { ItemTopAtTop, ItemBottomAtBottom, Center, EnsureVisible }
pub enum ApproachEdge { Top, Bottom }

pub enum NavInput {
    Intent { intent: NavIntent, at: Instant },
    Content { changes: TranscriptChanges, at: Instant },     // after window/units/index are updated
    PageResult { generation: u64, direction: PageDirection, inserted_rows: u64, exhausted: bool },
    WindowResult { generation: u64, target: WindowTarget },
    Geometry { viewport: ViewportGeometry },                 // resize; latest-bar reservation change (F6)
    Timer { key: NavTimer, at: Instant },                    // NavTimer::{GraceExpired, MotionFrame}
    SubmissionAccepted { item: UnitKey, at: Instant },
    PresentationCommitted { frame: FrameId },
}
pub enum NavEffect { Page(PageRequest), Window(WindowRequest), ScheduleTimer { key: NavTimer, at: Instant }, CancelTimer(NavTimer), Invalidate(Region) }

pub fn reduce(state: &mut NavState, input: NavInput, view: &NavView<'_>) -> Vec<NavEffect>;
```

`NavView` gives the reducer read access to the window, units and measured index (plus
`UnitFacts`: unit roles for `navigation::auto`, sequence lookup for timeline jumps, and the P4
correspondence of units removed by the change being reduced, supplied by the units layer), and can
measure units lazily (exact, bounded, rule L9).

| Rule | Statement |
|---|---|
| N1 | `NavState` is the sole navigation authority. No other module stores position, follow, hold, reveal, grace, pending movement, pending jump or motion state. BMUX list state is a derived projection that is overwritten from `NavState` and never read back (VP5). |
| N2 | Every change to `NavState` goes through `reduce`. Keys, mouse, scrollbar, focus traversal, automatic policy, timeline, engine switch and resurrection all produce `NavInput`. |
| N3 | Rows are derived (P1). The reducer never stores row indices across inputs. |

### 4.3 State

```rust
pub struct NavState {
    pub mode: NavMode,
    pub motion: Option<ScrollMotion>,          // keyed target; sampled with BMUX 4 mechanics (VP7)
    pub grace_until: Option<Instant>,          // manual-input grace
    pub pending_movement: Option<PendingMovement>,
    pub pending_jump: Option<PendingJump>,
    pub pending_reveals: BTreeMap<UnitKey, RevealRequest>, // submissions awaiting acceptance
    pub admitted: BTreeSet<UnitKey>,           // automatic admission by identity (resident only)
}
pub enum NavMode {
    Following,                                 // ListPosition::FollowBottom
    Holding { item: UnitKey, sticky: bool },   // item top on viewport top; blank rows below allowed
    Reading { anchor: ReadingAnchor },         // detached manual reading
    TailSpace { anchor: ReadingAnchor },       // detached, inside virtual space below content
}
pub struct PendingMovement { pub direction: VerticalDirection, pub rows: u64, pub generation: u64 }
pub struct ScrollMotion { pub from: ReadingAnchor, pub target: ScrollTarget, pub started: Instant, pub duration: Duration, pub easing: Easing }
pub enum ScrollTarget { ItemTop(UnitKey), ItemBottom(UnitKey), LatestBottom, Anchor(ReadingAnchor) }
```

### 4.4 State machine

`grace` = `grace_until > at`. Automatic transitions are those caused by `Content` or
`SubmissionAccepted` inputs.

| # | From | Input | Guard | To | Effects |
|---|---|---|---|---|---|
| T1 | Following | Content (growth) | — | Following | — |
| T2 | any, window Live | SubmissionAccepted(item) | reveal pending for item | Holding{item, sticky:false} via motion ItemTop | clears earlier submission reveals; detached windows use T27 |
| T3 | Following / Holding{k, _} | first visible content of a new assistant segment `a` (`a` ≠ `k`, not yet admitted, still streaming; an assistant message that arrives complete is ordinary growth, T1) | not Reading/TailSpace; not grace | Holding{a, sticky:true} via motion ItemTop | supersedes any earlier hold, sticky or not, exactly once per segment (C12, N10) |
| T4 | Holding{k, false} | Content extends past viewport bottom | not grace | Following | bottom-align only the overflow (C11, C13) |
| T5 | Holding{k, true} | Content extends past bottom | — | Holding{k, true} | latest bar may show |
| T6 | Holding{k, true} | new non-assistant unit admitted | — | Holding{k, false} | no movement if content fits (C11) |
| T7 | Holding{k, true} | assistant segment ends | — | Holding{k, true} | hold survives completion (C11) |
| T8 | any | ScrollBy/PageBy (manual) | — | Reading, or TailSpace if the result lies below content bottom | materialize motion at its visual position; cancel motion, pending reveals of already-pending submissions, pending jump, pending movement of the opposite direction; set grace (C14) |
| T9 | Reading | ScrollBy down ending with content bottom on viewport bottom | window Live | Following | — |
| T10 | Following / Reading at bottom | ScrollBy down | window Live | TailSpace{anchor} | blank rows ≤ `viewport_height - 1` |
| T11 | TailSpace | Content fills the virtual space to the viewport bottom | regardless of grace (C4) | Following | automatic reveal re-enabled (ambiguity 9) |
| T12 | TailSpace | ScrollBy up | — | TailSpace or Reading | detaching keeps tail space until content bottom leaves the viewport (C7) |
| T13 | any | JumpToLatest / Follow | window Live | Following via motion LatestBottom | — |
| T14 | any | JumpToLatest / Follow | window Detached | Following after `WindowResult(Tail)` | emits `Window(Tail)`; pending_jump set |
| T15 | any | JumpToSessionStart | head resident | Reading{Boundary(Older) row 0} via motion | — |
| T16 | any | JumpToSessionStart | head not resident | Reading after `WindowResult(Head)` | emits `Window(Head)` |
| T17 | any | JumpToItem / JumpToSequence (resident) | — | Reading at placement: `JumpToItem` via motion; `JumpToSequence` (timeline) at once, so the next committed frame shows the target (BUG-B4) | cancels pending movement/reveals |
| T18 | any | JumpToSequence (not resident) / JumpToFraction | fraction outside the resident rows | Reading after `WindowResult(Around)` | emits `Window(Around)`; a fraction that lands inside the resident rows (W12 estimate) becomes `Reading` at that row directly |
| T19 | any | FocusTarget{target, edge} | target not fully visible | Reading via motion (C32) | none if visible |
| T20 | Following / Holding | ReleaseHold | — | Reading at current visual position | — |
| T21 | any | RestorePosition | — | Reading/Following per payload | may emit `Window(Around)` |
| T22 | pending_jump | any manual intent | — | per T8 | cancels pending jump and drops its result (C14) |
| T23 | Reading | Content (any) | — | Reading | anchor unchanged; no automatic movement (C10, C16, C26) |
| T24 | any | Timer(GraceExpired) | `grace_until ≤ at` | same mode; `grace_until` cleared | re-evaluates once against current content: T3 for the newest assistant segment whose first visible content arrived during grace (if not yet admitted), then T4; intermediate content is not replayed (N5) |
| T25 | any | Timer(MotionFrame) | motion running | same mode; `motion` cleared when `at ≥ started + duration`, otherwise unchanged | completion resolves the keyed target against current geometry (N7); earlier ticks only emit `Invalidate` |
| T26 | Reading / TailSpace | PageResult(direction) | `pending_movement` in that direction (any page the window manager accepted; stale generations never reach the reducer) | Reading or TailSpace per the resulting position | spends the movement once against rows of newly inserted units (N8); zero rows: keeps it and requests the next page (subject to W7a); exhausted: drops the remainder |
| T27 | any, window Detached | SubmissionAccepted(item) | reveal pending for item | unchanged until `WindowResult(Tail)`, then T2 | emits `Window(Tail)` (W10) and sets `pending_jump`; T2 applies once the tail window is incorporated. A manual intent before the result cancels both (T22); the reader stays detached and the latest bar shows newer activity |

| Rule | Statement |
|---|---|
| N4 | Automatic movement occurs only for T2 (also when reached through T27), T3 (also when re-evaluated by T24), T4, T11 and the overflow part of T6 (C10). |
| N5 | During grace, T3 and T4 are suppressed (deferred until grace expires and re-evaluated once by T24, not replayed); content still fills tail space (T11). Explicit user intents and T2 for a new submission are never suppressed (ambiguity 3). Grace is 450 ms. |
| N6 | Manual input during motion materializes the motion's visual position (sampled at the input's `at`) as a keyed anchor before applying the delta. |
| N7 | Motion targets are keys (`ScrollTarget`), re-resolved against current geometry at every sample. Completion applies the target after correspondence; timers only request frames (C17). Duration 0 under `reduce_motion` / motion `none`; toggling mid-motion settles at the target (C22). |
| N8 | Pending movement (C28/C29): input at a resident boundary first traverses available rows; the remainder is stored and a page is requested if none is in flight. It is not tied to a window generation: an eviction that cancels the page or an abandoned replacement bumps the generation, and the page requested afterwards still satisfies it, because the window manager drops stale results before reduction. A window replacement drops it. On `PageResult`, it is spent once, only against rows of newly inserted units in that direction; reflow, rows on the other side and non-presenting content never consume it. Reversal cancels it. A zero-row page does not consume it and immediately requests the next page while `has_older`, subject to the W7a ceiling (T26). The `Older` boundary row stays at row 0 while a page is inserted below it, so a reader anchored on it is re-keyed by the page's inserted rows before spending (the content it was reading stays in place); every `Older` page is therefore reduced with its measured `inserted_rows`. |
| N9 | Automatic navigation is decided only in `navigation::auto` from `Content`/`SubmissionAccepted` inputs. Latest-bar activity code never emits navigation (ambiguity 5). Staged "sending" submissions never move the view; a cleared submission never anchors (C8). |
| N10 | Admission is by stable identity, once, among resident units; reorder, revision or tail removal never re-admits (C12). |
| N11 | Content input is reduced only after the window, units and measured index already reflect the change (W3). Therefore any later manual input in the same or a later update is computed against the keyed anchor and current geometry; there is no deferred resynchronization. |
| N12 | Without an anchor unit (empty document), position is `Following`; growth from empty never infers a prepend (C30). |

### 4.5 Checkpoint and commit semantics

| Rule | Statement |
|---|---|
| CK1 | Frame preparation and painting never mutate `NavState`, the window or units. They may fill derived caches only (layout cache, measured heights); derived caches cannot change the keyed position. |
| CK2 | Consequently navigation needs no checkpoint/restore: a repeated or abandoned (prepared but not presented) preparation has no navigation side effects (C27 holds by construction). A presenter failure terminates the BMUX runtime (`docs/tui-runtime.md`), so it needs no recovery rule. |
| CK3 | `PresentationCommitted` records the committed frame id, publishes BMUX interaction scenes for input routing, acknowledges latest-bar activity for units shown in that frame, and does not touch `NavState`: motion frames are scheduled by the reducer when a motion starts and on every `Timer(MotionFrame)` (T25), so commit needs no clock. Nothing else depends on commit. |
| CK4 | Input routing (hits, selection, focus geometry) uses only the last committed frame's scenes (BMUX contract). |

---

## 5. Resident window and paging

| Rule | Statement |
|---|---|
| W1 | The resident window is a contiguous range of whole turns of canonical history plus, in `Live` mode, live-only items after it. |
| W2 | `Live`: the window contains the session tail and live events apply. `Detached`: the window does not contain the tail; live transcript events are not applied to the projection; the window records `latest_known_sequence` and newer-activity for the latest bar. Non-transcript hydrated state (permissions, interactions, runtime work, usage, composer) continues in both modes. |
| W3 | A page/window result is incorporated in a single update: `SessionView` operation → units → layout measurement of the new units (exact, bounded by page caps) → `MeasuredListIndex` splice → `NavInput::PageResult`/`WindowResult`. No later frame is needed to restore correspondence. |
| W4 | Window edges are safe window starts (turn starts). Page and window requests are expanded or trimmed to the nearest safe boundary within caps; a turn larger than the caps is loaded whole by successive pages marked `UnalignedBoundary` and incorporated via the SD2 fallback. |
| W5 | Prefetch: whenever rows between a viewport edge and the corresponding window boundary fall below `prefetch_rows = max(2 × viewport_height, 48)` and that side has more history, request a page in that direction. At most one request in flight per direction; one window replacement at a time (a replacement cancels in-flight pages by generation). |
| W6 | Page sizing: each page is filled up to the per-request caps — 16 items, 512 events scanned, 512 KiB (D1) — and ends on a safe boundary. Requests continue until `prefetch_rows` is satisfied or the side is exhausted (see 11-P1). |
| W7 | Eviction runs after incorporation, in the same update, only when `event_count > 1024` (max). It removes whole turns from the side opposite to the side just extended, down to 512 events (target). Never evicted: the anchor unit, the motion target unit, units holding a selection endpoint, and the *protected band*: units intersecting `[viewport top − prefetch_rows, viewport bottom + prefetch_rows]`, taken outward from the viewport and capped at 512 events in total (units nearest the viewport first, the anchor unit always included). Head-side eviction never removes live-only or streaming items; tail-side eviction in `Live` mode follows W9 (SD3). While a selection gesture or selection is active, or a motion is running, eviction is deferred, so selected source text and motion geometry stay resident; the W7a ceiling still applies. A single turn larger than the target may be cut on its far side at an unaligned boundary (SD2 fallback for that edge). |
| W7a | Ceiling: the window never exceeds max + one page = 1,024 + 512 = 1,536 events. No W5 prefetch or N8 page request is issued while `event_count + 512 > 1,536` and eviction cannot make room (for example while eviction is deferred). Pending movement then waits at the boundary, whose unit shows `loading`, and resumes when eviction frees room. Zero-row pages count toward the ceiling like any other page. |
| W8 | Timeline (`Around`) windows are aligned to whole turns and sized ≤ 512 events total (256 before / 256 after before alignment), so the first newer or older page never triggers eviction. |
| W9 | Evicting the tail side in `Live` mode switches to `Detached` and discards live-only and streaming items (SD3). Returning uses `Window(Tail)`, the W10 tail request that V2 session attach also uses, which also rehydrates in-flight live state. |
| W10 | `JumpToSessionStart` and `JumpToLatest` load `Window(Head)` / `Window(Tail)` directly with one request when the target is not resident; they never page through intermediate history. `Head`, `Tail` and `Around` share one window request shape: turn-aligned, ≤ 512 events scanned, ≤ 64 items, ≤ 512 KiB, filled until `viewport_height + prefetch_rows` rows are covered where the session allows (`Around` is centered per W8). V2 session attach uses the same `Tail` request; V1's attach request (12–64 items, 2,048 events scanned) is not reused. |
| W11 | Transition `Detached → Live` happens when a newer page reports no more history. From the moment a `Newer` request is issued, the engine buffers live transcript events (bounded: 512 events or 512 KiB; overflow marks the buffer overflowed and stops buffering). When the page reports `has_newer = false`, the engine switches to `Live` in the same update and applies buffered events with sequence > the newest resident sequence, plus buffered live-only events, in arrival order. If the buffer overflowed, or a live-only stream was already in flight when the request was issued (W2 newer-activity), it issues `Window(LiveState)` instead: the live-state part of the W10 tail attach, without replacing the durable window; `Boundary(Newer)` shows `loading` until it is incorporated. When the page reports more history, the buffer is discarded. |
| W12 | Position estimate (scrollbar, `JumpToFraction`): `rows_before = (first_resident_sequence − first_available_sequence) × r`, `rows_after = (last_available_sequence − last_resident_sequence) × r`, where `r` is the resident rows-per-event average (exponentially smoothed across windows, initial 1.0). Estimates are presentation only and never used for navigation correctness. `JumpToFraction` maps the fraction to a sequence by inverting the estimate and issues `Window(Around)`. |
| W13 | The `Older`/`Newer` boundary units show availability/loading/exhaustion without height changes (U3). Resident-window bottom is never treated as session tail (C31). |

---

## 6. Focus model

```rust
pub enum ChatFocus { Composer, Transcript { target: FocusTargetId, return_to: ReturnFocus } }
```

Transcript keyboard focus is unit focus: there is one transcript focus mode, entered with
`alt+u` (user decision; no separate "free" transcript focus and no extra focus key).

| Rule | Statement |
|---|---|
| FO1 | Exactly one of composer or transcript has keyboard focus. Default is `Composer`. Focus is ephemeral presentation state. |
| FO2 | Composer → Transcript: `alt+u` (`tui.transcript.focusUnits`, remappable in the `chat` scope) or the command-palette action. Initial target = last visible target, else nearest above (C33). Pointer input never moves keyboard focus. |
| FO3 | Transcript → Composer: `Escape` (after closing the inspector, if open), `alt+u` again, a pointer press in the composer, or the command-palette action. Keys bound in the `transcript_focus` scope are consumed by the transcript; any other key or paste returns focus to the composer and is delivered to it. |
| FO4 | **Home/End (user decision):** composer focused → start/end of the text input, never the transcript. Transcript focused → `JumpToSessionStart` / `JumpToLatest` (window loaded directly, W10); focus moves to the first / last target of the loaded window. |
| FO5 | Page Up/Page Down scroll the transcript from either focus without changing focus. `ctrl+home` / `ctrl+end` (existing V1 defaults `TranscriptTop` / `TranscriptBottom`, next to `ctrl+up` / `ctrl+down` line scrolling) keep their keys from either focus; in V2 they mean `JumpToSessionStart` / `JumpToLatest` with direct `Head` / `Tail` loads (W10) instead of paging through the resident window. |
| FO6 | Transcript focus keys (scope `transcript_focus`, remappable): `j`/`k`/Up/Down, Enter/Space, `l`/`h`, `e`, `d`, `i`, shift+Up/Down, `m`, Home/End (FO4), Escape, as in [`tui-themes.md`](tui-themes.md#focus-mode). Moving focus emits `FocusTarget{target, edge}` (C32). |
| FO7 | Focus targets are BMUX item focus targets (BMUX 12) derived from the retained item layout (B12); focus highlight is a paint-only overlay (L5). |
| FO8 | A focus target leaving the resident window moves focus to the nearest surviving target; focus never forces paging. |
| FO9 | Clicking a disclosure header or section label (press and release without drag, BMUX 14) toggles it; dragging selects. Neither changes keyboard focus. |
| FO10 | The chat scope's double-Escape `AppInterrupt` (`multi_tap_bind("escape", …)` in `keymap.rs`) counts only taps delivered while the composer has focus. An Escape consumed by transcript focus (closing the inspector or returning to the composer) is not a tap of that sequence; a second Escape after returning to the composer starts a new sequence. |

---

## 7. Engine selection

| Name | Value |
|---|---|
| Environment variable | `BCODE_TUI_ENGINE=v1\|v2` |
| CLI flag | `--tui-engine <v1\|v2>` (global flag on the root `bcode` command) |
| Config key | `[tui] engine = "v1" \| "v2"` (`bcode_config::TuiConfig::engine: Option<TuiEngine>`) |
| Rust enum | `bcode_config::TuiEngine { V1, V2 }` (serde `"v1"`, `"v2"`) |
| Resolution | `bcode_tui::engine_selection::resolve_tui_engine(cli: Option<TuiEngine>, env: Option<&str>, config: Option<TuiEngine>) -> Result<TuiEngine, EngineSelectionError>` |

| Rule | Statement |
|---|---|
| EN1 | Precedence: CLI flag > environment variable > config > default. Default is `v1` until phase 7, then `v2`. |
| EN2 | Invalid CLI values are rejected by argument parsing. An invalid environment value is a startup error naming the valid values; there is no silent fallback. Config values are schema-validated. |
| EN3 | The engine is chosen once at startup and fixed for the process lifetime. There is no runtime switching (user decision: not worth the complexity); switching engines means restarting with another flag, environment value or config. |
| EN4 | Each engine owns its own `SessionView` instance and consumes daemon data through the same bounded attach and history paths. Host shell state (composer, overlays, permissions, plugin surfaces, configuration) is engine-independent. |
| EN5 | Shared code between engines is only the section 1.3 layers and the host shell. The seam in `bcode_tui` is a single dispatch point choosing which engine handles transcript ingest, transcript input and transcript painting. |

---

## 8. BMUX upstream work

All APIs are generic: no Bcode names, no product policy. BMUX owns mechanics; Bcode supplies
policy. Names are sketches; BMUX review may rename them without changing the contract.

| # | Capability | API sketch | Phase |
|---|---|---|---|
| 1 | VirtualList as a real Component with integrated scroll chrome | `pub struct VirtualListComponent<'a, K, S: VirtualItemSource<K>>` implementing `Component`; `VirtualListChrome { scrollbar: ScrollbarMode /* Off, Overlay, Gutter */, wheel_rows: u16, page: PageStep }`; `event()` returns `VirtualListAction<K>::{ScrollRequested(i64), PageRequested(i32), ScrollbarDrag(f64), SelectionAutoScroll(i64), Activated(K)}` so the caller's policy decides. | 1 |
| 2 | One anchor-based position model | `pub struct ListAnchor<K, S = ()> { key: K, offset: AnchorOffset<S> }`; `pub enum AnchorOffset<S> { Row(u64), Source { position: S, row_delta: u64 } }` where `S: Clone + Ord` is an opaque caller-defined source position that BMUX never interprets; `pub enum ListPosition<K, S = ()> { FollowBottom, Anchored(ListAnchor<K, S>), Overscrolled(ListAnchor<K, S>) }`; a controlled mode (`VirtualListState::controlled()`) in which the list never changes its own position and only reports `VirtualListAction`s (VP5); `pub enum AnchorPolicy { ItemTopAtTop, ItemBottomAtBottom /* never pushes a taller item's top off */, Center, EnsureVisibleFrom(Edge) }`; `VirtualListState::{set_position, position, resolve_top_offset, anchor_for(key, policy)}`; `FollowBottom` is released only explicitly. | 1 |
| 3 | Configurable overscroll / tail space | `pub trait OverscrollPolicy { fn max_below(&self, viewport_height: u64, content_height: u64) -> u64; }` with `OverscrollPolicy` impls `NoOverscroll`, `ViewportMinus(u64)`. | 1 |
| 4 | Animated scroll targets | `pub struct ScrollMotion<K> { from: ListAnchor<K>, target: ScrollTarget<K>, easing: Easing, duration: Duration, started: Instant }`; `pub enum ScrollTarget<K> { Key(K, AnchorPolicy), Bottom, Anchor(ListAnchor<K>) }`; `fn sample(&self, now: Instant, index: &MeasuredListIndex<K>) -> MotionSample { top_offset: u64, done: bool }`; driven by runtime keyed timers. | 1 |
| 5 | Incremental `MeasuredListIndex` | `insert(at: usize, items)`, `prepend(items)`, `append(items)`, `remove_range(Range<usize>)`, `splice(Range<usize>, items)`, `update_height(&K, u64)`; all O(k log n); `sync` stays for full rebuilds. | 1 |
| 6 | Lazy item construction | `pub trait VirtualItemSource<K> { fn len(&self) -> usize; fn key(&self, i: usize) -> &K; fn layout_revision(&self, i: usize) -> u64; fn paint_revision(&self, i: usize) -> u64; fn build(&self, i: usize) -> Box<dyn Component + '_>; }`; the list builds only visible and invalidated items. | 1 |
| 7 | Windowed data source | `pub struct WindowBounds { has_before: bool, has_after: bool }`; `pub struct PrefetchPolicy { before_rows: u64, after_rows: u64 }`; `fn boundary_signals(&self, position, viewport_height, bounds, policy) -> BoundarySignals { need_before: bool, need_after: bool }`; `fn eviction_plan(&self, anchor, protected_rows: u64, excess: EvictionBudget) -> EvictionPlan { before: Range<usize>, after: Range<usize> }` (never the anchor item or protected band); `ScrollbarState::with_estimate(PositionEstimate { before: u64, resident: u64, after: u64 })`. The estimate needs an amendment to BMUX `docs/tui-framework.md` ("Estimated heights are not part of the public contract unless measured evidence establishes that exact retained measurement cannot satisfy the supported scale"): it is a caller-supplied scrollbar geometry hint for content that is not resident at all, not an estimated item height, and exact retained measurement still governs every resident item. Evidence for the amendment: the resident window is capped at 1,536 events (W7a) while sessions are unbounded, so rows outside the window cannot be measured. If BMUX declines, `viewport` passes the estimated totals as plain content length and offset to BMUX's scrollbar geometry, under the same presentation-only rule (W12). | 1 |
| 8 | Per-frame presentation clock | `pub struct PresentationClock { now: Instant }`; `LayoutCx::clock()`, `PaintCx::clock()`; runtime samples once per presentation; tests inject. | 3 |
| 9 | Viewport-bounded text projection | `TextBlock::projection_window(layout: &LayoutNode, rows: Range<u64>) -> TextProjectionWindow` returning rows and source ranges for a row range of the retained layout without materializing the whole projection; `source_range_for_row`, `row_for_source_offset` and `source_rows` read the same retained row geometry. | 3 |
| 10 | Source-position anchoring across reflow | `pub trait SourceMapped<S> { fn row_for_source(&self, pos: &S) -> Option<u64>; fn source_for_row(&self, row: u64) -> Option<(S, u64)>; }` (same opaque `S` as BMUX 2; `source_for_row` also returns the row where the position begins, like the keyed `SourceMap`) consulted by anchor resolution for `AnchorOffset::Source`. | 3 |
| 11 | Scroll-region damage | `Damage::Scroll { region: Rect, rows: i64 }` coalesced by `DamagePolicy`; presenter option `ScrollRegionAcceleration::{Off, Auto}` using terminal scroll regions when safe. | 6 |
| 12 | Focus targets inside list items | `pub struct ItemFocusTarget<K> { key: K, id: FocusTargetId, rows: Range<u64>, parent: Option<FocusTargetId> }`; `FocusTraversal::{next, previous, parent}`; `reveal_for_focus(target, edge) -> ListPosition<K>` (edge-approach reveal using `AnchorPolicy`). | 6 |
| 13 | Selection across virtualized items | `pub struct LogicalSelectionEndpoint<K> { key: K, source_offset: usize }` surviving eviction and reflow; `SelectionAutoScrollRequest` routed to the list as `VirtualListAction::SelectionAutoScroll`. | 6 |
| 14 | Click activation through selection | `PointerGesture::Click { region }` when press and release occur without drag beyond `ClickSlop` over an interactive region, even while a selection is armed; drag beyond slop starts selection. | 6 |
| 15 | Deterministic runtime test harness | Landed in phase 0 as `bmux_tui_runtime::testing` behind the non-default feature `testing`: `ProgramHarness<P>` built with `HarnessBuilder`; `ManualClock` (plus `RuntimeHandle::now` and `schedule_timer_after`); scripted `push_key`/`push_mouse`/`push_resize`/`push_paste`/`push_message`/`push_timer` delivered by `step`/`deliver`/`settle`; commands and subscriptions on a paused Tokio clock; `PresentMode::{Manual, AfterEachEvent, Cadence}` with `present`/`force_present`/`present_if_due`; `FrameView` over the committed frame (cells, row text, `find`, style runs, cursor, hits, selection, semantics, focus, `PresentReport`). Any number of events and command results between frames; assertions on committed frames only. **15b** (with BMUX 11): `FrameView::damage()` exposing the damage regions the program declared for that frame, because `PresentReport` only has `changed_cells` and a backend-fallback `full_repaint`, which cannot tell regional from full damage under scrolling (D6, F5). | 0 (15b: 6) |

---

## 9. Frame pipeline and performance budgets

### 9.1 Pipeline

| Step | Where | Allowed mutation |
|---|---|---|
| 1. Update | `Program::update` → `engine.apply(EngineInput)` | Semantic (`SessionView` ops), window, units, layout invalidation and measurement of new/changed units, `NavState` via `reduce`, effects. |
| 2. Prepare | render callback → `frame::build_frame(&engine, clock, rect)` | Derived caches only (CK1). |
| 3. Paint | `frame::paint(&FrameSnapshot, &mut PaintCx)` | Nothing outside `PaintCx`. |
| 4. Commit | `Program::presentation_committed` → `NavInput::PresentationCommitted` | Commit records (CK3). |

| Rule | Statement |
|---|---|
| F1 | One `FrameSnapshot` per frame contains: viewport rect, latest-bar decision, derived top offset, visible units with offsets and retained layouts, scrollbar state, focus geometry, selection fragments, boundary state. |
| F2 | The clock is sampled once per frame (`FrameClock`, BMUX 8) and once per update for inputs; no other code reads `Instant::now()` or `SystemTime::now()` in V2 layout or paint paths. |
| F3 | Paint is pure: it reads the snapshot and writes only through `PaintCx` (B9). |
| F4 | Cells, hits, focus, selection, images and damage are produced from the same snapshot nodes in one pass (B3). |
| F5 | Damage is regional: composer, status row, latest bar and transcript viewport repaint alone; structural, theme, resize and overlay changes are full (D6). Scrolling uses `Damage::Scroll` when available (BMUX 11). |
| F6 | Latest-bar visibility is a pure function of (units, measured index, `NavState`, latest activity, full transcript-region height). It is evaluated at the end of every update, after the reducer ran (including the `PresentationCommitted` update that acknowledges activity), never in `build_frame`. When the decision changes, the engine reduces `NavInput::Geometry` with the effective viewport (region height minus the bar row) in the same update. Visibility is measured against the full region height, so the reservation cannot change the decision: one extra input, no oscillation (C19, ambiguity 6). `build_frame` copies the stored decision; navigation, W5 prefetch, the T4 overflow check and T19 visibility all use the effective viewport that is presented. |
| F7 | Spinner and elapsed labels advance on accepted invalidations, not wall time during drawing (D7). |

### 9.2 Budgets

| Budget | Value | Verification |
|---|---|---|
| Steady scrolling | No parse, highlight, projection or measurement; paints and registers only visible units (D4). | BMUX render stats + `LayoutWorkCounters` |
| Layout work counters | `bcode_tui_transcript` (feature `testing`) exposes per-frame `LayoutWorkCounters { markdown_parses, highlights, projections, measurements, producer_builds }`. Scroll-only frames keep every counter at zero; a prepend counts only the new units. | Scenario (D4, phase-3 exit) |
| Per-frame complexity | No O(resident units) work in prepare/paint; visible-range lookup sublinear. | Structural counters |
| Page incorporation | O(k log n) index splice + exact measurement of the k new units only. | Unit + counters |
| Prepend correctness | Zero anchor drift (row offset within anchor unit unchanged). | Scenario |
| Timeline jump / session start / latest | One window request (W10 shape: ≤ 512 events scanned, ≤ 64 items, ≤ 512 KiB); first frame after result shows the target. | Scenario |
| Resident window | Target 512 events, max 1,024, hard ceiling 1,536 (W7, W7a). | Unit |
| Page caps | 16 items, 512 events scanned, 512 KiB per request. | Unit |
| Frame cadence | `max_fps` default 60, max 240, 0 unlimited; cadence never delays semantics, cancellation or permissions (D5). | Existing runtime tests |
| Latency | p99 ≤ 100 ms (`scripts/capture-tui-product-latency.sh`). | Manual gate |
| Memory | RSS ≤ 33,554,432 bytes in the latency scenario. | Manual gate |
| Markdown worker | 1 active + 1 replaceable pending; install on exact id/revision/options match (D2). | Unit |
| Plugin visual adapters | Outside drawing, bounded queue, 2 s deadline, 512-entry cache, bounded rows/spans (D9). | Unit |
| Inline interactions | ≤ 4096 logical rows; ≤ 512 visible rows / 131,072 cells (D10). | Unit |
| Presentation memo | 256 entries; streaming re-resolves only the streaming unit (D11). | Unit |
| Smooth streaming | In `SessionView` (300 graphemes/s, 40 ms max lag, cap 1000 ms); TUI only schedules deadlines (D13). | Existing tests |

---

## 10. Acceptance criteria

IDs are kept from the requirements catalog (A–E). Additional IDs: `X<n>` for decisions and
session behaviors, `BUG-<id>` for regression scenarios of the diagnosed V1 bugs. Verification:
**S** scenario test (engine-level, deterministic, BMUX harness, asserts committed frames),
**U** unit test, **B** BMUX test, **G** mechanical guard (compilation, dependency or xtask
check), **M** manual. Every ID must be checked before phase 7.

### A. Hard invariants

| ID | Criterion | Verify |
|---|---|---|
| A1 | Shared/session-view/portable layers have no terminal, input, frame, viewport, key, mouse or BMUX dependencies (`bmux_color` only). | G |
| A2 | The TUI owns terminal layout, viewport, scroll anchoring, hit testing, cursor, input mapping, frame drawing and native plugin surfaces. | M (review); the dependency half is the A1/A6 guard |
| A3 | Interactive presentation choices persist only to user state; engine switching and focus are not persisted. | U |
| A4 | Renderers consume `SessionView` semantics; `SessionView` is the only transcript reducer; events applied once (SD1). | U, S |
| A5 | V2 holds ephemeral presentation state only; no second event-derived authority. | U; G: `cargo xtask architecture` rejects `bcode_tui_transcript` dependencies on `bcode_tui`, `bcode_client`, `bcode_ipc`, `bcode_session` and `bcode_server`, direct or transitive, except through `bcode_session_view`, whose client-backed actions use `bcode_client` and whose renderer neutrality has its own portable-presentation check (boundary added with the crate in phase 2) |
| A6 | Portable APIs get no terminal branches or data. | G |
| A7 | Native presentation may differ; parity means shared semantics. | M |
| A8 | Plugin presentation keeps a renderer-neutral fallback. | U |
| A9 | Plugin presentation is display data scoped to the plugin's own units. | U |
| A10 | Presentation never affects execution or persisted state. | U |
| A11 | Normal attach/history/renderer/context paths are bounded and non-mutating. | U, S |
| A12 | Terminal outcomes are absorbing. | U |
| A13 | Frontend contracts are portable; ordering, duplicates and terminal transitions explicit. | U |
| A14 | Chronological order is product meaning; grouping never reorders or crosses turns. | U |
| A15 | Tool output is untrusted and bounded before UI. | U |
| A16 | Visibility floor: awaiting-decision expanded with facts; failed never below summary; hidden leave a count marker; floor applies after manual toggles. | U, S |

### B. Layout mechanics

| ID | Criterion | Verify |
|---|---|---|
| B1 | Compose BMUX primitives; no parallel wrap/clip/measure engines; missing generic capability goes upstream. | G, M (review) |
| B2 | Never byte length, char count or format width as cell width. | U (CJK/emoji inputs) |
| B3 | Prefix widths, constraints, paint and hit regions from one layout definition. | U, S |
| B4 | Prefix and content constrained together; explicit narrow degradation. | U |
| B5 | Explicit overflow; wide graphemes bounded. | U |
| B6 | Measured rows retained with source mappings; no rewrap after measurement. | U |
| B7 | Hit regions only for visible, correctly positioned content. | S |
| B8 | Test matrix: zero/tiny widths, oversized prefixes, CJK, emoji, combining marks, styled spans, resize, clipping/selection/hit agreement. | U, S |
| B9 | Paint only via BMUX `PaintCx`. | G |
| B10 | Markdown via BMUX `TextBlock` projection; containers keep row count and clip overflow. | U |
| B11 | Language formatting width independent of terminal geometry. | U |
| B12 | Focus highlight, hits and traversal share retained anchor rows. | S |

### C. UX behaviors

| ID | Criterion | Verify |
|---|---|---|
| C1 | Position modes `Following`, `Holding{sticky}`, `Reading`, `TailSpace`; the navigation controller is the sole owner of follow/manual/tail/reveal policy (N1). | U |
| C2 | Scrolling down at the bottom enters virtual space up to `viewport_height − 1`; UI shows "N rows below latest". | S |
| C3 | Tail space is a top-origin reading position; blank rows derived from geometry. | U |
| C4 | New content fills virtual space even during grace; reaching the viewport boundary resumes following. | S |
| C5 | Growth/shrink never drifts; repeated measurement idempotent. | S |
| C6 | Correspondence resolved before tail exhaustion; rows inserted above don't consume virtual space. | S |
| C7 | Detaching in tail space keeps tail space. | S |
| C8 | Navigation waits for semantic acceptance; staged "sending" never moves the view; on acceptance the user message animates to the top leaving space below; cleared submission doesn't anchor. | S |
| C9 | Tool activity after a submitted message resumes following latest. | S |
| C10 | Automatic movement only for accepted submissions and first visible content of a new assistant segment. | S |
| C11 | New assistant item reveals its top and holds while growing (survives completion); a new non-assistant item releases into overflow following without moving fitting content; bottom-align only on overflow. | S |
| C12 | Admission once by identity; no re-admission; one superseding assistant transition. | U, S |
| C13 | Sticky reveal blocks overflow handoff until segment end; non-sticky hands off on overflow. | S |
| C14 | Manual scroll detaches, cancels reveals, pending correspondence, deferred jumps and pending submission reveals; materializes a running animation. | S |
| C15 | 450 ms grace suppresses stream top-anchoring and overflow follow (N5). | S |
| C16 | Stream doesn't anchor when the user scrolled up. | S |
| C17 | Targets item-top, latest-bottom, anchor; missing correspondence never implies follow; completion after correspondence; deadlines only request frames. | U, S |
| C18 | Latest bar only for a distinct newer entry fully below, or hidden activity; ignores hidden continuation of the visible entry (ambiguity 2). | S |
| C19 | Bar measured against the full body; cannot cause its own visibility. | S |
| C20 | Activity only when entries regenerate. | S |
| C21 | Glow 420 ms subtle / 630 ms full; instant under reduced motion; monotonic phase; final repaint still. | U |
| C22 | Reduced motion makes scrolls, glows and accent transitions instant; switching mid-animation settles at target. | S |
| C23 | Viewport preserved by stable id + row in item; restored offsets confined to the surviving item. | S |
| C24 | Source-backed anchors resolve after width change; unmapped rows fall back to item row. | S |
| C25 | Structural insertion and group collapse above the viewport keep the reading position. | S |
| C26 | Runtime-work events don't pull the final response to the bottom. | S |
| C27 | Repeated or abandoned (prepared, not presented) preparations have no navigation side effects; only commit records (CK1–CK3). Presenter failures terminate the runtime (CK2). | S (needs a prepare-without-present driver hook) |
| C28 | Boundary input traverses available rows first; unfulfilled movement spent once against measured page rows only. | S |
| C29 | Direction reversal cancels pending movement; unsolicited prepend preserves the reader. | S |
| C30 | Without an anchor, never infer a prepend from row growth. | U |
| C31 | Resident-window bottom is not the session tail. | S |
| C32 | Focus moving up puts the target's first row on top; moving down its last row on bottom (taller keep top); visible targets don't scroll. | S |
| C33 | Initial focus = last visible target else nearest above. | S |
| C34 | Selection uses logical source text; drag inside a response stays within it; from surrounding space can span items; follows scroll/paging/reflow. | S, B |
| C35 | Auto-copy on release; highlight clears 1000 ms after success; failure keeps selection; new gesture/session switch cancels the clear. | S |
| C36 | Code copy keeps line endings, blank lines, indentation; soft wraps add no newlines; formatter output copies the original command. | U |
| C37 | Interaction placement transcript/pinned, offscreen focus retain/suspend; Ctrl-I restores into view. | S |
| C38 | Incomplete streaming Markdown renderer-local; accepted source visible; unsafe destinations redacted; converges to final. | U |
| C39 | Inline shell expansion opens up to 128 rows, local horizontal scroll, input never reaches the PTY. | S |

### D. Performance and boundedness

| ID | Criterion | Verify |
|---|---|---|
| D1 | Resident target 512 / max 1024 events; per-request caps 16 items, 512 events scanned, 512 KiB; Page Up/Down step (see 11-P2). | U |
| D2 | One accepted Markdown projection per resident item; worker 1 + 1 pending; exact-match install. | U |
| D3 | Ordinary revisions dirty only that item; membership changes structural; inconsistent indexes full reset. | U |
| D4 | Normal scrolling does no parse/highlight/layout work. | S (`LayoutWorkCounters`, 9.2), B |
| D5 | `max_fps` semantics; cadence never delays semantics. | U |
| D6 | Regional damage per F5. | S (BMUX 15b damage observation) |
| D7 | Spinner/elapsed advance on accepted invalidations. | S |
| D8 | Latency p99 ≤ 100 ms; RSS ≤ 33,554,432 bytes. | M |
| D9 | Plugin visual adapter bounds. | U |
| D10 | Inline interaction caps. | U |
| D11 | Presentation memo 256; streaming re-resolves only the streaming unit; grouping incremental. | U |
| D12 | Manual disclosure overrides bounded, oldest-first, reconciled on residency. | U |
| D13 | Smooth streaming lives in `SessionView`; TUI schedules deadlines only. | U |

### E. Plugin and renderer neutrality

| ID | Criterion | Verify |
|---|---|---|
| E1 | Adapter resolution order: user preferred, manifest priority, producer default, deterministic id; disabled removed; failure advances; canonical fallback last. | U |
| E2 | `bcode.tui-visual-adapter` ABI carries no BMUX types; TUI keeps layout/chrome/viewport/hits/paint. | G |
| E3 | Content keys namespaced per producer, cannot authorize; v3 keys, v4 source ranges, older fall back to item level. | U |
| E4 | One primary identity per invocation; updates replace; completion absorbing; retention policies. | U |
| E5 | Unknown schemas get compact bounded fallbacks, never raw JSON. | U |
| E6 | Plugin context v3 carries resolved disclosure/sections/properties; plugins don't evaluate rules. | U |
| E7 | `SessionViewPatch` only at matching base revision, else bounded reset. | U |
| E8 | Usage, runtime work and notices are not transcript items. | U |
| E9 | Presentation commit is terminal-local; semantic contracts carry no frame/damage concepts. | G |

### X. Decisions and session behaviors

| ID | Criterion | Verify |
|---|---|---|
| X1 | Clicking a disclosure header or section label toggles it (press+release without drag); dragging selects. | S, B (BMUX 14) |
| X2 | `alt+u` focus mode with `j`/`k`/`h`/`l`/`e`/`d`/`i`/`m`/Escape and remapping in `transcript_focus` (FO7). | S |
| X3 | Structure v2: groups, sections, hidden markers, visibility floor, disclosure targets render and anchor correctly (U2, U5). | S |
| X4 | Themes, profiles and chrome (LineFrame borders, speaker, surfaces, separators, motion, density, icons) apply through `ItemRenderContext`. | S, M |
| X5 | Home/End per FO4 (composer: input start/end; transcript: true session start/latest loaded directly). | S |
| X6 | Optional scrollbar, off by default, chrome-themeable, positioned within the whole session (W12). | S |
| X7 | Engine selection by env, CLI and config with EN1 precedence and EN2 errors. | U |
| X8 | The engine is fixed at startup; no runtime switch path exists (EN3). | U |
| X9 | Composer/transcript focus transitions per FO2/FO3/FO10. | S |
| X10 | V2 declares no thread-locals or mutable statics; no pointer/wall-clock values in cache keys (L2, L3). | G |
| X11 | `JumpToLatest` from a detached window returns to live with one request (W10). | S |
| X12 | Prefetch keeps reading continuous: steady wheel scrolling over 5,000 events of history never shows a stall frame at a loaded boundary when page latency ≤ the time to scroll `prefetch_rows`. | S |

### Manual smoke checklist (phase 4 and phase 7 gates)

Run once per engine with a real daemon and a session of at least 5,000 events:

1. Attach: the latest bottom is shown and live streaming follows.
2. Wheel and Page Up into history across several pages: no stall, leap or blank frame.
3. Timeline jump deep into history, scroll down through newer pages, then `End` in transcript focus and `ctrl+end` from the composer: live content and live events apply.
4. `Home` in transcript focus: session start loads directly.
5. Submit while following, while reading and while detached: the message is revealed only on acceptance; the answer is held at its top.
6. Resize narrower and back while reading: the reading position is kept.
7. Theme and profile switch, reduce-motion toggle, `alt+u` focus mode walk, disclosure clicks, drag selection and copy.
8. Start once with `--tui-engine v1` and once with `BCODE_TUI_ENGINE=v2`: each engine attaches to the same session.

### BUG. Regression scenarios

| ID | Scenario | Verify |
|---|---|---|
| BUG-A1 | Older page arrives; a wheel tick is processed before the next frame; the committed frame shows the same anchor unit/row shifted only by the tick. | S |
| BUG-A2 | Continuous upward wheel input across many pages (including zero-row collapsed pages) moves monotonically with no stall-then-leap. | S |
| BUG-A3 | Eviction after prepend removes only units outside the protected band, by key (guard: not reproduced on V1). | S, U |
| BUG-A4 | Reaching session start does not shift content (boundary height constant). | S |
| BUG-B1 | Timeline jump then scrolling down through newer pages: no eviction on the first page; later eviction keeps the anchor. | S |
| BUG-B2 | As BUG-A1 but with eviction of hundreds of items above in the same update. | S |
| BUG-B3 | After a deep jump, `End` (transcript focus) / jump-to-latest shows live content with exactly one tail-window request and live events apply. | S |
| BUG-B4 | Resident jump while detached lands on the target in the next committed frame and stays there (guard: not reproduced on V1). | S |
| BUG-B5 | Timeline window edges are whole turns; later pages never change an edge unit's height in place. | S |

---

## 11. Ambiguity resolutions

| # | Ambiguity | Resolution | Status |
|---|---|---|---|
| 1 | Markdown projection cache: one per item vs current + up to 8 variants. | One accepted projection per resident item is the sole authority for rows, anchors, focus, hits and images (L7). A separate global bounded cache of up to 8 recent width variants may exist only to make resize-back cheap; a variant is used only on exact id/revision/options/width match and then becomes the accepted projection. | Confirmed by user |
| 2 | Latest bar vs hidden continuation of the visible entry. | Follow the tested behavior: hidden continuation of the visible entry does not activate the bar; only a distinct newer entry fully below or hidden activity does (C18). `tui-rendering.md` is corrected in phase 7. | Confirmed by user |
| 3 | Scope of the 450 ms grace window. | Grace follows every manual movement intent and suppresses automatic movement only (assistant segment reveal T3, overflow handoff T4). It does not stop content filling tail space (T11), explicit intents, or the reveal of a newly accepted submission (N5). | Confirmed by user |
| 4 | Theme crate split (renderer-neutral `bcode_theme` described vs `bcode_tui_theme` with BMUX color types in reality). | Keep `bcode_tui_theme` as the TUI theme crate (it may depend on `bmux_tui`); renderer-neutral structure lives in `bcode_presentation_policy_models`. No new theme crate. Documentation is corrected to match. | Confirmed by user |
| 5 | Navigation hidden in activity code. | Automatic navigation is decided only in `navigation::auto` and enters the reducer as intents (N9); activity code computes latest-bar activity only. | Confirmed by user |
| 6 | Latest bar measured on a cloned viewport. | Visibility is a pure function evaluated once per frame against the full transcript-region height (F6); no cloned viewport or second sync. | Confirmed by user |
| 7 | Home/End vs resident window. | Composer focused → text input start/end; transcript focused → true session start/latest, loaded directly (FO4, W10). | **Resolved by user** |
| 8 | Tail-space size hard-coded `viewport_height − 1`. | Keep `viewport_height − 1` (at least one content row stays visible), expressed as the BMUX overscroll policy `ViewportMinus(1)` and documented (VP2). | Confirmed by user |
| 9 | TailSpace exhaustion → following re-enables automatic reveal. | Yes: when content fills the virtual space the reader is at latest, so the state becomes `Following` and automatic reveal applies (T11). | Confirmed by user |
| 10 | Selection fallback: rendered-text fallback vs reject stale mappings. | Source-backed content rejects stale or unavailable mappings (copy fails, selection kept). Rendered-text fallback applies only to fragments explicitly declared without source mapping (transformed visuals: diagrams, images, plugin rows), marked as rendered fragments in the selection scene. | Confirmed by user |

Additional proposals raised by this specification:

| # | Topic | Proposal | Status |
|---|---|---|---|
| 11-P1 | Page target size | Replace the V1 target of 4 items / 24 rows with "fill to the per-request caps and continue until `prefetch_rows` is satisfied" (W5, W6). Caps are unchanged. | Confirmed by user |
| 11-P2 | Page Up/Down step | Keep 10 rows (D1) for parity. | Confirmed by user |
| 11-P3 | Focus bindings | `alt+u` is the single key that moves keyboard focus into the transcript (unit focus); Escape, `alt+u` again, or an unbound key returns to the composer (FO2, FO3). No `alt+t`. `ctrl+home`/`ctrl+end` keep their keys and mean direct session-start/latest loads in V2 (FO5). | Resolved by user |
| 11-P4 | Detached live handling | Live transcript events are not applied while detached. Return to live uses the bounded tail window (W9, W10), or the buffered catch-up when paging down reaches the tail (W11). Submitting while detached loads the tail window on acceptance (T27). | Confirmed by user |
| 11-P5 | Runtime engine switching | Dropped (user decision): the engine is chosen at startup only (EN3); each engine owns its `SessionView` (EN4). | Resolved by user |

---

## 12. Phases

| Phase | Deliverables | Exit criteria |
|---|---|---|
| 0 | This specification; development loop (`scripts/bmux-dev.sh`, which temporarily rewrites the root manifest's `bmux_*` patch lines, and the BMUX patch/Cargo.lock guard; section 13); BMUX 15 test harness; engine-neutral scenario suite with a V1 driver covering section 10 `S` items that V1 can express. `BUG-*` scenarios that reproduce on V1 are recorded as expected V1 failures; BUG-A3 and BUG-B4 did not reproduce and stay active as guards. | Spec merged; `bmux-dev.sh on`/`off` round-trips leaving `Cargo.toml` and `Cargo.lock` byte-identical to `HEAD`; the guard fails on a non-git `bmux_*` patch or a path/local BMUX lock source; harness tests pass in BMUX; scenario suite runs in `cargo test -p bcode_tui`. |
| 1 | BMUX 1–7 with BMUX tests; Bcode enables `virtual-list`. | BMUX tests pass; generic API review (no product names); Bcode builds against the new BMUX through the dev loop; V1 unchanged. |
| 2 | `bcode_tui_transcript` crate: `window`, `units` (minimal producers), `viewport`, `navigation` (intents, reducer, state machine, pending movement, motion); V2 scenario driver. Not wired into the host. | Navigation/window `S` and `U` items that need no phase-4/5 deliverable pass headlessly through the V2 driver with fixture producers, including BUG-A1–A4 and BUG-B4; clippy/tests clean. BUG-B1–B3 and BUG-B5 need aligned timeline windows, direct `Head`/`Tail` windows and fast return to live, so they are phase-5 exit criteria. |
| 3 | Identity-keyed item layout, `ItemRenderContext`, full producers (structure v2, themes, chrome, plugin visuals, Markdown, tools, diff, shell); BMUX 8–10. | B1–B12, C23–C25, C38, X3, X4, X10 pass; no thread-locals; `LayoutWorkCounters` (9.2) show no re-measure on scroll and only new units measured on prepend. |
| 4 | V2 wired: engine seam, `engine_selection`, `transcript_v2_host` effects, focus model, latest bar, selection, CLI flag, env var, config key. | X5–X9 pass; both engines selectable side by side; V1 test suite unchanged and passing with `v1`; the section 10 manual smoke checklist passes on both engines; host-owned scenarios are enabled for V2. |
| 5 | Incremental `SessionView` window operations, prefetch, aligned timeline windows, direct head/tail windows, fast return to live, scrollbar estimates. | SD1–SD7, W1–W13 (with W7a), X11, X12, BUG-B1–B5 pass (BUG-B3 through both `End` in transcript focus and jump-to-latest); no full rebuild on paging (counter). |
| 6 | Frame cleanup: single snapshot, pure paint, regional damage, split host-side god objects on the V2 path; BMUX 11–14 and 15b. | F1–F7, D4, D6, D7, X1, C34, C35 pass; latency/RSS gate (D8) passes manually. |
| 7 | V2 default; delete V1 transcript code and the seam; update `tui-rendering.md`, `renderer-architecture.md`, `tui-keybindings.md`, `INVARIANTS.md` (with approval) and remove resolved ambiguities. | Every section 10 ID checked; workspace validation per AGENTS.md passes; no V1 transcript modules remain. |

---

## 13. Development loop

The loop is implemented by `scripts/bmux-dev.sh` and `xtask/src/bmux_lock.rs`; the user guide is
[`bmux-development.md`](bmux-development.md).

| Item | Contract |
|---|---|
| Normal dependency | Root `Cargo.toml` `[patch.crates-io]` points every `bmux_*` crate at `https://github.com/BSteffaniak/bmux` `branch = "master"`; `Cargo.lock` pins the exact revision. `.cargo/config.toml` holds only the `xtask` alias and has no BMUX include. |
| Local override | `scripts/bmux-dev.sh on [<bmux-checkout>]` (default: `$BMUX_DEV_PATH`, else `~/GitHub/wt-bmux/robust-themes`) rewrites only the `bmux_*` lines of the root `Cargo.toml` `[patch.crates-io]` table to absolute `path` patches, each marked with a trailing `# bmux-dev:` comment. Each crate is mapped to its BMUX package directory by reading every BMUX manifest's `[package] name`; a missing or duplicated name is an error. |
| Why not a config patch | A Cargo configuration `[patch]` does take precedence over manifest patches, but Cargo discovers configuration from the working directory of every invocation, including nested builds below `target/`. `bmux_mermaid_render`'s build script runs a `--locked` guest build from its `OUT_DIR`, inherits the patches as unused, needs a lockfile update and fails (verified). Manifest patches apply only to the Bcode workspace. Moving back to an untracked config file would need BMUX's nested build to run from a directory outside the consumer's tree. |
| State | Git-ignored files `.cargo/bmux-dev.manifest-lines` (original patch lines), `.cargo/bmux-dev.state` (whether the lock matched `HEAD` before `on`, a fingerprint of uncommitted manifest edits, the BMUX path, and a fingerprint of the re-resolved lock without its `bmux_*` entries) and `.cargo/bmux-dev.Cargo.lock.orig` (the pre-`on` lock). |
| `on` | Records the state on first use, rewrites the patch lines, re-resolves `Cargo.lock` with `cargo metadata`, and verifies that every `bmux_*` package now resolves from the local path. Running it again regenerates the lines (also for a new path). |
| `off` | Restores only the marked lines (recorded lines first, `HEAD`'s otherwise), leaving other manifest edits alone, then restores `Cargo.lock` from `HEAD`, or from the pre-`on` copy when the lock already had uncommitted changes. It refuses to restore the lock when manifests changed while the override was on, when lock entries other than `bmux_*` packages changed while it was on (for example `cargo update -p <crate>`, or a dependency the local BMUX checkout added), when the lock has staged changes, or when no record exists and the lock differs from `HEAD` beyond BMUX sources. `off --force` restores it from `HEAD` anyway and keeps the discarded lock at `.cargo/bmux-dev.Cargo.lock.discarded`. |
| `status` | Prints whether the override is active, the BMUX path and its HEAD (with an uncommitted-changes marker), whether `Cargo.lock` is patched, and whether it matches `HEAD`. |
| `update [--force]` | After BMUX is pushed: runs `off`, then `cargo update -p bmux_tui`, reports the newly locked BMUX revision, and warns when `bmux_*` packages lock different revisions or the local BMUX `HEAD` is not the locked revision. `cargo update` may re-resolve unrelated entries; review the lock diff. |
| Guard | `cargo xtask bmux-lock-check` fails when a workspace `bmux_*` patch is not a `git` patch, or when `Cargo.lock` contains a `bmux_*` package whose source is not the git source that patch declares (path-patched, registry, unpinned, or another repository or reference). `cargo run -p xtask -- architecture` runs it first and reports its failures even when `--locked` metadata resolution then fails, so CI rejects a commit made while the override is on. |
| Target directories | Each checkout uses its own target directory. Never point `CARGO_TARGET_DIR` of one checkout at another checkout's target directory. |
| Release flow | 1. Land BMUX changes on BMUX `master` (user pushes). 2. `scripts/bmux-dev.sh update`. 3. Run `cargo xtask bmux-lock-check`. 4. Validate per AGENTS.md. 5. User commits Bcode. |
