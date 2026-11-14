//! Comparisons on the review screen (see `docs/review-stack-history.md`).
//!
//! A review of pull request layers compares each layer with the version the reviewer last
//! looked at, in place: the baselines come from the looks recorded as the reviewer works (seeded
//! once from completed passes), are pinned for the session, and each layer's delta is computed
//! in one batched call and laid over the document. Looks are recorded when a file is marked
//! viewed, a comment or AI question is made, or a pass is completed.

use super::super::loader::Message;
use super::ReviewSurface;
use crate::context::UiContext;
use bcode_code_review::document::{CompareView, FileKey, GroupComparison};
use bcode_code_review::history::looks::{self, BaselineRule, LastLooked};
use bcode_code_review_models::{
    LayerDelta, LayerLook, LayerPair, ListLayerLooksRequest, ListLayerLooksResponse, LookKind,
    RecordLayerLooksRequest, RecordLayerLooksResponse, ReviewCompareLayersRequest,
    ReviewCompareLayersResponse, ReviewVersionsKeepRequest, ReviewVersionsKeepResponse,
    VersionPoint,
};
use bmux_tui_components::status_bar::StatusSeverity;
use std::collections::{BTreeMap, BTreeSet};

/// The review's comparison state.
#[derive(Debug, Default)]
pub struct Comparing {
    /// Whether the looks were asked for (once per review).
    started: bool,
    /// The reviewer's looks at the review's layers, once listed.
    looks: Option<Vec<LayerLook>>,
    /// Each layer's baseline, pinned when first resolved.
    baselines: BTreeMap<u64, LastLooked>,
    /// The pair each compared source shows (or is computing), by source id.
    pairs: BTreeMap<String, LayerPair>,
    /// How compared sources show their files.
    view: CompareView,
    /// Looks recorded in this session, by layer, head, and kind.
    recorded: BTreeSet<(u64, String, LookKind)>,
    /// Looks waiting to be written.
    pending: Vec<LayerLook>,
    /// The review's layers may have moved: compare them again.
    moved: bool,
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
}

/// One step-through layer of the review.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LayerSource {
    source_id: String,
    number: u64,
    head: String,
    fork: String,
}

impl ReviewSurface {
    /// The review's pull request layers: step-through stack-layer sources, by their top layer.
    fn layer_sources(&self) -> Vec<LayerSource> {
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
    fn pulls_scope(&self) -> Option<(String, String)> {
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

    /// Ask for the reviewer's looks at the review's layers once its stack is known, seeding
    /// them first from the passes this review recorded.
    pub(super) fn start_comparison(&mut self, host: &UiContext) {
        if self.comparing.started {
            return;
        }
        let Some((provider, repository)) = self.pulls_scope() else {
            return;
        };
        let layers = self.layer_sources();
        if layers.is_empty() {
            return;
        }
        self.comparing.started = true;
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
                    numbers,
                })
                .await;
            inbox.push(Message::Looks(
                listed
                    .map(|listed| listed.looks)
                    .map_err(|error| error.message),
            ));
        }));
    }

    /// The looks arrived: compare every layer with its baseline.
    pub(super) fn apply_looks(&mut self, result: Result<Vec<LayerLook>, String>, host: &UiContext) {
        match result {
            Ok(looks) => {
                self.comparing.looks = Some(looks);
                self.refresh_comparison(host);
            }
            Err(error) => self.set_status(
                format!("could not read what you looked at before: {error}"),
                StatusSeverity::Warning,
            ),
        }
    }

    /// Compare each layer with its pinned baseline, computing only the pairs that changed (a
    /// layer that moved, or one listed since). Nothing is compared when the reviewer never
    /// looked at any layer.
    pub(super) fn refresh_comparison(&mut self, host: &UiContext) {
        let Some(looks) = self.comparing.looks.as_ref() else {
            return;
        };
        let layers = self.layer_sources();
        let mut wanted = Vec::new();
        for layer in &layers {
            let baseline = self
                .comparing
                .baselines
                .entry(layer.number)
                .or_insert_with(|| {
                    looks::last_looked(looks, layer.number, &layer.head, BaselineRule::Engaged)
                });
            // A layer that moved while open is compared with the head it had.
            if let LastLooked::Unchanged(look) = baseline
                && look.head_oid != layer.head
            {
                *baseline = LastLooked::Since(look.clone());
            }
            let timeline = self.stack_history.as_ref().and_then(|history| {
                history
                    .layers
                    .iter()
                    .find(|timeline| timeline.number == layer.number)
            });
            let pair = looks::baseline_pair(
                timeline,
                layer.number,
                baseline,
                VersionPoint {
                    head_oid: layer.head.clone(),
                    fork_oid: Some(layer.fork.clone()),
                },
            );
            wanted.push((layer.source_id.clone(), pair));
        }
        let looked = self
            .comparing
            .baselines
            .values()
            .any(|baseline| baseline.look().is_some());
        if !looked {
            return;
        }
        let view = self.comparing.view;
        let mut asked = Vec::new();
        for (source_id, pair) in wanted {
            if self.comparing.pairs.get(&source_id) == Some(&pair) {
                continue;
            }
            self.comparing.pairs.insert(source_id.clone(), pair.clone());
            if let Some(review) = self.review.as_mut() {
                review
                    .doc
                    .set_comparison(&source_id, Some(GroupComparison { delta: None, view }));
            }
            if !asked.contains(&pair) {
                asked.push(pair);
            }
        }
        if let Some(review) = self.review.as_mut() {
            review.note_splices();
        }
        if asked.is_empty() {
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
                    format!("could not compare with what you last looked at: {error}"),
                    StatusSeverity::Warning,
                );
                return;
            }
        };
        let Some(review) = self.review.as_mut() else {
            return;
        };
        for delta in deltas {
            for (source_id, pair) in &self.comparing.pairs {
                if *pair == delta.pair {
                    review.doc.set_comparison(
                        source_id,
                        Some(GroupComparison {
                            delta: Some(delta.clone()),
                            view,
                        }),
                    );
                }
            }
        }
        review.note_splices();
    }

    /// Switch every compared source between the full change and only what changed.
    pub(super) fn toggle_compare_view(&mut self) {
        if !self.comparing.active() {
            self.set_status(
                "nothing to compare: no layer changed since you last looked, or you never did",
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
                CompareView::Changes => "showing only what changed since you looked",
                CompareView::Full => "showing the full change, what is new marked",
            },
            StatusSeverity::Info,
        );
    }

    /// Record a look of `kind` at the layer `file` belongs to (its current version).
    pub(super) fn record_look(&mut self, file: &FileKey, kind: LookKind) {
        self.record_layer_looks(Some(&file.source_id), kind);
    }

    /// Record a look of `kind` at the layer of source `source_id`, or at every layer.
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
}
