//! Incremental resident-window operations.
//!
//! A renderer that keeps a bounded *resident window* of canonical history changes it with the
//! operations here instead of rebuilding the whole projection:
//!
//! * [`SessionView::prepend_history_page`] reduces a page of complete older turns in an
//!   isolated, bounded sub-projection and splices its items before the window. No resident
//!   event is replayed (SD2).
//! * [`SessionView::append_history_page`] applies a page of newer events after the window. The
//!   reducer is sequential, so applying newer events on top of the resident state is exactly a
//!   rebuild of the combined range; resident events are not replayed.
//! * [`SessionView::evict_before`] / [`SessionView::evict_after`] (and their sequence-based
//!   forms) remove whole turns and the aggregates owned only by the removed items (SD3).
//! * [`SessionView::replace_history_window`] replaces the window (jump, head, tail); it is the
//!   only operation that reduces every event of the new window.
//! * [`SessionView::apply_window_event`] applies one durable event at the window tail (live
//!   delivery), and [`SessionView::drain_transcript_changes`] reports the item-level changes
//!   accumulated by live and durable application since the last operation or drain.
//!
//! Every operation reports [`TranscriptChanges`]: an in-process, id-only notification (SD7).
//! Changes accumulated before an operation and not yet drained are reported first, in the same
//! change set, so a consumer never misses an intermediate change. Accumulation is an id/revision
//! ledger of the document (no item payloads), so draining costs one pass over the resident items.
//! [`SessionView::transcript_patch`] derives the portable [`SessionViewPatch`] from a change set.
//!
//! Page and window ranges are disjoint from the resident range; an overlapping page is rejected
//! with [`WindowOpError::Overlap`] and never re-applied (SD1). A prepend that cannot be proven
//! isolated from the resident window (an unaligned resident start, external-harness events, or
//! keyed state shared across the boundary) is rejected with [`WindowOpError::UnalignedBoundary`]
//! before any mutation; the caller then falls back to [`SessionView::replace_history_window`],
//! and because item ids are deterministic, identity anchors survive the fallback (P1).
//!
//! Non-transcript hydrated state (permissions, runtime work, usage, composer, runtime and model
//! selection) is authoritative from the daemon and from the newer resident events. A prepended
//! page never changes it, except that it supplies the title and working directory when they are
//! still unknown and adds pending permissions and interactions the window does not know yet.

use std::collections::{BTreeMap, BTreeSet};

use bcode_session_models::{SessionEvent, SessionEventKind};
use bcode_session_view_models::{
    SessionViewPatch, TranscriptChange, TranscriptChanges, TranscriptViewDocument,
    TranscriptViewItem, TranscriptViewItemId, TranscriptViewItemKind, TranscriptViewPatchOp,
    ViewRevision,
};

use crate::{SessionView, rich_tool_owns_item, rich_tool_owns_key};

/// Events one [`SessionView::prepend_history_page`] or [`SessionView::append_history_page`]
/// accepts (the per-request scan cap, D1).
pub const MAX_PAGE_EVENTS: usize = 512;

/// Whether a resident window may begin at this event: the start of a turn (a user message).
///
/// Window edges are safe window starts (W4), so a page of complete older turns ends right before
/// one.
#[must_use]
pub const fn is_safe_window_start(event: &SessionEvent) -> bool {
    matches!(event.kind, SessionEventKind::UserMessage { .. })
}

/// Why a window operation was rejected. A rejected operation leaves the view unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WindowOpError {
    /// The page cannot be spliced in isolation: the resident window does not start (prepend) or
    /// end (append after an unaligned tail eviction) at a turn boundary, the page carries
    /// external-harness events, the page leaves streaming state open, or the page and the window
    /// share keyed state. Fall back to [`SessionView::replace_history_window`].
    UnalignedBoundary,
    /// The page overlaps the resident range. It is never re-applied (SD1).
    Overlap,
    /// The page belongs to another session.
    SessionMismatch,
    /// The page exceeds [`MAX_PAGE_EVENTS`], is not strictly ascending by sequence, or holds an
    /// event without a canonical sequence.
    Bounds,
}

impl std::fmt::Display for WindowOpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::UnalignedBoundary => "page cannot be spliced at an unaligned window boundary",
            Self::Overlap => "page overlaps the resident window",
            Self::SessionMismatch => "page belongs to another session",
            Self::Bounds => "page exceeds the page bounds or is out of order",
        })
    }
}

impl std::error::Error for WindowOpError {}

/// Work performed by window operations since the view was created.
///
/// `events_reduced` counts every durable event the window operations handed to the reducer, so
/// a caller can verify that paging reduces only the page (no resident replay) and that only
/// [`SessionView::replace_history_window`] reduces a whole window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct WindowOpCounters {
    /// Durable events reduced by window operations and [`SessionView::apply_window_event`].
    pub events_reduced: u64,
    /// Whole-window replacements ([`SessionView::replace_history_window`] and its detached form).
    pub replacements: u64,
    /// Successful [`SessionView::prepend_history_page`] calls.
    pub prepends: u64,
    /// Successful [`SessionView::append_history_page`] calls.
    pub appends: u64,
    /// Evictions that removed at least one item.
    pub evictions: u64,
    /// Drains ([`SessionView::drain_transcript_changes`] and the drain each operation starts
    /// with) that rescanned the resident items because their membership or order changed. A
    /// drain with no change is O(1), and one that only revised items (streaming deltas) updates
    /// the ledger in place without rebuilding it.
    pub ledger_scans: u64,
}

/// Resident-window bookkeeping kept by [`SessionView`] for the window operations.
#[derive(Debug, Clone, Default)]
pub(crate) struct WindowTracking {
    /// Item ids and revisions as of the last operation or drain, in document order.
    mirror: Vec<(TranscriptViewItemId, ViewRevision)>,
    /// Transcript revision as of the last operation or drain.
    mirror_revision: ViewRevision,
    /// First resident canonical sequence, once a window operation established it.
    first: Option<u64>,
    /// The resident tail was cut inside a turn (unaligned tail eviction).
    tail_unaligned: bool,
    /// Resident safe window starts (turn starts).
    turn_starts: BTreeSet<u64>,
    /// Canonical sequence at which each resident durable item was first observed (creation).
    origins: BTreeMap<TranscriptViewItemId, u64>,
    counters: WindowOpCounters,
}

impl SessionView {
    /// Work performed by window operations since the view was created.
    #[must_use]
    pub const fn window_op_counters(&self) -> WindowOpCounters {
        self.window.counters
    }

    /// First resident canonical sequence established by the window operations.
    #[must_use]
    pub const fn window_first_sequence(&self) -> Option<u64> {
        self.window.first
    }

    /// Apply one durable event at the window tail (live delivery or a buffered event), recording
    /// it in the resident window. Its item changes accumulate for
    /// [`Self::drain_transcript_changes`]. An event at or below the newest applied sequence is
    /// ignored (SD1).
    pub fn apply_window_event(&mut self, event: &SessionEvent) {
        let applies = event.sequence != 0
            && self
                .snapshot
                .latest_sequence
                .is_none_or(|latest| event.sequence > latest);
        if !applies {
            return;
        }
        self.window.counters.events_reduced = self.window.counters.events_reduced.saturating_add(1);
        self.apply_event(event);
        if self.window.first.is_none() {
            self.window.first = Some(event.sequence);
        }
        if is_safe_window_start(event) {
            self.window.turn_starts.insert(event.sequence);
        }
        self.observe_origins(false);
    }

    /// Reduce a page of complete older turns and splice its items before the window (SD2).
    ///
    /// The page is reduced in an isolated, bounded sub-projection; no resident event is
    /// replayed. The change set reports the inserted items (after any undrained changes).
    ///
    /// # Errors
    ///
    /// * [`WindowOpError::Bounds`] if the page exceeds [`MAX_PAGE_EVENTS`], is not strictly
    ///   ascending, or holds an event with sequence 0.
    /// * [`WindowOpError::SessionMismatch`] if an event belongs to another session.
    /// * [`WindowOpError::Overlap`] if the page's last event does not precede the window.
    /// * [`WindowOpError::UnalignedBoundary`] if the window does not start at a turn start, its
    ///   start is unknown (the view was not built by window operations), the page carries
    ///   external-harness events, the page leaves streaming state open, or the page shares keyed
    ///   state (item ids, tool invocations, contributions, streams, interactions) with the window.
    pub fn prepend_history_page(
        &mut self,
        events: &[SessionEvent],
    ) -> Result<TranscriptChanges, WindowOpError> {
        self.validate_page(events)?;
        let (Some(page_first), Some(page_last)) = (events.first(), events.last()) else {
            return Ok(self.drain_transcript_changes());
        };
        match (self.window.first, self.snapshot.latest_sequence) {
            (Some(first), _) => {
                if page_last.sequence >= first {
                    return Err(WindowOpError::Overlap);
                }
                if !self.window.turn_starts.contains(&first) {
                    return Err(WindowOpError::UnalignedBoundary);
                }
            }
            // Durable state the window operations did not establish: its start is unknown.
            (None, Some(_)) => return Err(WindowOpError::UnalignedBoundary),
            (None, None) => {}
        }
        if events
            .iter()
            .any(|event| crate::external::is_external_event(&event.kind))
        {
            return Err(WindowOpError::UnalignedBoundary);
        }

        let mut sub = self.window_sub_projection();
        for event in events {
            sub.apply_window_event(event);
        }
        sub.observe_origins(true);
        self.window.counters.events_reduced = self
            .window
            .counters
            .events_reduced
            .saturating_add(sub.window.counters.events_reduced);
        if !self.isolated_from(&sub) {
            return Err(WindowOpError::UnalignedBoundary);
        }

        let mut changes = self.drain_transcript_changes();
        let resident_empty = self.window.first.is_none();
        let ids = self.splice_before(sub);
        self.window.first = Some(page_first.sequence);
        if resident_empty {
            self.window.tail_unaligned = false;
            self.snapshot.latest_sequence = Some(page_last.sequence);
        }
        self.window.counters.prepends = self.window.counters.prepends.saturating_add(1);
        if !ids.is_empty() {
            self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
            self.bump_revision();
            changes
                .changes
                .push(TranscriptChange::Inserted { after: None, ids });
        }
        self.sync_mirror();
        changes.revision = self.snapshot.transcript.revision;
        Ok(changes)
    }

    /// Reduce a page of newer events after the window (detached windows only).
    ///
    /// The reducer is sequential, so this is exactly a rebuild of the combined range without
    /// replaying resident events. The change set reports every item the page inserted, removed
    /// or revised (after any undrained changes).
    ///
    /// # Errors
    ///
    /// * [`WindowOpError::Bounds`] if the page exceeds [`MAX_PAGE_EVENTS`], is not strictly
    ///   ascending, or holds an event with sequence 0.
    /// * [`WindowOpError::SessionMismatch`] if an event belongs to another session.
    /// * [`WindowOpError::Overlap`] if the page's first event does not follow the window.
    /// * [`WindowOpError::UnalignedBoundary`] if the window tail was cut inside a turn by an
    ///   unaligned eviction, or its range is unknown (the view was not built by window
    ///   operations).
    pub fn append_history_page(
        &mut self,
        events: &[SessionEvent],
    ) -> Result<TranscriptChanges, WindowOpError> {
        self.validate_page(events)?;
        let Some(page_first) = events.first() else {
            return Ok(self.drain_transcript_changes());
        };
        if self
            .snapshot
            .latest_sequence
            .is_some_and(|latest| page_first.sequence <= latest)
        {
            return Err(WindowOpError::Overlap);
        }
        if self.window.tail_unaligned
            || (self.window.first.is_none() && self.snapshot.latest_sequence.is_some())
        {
            return Err(WindowOpError::UnalignedBoundary);
        }
        for event in events {
            self.apply_window_event(event);
        }
        self.window.counters.appends = self.window.counters.appends.saturating_add(1);
        Ok(self.drain_transcript_changes())
    }

    /// Remove whole turns whose items precede `first_kept` (exclusive) from the projection.
    ///
    /// The cut is the start of the turn `first_kept` was created in; see
    /// [`Self::evict_before_sequence`]. An unknown item or one without a resident turn start
    /// before it evicts nothing.
    pub fn evict_before(&mut self, first_kept: &TranscriptViewItemId) -> TranscriptChanges {
        let cut = self.item_origin(first_kept).and_then(|origin| {
            self.window
                .turn_starts
                .range(..=origin)
                .next_back()
                .copied()
        });
        match cut {
            Some(cut) => self.evict_before_sequence(cut),
            None => self.drain_transcript_changes(),
        }
    }

    /// Remove whole turns whose items follow `last_kept` (exclusive) from the projection.
    ///
    /// The cut is the first turn start after the turn `last_kept` was created in; see
    /// [`Self::evict_from_sequence`]. An unknown item or one without a later resident turn
    /// evicts nothing.
    pub fn evict_after(&mut self, last_kept: &TranscriptViewItemId) -> TranscriptChanges {
        let cut = self.item_origin(last_kept).and_then(|origin| {
            self.window
                .turn_starts
                .range(origin.saturating_add(1)..)
                .next()
                .copied()
        });
        match cut {
            Some(cut) => self.evict_from_sequence(cut),
            None => self.drain_transcript_changes(),
        }
    }

    /// Head-side eviction: remove every durable item last touched before `first_kept`, and the
    /// aggregates owned only by those items (SD3). The resident window then starts at
    /// `first_kept`; a cut that is not a turn start makes the next prepend report
    /// [`WindowOpError::UnalignedBoundary`]. Live-only (no sequence) and streaming items are
    /// never removed.
    pub fn evict_before_sequence(&mut self, first_kept: u64) -> TranscriptChanges {
        let mut changes = self.drain_transcript_changes();
        let removed = self
            .snapshot
            .transcript
            .items
            .iter()
            .filter(|item| !item.streaming && item.sequence.is_some_and(|seq| seq < first_kept))
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();
        if self.window.first.is_some() || self.snapshot.latest_sequence.is_some() {
            self.window.first = Some(
                self.window
                    .first
                    .map_or(first_kept, |first| first.max(first_kept)),
            );
        }
        self.window.turn_starts.retain(|start| *start >= first_kept);
        self.finish_eviction(&mut changes, removed, false);
        changes
    }

    /// Tail-side eviction (W9): remove every item created at or after `first_evicted`, every
    /// live-only item, and the aggregates and live state owned only by them (SD3). Newer events
    /// then apply again from `first_evicted` on; a cut that is not a turn start makes the next
    /// append report [`WindowOpError::UnalignedBoundary`].
    pub fn evict_from_sequence(&mut self, first_evicted: u64) -> TranscriptChanges {
        let mut changes = self.drain_transcript_changes();
        let removed = self
            .snapshot
            .transcript
            .items
            .iter()
            .filter(|item| {
                item.sequence.is_none()
                    || self
                        .window
                        .origins
                        .get(&item.id)
                        .copied()
                        .or(item.sequence)
                        .is_some_and(|origin| origin >= first_evicted)
            })
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();
        if self
            .window
            .first
            .is_some_and(|first| first >= first_evicted)
        {
            // Every resident durable event is evicted: the window is empty.
            self.window.first = None;
            self.window.tail_unaligned = false;
            self.snapshot.latest_sequence = None;
        } else if self
            .snapshot
            .latest_sequence
            .is_some_and(|latest| latest >= first_evicted)
        {
            self.snapshot.latest_sequence = Some(first_evicted.saturating_sub(1));
            self.window.tail_unaligned = !self.window.turn_starts.contains(&first_evicted);
        }
        self.window
            .turn_starts
            .retain(|start| *start < first_evicted);
        self.finish_eviction(&mut changes, removed, true);
        changes
    }

    /// Replace the window (jump, head, tail); equivalent to [`Self::rebuild_history_window`].
    ///
    /// Live-only items survive unless a durable event in the new window supersedes them. The
    /// change set reports item-level differences against the previous window, comparing
    /// content, so an item rebuilt with the same revision but different content is `Revised`.
    pub fn replace_history_window(&mut self, events: &[SessionEvent]) -> TranscriptChanges {
        self.replace_window(events, true)
    }

    /// Replace the window with a detached one: like [`Self::replace_history_window`], but
    /// live-only items are discarded because a detached window does not contain the tail (W2).
    pub fn replace_history_window_detached(
        &mut self,
        events: &[SessionEvent],
    ) -> TranscriptChanges {
        self.replace_window(events, false)
    }

    /// Drain item-level changes accumulated by live/durable event application since the last
    /// window operation or drain.
    ///
    /// Cost: O(1) when the transcript revision is unchanged; one comparison pass without
    /// allocation when only item revisions changed (streaming deltas); a full keyed diff only
    /// when items were inserted, removed or moved (counted as
    /// [`WindowOpCounters::ledger_scans`]).
    pub fn drain_transcript_changes(&mut self) -> TranscriptChanges {
        let base_revision = self.window.mirror_revision;
        let revision = self.snapshot.transcript.revision;
        if revision == base_revision
            && self.window.mirror.len() == self.snapshot.transcript.items.len()
        {
            return TranscriptChanges::empty(base_revision, revision);
        }
        if let Some(changes) = self.drain_revisions_in_place() {
            return TranscriptChanges {
                base_revision,
                revision,
                changes,
            };
        }
        self.window.counters.ledger_scans = self.window.counters.ledger_scans.saturating_add(1);
        self.observe_origins(true);
        let changes = diff_items(&self.window.mirror, &self.snapshot.transcript.items, None);
        self.sync_mirror();
        TranscriptChanges {
            base_revision,
            revision,
            changes,
        }
    }

    /// The drain when the resident items are the ledger's items in the same order: report and
    /// record revised items in place. `None` when membership or order changed.
    fn drain_revisions_in_place(&mut self) -> Option<Vec<TranscriptChange>> {
        let items = &self.snapshot.transcript.items;
        let mirror = &mut self.window.mirror;
        if mirror.len() != items.len()
            || mirror
                .iter()
                .zip(items)
                .any(|((id, _), item)| *id != item.id)
        {
            return None;
        }
        let origins = &mut self.window.origins;
        let mut changes = Vec::new();
        for ((_, revision), item) in mirror.iter_mut().zip(items) {
            if *revision == item.revision {
                continue;
            }
            *revision = item.revision;
            changes.push(TranscriptChange::Revised {
                id: item.id.clone(),
                revision: item.revision,
            });
            // A live-only item that became durable at this revision is first observed now.
            if let Some(sequence) = item.sequence {
                origins.entry(item.id.clone()).or_insert(sequence);
            }
        }
        self.window.mirror_revision = self.snapshot.transcript.revision;
        Some(changes)
    }

    /// Derive the portable transcript patch for `changes` (SD7).
    ///
    /// `previous` is the transcript document the change set's base revision describes. `Revised`
    /// becomes `Replace`, `Removed` becomes `Remove`, insertion at the tail becomes `Append`, and
    /// any other insertion, a `Reset`, or a change set that does not describe the step from
    /// `previous` to the current document becomes a bounded `Reset` of the transcript document.
    /// The patch carries transcript operations only and is applied with
    /// [`TranscriptViewDocument::apply_patch`].
    #[must_use]
    pub fn transcript_patch(
        &self,
        previous: &TranscriptViewDocument,
        changes: &TranscriptChanges,
    ) -> SessionViewPatch {
        let current = &self.snapshot.transcript;
        let mut patch = SessionViewPatch::empty(previous.revision, current.revision);
        patch.session_id = self.snapshot.session_id;
        patch.transcript = incremental_patch_ops(previous, current, changes).unwrap_or_else(|| {
            vec![TranscriptViewPatchOp::Reset {
                document: current.clone(),
            }]
        });
        patch
    }

    // ---- internals ----

    fn validate_page(&self, events: &[SessionEvent]) -> Result<(), WindowOpError> {
        if events.len() > MAX_PAGE_EVENTS
            || events.iter().any(|event| event.sequence == 0)
            || events
                .windows(2)
                .any(|pair| pair[0].sequence >= pair[1].sequence)
        {
            return Err(WindowOpError::Bounds);
        }
        if let Some(session) = self.snapshot.session_id
            && events.iter().any(|event| event.session_id != session)
        {
            return Err(WindowOpError::SessionMismatch);
        }
        if let Some(first) = events.first()
            && events
                .iter()
                .any(|event| event.session_id != first.session_id)
        {
            return Err(WindowOpError::SessionMismatch);
        }
        Ok(())
    }

    /// An empty projection carrying the settings that shape how events are projected.
    ///
    /// No dedupe fence of the window is seeded: the page is reduced exactly like a from-scratch
    /// rebuild of its range, and an exchange the window also knows is detected as shared keyed
    /// state by [`Self::isolated_from`]. Runtime-work fences only guard non-transcript state,
    /// which a page never changes. Copying no fence also keeps each prepend's cost bounded by
    /// the page rather than by the history paged through.
    fn window_sub_projection(&self) -> Self {
        let mut sub = Self::new();
        sub.streaming_presentation_policy = self.streaming_presentation_policy;
        sub.usage_checkpoint = self.usage_checkpoint;
        sub.snapshot.session_id = self.snapshot.session_id;
        sub.snapshot.thinking.clone_from(&self.snapshot.thinking);
        sub.snapshot.runtime.clone_from(&self.snapshot.runtime);
        sub.snapshot
            .permissions
            .clone_from(&self.snapshot.permissions);
        sub
    }

    /// Whether a sub-projection shares no keyed state with this view and left nothing open.
    fn isolated_from(&self, sub: &Self) -> bool {
        fn disjoint<K: Ord, A, B>(sub: &BTreeMap<K, A>, resident: &BTreeMap<K, B>) -> bool {
            sub.keys().all(|key| !resident.contains_key(key))
        }
        let sub_items = &sub.snapshot.transcript.items;
        if sub_items.iter().any(|item| item.streaming)
            || !sub.live_reasoning.is_empty()
            || !sub.pending_text_presentations.is_empty()
        {
            return false;
        }
        let sub_ids = sub_items
            .iter()
            .map(|item| &item.id)
            .collect::<BTreeSet<_>>();
        if self
            .snapshot
            .transcript
            .items
            .iter()
            .any(|item| sub_ids.contains(&item.id))
        {
            return false;
        }
        let resident_interactions = self
            .snapshot
            .interactions
            .iter()
            .map(|interaction| &interaction.interaction_id)
            .collect::<BTreeSet<_>>();
        disjoint(&sub.tool_item_ids, &self.tool_item_ids)
            && disjoint(&sub.interaction_item_ids, &self.interaction_item_ids)
            && disjoint(&sub.tool_invocations, &self.tool_invocations)
            && disjoint(&sub.contribution_sequences, &self.contribution_sequences)
            && disjoint(&sub.contribution_placements, &self.contribution_placements)
            && disjoint(
                &sub.last_text_stream_updates,
                &self.last_text_stream_updates,
            )
            && disjoint(&sub.snapshot.text_streams, &self.snapshot.text_streams)
            && disjoint(&sub.snapshot.contributions, &self.snapshot.contributions)
            && disjoint(
                &sub.snapshot.active_exchanges,
                &self.snapshot.active_exchanges,
            )
            && disjoint(
                &sub.snapshot.active_invocations,
                &self.snapshot.active_invocations,
            )
            && disjoint(&sub.snapshot.tools, &self.snapshot.tools)
            && disjoint(&sub.terminal_interactions, &self.terminal_interactions)
            && sub
                .snapshot
                .interactions
                .iter()
                .all(|interaction| !resident_interactions.contains(&interaction.interaction_id))
    }

    /// Splice an isolated sub-projection before the window. Returns the inserted item ids.
    fn splice_before(&mut self, sub: Self) -> Vec<TranscriptViewItemId> {
        let Self {
            snapshot: sub_snapshot,
            tool_item_ids,
            interaction_item_ids,
            tool_invocations,
            contribution_sequences,
            contribution_placements,
            // Runtime work is the window's and the daemon's state; a page never changes it.
            terminal_runtime_work: _,
            terminal_interactions,
            live_reasoning: _,
            last_text_stream_updates,
            streaming_presentation_policy: _,
            pending_text_presentations: _,
            // Usage accounting is the session owner's; a page is not a second source (E8).
            usage_by_request: _,
            unattributed_metered_tokens: _,
            unattributed_cost: _,
            usage_cost_revision: _,
            usage_checkpoint: _,
            external: _,
            window: sub_window,
        } = sub;
        let ids = sub_snapshot
            .transcript
            .items
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();
        let mut items = sub_snapshot.transcript.items;
        items.append(&mut self.snapshot.transcript.items);
        self.snapshot.transcript.items = items;
        self.tool_item_ids.extend(tool_item_ids);
        self.interaction_item_ids.extend(interaction_item_ids);
        self.tool_invocations.extend(tool_invocations);
        self.contribution_sequences.extend(contribution_sequences);
        self.contribution_placements.extend(contribution_placements);
        // The page's own resolved exchanges (disjoint from the window's, see `isolated_from`).
        self.terminal_interactions.extend(terminal_interactions);
        self.last_text_stream_updates
            .extend(last_text_stream_updates);
        self.snapshot.text_streams.extend(sub_snapshot.text_streams);
        self.snapshot
            .contributions
            .extend(sub_snapshot.contributions);
        self.snapshot
            .active_exchanges
            .extend(sub_snapshot.active_exchanges);
        self.snapshot
            .active_invocations
            .extend(sub_snapshot.active_invocations);
        self.snapshot.tools.extend(sub_snapshot.tools);
        self.snapshot.interactions.extend(sub_snapshot.interactions);
        let known_permissions = self
            .snapshot
            .permissions
            .iter()
            .map(|permission| permission.permission_id.clone())
            .collect::<BTreeSet<_>>();
        self.snapshot.permissions.extend(
            sub_snapshot
                .permissions
                .into_iter()
                .filter(|permission| !known_permissions.contains(&permission.permission_id)),
        );
        if self.snapshot.title.is_none() {
            self.snapshot.title = sub_snapshot.title;
        }
        if self.snapshot.working_directory.is_none() {
            self.snapshot.working_directory = sub_snapshot.working_directory;
        }
        if self.snapshot.session_id.is_none() {
            self.snapshot.session_id = sub_snapshot.session_id;
        }
        self.window.turn_starts.extend(sub_window.turn_starts);
        self.window.origins.extend(sub_window.origins);
        ids
    }

    fn replace_window(&mut self, events: &[SessionEvent], keep_live: bool) -> TranscriptChanges {
        let mut changes = self.drain_transcript_changes();
        let previous = self.snapshot.transcript.items.clone();
        // A resident window is a range of canonical history: resolved exchanges are rebuilt
        // from the window's own events instead of being retained (at the window top) and
        // fenced from re-application. Pending interactions stay retained (W2).
        self.forget_resolved_interactions(None);
        // `clear_history_window` starts from a fresh view; keep the ledger across it and take
        // only the new window's range, turn starts and origins from the rebuild.
        let mut tracking = std::mem::take(&mut self.window);
        if keep_live {
            self.rebuild_history_window_with(events, Self::apply_window_event);
        } else {
            self.clear_history_window();
            self.window = WindowTracking::default();
            for event in events {
                self.apply_window_event(event);
            }
        }
        let rebuilt = std::mem::take(&mut self.window);
        tracking.first = rebuilt.first;
        tracking.turn_starts = rebuilt.turn_starts;
        tracking.origins = rebuilt.origins;
        tracking.tail_unaligned = false;
        tracking.counters.events_reduced = tracking
            .counters
            .events_reduced
            .saturating_add(rebuilt.counters.events_reduced);
        tracking.counters.replacements = tracking.counters.replacements.saturating_add(1);
        self.window = tracking;
        self.prune_unreferenced_invocation_fences();
        self.observe_origins(true);
        let previous_items = previous
            .iter()
            .map(|item| (&item.id, item))
            .collect::<BTreeMap<_, _>>();
        changes.changes.extend(diff_items(
            &self.window.mirror,
            &self.snapshot.transcript.items,
            Some(&previous_items),
        ));
        self.sync_mirror();
        changes.revision = self.snapshot.transcript.revision;
        changes
    }

    /// Remove `removed` items and the aggregates owned only by them, then record the change.
    fn finish_eviction(
        &mut self,
        changes: &mut TranscriptChanges,
        removed: Vec<TranscriptViewItemId>,
        discard_live: bool,
    ) {
        if removed.is_empty() {
            return;
        }
        let removed_set = removed.iter().cloned().collect::<BTreeSet<_>>();
        let interactions = self
            .snapshot
            .transcript
            .items
            .iter()
            .filter(|item| removed_set.contains(&item.id))
            .filter_map(|item| match &item.kind {
                TranscriptViewItemKind::Interaction { interaction } => {
                    Some(interaction.interaction_id.clone())
                }
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        if !interactions.is_empty() {
            // An evicted resolved exchange is forgotten with its row, so paging its turn back
            // in rebuilds the row like a from-scratch rebuild of the range (SD2) and the
            // interaction state stays bounded by the resident window.
            self.forget_resolved_interactions(Some(&interactions));
        }
        let owners = self
            .snapshot
            .transcript
            .items
            .iter()
            .filter(|item| removed_set.contains(&item.id))
            .filter_map(invocation_owner)
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        self.snapshot
            .transcript
            .items
            .retain(|item| !removed_set.contains(&item.id));
        self.tool_item_ids.retain(|_, id| !removed_set.contains(id));
        self.interaction_item_ids
            .retain(|_, id| !removed_set.contains(id));
        self.snapshot
            .text_streams
            .retain(|id, _| !removed_set.contains(id));
        self.last_text_stream_updates
            .retain(|id, _| !removed_set.contains(id));
        self.pending_text_presentations
            .retain(|id, _| !removed_set.contains(id));
        if discard_live {
            self.live_reasoning.retain(|(turn, activity), _| {
                !removed_set.contains(&TranscriptViewItemId::reasoning(turn, activity))
            });
        }
        for owner in owners {
            let still_owned = self
                .snapshot
                .transcript
                .items
                .iter()
                .any(|item| rich_tool_owns_item(item, &owner));
            let live = self
                .tool_invocations
                .get(&owner)
                .is_some_and(|aggregate| aggregate.request_draft.is_some());
            if still_owned || (live && !discard_live) {
                continue;
            }
            let owned = |key: &String| *key == owner || rich_tool_owns_key(key, &owner);
            self.tool_invocations.remove(&owner);
            self.snapshot.tools.remove(&owner);
            self.snapshot
                .active_invocations
                .retain(|key, _| !owned(key));
            self.snapshot.active_exchanges.retain(|key, _| !owned(key));
            self.snapshot.contributions.retain(|key, _| !owned(key));
            self.contribution_sequences.retain(|key, _| !owned(key));
            self.contribution_placements.retain(|key, _| !owned(key));
        }
        for id in &removed {
            self.window.origins.remove(id);
        }
        self.snapshot.transcript.revision = self.snapshot.transcript.revision.saturating_add(1);
        self.bump_revision();
        self.window.counters.evictions = self.window.counters.evictions.saturating_add(1);
        self.sync_mirror();
        changes
            .changes
            .push(TranscriptChange::Removed { ids: removed });
        changes.revision = self.snapshot.transcript.revision;
    }

    /// Forget resolved interactions (all, or those in `only`) and their dedupe fences. Pending
    /// interactions are authoritative live state and are kept.
    fn forget_resolved_interactions(&mut self, only: Option<&BTreeSet<String>>) {
        let selected = |id: &String| only.is_none_or(|only| only.contains(id));
        self.snapshot
            .interactions
            .retain(|interaction| !(interaction.resolved && selected(&interaction.interaction_id)));
        let pending = self
            .snapshot
            .interactions
            .iter()
            .map(|interaction| interaction.interaction_id.clone())
            .collect::<BTreeSet<_>>();
        self.terminal_interactions
            .retain(|id, _| !selected(id) || pending.contains(id));
    }

    /// Drop tool-invocation aggregates a replacement kept only as draft fences for invocations
    /// that no resident item references and that have no live draft or presentation. They would
    /// otherwise make every page that brings such an invocation back count as shared keyed
    /// state (an SD2 fallback on every page). Eviction drops the same aggregates (SD3).
    fn prune_unreferenced_invocation_fences(&mut self) {
        let owners = self
            .snapshot
            .transcript
            .items
            .iter()
            .filter_map(invocation_owner)
            .collect::<BTreeSet<_>>();
        let empty = bcode_session_models::ToolInvocationProjection::default();
        self.tool_invocations.retain(|id, aggregate| {
            owners.contains(id.as_str())
                || aggregate.projection != empty
                || aggregate.terminal_status.is_some()
                || aggregate.request_draft.is_some()
                || !aggregate.presentations.is_empty()
        });
    }

    /// Creation sequence of a resident item (first observation), else its sequence.
    fn item_origin(&self, id: &TranscriptViewItemId) -> Option<u64> {
        self.window.origins.get(id).copied().or_else(|| {
            self.snapshot
                .transcript
                .items
                .iter()
                .find(|item| item.id == *id)
                .and_then(|item| item.sequence)
        })
    }

    /// Record the creation sequence of durable items not seen before. A partial pass scans from
    /// the tail (where the reducer appends) and stops at the first known item.
    fn observe_origins(&mut self, full: bool) {
        let items = &self.snapshot.transcript.items;
        let origins = &mut self.window.origins;
        for item in items.iter().rev() {
            let Some(sequence) = item.sequence else {
                continue;
            };
            if origins.contains_key(&item.id) {
                if full {
                    continue;
                }
                break;
            }
            origins.insert(item.id.clone(), sequence);
        }
        if full && origins.len() > items.len().saturating_mul(2).saturating_add(64) {
            let present = items.iter().map(|item| &item.id).collect::<BTreeSet<_>>();
            origins.retain(|id, _| present.contains(id));
        }
    }

    fn sync_mirror(&mut self) {
        self.window.mirror.clear();
        self.window.mirror.extend(
            self.snapshot
                .transcript
                .items
                .iter()
                .map(|item| (item.id.clone(), item.revision)),
        );
        self.window.mirror_revision = self.snapshot.transcript.revision;
    }
}

/// The tool invocation an item belongs to, if any.
fn invocation_owner(item: &TranscriptViewItem) -> Option<&str> {
    match &item.kind {
        TranscriptViewItemKind::ToolInvocation { tool }
        | TranscriptViewItemKind::ToolRequest { tool } => Some(&tool.tool_call_id),
        TranscriptViewItemKind::ToolContribution { contribution, .. } => {
            Some(&contribution.invocation_id)
        }
        TranscriptViewItemKind::ToolRequestDraft { draft } => Some(&draft.tool_call_id),
        _ => None,
    }
}

/// Keyed id/revision diff of `items` against the `mirror` the consumer last saw.
///
/// Survivors that keep their relative order (a longest increasing subsequence of their old
/// positions) are revised in place; survivors that moved are reported as removed and inserted at
/// their new position. `previous` optionally supplies the previous item payloads so an item whose
/// content changed at an unchanged revision (a rebuild) is still `Revised`.
fn diff_items(
    mirror: &[(TranscriptViewItemId, ViewRevision)],
    items: &[TranscriptViewItem],
    previous: Option<&BTreeMap<&TranscriptViewItemId, &TranscriptViewItem>>,
) -> Vec<TranscriptChange> {
    if previous.is_none()
        && mirror.len() == items.len()
        && mirror
            .iter()
            .zip(items)
            .all(|((id, revision), item)| *id == item.id && *revision == item.revision)
    {
        return Vec::new();
    }
    let old = mirror
        .iter()
        .enumerate()
        .map(|(index, (id, revision))| (id, (index, *revision)))
        .collect::<BTreeMap<_, _>>();
    // New positions of survivors and their old indices, in new order.
    let survivors = items
        .iter()
        .enumerate()
        .filter_map(|(position, item)| old.get(&item.id).map(|(index, _)| (position, *index)))
        .collect::<Vec<_>>();
    let stable = longest_increasing(&survivors.iter().map(|(_, old)| *old).collect::<Vec<_>>())
        .into_iter()
        .map(|index| survivors[index].0)
        .collect::<BTreeSet<_>>();
    let kept_ids = stable
        .iter()
        .map(|position| &items[*position].id)
        .collect::<BTreeSet<_>>();

    let mut changes = Vec::new();
    let removed = mirror
        .iter()
        .filter(|(id, _)| !kept_ids.contains(id))
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    if !removed.is_empty() {
        changes.push(TranscriptChange::Removed { ids: removed });
    }
    let mut previous_id: Option<&TranscriptViewItemId> = None;
    let mut run: Option<(Option<TranscriptViewItemId>, Vec<TranscriptViewItemId>)> = None;
    for (position, item) in items.iter().enumerate() {
        if stable.contains(&position) {
            if let Some((after, ids)) = run.take() {
                changes.push(TranscriptChange::Inserted { after, ids });
            }
            let old_revision = old.get(&item.id).map(|(_, revision)| *revision);
            let content_changed = previous
                .and_then(|items| items.get(&item.id))
                .is_some_and(|before| **before != *item);
            if old_revision != Some(item.revision) || content_changed {
                changes.push(TranscriptChange::Revised {
                    id: item.id.clone(),
                    revision: item.revision,
                });
            }
        } else {
            run.get_or_insert_with(|| (previous_id.cloned(), Vec::new()))
                .1
                .push(item.id.clone());
        }
        previous_id = Some(&item.id);
    }
    if let Some((after, ids)) = run.take() {
        changes.push(TranscriptChange::Inserted { after, ids });
    }
    changes
}

/// Indices of one longest strictly increasing subsequence of `values` (O(n log n)).
fn longest_increasing(values: &[usize]) -> Vec<usize> {
    // `tails[k]`: index of the smallest tail value of an increasing run of length k + 1.
    let mut tails: Vec<usize> = Vec::new();
    let mut parent = vec![None; values.len()];
    for (index, value) in values.iter().enumerate() {
        let length = tails.partition_point(|tail| values[*tail] < *value);
        if length > 0 {
            parent[index] = Some(tails[length - 1]);
        }
        if length == tails.len() {
            tails.push(index);
        } else {
            tails[length] = index;
        }
    }
    let mut result = Vec::with_capacity(tails.len());
    let mut cursor = tails.last().copied();
    while let Some(index) = cursor {
        result.push(index);
        cursor = parent[index];
    }
    result.reverse();
    result
}

/// Incremental patch operations for `changes`, or `None` when only a reset is correct.
fn incremental_patch_ops(
    previous: &TranscriptViewDocument,
    current: &TranscriptViewDocument,
    changes: &TranscriptChanges,
) -> Option<Vec<TranscriptViewPatchOp>> {
    let describes_step =
        (changes.base_revision, changes.revision) == (previous.revision, current.revision);
    if !describes_step {
        return None;
    }
    let current_items = current
        .items
        .iter()
        .map(|item| (&item.id, item))
        .collect::<BTreeMap<_, _>>();
    let mut working = previous
        .items
        .iter()
        .map(|item| (item.id.clone(), item.revision))
        .collect::<Vec<_>>();
    let mut emitted = BTreeSet::new();
    let mut ops = Vec::new();
    for change in &changes.changes {
        match change {
            TranscriptChange::Removed { ids } => {
                for id in ids {
                    let position = working.iter().position(|(working, _)| working == id)?;
                    let (_, revision) = working.remove(position);
                    ops.push(TranscriptViewPatchOp::Remove {
                        id: id.clone(),
                        revision: revision.saturating_add(1),
                    });
                }
            }
            TranscriptChange::Inserted { after, ids } => {
                if working.last().map(|(id, _)| id) != after.as_ref() {
                    return None;
                }
                for id in ids {
                    let item = current_items.get(id)?;
                    if working.iter().any(|(working, _)| working == id) {
                        return None;
                    }
                    working.push((id.clone(), item.revision));
                    emitted.insert(id.clone());
                    ops.push(TranscriptViewPatchOp::Append {
                        item: (*item).clone(),
                    });
                }
            }
            TranscriptChange::Revised { id, .. } => {
                if emitted.contains(id) {
                    continue;
                }
                let item = current_items.get(id)?;
                let slot = working.iter_mut().find(|(working, _)| working == id)?;
                if item.revision <= slot.1 {
                    return None;
                }
                slot.1 = item.revision;
                emitted.insert(id.clone());
                ops.push(TranscriptViewPatchOp::Replace {
                    item: (*item).clone(),
                });
            }
            TranscriptChange::Reset => return None,
        }
    }
    let consistent = working.len() == current.items.len()
        && working
            .iter()
            .zip(&current.items)
            .all(|((id, _), item)| *id == item.id);
    consistent.then_some(ops)
}

#[cfg(test)]
mod tests;
