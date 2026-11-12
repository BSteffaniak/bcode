//! The stack rail and layer stepping.
//!
//! * The rail is a BMUX `tab_bar` row under the header, shown when the review has stack or
//!   stack-layer sources: one tab per such source group (`#101 Title`, its position in the
//!   stack, viewed progress cells and `viewed/files`, and its draft threads). The tab of the
//!   layer the cursor is in is selected. Labels degrade as the row narrows: full, then compact
//!   (`#101 2/5 ●1`), then the numbers only; tabs that still do not fit are cut by the bar.
//! * `.`/`,` (and clicks) step to the next/previous layer. Leaving a layer remembers the cursor
//!   in it; entering one returns to its remembered position (else its header). The positions
//!   are kept in the workspace's `presentation.layer_positions`.
//! * A step-through review (two or more stack-layer sources) shows one layer at a time by
//!   default: the document is focused on the current layer ([`ReviewDocument::set_focus`]), so
//!   the diff and the file panel hold that pull request alone. `shift+o` switches to every layer
//!   in one document, where each layer starts with a banner, the file panel keeps only the
//!   current layer's group open, and a strip under the rail names the layer the cursor is in.
//!   The choice is kept in `presentation.all_stack_layers`.
//!
//! [`ReviewDocument::set_focus`]: bcode_code_review::document::ReviewDocument::set_focus

use super::ReviewSurface;
use bcode_code_review::document::{FileRef, ItemKey, ReviewItem};
use bcode_code_review_models::ReviewSourceKind;
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::composition::TextBlock;
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::text::{Line, Span, Text};
use bmux_tui_components::status_bar::StatusSeverity;
use bmux_tui_components::tab_bar::{TabBar, TabBarComponent, TabBarPolicy, TabItem};
use bmux_tui_components::virtual_list::AnchorPolicy;
use std::collections::BTreeSet;

/// Layout id of the rail.
pub const RAIL_ID: &str = "review.stack_rail";

/// One tab of the rail.
#[derive(Debug, Clone)]
pub struct RailEntry {
    /// Workspace source id of the layer's group.
    pub source_id: String,
    /// Where entering the layer lands without a remembered position.
    pub home: ItemKey,
    /// Full, compact, and minimal labels.
    pub labels: [Line; 3],
}

/// How a step-through stack review is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StackView {
    /// One layer at a time.
    #[default]
    OneLayer,
    /// Every layer in one document, divided by banners.
    AllLayers,
}

impl ReviewSurface {
    /// Whether the review steps through a stack: two or more stack-layer groups.
    pub(super) fn steps_through_stack(&self) -> bool {
        self.review.as_ref().is_some_and(|review| {
            review
                .doc
                .groups()
                .filter(|group| group.stack.is_some_and(|stack| !stack.whole))
                .count()
                >= 2
        })
    }

    /// Show the stack as [`Self::stack_view`] says: focus the document on the current layer (the
    /// one the cursor is in, else the last entered, else the first), or show every layer with
    /// only the current layer's file group open.
    pub(super) fn apply_stack_view(&mut self) {
        self.show_stack_layer(None);
    }

    /// [`Self::apply_stack_view`] on layer `wanted` (a workspace source id), else the current one.
    fn show_stack_layer(&mut self, wanted: Option<&str>) {
        if !self.steps_through_stack() {
            return;
        }
        let entries = self.rail_entries();
        let current = wanted
            .and_then(|wanted| entries.iter().position(|entry| entry.source_id == wanted))
            .or_else(|| self.current_layer(&entries))
            .or_else(|| {
                let remembered = self.rail_layer.as_ref()?;
                entries
                    .iter()
                    .position(|entry| &entry.source_id == remembered)
            })
            .unwrap_or(0);
        let Some(target) = entries.get(current) else {
            return;
        };
        let source = target.source_id.clone();
        let home = target.home.clone();
        let one_layer = self.stack_view == StackView::OneLayer;
        let theme = self.theme;
        let hide_viewed = self.hide_viewed_files;
        let Some(review) = self.review.as_mut() else {
            return;
        };
        let cursor = review.cursor.clone();
        if review.doc.set_focus(one_layer.then_some(source.as_str())) {
            review.note_splices();
            review.rebuild_files(&theme, hide_viewed);
            let landing = cursor
                .filter(|key| review.doc.index_of(key).is_some())
                .unwrap_or(home);
            if let Some(index) = review.doc.index_of(&landing) {
                review.set_cursor(index, AnchorPolicy::ItemTopAtTop);
            }
        }
        review.open_only_group(&source);
        self.rail_layer = Some(source);
    }

    /// Switch between one layer at a time and every layer.
    pub(super) fn toggle_stack_view(&mut self) {
        if !self.steps_through_stack() {
            self.set_status(
                "only a review that steps through a stack has layers to show one at a time",
                StatusSeverity::Info,
            );
            return;
        }
        self.stack_view = match self.stack_view {
            StackView::OneLayer => StackView::AllLayers,
            StackView::AllLayers => StackView::OneLayer,
        };
        self.apply_stack_view();
        let text = match self.stack_view {
            StackView::OneLayer => "one pull request at a time (shift+o: all)",
            StackView::AllLayers => "every pull request of the stack (shift+o: one at a time)",
        };
        self.set_status(text, StatusSeverity::Info);
        self.persist_presentation(true);
    }

    /// After a file of a layer is marked viewed: when that was the layer's last unviewed file,
    /// say so and how to go on.
    pub(super) fn note_layer_done(&mut self, file: &FileRef) {
        if !self.steps_through_stack() {
            return;
        }
        let Some(review) = self.review.as_ref() else {
            return;
        };
        let Some(group) = review
            .doc
            .groups()
            .find(|group| group.source.source_id == file.source_id && group.stack.is_some())
        else {
            return;
        };
        if group.file_count == 0 || group.viewed < group.file_count {
            return;
        }
        let entries = self.rail_entries();
        let position = entries
            .iter()
            .position(|entry| entry.source_id == file.source_id);
        let text = match position {
            Some(index) if index + 1 < entries.len() => format!(
                "every file of this pull request is viewed; . for the next ({} of {})",
                index + 2,
                entries.len()
            ),
            _ => "every file of the last pull request is viewed".to_owned(),
        };
        self.set_status(text, StatusSeverity::Success);
    }

    /// Rail entries: one per stack or stack-layer source group, in document order.
    pub(super) fn rail_entries(&self) -> Vec<RailEntry> {
        let Some(review) = self.review.as_ref() else {
            return Vec::new();
        };
        if !review.doc.groups().any(|group| group.stack.is_some()) {
            return Vec::new();
        }
        let theme = &self.theme;
        let drafts: Vec<(Option<&str>, &str)> = self
            .notes
            .drafts()
            .iter()
            .map(|draft| (draft.anchor.source_id.as_deref(), draft.thread_id.as_str()))
            .collect();
        let mut entries = Vec::new();
        for group in review.doc.groups() {
            let Some(stack) = group.stack else {
                continue;
            };
            let source_id = group.source.source_id.clone();
            let threads: BTreeSet<&str> = drafts
                .iter()
                .filter(|(source, _)| *source == Some(source_id.as_str()))
                .map(|(_, thread)| *thread)
                .collect();
            let home = group.header.clone().or_else(|| {
                review
                    .doc
                    .group_files(group.index)
                    .next()
                    .map(|file| ItemKey::FileHeader(file.clone()))
            });
            let Some(home) = home else {
                continue;
            };
            let layers = &stack.stack.layers;
            let (name, position) = if stack.whole {
                let first = layers.first().map_or(0, |layer| layer.number);
                let last = layers.last().map_or(0, |layer| layer.number);
                (
                    format!("Whole stack #{first}–#{last}"),
                    format!("{} layers", layers.len()),
                )
            } else {
                let top = layers.last();
                (
                    top.map_or_else(String::new, |layer| {
                        format!("#{} {}", layer.number, layer.title)
                    }),
                    format!(
                        "{}/{}",
                        layers.len(),
                        self.stack_size(&source_id).max(layers.len())
                    ),
                )
            };
            let number = if stack.whole {
                "all".to_owned()
            } else {
                layers
                    .last()
                    .map_or_else(String::new, |layer| format!("#{}", layer.number))
            };
            let total = group.file_count;
            let viewed = format!("{}/{}", group.viewed, total);
            let complete = total > 0 && group.viewed == total;
            let viewed_style = if complete { theme.viewed } else { theme.muted };
            let cells = progress_cells(group.viewed, total, theme);
            let mut full = vec![
                Span::styled(name, theme.text),
                Span::raw(" "),
                Span::styled(position, theme.muted),
                Span::raw(" "),
            ];
            full.extend(cells);
            full.push(Span::raw(" "));
            full.push(Span::styled(viewed.clone(), viewed_style));
            let mut compact = vec![
                Span::styled(number.clone(), theme.badge_open),
                Span::raw(" "),
                Span::styled(viewed, viewed_style),
            ];
            if !threads.is_empty() {
                let count = Span::styled(
                    format!(" {}{}", theme.icons.draft, threads.len()),
                    theme.badge_draft,
                );
                full.push(count.clone());
                compact.push(count);
            }
            entries.push(RailEntry {
                source_id,
                home,
                labels: [
                    Line::from_spans(full),
                    Line::from_spans(compact),
                    Line::from_spans(vec![Span::styled(number, theme.badge_open)]),
                ],
            });
        }
        entries
    }

    /// Layers in the whole stack of a stack-layer source (its workspace kind does not say).
    fn stack_size(&self, source_id: &str) -> usize {
        // Every layer source of one stack shares the stack: the largest resolved chain is the
        // stack as far as the review reaches.
        let Some(review) = self.review.as_ref() else {
            return 0;
        };
        let is_layer = self.sources.iter().any(|source| {
            source.id == source_id && matches!(source.kind, ReviewSourceKind::StackLayer { .. })
        });
        if !is_layer {
            return 0;
        }
        review
            .doc
            .groups()
            .filter_map(|group| group.stack.map(|stack| stack.stack.layers.len()))
            .max()
            .unwrap_or(0)
    }

    /// Index of the rail entry the cursor is in.
    pub(super) fn current_layer(&self, entries: &[RailEntry]) -> Option<usize> {
        let review = self.review.as_ref()?;
        let source = match review.doc.item(review.cursor_index)? {
            ReviewItem::SourceHeader(header) => Some(header.source_id.clone()),
            item => item.file().map(|file: &FileRef| file.source_id.clone()),
        }
        .or_else(|| self.rail_layer.clone())?;
        entries.iter().position(|entry| entry.source_id == source)
    }

    /// Step to the next or previous layer (wrapping), remembering the position in the one left.
    pub(super) fn step_layer(&mut self, forward: bool) {
        let entries = self.rail_entries();
        if entries.is_empty() {
            self.set_status("this review has no stack layers", StatusSeverity::Info);
            return;
        }
        let current = self.current_layer(&entries);
        let next = match current {
            Some(index) if forward => (index + 1) % entries.len(),
            Some(index) => (index + entries.len() - 1) % entries.len(),
            None => 0,
        };
        self.enter_layer(&entries, current, next);
    }

    /// Move to rail entry `next`, remembering the cursor of entry `current`.
    pub(super) fn enter_layer(
        &mut self,
        entries: &[RailEntry],
        current: Option<usize>,
        next: usize,
    ) {
        let Some(target) = entries.get(next) else {
            return;
        };
        if let (Some(current), Some(cursor)) = (
            current.and_then(|index| entries.get(index)),
            self.review
                .as_ref()
                .and_then(|review| review.cursor.clone()),
        ) && let Some(persisted) = cursor.to_persisted()
        {
            self.layer_positions
                .insert(current.source_id.clone(), persisted);
        }
        self.rail_layer = Some(target.source_id.clone());
        // Show the entered layer (alone, or with only its file group open).
        let entered = target.source_id.clone();
        self.show_stack_layer(Some(&entered));
        let remembered = self
            .layer_positions
            .get(&target.source_id)
            .and_then(|persisted| {
                self.review
                    .as_ref()
                    .and_then(|review| review.doc.resolve_persisted(persisted))
            });
        let Some(review) = self.review.as_mut() else {
            return;
        };
        let key = remembered.unwrap_or_else(|| target.home.clone());
        match review.doc.jump_to(&key) {
            Some(jump) => {
                review.set_cursor(jump.index, AnchorPolicy::ItemTopAtTop);
                review.pending_jump = jump.needs_load.map(|_| key.clone());
            }
            None => {
                if let Some(index) = review.doc.index_of(&target.home) {
                    review.set_cursor(index, AnchorPolicy::ItemTopAtTop);
                }
            }
        }
        let label = target.labels[0].plain_text();
        self.set_status(format!("layer {label}"), StatusSeverity::Info);
        self.persist_presentation(true);
    }

    /// Whether the strip naming the cursor's layer shows: every layer in one document.
    pub(super) fn shows_layer_strip(&self) -> bool {
        self.stack_view == StackView::AllLayers && self.steps_through_stack()
    }

    /// Paint the strip naming the layer the cursor is in on `area` (one row): its position,
    /// number and title, branches, and viewed progress.
    pub(super) fn paint_layer_strip(&self, area: Rect, cx: &mut PaintCx<'_, '_>) {
        let theme = &self.theme;
        cx.fill(
            LocalRect::new(area.x.into(), area.y.into(), area.width, 1),
            " ",
            theme.header_bar,
        );
        let entries = self.rail_entries();
        let (Some(index), Some(review)) = (self.current_layer(&entries), self.review.as_ref())
        else {
            return;
        };
        let Some(group) = review
            .doc
            .groups()
            .find(|group| group.source.source_id == entries[index].source_id)
        else {
            return;
        };
        let Some(layers) = group.stack.map(|stack| &stack.stack.layers) else {
            return;
        };
        let Some(top) = layers.last() else {
            return;
        };
        // The position in the whole stack, as the layer's banner gives it.
        let count = self.stack_size(&entries[index].source_id).max(layers.len());
        let separator = theme.icons.separator;
        let mut spans = vec![
            Span::styled(
                format!(" PR {} of {count} ", layers.len()),
                theme.badge_open,
            ),
            Span::styled(format!(" #{} ", top.number), theme.badge_open),
            Span::styled(top.title.clone(), theme.text),
        ];
        if !top.head_branch.is_empty() {
            spans.push(Span::styled(separator, theme.muted));
            spans.push(Span::styled(
                format!("{} ← {}", top.base_branch, top.head_branch),
                theme.muted,
            ));
        }
        let complete = group.file_count > 0 && group.viewed == group.file_count;
        spans.push(Span::styled(separator, theme.muted));
        spans.push(Span::styled(
            format!("{}/{} viewed", group.viewed, group.file_count),
            if complete { theme.viewed } else { theme.muted },
        ));
        let line = Line::from_spans(spans).truncate(usize::from(area.width));
        cx.paint_component(&TextBlock::new(Text::from_lines(vec![line])), area);
    }

    /// Tab items for the rail at `width`: the most detailed labels that fit.
    fn rail_items(entries: &[RailEntry], width: u16) -> Vec<TabItem<'_>> {
        let build = |level: usize| -> Vec<TabItem<'_>> {
            entries
                .iter()
                .map(|entry| TabItem::rich(&entry.source_id, entry.labels[level].clone()))
                .collect()
        };
        for level in 0..3 {
            let items = build(level);
            let text = TabBar::new(&items).policy(rail_policy()).text();
            if bmux_tui::text_width::display_width(&text) <= usize::from(width) {
                return items;
            }
        }
        build(2)
    }

    /// Paint the rail on `area` (one row).
    pub(super) fn paint_rail(&mut self, area: Rect, cx: &mut PaintCx<'_, '_>) {
        let entries = self.rail_entries();
        let selected = self.current_layer(&entries);
        self.rail_state.borrow_mut().set_selected(selected);
        let items = Self::rail_items(&entries, area.width);
        let component = TabBarComponent::new(RAIL_ID, &items, &self.rail_state)
            .policy(rail_policy())
            .styles(self.theme.component.tab_bar_styles());
        cx.fill(
            LocalRect::new(area.x.into(), area.y.into(), area.width, 1),
            " ",
            self.theme.header_bar,
        );
        let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
        cx.with_child(
            i32::from(area.x),
            i64::from(area.y),
            LocalRect::new(0, 0, area.width, area.height),
            |cx| component.paint(&layout, cx),
        );
        self.rail = Some((area, layout));
    }

    /// Route a pointer event on the rail; a click on a tab enters that layer.
    pub(super) fn rail_mouse(&mut self, event: &Event) -> bool {
        let Some((area, layout)) = self.rail.clone() else {
            return false;
        };
        let Event::Mouse(mouse) = event else {
            return false;
        };
        if !area.contains(mouse.position) {
            return false;
        }
        let entries = self.rail_entries();
        let current = self.current_layer(&entries);
        let before = self.rail_state.borrow().selected();
        let outcome = {
            let items = Self::rail_items(&entries, area.width);
            let component = TabBarComponent::new(RAIL_ID, &items, &self.rail_state)
                .policy(rail_policy())
                .styles(self.theme.component.tab_bar_styles());
            super::super::modal::route(&component, area, &layout, event)
        };
        let after = self.rail_state.borrow().selected();
        if after != before
            && let Some(next) = after
        {
            self.enter_layer(&entries, current, next);
            return true;
        }
        outcome != EventOutcome::Ignored
    }
}

const fn rail_policy() -> TabBarPolicy {
    TabBarPolicy::interactive().separator(" │ ")
}

/// Five viewed-progress cells.
fn progress_cells(viewed: usize, total: usize, theme: &super::ReviewTheme) -> Vec<Span> {
    const CELLS: usize = 5;
    let filled = if total == 0 {
        0
    } else {
        (viewed * CELLS).div_ceil(total).min(CELLS)
    };
    vec![
        Span::styled(theme.icons.progress_filled.repeat(filled), theme.viewed),
        Span::styled(
            theme.icons.progress_empty.repeat(CELLS - filled),
            theme.bar_empty,
        ),
    ]
}
