//! Unit producers (L4): pure functions from a unit and its [`ItemRenderContext`] to a retained
//! component plus its geometry.
//!
//! # Producer contract
//!
//! * A producer implements [`UnitProducer::produce`]: `fn(UnitView, &ItemRenderContext) ->
//!   ProducedItem`. It reads nothing else: no thread-locals, no statics, no clocks, no
//!   application state (L2, L4). Equal inputs produce equal output.
//! * [`ProducedItem::content`] is one owned BMUX component ([`ItemContent`]). The layout cache
//!   lays it out exactly once per `(unit, revision, fingerprint, width)` with
//!   `Constraints::for_width(cx.width)`, retains the resulting `LayoutNode`, and paints and
//!   routes events through that same node (B3, L6). Producers never measure text themselves
//!   (B1, B2): they compose BMUX primitives (`TextBlock`, `Surface`, `Padding`, `Column`, …)
//!   and the helpers in [`common`].
//! * Geometry (source map, focus targets, hit regions, selection fragments, content keys,
//!   paint slots) comes from [`ItemContent::geometry`], computed from the retained node only.
//! * Paint-only state (focus, selection, spinner, elapsed labels, boundary status) is never
//!   baked into the component (L5); reserve a [`crate::layout::PaintSlot`] instead.
//! * Report parse/highlight/projection work in [`ProducedItem::work`] so the layout work
//!   counters (9.2) observe it.
//! * Widths may be zero or tiny: degrade explicitly (drop chrome first, then clip), never
//!   panic (B4, B5).
//!
//! [`ProducerRegistry`] dispatches by [`ProducerKind`] with a renderer-neutral
//! [`FallbackProducer`] last (A8, E5).
//!
//! File ownership for content producers: [`message`] (user, assistant, reasoning, system and
//! note units) and [`markdown`] (Markdown bodies), [`tool`] (tools, permissions,
//! interactions), [`group`] (group headers, hidden markers), [`visual`] (accepted plugin
//! visuals). Shared helpers live in [`common`].

pub mod common;
pub mod group;
pub mod markdown;
pub mod message;
pub mod tool;
pub mod visual;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

use std::collections::BTreeMap;

use bcode_session_view_models::{TranscriptViewItem, TranscriptViewItemKind};
use bmux_tui::component::{Component, LayoutNode, SyncElement};
use bmux_tui::composition::TextBlock;

use super::context::ItemRenderContext;
use super::geometry::ItemGeometry;
use crate::units::{UnitEntry, UnitKind};

/// Everything a producer may read about one unit besides its render context.
#[derive(Debug, Clone, Copy)]
pub struct UnitView<'a> {
    /// The derived unit.
    pub unit: &'a UnitEntry,
    /// The unit's transcript item (the first item of the run for group headers and hidden
    /// markers); `None` for boundary rows.
    pub item: Option<&'a TranscriptViewItem>,
}

/// A retained item component: a BMUX [`Component`] that also derives the item's geometry from
/// its own retained layout.
///
/// Item content is `Send + Sync`, so the engine that retains it is `Send` and parts can be
/// shared through `Arc` between a composition and the geometry pass: compose retained trees from
/// thread-safe BMUX elements ([`SyncElement`] and containers over [`RetainedObject`]).
pub trait ItemContent: Component + Send + Sync {
    /// Per-item geometry read from `layout`, the node this component produced. Default: no
    /// mapped geometry (rows fall back to item-row anchors).
    fn geometry(&self, _layout: &LayoutNode) -> ItemGeometry {
        ItemGeometry::default()
    }
}

impl ItemContent for TextBlock {}

impl ItemContent for SyncElement<'static> {}

/// The BMUX element object retained item trees erase their children into.
pub type RetainedObject = dyn Component + Send + Sync;

/// Work a producer performed, folded into the layout work counters (9.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ProducerWork {
    /// Markdown source parses.
    pub markdown_parses: u64,
    /// Syntax highlighting runs.
    pub highlights: u64,
    /// Markdown projections built.
    pub projections: u64,
}

/// Output of one producer call.
pub struct ProducedItem {
    /// The retained component.
    pub content: Box<dyn ItemContent>,
    /// Work performed while producing.
    pub work: ProducerWork,
}

impl std::fmt::Debug for ProducedItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProducedItem")
            .field("work", &self.work)
            .finish_non_exhaustive()
    }
}

impl ProducedItem {
    /// Wrap one component.
    #[must_use]
    pub fn new(content: impl ItemContent + 'static) -> Self {
        Self {
            content: Box::new(content),
            work: ProducerWork::default(),
        }
    }

    /// Wrap an already boxed component.
    #[must_use]
    pub fn boxed(content: Box<dyn ItemContent>) -> Self {
        Self {
            content,
            work: ProducerWork::default(),
        }
    }

    /// Return the item with `work` recorded.
    #[must_use]
    pub const fn with_work(mut self, work: ProducerWork) -> Self {
        self.work = work;
        self
    }
}

/// Builds the retained component of one unit (L4).
///
/// Producers are `Send` so the registry, and the engine that owns it, are `Send`.
pub trait UnitProducer: Send {
    /// Component and geometry source for one unit. Pure in `unit` and `cx`.
    fn produce(&self, unit: UnitView<'_>, cx: &ItemRenderContext<'_>) -> ProducedItem;
}

impl<F> UnitProducer for F
where
    F: Fn(UnitView<'_>, &ItemRenderContext<'_>) -> ProducedItem + Send,
{
    fn produce(&self, unit: UnitView<'_>, cx: &ItemRenderContext<'_>) -> ProducedItem {
        self(unit, cx)
    }
}

/// Which producer renders a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProducerKind {
    /// Resident-window boundary row.
    Boundary,
    /// Group header.
    GroupHeader,
    /// Hidden-items count marker.
    HiddenMarker,
    /// User message.
    UserMessage,
    /// Assistant message.
    AssistantMessage,
    /// Reasoning (legacy message or structured activity).
    Reasoning,
    /// System or plugin status message.
    System,
    /// Compaction or skill note.
    Note,
    /// Tool invocation, historical request, request draft or contribution without an
    /// accepted plugin visual.
    Tool,
    /// Tool unit with an accepted plugin visual (L8).
    PluginVisual,
    /// Permission request.
    Permission,
    /// Interactive request.
    Interaction,
}

/// Which producer renders `unit` under `cx`.
#[must_use]
pub fn producer_kind(unit: UnitView<'_>, cx: &ItemRenderContext<'_>) -> ProducerKind {
    match unit.unit.kind {
        UnitKind::Boundary(_) => return ProducerKind::Boundary,
        UnitKind::GroupHeader { .. } => return ProducerKind::GroupHeader,
        UnitKind::HiddenMarker { .. } => return ProducerKind::HiddenMarker,
        UnitKind::Item => {}
    }
    let Some(item) = unit.item else {
        return ProducerKind::System;
    };
    match &item.kind {
        TranscriptViewItemKind::UserMessage { .. } => ProducerKind::UserMessage,
        TranscriptViewItemKind::AssistantMessage { .. } => ProducerKind::AssistantMessage,
        TranscriptViewItemKind::ReasoningMessage { .. }
        | TranscriptViewItemKind::ReasoningActivity { .. } => ProducerKind::Reasoning,
        TranscriptViewItemKind::SystemMessage { .. } => ProducerKind::System,
        TranscriptViewItemKind::Compaction { .. } | TranscriptViewItemKind::Skill { .. } => {
            ProducerKind::Note
        }
        TranscriptViewItemKind::ToolInvocation { .. }
        | TranscriptViewItemKind::ToolRequestDraft { .. }
        | TranscriptViewItemKind::ToolRequest { .. }
        | TranscriptViewItemKind::ToolContribution { .. } => {
            if cx.plugin_visuals.accepted(&item.id).is_some() {
                ProducerKind::PluginVisual
            } else {
                ProducerKind::Tool
            }
        }
        TranscriptViewItemKind::Permission { .. } => ProducerKind::Permission,
        TranscriptViewItemKind::Interaction { .. } => ProducerKind::Interaction,
    }
}

/// Renderer-neutral fallback (A8, E5): compact plain rows for any unit.
///
/// Message text is word-wrapped by BMUX; other items show a one-row kind label; boundary, header and marker
/// units use their one-row forms.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FallbackProducer;

impl UnitProducer for FallbackProducer {
    fn produce(&self, unit: UnitView<'_>, _cx: &ItemRenderContext<'_>) -> ProducedItem {
        ProducedItem::new(common::plain_unit(unit))
    }
}

/// Dispatches units to producers by [`ProducerKind`], with a fallback for unregistered kinds.
pub struct ProducerRegistry {
    by_kind: BTreeMap<ProducerKind, Box<dyn UnitProducer>>,
    fallback: Box<dyn UnitProducer>,
}

impl std::fmt::Debug for ProducerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProducerRegistry")
            .field("kinds", &self.by_kind.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl Default for ProducerRegistry {
    fn default() -> Self {
        Self::standard()
    }
}

impl ProducerRegistry {
    /// A registry that sends every unit to `fallback`.
    #[must_use]
    pub fn with_fallback(fallback: Box<dyn UnitProducer>) -> Self {
        Self {
            by_kind: BTreeMap::new(),
            fallback,
        }
    }

    /// The standard producers: boundary, group header and hidden marker ([`group`]), messages
    /// and notes ([`message`]), tools, permissions and interactions ([`tool`]) and plugin
    /// visuals ([`visual`]), with [`FallbackProducer`] last.
    #[must_use]
    pub fn standard() -> Self {
        Self::with_fallback(Box::new(FallbackProducer))
            .with(ProducerKind::Boundary, Box::new(common::BoundaryProducer))
            .with(
                ProducerKind::GroupHeader,
                Box::new(group::GroupHeaderProducer),
            )
            .with(
                ProducerKind::HiddenMarker,
                Box::new(group::HiddenMarkerProducer),
            )
            .with(
                ProducerKind::UserMessage,
                Box::new(message::UserMessageProducer),
            )
            .with(
                ProducerKind::AssistantMessage,
                Box::new(message::AssistantMessageProducer),
            )
            .with(
                ProducerKind::Reasoning,
                Box::new(message::ReasoningProducer),
            )
            .with(
                ProducerKind::System,
                Box::new(message::SystemMessageProducer),
            )
            .with(ProducerKind::Note, Box::new(message::NoteProducer))
            .with(ProducerKind::Tool, Box::new(tool::ToolProducer))
            .with(ProducerKind::Permission, Box::new(tool::PermissionProducer))
            .with(
                ProducerKind::Interaction,
                Box::new(tool::InteractionProducer),
            )
            .with(
                ProducerKind::PluginVisual,
                Box::new(visual::PluginVisualProducer),
            )
    }

    /// Return the registry with `producer` handling `kind`.
    #[must_use]
    pub fn with(mut self, kind: ProducerKind, producer: Box<dyn UnitProducer>) -> Self {
        self.by_kind.insert(kind, producer);
        self
    }
}

impl UnitProducer for ProducerRegistry {
    fn produce(&self, unit: UnitView<'_>, cx: &ItemRenderContext<'_>) -> ProducedItem {
        // D10: a unit holding the inline interaction surface renders only the blank rows the
        // host paints the surface over.
        if let crate::units::structure::ResolvedUnitPresentation::Item(item) = cx.structure
            && let Some(rows) = item.reserved_rows
        {
            return ProducedItem::new(reserved_rows(rows));
        }
        self.by_kind
            .get(&producer_kind(unit, cx))
            .unwrap_or(&self.fallback)
            .produce(unit, cx)
    }
}

/// `rows` blank rows (at least one) reserved for a surface the host paints over the unit.
fn reserved_rows(rows: u32) -> TextBlock {
    let rows = usize::try_from(rows.max(1)).unwrap_or(usize::MAX);
    TextBlock::new(bmux_tui::text::Text::from_lines(vec![
        bmux_tui::text::Line::default();
        rows
    ]))
}
