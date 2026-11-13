//! Opening a review progressively: what the first frame shows first, everything else after.
//!
//! [`open`] reports through a callback, one source at a time, so a frontend draws as soon as the
//! source the reviewer lands on is ready and fills in the rest as it arrives:
//!
//! 1. The first source (the one asked for, else the first included one): its provider is asked
//!    about it alone. A stack the registry remembers is used as it was ([`OpenEvent::Stack`]);
//!    with none, a stack layer is opened as its pull request after one quick request
//!    ([`quick_layer`]) instead of waiting for the stack walk.
//! 2. Every other stack source whose stack is known is reported as [`OpenEvent::Pending`]: a
//!    placeholder with its layers, branches, and commits, and no files yet. Then the first source
//!    is listed and reported ([`OpenEvent::Ready`], or [`OpenEvent::Failed`]).
//! 3. [`OpenStage::Confirming`]: every source is asked about, confirming a remembered stack with
//!    one batched request (or resolving it when nothing was remembered or it changed shape). The
//!    stacks are reported again as confirmed, and sources not yet shown as pending.
//! 4. [`OpenStage::Listing`]: the other sources (and the first, when confirming moved its
//!    commits) are resolved and listed in batches, nearest to the first source first, each
//!    reported as its batch finishes.
//! 5. [`OpenEvent::Done`].
//!
//! Everything blocks; callers run it on a blocking thread. See
//! `docs/review-loading-architecture.md`.

use crate::data::{DataError, ReviewData};
use crate::provider::{Freshness, ProviderRegistry, gather_provider_resolutions, quick_layer};
use bcode_code_review_models::{
    REVIEW_INDEX_MAX_PAGE_SIZE, ResolveReviewSourcesRequest, ResolvedReviewStack, ResolvedSource,
    ResolvedSourceMode, ResolvedStackLayer, ReviewIndexEntry, ReviewIndexManyRequest,
    ReviewIndexRequest, ReviewSource, ReviewSourceDiagnostic, ReviewSourceKind,
    ReviewSourceProviderOutcome, ReviewSourceProviderResolution, ReviewStack, RevisionRef,
};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::time::SystemTime;

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
    /// Set when the review no longer wants the open (it was closed or opened again): the open
    /// stops at its next step, and providers that check (see
    /// [`bcode_code_review_provider::cancelled`]) between their requests.
    pub cancel: crate::provider::Cancel,
}

/// What [`open`] is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenStage {
    /// Asking providers about the first source.
    AskingProviders,
    /// Resolving and listing the first source.
    OpeningFirst,
    /// Confirming (or resolving) the review's stacks with their providers.
    Confirming,
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
    /// Its stack, for stack and stack-layer sources once the stack is known.
    pub stack: Option<ResolvedReviewStack>,
}

/// A stack the review's stack sources follow, and when its provider last vouched for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackNews {
    /// Provider id.
    pub provider: String,
    /// Provider repository.
    pub repository: String,
    /// The stack.
    pub stack: ReviewStack,
    /// When its provider resolved or confirmed it (earlier than now for a remembered stack that is
    /// not confirmed yet).
    pub confirmed_at: SystemTime,
}

/// Progress of [`open`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenEvent {
    /// A stage started (or, for listing, advanced).
    Stage(OpenStage),
    /// A stack became known, or was confirmed.
    Stack(StackNews),
    /// A source to show before it is listed.
    Pending(OpenedSource),
    /// A source, listed (again, when its commits moved).
    Ready(OpenedSource),
    /// A source that could not be opened.
    Failed(ReviewSourceDiagnostic),
    /// Every source was reported.
    Done,
}

/// Open `request`'s sources progressively, reporting through `emit` (see the module docs).
///
/// `emit` answers whether to go on: [`ControlFlow::Break`] (the review was closed, say) stops the
/// open at the next event, without further events.
///
/// # Errors
///
/// * [`DataError::InvalidRequest`] when the repository path is invalid, and git failures that stop
///   every source. Problems of one source are [`OpenEvent::Failed`].
pub fn open(
    data: &ReviewData,
    registry: &ProviderRegistry,
    request: &OpenRequest,
    mut emit: impl FnMut(OpenEvent) -> ControlFlow<()>,
) -> Result<(), DataError> {
    if open_until_stopped(data, registry, request, &mut emit)?.is_continue() {
        let _ = emit(OpenEvent::Done);
    }
    Ok(())
}

/// Shorthand for "stop when `emit` says so".
macro_rules! send {
    ($emit:expr, $event:expr) => {
        if $emit($event).is_break() {
            return Ok(ControlFlow::Break(()));
        }
    };
}

/// Shorthand for "stop when the open was cancelled".
macro_rules! unless_cancelled {
    ($request:expr) => {
        if $request.cancel.is_cancelled() {
            return Ok(ControlFlow::Break(()));
        }
    };
}

fn open_until_stopped(
    data: &ReviewData,
    registry: &ProviderRegistry,
    request: &OpenRequest,
    emit: &mut dyn FnMut(OpenEvent) -> ControlFlow<()>,
) -> Result<ControlFlow<()>, DataError> {
    let included: Vec<&ReviewSource> = request
        .sources
        .iter()
        .filter(|source| source.included)
        .collect();
    let Some(first_index) = request
        .first
        .as_deref()
        .and_then(|first| included.iter().position(|source| source.id == first))
        .or_else(|| (!included.is_empty()).then_some(0))
    else {
        return Ok(ControlFlow::Continue(()));
    };
    let first = included[first_index];
    let mut stacks = Vec::new();

    // 1. The first source.
    if needs_provider(&[first]) {
        send!(emit, OpenEvent::Stage(OpenStage::AskingProviders));
    }
    let first_resolutions =
        crate::provider::with_cancel(&request.cancel, || first_answer(registry, request, first));
    unless_cancelled!(request);
    if send_stacks(registry, &included, &first_resolutions, &mut stacks, emit)?.is_break() {
        return Ok(ControlFlow::Break(()));
    }
    let rest: Vec<&ReviewSource> = included
        .iter()
        .copied()
        .filter(|source| source.id != first.id)
        .collect();
    // 2. Placeholders for the other stack sources whose stack is known: no git, so the first
    // frame shows the whole stack.
    let mut shown: Vec<&str> = Vec::new();
    let known: Vec<ReviewSourceProviderResolution> = rest
        .iter()
        .filter_map(|source| known_resolution(registry, source, &first_resolutions))
        .collect();
    for source in &rest {
        if let Some(pending) = placeholder(source, &known) {
            shown.push(&source.id);
            send!(emit, OpenEvent::Pending(pending));
        }
    }

    unless_cancelled!(request);
    send!(emit, OpenEvent::Stage(OpenStage::OpeningFirst));
    if resolve_and_list(data, request, &[first], &first_resolutions, &mut |event| {
        emit(event)
    })?
    .is_break()
    {
        return Ok(ControlFlow::Break(()));
    }
    if rest.is_empty() {
        return Ok(ControlFlow::Continue(()));
    }

    // 3. Confirm or resolve every stack; the first source is listed again only when that moved
    // its commits.
    unless_cancelled!(request);
    let mut confirmed = if needs_provider(&included) {
        send!(emit, OpenEvent::Stage(OpenStage::Confirming));
        crate::provider::with_cancel(&request.cancel, || {
            gather_provider_resolutions(
                registry,
                &request.repo_path,
                &included
                    .iter()
                    .map(|source| (*source).clone())
                    .collect::<Vec<_>>(),
                request.max_layers,
                Freshness::Confirmed,
            )
        })
    } else {
        Vec::new()
    };
    unless_cancelled!(request);
    if send_stacks(registry, &included, &confirmed, &mut stacks, emit)?.is_break() {
        return Ok(ControlFlow::Break(()));
    }
    for source in &rest {
        if !shown.contains(&source.id.as_str())
            && let Some(pending) = placeholder(source, &confirmed)
        {
            send!(emit, OpenEvent::Pending(pending));
        }
    }
    let mut relist = rest;
    if source_pins(&confirmed, &first.id) == source_pins(&first_resolutions, &first.id) {
        confirmed.retain(|resolution| resolution.source_id != first.id);
    } else {
        relist.push(first);
    }

    // 4. The rest, nearest to the first source first.
    let position = |source: &&ReviewSource| {
        included
            .iter()
            .position(|included| included.id == source.id)
            .unwrap_or(usize::MAX)
    };
    relist.sort_by_key(|source| position(source).abs_diff(first_index));
    let total = relist.len();
    send!(
        emit,
        OpenEvent::Stage(OpenStage::Listing { done: 0, total })
    );
    resolve_and_list(data, request, &relist, &confirmed, &mut |event| emit(event))
}

/// Whether any of `sources` needs a review-source provider.
fn needs_provider(sources: &[&ReviewSource]) -> bool {
    sources
        .iter()
        .any(|source| bcode_code_review_models::source_kind_requires_provider(&source.kind))
}

/// The providers' answer for the first source: a remembered stack as it was; else, for a stack
/// layer, its pull request alone ([`quick_layer`]); else what resolving it says.
fn first_answer(
    registry: &ProviderRegistry,
    request: &OpenRequest,
    first: &ReviewSource,
) -> Vec<ReviewSourceProviderResolution> {
    if !needs_provider(&[first]) {
        return Vec::new();
    }
    if let ReviewSourceKind::StackLayer {
        provider,
        repository,
        number,
    } = &first.kind
        && registry
            .known_stack(provider, repository, *number)
            .is_none()
        && let Some(route) = registry.source(provider)
        && let Some(outcome) = quick_layer(registry, route, &request.repo_path, repository, *number)
    {
        return vec![ReviewSourceProviderResolution {
            source_id: first.id.clone(),
            outcome,
        }];
    }
    gather_provider_resolutions(
        registry,
        &request.repo_path,
        std::slice::from_ref(first),
        request.max_layers,
        Freshness::Remembered,
    )
}

/// Report the stacks among `resolutions` (answers for `sources`) not reported as they are yet
/// (`sent`).
fn send_stacks(
    registry: &ProviderRegistry,
    sources: &[&ReviewSource],
    resolutions: &[ReviewSourceProviderResolution],
    sent: &mut Vec<ReviewStack>,
    emit: &mut dyn FnMut(OpenEvent) -> ControlFlow<()>,
) -> Result<ControlFlow<()>, DataError> {
    for resolution in resolutions {
        let ReviewSourceProviderOutcome::Resolved {
            stack: Some(stack), ..
        } = &resolution.outcome
        else {
            continue;
        };
        if sent.contains(stack) {
            continue;
        }
        sent.push(stack.clone());
        let Some((provider, repository)) = sources
            .iter()
            .find(|source| source.id == resolution.source_id)
            .and_then(|source| match &source.kind {
                ReviewSourceKind::StackLayer {
                    provider,
                    repository,
                    ..
                }
                | ReviewSourceKind::Stack {
                    provider,
                    repository,
                    ..
                } => Some((provider.clone(), repository.clone())),
                _ => None,
            })
        else {
            continue;
        };
        let age = stack
            .layers
            .first()
            .and_then(|layer| registry.known_stack_age(&provider, &repository, layer.pull.number))
            .unwrap_or_default();
        send!(
            emit,
            OpenEvent::Stack(StackNews {
                provider,
                repository,
                stack: stack.clone(),
                confirmed_at: SystemTime::now()
                    .checked_sub(age)
                    .unwrap_or(SystemTime::UNIX_EPOCH),
            })
        );
    }
    Ok(ControlFlow::Continue(()))
}

/// Resolve `sources` (with their provider answers) and list every resolved source fully, in
/// batches, reporting each source as its batch finishes ([`OpenEvent::Ready`] or
/// [`OpenEvent::Failed`]) and, for more than one source, progress after each batch.
fn resolve_and_list(
    data: &ReviewData,
    request: &OpenRequest,
    sources: &[&ReviewSource],
    resolutions: &[ReviewSourceProviderResolution],
    emit: &mut dyn FnMut(OpenEvent) -> ControlFlow<()>,
) -> Result<ControlFlow<()>, DataError> {
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
    for diagnostic in resolved.diagnostics {
        send!(emit, OpenEvent::Failed(diagnostic));
    }
    // Resolution keeps workspace order; listing follows the order asked for.
    let mut ordered: Vec<&ResolvedSource> = resolved.sources.iter().collect();
    ordered.sort_by_key(|source| {
        ids.iter()
            .position(|id| *id == source.source_id)
            .unwrap_or(usize::MAX)
    });
    // Every listing at once (one git process), then reported in batches.
    data.prepare_listings(
        &request.repo_path,
        &ordered
            .iter()
            .map(|source| (*source).clone())
            .collect::<Vec<_>>(),
    )?;
    let total = sources.len();
    let mut done = 0;
    for batch in ordered.chunks(LISTING_BATCH) {
        unless_cancelled!(request);
        let listed = data.index_get_many(&ReviewIndexManyRequest {
            repo_path: request.repo_path.clone(),
            sources: batch.iter().map(|source| (*source).clone()).collect(),
            limit: Some(REVIEW_INDEX_MAX_PAGE_SIZE),
        })?;
        for (source, outcome) in batch.iter().zip(listed.indexes) {
            let event = match outcome.index {
                Some(first_page) => OpenEvent::Ready(OpenedSource {
                    source: (*source).clone(),
                    entries: rest_of_index(data, request, source, first_page)?,
                    stack: resolved
                        .stacks
                        .iter()
                        .find(|stack| stack.source_id == source.source_id)
                        .cloned(),
                }),
                None => OpenEvent::Failed(ReviewSourceDiagnostic {
                    source_id: source.source_id.clone(),
                    severity: bcode_code_review_models::ReviewSourceDiagnosticSeverity::Error,
                    code: outcome.error_code.unwrap_or_default(),
                    message: outcome.error_message.unwrap_or_default(),
                }),
            };
            send!(emit, event);
        }
        done += batch.len();
        if total > 1 {
            send!(emit, OpenEvent::Stage(OpenStage::Listing { done, total }));
        }
    }
    Ok(ControlFlow::Continue(()))
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
    id: &str,
) -> Option<Pins<'a>> {
    resolutions
        .iter()
        .find(|resolution| resolution.source_id == id)
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
