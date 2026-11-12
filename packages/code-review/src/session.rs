//! Opening a review progressively: what the first frame shows first, everything else after.
//!
//! [`open`] reports through a callback, so a frontend can draw as soon as the source the reviewer
//! lands on is ready and fill in the rest without waiting:
//!
//! 1. [`OpenEvent::Stage`] `AskingProviders`: pull-request and stack sources are resolved with
//!    their providers and their commits made local in one batched fetch. Only the first source
//!    is asked about first, and a remembered stack is used as it was, so a review opened from
//!    the wizard asks nothing before its first frame.
//! 2. [`OpenEvent::Ready`]: the first source (the one asked for, else the first included one) is
//!    resolved and fully listed. Every other stack source comes with it as a *pending* source: a
//!    placeholder built from what the providers already reported (its layers, branches, and
//!    commits), with no files yet, so the frontend can show the whole stack at once.
//! 3. [`OpenEvent::Stage`] `AskingProviders` again: every source is asked about, confirming a
//!    remembered stack that is not fresh with one batched request; then `Listing`: the other
//!    sources (and the first, when confirming moved its commits) are resolved and listed in
//!    parallel batches, reporting progress.
//! 4. [`OpenEvent::Rest`]: every source, in workspace order, fully listed.
//!
//! A review with one source reports only `Ready`. Everything blocks; callers run it on a blocking
//! thread. See `docs/review-loading-architecture.md`.

use crate::data::{DataError, ReviewData};
use crate::provider::{Freshness, ProviderRegistry, gather_provider_resolutions};
use bcode_code_review_models::{
    REVIEW_INDEX_MAX_PAGE_SIZE, ResolveReviewSourcesRequest, ResolvedReviewStack, ResolvedSource,
    ResolvedSourceMode, ResolvedStackLayer, ReviewIndexEntry, ReviewIndexManyRequest,
    ReviewIndexRequest, ReviewSource, ReviewSourceDiagnostic, ReviewSourceKind,
    ReviewSourceProviderOutcome, ReviewSourceProviderResolution, RevisionRef,
};
use std::ops::ControlFlow;
use std::path::PathBuf;

/// Sources listed per [`ReviewData::index_get_many`] call while listing the rest, so progress is
/// reported as batches finish.
const LISTING_BATCH: usize = 16;

/// What to open.
#[derive(Debug, Clone)]
pub struct OpenRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// The review's sources (excluded ones are skipped).
    pub sources: Vec<ReviewSource>,
    /// The source to make ready first (for example the one the reviewer was last in).
    pub first: Option<String>,
    /// Most layers a provider resolves per stack (`None`: all).
    pub max_layers: Option<usize>,
}

/// What [`open`] is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenStage {
    /// Asking providers about pull requests and stacks.
    AskingProviders,
    /// Resolving and listing the first source.
    OpeningFirst,
    /// Listing the other sources: `done` of `total`.
    Listing {
        /// Sources listed.
        done: usize,
        /// Sources to list.
        total: usize,
    },
}

/// One resolved source with its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedSource {
    /// The resolved source (for a pending source, a placeholder: its own commits, no files yet).
    pub source: ResolvedSource,
    /// Every index entry, in index order (empty for a pending source).
    pub entries: Vec<ReviewIndexEntry>,
    /// Its stack, for stack and stack-layer sources.
    pub stack: Option<ResolvedReviewStack>,
}

/// Sources ready to show, plus the ones still loading.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenedSources {
    /// Ready sources, in workspace order.
    pub ready: Vec<OpenedSource>,
    /// Sources still loading, in workspace order (placeholders, without files).
    pub pending: Vec<OpenedSource>,
    /// Per-source problems.
    pub diagnostics: Vec<ReviewSourceDiagnostic>,
}

/// Progress of [`open`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenEvent {
    /// A stage started.
    Stage(OpenStage),
    /// The first source is ready (with the others pending).
    Ready(OpenedSources),
    /// Every source is ready.
    Rest(OpenedSources),
}

/// Open `request`'s sources progressively, reporting through `emit` (see the module docs).
///
/// `emit` answers whether to go on: [`ControlFlow::Break`] (the review was closed, say) stops the
/// open at the next stage or listing batch, without further events.
///
/// # Errors
///
/// * [`DataError::InvalidRequest`] when the repository path is invalid, and git failures that stop
///   every source. Problems of one source are diagnostics.
pub fn open(
    data: &ReviewData,
    registry: &ProviderRegistry,
    request: &OpenRequest,
    mut emit: impl FnMut(OpenEvent) -> ControlFlow<()>,
) -> Result<(), DataError> {
    let included: Vec<&ReviewSource> = request
        .sources
        .iter()
        .filter(|source| source.included)
        .collect();
    let first_id = request
        .first
        .as_deref()
        .filter(|first| included.iter().any(|source| source.id == *first))
        .or_else(|| included.first().map(|source| source.id.as_str()))
        .map(str::to_owned);
    let (first, rest): (Vec<&ReviewSource>, Vec<&ReviewSource>) = included
        .iter()
        .partition(|source| Some(&source.id) == first_id.as_ref());
    // Providers are asked about the first source alone first: its stack (reused when recently
    // resolved) and only the layers it reaches, so the first frame does not wait for every
    // layer's commits.
    // A remembered stack is shown as it was; it is confirmed with the rest.
    let Some(first_resolutions) = ask(registry, request, &first, Freshness::Remembered, &mut emit)
    else {
        return Ok(());
    };
    if emit(OpenEvent::Stage(OpenStage::OpeningFirst)).is_break() {
        return Ok(());
    }
    let Some(first_opened) =
        resolve_and_list(data, request, &first, &first_resolutions, &mut |_| {
            ControlFlow::Continue(())
        })?
    else {
        return Ok(());
    };
    if rest.is_empty() {
        let _ = emit(OpenEvent::Ready(first_opened));
        return Ok(());
    }
    // The other stack sources' placeholders come from their stacks (already known: the first
    // source's stack, or recently resolved ones) and the commits the providers reported, without
    // fetching anything yet.
    let known: Vec<ReviewSourceProviderResolution> = rest
        .iter()
        .filter_map(|source| known_resolution(registry, source, &first_resolutions))
        .collect();
    let pending: Vec<OpenedSource> = rest
        .iter()
        .filter_map(|source| placeholder(source, &known))
        .collect();
    if emit(OpenEvent::Ready(OpenedSources {
        ready: first_opened.ready.clone(),
        pending,
        diagnostics: first_opened.diagnostics.clone(),
    }))
    .is_break()
    {
        return Ok(());
    }
    // Every source is asked again, confirming a remembered stack; the first source is listed
    // again only when that changed its answer (a layer's commits moved).
    let Some(mut confirmed) = ask(
        registry,
        request,
        &included,
        Freshness::Confirmed,
        &mut emit,
    ) else {
        return Ok(());
    };
    let first_changed = source_pins(&confirmed, first_id.as_deref())
        != source_pins(&first_resolutions, first_id.as_deref());
    let (relist, first_opened) = if first_changed {
        (included.clone(), OpenedSources::default())
    } else {
        confirmed.retain(|resolution| Some(&resolution.source_id) != first_id.as_ref());
        (rest.clone(), first_opened)
    };
    let total = relist.len();
    if emit(OpenEvent::Stage(OpenStage::Listing { done: 0, total })).is_break() {
        return Ok(());
    }
    let Some(rest_opened) = resolve_and_list(data, request, &relist, &confirmed, &mut |done| {
        emit(OpenEvent::Stage(OpenStage::Listing { done, total }))
    })?
    else {
        return Ok(());
    };
    let _ = emit(OpenEvent::Rest(in_order(
        &included,
        first_opened,
        rest_opened,
    )));
    Ok(())
}

/// Ask `registry`'s providers about `sources` (those that need one), trusting remembered stacks
/// as `freshness` says. `None` when `emit` says to stop.
fn ask(
    registry: &ProviderRegistry,
    request: &OpenRequest,
    sources: &[&ReviewSource],
    freshness: Freshness,
    emit: &mut dyn FnMut(OpenEvent) -> ControlFlow<()>,
) -> Option<Vec<ReviewSourceProviderResolution>> {
    if !sources
        .iter()
        .any(|source| bcode_code_review_models::source_kind_requires_provider(&source.kind))
    {
        return Some(Vec::new());
    }
    if emit(OpenEvent::Stage(OpenStage::AskingProviders)).is_break() {
        return None;
    }
    let sources: Vec<ReviewSource> = sources.iter().map(|source| (*source).clone()).collect();
    Some(gather_provider_resolutions(
        registry,
        &request.repo_path,
        &sources,
        request.max_layers,
        freshness,
    ))
}

/// `first` and `rest` together, in the order of `included` (the workspace's).
fn in_order(
    included: &[&ReviewSource],
    first: OpenedSources,
    rest: OpenedSources,
) -> OpenedSources {
    let order: Vec<&str> = included.iter().map(|source| source.id.as_str()).collect();
    let mut ready: Vec<OpenedSource> = first.ready.into_iter().chain(rest.ready).collect();
    ready.sort_by_key(|opened| {
        order
            .iter()
            .position(|id| *id == opened.source.source_id)
            .unwrap_or(usize::MAX)
    });
    let mut diagnostics = first.diagnostics;
    diagnostics.extend(rest.diagnostics);
    OpenedSources {
        ready,
        pending: Vec::new(),
        diagnostics,
    }
}

/// Resolve `sources` (with their provider answers) and list every resolved source fully,
/// reporting the number of sources listed through `progress` after each batch. `None` when
/// `progress` says to stop.
fn resolve_and_list(
    data: &ReviewData,
    request: &OpenRequest,
    sources: &[&ReviewSource],
    resolutions: &[ReviewSourceProviderResolution],
    progress: &mut dyn FnMut(usize) -> ControlFlow<()>,
) -> Result<Option<OpenedSources>, DataError> {
    let ids: Vec<&str> = sources.iter().map(|source| source.id.as_str()).collect();
    let resolved = data.resolve_sources(&ResolveReviewSourcesRequest {
        repo_path: request.repo_path.clone(),
        sources: sources.iter().map(|source| (*source).clone()).collect(),
        provider_resolutions: resolutions
            .iter()
            .filter(|resolution| ids.contains(&resolution.source_id.as_str()))
            .cloned()
            .collect(),
    })?;
    let mut ready = Vec::with_capacity(resolved.sources.len());
    let mut diagnostics = resolved.diagnostics;
    let mut done = 0;
    for batch in resolved.sources.chunks(LISTING_BATCH) {
        let listed = data.index_get_many(&ReviewIndexManyRequest {
            repo_path: request.repo_path.clone(),
            sources: batch.to_vec(),
            limit: Some(REVIEW_INDEX_MAX_PAGE_SIZE),
        })?;
        for (source, outcome) in batch.iter().zip(listed.indexes) {
            match outcome.index {
                Some(first_page) => {
                    let entries = rest_of_index(data, request, source, first_page)?;
                    ready.push(OpenedSource {
                        source: source.clone(),
                        entries,
                        stack: resolved
                            .stacks
                            .iter()
                            .find(|stack| stack.source_id == source.source_id)
                            .cloned(),
                    });
                }
                None => diagnostics.push(ReviewSourceDiagnostic {
                    source_id: source.source_id.clone(),
                    severity: bcode_code_review_models::ReviewSourceDiagnosticSeverity::Error,
                    code: outcome.error_code.unwrap_or_default(),
                    message: outcome.error_message.unwrap_or_default(),
                }),
            }
        }
        done += batch.len();
        if progress(done).is_break() {
            return Ok(None);
        }
    }
    Ok(Some(OpenedSources {
        ready,
        pending: Vec::new(),
        diagnostics,
    }))
}

/// Every entry of `source`'s index, from its first page on.
fn rest_of_index(
    data: &ReviewData,
    request: &OpenRequest,
    source: &ResolvedSource,
    first_page: bcode_code_review_models::ReviewIndexResponse,
) -> Result<Vec<ReviewIndexEntry>, DataError> {
    let mut entries = first_page.entries;
    let mut cursor = first_page.next_cursor;
    while let Some(next) = cursor {
        let page = data.index_get(&ReviewIndexRequest {
            repo_path: request.repo_path.clone(),
            source: source.clone(),
            cursor: Some(next),
            limit: Some(REVIEW_INDEX_MAX_PAGE_SIZE),
        })?;
        entries.extend(page.entries);
        cursor = page.next_cursor;
    }
    Ok(entries)
}

/// The commits a provider answer pins: each pull request's number, head, and base; or the
/// failure's code.
type Pins<'a> = Result<Vec<(u64, &'a str, &'a str)>, &'a str>;

/// [`pins`] of the answer for source `id` among `resolutions`.
fn source_pins<'a>(
    resolutions: &'a [ReviewSourceProviderResolution],
    id: Option<&str>,
) -> Option<Pins<'a>> {
    resolutions
        .iter()
        .find(|resolution| Some(resolution.source_id.as_str()) == id)
        .map(pins)
}

/// The commits a provider answer pins (each pull request's head and base), or its failure: what
/// decides whether a source must be listed again.
fn pins(resolution: &ReviewSourceProviderResolution) -> Pins<'_> {
    match &resolution.outcome {
        ReviewSourceProviderOutcome::Resolved { fetched, .. } => Ok(fetched
            .iter()
            .map(|fetch| {
                (
                    fetch.number,
                    fetch.head_oid.as_str(),
                    fetch.base_oid.as_str(),
                )
            })
            .collect()),
        ReviewSourceProviderOutcome::Failed { code, .. } => Err(code.as_str()),
    }
}

/// What is already known about stack `source` without asking its provider: its stack (the first
/// source's, or one resolved recently) with the commits that stack's provider reported, as an
/// answer [`placeholder`] can use. `None` when nothing is known.
fn known_resolution(
    registry: &ProviderRegistry,
    source: &ReviewSource,
    first: &[ReviewSourceProviderResolution],
) -> Option<ReviewSourceProviderResolution> {
    let (provider, repository, number) = match &source.kind {
        ReviewSourceKind::StackLayer {
            provider,
            repository,
            number,
        } => (provider, repository, *number),
        ReviewSourceKind::Stack {
            provider,
            repository,
            top,
        } => (provider, repository, *top),
        _ => return None,
    };
    let from_first = first
        .iter()
        .find_map(|resolution| match &resolution.outcome {
            ReviewSourceProviderOutcome::Resolved {
                stack: Some(stack),
                fetched,
            } if stack.layer(number).is_some() => Some((stack.clone(), fetched.clone())),
            _ => None,
        });
    let (stack, mut fetched) = from_first.or_else(|| {
        registry
            .known_stack(provider, repository, number)
            .map(|stack| (stack, Vec::new()))
    })?;
    // Layers the first source did not fetch stand in with the commits the provider reported. Only
    // a layer's head is needed; the bottom layer's base (the trunk) must be known.
    for layer in &stack.layers {
        if fetched
            .iter()
            .any(|fetch| fetch.number == layer.pull.number)
            || layer.pull.head_oid.is_empty()
        {
            continue;
        }
        fetched.push(bcode_code_review_models::ReviewPullFetchResult {
            number: layer.pull.number,
            head_ref: String::new(),
            head_oid: layer.pull.head_oid.clone(),
            base_branch: layer.pull.base_branch.clone(),
            base_ref: String::new(),
            base_oid: layer.pull.base_oid.clone().unwrap_or_default(),
        });
    }
    Some(ReviewSourceProviderResolution {
        source_id: source.id.clone(),
        outcome: ReviewSourceProviderOutcome::Resolved {
            stack: Some(stack),
            fetched,
        },
    })
}

/// A pending stand-in for a stack source from its provider answer: the layers it reaches, with the
/// commits and branches the provider reported, and no files. `None` for other sources and when the
/// answer is incomplete (the source then appears when it is ready).
fn placeholder(
    source: &ReviewSource,
    resolutions: &[ReviewSourceProviderResolution],
) -> Option<OpenedSource> {
    let number = match &source.kind {
        ReviewSourceKind::StackLayer { number, .. } => *number,
        ReviewSourceKind::Stack { top, .. } => *top,
        _ => return None,
    };
    let ReviewSourceProviderOutcome::Resolved {
        stack: Some(stack),
        fetched,
    } = &resolutions
        .iter()
        .find(|resolution| resolution.source_id == source.id)?
        .outcome
    else {
        return None;
    };
    let layers = stack.layers_through(number)?;
    let fetch = |layer: u64| fetched.iter().find(|fetch| fetch.number == layer);
    let trunk_oid =
        Some(fetch(layers.first()?.pull.number)?.base_oid.clone()).filter(|oid| !oid.is_empty())?;
    let mut below = trunk_oid.clone();
    let mut resolved_layers = Vec::with_capacity(layers.len());
    for layer in layers {
        let head = fetch(layer.pull.number)?.head_oid.clone();
        resolved_layers.push(ResolvedStackLayer {
            number: layer.pull.number,
            title: layer.pull.title.clone(),
            // The layer below's head stands in for the merge base until the source resolves.
            base_oid: std::mem::replace(&mut below, head.clone()),
            head_oid: head,
            base_branch: layer.pull.base_branch.clone(),
            head_branch: layer.pull.head_branch.clone(),
        });
    }
    let top = resolved_layers.last()?;
    let whole = matches!(source.kind, ReviewSourceKind::Stack { .. });
    Some(OpenedSource {
        source: ResolvedSource {
            source_id: source.id.clone(),
            base: RevisionRef::commit(if whole {
                trunk_oid.clone()
            } else {
                top.base_oid.clone()
            }),
            head: RevisionRef::commit(top.head_oid.clone()),
            mode: ResolvedSourceMode::Diff,
            paths: Vec::new(),
            line_range: None,
            part: None,
        },
        entries: Vec::new(),
        stack: Some(ResolvedReviewStack {
            source_id: source.id.clone(),
            trunk: stack.trunk.clone(),
            trunk_oid: trunk_oid.clone(),
            base_oid: trunk_oid,
            layers: resolved_layers,
        }),
    })
}

#[cfg(test)]
mod budgets;
#[cfg(test)]
mod tests;
