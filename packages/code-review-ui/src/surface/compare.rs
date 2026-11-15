//! Comparisons on the review screen (see `docs/review-stack-history.md`).
//!
//! A review compares each of its sources, in place, with a baseline. Pull requests (stack
//! layers and single pull requests) are compared by default with the version the reviewer last
//! looked at (from the looks recorded as the reviewer works, seeded from completed passes and
//! the reviewer's own reviews on the forge, pinned for the session); else with the version
//! before the stack's latest push, the versions at some time or between two pushes, or two
//! versions picked for one pull request. Other sources (a branch, a range, a whole stack) are
//! compared with the revisions of the review's last completed pass. Every delta is computed in
//! one batched call and laid over the document; nothing about the review's sources changes.
//! Looks are recorded when a file is marked viewed, a comment or AI question is made, a pass is
//! completed, or a layer stays current for the dwell time.

use super::super::loader::{Message, StepsArrived};
use super::ReviewSurface;
use crate::compare_bar::{BannerPlan, BarPlan};
use crate::config::compare::{BaselineLook, RebasedLayers, StartView};
use crate::context::UiContext;
use bcode_code_review::document::{CompareView, FileKey, GroupComparison, VersionStep};
use bcode_code_review::history::looks::{self, BaselineRule, LastLooked};
use bcode_code_review::history::version_at;
use bcode_code_review_models::{
    ChangeKind, DeltaKind, LayerDelta, LayerLook, LayerPair, LayerTimeline, ListLayerLooksRequest,
    ListLayerLooksResponse, LookKind, RecordLayerLooksRequest, RecordLayerLooksResponse,
    ResolvedSourceMode, ReviewCompareBaseline as Baseline, ReviewCompareLayersRequest,
    ReviewCompareLayersResponse, ReviewSourceKind, ReviewVersionsKeepRequest,
    ReviewVersionsKeepResponse, ReviewedRevision, StackTimeline, VersionPoint,
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
    /// Layers whose looks could not be read: not compared with what was looked at, and asked
    /// again when the review is refreshed.
    failed: BTreeSet<u64>,
    /// Each layer's last-looked baseline, pinned when first resolved, with the head the layer
    /// had when it was pinned.
    baselines: BTreeMap<u64, (String, LastLooked)>,
    /// The saved baseline, restored: kept only while no layer moved since the reviewer last
    /// looked (checked once every layer's looks are in).
    restored: Option<Baseline>,
    /// Looks recorded in this session, in order.
    session_looks: Vec<LayerLook>,
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
    /// Heads the history was read again for, because it did not know them.
    history_asked: BTreeSet<String>,
    /// Each layer's version steps, by number, once computed (they belong to the layer, not to
    /// what it is compared with), with the head they were computed for.
    steps: BTreeMap<u64, LayerSteps>,
    /// The point in time of the menu the baseline was chosen as, while it is in effect.
    ago: Option<u64>,
    /// The time the whole stack is shown as it was at, while it is.
    stack_at: Option<u64>,
    /// Deltas computed in this session, by pair (a pair names its commits, so its delta never
    /// changes): picking a baseline again in the menu lays them at once.
    deltas: BTreeMap<LayerPair, LayerDelta>,
    /// The comparison state the menu opened on, while the menu is open: put back on `esc`,
    /// and nothing is saved until the menu closes.
    snapshot: Option<CompareSnapshot>,
    /// The sources whose hidden files were shown when the menu opened, while it is open (or
    /// a picker it opened is): put back with the snapshot.
    files_shown: Option<BTreeSet<String>>,
    /// The push timeline while it has the keyboard.
    pub(super) timeline: Option<TimelineFocus>,
    /// The comparison row as last painted.
    pub(super) bar: Option<(Rect, BarPlan)>,
    /// The banner as last painted.
    pub(super) banner_plan: Option<(Rect, BannerPlan)>,
}

/// The comparison choice, as the menu opened on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CompareSnapshot {
    baseline: Baseline,
    picks: BTreeMap<u64, LayerPair>,
    only_changed: bool,
    view: CompareView,
    banner: BannerState,
    restored: Option<Baseline>,
    ago: Option<u64>,
    stack_at: Option<u64>,
}

/// Most deltas kept for picking a baseline again.
const DELTA_CACHE: usize = 512;

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

    /// The point in time of the menu the baseline was chosen as, while it is in effect.
    #[must_use]
    pub const fn ago(&self) -> Option<u64> {
        self.ago
    }

    /// The time the whole stack is shown as it was at, while it is.
    #[must_use]
    pub const fn stack_at(&self) -> Option<u64> {
        self.stack_at
    }

    /// The choice as it stands, to put back later.
    pub(super) fn snapshot(&self) -> CompareSnapshot {
        CompareSnapshot {
            baseline: self.baseline,
            picks: self.picks.clone(),
            only_changed: self.only_changed,
            view: self.view,
            banner: self.banner,
            restored: self.restored,
            ago: self.ago,
            stack_at: self.stack_at,
        }
    }

    /// Hold the choice as it stands while the menu previews others.
    pub(super) fn begin_preview(&mut self) {
        self.snapshot = Some(self.snapshot());
    }

    /// The menu closed keeping what it previewed: returns whether the choice differs from the
    /// one it opened on.
    pub(super) fn end_preview(&mut self) -> bool {
        self.files_shown = None;
        let before = self.snapshot.take();
        before.is_some_and(|before| before != self.snapshot())
    }

    /// The banner as the menu opened on it, when the menu opened comparing with what the
    /// reviewer last looked at (picking that again in the menu brings the banner back).
    fn preview_banner(&self) -> Option<BannerState> {
        self.snapshot
            .as_ref()
            .filter(|before| {
                before.baseline == Baseline::LastLooked
                    && before.picks.is_empty()
                    && before.stack_at.is_none()
            })
            .map(|before| before.banner)
    }

    /// The menu closed putting back what it opened on: returns that choice.
    pub(super) const fn take_snapshot(&mut self) -> Option<CompareSnapshot> {
        self.snapshot.take()
    }

    /// The time the whole stack was shown as it was at when the menu opened, while it is open.
    pub(super) fn preview_stack_at(&self) -> Option<u64> {
        self.snapshot.as_ref().and_then(|before| before.stack_at)
    }

    /// Hold the sources whose hidden files are shown, to put back with the snapshot.
    pub(super) fn hold_files_shown(&mut self, shown: BTreeSet<String>) {
        self.files_shown = Some(shown);
    }

    /// The sources held by [`Self::hold_files_shown`] (and forget them).
    pub(super) const fn take_files_shown(&mut self) -> Option<BTreeSet<String>> {
        self.files_shown.take()
    }

    /// A source was listed again (or added): its layer may have moved.
    pub(super) const fn layers_moved(&mut self) {
        self.moved = true;
    }

    /// The document was replaced (opened or refreshed): it has no overlays, so every layer is
    /// laid over again; looks that could not be read are asked for again.
    pub(super) fn document_replaced(&mut self) {
        self.pairs.clear();
        self.failed.clear();
        self.moved = true;
    }

    /// Whether the layers may have moved since last compared (and forget it).
    pub(super) const fn take_moved(&mut self) -> bool {
        std::mem::replace(&mut self.moved, false)
    }

    /// The looks of layer `number`: those listed and those recorded in this session.
    pub(super) fn looks_of(&self, number: u64) -> Vec<LayerLook> {
        self.looks
            .iter()
            .flatten()
            .chain(&self.session_looks)
            .filter(|look| look.number == number)
            .cloned()
            .collect()
    }

    /// Heads the reviewer engaged with (viewed, commented, asked, completed a pass, reviewed),
    /// from the looks listed and those recorded in this session.
    pub(super) fn engaged_heads(&self) -> BTreeSet<String> {
        self.looks
            .iter()
            .flatten()
            .chain(&self.session_looks)
            .filter(|look| look.kind.engaged())
            .map(|look| look.head_oid.clone())
            .collect()
    }
}

/// A layer's version steps and the head they were computed for.
#[derive(Debug, Clone, Default)]
struct LayerSteps {
    head: String,
    current_version: usize,
    steps: Vec<VersionStep>,
}

/// One pull request of the review: a step-through stack layer (by its top layer) or a single
/// pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LayerSource {
    pub source_id: String,
    pub provider: String,
    pub repository: String,
    pub number: u64,
    pub head: String,
    pub fork: String,
}

/// A source of the review that is not a pull request (a branch, a range, a whole stack): it is
/// compared with the revisions of the review's last completed pass.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlainSource {
    source_id: String,
    head: String,
    base: String,
}

/// The pair comparing a source that is not a pull request from `reviewed` (the last completed
/// pass) to its current `head` and `base`; `None` without a pass that recorded both commits.
fn plain_pair(source: &PlainSource, reviewed: Option<&ReviewedRevision>) -> Option<LayerPair> {
    let reviewed = reviewed?;
    let from = VersionPoint {
        head_oid: reviewed.reviewed_head_oid.clone()?,
        fork_oid: Some(reviewed.reviewed_base_oid.clone()?),
    };
    Some(LayerPair {
        number: 0,
        from: Some(from),
        to: VersionPoint {
            head_oid: source.head.clone(),
            fork_oid: Some(source.base.clone()),
        },
    })
}

/// The baseline of a layer that moved to `head` while the review was open, its baseline
/// `pinned` until then: the newer of the pinned look and the newest look of this session that
/// `rule` counts (the version the reviewer engaged with before the move).
fn moved_baseline(
    pinned: &LastLooked,
    session_looks: &[LayerLook],
    number: u64,
    head: &str,
    rule: BaselineRule,
) -> LastLooked {
    let order = |look: &LayerLook| (look.looked_at_ms, look.kind);
    let engaged = session_looks
        .iter()
        .filter(|look| look.number == number)
        .filter(|look| rule == BaselineRule::Opened || look.kind.engaged())
        .max_by_key(|look| order(look));
    let newest = match (engaged, pinned.look()) {
        (Some(session), Some(pinned)) => {
            if order(session) >= order(pinned) {
                session
            } else {
                pinned
            }
        }
        (Some(look), None) | (None, Some(look)) => look,
        (None, None) => return LastLooked::NewToYou,
    };
    if newest.head_oid == head {
        LastLooked::Unchanged(newest.clone())
    } else {
        LastLooked::Since(newest.clone())
    }
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

/// One of the reviewer's reviews on the forge: number, commit, and time.
type ForgeReview = (u64, String, u64);

/// What the rule in effect makes of each source: source, head shown, layer number (0 for a
/// source that is not a pull request), and rule.
type Wanted = Vec<(String, String, u64, Rule)>;

/// Record `looks` and keep their heads' commits with refs, releasing the heads pruned.
async fn record_and_keep(
    services: &crate::services::ReviewServices,
    repo: &std::path::Path,
    looks: Vec<LayerLook>,
) {
    let keep: Vec<String> = looks
        .iter()
        .map(|look| look.head_oid.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let recorded: Result<RecordLayerLooksResponse, _> = services
        .call(RecordLayerLooksRequest {
            repo_path: repo.to_path_buf(),
            looks,
        })
        .await;
    let release = recorded
        .map(|recorded| recorded.pruned_heads)
        .unwrap_or_default();
    let _: Result<ReviewVersionsKeepResponse, _> = services
        .call(ReviewVersionsKeepRequest {
            repo_path: repo.to_path_buf(),
            keep,
            release,
        })
        .await;
}

impl ReviewSurface {
    /// The review's pull requests: step-through stack-layer sources (by their top layer) and
    /// single pull request sources.
    pub(super) fn layer_sources(&self) -> Vec<LayerSource> {
        let Some(review) = self.review.as_ref() else {
            return Vec::new();
        };
        review
            .doc
            .groups()
            .filter(|group| !group.loading && group.source.part.is_none())
            .filter_map(|group| {
                let source_id = group.source.source_id.clone();
                let (provider, repository, kind_number) = self.pull_scope_of(&source_id)?;
                let (number, head, fork) = match group.stack {
                    Some(stack) if stack.whole => return None,
                    Some(stack) => {
                        let layer = stack.stack.layers.last()?;
                        (layer.number, layer.head_oid.clone(), layer.base_oid.clone())
                    }
                    None => (
                        kind_number?,
                        group.source.head.commit_oid()?.to_owned(),
                        group.source.base.commit_oid()?.to_owned(),
                    ),
                };
                Some(LayerSource {
                    source_id,
                    provider,
                    repository,
                    number,
                    head,
                    fork,
                })
            })
            .collect()
    }

    /// The provider, repository, and pull request number (for a single pull request) of source
    /// `source_id`: from its kind, else from the stack the review follows. `None` for a source
    /// that is not about pull requests.
    fn pull_scope_of(&self, source_id: &str) -> Option<(String, String, Option<u64>)> {
        let kind = &self
            .sources
            .iter()
            .find(|source| source.id == source_id)?
            .kind;
        match kind {
            ReviewSourceKind::PullRequest {
                provider,
                repository,
                number,
            } => Some((provider.clone(), repository.clone(), Some(*number))),
            ReviewSourceKind::StackLayer {
                provider,
                repository,
                ..
            }
            | ReviewSourceKind::Stack {
                provider,
                repository,
                ..
            } => Some((provider.clone(), repository.clone(), None)),
            _ => None,
        }
    }

    /// The review's sources that are not pull requests and show a diff between two commits (a
    /// branch, a range, a whole stack): compared with the review's last completed pass.
    fn plain_sources(&self) -> Vec<PlainSource> {
        let Some(review) = self.review.as_ref() else {
            return Vec::new();
        };
        let layers: BTreeSet<String> = self
            .layer_sources()
            .into_iter()
            .map(|layer| layer.source_id)
            .collect();
        review
            .doc
            .groups()
            .filter(|group| {
                !group.loading
                    && group.source.part.is_none()
                    && group.source.mode == ResolvedSourceMode::Diff
                    && !layers.contains(&group.source.source_id)
            })
            .filter(|group| {
                // A pull request source whose stack or commits are not known is not plain.
                group.stack.map_or_else(
                    || self.pull_scope_of(&group.source.source_id).is_none(),
                    |stack| stack.whole,
                )
            })
            .filter_map(|group| {
                Some(PlainSource {
                    source_id: group.source.source_id.clone(),
                    head: group.source.head.commit_oid()?.to_owned(),
                    base: group.source.base.commit_oid()?.to_owned(),
                })
            })
            .collect()
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

    /// Whether the review compares its sources: it has pull requests, or other sources a
    /// completed pass recorded.
    pub(super) fn compares_pulls(&self) -> bool {
        self.comparing.started
            && (!self.layer_sources().is_empty() || !self.plain_sources().is_empty())
    }

    /// Ask for the reviewer's looks at the review's pull requests once its sources are known,
    /// seeding them first from the passes this review recorded; restore the baseline the
    /// review last compared with (else the configured one).
    pub(super) fn start_comparison(&mut self, host: &UiContext) {
        if self.comparing.started {
            return;
        }
        if self.layer_sources().is_empty() && self.plain_sources().is_empty() {
            return;
        }
        self.comparing.started = true;
        let config = self.services.compare_config();
        let presentation = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.presentation.as_ref());
        let ago = presentation.and_then(|state| state.compare.ago_ms);
        // A time chosen as "this long ago" is that long before this open.
        let saved = presentation
            .and_then(|state| state.compare.baseline)
            .map(|saved| match (saved, ago) {
                (Baseline::Between { to_ms: None, .. }, Some(ago)) => Baseline::Between {
                    from_ms: super::super::history::now_ms().saturating_sub(ago),
                    to_ms: None,
                },
                (saved, _) => saved,
            });
        self.comparing.ago =
            ago.filter(|_| matches!(saved, Some(Baseline::Between { to_ms: None, .. })));
        self.comparing.restored = saved.filter(|saved| *saved != Baseline::LastLooked);
        self.comparing.baseline = saved.unwrap_or_else(|| {
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
        let layers: Vec<LayerSource> = self
            .layer_sources()
            .into_iter()
            .filter(|layer| {
                !self.comparing.asked.contains(&layer.number)
                    && !self.comparing.failed.contains(&layer.number)
            })
            .collect();
        if layers.is_empty() {
            return;
        }
        let review_id = self.review_id();
        // One listing per repository (a review's pull requests are normally of one).
        let mut scopes: BTreeMap<(String, String), Vec<LayerSource>> = BTreeMap::new();
        for layer in layers {
            scopes
                .entry((layer.provider.clone(), layer.repository.clone()))
                .or_default()
                .push(layer);
        }
        for ((provider, repository), layers) in scopes {
            let reviewed: Vec<(u64, ReviewedRevision)> = self
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
            let seeded = looks::pass_looks(&provider, &repository, &reviewed, review_id.as_deref());
            let numbers: Vec<u64> = layers.iter().map(|layer| layer.number).collect();
            self.comparing.asked.extend(numbers.iter().copied());
            let (services, inbox, repo) = (
                self.services.clone(),
                self.inbox.clone(),
                self.repo_path.clone(),
            );
            host.spawn(Box::pin(async move {
                if !seeded.is_empty() {
                    record_and_keep(&services, &repo, seeded).await;
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
    }

    /// The looks at layers `numbers` arrived: compare those layers with their baselines. When
    /// they could not be read, those layers are not compared with what was looked at (until
    /// the review is refreshed), so the rest of the review is not held up.
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
            }
            Err(error) => {
                for number in numbers {
                    self.comparing.asked.remove(number);
                    self.comparing.failed.insert(*number);
                }
                self.set_status(
                    format!("could not read what you looked at before: {error} (r tries again)"),
                    StatusSeverity::Warning,
                );
            }
        }
        self.check_restored_baseline();
        self.refresh_comparison(host);
        self.decide_banner();
    }

    /// Once every layer's looks are in: a restored baseline other than "you last looked"
    /// yields to it when some layer moved since the reviewer last looked, so a changed stack
    /// reopens showing what is new since then (with the banner).
    fn check_restored_baseline(&mut self) {
        if self.comparing.restored.is_none() || !self.looks_settled() {
            return;
        }
        self.comparing.restored = None;
        let rule = self.baseline_rule();
        let looks = self.comparing.looks.clone().unwrap_or_default();
        let moved = self.layer_sources().iter().any(|layer| {
            matches!(
                looks::last_looked(&looks, layer.number, &layer.head, rule),
                LastLooked::Since(_)
            )
        }) || self.plain_sources().iter().any(|source| {
            self.reviewed_revision(&source.source_id)
                .and_then(|revision| revision.reviewed_head_oid.as_deref())
                .is_some_and(|head| head != source.head)
        });
        if moved {
            self.comparing.baseline = Baseline::LastLooked;
            self.comparing.ago = None;
            self.comparing.picks.clear();
            self.comparing.banner = BannerState::Undecided;
        }
    }

    /// Whether the comparison is still being set up: started not yet, or some pull request's
    /// looks not listed yet (a source not compared may be compared once they are).
    pub(super) fn compare_settling(&self) -> bool {
        !self.comparing.started || !self.looks_settled()
    }

    /// Whether every pull request's looks were listed (or could not be read).
    fn looks_settled(&self) -> bool {
        self.layer_sources().iter().all(|layer| {
            self.comparing.listed.contains(&layer.number)
                || self.comparing.failed.contains(&layer.number)
        })
    }

    /// The revisions the review's last completed pass recorded for source `source_id`.
    fn reviewed_revision(&self, source_id: &str) -> Option<&ReviewedRevision> {
        self.workspace.as_ref()?.reviewed_revisions.get(source_id)
    }

    /// Which looks count for the baseline, by configuration.
    const fn baseline_rule(&self) -> BaselineRule {
        match self.services.compare_config().baseline_look {
            BaselineLook::Engaged => BaselineRule::Engaged,
            BaselineLook::Opened => BaselineRule::Opened,
        }
    }

    /// The last-looked rule for `layer`: its pinned baseline; a layer that moved while open is
    /// compared from the version the reviewer engaged with before the move, else from its
    /// pinned baseline.
    fn last_looked_pair(&mut self, layer: &LayerSource) -> Option<LayerPair> {
        let rule = self.baseline_rule();
        let looks = self.comparing.looks.as_ref()?;
        let (pinned_head, baseline) =
            self.comparing
                .baselines
                .entry(layer.number)
                .or_insert_with(|| {
                    (
                        layer.head.clone(),
                        looks::last_looked(looks, layer.number, &layer.head, rule),
                    )
                });
        if *pinned_head != layer.head {
            *baseline = moved_baseline(
                baseline,
                &self.comparing.session_looks,
                layer.number,
                &layer.head,
                rule,
            );
            pinned_head.clone_from(&layer.head);
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
                } else if self.comparing.failed.contains(&layer.number) {
                    Rule::None
                } else {
                    Rule::Wait
                }
            }
            // Without a stack there is no history: nothing to compare by time.
            Baseline::PreviousPush | Baseline::Between { .. } if self.stack_news.is_none() => {
                Rule::None
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

    /// What the rule in effect makes of a source that is not a pull request: with "you last
    /// looked", the last completed pass to now; nothing otherwise.
    fn plain_rule(&self, source: &PlainSource) -> Rule {
        if self.comparing.baseline != Baseline::LastLooked {
            return Rule::None;
        }
        plain_pair(source, self.reviewed_revision(&source.source_id)).map_or(Rule::None, Rule::Pair)
    }

    /// The version steps of layer `number` for the head it shows, when computed.
    fn steps_for(&self, number: u64, head: &str) -> LayerSteps {
        self.comparing
            .steps
            .get(&number)
            .filter(|steps| steps.head == head)
            .cloned()
            .unwrap_or_default()
    }

    /// What the rule in effect makes of each source. With the last-looked rule no pull request
    /// is compared when the reviewer never looked at any.
    fn wanted_rules(&mut self) -> Wanted {
        let mut wanted: Wanted = Vec::new();
        for layer in self.layer_sources() {
            let rule = self.rule_for(&layer);
            wanted.push((layer.source_id, layer.head, layer.number, rule));
        }
        if self.comparing.baseline == Baseline::LastLooked
            && !self
                .comparing
                .baselines
                .values()
                .any(|(_, baseline)| baseline.look().is_some())
        {
            // Never looked at any layer: a first visit shows the review as it is (but for the
            // pull requests whose versions were picked).
            for (_, _, number, rule) in &mut wanted {
                if matches!(rule, Rule::Pair(_)) && !self.comparing.picks.contains_key(number) {
                    *rule = Rule::None;
                }
            }
        }
        for source in self.plain_sources() {
            let rule = self.plain_rule(&source);
            wanted.push((source.source_id, source.head, 0, rule));
        }
        wanted
    }

    /// Compare each source by the rule in effect, computing only the pairs that changed (a
    /// layer that moved, a baseline chosen, or a source listed since). With the last-looked
    /// rule no pull request is compared when the reviewer never looked at any.
    pub(super) fn refresh_comparison(&mut self, host: &UiContext) {
        if !self.comparing.started {
            return;
        }
        self.check_restored_baseline();
        self.list_looks(host);
        let wanted = self.wanted_rules();
        if wanted.is_empty() {
            return;
        }
        let view = self.comparing.view;
        let mut asked = Vec::new();
        let mut ready = Vec::new();
        for (source_id, head, number, rule) in wanted {
            let pair = match rule {
                Rule::Wait => continue,
                Rule::None => {
                    if self.comparing.pairs.remove(&source_id).is_some()
                        && let Some(review) = self.review.as_mut()
                    {
                        review.doc.set_comparison(&source_id, None);
                    }
                    continue;
                }
                Rule::Pair(pair) => pair,
            };
            let compared = Compared {
                historical: pair.to.head_oid != head,
                pair,
            };
            if self.comparing.pairs.get(&source_id) == Some(&compared) {
                continue;
            }
            let steps = if number == 0 {
                LayerSteps::default()
            } else {
                self.steps_for(number, &head)
            };
            if let Some(review) = self.review.as_mut() {
                review.doc.set_comparison(
                    &source_id,
                    Some(GroupComparison {
                        delta: None,
                        view,
                        historical: compared.historical,
                        steps: steps.steps,
                        current_version: steps.current_version,
                        ..GroupComparison::default()
                    }),
                );
            }
            if let Some(delta) = self.comparing.deltas.get(&compared.pair) {
                if !ready.contains(delta) {
                    ready.push(delta.clone());
                }
            } else if !asked.contains(&compared.pair) {
                asked.push(compared.pair.clone());
            }
            self.comparing.pairs.insert(source_id, compared);
        }
        if ready.is_empty() {
            self.apply_compare_filter();
            self.rebuild_compared_files();
            if let Some(review) = self.review.as_mut() {
                review.note_splices();
            }
        } else {
            // Computed before: laid over at once.
            self.lay_deltas(ready);
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
        // The cursor's source, else the layer last entered (as the rail reads it, without
        // building the rail's labels).
        let source = self.cursor_source()?;
        self.layer_sources()
            .into_iter()
            .find(|layer| layer.source_id == source)
            .map(|layer| layer.number)
    }

    /// Layer deltas arrived: lay each over the sources still showing its pair.
    pub(super) fn apply_compared(&mut self, result: Result<Vec<LayerDelta>, String>) {
        let deltas = match result {
            Ok(deltas) => deltas,
            Err(error) => {
                // The layers still computing show the full change.
                let pending: Vec<String> = self.comparing.pairs.keys().cloned().collect();
                let (theme, file_view) = (self.theme, self.file_view);
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
                    // The panel's group rows and the open menu stop saying "comparing".
                    review.rebuild_files(&theme, file_view);
                }
                self.apply_compare_filter();
                self.sync_compare_menu();
                self.set_status(
                    format!("could not compare: {error}"),
                    StatusSeverity::Warning,
                );
                return;
            }
        };
        if self.comparing.deltas.len() + deltas.len() > DELTA_CACHE {
            self.comparing.deltas.clear();
        }
        for delta in &deltas {
            // Commits missing now may be fetched later.
            if delta.kind != DeltaKind::Unavailable {
                self.comparing
                    .deltas
                    .insert(delta.pair.clone(), delta.clone());
            }
        }
        self.lay_deltas(deltas);
    }

    /// Lay `deltas` over the sources showing their pairs.
    fn lay_deltas(&mut self, deltas: Vec<LayerDelta>) {
        let view = self.comparing.view;
        let show_rebased = self.services.compare_config().rebased_layers == RebasedLayers::Show;
        let (theme, file_view) = (self.theme, self.file_view);
        let Some(review) = self.review.as_mut() else {
            return;
        };
        for delta in deltas {
            for (source_id, compared) in &self.comparing.pairs {
                if compared.pair == delta.pair {
                    // The steps the source already carries (they belong to its head).
                    let (steps, current_version) = review
                        .doc
                        .comparison(source_id)
                        .map(|comparison| (comparison.steps.clone(), comparison.current_version))
                        .unwrap_or_default();
                    review.doc.set_comparison(
                        source_id,
                        Some(GroupComparison {
                            delta: Some(delta.clone()),
                            view,
                            historical: compared.historical,
                            steps,
                            current_version,
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
        review.rebuild_files(&theme, file_view);
        self.apply_compare_filter();
        self.decide_banner();
        self.sync_compare_menu();
    }

    /// Rebuild the file panel after how compared files show changed.
    pub(super) fn rebuild_compared_files(&mut self) {
        let (theme, file_view) = (self.theme, self.file_view);
        if let Some(review) = self.review.as_mut() {
            review.rebuild_files(&theme, file_view);
        }
    }

    /// Once every compared layer's delta is in: show the banner when, compared with what the
    /// reviewer last looked at, some layer moved.
    fn decide_banner(&mut self) {
        if self.comparing.banner != BannerState::Undecided
            || self.compare_pending() > 0
            || !self.looks_settled()
            || self.comparing.restored.is_some()
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
        let others = self.plain_sources().len();
        let mut counts = CompareCounts {
            layers: self.layer_sources().len() + others,
            others,
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
        let file_view = self.file_view;
        if let Some(review) = self.review.as_mut()
            && review.doc.set_filtered_sources(filtered)
        {
            review.note_splices();
            review.rebuild_files(&theme, file_view);
        }
    }

    /// Switch every compared source between the full change and only what changed.
    /// With nothing compared, the setting still flips and applies once something is.
    pub(super) fn toggle_compare_view(&mut self) {
        let view = match self.comparing.view {
            CompareView::Full => CompareView::Changes,
            CompareView::Changes => CompareView::Full,
        };
        self.comparing.view = view;
        if let Some(review) = self.review.as_mut() {
            review.doc.set_compare_view(view);
            review.note_splices();
        }
        self.rebuild_compared_files();
        let text = match (view, self.comparing.active()) {
            (CompareView::Changes, true) => "showing only what changed",
            (CompareView::Full, true) => "showing the full change, what is new marked",
            (CompareView::Changes, false) => {
                "only what changed: applies once something is compared (b chooses what with)"
            }
            (CompareView::Full, false) => "the full change, once something is compared",
        };
        self.set_status(text, StatusSeverity::Info);
    }

    /// Compare every layer by `baseline` (clearing single picks), kept for the next open
    /// (while no layer moved since the reviewer last looked).
    pub(super) fn set_compare_baseline(&mut self, baseline: Baseline, host: &UiContext) {
        self.choose_baseline(baseline, None, host);
    }

    /// Compare every layer by `baseline`, chosen as the time `ago` before now when given.
    fn choose_baseline(&mut self, baseline: Baseline, ago: Option<u64>, host: &UiContext) {
        self.comparing.baseline = baseline;
        self.comparing.restored = None;
        self.comparing.picks.clear();
        self.comparing.timeline = None;
        self.comparing.ago = ago;
        self.comparing.stack_at = None;
        if baseline == Baseline::LastLooked {
            if let Some(banner) = self.comparing.preview_banner() {
                self.comparing.banner = banner;
            }
        } else {
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
        self.comparing.stack_at = None;
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
        self.comparing.restored = None;
        self.comparing.picks = picks;
        self.comparing.banner = BannerState::Hidden;
        self.comparing.ago = None;
        self.comparing.stack_at = Some(at_ms);
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

    /// Put back the comparison choice `snapshot` (the menu was cancelled): pairs computed for
    /// it before are laid over again at once.
    pub(super) fn restore_compare(&mut self, snapshot: CompareSnapshot, host: &UiContext) {
        let CompareSnapshot {
            baseline,
            picks,
            only_changed,
            view,
            banner,
            restored,
            ago,
            stack_at,
        } = snapshot;
        self.comparing.baseline = baseline;
        self.comparing.picks = picks;
        self.comparing.only_changed = only_changed;
        self.comparing.banner = banner;
        self.comparing.restored = restored;
        self.comparing.ago = ago;
        self.comparing.stack_at = stack_at;
        self.comparing.timeline = None;
        if self.comparing.view != view {
            self.comparing.view = view;
            if let Some(review) = self.review.as_mut() {
                review.doc.set_compare_view(view);
                review.note_splices();
            }
        }
        self.refresh_comparison(host);
        self.apply_compare_filter();
        self.rebuild_compared_files();
    }

    /// The sources whose hidden files are shown now.
    pub(super) fn compare_files_shown_now(&self) -> BTreeSet<String> {
        let Some(review) = self.review.as_ref() else {
            return BTreeSet::new();
        };
        review
            .doc
            .groups()
            .map(|group| group.source.source_id.clone())
            .filter(|source| review.doc.compare_files_shown(source))
            .collect()
    }

    /// Show the hidden files of exactly the sources in `shown`.
    pub(super) fn restore_compare_files_shown(&mut self, shown: &BTreeSet<String>) {
        let (theme, file_view) = (self.theme, self.file_view);
        let Some(review) = self.review.as_mut() else {
            return;
        };
        let sources: BTreeSet<String> = review
            .doc
            .groups()
            .map(|group| group.source.source_id.clone())
            .collect();
        let mut changed = false;
        for source in &sources {
            changed |= review
                .doc
                .set_compare_files_shown(source, shown.contains(source));
        }
        if changed {
            review.note_splices();
            review.rebuild_files(&theme, file_view);
            self.layout_revision += 1;
        }
    }

    /// Keep the comparison choice for the next open. Versions picked for single pull requests
    /// (and the stack as it was) are for this session: the baseline under them is not saved.
    pub(super) fn persist_compare_choice(&mut self) {
        // While the menu previews, nothing is saved: closing it saves or puts back.
        if self.comparing.snapshot.is_some() {
            if matches!(self.modal, Some(super::flows::Modal::Compare(_))) {
                return;
            }
            self.comparing.snapshot = None;
            self.comparing.files_shown = None;
        }
        let (baseline, only_changed) = (self.comparing.baseline, self.comparing.only_changed);
        let ago = self.comparing.ago;
        let picked = !self.comparing.picks.is_empty();
        let changed = self.workspace.as_mut().is_some_and(|workspace| {
            let state = workspace.presentation.get_or_insert_with(Default::default);
            let before = state.compare;
            if !picked {
                state.compare.baseline = Some(baseline);
                state.compare.ago_ms = ago;
            }
            state.compare.only_changed = only_changed;
            state.compare != before
        });
        self.persist_presentation(true);
        // The choice is written into the workspace's state above, so the presentation check
        // sees no difference: save it here.
        if changed {
            self.request_save();
        }
    }

    /// The baseline in words: in full and briefly.
    pub(super) fn baseline_labels(&self) -> [String; 2] {
        use super::super::history::{ago, now_ms};
        let now = now_ms();
        if let Some(at) = self.comparing.stack_at {
            let when = ago(at, now);
            return [
                format!("the stack as it was · {when}"),
                format!("as of {when}"),
            ];
        }
        if !self.comparing.picks.is_empty() {
            return if self.comparing.baseline == Baseline::Off {
                ["picked versions".to_owned(), "versions".to_owned()]
            } else {
                ["mixed".to_owned(), "mixed".to_owned()]
            };
        }
        match self.comparing.baseline {
            Baseline::LastLooked => {
                let detail = self
                    .last_looked_detail()
                    .map(|detail| format!(" · {detail}"))
                    .unwrap_or_default();
                [format!("you last looked{detail}"), "last looked".to_owned()]
            }
            Baseline::PreviousPush => {
                let when = self
                    .last_push_detail()
                    .map(|when| format!(" · {when}"))
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

    /// What the comparison's rows say the changes are since.
    pub(super) fn compare_since(&self) -> crate::rows::compare::Since {
        use crate::rows::compare::Since;
        if self.comparing.stack_at.is_some() || !self.comparing.picks.is_empty() {
            return Since::Then;
        }
        match self.comparing.baseline {
            Baseline::LastLooked => Since::LastLooked,
            Baseline::PreviousPush => Since::PreviousPush,
            Baseline::Between { .. } | Baseline::Off => Since::Then,
        }
    }

    /// When the reviewer last looked, in words (`4m ago`, `nothing new`, `not yet`), once
    /// known.
    pub(super) fn last_looked_detail(&self) -> Option<String> {
        use super::super::history::{ago, now_ms};
        let newest = self
            .comparing
            .baselines
            .values()
            .filter_map(|(_, baseline)| match baseline {
                LastLooked::Since(look) => Some(look.looked_at_ms),
                _ => None,
            })
            // Sources that are not pull requests: their last completed pass.
            .chain(self.plain_sources().iter().filter_map(|source| {
                let revision = self.reviewed_revision(&source.source_id)?;
                (revision.reviewed_head_oid.as_deref() != Some(source.head.as_str()))
                    .then_some(revision.recorded_at_ms)?
            }))
            .max();
        let looked = self
            .comparing
            .baselines
            .values()
            .any(|(_, baseline)| baseline.look().is_some());
        match newest {
            Some(at) if at > 0 => Some(ago(at, now_ms())),
            None if looked => Some("nothing new".to_owned()),
            None if self.comparing.looks.is_some() => Some("not yet".to_owned()),
            _ => None,
        }
    }

    /// When the stack was last pushed, in words, once known.
    pub(super) fn last_push_detail(&self) -> Option<String> {
        use super::super::history::{ago, now_ms};
        self.stack_history
            .as_ref()
            .and_then(|history| history.pushes.last())
            .map(|push| ago(push.at_ms, now_ms()))
    }

    /// Compare every layer with how it was `ms` ago.
    pub(super) fn compare_ago(&mut self, ms: u64, host: &UiContext) {
        self.choose_baseline(
            Baseline::Between {
                from_ms: super::super::history::now_ms().saturating_sub(ms),
                to_ms: None,
            },
            Some(ms),
            host,
        );
    }

    /// Record a look of `kind` at the layer `file` belongs to (its current version).
    pub(super) fn record_look(&mut self, file: &FileKey, kind: LookKind) {
        self.record_layer_looks(Some(&file.source_id), kind);
    }

    /// Record a look of `kind` at the layer of source `source_id`, or at every layer. A layer
    /// compared up to an older version records nothing: the reviewer is not looking at the
    /// current one.
    pub(super) fn record_layer_looks(&mut self, source_id: Option<&str>, kind: LookKind) {
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
            let look = LayerLook {
                provider: layer.provider,
                repository: layer.repository,
                number: layer.number,
                head_oid: layer.head,
                fork_oid: Some(layer.fork),
                kind,
                looked_at_ms: now,
                review_id: review_id.clone(),
            };
            self.comparing.session_looks.push(look.clone());
            self.comparing.pending.push(look);
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
        let scopes: BTreeMap<u64, (String, String)> = self
            .layer_sources()
            .into_iter()
            .map(|layer| (layer.number, (layer.provider, layer.repository)))
            .collect();
        if scopes.is_empty() {
            return;
        }
        // By provider and repository: each review's number, commit, and time.
        let mut reviews: BTreeMap<(String, String), Vec<ForgeReview>> = BTreeMap::new();
        for link in &self.talk.data.links {
            let Some(scope) = scopes.get(&link.pull.number) else {
                continue;
            };
            let Some(activity) = self.talk.data.activity(&link.pull) else {
                continue;
            };
            for review in &activity.reviews {
                if let (true, Some(commit), Some(at)) =
                    (review.author.is_viewer, &review.commit_oid, review.at_ms)
                {
                    reviews.entry(scope.clone()).or_default().push((
                        link.pull.number,
                        commit.clone(),
                        at,
                    ));
                }
            }
        }
        for ((provider, repository), reviews) in reviews {
            for look in looks::forge_review_looks(&provider, &repository, &reviews) {
                if self
                    .comparing
                    .recorded
                    .insert((look.number, look.head_oid.clone(), look.kind))
                {
                    self.comparing.session_looks.push(look.clone());
                    self.comparing.pending.push(look);
                }
            }
        }
    }

    /// Write the looks waiting, and keep their heads' commits (releasing pruned ones).
    pub(super) fn flush_looks(&mut self, host: &UiContext) {
        if self.comparing.pending.is_empty() {
            return;
        }
        let looks = std::mem::take(&mut self.comparing.pending);
        let (services, repo) = (self.services.clone(), self.repo_path.clone());
        host.spawn(Box::pin(async move {
            record_and_keep(&services, &repo, looks).await;
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
        // A history that does not know the head shown yet (a push seen before the history
        // was read again) is read again once and waited for: steps from it would end at an
        // older version.
        let Some(current) = versions
            .versions
            .iter()
            .rposition(|version| version.version.head_oid == layer.head)
        else {
            if self.comparing.history_asked.insert(layer.head) {
                self.load_history_quietly(host);
            }
            return;
        };
        if versions.versions.len() < 2 {
            return;
        }
        let asked: Vec<(LayerPair, usize, usize)> = (1..versions.versions.len())
            .filter(|index| {
                versions.versions[*index].kind != ChangeKind::Unavailable
                    && versions.versions[*index - 1].kind != ChangeKind::Unavailable
            })
            .map(|index| {
                let pair = LayerPair {
                    number,
                    from: Some(point_of(versions, index - 1, &layer)),
                    to: point_of(versions, index, &layer),
                };
                (pair, index, index + 1)
            })
            .collect();
        self.comparing
            .steps_asked
            .insert((number, layer.head.clone()));
        if asked.is_empty() {
            return;
        }
        let pairs: Vec<LayerPair> = asked.iter().map(|(pair, _, _)| pair.clone()).collect();
        let (services, inbox, repo) = (
            self.services.clone(),
            self.inbox.clone(),
            self.repo_path.clone(),
        );
        let head = layer.head;
        host.spawn(Box::pin(async move {
            let result: Result<ReviewCompareLayersResponse, _> = services
                .call(ReviewCompareLayersRequest {
                    repo_path: repo,
                    pairs,
                })
                .await;
            inbox.push(Message::CompareSteps(Box::new(StepsArrived {
                number,
                head,
                current_version: current + 1,
                versions: asked,
                result: result
                    .map(|response| response.deltas)
                    .map_err(|error| error.message),
            })));
        }));
    }

    /// A layer's version steps arrived: its file headers show each file's versions (when the
    /// layer still shows the head they were computed for).
    pub(super) fn apply_steps(&mut self, arrived: StepsArrived) {
        let Ok(deltas) = arrived.result else {
            return;
        };
        let steps: Vec<VersionStep> = arrived
            .versions
            .into_iter()
            .filter_map(|(pair, from, to)| {
                let delta = deltas.iter().find(|delta| delta.pair == pair)?.clone();
                Some(VersionStep { from, to, delta })
            })
            .collect();
        let layer_steps = LayerSteps {
            head: arrived.head,
            current_version: arrived.current_version,
            steps,
        };
        let Some(layer) = self
            .layer_sources()
            .into_iter()
            .find(|layer| layer.number == arrived.number)
        else {
            return;
        };
        let current = layer.head == layer_steps.head;
        self.comparing
            .steps
            .insert(arrived.number, layer_steps.clone());
        if !current {
            return;
        }
        let Some(review) = self.review.as_mut() else {
            return;
        };
        if let Some(mut comparison) = review.doc.comparison(&layer.source_id).cloned() {
            comparison.steps = layer_steps.steps;
            comparison.current_version = layer_steps.current_version;
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
            .filter(|index| steps[*index].delta.file(&file.path).changed())
            .collect();
        let shown = review
            .doc
            .file_override(&file)
            .and_then(|delta| steps.iter().position(|step| step.delta.pair == delta.pair));
        let target = match (shown, forward) {
            (None, false) => changed.last().copied(),
            (None, true) => None,
            (Some(at), false) => changed.iter().rev().find(|index| **index < at).copied(),
            (Some(at), true) => changed.iter().find(|index| **index > at).copied(),
        };
        let delta = match (shown, target) {
            (_, Some(index)) => Some(steps[index].delta.clone()),
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
        let current = if comparison.current_version > 0 {
            comparison.current_version
        } else {
            steps.last().map_or(0, |step| step.to)
        };
        let label = delta.as_ref().and_then(|delta| {
            let step = steps.iter().find(|step| step.delta.pair == delta.pair)?;
            let older = if step.to < current {
                " (an older version: read-only)"
            } else {
                ""
            };
            Some(format!(
                "{}: v{} → v{}{older}",
                file.path, step.from, step.to
            ))
        });
        let (theme, file_view) = (self.theme, self.file_view);
        if let Some(review) = self.review.as_mut()
            && review.doc.set_file_comparison(&file, delta)
        {
            review.note_splices();
            // The file's mark, its counts, and whether it is hidden follow the version shown.
            review.rebuild_files(&theme, file_view);
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
    /// Sources of the review that are compared (pull requests and others).
    pub layers: usize,
    /// Of those, sources that are not pull requests.
    pub others: usize,
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
            provider: "p".into(),
            repository: "o/r".into(),
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
