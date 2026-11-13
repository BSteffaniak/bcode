//! Group header and hidden-marker producers (structure v2: U2, A16, X1, X3), plus the
//! disclosure row and item stack that the tool and visual producers compose.
//!
//! * [`DisclosureRow`] is a one-row header that is a disclosure focus target and toggle hit
//!   region over its whole retained row (B3, B12, X1), optionally reserving activity paint
//!   slots (spinner, elapsed label) at its right edge (L5).
//! * [`ItemStack`] stacks item contents vertically in one BMUX `Column` and merges their
//!   geometry, renumbering focus targets and nesting them under the first one (a unit header
//!   is the parent of its section labels, B12).
//! * [`GroupHeaderProducer`] renders the policy header fields of a group (`marker`, `count`,
//!   `tools`, `tool`, `status`, `timing`) on one row that toggles the group. Members render as
//!   their own units (collapsed groups reveal members in place, summarized groups show member
//!   header rows, expanded groups show every member), so the header unit is the same at every
//!   disclosure level except for its marker and spacing.
//! * [`HiddenMarkerProducer`] renders `▸ N hidden` (A16).

use std::collections::BTreeMap;

use bcode_presentation_policy::models::{DEFAULT_GROUP_HEADER, Disclosure, Spacing, UnitStatus};
use bcode_tui_components::transcript::{
    TranscriptContainerBorder, TranscriptContainerLayout, TranscriptContainerRecipe,
    TranscriptContainerStyle, TranscriptContainerWidth,
};
use bcode_tui_theme::chrome::SurfaceStyle;
use bcode_tui_theme::definition::ResolvedThemeDefinition;
use bmux_tui::chrome::BorderSet;
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutNode, LogicalRect,
};
use bmux_tui::composition::{Column, TextBlock};
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::paint::PaintCx;
use bmux_tui::style::Style;
use bmux_tui::text::{Line, Span};

use super::common::{self, HeaderSegment, SharedContent};
use super::{ItemContent, ItemRenderContext, ProducedItem, RetainedObject, UnitProducer, UnitView};
use crate::layout::geometry::{
    ItemAction, ItemFocusTarget, ItemGeometry, ItemHitRegion, PaintSlot, PaintSlotKind, SourceRow,
    SourceRows,
};
use crate::units::UnitKind;
use crate::units::structure::{DisclosureTarget, GroupStructure, ResolvedUnitPresentation};

/// Cells reserved for an elapsed label (`59m59s` plus one spare cell).
pub const ELAPSED_CELLS: u64 = 7;
/// Most distinct tools the group `tools` field names before eliding the rest.
pub const MAX_HEADER_TOOLS: usize = 6;
/// Narrowest text column kept beside activity slots; narrower rows drop the slots (B4).
const MIN_TEXT_CELLS: u64 = 4;

// ---- theme helpers -----------------------------------------------------------------------

/// Theme style of `role`, or the default style when the theme does not define it.
#[must_use]
pub fn role_style(theme: &ResolvedThemeDefinition, role: &str) -> Style {
    theme.style(role).unwrap_or_default()
}

/// Title style role of a tool or group status (`tool.<status>.title`); errors use the failed
/// title.
#[must_use]
pub fn status_title_role(status: Option<UnitStatus>, is_error: bool) -> String {
    let status = if is_error && matches!(status, None | Some(UnitStatus::Succeeded)) {
        Some(UnitStatus::Failed)
    } else {
        status
    };
    format!("{}.title", common::tool_container_role(status))
}

/// Short status text (`ok`, `running`, `timed out`, …).
#[must_use]
pub const fn status_text(status: UnitStatus) -> &'static str {
    match status {
        UnitStatus::Requested => "requested",
        UnitStatus::Running => "running",
        UnitStatus::Waiting => "waiting",
        UnitStatus::Succeeded => "ok",
        UnitStatus::Failed => "failed",
        UnitStatus::Cancelled => "cancelled",
        UnitStatus::TimedOut => "timed out",
    }
}

/// Container style of a raised surface (chrome `surfaces = "layered"`), when the theme
/// defines `surface.raised`: a borderless panel that fills the unit's rows.
#[must_use]
pub fn raised_surface(theme: &ResolvedThemeDefinition) -> Option<TranscriptContainerStyle> {
    if theme.chrome.surfaces != SurfaceStyle::Layered {
        return None;
    }
    let style = theme.style("surface.raised")?;
    Some(TranscriptContainerStyle {
        recipe: TranscriptContainerRecipe {
            layout: TranscriptContainerLayout::Panel,
            width: TranscriptContainerWidth::Full,
            border: TranscriptContainerBorder::None,
            padding_x: 0,
            padding_y: 0,
        },
        style,
        border_set: BorderSet::SINGLE,
    })
}

/// Indent actually applied at `width`: never more than half the width, so narrow layouts keep
/// a content column (B4).
#[must_use]
pub fn effective_indent(indent: u8, width: u64) -> u8 {
    u8::try_from((width / 2).min(u64::from(indent))).unwrap_or(indent)
}

/// A header line with a leading disclosure marker prefix and the remaining segments fitted
/// into what is left of `width` (cells, measured by BMUX).
#[must_use]
pub fn marker_header_line(
    marker: Option<(&str, Style)>,
    segments: Vec<HeaderSegment>,
    width: u64,
) -> Line {
    let mut spans = Vec::new();
    let mut remaining = width;
    if let Some((marker, style)) = marker {
        let prefix = Span::styled(format!("{marker} "), style);
        let prefix_width =
            u64::try_from(Line::from_spans(vec![prefix.clone()]).width()).unwrap_or(u64::MAX);
        if prefix_width < width {
            remaining = width - prefix_width;
            spans.push(prefix);
        }
    }
    spans.extend(common::header_line(segments, " · ", remaining).spans);
    Line::from_spans(spans)
}

// ---- disclosure row ----------------------------------------------------------------------

/// Paint-only activity slots reserved at the right edge of a header row (L5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ActivitySlots {
    /// Reserve a one-cell spinner slot.
    pub spinner: bool,
    /// Reserve an elapsed-label slot counting from this wall-clock start.
    pub elapsed_from: Option<u64>,
}

impl ActivitySlots {
    /// No slots.
    pub const NONE: Self = Self {
        spinner: false,
        elapsed_from: None,
    };

    const fn wanted_cells(self) -> u64 {
        let spinner = if self.spinner { 2 } else { 0 };
        let elapsed = if self.elapsed_from.is_some() {
            ELAPSED_CELLS + 1
        } else {
            0
        };
        spinner + elapsed
    }

    /// Cells reserved at the right edge of a row `width` cells wide: zero when the slots would
    /// leave less than a minimal text column (explicit narrow degradation, B4). Producers fit
    /// header text into `width - reserved_cells(width)`, and slot rectangles derive from the
    /// same function over the retained row width.
    #[must_use]
    pub const fn reserved_cells(self, width: u64) -> u64 {
        let wanted = self.wanted_cells();
        if wanted == 0 || width < wanted + MIN_TEXT_CELLS {
            0
        } else {
            wanted
        }
    }

    fn slots(self, width: u64) -> Vec<PaintSlot> {
        let reserved = self.reserved_cells(width);
        if reserved == 0 {
            return Vec::new();
        }
        let mut slots = Vec::new();
        let mut x = width - reserved;
        if self.spinner {
            slots.push(PaintSlot {
                rect: LogicalRect::new(x + 1, 0, 1, 1),
                kind: PaintSlotKind::Spinner,
            });
            x += 2;
        }
        if let Some(started_at_ms) = self.elapsed_from {
            slots.push(PaintSlot {
                rect: LogicalRect::new(x + 1, 0, ELAPSED_CELLS, 1),
                kind: PaintSlotKind::Elapsed { started_at_ms },
            });
        }
        slots
    }
}

/// A one-row header that is a disclosure focus target and toggle hit region over its whole
/// retained row (B3, B12, X1).
#[derive(Debug, Clone)]
pub struct DisclosureRow {
    row: TextBlock,
    target: Option<DisclosureTarget>,
    interactive: bool,
    activity: ActivitySlots,
}

impl DisclosureRow {
    /// A row toggling `target` (no interaction when `None`).
    #[must_use]
    pub const fn new(row: TextBlock, target: Option<DisclosureTarget>) -> Self {
        Self {
            row,
            target,
            interactive: true,
            activity: ActivitySlots::NONE,
        }
    }

    /// Return the row with its toggle hit region enabled or disabled; the row stays a focus
    /// target (units awaiting a decision cannot collapse, A16).
    #[must_use]
    pub const fn interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }

    /// Return the row reserving `activity` slots at its right edge.
    #[must_use]
    pub const fn with_activity(mut self, activity: ActivitySlots) -> Self {
        self.activity = activity;
        self
    }
}

impl Component for DisclosureRow {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.row.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.row.paint(layout, cx);
    }

    fn revision(&self) -> ComponentRevision {
        self.row.revision()
    }
}

impl ItemContent for DisclosureRow {
    fn geometry(&self, layout: &LayoutNode) -> ItemGeometry {
        let size = layout.size;
        let paint_slots = if size.height == 0 {
            Vec::new()
        } else {
            self.activity.slots(size.width)
        };
        let Some(target) = &self.target else {
            return ItemGeometry {
                paint_slots,
                ..ItemGeometry::default()
            };
        };
        ItemGeometry {
            focus_targets: vec![ItemFocusTarget {
                local: 0,
                rows: 0..size.height,
                parent: None,
                disclosure: Some(target.clone()),
            }],
            hit_regions: if !self.interactive || size.width == 0 || size.height == 0 {
                Vec::new()
            } else {
                vec![ItemHitRegion {
                    rect: LogicalRect::new(0, 0, size.width, size.height),
                    action: ItemAction::ToggleDisclosure(target.clone()),
                }]
            },
            paint_slots,
            ..ItemGeometry::default()
        }
    }
}

// ---- item stack --------------------------------------------------------------------------

/// Item contents stacked vertically in one BMUX `Column`, each stretched to the full width.
///
/// Geometry merges the children's geometry translated by their placement. Focus targets are
/// renumbered in layout order; in a [`nested`](Self::nested) stack every parentless target
/// after the first becomes a child of the first (a unit header owns its section labels, B12).
pub struct ItemStack {
    column: Column<'static, RetainedObject>,
    children: Vec<SharedContent>,
    nested: bool,
}

impl std::fmt::Debug for ItemStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ItemStack")
            .field("children", &self.children.len())
            .field("nested", &self.nested)
            .finish_non_exhaustive()
    }
}

impl Default for ItemStack {
    fn default() -> Self {
        Self::new()
    }
}

impl ItemStack {
    /// An empty, flat stack.
    #[must_use]
    pub fn new() -> Self {
        Self {
            column: Column::empty().id("item-stack"),
            children: Vec::new(),
            nested: false,
        }
    }

    /// Return the stack nesting later parentless focus targets under the first.
    #[must_use]
    pub const fn nested(mut self) -> Self {
        self.nested = true;
        self
    }

    /// Append a child.
    pub fn push(&mut self, child: Box<dyn ItemContent>) {
        let child = SharedContent::new(child);
        let column = std::mem::replace(&mut self.column, Column::empty());
        self.column = column.child(child.clone());
        self.children.push(child);
    }

    /// Return the stack with `child` appended.
    #[must_use]
    pub fn with(mut self, child: Box<dyn ItemContent>) -> Self {
        self.push(child);
        self
    }

    /// Number of children.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.children.len()
    }

    /// Whether the stack has no children.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.children.is_empty()
    }
}

impl Component for ItemStack {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.column.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.column.paint(layout, cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.column.event(event, layout, cx)
    }

    fn revision(&self) -> ComponentRevision {
        self.column.revision()
    }
}

impl ItemContent for ItemStack {
    fn geometry(&self, layout: &LayoutNode) -> ItemGeometry {
        let mut merged = ItemGeometry::default();
        let mut source_rows: Vec<SourceRow> = Vec::new();
        let mut root: Option<u32> = None;
        for (child, placed) in self.children.iter().zip(&layout.children) {
            let geometry = child.geometry(&placed.node).translated(placed.x, placed.y);
            let offset = u32::try_from(merged.focus_targets.len()).unwrap_or(u32::MAX);
            let locals: BTreeMap<u32, u32> = geometry
                .focus_targets
                .iter()
                .enumerate()
                .map(|(index, target)| {
                    let index = u32::try_from(index).unwrap_or(u32::MAX);
                    (target.local, offset.saturating_add(index))
                })
                .collect();
            for target in geometry.focus_targets {
                let local = locals.get(&target.local).copied().unwrap_or(offset);
                let parent = match target.parent {
                    Some(parent) => locals.get(&parent).copied(),
                    None if self.nested => root,
                    None => None,
                };
                if self.nested && root.is_none() {
                    root = Some(local);
                }
                merged.focus_targets.push(ItemFocusTarget {
                    local,
                    parent,
                    ..target
                });
            }
            source_rows.extend(geometry.source_map.rows().iter().cloned());
            merged.hit_regions.extend(geometry.hit_regions);
            merged
                .selection_fragments
                .extend(geometry.selection_fragments);
            merged.content_keys.extend(geometry.content_keys);
            merged.paint_slots.extend(geometry.paint_slots);
            merged.visual_slots.extend(geometry.visual_slots);
            merged.markdown.extend(geometry.markdown);
            merged.content_targets.extend(geometry.content_targets);
            merged.selectable.extend(geometry.selectable);
        }
        merged.source_map = SourceRows::new(source_rows);
        merged
    }
}

/// Wrap `content` in policy spacing rows and an indent capped at half the width (B4).
#[must_use]
pub fn place(
    content: Box<dyn ItemContent>,
    spacing: Spacing,
    indent: u8,
    width: u64,
) -> Box<dyn ItemContent> {
    common::spaced(content, spacing, effective_indent(indent, width), width)
}

/// Width left for content inside [`place`] at `width`.
#[must_use]
pub fn placed_width(indent: u8, width: u64) -> u64 {
    let indent = u64::from(effective_indent(indent, width));
    // `spaced` keeps the indent only when it leaves a content column.
    if indent >= width {
        width
    } else {
        width - indent
    }
}

// ---- group header ------------------------------------------------------------------------

/// `5 tool calls`.
#[must_use]
pub fn count_text(count: usize) -> String {
    if count == 1 {
        "1 tool call".to_owned()
    } else {
        format!("{count} tool calls")
    }
}

/// `read ×3, grep`, eliding tools past [`MAX_HEADER_TOOLS`].
#[must_use]
pub fn tools_text(tools: &[(String, u64)]) -> String {
    let mut parts: Vec<String> = tools
        .iter()
        .take(MAX_HEADER_TOOLS)
        .map(|(tool, count)| {
            if *count == 1 {
                tool.clone()
            } else {
                format!("{tool} ×{count}")
            }
        })
        .collect();
    if tools.len() > MAX_HEADER_TOOLS {
        parts.push("…".to_owned());
    }
    parts.join(", ")
}

/// Group header units: the policy header fields on one row that toggles the group.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GroupHeaderProducer;

impl UnitProducer for GroupHeaderProducer {
    fn produce(&self, unit: UnitView<'_>, cx: &ItemRenderContext<'_>) -> ProducedItem {
        let ResolvedUnitPresentation::Group(group) = cx.structure else {
            // No resolved group (a caller-chosen structure): the renderer-neutral one-row form.
            return ProducedItem::new(DisclosureRow::new(
                common::plain_unit(unit),
                cx.disclosure.target.clone(),
            ));
        };
        let theme = cx.theme;
        let icons = theme.chrome.icons;
        let presentation = &group.presentation;
        let disclosure = cx.disclosure.disclosure;
        let meta = role_style(theme, "tool.metadata");
        let title = role_style(
            theme,
            &status_title_role(group.facts.status, group.facts.is_error),
        );
        let activity = ActivitySlots {
            spinner: matches!(
                group.facts.status,
                Some(UnitStatus::Running | UnitStatus::Waiting)
            ),
            elapsed_from: None,
        };
        let width = placed_width(presentation.indent, cx.width);
        let fields: Vec<&str> = presentation.header.as_ref().map_or_else(
            || DEFAULT_GROUP_HEADER.to_vec(),
            |header| header.fields.iter().map(String::as_str).collect(),
        );
        let count = match unit.unit.kind {
            UnitKind::GroupHeader { members, .. } => usize::try_from(members).unwrap_or(0),
            _ => group.members.len(),
        };
        if let Some(template) = presentation
            .header
            .as_ref()
            .and_then(|header| header.template.as_deref())
        {
            let line = group_template_line(
                template,
                GroupTemplate {
                    group,
                    count,
                    disclosure,
                    meta,
                    title,
                    theme,
                },
            );
            return ProducedItem::boxed(finish_group_header(
                line, group, activity, width, theme, cx.width,
            ));
        }
        let mut marker = None;
        let mut segments = Vec::new();
        for field in fields {
            match field {
                "marker" if marker.is_none() && segments.is_empty() => {
                    marker = Some((common::disclosure_marker(disclosure, icons), meta));
                }
                "marker" => segments.push(HeaderSegment::required(
                    common::disclosure_marker(disclosure, icons),
                    meta,
                )),
                "count" => {
                    segments.push(HeaderSegment::required(count_text(count), title).truncatable());
                }
                "tools" if !group.tools.is_empty() => segments.push(
                    HeaderSegment::optional(tools_text(&group.tools), title, 3).truncatable(),
                ),
                "tool" => {
                    if let Some(tool) = &group.facts.tool {
                        segments
                            .push(HeaderSegment::optional(tool.clone(), title, 3).truncatable());
                    }
                }
                "status" => {
                    if let Some(status) = group.facts.status {
                        segments.push(HeaderSegment::optional(status_text(status), meta, 2));
                    }
                }
                "timing" if presentation.part_visible("timing") => {
                    if let Some(duration) = group.duration_ms {
                        segments.push(HeaderSegment::optional(
                            format!("duration {}", common::elapsed_label(duration)),
                            meta,
                            1,
                        ));
                    }
                }
                _ => {}
            }
        }
        let line = marker_header_line(
            marker,
            segments,
            width.saturating_sub(activity.reserved_cells(width)),
        );
        ProducedItem::boxed(finish_group_header(
            line, group, activity, width, theme, cx.width,
        ))
    }
}

/// The inputs a group header template substitutes.
#[derive(Clone, Copy)]
struct GroupTemplate<'a> {
    group: &'a GroupStructure,
    count: usize,
    disclosure: Disclosure,
    meta: Style,
    title: Style,
    theme: &'a ResolvedThemeDefinition,
}

/// A group header row from a template: `{count}` in `tool.count` (or bold metadata), `{plural}`
/// as `s` unless the group has one member, `{tool}` as the shared tool's display name, and the
/// other group fields as in field headers.
fn group_template_line(template: &str, inputs: GroupTemplate<'_>) -> Line {
    let GroupTemplate {
        group,
        count,
        disclosure,
        meta,
        title,
        theme,
    } = inputs;
    let presentation = &group.presentation;
    let icons = theme.chrome.icons;
    let count_style = theme
        .style("tool.count")
        .unwrap_or_else(|| meta.add_modifier(bmux_tui::style::Modifier::BOLD));
    let bullet = theme
        .chrome
        .markers
        .bullet
        .as_ref()
        .map_or(super::tool::DEFAULT_BULLET, |glyph| glyph.as_str())
        .to_owned();
    let timing_visible = presentation.part_visible("timing");
    common::template_line(template, meta, |name| match name {
        "marker" => Some((
            common::disclosure_marker(disclosure, icons).to_owned(),
            meta,
        )),
        "bullet" => Some((bullet.clone(), title)),
        "count" => Some((count.to_string(), count_style)),
        "plural" => Some(((if count == 1 { "" } else { "s" }).to_owned(), meta)),
        "tool" => presentation
            .display_name
            .clone()
            .or_else(|| group.facts.tool.clone())
            .map(|tool| (tool, role_style(theme, "tool.label"))),
        "tools" => (!group.tools.is_empty()).then(|| (tools_text(&group.tools), meta)),
        "status" => group
            .facts
            .status
            .map(|status| (status_text(status).to_owned(), meta)),
        "timing" if timing_visible => group.duration_ms.map(|duration| {
            (
                format!("duration {}", common::elapsed_label(duration)),
                meta,
            )
        }),
        "elapsed" if timing_visible => group
            .duration_ms
            .map(|duration| (common::elapsed_label(duration), meta)),
        _ => None,
    })
}

/// The group header row in its surface, placed with the group's spacing and indent.
fn finish_group_header(
    line: Line,
    group: &GroupStructure,
    activity: ActivitySlots,
    width: u64,
    theme: &ResolvedThemeDefinition,
    full_width: u64,
) -> Box<dyn ItemContent> {
    let presentation = &group.presentation;
    let row = DisclosureRow::new(common::header_row(line), Some(group.target.clone()))
        .with_activity(activity);
    let mut content: Box<dyn ItemContent> = Box::new(row);
    if let Some(raised) = raised_surface(theme) {
        content = common::framed(content, raised, width);
    }
    // The group's closing spacing follows its last rendered row. Rendered members (full or
    // header rows, including members revealed in a collapsed group) carry it themselves; only
    // a group whose header is its last row ends with it here.
    let spacing = presentation.spacing.clamped();
    let spacing = Spacing {
        before: spacing.before,
        after: if group.closes { spacing.after } else { 0 },
    };
    place(content, spacing, presentation.indent, full_width)
}

/// Hidden-items marker units: `▸ N hidden` on one clipped row (A16).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HiddenMarkerProducer;

impl UnitProducer for HiddenMarkerProducer {
    fn produce(&self, unit: UnitView<'_>, cx: &ItemRenderContext<'_>) -> ProducedItem {
        let count = match unit.unit.kind {
            UnitKind::HiddenMarker { count } => count,
            _ => 0,
        };
        let line = Line::from_spans(vec![common::span(
            common::hidden_marker_text(count, cx.theme.chrome.icons),
            role_style(cx.theme, "tool.metadata"),
        )]);
        ProducedItem::new(common::header_row(line))
    }
}

#[cfg(test)]
pub(crate) mod harness;

#[cfg(test)]
mod tests;
