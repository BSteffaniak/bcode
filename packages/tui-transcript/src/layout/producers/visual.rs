//! Plugin visual producer (L8, A8, E1–E6), rendered-row components and content targets.
//!
//! Plugin visual adapters run host-side, outside drawing (D9): the host resolves the adapter
//! (E1), renders it with the unit's resolved presentation (E6), validates and bounds the rows,
//! and accepts them into [`crate::layout::PluginVisualState`] for an exact item revision,
//! together with everything else the adapter returned ([`AcceptedPluginVisual`]): its render
//! mode, header title and timeout, operation target, one-line summary, named sections,
//! anchors and selection rows. This producer only consumes accepted visuals:
//!
//! * With an accepted visual, the unit renders through the tool anatomy
//!   ([`super::tool::compose`]) the way the plugin contract asks for it:
//!   * The host header shows lifecycle, timing and the disclosure marker, with the adapter's
//!     target (falling back to the unit's `target` fact) and title, and the adapter's timeout
//!     in the timing phrase. Transcript-block visuals replace the label, tool and status with
//!     the adapter title (or the host label). Full-block visuals draw their own header: while
//!     expanded the host header is omitted and the first adapter row is the unit's focus
//!     target.
//!   * A `visual` section holds the adapter rows (omitted when the adapter returned only
//!     named sections), then each named section follows in adapter order under its label (or
//!     its leaf name), then the host `error` section when the call failed. Sections are
//!     bounded by their previews and clipped at the body width (rows are never reflowed).
//!   * The producer summary is the unit's `plugin` summary source.
//! * Without one (adapter missing, disabled, failed or not yet accepted), the unit renders the
//!   renderer-neutral canonical tool fallback of [`super::tool::ToolProducer`] (A8, E1
//!   "canonical fallback last", E5).
//!
//! Adapter rows ([`VisualRows::plugin`] with a [`VisualCorrespondence`]) map to the adapter's
//! own coordinates, read from the same retained rows that paint them:
//!
//! * Anchors (E3): a version 4 source range maps its row to
//!   [`SourcePosition::PluginSource`] at the range start; a version 3 content key maps its row
//!   and the unmapped rows below it to the key with the row offset. Rows without either fall
//!   back to item rows (versions 1–2). Reflow resolves a retained position inside its own
//!   identity, by containment.
//! * Selection (C34, C36): rows with adapter selection metadata copy the adapter's source text
//!   (shell formatter output copies the original command) through one indivisible fragment
//!   per adapter cell; contiguous rows of one identity join without a newline (soft wraps) and
//!   an identity change starts a new line. Cells clipped at the body width are not selectable.
//!   Rows without metadata copy their painted text (ambiguity 10).
//! * Content targets: rows carrying an identity are [`ContentTargetKind::PluginVisual`]
//!   targets, so the host routes horizontal wheel input, right-button presses and the focused
//!   visual's keys to the adapter, as V1 did.
//!
//! The canonical shell screen ([`super::tool::SectionBody::Output`] with a shell key) is a
//! [`ContentTargetKind::ShellScreen`] target that scrolls horizontally on its own (C39): the
//! host keeps the column offset per item ([`ShellScroll`]) and the producer clamps it to the
//! widest shown line.
//!
//! [`SourcePosition::PluginSource`]: crate::layout::SourcePosition::PluginSource

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;

use bcode_session_view_models::{
    ToolTimingView, TranscriptViewItem, TranscriptViewItemId, TranscriptViewItemKind,
};
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutNode, LogicalRect,
};
use bmux_tui::composition::TextBlock;
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::paint::PaintCx;
use bmux_tui::text::Line;

use super::tool::{self, Section, SectionBody};
use super::{ItemContent, ItemRenderContext, ProducedItem, UnitProducer, UnitView, common};
use crate::layout::SourcePosition;
use crate::layout::context::AcceptedPluginVisual;
use crate::layout::geometry::{
    ContentKeyRegion, ItemFocusTarget, ItemGeometry, ItemSelectionFragment, SelectableRun,
    SelectableRunKind, SelectableText, SourceRow, SourceRows,
};
use crate::units::structure::DisclosureTarget;

// ---- accepted visual parts ---------------------------------------------------------------

/// How an adapter's rows sit in the host transcript chrome (plugin render mode).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum VisualRenderMode {
    /// Rows inside the host header and chrome.
    #[default]
    Inline,
    /// Rows inside the host chrome under the adapter's title.
    TranscriptBlock,
    /// The rows draw their own header; the host header shows only while not expanded.
    FullBlock,
}

/// One named adapter section (plugin contract v3), already bounded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VisualSection {
    /// Host or namespaced section name: the key for presentation and disclosure targets.
    pub name: String,
    /// Label; `None` shows the name's last `.`-separated part.
    pub label: Option<String>,
    /// Rows.
    pub rows: Arc<[Line]>,
}

/// Version 4 half-open source interval of one adapter row, producer-scoped by the host.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VisualSourceRange {
    /// Producer-scoped identity (`<plugin>:<identity>`).
    pub identity: String,
    /// Range start in adapter coordinates.
    pub start: u64,
    /// Range end in adapter coordinates.
    pub end: u64,
}

/// One adapter anchor (E3): a content key on a row, optionally with a version 4 source range.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VisualAnchor {
    /// Adapter row.
    pub row: usize,
    /// Producer-scoped content key (`<plugin>:<key>`).
    pub key: String,
    /// Version 4 source range of the row.
    pub source: Option<VisualSourceRange>,
}

/// One adapter cell of a selection row: its columns and the source bytes it shows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VisualSelectionCell {
    /// First column inside the row.
    pub column: u16,
    /// Width in cells (at least one).
    pub width: u16,
    /// Source bytes in the identity's coordinates.
    pub bytes: Range<usize>,
}

/// Source text and cell geometry of one adapter row (validated by the host).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VisualSelectionRow {
    /// Producer-scoped source identity; rows of one identity are one logical line.
    pub identity: String,
    /// Byte offset of `text` in the identity's source.
    pub byte_start: usize,
    /// Source text the row shows.
    pub text: String,
    /// Cells in column order.
    pub cells: Vec<VisualSelectionCell>,
}

/// Correspondence between adapter rows and the adapter's source (E3, C34): anchors in row
/// order and selection rows by adapter row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct VisualCorrespondence {
    anchors: Vec<VisualAnchor>,
    selection: BTreeMap<usize, VisualSelectionRow>,
}

impl VisualCorrespondence {
    /// Correspondence of an adapter visual with `rows` rows: anchors and selection rows outside
    /// the rows are dropped; anchors are kept in row order (ties keep their order).
    #[must_use]
    pub fn new(
        mut anchors: Vec<VisualAnchor>,
        selection: impl IntoIterator<Item = (usize, VisualSelectionRow)>,
        rows: usize,
    ) -> Self {
        anchors.retain(|anchor| anchor.row < rows);
        anchors.sort_by_key(|anchor| anchor.row);
        Self {
            anchors,
            selection: selection
                .into_iter()
                .filter(|(row, _)| *row < rows)
                .collect(),
        }
    }

    /// Whether nothing corresponds (rows copy their painted text and anchor at item level).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.anchors.is_empty() && self.selection.is_empty()
    }

    /// Anchors in row order.
    #[must_use]
    pub fn anchors(&self) -> &[VisualAnchor] {
        &self.anchors
    }

    /// Selection row of adapter `row`, if any.
    #[must_use]
    pub fn selection(&self, row: usize) -> Option<&VisualSelectionRow> {
        self.selection.get(&row)
    }

    /// Producer-scoped identity and offset of adapter `row`, as V1 resolved content anchors:
    /// a source range describes only its own row (rows below it are unmapped), a content key
    /// covers its row and the rows below it until the next anchor (offset = rows below it).
    #[must_use]
    pub fn position(&self, row: usize) -> Option<(&str, u64)> {
        let end = self.anchors.partition_point(|anchor| anchor.row <= row);
        let anchor = self.anchors[..end].last()?;
        anchor.source.as_ref().map_or_else(
            || {
                Some((
                    anchor.key.as_str(),
                    u64::try_from(row - anchor.row).unwrap_or(u64::MAX),
                ))
            },
            |source| (anchor.row == row).then_some((source.identity.as_str(), source.start)),
        )
    }
}

/// Horizontal column offsets of canonical shell screens, per item (C39). Host-owned view
/// state; an item's offset is part of its unit's layout fingerprint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellScroll {
    columns: BTreeMap<TranscriptViewItemId, u64>,
}

impl ShellScroll {
    /// Columns `item`'s shell screen is scrolled by (`0` when unscrolled).
    #[must_use]
    pub fn columns(&self, item: &TranscriptViewItemId) -> u64 {
        self.columns.get(item).copied().unwrap_or(0)
    }

    /// Whether no shell screen is scrolled.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Scroll `item`'s shell screen to `columns`; `0` removes the entry. Returns whether it
    /// changed.
    pub fn set(&mut self, item: TranscriptViewItemId, columns: u64) -> bool {
        if columns == 0 {
            return self.columns.remove(&item).is_some();
        }
        self.columns.insert(item, columns) != Some(columns)
    }

    /// Keep only offsets of items `keep` accepts (residency reconciliation). Returns whether
    /// any offset was dropped.
    pub fn retain(&mut self, keep: impl Fn(&TranscriptViewItemId) -> bool) -> bool {
        let before = self.columns.len();
        self.columns.retain(|id, _| keep(id));
        self.columns.len() != before
    }
}

// ---- content targets ---------------------------------------------------------------------

/// What a content target routes input to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ContentTargetKind {
    /// Plugin visual rows: horizontal wheel input, right-button presses and the focused
    /// visual's keys go to the adapter under this producer-scoped identity (E1).
    PluginVisual {
        /// Producer-scoped content identity (`<plugin>:<identity>`).
        identity: String,
    },
    /// A canonical shell screen scrolling horizontally on its own (C39).
    ShellScreen {
        /// Columns currently scrolled (already clamped).
        columns: u64,
        /// Most columns the widest shown line can scroll.
        max_columns: u64,
    },
}

/// Item-local cells whose pointer and key input belong to their content.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContentTarget {
    /// Region in item-local cells (the content's own columns, one or more rows).
    pub rect: LogicalRect,
    /// What the region routes to.
    pub kind: ContentTargetKind,
}

// ---- rendered rows -----------------------------------------------------------------------

/// Plugin metadata of a [`VisualRows`] block: the visual's correspondence and the adapter row
/// of the block's first row.
#[derive(Debug, Clone)]
struct PluginRows {
    correspondence: Arc<VisualCorrespondence>,
    base: usize,
}

/// Rows copied as rendered text (adapter rows, argument fields, metadata).
///
/// They are one rendered selection fragment, optionally one content key region over the same
/// rows. Plugin rows ([`Self::plugin`]) additionally carry the adapter's anchors, selection text and
/// content targets.
#[derive(Debug, Clone)]
pub struct VisualRows {
    block: TextBlock,
    key: Option<String>,
    plugin: Option<PluginRows>,
}

impl VisualRows {
    /// Rendered rows of `block`.
    #[must_use]
    pub const fn new(block: TextBlock) -> Self {
        Self {
            block,
            key: None,
            plugin: None,
        }
    }

    /// Return the rows covered by the producer-namespaced content `key` (E3).
    #[must_use]
    pub fn keyed(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// Return the rows as adapter rows starting at adapter row `base` of a visual with
    /// `correspondence`: anchored, selectable and routed as the module documentation describes.
    #[must_use]
    pub fn plugin(mut self, correspondence: Arc<VisualCorrespondence>, base: usize) -> Self {
        self.plugin = Some(PluginRows {
            correspondence,
            base,
        });
        self
    }
}

impl Component for VisualRows {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.block.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.block.paint(layout, cx);
    }

    fn revision(&self) -> ComponentRevision {
        self.block.revision()
    }
}

impl ItemContent for VisualRows {
    fn geometry(&self, layout: &LayoutNode) -> ItemGeometry {
        let height = layout.size.height;
        if height == 0 {
            return ItemGeometry::default();
        }
        let mut geometry = ItemGeometry {
            selection_fragments: vec![ItemSelectionFragment {
                rows: 0..height,
                source: None,
                rendered: true,
            }],
            content_keys: self
                .key
                .iter()
                .map(|key| ContentKeyRegion {
                    key: key.clone(),
                    rows: 0..height,
                })
                .collect(),
            ..ItemGeometry::default()
        };
        if let Some(plugin) = &self.plugin {
            plugin.geometry(&self.block, layout, &mut geometry);
        }
        geometry
    }
}

/// Builder of one selectable part of plugin rows.
enum PartBuilder {
    /// Rows without selection metadata: their painted text, newline-separated.
    Painted(String, Vec<SelectableRun>),
    /// Contiguous rows of one source identity: their source text from `base`.
    Source {
        identity: String,
        base: usize,
        text: String,
        runs: Vec<SelectableRun>,
    },
}

impl PartBuilder {
    fn finish(self) -> Option<SelectableText> {
        let (text, runs) = match self {
            Self::Painted(text, runs) | Self::Source { text, runs, .. } => (text, runs),
        };
        (!runs.is_empty()).then(|| SelectableText::new(Arc::from(text), runs))
    }
}

impl PluginRows {
    /// Source map, selectable parts and content targets of the block's measured rows (one per
    /// line: adapter rows are never reflowed), read from the retained layout.
    fn geometry(&self, block: &TextBlock, layout: &LayoutNode, geometry: &mut ItemGeometry) {
        let width = layout.size.width;
        let lines = &block.text().lines;
        let rows = usize::try_from(layout.size.height)
            .unwrap_or(usize::MAX)
            .min(lines.len());
        let mut source_rows = Vec::new();
        let mut targets: Vec<(Range<u64>, String)> = Vec::new();
        for local in 0..rows {
            let row = u64::try_from(local).unwrap_or(u64::MAX);
            let Some((identity, offset)) = self.correspondence.position(self.base + local) else {
                continue;
            };
            source_rows.push(SourceRow {
                row,
                position: SourcePosition::PluginSource {
                    identity: identity.to_owned(),
                    offset,
                },
            });
            match targets.last_mut() {
                Some((rows, last)) if last == identity && rows.end == row => rows.end += 1,
                _ => targets.push((row..row + 1, identity.to_owned())),
            }
        }
        geometry.source_map = SourceRows::new(source_rows);
        if width > 0 {
            geometry.content_targets = targets
                .into_iter()
                .map(|(rows, identity)| ContentTarget {
                    rect: LogicalRect::new(0, rows.start, width, rows.end - rows.start),
                    kind: ContentTargetKind::PluginVisual { identity },
                })
                .collect();
        }
        geometry.selectable = self.selectable(lines, rows, width);
    }

    /// Selectable parts in row order (see the module documentation).
    fn selectable(&self, lines: &[Line], rows: usize, width: u64) -> Vec<SelectableText> {
        let mut parts = Vec::new();
        let mut current: Option<PartBuilder> = None;
        for (local, line) in lines.iter().enumerate().take(rows) {
            let row = u64::try_from(local).unwrap_or(u64::MAX);
            if let Some(selection) = self.correspondence.selection(self.base + local) {
                let continues = matches!(
                    &current,
                    Some(PartBuilder::Source { identity, base, text, .. })
                        if *identity == selection.identity
                            && base.checked_add(text.len()) == Some(selection.byte_start)
                );
                if !continues {
                    parts.extend(current.take().and_then(PartBuilder::finish));
                    current = Some(PartBuilder::Source {
                        identity: selection.identity.clone(),
                        base: selection.byte_start,
                        text: String::new(),
                        runs: Vec::new(),
                    });
                }
                if let Some(PartBuilder::Source {
                    base, text, runs, ..
                }) = current.as_mut()
                {
                    text.push_str(&selection.text);
                    let base = *base;
                    runs.extend(selection.cells.iter().filter_map(|cell| {
                        let x = u64::from(cell.column);
                        let cells = u64::from(cell.width);
                        // Cells clipped at the body width are not selectable.
                        if cells == 0 || x + cells > width {
                            return None;
                        }
                        Some(SelectableRun {
                            row,
                            x,
                            width: cells,
                            source: cell.bytes.start.checked_sub(base)?
                                ..cell.bytes.end.checked_sub(base)?,
                            kind: SelectableRunKind::Unit,
                        })
                    }));
                }
                continue;
            }
            if !matches!(current, Some(PartBuilder::Painted(..))) {
                parts.extend(current.take().and_then(PartBuilder::finish));
                current = Some(PartBuilder::Painted(String::new(), Vec::new()));
            }
            if let Some(PartBuilder::Painted(text, runs)) = current.as_mut() {
                if !text.is_empty() || !runs.is_empty() {
                    text.push('\n');
                }
                let painted = line.plain_text();
                let start = text.len();
                text.push_str(&painted);
                if !painted.is_empty() && width > 0 {
                    runs.push(SelectableRun {
                        row,
                        x: 0,
                        width,
                        source: start..text.len(),
                        kind: SelectableRunKind::Text,
                    });
                }
            }
        }
        parts.extend(current.and_then(PartBuilder::finish));
        parts
    }
}

/// Any item content additionally covered by one producer-namespaced content key region (E3)
/// and, optionally, one content target over the same cells.
pub struct ContentKeyed {
    inner: Box<dyn ItemContent>,
    key: String,
    target: Option<ContentTargetKind>,
}

impl std::fmt::Debug for ContentKeyed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentKeyed")
            .field("key", &self.key)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl ContentKeyed {
    /// `inner` covered by `key`.
    #[must_use]
    pub fn new(inner: Box<dyn ItemContent>, key: impl Into<String>) -> Self {
        Self {
            inner,
            key: key.into(),
            target: None,
        }
    }

    /// Return the content as a content target of `kind` over all its cells.
    #[must_use]
    pub fn with_target(mut self, kind: ContentTargetKind) -> Self {
        self.target = Some(kind);
        self
    }
}

impl Component for ContentKeyed {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.inner.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.inner.paint(layout, cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.inner.event(event, layout, cx)
    }

    fn revision(&self) -> ComponentRevision {
        self.inner.revision()
    }
}

impl ItemContent for ContentKeyed {
    fn geometry(&self, layout: &LayoutNode) -> ItemGeometry {
        let mut geometry = self.inner.geometry(layout);
        let size = layout.size;
        if size.height > 0 {
            geometry.content_keys.push(ContentKeyRegion {
                key: self.key.clone(),
                rows: 0..size.height,
            });
            if let Some(kind) = self.target.clone().filter(|_| size.width > 0) {
                geometry.content_targets.push(ContentTarget {
                    rect: LogicalRect::new(0, 0, size.width, size.height),
                    kind,
                });
            }
        }
        geometry
    }
}

/// Content whose first row is the unit's own focus target.
///
/// Full-block visuals draw their own header, so their first row stands in for the omitted host
/// header (FO7). Nested focus targets without a parent become children of it.
pub struct UnitTargetRow {
    inner: Box<dyn ItemContent>,
    target: DisclosureTarget,
}

impl std::fmt::Debug for UnitTargetRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnitTargetRow")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl UnitTargetRow {
    /// `inner` with its first row focusing `target`.
    #[must_use]
    pub fn new(inner: Box<dyn ItemContent>, target: DisclosureTarget) -> Self {
        Self { inner, target }
    }
}

impl Component for UnitTargetRow {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.inner.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.inner.paint(layout, cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.inner.event(event, layout, cx)
    }

    fn revision(&self) -> ComponentRevision {
        self.inner.revision()
    }
}

impl ItemContent for UnitTargetRow {
    fn geometry(&self, layout: &LayoutNode) -> ItemGeometry {
        let mut geometry = self.inner.geometry(layout);
        if layout.size.height == 0 {
            return geometry;
        }
        let nested = std::mem::take(&mut geometry.focus_targets);
        geometry.focus_targets = std::iter::once(ItemFocusTarget {
            local: 0,
            rows: 0..1,
            parent: None,
            disclosure: Some(self.target.clone()),
        })
        .chain(nested.into_iter().map(|target| ItemFocusTarget {
            local: target.local.saturating_add(1),
            parent: Some(target.parent.map_or(0, |parent| parent.saturating_add(1))),
            ..target
        }))
        .collect();
        geometry
    }
}

/// Content key of an adapter's visual rows (namespaced by the adapter, E3).
#[must_use]
pub fn visual_key(adapter: &str) -> String {
    format!("{adapter}:visual")
}

/// Timing facts of a tool-like item (invocations and contributions with one).
fn item_timing(item: &TranscriptViewItem) -> ToolTimingView {
    match &item.kind {
        TranscriptViewItemKind::ToolInvocation { tool }
        | TranscriptViewItemKind::ToolRequest { tool }
        | TranscriptViewItemKind::ToolContribution {
            invocation: Some(tool),
            ..
        } => tool.timing.clone(),
        _ => ToolTimingView::default(),
    }
}

/// The anatomy sections of an accepted visual: its rows as the `visual` section (omitted when
/// empty and the adapter returned named sections), then the named sections.
fn visual_sections(visual: &AcceptedPluginVisual) -> Vec<Section> {
    let mut sections = Vec::with_capacity(visual.sections.len().saturating_add(1));
    if visual.sections.is_empty() || !visual.rows.is_empty() {
        sections.push(Section {
            name: "visual".to_owned(),
            label: None,
            body: SectionBody::Rows {
                rows: Arc::clone(&visual.rows),
                key: visual_key(&visual.adapter),
                correspondence: Arc::clone(&visual.correspondence),
            },
        });
    }
    sections.extend(visual.sections.iter().map(|section| Section {
        // Unlabelled namespaced sections (`bcode.git.files`) show their leaf name.
        label: Some(section.label.clone().unwrap_or_else(|| {
            section
                .name
                .rsplit('.')
                .next()
                .unwrap_or(&section.name)
                .to_owned()
        })),
        name: section.name.clone(),
        body: SectionBody::Rows {
            rows: Arc::clone(&section.rows),
            key: format!("{}:{}", visual.adapter, section.name),
            correspondence: Arc::default(),
        },
    }));
    sections
}

/// Tool units with an accepted plugin visual; the canonical tool fallback otherwise.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PluginVisualProducer;

impl UnitProducer for PluginVisualProducer {
    fn produce(&self, unit: UnitView<'_>, cx: &ItemRenderContext<'_>) -> ProducedItem {
        let Some(item) = unit.item else {
            return ProducedItem::new(common::plain_unit(unit));
        };
        let Some(visual) = cx.plugin_visuals.accepted(&item.id) else {
            return tool::ToolProducer.produce(unit, cx);
        };
        let mut anatomy = tool::tool_anatomy(item, tool::unit_facts(cx));
        // The adapter renders the call's arguments and result; the host keeps lifecycle chrome
        // and the failure notice.
        anatomy.sections.retain(|section| section.name == "error");
        anatomy.sections.splice(0..0, visual_sections(visual));
        let header = &mut anatomy.header;
        if visual.target.is_some() {
            header.target.clone_from(&visual.target);
        }
        header.title.clone_from(&visual.title);
        if let Some(timeout_ms) = visual.timeout_ms {
            let mut timing = item_timing(item);
            timing.timeout_ms = Some(timeout_ms);
            header.timing = tool::timing_text(&timing, header.active());
        }
        match visual.mode {
            VisualRenderMode::Inline => {}
            VisualRenderMode::TranscriptBlock => {
                header.title_override = visual
                    .title
                    .clone()
                    .or_else(|| visual.fallback_title.clone())
                    .or_else(|| header.label.clone());
            }
            VisualRenderMode::FullBlock => header.self_drawn = true,
        }
        if visual.summary.is_some() {
            anatomy.summary.clone_from(&visual.summary);
        }
        tool::compose(anatomy, &item.id, cx)
    }
}

#[cfg(test)]
mod tests;
