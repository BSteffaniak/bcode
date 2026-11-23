//! Series identity and grouping policy shared by the stack and selected-commit rail.

use super::ReviewSurface;
use super::stack::StackView;
use bcode_code_review_models::{
    ReviewGroupKey, ReviewSeriesPresentation, ReviewSeriesView as View, ReviewSourceKind,
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

pub(super) const fn from_stack(view: StackView) -> View {
    match view {
        StackView::OneLayer => View::OneMember,
        StackView::AllLayers => View::AllMembers,
        StackView::Whole => View::Combined,
    }
}

pub(super) const fn to_stack(view: View) -> StackView {
    match view {
        View::OneMember => StackView::OneLayer,
        View::AllMembers => StackView::AllLayers,
        View::Combined => StackView::Whole,
    }
}

/// What the cached series catalog was built for: the document generation, its group count,
/// and [`CatalogCache::epoch`].
pub(super) type CatalogStamp = (u64, usize, u64);

/// The series catalog of the review's current state.
#[derive(Default)]
pub(super) struct CatalogCache {
    /// Bumped whenever the groups, sources, or series descriptions the catalog is built from
    /// change outside a document replacement.
    epoch: u64,
    built: RefCell<Option<(CatalogStamp, Arc<Vec<Series>>)>>,
}

#[derive(Clone, Default)]
pub(super) struct Series {
    pub id: String,
    pub commits: bool,
    pub members: Vec<ReviewGroupKey>,
    pub combined: Option<ReviewGroupKey>,
}

impl Series {
    pub fn contains(&self, selector: &str) -> bool {
        self.members
            .iter()
            .chain(&self.combined)
            .any(|key| key.selector() == selector)
    }

    pub fn member_id<'a>(&'a self, key: &'a ReviewGroupKey) -> &'a str {
        match key {
            ReviewGroupKey::Member { member, .. } => member,
            ReviewGroupKey::Standalone { source_id } => source_id,
            ReviewGroupKey::Combined { .. } => &self.id,
        }
    }

    pub fn views(&self) -> Vec<View> {
        let mut views = Vec::new();
        if !self.members.is_empty() {
            views.push(View::OneMember);
        }
        if self.members.len() > 1 {
            views.push(View::AllMembers);
        }
        if self.combined.is_some() || (self.commits && self.members.len() > 1) {
            views.push(View::Combined);
        }
        views
    }

    /// The view the series is asked to show: the saved choice, else the default (the view a
    /// commit series resolved with, the stack view for a stack), limited to [`Self::views`].
    /// A combined diff that cannot be computed is never asked for. While a combined diff is
    /// still being computed this is already the combined view, so choices made meanwhile build
    /// on it.
    pub fn requested_view(&self, surface: &ReviewSurface) -> Option<View> {
        let state = surface.commit_series.get(&self.id);
        let wanted = surface.series_preference(&self.id).map_or_else(
            || {
                if self.commits {
                    state.map_or(View::AllMembers, |state| state.view)
                } else {
                    from_stack(surface.stack_view)
                }
            },
            |saved| saved.view,
        );
        let unavailable = state.is_some_and(|state| state.unavailable.is_some());
        if self.commits && wanted == View::Combined && unavailable {
            return Some(View::AllMembers);
        }
        let views = self.views();
        views
            .contains(&wanted)
            .then_some(wanted)
            .or_else(|| views.first().copied())
    }

    /// The view the document shows: [`Self::requested_view`], except that a commit series
    /// keeps showing every member until its combined diff is in the document.
    pub fn view(&self, surface: &ReviewSurface) -> Option<View> {
        let wanted = self.requested_view(surface)?;
        if wanted == View::Combined
            && self.commits
            && !self
                .combined
                .as_ref()
                .is_some_and(|key| surface.has_group(key))
        {
            return Some(View::AllMembers);
        }
        Some(wanted)
    }

    pub fn selected(&self, surface: &ReviewSurface) -> Option<&ReviewGroupKey> {
        let saved = surface
            .series_preference(&self.id)
            .and_then(|p| p.last_member.as_deref());
        let remembered =
            saved.and_then(|id| self.members.iter().find(|key| self.member_id(key) == id));
        let cursor = surface
            .review
            .as_ref()
            .and_then(|review| review.doc.group_of_key(review.cursor.as_ref()?));
        let current = cursor.and_then(|key| self.members.iter().find(|member| *member == key));
        let rail = surface
            .rail_layer
            .as_deref()
            .and_then(|id| self.members.iter().find(|key| key.selector() == id));
        // A refresh may prepend newly discovered PRs. The first member of the new
        // document is not necessarily the one that was being read. In all-member
        // mode ordinary cursor motion chooses the open group; in one-member mode
        // an explicit saved choice takes precedence over the initial cursor.
        if self.view(surface) == Some(View::AllMembers) {
            current
                .or(remembered)
                .or(rail)
                .or_else(|| self.members.first())
        } else {
            remembered
                .or(rail)
                .or(current)
                .or_else(|| self.members.first())
        }
    }
}

impl ReviewSurface {
    pub(super) fn series_menu_state(&self) -> Option<crate::compare_menu::SeriesMenuState> {
        let series = self.active_series()?;
        if series.views().len() < 2 {
            return None;
        }
        let view = series.requested_view(self)?;
        let empty_combined = view == View::Combined
            && self.review.as_ref()?.doc.groups().any(|g| {
                Some(g.key) == series.combined.as_ref()
                    && !g.loading
                    && !g.alternate
                    && g.file_count == 0
                    && !self.source_notes.failed.contains(&g.source.source_id)
            });
        Some(crate::compare_menu::SeriesMenuState {
            view,
            commits: series.commits,
            empty_combined,
            unavailable: self
                .commit_series
                .get(&series.id)
                .and_then(|s| s.unavailable.clone()),
        })
    }

    pub(super) fn seed_stack_series_view(&mut self, view: View) {
        let news = self.stack_news.as_ref();
        let Some(id) = self
            .sources
            .iter()
            .find(|s| match &s.kind {
                ReviewSourceKind::Stack {
                    provider,
                    repository,
                    top,
                } => news.is_some_and(|news| {
                    provider == &news.provider
                        && repository == &news.repository
                        && news
                            .stack
                            .layers
                            .last()
                            .is_some_and(|layer| layer.pull.number == *top)
                }),
                _ => false,
            })
            .map(|s| s.id.clone())
        else {
            return;
        };
        let state = self
            .workspace_mut()
            .presentation
            .get_or_insert_with(Default::default);
        // Filling in the missing layer sources supplies a default, not a new
        // choice. Honor explicit legacy choices before seeding per-series state.
        let view = state.stack_view.map_or(
            if state.all_stack_layers {
                View::AllMembers
            } else {
                view
            },
            from_stack,
        );
        let saved = state
            .series_views
            .entry(id)
            .or_insert(ReviewSeriesPresentation {
                view,
                last_member: None,
            });
        let legacy = to_stack(saved.view);
        state.set_stack_view(legacy);
        self.stack_view = legacy;
    }

    pub(super) fn schedule_series_reload(&mut self) {
        // Invalidate the old generation now, including queued deliveries preceding the next tick.
        if let Some(cancel) = self.opening.0.take() {
            cancel.cancel();
        }
        self.switching = true;
        self.reopen_pending = true;
    }

    pub(super) fn series_preference(&self, id: &str) -> Option<&ReviewSeriesPresentation> {
        self.workspace
            .as_ref()?
            .presentation
            .as_ref()?
            .series_views
            .get(id)
    }

    /// Whether the document holds group `key` (loaded or still loading).
    pub(super) fn has_group(&self, key: &ReviewGroupKey) -> bool {
        self.review
            .as_ref()
            .is_some_and(|review| review.doc.groups().any(|group| group.key == key))
    }

    /// Note that the groups, sources, or series descriptions the catalog is built from
    /// changed.
    pub(super) const fn invalidate_series(&mut self) {
        self.series_cache.epoch = self.series_cache.epoch.wrapping_add(1);
    }

    /// Identifies the document's groups and the series descriptions: changes when a
    /// document replaces the previous one, a group arrives, or a description changes.
    pub(super) fn catalog_stamp(&self) -> CatalogStamp {
        (
            self.generation,
            self.review
                .as_ref()
                .map_or(0, |review| review.doc.groups().count()),
            self.series_cache.epoch,
        )
    }

    /// Every series of the review: each commit series and each stack of pull requests, with
    /// its members and combined representation. Built once per document state (see
    /// [`Self::invalidate_series`]); painting reads it several times per frame.
    pub(super) fn series_catalog(&self) -> Arc<Vec<Series>> {
        let stamp = self.catalog_stamp();
        if let Some((built, catalog)) = self.series_cache.built.borrow().as_ref()
            && *built == stamp
        {
            return Arc::clone(catalog);
        }
        let catalog = Arc::new(self.build_series_catalog());
        *self.series_cache.built.borrow_mut() = Some((stamp, Arc::clone(&catalog)));
        catalog
    }

    fn build_series_catalog(&self) -> Vec<Series> {
        let Some(review) = &self.review else {
            return Vec::new();
        };
        let mut sets = BTreeMap::new();
        // Match PR sources by provider/repository and the numbers in the resolved chain,
        // not merely by the presence of stack metadata in the document.
        let mut chains = Vec::new();
        for g in review.doc.groups() {
            let Some(stack) = g.stack else { continue };
            let Some(owner) = self.sources.iter().find(|s| s.id == g.source.source_id) else {
                continue;
            };
            let (ReviewSourceKind::Stack {
                provider,
                repository,
                ..
            }
            | ReviewSourceKind::StackLayer {
                provider,
                repository,
                ..
            }) = &owner.kind
            else {
                continue;
            };
            chains.push((owner, provider, repository, stack));
        }
        for group in review.doc.groups() {
            let owner = &group.source.source_id;
            let Some((_, provider, repository, stack)) =
                chains.iter().find(|(s, ..)| s.id == *owner)
            else {
                continue;
            };
            let Some(top) = stack.stack.layers.last() else {
                continue;
            };
            let chain = chains
                .iter()
                .filter(|(_, p, r, candidate)| {
                    p == provider
                        && r == repository
                        && candidate
                            .stack
                            .layers
                            .iter()
                            .any(|layer| layer.number == top.number)
                })
                .max_by_key(|(_, _, _, candidate)| (candidate.whole, candidate.stack.layers.len()));
            let id = if stack.whole {
                owner
            } else {
                chain.map_or(owner, |(source, ..)| &source.id)
            };
            let series = sets.entry(id.clone()).or_insert_with(|| Series {
                id: id.clone(),
                ..Series::default()
            });
            if stack.whole {
                series.combined = Some(group.key.clone());
            } else {
                series.members.push(group.key.clone());
            }
        }
        self.add_commit_series(&mut sets);
        sets.into_values().collect()
    }

    /// Each selected-commits source that resolved as a series, with every member it
    /// describes (listed or not) and its combined diff.
    fn add_commit_series(&self, sets: &mut BTreeMap<String, Series>) {
        for (id, state) in &self.commit_series {
            if state.members.len() < 2 || !self.sources.iter().any(|s| s.id == *id) {
                continue;
            }
            sets.insert(
                id.clone(),
                Series {
                    id: id.clone(),
                    commits: true,
                    members: state
                        .members
                        .iter()
                        .map(|(oid, _)| ReviewGroupKey::Member {
                            source_id: id.clone(),
                            member: oid.clone(),
                        })
                        .collect(),
                    combined: Some(ReviewGroupKey::Combined {
                        source_id: id.clone(),
                    }),
                },
            );
        }
    }

    /// The series the rail shows: the one the cursor is in, else the one last entered, else
    /// the first. Reading another source of the review keeps the rail where it was.
    pub(super) fn active_series(&self) -> Option<Series> {
        let catalog = self.series_catalog();
        let holding = |id: &str| catalog.iter().position(|set| set.contains(id));
        let cursor = self
            .review
            .as_ref()
            .and_then(|r| r.doc.group_of_key(r.cursor.as_ref()?))
            .map(ReviewGroupKey::selector);
        let index = cursor
            .as_deref()
            .and_then(holding)
            .or_else(|| self.rail_layer.as_deref().and_then(holding))
            .unwrap_or(0);
        catalog.get(index).cloned()
    }

    /// The effective grouping of the series at the cursor.
    pub fn shown_series_view(&self) -> Option<View> {
        self.active_series()?.view(self)
    }

    /// The grouping asked for the series at the cursor (see [`Series::requested_view`]).
    pub(super) fn requested_series_view(&self) -> Option<View> {
        self.active_series()?.requested_view(self)
    }

    pub(super) fn remember_series(
        &mut self,
        series: &Series,
        view: View,
        member: Option<&ReviewGroupKey>,
    ) {
        let last_member = member
            .map(|key| series.member_id(key).to_owned())
            .or_else(|| {
                self.series_preference(&series.id)
                    .and_then(|saved| saved.last_member.clone())
            })
            .or_else(|| {
                series
                    .selected(self)
                    .map(|key| series.member_id(key).to_owned())
            });
        let state = ReviewSeriesPresentation { view, last_member };
        self.workspace_mut()
            .presentation
            .get_or_insert_with(Default::default)
            .series_views
            .insert(series.id.clone(), state);
        if !series.commits {
            self.stack_view = to_stack(view);
        }
        self.request_save();
    }

    /// Land in [`ReviewSurface::pending_member`] once it is in the document and shown: stay
    /// where the cursor is when it is already in that group (a view change keeps the file),
    /// else return to the group's remembered position, else its header or first file.
    pub(super) fn finish_member_jump(&mut self) {
        let Some(selector) = self.pending_member.clone() else {
            return;
        };
        let catalog = self.series_catalog();
        let Some(series) = catalog.iter().find(|s| s.contains(&selector)) else {
            return;
        };
        let Some(review) = self.review.as_mut() else {
            return;
        };
        let target = review
            .doc
            .groups()
            .find(|g| !g.hidden && !g.alternate && g.key.selector() == selector)
            .and_then(|g| {
                review
                    .cursor
                    .as_ref()
                    .filter(|key| review.doc.group_of_key(key) == Some(g.key))
                    .cloned()
                    .or_else(|| {
                        self.layer_positions
                            .get(&selector)
                            .and_then(|p| review.doc.resolve_persisted(p))
                            .filter(|key| review.doc.group_of_key(key) == Some(g.key))
                    })
                    .or(g.header)
                    .or_else(|| {
                        review
                            .doc
                            .group_files(g.index)
                            .next()
                            .cloned()
                            .map(bcode_code_review::document::ItemKey::FileHeader)
                    })
            });
        if let Some(key) = target
            && let Some(jump) = review.doc.jump_to(&key)
        {
            if review.cursor.as_ref() != Some(&key) {
                review.set_cursor(
                    jump.index,
                    bmux_tui_components::virtual_list::AnchorPolicy::ItemTopAtTop,
                );
                review.pending_jump = jump.needs_load.map(|_| key);
            }
            review.open_only_group(&selector, series);
            self.pending_member = None;
        } else if let Some(reason) = self.commit_series.values().find_map(|s| {
            let combined = ReviewGroupKey::Combined {
                source_id: s.source_id.clone(),
            };
            (combined.selector() == selector)
                .then(|| s.unavailable.clone())
                .flatten()
        }) {
            self.pending_member = None;
            self.set_status(
                reason,
                bmux_tui_components::status_bar::StatusSeverity::Info,
            );
        }
    }

    /// The load that should have brought [`ReviewSurface::pending_member`] completed: stop
    /// waiting for it, so a later refresh or arrival does not move the cursor.
    pub(super) fn settle_member_jump(&mut self) {
        self.finish_member_jump();
        self.pending_member = None;
    }

    /// Select a loaded member of the active series by full commit OID, PR source id, or group selector.
    pub fn select_series_member(&mut self, member: &str) -> bool {
        let Some(series) = self.active_series() else {
            return false;
        };
        let Some(key) = series
            .members
            .iter()
            .find(|k| series.member_id(k) == member || k.selector() == member)
        else {
            return false;
        };
        let entries = self.rail_entries();
        let Some(index) = entries.iter().position(|e| e.source_id == key.selector()) else {
            return false;
        };
        self.enter_layer(&entries, self.current_layer(&entries), index);
        true
    }
}
