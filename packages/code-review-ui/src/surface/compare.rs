//! Comparisons on the review screen (see `docs/review-stack-history.md`).
//!
//! A review of pull request layers compares each layer, in place, with a baseline: by default
//! the version the reviewer last looked at (from the looks recorded as the reviewer works,
//! seeded from completed passes and the reviewer's own reviews on the forge, pinned for the
//! session); else the version before the stack's latest push, the versions at some time or
//! between two pushes, or two versions picked for one pull request. Each layer's delta is
//! computed in one batched call and laid over the document; nothing about the review's sources
//! changes. Looks are recorded when a file is marked viewed, a comment or AI question is made, a
//! pass is completed, or a layer stays current for the dwell time.

use super::super::loader::Message;
use super::ReviewSurface;
use crate::compare_bar::{BannerPlan, BarPlan};
use crate::config::compare::{BaselineLook, RebasedLayers, StartView};
use crate::context::UiContext;
use bcode_code_review::document::{CompareView, FileKey, GroupComparison};
use bcode_code_review::history::looks::{self, BaselineRule, LastLooked};
use bcode_code_review::history::version_at;
use bcode_code_review_models::{
    ChangeKind, DeltaKind, LayerDelta, LayerLook, LayerPair, LayerTimeline, ListLayerLooksRequest,
    ListLayerLooksResponse, LookKind, RecordLayerLooksRequest, RecordLayerLooksResponse,
    ReviewCompareBaseline as Baseline, ReviewCompareLayersRequest, ReviewCompareLayersResponse,
    ReviewVersionsKeepRequest, ReviewVersionsKeepResponse, StackTimeline, VersionPoint,
};
use bmux_tui::geometry::Rect;
use bmux_tui_components::status_bar::StatusSeverity;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

/// Whether the banner of a changed review shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum BannerState {
    /// Not decided yet: it is, once every layer's comparison is in.
    #[default]
    Undecided,
    /// Shown.
    Shown,
    /// Hidden (dismissed, or nothing changed).
    Hidden,
}

/// The push timeline while it has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelineFocus {
    /// The push the keyboard is on.
    pub cursor: usize,
    /// The column compared from, when set (column `c` is just before push `c`).
    pub from: Option<usize>,
    /// The column compared to, when set (the number of pushes is now).
    pub to: Option<usize>,
}

/// A source compared: its pair and whether it ends at an older version than the source shows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Compared {
    pair: LayerPair,
    historical: bool,
}

/// The review's comparison state.
#[derive(Debug, Default)]
pub struct Comparing {
    /// Whether the looks were asked for (once per review).
    started: bool,
    /// The reviewer's looks at the review's layers, once listed.
    looks: Option<Vec<LayerLook>>,
    /// Layers whose looks were asked for.
    asked: BTreeSet<u64>,
    /// Layers whose looks were listed.
    listed: BTreeSet<u64>,
    /// Each layer's last-looked baseline, pinned when first resolved.
    baselines: BTreeMap<u64, LastLooked>,
    /// The pair each compared source shows (or is computing), by source id.
    pairs: BTreeMap<String, Compared>,
    /// How compared sources show their files.
    view: CompareView,
    /// What layers are compared with.
    baseline: Baseline,
    /// Pairs picked for single pull requests, over the baseline, by number.
    picks: BTreeMap<u64, LayerPair>,
    /// Only pull requests with real changes are shown.
    only_changed: bool,
    /// The banner of a changed review.
    banner: BannerState,
    /// Looks recorded in this session, by layer, head, and kind.
    recorded: BTreeSet<(u64, String, LookKind)>,
    /// Looks waiting to be written.
    pending: Vec<LayerLook>,
    /// The review's layers may have moved: compare them again.
    moved: bool,
    /// The layer current since when, for `opened` looks: number, head, since.
    dwell: Option<(u64, String, Instant)>,
    /// Layers whose version steps were asked for, with the head they were asked at.
    steps_asked: BTreeSet<(u64, String)>,
    /// Each layer's version steps, by number, once computed (they belong to the layer, not to
    /// what it is compared with).
    steps: BTreeMap<u64, Vec<LayerDelta>>,
    /// The push timeline while it has the keyboard.
    pub(super) timeline: Option<TimelineFocus>,
    /// The comparison row as last painted.
    pub(super) bar: Option<(Rect, BarPlan)>,
    /// The banner as last painted.
    pub(super) banner_plan: Option<(Rect, BannerPlan)>,
}

impl Comparing {
    /// How compared sources show their files.
    #[must_use]
    pub const fn view(&self) -> CompareView {
        self.view
    }

    /// Whether some source is compared.
    #[must_use]
    pub fn active(&self) -> bool {
        !self.pairs.is_empty()
    }

    /// What layers are compared with.
    #[must_use]
    pub const fn baseline(&self) -> Baseline {
        self.baseline
    }

    /// Whether only pull requests with real changes are shown.
    #[must_use]
    pub const fn only_changed(&self) -> bool {
        self.only_changed
    }

    /// Whether the banner of a changed review shows.
    #[must_use]
    pub fn banner_shown(&self) -> bool {
        self.banner == BannerState::Shown
    }

    /// Pairs picked for single pull requests, by number.
    #[must_use]
    pub const fn picks(&self) -> &BTreeMap<u64, LayerPair> {
        &self.picks
    }

    /// A source was listed again (or added): its layer may have moved.
    pub(super) const fn layers_moved(&mut self) {
        self.moved = true;
    }

    /// The document was replaced: it has no overlays, so every layer is laid over again.
    pub(super) fn document_replaced(&mut self) {
        self.pairs.clear();
        self.moved = true;
    }

    /// Whether the layers may have moved since last compared (and forget it).
    pub(super) const fn take_moved(&mut self) -> bool {
        std::mem::replace(&mut self.moved, false)
    }

    /// The looks of layer `number`.
    pub(super) fn looks_of(&self, number: u64) -> Vec<LayerLook> {
        self.looks
            .iter()
            .flatten()
            .filter(|look| look.number == number)
            .cloned()
            .collect()
    }
}

/// One step-through layer of the review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LayerSource {
    pub source_id: String,
    pub number: u64,
    pub head: String,
    pub fork: String,
}

impl LayerSource {
    fn current(&self) -> VersionPoint {
        VersionPoint {
            head_oid: self.head.clone(),
            fork_oid: Some(self.fork.clone()),
        }
    }
}

/// What a rule makes of one layer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rule {
    /// Compare this pair.
    Pair(LayerPair),
    /// Not known yet (the history is still loading).
    Wait,
    /// Not compared.
    None,
}

/// The point of `layer`'s version `index`: the current version when it is the head the review
/// shows (a newer version than the review shows is compared as recorded, read-only).
pub(super) fn point_of(layer: &LayerTimeline, index: usize, current: &LayerSource) -> VersionPoint {
    let version = &layer.versions[index];
    if version.version.head_oid == current.head {
        return current.current();
    }
    VersionPoint {
        head_oid: version.version.head_oid.clone(),
        fork_oid: version.fork_oid.clone(),
    }
}

/// The pair comparing `layer` from its version at `from_ms` to its version at `to_ms` (`None`:
/// now) by `timeline`. A layer that did not exist at `from_ms` is new; one that did not exist
/// at `to_ms` is not compared.
fn between(
    timeline: Option<&StackTimeline>,
    layer: &LayerSource,
    from_ms: u64,
    to_ms: Option<u64>,
) -> Rule {
    let Some(timeline) = timeline else {
        return Rule::Wait;
    };
    let Some(versions) = timeline
        .layers
        .iter()
        .find(|timeline| timeline.number == layer.number)
    else {
        return Rule::None;
    };
    let to = match to_ms {
        None => layer.current(),
        Some(to_ms) => match version_at(versions, to_ms) {
            Some(index) => point_of(versions, index, layer),
            None => return Rule::None,
        },
    };
    let from = version_at(versions, from_ms).map(|index| point_of(versions, index, layer));
    Rule::Pair(LayerPair {
        number: layer.number,
        from,
        to,
    })
}

/// The time just before the stack's latest push.
fn before_last_push(timeline: &StackTimeline) -> Option<u64> {
    timeline
        .pushes
        .last()
        .map(|push| push.at_ms.saturating_sub(1))
}

/// The time column `column` of `timeline` stands for: just before push `column`, or `None`
/// (now) for the last.
pub(super) fn column_time(timeline: &StackTimeline, column: usize) -> Option<u64> {
    timeline
        .pushes
        .get(column)
        .map(|push| push.at_ms.saturating_sub(1))
}

impl ReviewSurface {
    /// The review's pull request layers: step-through stack-layer sources, by their top layer.
    pub(super) fn layer_sources(&self) -> Vec<LayerSource> {
        let Some(review) = self.review.as_ref() else {
            return Vec::new();
        };
        review
            .doc
            .groups()
            .filter(|group| !group.loading && group.source.part.is_none())
            .filter_map(|group| {
                let stack = group.stack.filter(|stack| !stack.whole)?;
                let layer = stack.stack.layers.last()?;
                Some(LayerSource {
                    source_id: group.source.source_id.clone(),
                    number: layer.number,
                    head: layer.head_oid.clone(),
                    fork: layer.base_oid.clone(),
                })
            })
            .collect()
    }

    /// The provider and repository the review's pull requests belong to.
    pub(super) fn pulls_scope(&self) -> Option<(String, String)> {
        let news = self.stack_news.as_ref()?;
        Some((news.provider.clone(), news.repository.clone()))
    }

    fn review_id(&self) -> Option<String> {
        (!self.is_transient())
            .then(|| {
                self.workspace
                    .as_ref()
                    .map(|workspace| workspace.id.clone())
            })
            .flatten()
    }

    /// Whether the review compares pull requests: it has layers and knows their repository.
    pub(super) fn compares_pulls(&self) -> bool {
        self.comparing.started && self.pulls_scope().is_some() && !self.layer_sources().is_empty()
    }

    /// Ask for the reviewer's looks at the review's layers once its stack is known, seeding
    /// them first from the passes this review recorded; restore the baseline the review last
    /// compared with (else the configured one).
    pub(super) fn start_comparison(&mut self, host: &UiContext) {
        if self.comparing.started || self.pulls_scope().is_none() {
            return;
        }
        let layers = self.layer_sources();
        if layers.is_empty() {
            return;
        }
        self.comparing.started = true;
        let config = self.services.compare_config();
        let presentation = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.presentation.as_ref());
        self.comparing.baseline = presentation
            .and_then(|state| state.compare.baseline)
            .unwrap_or_else(|| {
                config
                    .default_baseline
                    .baseline(super::super::history::now_ms())
            });
        self.comparing.only_changed = presentation.is_some_and(|state| state.compare.only_changed)
            || config.rebased_layers == RebasedLayers::Hide;
        self.comparing.view = match config.start_view {
            StartView::Full => CompareView::Full,
            StartView::Changes => CompareView::Changes,
        };
        self.list_looks(host);
    }

    /// List the reviewer's looks at the layers not asked about yet (layers arrive after the
    /// first frame), seeding them first from the passes this review recorded.
    fn list_looks(&mut self, host: &UiContext) {
        let Some((provider, repository)) = self.pulls_scope() else {
            return;
        };
        let layers: Vec<LayerSource> = self
            .layer_sources()
            .into_iter()
            .filter(|layer| !self.comparing.asked.contains(&layer.number))
            .collect();
        if layers.is_empty() {
            return;
        }
        let reviewed: Vec<(u64, bcode_code_review_models::ReviewedRevision)> = self
            .workspace
            .as_ref()
            .map(|workspace| {
                layers
                    .iter()
                    .filter_map(|layer| {
                        let revision = workspace.reviewed_revisions.get(&layer.source_id)?;
                        Some((layer.number, revision.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let seeded = looks::pass_looks(
            &provider,
            &repository,
            &reviewed,
            self.review_id().as_deref(),
        );
        let numbers: Vec<u64> = layers.iter().map(|layer| layer.number).collect();
        self.comparing.asked.extend(numbers.iter().copied());
        let (services, inbox, repo) = (
            self.services.clone(),
            self.inbox.clone(),
            self.repo_path.clone(),
        );
        host.spawn(Box::pin(async move {
            if !seeded.is_empty() {
                let _: Result<RecordLayerLooksResponse, _> = services
                    .call(RecordLayerLooksRequest {
                        repo_path: repo.clone(),
                        looks: seeded,
                    })
                    .await;
            }
            let listed: Result<ListLayerLooksResponse, _> = services
                .call(ListLayerLooksRequest {
                    repo_path: repo,
                    provider,
                    repository,
                    numbers: numbers.clone(),
                })
                .await;
            inbox.push(Message::Looks(
                numbers,
                listed
                    .map(|listed| listed.looks)
                    .map_err(|error| error.message),
            ));
        }));
    }

    /// The looks at layers `numbers` arrived: compare those layers with their baselines.
    pub(super) fn apply_looks(
        &mut self,
        numbers: &[u64],
        result: Result<Vec<LayerLook>, String>,
        host: &UiContext,
    ) {
        match result {
            Ok(looks) => {
                self.comparing.listed.extend(numbers.iter().copied());
                self.comparing
                    .looks
                    .get_or_insert_with(Vec::new)
                    .extend(looks);
                self.refresh_comparison(host);
            }
            Err(error) => self.set_status(
                format!("could not read what you looked at before: {error}"),
                StatusSeverity::Warning,
            ),
        }
    }

    /// The last-looked rule for `layer`: its pinned baseline (a layer that moved while open is
    /// compared with the head it had).
    fn last_looked_pair(&mut self, layer: &LayerSource) -> Option<LayerPair> {
        let rule = match self.services.compare_config().baseline_look {
            BaselineLook::Engaged => BaselineRule::Engaged,
            BaselineLook::Opened => BaselineRule::Opened,
        };
        let looks = self.comparing.looks.as_ref()?;
        let baseline = self
            .comparing
            .baselines
            .entry(layer.number)
            .or_insert_with(|| looks::last_looked(looks, layer.number, &layer.head, rule));
        if let LastLooked::Unchanged(look) = baseline
            && look.head_oid != layer.head
        {
            *baseline = LastLooked::Since(look.clone());
        }
        let baseline = baseline.clone();
        let timeline = self.stack_history.as_ref().and_then(|history| {
            history
                .layers
                .iter()
                .find(|timeline| timeline.number == layer.number)
        });
        Some(looks::baseline_pair(
            timeline,
            layer.number,
            &baseline,
            layer.current(),
        ))
    }

    /// What the rule in effect makes of `layer`.
    fn rule_for(&mut self, layer: &LayerSource) -> Rule {
        if let Some(pick) = self.comparing.picks.get(&layer.number) {
            return Rule::Pair(pick.clone());
        }
        match self.comparing.baseline {
            Baseline::Off => Rule::None,
            Baseline::LastLooked => {
                if self.comparing.listed.contains(&layer.number) {
                    self.last_looked_pair(layer).map_or(Rule::None, Rule::Pair)
                } else {
                    Rule::Wait
                }
            }
            Baseline::PreviousPush => self.stack_history.as_ref().map_or(Rule::Wait, |history| {
                before_last_push(history).map_or(Rule::None, |from_ms| {
                    between(Some(history), layer, from_ms, None)
                })
            }),
            Baseline::Between { from_ms, to_ms } => {
                between(self.stack_history.as_ref(), layer, from_ms, to_ms)
            }
        }
    }

    /// Compare each layer by the rule in effect, computing only the pairs that changed (a
    /// layer that moved, a baseline chosen, or a source listed since). With the last-looked
    /// rule nothing is compared when the reviewer never looked at any layer.
    pub(super) fn refresh_comparison(&mut self, host: &UiContext) {
        let layers = self.layer_sources();
        if layers.is_empty() || !self.comparing.started {
            return;
        }
        self.list_looks(host);
        let mut wanted: Vec<(LayerSource, Rule)> = Vec::new();
        for layer in layers {
            let rule = self.rule_for(&layer);
            wanted.push((layer, rule));
        }
        if self.comparing.baseline == Baseline::LastLooked
            && !self
                .comparing
                .baselines
                .values()
                .any(|baseline| baseline.look().is_some())
        {
            // Never looked at any layer: a first visit shows the review as it is (but for the
            // pull requests whose versions were picked).
            for (layer, rule) in &mut wanted {
                if matches!(rule, Rule::Pair(_))
                    && !self.comparing.picks.contains_key(&layer.number)
                {
                    *rule = Rule::None;
                }
            }
        }
        let view = self.comparing.view;
        let mut asked = Vec::new();
        for (layer, rule) in wanted {
            let pair = match rule {
                Rule::Wait => continue,
                Rule::None => {
                    if self.comparing.pairs.remove(&layer.source_id).is_some()
                        && let Some(review) = self.review.as_mut()
                    {
                        review.doc.set_comparison(&layer.source_id, None);
                    }
                    continue;
                }
                Rule::Pair(pair) => pair,
            };
            let compared = Compared {
                historical: pair.to.head_oid != layer.head,
                pair,
            };
            if self.comparing.pairs.get(&layer.source_id) == Some(&compared) {
                continue;
            }
            let steps = self
                .comparing
                .steps
                .get(&layer.number)
                .cloned()
                .unwrap_or_default();
            if let Some(review) = self.review.as_mut() {
                review.doc.set_comparison(
                    &layer.source_id,
                    Some(GroupComparison {
                        delta: None,
                        view,
                        historical: compared.historical,
                        steps,
                        ..GroupComparison::default()
                    }),
                );
            }
            if !asked.contains(&compared.pair) {
                asked.push(compared.pair.clone());
            }
            self.comparing.pairs.insert(layer.source_id, compared);
        }
        self.apply_compare_filter();
        if let Some(review) = self.review.as_mut() {
            review.note_splices();
        }
        if asked.is_empty() {
            return;
        }
        // The current layer first, so its comparison lands first.
        if let Some(current) = self.current_layer_number() {
            asked.sort_by_key(|pair| pair.number != current);
        }
        let (services, inbox, repo) = (
            self.services.clone(),
            self.inbox.clone(),
            self.repo_path.clone(),
        );
        host.spawn(Box::pin(async move {
            let result: Result<ReviewCompareLayersResponse, _> = services
                .call(ReviewCompareLayersRequest {
                    repo_path: repo,
                    pairs: asked,
                })
                .await;
            inbox.push(Message::Compared(
                result
                    .map(|response| response.deltas)
                    .map_err(|error| error.message),
            ));
        }));
    }

    /// The pull request number of the layer the cursor is in.
    pub(super) fn current_layer_number(&self) -> Option<u64> {
        let entries = self.rail_entries();
        let source = entries
            .get(self.current_layer(&entries)?)?
            .source_id
            .clone();
        self.layer_sources()
            .into_iter()
            .find(|layer| layer.source_id == source)
            .map(|layer| layer.number)
    }

    /// Layer deltas arrived: lay each over the sources still showing its pair.
    pub(super) fn apply_compared(&mut self, result: Result<Vec<LayerDelta>, String>) {
        let view = self.comparing.view;
        let deltas = match result {
            Ok(deltas) => deltas,
            Err(error) => {
                // The layers still computing show the full change.
                let pending: Vec<String> = self.comparing.pairs.keys().cloned().collect();
                if let Some(review) = self.review.as_mut() {
                    for source_id in &pending {
                        if review
                            .doc
                            .comparison(source_id)
                            .is_some_and(|comparison| comparison.delta.is_none())
                        {
                            review.doc.set_comparison(source_id, None);
                            self.comparing.pairs.remove(source_id);
                        }
                    }
                    review.note_splices();
                }
                self.set_status(
                    format!("could not compare: {error}"),
                    StatusSeverity::Warning,
                );
                return;
            }
        };
        let show_rebased = self.services.compare_config().rebased_layers == RebasedLayers::Show;
        let (theme, hide_viewed) = (self.theme, self.hide_viewed_files);
        let Some(review) = self.review.as_mut() else {
            return;
        };
        for delta in deltas {
            for (source_id, compared) in &self.comparing.pairs {
                if compared.pair == delta.pair {
                    let steps = self
                        .comparing
                        .steps
                        .get(&compared.pair.number)
                        .cloned()
                        .unwrap_or_default();
                    review.doc.set_comparison(
                        source_id,
                        Some(GroupComparison {
                            delta: Some(delta.clone()),
                            view,
                            historical: compared.historical,
                            steps,
                            ..GroupComparison::default()
                        }),
                    );
                    if show_rebased && delta.kind == DeltaKind::RebasedOnly {
                        review.doc.set_compare_files_shown(source_id, true);
                    }
                }
            }
        }
        review.note_splices();
        // The file panel's group rows carry each layer's badge.
        review.rebuild_files(&theme, hide_viewed);
        self.apply_compare_filter();
        self.decide_banner();
    }

    /// Once every compared layer's delta is in: show the banner when, compared with what the
    /// reviewer last looked at, some layer moved.
    fn decide_banner(&mut self) {
        let all_listed = self
            .layer_sources()
            .iter()
            .all(|layer| self.comparing.listed.contains(&layer.number));
        if self.comparing.banner != BannerState::Undecided
            || self.compare_pending() > 0
            || !all_listed
        {
            return;
        }
        let counts = self.compare_counts();
        let moved = counts.changed + counts.rebased + counts.unavailable > 0;
        self.comparing.banner = if self.comparing.baseline == Baseline::LastLooked
            && self.comparing.picks.is_empty()
            && moved
        {
            BannerState::Shown
        } else {
            BannerState::Hidden
        };
    }

    /// Hide the banner (the comparison stays).
    pub(super) const fn dismiss_compare_banner(&mut self) {
        self.comparing.banner = BannerState::Hidden;
    }

    /// Compared sources whose delta is still computing.
    pub(super) fn compare_pending(&self) -> usize {
        let Some(review) = self.review.as_ref() else {
            return 0;
        };
        self.comparing
            .pairs
            .keys()
            .filter(|source| {
                review
                    .doc
                    .comparison(source)
                    .is_some_and(|comparison| comparison.delta.is_none())
            })
            .count()
    }

    /// How many compared layers found what.
    pub(super) fn compare_counts(&self) -> CompareCounts {
        let mut counts = CompareCounts {
            layers: self.layer_sources().len(),
            ..CompareCounts::default()
        };
        for source in self.comparing.pairs.keys() {
            match self.compared_kind(source) {
                None => counts.pending += 1,
                Some(DeltaKind::Changed) => counts.changed += 1,
                Some(DeltaKind::WithRebase) => {
                    counts.changed += 1;
                    counts.with_rebase += 1;
                }
                Some(DeltaKind::RebasedOnly) => counts.rebased += 1,
                Some(DeltaKind::Unchanged) => counts.unchanged += 1,
                Some(DeltaKind::NewToYou) => counts.new_to_you += 1,
                Some(DeltaKind::Unavailable) => counts.unavailable += 1,
            }
        }
        counts
    }

    /// The delta kind of source `source_id`'s comparison, once computed.
    pub(super) fn compared_kind(&self, source_id: &str) -> Option<DeltaKind> {
        self.review
            .as_ref()?
            .doc
            .comparison(source_id)?
            .delta
            .as_ref()
            .map(|delta| delta.kind)
    }

    /// Sources left out while only pull requests with real changes show.
    pub(super) fn compare_filtered(&self) -> BTreeSet<String> {
        if !self.comparing.only_changed {
            return BTreeSet::new();
        }
        self.comparing
            .pairs
            .keys()
            .filter(|source| {
                matches!(
                    self.compared_kind(source),
                    Some(DeltaKind::Unchanged | DeltaKind::RebasedOnly)
                )
            })
            .cloned()
            .collect()
    }

    /// Leave filtered sources out of the document when every layer shows in it (one layer at a
    /// time, the filter only changes which layers `.`/`,` visit).
    pub(super) fn apply_compare_filter(&mut self) {
        let filtered = if self.stack_view == super::stack::StackView::AllLayers {
            self.compare_filtered()
        } else {
            BTreeSet::new()
        };
        let theme = self.theme;
        let hide_viewed = self.hide_viewed_files;
        if let Some(review) = self.review.as_mut()
            && review.doc.set_filtered_sources(filtered)
        {
            review.note_splices();
            review.rebuild_files(&theme, hide_viewed);
        }
    }

    /// Switch every compared source between the full change and only what changed.
    pub(super) fn toggle_compare_view(&mut self) {
        if !self.comparing.active() {
            self.set_status(
                "nothing to compare: no layer changed since you last looked, or you never did \
                 (b compares with something else)",
                StatusSeverity::Info,
            );
            return;
        }
        let view = match self.comparing.view {
            CompareView::Full => CompareView::Changes,
            CompareView::Changes => CompareView::Full,
        };
        self.comparing.view = view;
        if let Some(review) = self.review.as_mut() {
            review.doc.set_compare_view(view);
            review.note_splices();
        }
        self.set_status(
            match view {
                CompareView::Changes => "showing only what changed",
                CompareView::Full => "showing the full change, what is new marked",
            },
            StatusSeverity::Info,
        );
    }

    /// Compare every layer by `baseline` (clearing single picks), kept for the next open.
    pub(super) fn set_compare_baseline(&mut self, baseline: Baseline, host: &UiContext) {
        self.comparing.baseline = baseline;
        self.comparing.picks.clear();
        self.comparing.timeline = None;
        if baseline != Baseline::LastLooked {
            self.comparing.banner = BannerState::Hidden;
        }
        if baseline != Baseline::Off && self.stack_history.is_none() {
            self.load_history_quietly(host);
        }
        self.refresh_comparison(host);
        self.persist_compare_choice();
        let label = self.baseline_labels()[0].clone();
        self.set_status(format!("changes since: {label}"), StatusSeverity::Info);
    }

    /// Compare pull request `number` alone between two of its versions (`from` and `to` are
    /// indices into its timeline's versions).
    pub(super) fn pick_versions(&mut self, number: u64, from: usize, to: usize, host: &UiContext) {
        let Some(layer) = self
            .layer_sources()
            .into_iter()
            .find(|layer| layer.number == number)
        else {
            return;
        };
        let Some(versions) = self
            .stack_history
            .as_ref()
            .and_then(|history| history.layers.iter().find(|layer| layer.number == number))
        else {
            return;
        };
        if from >= versions.versions.len() || to >= versions.versions.len() {
            return;
        }
        let pair = LayerPair {
            number,
            from: Some(point_of(versions, from, &layer)),
            to: point_of(versions, to, &layer),
        };
        let historical = pair.to.head_oid != layer.head;
        self.comparing.picks.insert(number, pair);
        self.comparing.banner = BannerState::Hidden;
        self.refresh_comparison(host);
        self.set_status(
            if historical {
                format!(
                    "comparing #{number} v{} → v{} (not current: read-only)",
                    from + 1,
                    to + 1
                )
            } else {
                format!("comparing #{number} v{} → now", from + 1)
            },
            StatusSeverity::Info,
        );
    }

    /// Show every layer as it was at `at_ms`: its whole change then, from its fork point then
    /// (an older version, read-only).
    pub(super) fn compare_stack_at(&mut self, at_ms: u64, host: &UiContext) {
        let Some(history) = self.stack_history.as_ref() else {
            return;
        };
        let mut picks = BTreeMap::new();
        for layer in self.layer_sources() {
            let Some(versions) = history
                .layers
                .iter()
                .find(|item| item.number == layer.number)
            else {
                continue;
            };
            let Some(index) = version_at(versions, at_ms) else {
                continue;
            };
            let to = point_of(versions, index, &layer);
            let Some(fork) = to.fork_oid.clone() else {
                continue;
            };
            // From the fork point itself: nothing of the layer's own change, so all of it shows.
            let from = VersionPoint {
                head_oid: fork.clone(),
                fork_oid: Some(fork),
            };
            picks.insert(
                layer.number,
                LayerPair {
                    number: layer.number,
                    from: Some(from),
                    to,
                },
            );
        }
        self.comparing.baseline = Baseline::Off;
        self.comparing.picks = picks;
        self.comparing.banner = BannerState::Hidden;
        self.refresh_comparison(host);
        self.set_status(
            format!(
                "the stack as it was {} (read-only where it differs from now)",
                super::super::history::ago(at_ms, super::super::history::now_ms())
            ),
            StatusSeverity::Info,
        );
    }

    /// Show only pull requests with real changes, or every one again.
    pub(super) fn toggle_only_changed(&mut self) {
        self.comparing.only_changed = !self.comparing.only_changed;
        self.apply_compare_filter();
        self.persist_compare_choice();
        let hidden = self.compare_filtered().len();
        self.set_status(
            if self.comparing.only_changed {
                format!("only pull requests with real changes ({hidden} hidden; . and , skip them)")
            } else {
                "every pull request".to_owned()
            },
            StatusSeverity::Info,
        );
    }

    fn persist_compare_choice(&mut self) {
        let (baseline, only_changed) = (self.comparing.baseline, self.comparing.only_changed);
        if let Some(workspace) = self.workspace.as_mut() {
            let state = workspace.presentation.get_or_insert_with(Default::default);
            state.compare.baseline = Some(baseline);
            state.compare.only_changed = only_changed;
        }
        self.persist_presentation(true);
    }

    /// The baseline in words: in full and briefly.
    pub(super) fn baseline_labels(&self) -> [String; 2] {
        use super::super::history::{ago, now_ms};
        let now = now_ms();
        if !self.comparing.picks.is_empty() {
            return if self.comparing.baseline == Baseline::Off {
                ["picked versions".to_owned(), "versions".to_owned()]
            } else {
                ["mixed".to_owned(), "mixed".to_owned()]
            };
        }
        match self.comparing.baseline {
            Baseline::LastLooked => {
                let newest = self
                    .comparing
                    .baselines
                    .values()
                    .filter_map(|baseline| match baseline {
                        LastLooked::Since(look) => Some(look.looked_at_ms),
                        _ => None,
                    })
                    .max();
                let looked = self
                    .comparing
                    .baselines
                    .values()
                    .any(|baseline| baseline.look().is_some());
                let detail = match newest {
                    Some(at) if at > 0 => format!(" · {}", ago(at, now)),
                    None if looked => " · nothing new".to_owned(),
                    None if self.comparing.looks.is_some() => " · not yet".to_owned(),
                    _ => String::new(),
                };
                [format!("you last looked{detail}"), "last looked".to_owned()]
            }
            Baseline::PreviousPush => {
                let when = self
                    .stack_history
                    .as_ref()
                    .and_then(|history| history.pushes.last())
                    .map(|push| format!(" · {}", ago(push.at_ms, now)))
                    .unwrap_or_default();
                [format!("previous push{when}"), "prev push".to_owned()]
            }
            Baseline::Between { from_ms, to_ms } => {
                let from = ago(from_ms, now);
                match to_ms {
                    None => [from.clone(), from],
                    Some(to_ms) => {
                        let to = ago(to_ms, now);
                        [format!("{from} → {to}"), format!("{from}→{to}")]
                    }
                }
            }
            Baseline::Off => ["full change".to_owned(), "off".to_owned()],
        }
    }

    /// Record a look of `kind` at the layer `file` belongs to (its current version).
    pub(super) fn record_look(&mut self, file: &FileKey, kind: LookKind) {
        self.record_layer_looks(Some(&file.source_id), kind);
    }

    /// Record a look of `kind` at the layer of source `source_id`, or at every layer. A layer
    /// compared up to an older version records nothing: the reviewer is not looking at the
    /// current one.
    pub(super) fn record_layer_looks(&mut self, source_id: Option<&str>, kind: LookKind) {
        let Some((provider, repository)) = self.pulls_scope() else {
            return;
        };
        let review_id = self.review_id();
        let now = super::annotate::now_ms();
        for layer in self.layer_sources() {
            if source_id.is_some_and(|source_id| source_id != layer.source_id) {
                continue;
            }
            if self
                .comparing
                .pairs
                .get(&layer.source_id)
                .is_some_and(|compared| compared.historical)
            {
                continue;
            }
            if !self
                .comparing
                .recorded
                .insert((layer.number, layer.head.clone(), kind))
            {
                continue;
            }
            self.comparing.pending.push(LayerLook {
                provider: provider.clone(),
                repository: repository.clone(),
                number: layer.number,
                head_oid: layer.head,
                fork_oid: Some(layer.fork),
                kind,
                looked_at_ms: now,
                review_id: review_id.clone(),
            });
        }
    }

    /// Record an `opened` look at the current layer once it stayed current, with the terminal
    /// focused, for the dwell time.
    pub(super) fn track_dwell(&mut self) {
        if self.talk.data.unfocused || !self.comparing.started {
            self.comparing.dwell = None;
            return;
        }
        let Some(number) = self.current_layer_number() else {
            return;
        };
        let Some(layer) = self
            .layer_sources()
            .into_iter()
            .find(|layer| layer.number == number)
        else {
            return;
        };
        let dwell = std::time::Duration::from_secs(self.services.compare_config().dwell_seconds);
        match &self.comparing.dwell {
            Some((current, head, since)) if *current == number && *head == layer.head => {
                if since.elapsed() >= dwell {
                    self.record_layer_looks(Some(&layer.source_id), LookKind::Opened);
                }
            }
            _ => self.comparing.dwell = Some((number, layer.head, Instant::now())),
        }
    }

    /// Record the reviewer's own reviews on the forge as `reviewed` looks (read with each pull
    /// request's conversation).
    pub(super) fn record_forge_looks(&mut self) {
        let Some((provider, repository)) = self.pulls_scope() else {
            return;
        };
        let numbers: BTreeSet<u64> = self
            .layer_sources()
            .iter()
            .map(|layer| layer.number)
            .collect();
        let mut reviews = Vec::new();
        for link in &self.talk.data.links {
            if !numbers.contains(&link.pull.number) {
                continue;
            }
            let Some(activity) = self.talk.data.activity(&link.pull) else {
                continue;
            };
            for review in &activity.reviews {
                if let (true, Some(commit), Some(at)) =
                    (review.author.is_viewer, &review.commit_oid, review.at_ms)
                {
                    reviews.push((link.pull.number, commit.clone(), at));
                }
            }
        }
        for look in looks::forge_review_looks(&provider, &repository, &reviews) {
            if self
                .comparing
                .recorded
                .insert((look.number, look.head_oid.clone(), look.kind))
            {
                self.comparing.pending.push(look);
            }
        }
    }

    /// Write the looks waiting, and keep their heads' commits (releasing pruned ones).
    pub(super) fn flush_looks(&mut self, host: &UiContext) {
        if self.comparing.pending.is_empty() {
            return;
        }
        let looks = std::mem::take(&mut self.comparing.pending);
        let keep: Vec<String> = looks
            .iter()
            .map(|look| look.head_oid.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let (services, repo) = (self.services.clone(), self.repo_path.clone());
        host.spawn(Box::pin(async move {
            let recorded: Result<RecordLayerLooksResponse, _> = services
                .call(RecordLayerLooksRequest {
                    repo_path: repo.clone(),
                    looks,
                })
                .await;
            let release = recorded
                .map(|recorded| recorded.pruned_heads)
                .unwrap_or_default();
            let _: Result<ReviewVersionsKeepResponse, _> = services
                .call(ReviewVersionsKeepRequest {
                    repo_path: repo,
                    keep,
                    release,
                })
                .await;
        }));
    }

    /// Ask for the current layer's version steps (each pair of consecutive versions compared),
    /// once per head, when it is compared and its history has several versions.
    pub(super) fn request_steps(&mut self, host: &UiContext) {
        let Some(number) = self.current_layer_number() else {
            return;
        };
        let Some(layer) = self
            .layer_sources()
            .into_iter()
            .find(|layer| layer.number == number)
        else {
            return;
        };
        if !self.comparing.pairs.contains_key(&layer.source_id)
            || self
                .comparing
                .steps_asked
                .contains(&(number, layer.head.clone()))
        {
            return;
        }
        let Some(versions) = self
            .stack_history
            .as_ref()
            .and_then(|history| history.layers.iter().find(|layer| layer.number == number))
        else {
            return;
        };
        if versions.versions.len() < 2 {
            return;
        }
        let pairs: Vec<LayerPair> = (1..versions.versions.len())
            .filter(|index| {
                versions.versions[*index].kind != ChangeKind::Unavailable
                    && versions.versions[*index - 1].kind != ChangeKind::Unavailable
            })
            .map(|index| LayerPair {
                number,
                from: Some(point_of(versions, index - 1, &layer)),
                to: point_of(versions, index, &layer),
            })
            .collect();
        self.comparing.steps_asked.insert((number, layer.head));
        if pairs.is_empty() {
            return;
        }
        let (services, inbox, repo) = (
            self.services.clone(),
            self.inbox.clone(),
            self.repo_path.clone(),
        );
        host.spawn(Box::pin(async move {
            let result: Result<ReviewCompareLayersResponse, _> = services
                .call(ReviewCompareLayersRequest {
                    repo_path: repo,
                    pairs,
                })
                .await;
            inbox.push(Message::CompareSteps(
                number,
                result
                    .map(|response| response.deltas)
                    .map_err(|error| error.message),
            ));
        }));
    }

    /// Layer `number`'s version steps arrived: its file headers show each file's versions.
    pub(super) fn apply_steps(&mut self, number: u64, result: Result<Vec<LayerDelta>, String>) {
        let Ok(steps) = result else {
            return;
        };
        self.comparing.steps.insert(number, steps.clone());
        let Some(layer) = self
            .layer_sources()
            .into_iter()
            .find(|layer| layer.number == number)
        else {
            return;
        };
        let Some(review) = self.review.as_mut() else {
            return;
        };
        if let Some(mut comparison) = review.doc.comparison(&layer.source_id).cloned() {
            comparison.steps = steps;
            review
                .doc
                .set_comparison(&layer.source_id, Some(comparison));
            review.note_splices();
        }
    }

    /// Step the cursor's file to its previous (or next) version in which it changed; past the
    /// newest, the file is compared like its layer again.
    pub(super) fn step_file_version(&mut self, forward: bool) {
        let Some(review) = self.review.as_ref() else {
            return;
        };
        let Some(file) = review.cursor.as_ref().and_then(|key| key.file()).cloned() else {
            self.set_status("put the cursor on a file first", StatusSeverity::Info);
            return;
        };
        let Some(comparison) = review.doc.comparison(&file.source_id) else {
            self.set_status(
                "this file's layer is not compared (b chooses what to compare with)",
                StatusSeverity::Info,
            );
            return;
        };
        let steps = &comparison.steps;
        if steps.is_empty() {
            self.set_status(
                "this file's versions are still being read",
                StatusSeverity::Info,
            );
            return;
        }
        let changed: Vec<usize> = (0..steps.len())
            .filter(|index| steps[*index].file(&file.path).changed())
            .collect();
        let shown = review
            .doc
            .file_override(&file)
            .and_then(|delta| steps.iter().position(|step| step.pair == delta.pair));
        let target = match (shown, forward) {
            (None, false) => changed.last().copied(),
            (None, true) => None,
            (Some(at), false) => changed.iter().rev().find(|index| **index < at).copied(),
            (Some(at), true) => changed.iter().find(|index| **index > at).copied(),
        };
        let delta = match (shown, target) {
            (_, Some(index)) => Some(steps[index].clone()),
            (Some(_), None) if forward => None,
            _ => {
                self.set_status(
                    if forward {
                        "this file shows its newest version"
                    } else {
                        "no earlier version of this file changed it"
                    },
                    StatusSeverity::Info,
                );
                return;
            }
        };
        let newest = steps.len();
        let label = delta.as_ref().and_then(|delta| {
            let index = steps.iter().position(|step| step.pair == delta.pair)?;
            let older = if index + 1 < newest {
                " (an older version: read-only)"
            } else {
                ""
            };
            Some(format!(
                "{}: v{} → v{}{older}",
                file.path,
                index + 1,
                index + 2
            ))
        });
        if let Some(review) = self.review.as_mut()
            && review.doc.set_file_comparison(&file, delta)
        {
            review.note_splices();
        }
        self.set_status(
            label.unwrap_or_else(|| format!("{}: as its layer is compared", file.path)),
            StatusSeverity::Info,
        );
    }
}

/// What a comparison found across the review's layers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompareCounts {
    /// Layers of the review.
    pub layers: usize,
    /// Changed (including with the rebase).
    pub changed: usize,
    /// Of those, compared with the rebase.
    pub with_rebase: usize,
    /// Only rebased.
    pub rebased: usize,
    /// Same version.
    pub unchanged: usize,
    /// Never looked at.
    pub new_to_you: usize,
    /// Baseline commits gone.
    pub unavailable: usize,
    /// Still computing.
    pub pending: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::{LayerVersion, StackPush, TimelineVersion};

    fn timeline() -> StackTimeline {
        let version = |head: &str, at: u64| TimelineVersion {
            version: LayerVersion {
                provider: "p".into(),
                repository: "o/r".into(),
                number: 7,
                head_oid: head.into(),
                base_oid: None,
                base_branch: None,
                seen_at_ms: at,
                actor: None,
                sources: BTreeSet::new(),
            },
            kind: ChangeKind::Content,
            fork_oid: Some(format!("{head}-fork")),
            reviewed: false,
        };
        StackTimeline {
            layers: vec![LayerTimeline {
                number: 7,
                title: "t".into(),
                head_branch: "b".into(),
                versions: vec![version("a", 10), version("b", 100), version("c", 200)],
            }],
            pushes: vec![
                StackPush {
                    at_ms: 100,
                    layers: vec![(7, ChangeKind::Content)],
                },
                StackPush {
                    at_ms: 200,
                    layers: vec![(7, ChangeKind::Content)],
                },
            ],
        }
    }

    fn layer() -> LayerSource {
        LayerSource {
            source_id: "s".into(),
            number: 7,
            head: "c".into(),
            fork: "now-fork".into(),
        }
    }

    #[test]
    fn rules_pick_each_layers_versions_at_the_times_asked() {
        let timeline = timeline();
        // Before the latest push to now: b to the current version (with its fork now).
        let Rule::Pair(pair) = between(
            Some(&timeline),
            &layer(),
            before_last_push(&timeline).unwrap(),
            None,
        ) else {
            panic!("a pair");
        };
        assert_eq!(
            pair.from.as_ref().map(|from| from.head_oid.as_str()),
            Some("b")
        );
        assert_eq!(pair.to.fork_oid.as_deref(), Some("now-fork"));
        // Between the first and the second push: a to b (an older version, as recorded).
        let Rule::Pair(pair) = between(
            Some(&timeline),
            &layer(),
            column_time(&timeline, 0).unwrap(),
            column_time(&timeline, 1),
        ) else {
            panic!("a pair");
        };
        assert_eq!(pair.from.map(|from| from.head_oid), Some("a".to_owned()));
        assert_eq!(pair.to.head_oid, "b");
        assert_eq!(pair.to.fork_oid.as_deref(), Some("b-fork"));
        // Before the layer existed: new; history not loaded: wait; unknown layer: none.
        let Rule::Pair(pair) = between(Some(&timeline), &layer(), 5, None) else {
            panic!("a pair");
        };
        assert_eq!(pair.from, None);
        assert_eq!(between(None, &layer(), 5, None), Rule::Wait);
        let other = LayerSource {
            number: 8,
            ..layer()
        };
        assert_eq!(between(Some(&timeline), &other, 5, None), Rule::None);
        assert_eq!(column_time(&timeline, 2), None);
    }
}
