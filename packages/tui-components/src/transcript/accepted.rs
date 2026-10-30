//! Caller-owned semantic acceptance and dispatch for resident transcript items.

use super::item::{TranscriptItem, TranscriptItemKind};
use super::transcript_document::{SessionViewTerminalAdapter, TranscriptDocument};
use super::transcript_layout::{
    TranscriptLayoutCache, TranscriptLayoutFingerprint, TranscriptLayoutRows,
    TranscriptLayoutSignature, TranscriptLayoutSpec,
};
use super::transcript_markdown_cache::TranscriptMarkdownCache;
use crate::transcript_markdown::{MarkdownBlockStyle, append_markdown_block};
use bcode_markdown_render::MarkdownRenderOptions;
use bcode_session_view_models::{TextFormat, TranscriptViewDocument, TranscriptViewItemId};
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::prelude::{Line, Style};
use std::collections::{BTreeMap, BTreeSet};

/// Accepted rich adapter output. Rows, anchors and selection are replaced together.
/// Adapter execution is host-owned and must finish before composition.
#[derive(Debug, Clone)]
pub struct AcceptedVisual {
    /// Semantic revision this output was prepared from.
    pub item_revision: u64,
    /// Layout width, including transcript chrome.
    pub width: u16,
    /// Adapter-owned revision, for invalidation without a semantic event.
    pub revision: u64,
    /// Complete native output; never a patch to the previous snapshot.
    pub rows: TranscriptLayoutRows,
}

/// Renderer-boundary presentation policy; portable session contracts stay neutral.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TranscriptPresentation {
    /// Semantic heading style.
    pub heading: Style,
    /// Secondary metadata style.
    pub muted: Style,
    /// Markdown styles resolved by the consuming host.
    pub markdown: bcode_markdown_render::MarkdownTheme,
}

/// Identity of all inputs used by an asynchronous visual preparation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualPreparation {
    item_id: TranscriptViewItemId,
    item_revision: u64,
    width: u16,
    presentation_revision: u64,
    retained_visual_revision: Option<u64>,
}

/// Exact retained provenance for a producer action. This is not execution authority;
/// callers must first verify that the containing scene successfully committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedActionProvenance {
    /// Canonical source item owning the accepted region.
    pub item_id: TranscriptViewItemId,
    /// Canonical source revision used for the accepted region.
    pub item_revision: u64,
    /// Exact adapter-local action identity, never inferred from its payload.
    pub action_id: String,
    /// Width at which the producer output was accepted.
    pub width: u16,
}

/// Result of applying an accepted local action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptLocalAction {
    /// Local disclosure changed; prepare layout before committing the next scene.
    Reprepare,
    /// Exact destination in retained BMUX geometry.
    ScrollTo {
        /// Stable canonical item identity.
        item_id: TranscriptViewItemId,
        /// Source position when BMUX provides correspondence.
        source_position: Option<usize>,
        /// Absolute current transcript row.
        row: usize,
    },
    /// Intent subject to the embedding host's capability policy.
    External(super::actions::TranscriptAction),
}

/// Resident presentation state, owned by the caller rather than recreated in paint.
#[derive(Debug, Clone, Default)]
pub struct AcceptedTranscript {
    /// Incrementally adapted semantic items, including plugin contributions.
    pub document: TranscriptDocument,
    /// The same indexed retained layout used by the native host.
    pub layout: TranscriptLayoutCache,
    /// One bounded set of projection variants per resident item.
    pub markdown: TranscriptMarkdownCache,
    adapter: SessionViewTerminalAdapter,
    visuals: BTreeMap<TranscriptViewItemId, AcceptedVisual>,
    images: BTreeMap<TranscriptViewItemId, Vec<bmux_tui::image::ImagePlacement>>,
    actions: BTreeMap<TranscriptViewItemId, Vec<super::actions::TranscriptActionRegion>>,
    prepared_width: Option<u16>,
    portable_generation: Option<(u64, u64)>,
    portable_items: BTreeSet<TranscriptViewItemId>,
    presentation: TranscriptPresentation,
    presentation_revision: u64,
    prepared_visual_revision: u64,
    visual_revision: u64,
    accepted_revision: Option<u64>,
    /// A rejected oversized resident window requires host paging, not truncation.
    pub window_rejected: bool,
    rejected_revision: Option<u64>,
}

impl AcceptedTranscript {
    fn reset_projection(&mut self) {
        let presentation = self.presentation;
        let revision = self.presentation_revision.wrapping_add(1);
        *self = Self::default();
        self.presentation = presentation;
        self.presentation_revision = revision;
    }

    /// Reload renderer styles without changing canonical items, draft or viewport.
    /// Adapter projections are discarded because their styles are derived state.
    pub fn set_presentation(&mut self, presentation: TranscriptPresentation) {
        if self.presentation == presentation {
            return;
        }
        self.presentation = presentation;
        self.presentation_revision = self.presentation_revision.wrapping_add(1);
        self.visuals.clear();
        self.portable_items.clear();
        self.images.clear();
        self.actions.clear();
        self.visual_revision = self.visual_revision.wrapping_add(1);
        if let Some(width) = self.prepared_width {
            self.prepare_layout(width);
        }
    }

    /// Capture input identity before starting asynchronous adapter work.
    pub fn visual_preparation(&self, id: &TranscriptViewItemId) -> Option<VisualPreparation> {
        let item = self.document.get(self.document.source_index(id)?)?;
        Some(VisualPreparation {
            item_id: id.clone(),
            item_revision: item.source_view_item_revision()?,
            width: self.prepared_width?,
            presentation_revision: self.presentation_revision,
            retained_visual_revision: self.visuals.get(id).map(|visual| visual.revision),
        })
    }

    /// Allocate a strictly newer item-local output revision for an exact-current
    /// preparation without removing or mutating the retained output.
    ///
    /// Returns `None` for a stale preparation or exhausted revision space.
    pub fn replacement_visual_revision(&self, preparation: &VisualPreparation) -> Option<u64> {
        if self.visual_preparation(&preparation.item_id).as_ref() != Some(preparation) {
            return None;
        }
        preparation
            .retained_visual_revision
            .unwrap_or(0)
            .checked_add(1)
    }

    /// Accept an asynchronous completion only if every captured input still matches.
    #[must_use]
    pub fn accept_prepared_visual(
        &mut self,
        preparation: &VisualPreparation,
        visual: AcceptedVisual,
        images: Vec<bmux_tui::image::ImagePlacement>,
        actions: Vec<super::actions::TranscriptActionRegion>,
    ) -> bool {
        if self.visual_preparation(&preparation.item_id).as_ref() != Some(preparation)
            || visual.item_revision != preparation.item_revision
            || visual.width != preparation.width
        {
            return false;
        }
        self.accept_rich_visual_with_actions(preparation.item_id.clone(), visual, images, actions)
    }

    /// Replace a complete plugin snapshot outside paint. Generation mismatch is
    /// rejected again during dispatch, so stale completions cannot replace rows.
    #[must_use]
    pub fn accept_visual(&mut self, id: TranscriptViewItemId, visual: AcceptedVisual) -> bool {
        self.accept_rich_visual(id, visual, Vec::new())
    }

    /// Replace rows, source geometry and item-local images as one bounded snapshot.
    /// Images use frame lifetime: replacement, eviction and offscreen composition
    /// therefore release resources through the existing retained BMUX scene.
    #[must_use]
    pub fn accept_rich_visual(
        &mut self,
        id: TranscriptViewItemId,
        visual: AcceptedVisual,
        images: Vec<bmux_tui::image::ImagePlacement>,
    ) -> bool {
        self.accept_rich_visual_with_actions(id, visual, images, Vec::new())
    }

    /// Accept action geometry atomically with the exact rows and image resources.
    #[must_use]
    pub fn accept_rich_visual_with_actions(
        &mut self,
        id: TranscriptViewItemId,
        visual: AcceptedVisual,
        images: Vec<bmux_tui::image::ImagePlacement>,
        actions: Vec<super::actions::TranscriptActionRegion>,
    ) -> bool {
        if !super::actions::supported(&actions, visual.width, visual.rows.len())
            || self.prepared_width != Some(visual.width)
            || !images_supported(&images, visual.width, visual.rows.len())
            || !visual_supported(&visual.rows)
        {
            return false;
        }
        let Some(index) = self.document.source_index(&id) else {
            return false;
        };
        let Some(item) = self.document.get(index) else {
            return false;
        };
        if matches!(
            item.kind(),
            TranscriptItemKind::ToolContribution {
                placement: bcode_session_models::ToolContributionPlacement::Hidden,
                ..
            }
        ) || item.source_view_item_revision() != Some(visual.item_revision)
        {
            return false;
        }
        if let Some(previous) = self.visuals.get(&id)
            && previous.item_revision == visual.item_revision
            && previous.width == visual.width
            && previous.revision >= visual.revision
        {
            return false;
        }
        let retained_bytes = self
            .images
            .iter()
            .filter(|(key, _)| *key != &id)
            .flat_map(|(_, images)| images)
            .chain(&images)
            .fold(0usize, |sum, image| {
                sum.saturating_add(match &image.payload {
                    bmux_tui::image::ImagePayload::Png { bytes, .. }
                    | bmux_tui::image::ImagePayload::Pixels { bytes, .. } => bytes.len(),
                })
            });
        let action_bytes = self
            .actions
            .iter()
            .filter(|(key, _)| *key != &id)
            .map(|(_, regions)| super::actions::retained_bytes(regions))
            .fold(
                super::actions::retained_bytes(&actions),
                usize::saturating_add,
            );
        if retained_bytes > 16 * 1024 * 1024 || action_bytes > 4 * 1024 * 1024 {
            return false;
        }
        self.actions.insert(id.clone(), actions);
        self.portable_items.remove(&id);
        self.images.insert(id.clone(), images);
        self.visuals.insert(id, visual);
        self.visual_revision = self.visual_revision.wrapping_add(1);
        true
    }

    /// Prepare static producer adapters outside painting and atomically accept their
    /// rows, source mappings, images and actions. Reload generations invalidate all
    /// previous outputs even if the semantic transcript did not change.
    pub fn prepare_portable_visuals(
        &mut self,
        registry: &super::tool_visual::portable::PortableVisualRegistry,
        theme: Option<&bmux_tui_components::theme::ComponentTheme>,
    ) {
        use super::tool_visual::{CanonicalToolVisual, portable::PortableVisualContext};
        let generation = (registry.generation(), registry.artifact_revision());
        if self.portable_generation != Some(generation) {
            // Registry invalidation owns only registry-prepared output. Decoded
            // images and other independently accepted visuals have separate
            // preparation fences and cannot be regenerated by this registry.
            for id in std::mem::take(&mut self.portable_items) {
                self.visuals.remove(&id);
                self.images.remove(&id);
                self.actions.remove(&id);
            }
            self.portable_generation = Some(generation);
            self.presentation_revision = self.presentation_revision.wrapping_add(1);
            self.visual_revision = self.visual_revision.wrapping_add(1);
        }
        let Some(width) = self.prepared_width else {
            return;
        };
        // The resident document has already passed the accepted-window budget.
        let ids: Vec<_> = self
            .document
            .iter()
            .filter_map(|item| {
                let id = item.source_view_item_id()?;
                (!self.visuals.contains_key(id)).then_some(id.clone())
            })
            .collect();
        for id in ids {
            let Some(item) = self
                .document
                .source_index(&id)
                .and_then(|i| self.document.get(i))
            else {
                continue;
            };
            let Some(CanonicalToolVisual::Plugin(visual)) =
                CanonicalToolVisual::for_item(item, None)
            else {
                continue;
            };
            let Some(preparation) = self.visual_preparation(&id) else {
                continue;
            };
            let context = PortableVisualContext { width, theme };
            let Some(prepared) = registry.prepare(&visual, &context) else {
                continue;
            };
            let Some(output) = registry.resolve(prepared, &visual, width) else {
                continue;
            };
            self.visual_revision = self.visual_revision.wrapping_add(1);
            let accepted = AcceptedVisual {
                item_revision: preparation.item_revision,
                width,
                revision: self.visual_revision,
                rows: output.rows,
            };
            if self.accept_prepared_visual(&preparation, accepted, output.images, output.actions) {
                self.portable_items.insert(id);
            }
        }
        self.prepare_layout(width);
    }

    /// Prepare missing adapter outputs outside paint. The host resolves the
    /// actual plugin registry and executes eligible adapters; `None` keeps the
    /// truthful semantic fallback. Each resident item is visited at most once.
    pub fn prepare_visuals(
        &mut self,
        mut render: impl FnMut(
            &TranscriptItem,
            u16,
        ) -> Option<(
            u64,
            TranscriptLayoutRows,
            Vec<bmux_tui::image::ImagePlacement>,
        )>,
    ) {
        let Some(width) = self.prepared_width else {
            return;
        };
        let ids: Vec<_> = self
            .document
            .iter()
            .filter_map(|item| {
                let id = item.source_view_item_id()?;
                (!self.visuals.contains_key(id)).then_some(id.clone())
            })
            .collect();
        for id in ids {
            let Some(index) = self.document.source_index(&id) else {
                continue;
            };
            let Some(item) = self.document.get(index) else {
                continue;
            };
            let Some(item_revision) = item.source_view_item_revision() else {
                continue;
            };
            let Some((revision, rows, images)) = render(item, width) else {
                continue;
            };
            let _ = self.accept_rich_visual(
                id,
                AcceptedVisual {
                    item_revision,
                    width,
                    revision,
                    rows,
                },
                images,
            );
        }
        self.prepare_layout(width);
    }

    /// Resolve a hit routed by the host's last committed scene. This performs no
    /// platform work. Revision/width-qualified IDs reject old painted targets.
    /// Apply a committed hit using retained Markdown semantics and geometry.
    /// No Markdown parsing, host invocation or canonical mutation occurs here.
    pub fn activate_local(&mut self, hit_id: &str) -> Option<TranscriptLocalAction> {
        use super::actions::TranscriptAction;
        use bcode_markdown_render::{MarkdownContributionKind, MarkdownDestination};
        let action = self.activate(hit_id)?;
        let local = match &action {
            TranscriptAction::Open(MarkdownDestination::Fragment(_)) => true,
            TranscriptAction::Presentation { kind, .. } => matches!(
                kind.as_ref(),
                MarkdownContributionKind::Details { .. }
                    | MarkdownContributionKind::FootnoteReference { .. }
            ),
            _ => false,
        };
        if !local {
            return Some(TranscriptLocalAction::External(action));
        }
        let suffix = hit_id.strip_prefix("markdown:")?;
        let item_id: u64 = suffix.split(':').next()?.parse().ok()?;
        let index = self
            .document
            .iter()
            .position(|item| item.id().get() == item_id)?;
        let item = self.document.get(index)?;
        let canonical_id = item.source_view_item_id()?.clone();
        let (projection, body_start) = self.layout.markdown(index)?;
        let target = match &action {
            TranscriptAction::Presentation {
                contribution_id,
                kind,
            } => match kind.as_ref() {
                MarkdownContributionKind::Details { default_open, .. } => {
                    if !self.markdown.toggle_details(
                        item_id,
                        item.revision(),
                        contribution_id,
                        *default_open,
                    ) {
                        return None;
                    }
                    self.presentation_revision = self.presentation_revision.wrapping_add(1);
                    self.visual_revision = self.visual_revision.wrapping_add(1);
                    return Some(TranscriptLocalAction::Reprepare);
                }
                MarkdownContributionKind::FootnoteReference { target_id, .. } => projection
                    .geometry
                    .iter()
                    .find(|geometry| geometry.contribution_id == *target_id)
                    .and_then(|geometry| geometry.rects.first())
                    .map(|rect| usize::from(rect.y))?,
                _ => return Some(TranscriptLocalAction::External(action)),
            },
            TranscriptAction::Open(MarkdownDestination::Fragment(fragment)) => usize::from(
                projection
                    .anchors
                    .iter()
                    .find(|anchor| anchor.fragment == *fragment)?
                    .row,
            ),
            _ => return Some(TranscriptLocalAction::External(action)),
        };
        let local_row = body_start.saturating_add(target);
        let row = self
            .layout
            .entry_start_row(
                super::transcript_layout::VisibleTranscriptSource::Transcript,
                index,
            )?
            .saturating_add(local_row);
        Some(TranscriptLocalAction::ScrollTo {
            item_id: canonical_id,
            source_position: self.layout.source_position(index, local_row),
            row,
        })
    }

    /// Recover exact retained producer-region provenance without rendering or mutation.
    /// Callers must gate this lookup on their successfully committed scene identity.
    pub fn action_provenance(&self, hit_id: &str) -> Option<AcceptedActionProvenance> {
        if self.prepared_visual_revision != self.visual_revision {
            return None;
        }
        for (id, regions) in &self.actions {
            let item = self.document.get(self.document.source_index(id)?)?;
            let visual = self.visuals.get(id)?;
            if item.source_view_item_revision() != Some(visual.item_revision)
                || self.prepared_width != Some(visual.width)
            {
                continue;
            }
            for region in regions {
                if action_hit_id(item, visual, &region.id) == hit_id {
                    return Some(AcceptedActionProvenance {
                        item_id: id.clone(),
                        item_revision: visual.item_revision,
                        action_id: region.id.clone(),
                        width: visual.width,
                    });
                }
            }
        }
        None
    }

    /// Resolve an exact-current action without platform work.
    pub fn activate(&self, hit_id: &str) -> Option<super::actions::TranscriptAction> {
        use super::actions::TranscriptAction;
        if self.prepared_visual_revision != self.visual_revision {
            return None;
        }
        for (id, regions) in &self.actions {
            let index = self.document.source_index(id)?;
            let item = self.document.get(index)?;
            let visual = self.visuals.get(id)?;
            if item.source_view_item_revision() != Some(visual.item_revision) {
                continue;
            }
            for region in regions {
                if action_hit_id(item, visual, &region.id) == hit_id {
                    return Some(region.action.clone());
                }
            }
        }
        if let Some(suffix) = hit_id.strip_prefix("transcript-copy:") {
            let mut parts = suffix.splitn(4, ':');
            let item_id: u64 = parts.next()?.parse().ok()?;
            let revision: u64 = parts.next()?.parse().ok()?;
            let width: u16 = parts.next()?.parse().ok()?;
            if self.prepared_width != Some(width) {
                return None;
            }
            let block_id = parts.next()?;
            let (index, item) = self
                .document
                .iter()
                .enumerate()
                .find(|(_, item)| item.id().get() == item_id && item.revision() == revision)?;
            let projection = self.layout.markdown(index)?.0;
            let block = projection
                .code_block_selections()
                .iter()
                .find(|block| block.id == block_id)?;
            let mut text = String::new();
            for range in &block.body_ranges {
                text.push_str(item.text().get(range.clone())?);
            }
            return Some(TranscriptAction::Copy(text));
        }
        let suffix = hit_id.strip_prefix("markdown:")?;
        let mut parts = suffix.splitn(4, ':');
        let item_id: u64 = parts.next()?.parse().ok()?;
        let revision: u64 = parts.next()?.parse().ok()?;
        let width: u16 = parts.next()?.parse().ok()?;
        if self.prepared_width != Some(width) {
            return None;
        }
        let (contribution_id, _) = parts.next()?.rsplit_once(':')?;
        for (index, item) in self.document.iter().enumerate() {
            if item.id().get() != item_id || item.revision() != revision {
                continue;
            }
            let Some((projection, _)) = self.layout.markdown(index) else {
                continue;
            };
            let Some(contribution) = projection
                .contributions
                .iter()
                .find(|value| value.id == contribution_id)
            else {
                continue;
            };
            return match &contribution.kind {
                bcode_markdown_render::MarkdownContributionKind::Link { destination, .. }
                | bcode_markdown_render::MarkdownContributionKind::GitHubIssue {
                    destination,
                    ..
                } if crate::transcript_markdown::contribution_actionable(&contribution.kind) => {
                    Some(TranscriptAction::Open(destination.clone()))
                }
                bcode_markdown_render::MarkdownContributionKind::Image {
                    source,
                    linked_destination,
                    ..
                } => {
                    let destination = linked_destination.as_ref().unwrap_or(source);
                    matches!(
                        destination,
                        bcode_markdown_render::MarkdownDestination::Web(_)
                            | bcode_markdown_render::MarkdownDestination::LocalPath(_)
                            | bcode_markdown_render::MarkdownDestination::Fragment(_)
                    )
                    .then(|| TranscriptAction::Open(destination.clone()))
                }
                kind if crate::transcript_markdown::contribution_actionable(kind) => {
                    Some(TranscriptAction::Presentation {
                        contribution_id: contribution.id.clone(),
                        kind: Box::new(kind.clone()),
                    })
                }
                _ => None,
            };
        }
        None
    }

    /// Compose accepted rows and images using the same viewport and clipping.
    #[must_use]
    pub const fn child<'a>(
        &'a self,
        id: &'a str,
        top: usize,
        selection_scope: &'a str,
    ) -> AcceptedTranscriptChild<'a> {
        AcceptedTranscriptChild {
            transcript: self,
            id,
            top,
            selection_scope,
        }
    }

    /// Remove derived presentation when an adapter is disabled or fails.
    #[must_use]
    pub fn remove_visual(&mut self, id: &TranscriptViewItemId) -> bool {
        self.portable_items.remove(id);
        self.images.remove(id);
        self.actions.remove(id);
        let removed = self.visuals.remove(id).is_some();
        if removed {
            self.presentation_revision = self.presentation_revision.wrapping_add(1);
            self.visual_revision = self.visual_revision.wrapping_add(1);
        }
        removed
    }

    /// Copy logical sources from the retained projection, rejecting stale revisions.
    /// Soft wraps never introduce newlines; code selections preserve source fences.
    pub fn copy(&self, selection: &bmux_tui::selection::SelectionSnapshot) -> Option<String> {
        let mut output = String::new();
        for slice in &selection.slices {
            if let Some(source) = self.producer_source(
                slice.content_id.as_str(),
                slice.revision,
                slice.source_range.clone(),
            ) {
                output.push_str(&source);
                continue;
            }
            let (index, item, suffix) =
                self.document.iter().enumerate().find_map(|(index, item)| {
                    let id = format!("transcript:{}", item.id().get());
                    let suffix = slice.content_id.as_str().strip_prefix(&id)?;
                    (suffix.is_empty() || suffix.starts_with('.')).then_some((index, item, suffix))
                })?;
            if slice.revision != item.revision() {
                return None;
            }
            let ranges = if let Some(code_id) = suffix.strip_prefix('.') {
                let projection = self.layout.markdown(index)?.0;
                crate::markdown_selection::expand_code_selection_range(
                    item.text(),
                    projection.code_block_selections(),
                    code_id,
                    slice.source_range.clone(),
                )?
            } else {
                vec![slice.source_range.clone()]
            };
            for range in ranges {
                output.push_str(item.text().get(range)?);
            }
        }
        Some(output)
    }

    /// Resolve accepted producer source by stable identity and revision, never by
    /// presentation index or by a semantic item's fallback text.
    pub fn producer_source(
        &self,
        identity: &str,
        revision: u64,
        range: std::ops::Range<usize>,
    ) -> Option<String> {
        let mut segments = BTreeMap::new();
        for (id, visual) in &self.visuals {
            let Some(index) = self.document.source_index(id) else {
                continue;
            };
            if self.document.get(index)?.source_view_item_revision() != Some(visual.item_revision)
                || self.prepared_width != Some(visual.width)
            {
                continue;
            }
            if let TranscriptLayoutRows::Selected { selection, .. } = &visual.rows {
                for row in selection
                    .values()
                    .filter(|row| row.identity == identity && row.revision == revision)
                {
                    if let Some(previous) = segments.insert(row.byte_start, row.text.as_str())
                        && previous != row.text
                    {
                        // Two producers cannot silently establish competing source truth.
                        return None;
                    }
                }
            }
        }
        let mut cursor = range.start;
        let mut result = String::new();
        for (start, text) in segments {
            let end = start.checked_add(text.len())?;
            if end <= cursor || start >= range.end {
                continue;
            }
            if start > cursor {
                return None;
            }
            let selected_end = end.min(range.end);
            result.push_str(text.get(cursor.checked_sub(start)?..selected_end - start)?);
            cursor = selected_end;
        }
        (cursor == range.end).then_some(result)
    }

    /// Accept a complete, already bounded resident semantic window and prepare its
    /// rows. Never invoke this method from `Component::paint`.
    pub fn prepare(&mut self, document: &TranscriptViewDocument, width: u16) {
        if self.accepted_revision == Some(document.revision)
            || self.rejected_revision == Some(document.revision)
        {
            self.prepare_layout(width);
            return;
        }
        if !resident_window_supported(document) {
            self.reset_projection();
            self.window_rejected = true;
            self.rejected_revision = Some(document.revision);
            self.document.push_ephemeral("resident-window".into(), TranscriptItem::new("System", "Resident transcript window exceeds the local projection budget; load a smaller history page.".into()));
            self.prepare_layout(width);
            return;
        }
        if self.window_rejected {
            self.reset_projection();
        }
        self.adapter
            .apply_document(document, &BTreeMap::new(), true, &mut self.document);
        self.accepted_revision = Some(document.revision);
        self.prepare_layout(width);
    }

    /// Accept stream integrity and reasoning visibility with the semantic snapshot.
    pub fn prepare_snapshot(
        &mut self,
        snapshot: &bcode_session_view_models::SessionViewSnapshot,
        width: u16,
    ) {
        if !resident_window_supported(&snapshot.transcript) {
            self.prepare(&snapshot.transcript, width);
            return;
        }
        if self.window_rejected {
            self.reset_projection();
        }
        self.adapter.apply(snapshot, &mut self.document);
        self.accepted_revision = Some(snapshot.transcript.revision);
        self.prepare_layout(width);
    }

    fn prepare_layout(&mut self, width: u16) {
        if self.prepared_width != Some(width) {
            self.presentation_revision = self.presentation_revision.wrapping_add(1);
        }
        let fingerprint = TranscriptLayoutFingerprint::new(format!(
            "{}:{width}:{}",
            self.document.revision(),
            self.visual_revision
        ));
        if self.layout.is_current(&fingerprint) {
            return;
        }
        let revisions: BTreeMap<_, _> = self
            .document
            .iter()
            .filter_map(|item| {
                Some((
                    item.source_view_item_id()?.clone(),
                    item.source_view_item_revision()?,
                ))
            })
            .collect();
        self.visuals.retain(|id, visual| {
            revisions.get(id) == Some(&visual.item_revision) && visual.width == width
        });
        self.portable_items
            .retain(|id| self.visuals.contains_key(id));
        self.images.retain(|id, _| self.visuals.contains_key(id));
        self.actions.retain(|id, _| self.visuals.contains_key(id));
        self.prepared_width = Some(width);
        let items = self.document.items();
        self.markdown
            .retain_resident_iter(items.iter(), self.document.revision());
        let markdown = &self.markdown;
        let visuals = &self.visuals;
        let presentation = self.presentation;
        let _ = self.layout.sync(TranscriptLayoutSpec {
            width,
            fingerprint,
            structural_fingerprint: TranscriptLayoutFingerprint::new(format!(
                "{}:{width}",
                items.len()
            )),
            transcript_len: items.len(),
            pending_len: 0,
            transcript_signature: |index| {
                let item = &items[index];
                TranscriptLayoutSignature::new(format!(
                    "{}:{}:{:?}:{presentation:?}:{}",
                    item.id().get(),
                    item.revision(),
                    item.source_view_item_id()
                        .and_then(|id| visuals.get(id))
                        .map(|visual| visual.revision),
                    self.presentation_revision
                ))
            },
            transcript_rows: |index| {
                let item = &items[index];
                if let Some(visual) = item.source_view_item_id().and_then(|id| visuals.get(id))
                    && visual.width == width
                    && item.source_view_item_revision() == Some(visual.item_revision)
                {
                    return visual.rows.clone();
                }
                let rows = semantic_rows_with_presentation(item, width, markdown, presentation);
                if rows.len() > usize::from(u16::MAX) {
                    TranscriptLayoutRows::Rendered(vec![Line::raw("Transcript item exceeds supported source geometry; load a smaller history page.")])
                } else { rows }
            },
            transcript_invocation_id: |index| {
                items[index].visual_invocation_id().map(str::to_owned)
            },
            pending_signature: |_| unreachable!("no pending entries"),
            pending_rows: |_| unreachable!("no pending entries"),
            history_banner_signature: || None,
            history_banner_rows: || TranscriptLayoutRows::Rendered(Vec::new()),
            reset: || false,
        });
        // Native drains these into telemetry; the browser must not accumulate a queue.
        self.prepared_visual_revision = self.visual_revision;
        let _ = self.layout.drain_sync_stats();
    }
}

fn action_hit_id(item: &TranscriptItem, visual: &AcceptedVisual, id: &str) -> String {
    format!(
        "transcript-action:{}:{}:{}:{}:{}",
        item.id().get(),
        visual.item_revision,
        visual.width,
        visual.revision,
        id
    )
}

/// One accepted BMUX child: image placement and source rows share one layout.
pub struct AcceptedTranscriptChild<'a> {
    transcript: &'a AcceptedTranscript,
    id: &'a str,
    top: usize,
    selection_scope: &'a str,
}

impl AcceptedTranscriptChild<'_> {
    fn rows(&self) -> super::child::TranscriptChild<'_> {
        super::child::TranscriptChild {
            id: self.id,
            layout: &self.transcript.layout,
            items: self.transcript.document.items(),
            top: self.top,
            selection_scope: self.selection_scope,
            metadata: true,
            decorate: None,
        }
    }
}

impl AcceptedTranscriptChild<'_> {
    fn paint_markdown_semantics(
        &self,
        visible: super::transcript_layout::VisibleTranscriptLine,
        width: u16,
        cx: &mut bmux_tui::paint::PaintCx<'_, '_>,
    ) {
        use bmux_tui::geometry::Rect;
        if visible.source != super::transcript_layout::VisibleTranscriptSource::Transcript {
            return;
        }
        let Some(item) = self.transcript.document.get(visible.entry_index) else {
            return;
        };
        let Some((projection, body_start)) = self.transcript.layout.markdown(visible.entry_index)
        else {
            return;
        };
        let Some(row) = visible
            .row_in_entry
            .checked_sub(body_start)
            .and_then(|row| u16::try_from(row).ok())
        else {
            return;
        };
        let Ok(y) = u16::try_from(visible.row_index.saturating_sub(self.top)) else {
            return;
        };
        for (index, block) in projection.code_block_selections().iter().enumerate() {
            let Some(geometry) = projection
                .code_block_geometry
                .iter()
                .find(|geometry| geometry.contribution_id == format!("code-header:{index}"))
            else {
                continue;
            };
            for area in &geometry.rects {
                if row >= area.y && row < area.y.saturating_add(area.height) {
                    paint_markdown_semantic(
                        cx,
                        format!(
                            "transcript-copy:{}:{}:{width}:{}",
                            item.id().get(),
                            item.revision(),
                            block.id
                        ),
                        Rect::new(area.x.saturating_add(2), y, area.width, 1),
                        "button",
                        "Copy code block".into(),
                    );
                }
            }
        }
        for geometry in &projection.geometry {
            let Some(contribution) = projection.contributions.iter().find(|contribution| {
                contribution.id == geometry.contribution_id
                    && (crate::transcript_markdown::contribution_actionable(&contribution.kind)
                        || matches!(
                            contribution.kind,
                            bcode_markdown_render::MarkdownContributionKind::Image { .. }
                        ))
            }) else {
                continue;
            };
            let (role, label) = match &contribution.kind {
                bcode_markdown_render::MarkdownContributionKind::Link { label, .. } => {
                    ("link", label.clone())
                }
                bcode_markdown_render::MarkdownContributionKind::Image { alt, .. } => {
                    ("button", alt.clone())
                }
                bcode_markdown_render::MarkdownContributionKind::Details { summary, .. } => {
                    ("button", format!("Toggle details: {summary}"))
                }
                _ => ("button", "Activate Markdown content".into()),
            };
            for (index, area) in geometry.rects.iter().enumerate() {
                if row >= area.y && row < area.y.saturating_add(area.height) {
                    paint_markdown_semantic(
                        cx,
                        format!(
                            "markdown:{}:{}:{width}:{}:{index}",
                            item.id().get(),
                            item.revision(),
                            geometry.contribution_id
                        ),
                        Rect::new(area.x.saturating_add(2), y, area.width, 1),
                        role,
                        label.clone(),
                    );
                }
            }
        }
    }
}

fn paint_markdown_semantic(
    cx: &mut bmux_tui::paint::PaintCx<'_, '_>,
    id: String,
    area: bmux_tui::geometry::Rect,
    role: &str,
    label: String,
) {
    let mut semantic = bmux_tui::semantic::SemanticRegion::new(id, area, role);
    semantic.accessibility = Some(bmux_tui::semantic::SemanticAccessibility {
        label: Some(label),
        actions: vec![bmux_tui::semantic::SemanticAction::Activate],
        ..Default::default()
    });
    cx.push_semantic(semantic);
}

impl bmux_tui::component::Component for AcceptedTranscriptChild<'_> {
    fn layout(
        &self,
        constraints: bmux_tui::component::Constraints,
        cx: &mut bmux_tui::component::LayoutCx,
    ) -> bmux_tui::component::LayoutNode {
        self.rows().layout(constraints, cx)
    }

    fn paint(
        &self,
        layout: &bmux_tui::component::LayoutNode,
        cx: &mut bmux_tui::paint::PaintCx<'_, '_>,
    ) {
        use bmux_tui::{
            geometry::Rect,
            image::{ImageContribution, ImageKey},
            paint::LocalRect,
        };
        self.rows().paint(layout, cx);
        if self.transcript.prepared_visual_revision != self.transcript.visual_revision {
            return;
        }
        let rect = Rect::new(
            0,
            0,
            u16::try_from(layout.size.width).unwrap_or(u16::MAX),
            u16::try_from(layout.size.height).unwrap_or(u16::MAX),
        );
        for visible in self
            .transcript
            .layout
            .visible_lines_from_top(self.top, rect.height)
        {
            self.paint_markdown_semantics(visible, rect.width, cx);
        }
        let mut painted = BTreeSet::new();
        for visible in self
            .transcript
            .layout
            .visible_lines_from_top(self.top, rect.height)
        {
            if visible.source != super::transcript_layout::VisibleTranscriptSource::Transcript
                || !painted.insert(visible.entry_index)
            {
                continue;
            }
            let Some(item) = self.transcript.document.get(visible.entry_index) else {
                continue;
            };
            let Some(id) = item.source_view_item_id() else {
                continue;
            };
            let regions = self.transcript.actions.get(id);
            let Some(start) = self
                .transcript
                .layout
                .entry_start_row(visible.source, visible.entry_index)
            else {
                continue;
            };
            let origin_y = i64::from(rect.y)
                .saturating_add(i64::try_from(start).unwrap_or(i64::MAX))
                .saturating_sub(i64::try_from(self.top).unwrap_or(i64::MAX));
            cx.with_child(0, 0, LocalRect::terminal(rect), |cx| {
                cx.with_child(
                    i32::from(rect.x),
                    origin_y,
                    LocalRect::new(
                        0,
                        i64::try_from(self.top.saturating_sub(start)).unwrap_or(i64::MAX),
                        rect.width,
                        rect.height,
                    ),
                    |cx| {
                        if let (Some(regions), Some(visual)) =
                            (regions, self.transcript.visuals.get(id))
                        {
                            for region in regions {
                                let action_id = action_hit_id(item, visual, &region.id);
                                cx.push_hit(
                                    bmux_tui::hit::HitRegion::new(action_id.clone(), region.area)
                                        .role(bmux_tui::hit::HitRole::Action)
                                        .focusable(true)
                                        .layer(1),
                                );
                                cx.push_semantic(bmux_tui::semantic::SemanticRegion::new(
                                    action_id,
                                    region.area,
                                    "button",
                                ));
                            }
                        }
                        for image in self.transcript.images.get(id).into_iter().flatten() {
                            let mut image = image.clone();
                            // The host selection scope also isolates independently mounted views.
                            image.key = ImageKey::new(format!(
                                "{}:{}:{}",
                                self.selection_scope,
                                item.id().get(),
                                image.key.as_str()
                            ));
                            cx.push_image(ImageContribution::Present(image));
                        }
                    },
                );
            });
        }
    }
}

/// Dispatch every supported native semantic family.
///
/// Rich visual rows supplied by adapters supersede this truthful semantic fallback;
/// opaque payloads are never printed as though they were human-readable content.
#[must_use]
pub fn semantic_rows(
    item: &TranscriptItem,
    width: u16,
    markdown: &TranscriptMarkdownCache,
) -> TranscriptLayoutRows {
    semantic_rows_with_presentation(item, width, markdown, TranscriptPresentation::default())
}

/// Dispatch semantic fallbacks using renderer-owned styles.
#[must_use]
pub fn semantic_rows_with_presentation(
    item: &TranscriptItem,
    width: u16,
    markdown: &TranscriptMarkdownCache,
    presentation: TranscriptPresentation,
) -> TranscriptLayoutRows {
    if width == 0 {
        return TranscriptLayoutRows::Rendered(Vec::new());
    }
    if matches!(
        item.kind(),
        TranscriptItemKind::ToolContribution {
            placement: bcode_session_models::ToolContributionPlacement::Hidden,
            ..
        }
    ) {
        return TranscriptLayoutRows::Rendered(Vec::new());
    }
    let mut rows = Vec::new();
    let title = match item.kind() {
        TranscriptItemKind::ToolRequest {
            tool_name,
            status: Some(status),
            timing,
            ..
        } => format!(
            "{tool_name} · {}{}",
            super::tool_fallback::tool_status_label(*status),
            if timing.timed_out == Some(true) {
                " · timed out"
            } else {
                ""
            }
        ),
        _ => item.display_role(),
    };
    let uses_markdown = item.text_format() == TextFormat::Markdown
        && matches!(
            item.kind(),
            TranscriptItemKind::UserMessage
                | TranscriptItemKind::AssistantMessage
                | TranscriptItemKind::System
                | TranscriptItemKind::ReasoningMessage
        );
    if uses_markdown {
        let options = MarkdownRenderOptions::new(width.saturating_sub(2))
            .with_theme(presentation.markdown)
            .with_document_id(format!("transcript:{}", item.id().get()))
            .with_streaming(item.streaming())
            .with_link_destination_fallbacks(true)
            .with_details_open(BTreeMap::new());
        let projection = markdown.project(item, options);
        let body_start = append_markdown_block(
            &mut rows,
            &title,
            &projection,
            width,
            MarkdownBlockStyle {
                heading: presentation.heading,
                muted: presentation.muted,
                prominent: matches!(item.kind(), TranscriptItemKind::UserMessage),
                body_indent: 2,
            },
        );
        return TranscriptLayoutRows::Markdown {
            rows,
            anchors: Vec::new(),
            projection,
            body_start,
        };
    }
    plain_rows(item, width, title, presentation)
}

fn plain_rows(
    item: &TranscriptItem,
    width: u16,
    title: String,
    presentation: TranscriptPresentation,
) -> TranscriptLayoutRows {
    let mut rows = Vec::new();
    let heading = if item.streaming() {
        format!("{title} · streaming")
    } else {
        title
    };
    rows.extend(bmux_tui::text::wrap_line_bounded(
        &Line::from_spans(vec![bmux_tui::text::Span::styled(
            heading,
            presentation.heading,
        )]),
        bmux_tui::text::TextWrapGeometry::uniform(usize::from(width)),
        bmux_tui::text::TextWrap::Word,
    ));
    // BMUX owns both wrapping and source provenance. Prepare them once, never
    // infer byte offsets by searching the painted strings during selection.
    let plain_source = if matches!(
        item.kind(),
        TranscriptItemKind::ToolRequest {
            status: Some(bcode_session_view_models::ToolInvocationViewStatus::Requested),
            ..
        }
    ) {
        ""
    } else {
        item.text()
    };
    let block =
        bmux_tui::composition::TextBlock::new(plain_source).wrap(bmux_tui::text::TextWrap::Word);
    let layout = block.layout(
        Constraints::tight(bmux_tui::geometry::Size::new(width, 0)),
        &mut LayoutCx::new(),
    );
    let mut source_offsets = Vec::new();
    let (mut normalized_offset, mut source_offset) = (0usize, 0usize);
    for line in item.text().split_inclusive('\n') {
        source_offsets.push((normalized_offset, source_offset));
        source_offset += line.len();
        normalized_offset += line.len() - usize::from(line.ends_with("\r\n"));
    }
    let mut selection = BTreeMap::new();
    for projected in block.projection(&layout) {
        let row_index = rows.len();
        let text = projected.line.plain_text();
        let (normalized_start, source_start) = source_offsets
            .get(projected.source_line)
            .copied()
            .unwrap_or((normalized_offset, source_offset));
        let byte_start = source_start
            + projected
                .source_range
                .start
                .saturating_sub(normalized_start);
        let fragments = bmux_tui::selection::plain_text_fragments(
            "retained",
            "retained",
            bmux_tui::geometry::Rect::new(0, 0, width, 1),
            0,
            &text,
            byte_start,
            item.revision(),
        );
        selection.insert(
            row_index,
            bcode_plugin_models::tui_selection::PluginTuiSelectionRow {
                identity: format!("transcript:{}", item.id().get()),
                revision: item.revision(),
                byte_start,
                text,
                cells: fragments
                    .into_iter()
                    .map(
                        |fragment| bcode_plugin_models::tui_selection::PluginTuiSelectionCell {
                            column: fragment.area.x,
                            width: fragment.area.width,
                            bytes: fragment.source_range,
                        },
                    )
                    .collect(),
            },
        );
        rows.push(projected.line);
    }
    rows.push(Line::default());
    TranscriptLayoutRows::Selected {
        rows,
        anchors: Vec::new(),
        selection,
    }
}

#[cfg(test)]
#[path = "accepted/local_action_tests.rs"]
mod local_action_tests;

#[cfg(test)]
#[path = "accepted/action_tests.rs"]
mod action_tests;
#[cfg(test)]
#[path = "accepted/rich_tests.rs"]
mod rich_tests;
#[cfg(test)]
mod tests;

/// Bound semantic adaptation before cloning or parsing. Larger history is paged by
/// the host; it is never silently parsed as a truncated complete document.
#[must_use]
pub fn resident_window_supported(document: &TranscriptViewDocument) -> bool {
    use bcode_session_models::retention::{ObservationRetentionLimits, ObservationRetentionUsage};
    if document.items.len() > 512 {
        return false;
    }
    let limits = ObservationRetentionLimits {
        scalar_capacity_bytes: 256 * 1024,
        collection_entries: 16_384,
        value_nodes: 32_768,
        depth: 32,
        copies: 1,
    };
    let mut usage = ObservationRetentionUsage::default();
    document.items.iter().all(|item| {
        bcode_session_view_models::retention::inspect_transcript_item(item, limits)
            .and_then(|item_usage| usage.try_add(item_usage, limits))
            .is_ok()
    })
}

/// Validate bounded, item-local image allocations before retaining any bytes.
pub(super) fn images_supported(
    images: &[bmux_tui::image::ImagePlacement],
    width: u16,
    rows: usize,
) -> bool {
    use bmux_tui::image::{ImageLifecycle, ImagePayload, ImagePixelFormat};
    if images.len() > 32 {
        return false;
    }
    let mut keys = BTreeSet::new();
    let mut bytes = 0usize;
    images.iter().all(|image| {
        let (payload, w, h, valid) = match &image.payload {
            ImagePayload::Png {
                bytes,
                width,
                height,
            } => (bytes, *width, *height, true),
            ImagePayload::Pixels {
                bytes,
                width,
                height,
                format,
            } => {
                let channels = match format {
                    ImagePixelFormat::Rgb8 => 3usize,
                    ImagePixelFormat::Rgba8 => 4usize,
                };
                let expected = usize::try_from(*width)
                    .ok()
                    .and_then(|w| usize::try_from(*height).ok().and_then(|h| w.checked_mul(h)))
                    .and_then(|n| n.checked_mul(channels));
                (bytes, *width, *height, expected == Some(bytes.len()))
            }
        };
        bytes = bytes.saturating_add(payload.len());
        valid
            && w > 0
            && h > 0
            && w <= 4096
            && h <= 4096
            && !payload.is_empty()
            && bytes <= 8 * 1024 * 1024
            && !image.key.as_str().is_empty()
            && image.key.as_str().len() <= 512
            && keys.insert(image.key.clone())
            && image.lifecycle == ImageLifecycle::Frame
            && image.allocation.is_none()
            && !image.destination.is_empty()
            && image.destination.right() <= width
            && usize::from(image.destination.bottom()) <= rows
            && image.clip == image.destination
    })
}

pub(super) fn visual_supported(bundle: &TranscriptLayoutRows) -> bool {
    let (rows, anchors, selection) = match bundle {
        TranscriptLayoutRows::Rendered(rows) => (rows, &[][..], None),
        TranscriptLayoutRows::Anchored { rows, anchors }
        | TranscriptLayoutRows::Markdown { rows, anchors, .. } => (rows, anchors.as_slice(), None),
        TranscriptLayoutRows::Selected {
            rows,
            anchors,
            selection,
        } => (rows, anchors.as_slice(), Some(selection)),
        TranscriptLayoutRows::BlankSpan(height) => return *height <= 8192,
    };
    if rows.len() > 8192
        || bcode_plugin_models::tui_visual::validate_visual_anchors(anchors, rows.len()).is_err()
    {
        return false;
    }
    let bytes = rows
        .iter()
        .flat_map(|line| &line.spans)
        .try_fold(0usize, |sum, span| sum.checked_add(span.content.len()));
    if bytes.is_none_or(|bytes| bytes > 256 * 1024) {
        return false;
    }
    selection.is_none_or(|selection| {
        selection.keys().all(|row| *row < rows.len())
            && selection
                .values()
                .all(bcode_plugin_models::tui_selection::PluginTuiSelectionRow::is_valid)
            && selection.values().map(|row| row.text.len()).sum::<usize>() <= 256 * 1024
    })
}

#[cfg(test)]
mod replacement_revision_tests {
    use super::*;

    fn visual(revision: u64) -> AcceptedVisual {
        AcceptedVisual {
            item_revision: 1,
            width: 20,
            revision,
            rows: TranscriptLayoutRows::Rendered(vec![Line::raw("retained")]),
        }
    }

    #[test]
    fn replacement_revision_is_checked_and_fenced_by_retained_output() {
        let source = super::tests::document(TextFormat::PlainText, "source");
        let id = TranscriptViewItemId::new("fixture");
        let mut accepted = AcceptedTranscript::default();
        accepted.prepare(&source, 20);
        assert!(accepted.accept_visual(id.clone(), visual(500)));
        let token = accepted.visual_preparation(&id).unwrap();
        assert_eq!(accepted.replacement_visual_revision(&token), Some(501));
        assert!(accepted.accept_prepared_visual(&token, visual(501), vec![], vec![]));
        // A second completion from the same preparation cannot replace output
        // that arrived after it, even if it supplies a larger revision.
        assert_eq!(accepted.replacement_visual_revision(&token), None);
        assert!(!accepted.accept_prepared_visual(&token, visual(502), vec![], vec![]));
        assert_eq!(accepted.visuals[&id].revision, 501);
    }

    #[test]
    fn replacement_revision_rejects_stale_geometry_semantics_and_overflow() {
        let mut source = super::tests::document(TextFormat::PlainText, "source");
        let id = TranscriptViewItemId::new("fixture");
        let mut accepted = AcceptedTranscript::default();
        accepted.prepare(&source, 20);
        assert!(accepted.accept_visual(id.clone(), visual(u64::MAX)));
        let token = accepted.visual_preparation(&id).unwrap();
        assert_eq!(accepted.replacement_visual_revision(&token), None);
        assert_eq!(accepted.visuals[&id].revision, u64::MAX);
        accepted.prepare(&source, 19);
        assert_eq!(accepted.replacement_visual_revision(&token), None);
        assert!(!accepted.accept_prepared_visual(&token, visual(1), vec![], vec![]));
        let width_token = accepted.visual_preparation(&id).unwrap();
        source.revision += 1;
        source.items[0].revision += 1;
        accepted.prepare(&source, 19);
        assert_eq!(accepted.replacement_visual_revision(&width_token), None);
    }
}
