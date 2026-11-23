//! The stack rail, layer stepping, and the three ways a stack shows.
//!
//! * A stack review holds its stack two ways (see `reconcile`): one source per layer and one
//!   whole-stack source. The view picks which one shows; the other is left out of the document as
//!   an alternate ([`ReviewDocument::set_alternate_sources`]): hidden, and not counted in totals
//!   or viewed progress.
//! * The rail is a BMUX `tab_bar` row under the header, shown when the review has stack or
//!   stack-layer sources: one tab per layer (`#101 Title`, its position in the stack, viewed
//!   progress cells and `viewed/files`, and its draft threads), then one for the whole stack. The
//!   tab the cursor is in is selected. Labels degrade as the row narrows: full, then compact
//!   (`#101 2/5 ●1`), then the numbers only; tabs that still do not fit are cut by the bar.
//! * `.`/`,` (and clicks) step to the next/previous layer. Leaving a layer remembers the cursor
//!   in it; entering one returns to its remembered position (else its header). The positions
//!   are kept in the workspace's `presentation.layer_positions`. Entering a layer from the
//!   whole-stack view shows that layer alone; clicking the whole-stack tab shows the whole stack.
//! * Views, cycled by `shift+o`: one layer at a time (the default: the document is focused on
//!   the current layer with [`ReviewDocument::set_focus`], so the diff and the file panel hold
//!   that pull request alone); every layer in one document, where each layer starts with a
//!   banner, the file panel keeps only the current layer's group open, and a strip under the rail
//!   names the layer the cursor is in; and the whole stack as one diff, labeled by layer. The
//!   choice is kept in `presentation.stack_view`.
//!
//! [`ReviewDocument::set_focus`]: bcode_code_review::document::ReviewDocument::set_focus
//! [`ReviewDocument::set_alternate_sources`]: bcode_code_review::document::ReviewDocument::set_alternate_sources

use super::ReviewSurface;
use super::series::{Series, to_stack};
use bcode_code_review::document::{FileRef, ItemKey, ReviewItem};
use bcode_code_review_models::ReviewSourceKind;
use bcode_code_review_models::{ReviewGroupKey, ReviewSeriesView as View};
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

/// Id of the rail tab standing for the layers hidden while only real changes show.
const FOLD_ID: &str = "\u{1}hidden";

/// One tab of the rail.
#[derive(Debug, Clone)]
pub struct RailEntry {
    /// Workspace source id of the layer's group.
    pub source_id: String,
    /// Where entering the layer lands without a remembered position.
    pub home: ItemKey,
    /// Full, compact, and minimal labels.
    pub labels: [Line; 3],
    /// Whether the tab stands for the whole stack rather than one layer.
    pub whole: bool,
}

/// How a stack review is shown: one layer at a time, every layer in one document divided by
/// banners, or the whole stack as one diff labeled by layer.
pub use bcode_code_review_models::ReviewStackView as StackView;

impl ReviewSurface {
    /// Legacy PR-only view for comparison, forge and assistant actions.
    pub(super) fn shown_stack_view(&self) -> Option<StackView> {
        self.active_series()
            .filter(|s| !s.commits)?
            .view(self)
            .map(to_stack)
    }

    /// The pull request numbers of the stack's layer sources, bottom first (shown or not).
    pub(super) fn stack_layer_numbers(&self) -> Vec<u64> {
        self.review.as_ref().map_or_else(Vec::new, |review| {
            review
                .doc
                .groups()
                .filter_map(|group| {
                    let stack = group.stack.filter(|stack| !stack.whole)?;
                    stack.stack.layers.last().map(|layer| layer.number)
                })
                .collect()
        })
    }

    /// Whether source `source_id` is left out because the stack shows another way.
    pub(super) fn source_shows_stack_otherwise(&self, source_id: &str) -> bool {
        self.review.as_ref().is_some_and(|review| {
            review
                .doc
                .groups()
                .any(|g| g.key.selector() == source_id && g.alternate)
        })
    }

    /// Whether the review steps through a stack: two or more stack-layer groups.
    pub(super) fn steps_through_stack(&self) -> bool {
        self.active_series()
            .is_some_and(|s| !s.commits && s.members.len() >= 2)
    }

    /// Show the stack as [`Self::stack_view`] says: the whole-stack diff, or its layers with the
    /// document focused on the current layer (the one the cursor is in, else the last entered,
    /// else the first) or every layer with only the current layer's file group open.
    pub(super) fn apply_stack_view(&mut self) {
        self.show_stack_layer(None);
    }

    /// [`Self::apply_stack_view`] on layer `wanted` (a workspace source id), else the current one.
    pub(super) fn show_stack_layer(&mut self, wanted: Option<&str>) {
        let series = self.active_series();
        let entries = self.rail_entries();
        let target = series
            .as_ref()
            .filter(|s| s.view(self) != Some(View::Combined))
            .and_then(|series| {
                wanted
                    .and_then(|id| entries.iter().find(|entry| entry.source_id == id))
                    .or_else(|| {
                        series
                            .selected(self)
                            .and_then(|key| entries.iter().find(|e| e.source_id == key.selector()))
                    })
                    .or_else(|| entries.iter().find(|e| !e.whole))
                    .map(|e| (e.source_id.clone(), e.home.clone()))
            });
        let (mut alternates, hidden) =
            self.series_visibility(series.as_ref(), target.as_ref().map(|(id, _)| id.as_str()));
        let theme = self.theme;
        let file_view = self.file_view;
        let Some(review) = self.review.as_mut() else {
            return;
        };
        let cursor = review.cursor.clone();
        // Standalone PR ids remain visible to legacy PR comparison and publishing callers.
        let source_alternates = alternates
            .iter()
            .filter_map(|key| match key {
                ReviewGroupKey::Standalone { source_id } => Some(source_id.clone()),
                _ => None,
            })
            .collect();
        alternates.retain(|key| !matches!(key, ReviewGroupKey::Standalone { .. }));
        let swapped = review.doc.set_alternate_sources(source_alternates)
            | review.doc.set_alternate_groups(alternates);
        let focused = review.doc.set_hidden_groups(hidden);
        if swapped {
            // The sources shown changed: compare the ones now shown, and show each pull
            // request thread on them.
            self.comparing.layers_moved();
            self.place_notes();
            self.talk.resave_threads = true;
        }
        let Some(review) = self.review.as_mut() else {
            return;
        };
        if swapped || focused {
            review.note_splices();
            review.totals = review.doc.totals();
            review.rebuild_files(&theme, file_view);
            // Stay on the cursor's item when it still shows, else on the same file in the
            // sources now shown, else on the shown layer's home (or the document's start).
            let same_file = cursor
                .as_ref()
                .and_then(ItemKey::file)
                .map(|file| file.path.clone());
            let landing = cursor
                .filter(|key| review.doc.index_of(key).is_some())
                .or_else(|| {
                    let path = same_file?;
                    review
                        .doc
                        .groups()
                        .filter(|g| {
                            !g.alternate
                                && !g.hidden
                                && series
                                    .as_ref()
                                    .is_some_and(|s| s.contains(&g.key.selector()))
                        })
                        .find_map(|g| {
                            review
                                .doc
                                .group_files(g.index)
                                .find(|f| f.path == path)
                                .cloned()
                                .map(ItemKey::FileHeader)
                        })
                })
                .or_else(|| target.as_ref().map(|(_, home)| home.clone()));
            let index = landing
                .and_then(|key| review.doc.index_of(&key))
                .unwrap_or(0);
            review.set_cursor(index, AnchorPolicy::ItemTopAtTop);
        }
        if let (Some((source, _)), Some(series)) = (target, series) {
            review.open_only_group(&source, &series);
            self.rail_layer = Some(source);
        }
    }

    fn series_visibility(
        &self,
        series: Option<&Series>,
        target: Option<&str>,
    ) -> (BTreeSet<ReviewGroupKey>, BTreeSet<ReviewGroupKey>) {
        let catalog = self.series_catalog();
        let mut alternates = BTreeSet::new();
        let mut hidden = BTreeSet::new();
        for set in &catalog {
            match set.view(self) {
                Some(View::Combined) => alternates.extend(set.members.iter().cloned()),
                Some(mode) => {
                    alternates.extend(set.combined.iter().cloned());
                    if mode == View::OneMember {
                        let selected = target
                            .filter(|_| series.is_some_and(|s| s.id == set.id))
                            .and_then(|selector| {
                                set.members.iter().find(|key| key.selector() == selector)
                            })
                            .or_else(|| set.selected(self));
                        hidden.extend(set.members.iter().filter(|k| Some(*k) != selected).cloned());
                    }
                }
                None => {}
            }
        }
        (alternates, hidden)
    }

    /// Switch to the next way of showing the stack: one layer at a time, every layer, then the
    /// whole stack as one diff (skipping the ones the review cannot show).
    pub(super) fn toggle_stack_view(&mut self) {
        let Some(series) = self.active_series() else {
            self.set_status("this review has no series to switch", StatusSeverity::Info);
            return;
        };
        let mut views = series.views();
        if self
            .commit_series
            .get(&series.id)
            .is_some_and(|state| state.unavailable.is_some())
        {
            views.retain(|view| *view != View::Combined);
        }
        if views.len() < 2 {
            return;
        }
        let current = series.view(self);
        let next = views
            .iter()
            .position(|v| Some(*v) == current)
            .map_or(0, |i| (i + 1) % views.len());
        self.set_series_view(views[next]);
    }

    /// Change grouping without changing a comparison baseline or workspace source.
    pub fn set_series_view(&mut self, view: View) {
        let Some(series) = self.active_series() else {
            return;
        };
        if !series.views().contains(&view) {
            return;
        }
        if view == View::Combined
            && let Some(reason) = self
                .commit_series
                .get(&series.id)
                .and_then(|s| s.unavailable.clone())
        {
            self.set_status(reason, StatusSeverity::Info);
            return;
        }
        let key = self
            .review
            .as_ref()
            .and_then(|r| r.doc.group_of_key(r.cursor.as_ref()?))
            .cloned();
        if let Some(cursor) = self
            .review
            .as_ref()
            .and_then(|r| r.cursor.as_ref())
            .and_then(ItemKey::to_persisted)
            && let Some(key) = &key
        {
            self.layer_positions.insert(key.selector(), cursor);
        }
        let member = key.as_ref().filter(|key| series.members.contains(key));
        self.remember_series(&series, view, member);
        self.pending_member = if view == View::Combined {
            series.combined.as_ref()
        } else {
            member.or_else(|| series.selected(self))
        }
        .map(ReviewGroupKey::selector);
        if series.commits && (series.view(self) != Some(view) || self.switching) {
            self.schedule_series_reload();
        } else {
            self.apply_stack_view();
            self.finish_member_jump();
            self.apply_compare_filter();
        }
        let status = if series.commits {
            match view {
                View::Combined => "combined diff",
                View::AllMembers => "every member separately",
                View::OneMember => "one member at a time",
            }
            .to_owned()
        } else {
            let description = |mode| match mode {
                View::OneMember => "one pull request at a time",
                View::AllMembers => "every pull request of the stack",
                View::Combined => "the whole stack as one diff",
            };
            let views = series.views();
            match views.iter().position(|mode| *mode == view) {
                Some(i) if views.len() > 1 => format!(
                    "{} (shift+o: {})",
                    description(view),
                    description(views[(i + 1) % views.len()])
                ),
                _ => description(view).to_owned(),
            }
        };
        self.set_status(status, StatusSeverity::Info);
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
        let entries: Vec<RailEntry> = self
            .rail_entries()
            .into_iter()
            .filter(|entry| !entry.whole)
            .collect();
        let Some(position) = entries
            .iter()
            .position(|entry| entry.source_id == file.source_id)
        else {
            // The whole stack: its files are every layer's.
            self.set_status(
                "every file of the whole stack is viewed",
                StatusSeverity::Success,
            );
            return;
        };
        let text = if position + 1 < entries.len() {
            format!(
                "every file of this pull request is viewed; . for the next ({} of {})",
                position + 2,
                entries.len()
            )
        } else {
            "every file of the last pull request is viewed".to_owned()
        };
        self.set_status(text, StatusSeverity::Success);
    }

    /// Rail entries: one per stack-layer source group, then the whole stack's, in document order.
    pub(super) fn rail_entries(&self) -> Vec<RailEntry> {
        let Some(review) = self.review.as_ref() else {
            return Vec::new();
        };
        let Some(series) = self.active_series() else {
            return Vec::new();
        };
        let loaded: std::collections::BTreeMap<_, _> =
            review.doc.groups().map(|g| (g.key.selector(), g)).collect();
        let theme = &self.theme;
        let drafts = self.draft_threads();
        let filtered = self.compare_filtered();
        let mut entries = Vec::new();
        for (i, key) in series.members.iter().chain(&series.combined).enumerate() {
            let source_id = key.selector();
            if self.folded_away(&filtered, &source_id) {
                continue;
            }
            let group = loaded.get(&source_id);
            let stack = group.and_then(|g| g.stack);
            let whole = series.combined.as_ref() == Some(key);
            let threads: BTreeSet<&str> = drafts
                .iter()
                .filter(|(source, _)| *source == Some(source_id.as_str()))
                .map(|(_, thread)| *thread)
                .collect();
            let home = group
                .and_then(|g| group_home(&review.doc, g))
                .unwrap_or_else(|| ItemKey::SourceHeader {
                    source_id: series.id.clone(),
                    part: if whole { None } else { u32::try_from(i).ok() },
                });
            let (name, position, number) = self.rail_caption(&series, key, i, stack);
            let failed = self.source_notes.failed.contains(&source_id);
            let (viewed, viewed_style, cells) = tab_progress(
                failed,
                group.is_some_and(|g| g.loading),
                group.map_or(0, |g| g.viewed),
                group.map_or(0, |g| g.file_count),
                theme,
            );
            let mut full = vec![
                Span::styled(name, theme.text),
                Span::raw(" "),
                Span::styled(position, theme.muted),
                Span::raw(" "),
            ];
            if group.is_some() {
                full.extend(cells);
                full.push(Span::raw(" "));
                full.push(Span::styled(viewed.clone(), viewed_style));
            }
            let mut compact = vec![
                Span::styled(number.clone(), theme.badge_open),
                Span::raw(" "),
                Span::styled(
                    if group.is_some() {
                        viewed
                    } else {
                        String::new()
                    },
                    viewed_style,
                ),
            ];
            if !threads.is_empty() {
                let count = Span::styled(
                    format!(" {}{}", theme.icons.draft, threads.len()),
                    theme.badge_draft,
                );
                full.push(count.clone());
                compact.push(count);
            }
            // The pull request's review decision, open threads, and whether something is new.
            Self::extend_both(
                &mut full,
                &mut compact,
                self.conversation_tab_spans(&source_id),
            );
            let mut minimal = vec![Span::styled(number, theme.badge_open)];
            self.append_badge(&source_id, [&mut full, &mut compact, &mut minimal]);
            entries.push(RailEntry {
                source_id,
                home,
                whole,
                labels: [
                    Line::from_spans(full),
                    Line::from_spans(compact),
                    Line::from_spans(minimal),
                ],
            });
        }
        entries
    }

    /// Full name, position, and compact identifier for a rail tab.
    fn rail_caption(
        &self,
        series: &Series,
        key: &ReviewGroupKey,
        i: usize,
        stack: Option<&bcode_code_review::document::SourceStack>,
    ) -> (String, String, String) {
        let whole = series.combined.as_ref() == Some(key);
        stack.map_or_else(
            || {
                if whole {
                    (
                        "Combined commits".to_owned(),
                        format!("{} commits", series.members.len()),
                        "all".to_owned(),
                    )
                } else {
                    let oid = series.member_id(key);
                    let label = self
                        .commit_series
                        .get(&series.id)
                        .and_then(|s| s.members.get(i))
                        .map_or_else(|| oid.to_owned(), |(_, label)| label.clone());
                    (
                        label,
                        format!("{}/{}", i + 1, series.members.len()),
                        oid.chars().take(8).collect(),
                    )
                }
            },
            |stack| {
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
                            self.stack_size(&key.selector()).max(layers.len())
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
                (name, position, number)
            },
        )
    }

    /// Append `spans` to both labels.
    fn extend_both(full: &mut Vec<Span>, compact: &mut Vec<Span>, spans: Vec<Span>) {
        full.extend(spans.iter().cloned());
        compact.extend(spans);
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
        let cursor = self
            .review
            .as_ref()
            .and_then(|r| r.doc.group_of_key(r.cursor.as_ref()?))
            .map(ReviewGroupKey::selector);
        let found = cursor
            .as_ref()
            .and_then(|id| entries.iter().position(|entry| &entry.source_id == id))
            .or_else(|| {
                self.rail_layer
                    .as_ref()
                    .and_then(|id| entries.iter().position(|entry| &entry.source_id == id))
            });
        // Layers do not show with the whole stack: a layer found is only the one last entered.
        if self.shown_series_view() == Some(View::Combined)
            && found.is_none_or(|index| !entries[index].whole)
            && let Some(whole) = entries.iter().position(|entry| entry.whole)
        {
            return Some(whole);
        }
        found
    }

    /// The source the cursor is in, else the layer last entered.
    pub(super) fn cursor_source(&self) -> Option<String> {
        let review = self.review.as_ref()?;
        match review.doc.item(review.cursor_index)? {
            ReviewItem::SourceHeader(header) => Some(header.source_id.clone()),
            item => item.file().map(|file: &FileRef| file.source_id.clone()),
        }
        .or_else(|| self.rail_layer.clone())
    }

    /// Step to the next or previous layer (wrapping), remembering the position in the one left.
    /// From the whole-stack view, the layer last entered (else the first) shows alone.
    pub(super) fn step_layer(&mut self, forward: bool) {
        let entries = self.rail_entries();
        let layers: Vec<usize> = (0..entries.len())
            .filter(|index| !entries[*index].whole)
            .collect();
        if layers.is_empty() {
            self.set_status("this review has no stack layers", StatusSeverity::Info);
            return;
        }
        let current = self.current_layer(&entries);
        if current.is_some_and(|index| entries[index].whole) {
            let remembered = self.rail_layer.as_ref().and_then(|layer| {
                layers
                    .iter()
                    .copied()
                    .find(|index| &entries[*index].source_id == layer)
            });
            self.enter_layer(&entries, current, remembered.unwrap_or(layers[0]));
            return;
        }
        let entries: Vec<RailEntry> = layers.iter().map(|index| entries[*index].clone()).collect();
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
            && self
                .review
                .as_ref()
                .and_then(|r| r.doc.group_of_key(&cursor))
                .is_some_and(|key| key.selector() == current.source_id)
        {
            self.layer_positions
                .insert(current.source_id.clone(), persisted);
        }
        if target.whole {
            // The whole-stack tab shows the whole stack as one diff.
            self.set_series_view(View::Combined);
            return;
        }
        let Some(series) = self.active_series() else {
            return;
        };
        let Some(key) = series
            .members
            .iter()
            .find(|key| key.selector() == target.source_id)
        else {
            return;
        };
        let prior = series.view(self);
        let view = if prior == Some(View::AllMembers) {
            View::AllMembers
        } else {
            View::OneMember
        };
        self.remember_series(&series, view, Some(key));
        if series.commits
            && (prior != Some(view)
                || !self
                    .review
                    .as_ref()
                    .is_some_and(|r| r.doc.groups().any(|g| g.key == key)))
        {
            self.pending_member = Some(key.selector());
            self.rail_layer = self.pending_member.clone();
            self.schedule_series_reload();
            return;
        }
        self.apply_compare_filter();
        self.rail_layer = Some(target.source_id.clone());
        // Show the entered layer (alone, or with only its file group open).
        let entered = target.source_id.clone();
        self.show_stack_layer(Some(&entered));
        let remembered = self
            .layer_positions
            .get(&target.source_id)
            .and_then(|persisted| {
                self.review.as_ref().and_then(|review| {
                    review
                        .doc
                        .resolve_persisted(persisted)
                        .filter(|item| review.doc.group_of_key(item) == Some(key))
                })
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
        self.shown_series_view() == Some(View::AllMembers)
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
            .find(|group| group.key.selector() == entries[index].source_id)
        else {
            return;
        };
        let Some(layers) = group.stack.map(|stack| &stack.stack.layers) else {
            cx.paint_component(
                &TextBlock::new(Text::from_lines(vec![entries[index].labels[0].clone()])),
                area,
            );
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
        spans.push(Span::styled(separator, theme.muted));
        if self.source_notes.failed.contains(&group.source.source_id) {
            spans.push(Span::styled("could not be opened", theme.error));
        } else if group.loading {
            spans.push(Span::styled(
                format!("{} loading files", theme.icons.ellipsis),
                theme.notice,
            ));
        } else {
            let complete = group.file_count > 0 && group.viewed == group.file_count;
            spans.push(Span::styled(
                format!("{}/{} viewed", group.viewed, group.file_count),
                if complete { theme.viewed } else { theme.muted },
            ));
        }
        // What the rest of the stack is doing, and how current the stack is.
        let loading = review.doc.groups().filter(|group| group.loading).count();
        if loading > 0 {
            spans.push(Span::styled(separator, theme.muted));
            spans.push(Span::styled(
                format!(
                    "{loading} more layer{} loading",
                    if loading == 1 { "" } else { "s" }
                ),
                theme.notice,
            ));
        }
        if let Some(status) = stack_status(
            self.open_progress.filling_note.as_deref(),
            self.open_progress.stack_confirmed_at,
            std::time::SystemTime::now(),
        ) {
            spans.push(Span::styled(separator, theme.muted));
            spans.push(Span::styled(status, theme.muted));
        }
        if let Some(summary) = self.last_push_summary() {
            spans.push(Span::styled(separator, theme.muted));
            spans.push(Span::styled(summary, theme.muted));
        }
        let line = Line::from_spans(spans).truncate(usize::from(area.width));
        cx.paint_component(&TextBlock::new(Text::from_lines(vec![line])), area);
    }

    /// A layer's badge from its comparison: full (`● +40 −12 · 3 files`), compact
    /// (`●+40−12`), and its glyph alone; `None` when the layer is not compared.
    pub(super) fn layer_badge(&self, source_id: &str) -> Option<[Vec<Span>; 3]> {
        use bcode_code_review_models::DeltaKind;
        let review = self.review.as_ref()?;
        let comparison = review.doc.comparison(source_id)?;
        let theme = &self.theme;
        let icons = &theme.icons;
        let Some(delta) = comparison.delta.as_ref() else {
            let busy = vec![Span::styled(icons.ellipsis, theme.muted)];
            return Some([busy.clone(), busy.clone(), busy]);
        };
        let (glyph, text, style) = match delta.kind {
            DeltaKind::Changed | DeltaKind::WithRebase => {
                // Summed when the comparison was set: painting costs nothing per file.
                let summary = review.doc.comparison_summary(source_id).unwrap_or_default();
                let (added, removed, files) = (summary.added, summary.removed, summary.files);
                let glyph = if delta.kind == DeltaKind::WithRebase {
                    icons.since_with_rebase
                } else {
                    icons.since_changed
                };
                let full = vec![
                    Span::styled(
                        format!("{glyph} +{added} {}{removed}", icons.minus),
                        theme.since.badge,
                    ),
                    Span::styled(
                        format!(
                            "{}{files} file{}",
                            icons.separator,
                            if files == 1 { "" } else { "s" }
                        ),
                        theme.muted,
                    ),
                ];
                let compact = vec![Span::styled(
                    format!("{glyph}+{added}{}{removed}", icons.minus),
                    theme.since.badge,
                )];
                return Some([full, compact, vec![Span::styled(glyph, theme.since.badge)]]);
            }
            DeltaKind::RebasedOnly => (icons.since_rebased, "rebased", theme.muted),
            DeltaKind::Unchanged => (icons.since_unchanged, "unchanged", theme.viewed),
            DeltaKind::NewToYou => (icons.since_new_to_you, "new to you", theme.notice),
            DeltaKind::Unavailable => (icons.since_unavailable, "unavailable", theme.error),
        };
        Some([
            vec![Span::styled(format!("{glyph} {text}"), style)],
            vec![Span::styled(glyph, style)],
            vec![Span::styled(glyph, style)],
        ])
    }

    /// Each draft's source and thread.
    fn draft_threads(&self) -> Vec<(Option<&str>, &str)> {
        self.notes
            .drafts()
            .iter()
            .map(|draft| (draft.anchor.source_id.as_deref(), draft.thread_id.as_str()))
            .collect()
    }

    /// Whether layer `source_id` is folded into the tab of hidden layers: without real changes
    /// while only those show (the current layer stays).
    fn folded_away(&self, filtered: &BTreeSet<String>, source_id: &str) -> bool {
        filtered.contains(source_id) && self.rail_layer.as_deref() != Some(source_id)
    }

    /// Append layer `source_id`'s comparison badge to its full, compact, and minimal labels.
    fn append_badge(&self, source_id: &str, labels: [&mut Vec<Span>; 3]) {
        let Some([wide, narrow, glyph]) = self.layer_badge(source_id) else {
            return;
        };
        let [full, compact, minimal] = labels;
        full.push(Span::raw(" "));
        full.extend(wide);
        compact.push(Span::raw(" "));
        compact.extend(narrow);
        minimal.extend(glyph);
    }

    /// The tab standing for the layers hidden while only real changes show, when there are.
    fn fold_tab(&self) -> Option<Line> {
        let hidden = self
            .compare_filtered()
            .into_iter()
            .filter(|source| self.rail_layer.as_ref() != Some(source))
            .count();
        (hidden > 0).then(|| {
            Line::from_spans(vec![Span::styled(
                format!("+{hidden} {}", self.theme.icons.since_rebased),
                self.theme.muted,
            )])
        })
    }

    /// Tab items for the rail at `width`: the most detailed labels that fit, then the tab of
    /// hidden layers.
    fn rail_items<'a>(
        entries: &'a [RailEntry],
        fold: Option<&Line>,
        width: u16,
    ) -> Vec<TabItem<'a>> {
        let build = |level: usize| -> Vec<TabItem<'a>> {
            entries
                .iter()
                .map(|entry| TabItem::rich(&entry.source_id, entry.labels[level].clone()))
                .chain(fold.map(|fold| TabItem::rich(FOLD_ID, fold.clone())))
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
        let fold = self.fold_tab();
        let items = Self::rail_items(&entries, fold.as_ref(), area.width);
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
        let fold = self.fold_tab();
        let outcome = {
            let items = Self::rail_items(&entries, fold.as_ref(), area.width);
            let component = TabBarComponent::new(RAIL_ID, &items, &self.rail_state)
                .policy(rail_policy())
                .styles(self.theme.component.tab_bar_styles());
            super::super::modal::route(&component, area, &layout, event)
        };
        let after = self.rail_state.borrow().selected();
        if after != before
            && let Some(next) = after
        {
            if next >= entries.len() {
                // The tab of hidden layers: show every pull request again.
                self.rail_state.borrow_mut().set_selected(before);
                self.toggle_only_changed();
                return true;
            }
            self.enter_layer(&entries, current, next);
            return true;
        }
        outcome != EventOutcome::Ignored
    }
}

const fn rail_policy() -> TabBarPolicy {
    TabBarPolicy::interactive().separator(" │ ")
}

/// What the layer strip says about the stack: what the open is still doing while it checks the
/// stack, else how long ago its provider confirmed it. `None` when neither is known.
fn stack_status(
    filling: Option<&str>,
    confirmed_at: Option<std::time::SystemTime>,
    now: std::time::SystemTime,
) -> Option<String> {
    if let Some(note) = filling.filter(|note| note.starts_with("checking")) {
        return Some(format!("{note}…"));
    }
    let age = now
        .duration_since(confirmed_at?)
        .unwrap_or_default()
        .as_secs();
    Some(match age {
        0..60 => "stack confirmed just now".to_owned(),
        60..3_600 => format!("stack confirmed {}m ago", age / 60),
        3_600..86_400 => format!("stack confirmed {}h ago", age / 3_600),
        _ => format!("stack confirmed {}d ago", age / 86_400),
    })
}

/// A tab's viewed progress: its label (`viewed/files`, an ellipsis while the layer's files are
/// still loading, `!` when it could not be opened), the label's style, and its progress cells.
fn tab_progress(
    failed: bool,
    loading: bool,
    viewed: usize,
    total: usize,
    theme: &super::ReviewTheme,
) -> (String, bmux_tui::style::Style, Vec<Span>) {
    if failed {
        return ("!".to_owned(), theme.error, progress_cells(0, 0, theme));
    }
    let label = if loading {
        theme.icons.ellipsis.to_owned()
    } else {
        format!("{viewed}/{total}")
    };
    let complete = total > 0 && viewed == total;
    let style = if complete { theme.viewed } else { theme.muted };
    (label, style, progress_cells(viewed, total, theme))
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

/// Where entering a layer's group lands first: its header, else its first file.
fn group_home(
    doc: &bcode_code_review::document::ReviewDocument,
    group: &bcode_code_review::document::GroupInfo<'_>,
) -> Option<ItemKey> {
    group.header.clone().or_else(|| {
        doc.group_files(group.index)
            .next()
            .map(|file| ItemKey::FileHeader(file.clone()))
    })
}

#[cfg(test)]
mod tests {
    use super::stack_status;
    use std::time::{Duration, SystemTime};

    #[test]
    fn the_strip_says_what_the_stack_is_doing_then_how_current_it_is() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let ago = |secs: u64| Some(now - Duration::from_secs(secs));
        // Checking wins over an older confirmation; other stages say nothing about the stack.
        assert_eq!(
            stack_status(Some("checking the stack with its provider"), ago(90), now).as_deref(),
            Some("checking the stack with its provider…")
        );
        assert_eq!(
            stack_status(Some("listing files of every layer: 3 of 9"), ago(90), now).as_deref(),
            Some("stack confirmed 1m ago")
        );
        assert_eq!(
            stack_status(None, ago(5), now).as_deref(),
            Some("stack confirmed just now")
        );
        assert_eq!(
            stack_status(None, ago(7_200), now).as_deref(),
            Some("stack confirmed 2h ago")
        );
        assert_eq!(
            stack_status(None, ago(3 * 86_400), now).as_deref(),
            Some("stack confirmed 3d ago")
        );
        assert_eq!(stack_status(None, None, now), None);
    }
}
