//! Publish planning: which publish target each thread goes to, how its
//! anchor reads in that target's diff, and the provider-neutral bundle thread built from the
//! target diff's hunks.
//!
//! * A publisher that declares a review-source provider (`pull_request_provider`) publishes
//!   that provider's pull-request sources one pull request at a time: a pull request or stack
//!   layer source publishes its threads to its own pull request; a whole-stack source maps each
//!   line comment through `review.stack.attribution` to the layer that introduced (or removed)
//!   its line and publishes it to that layer's pull request at the line's number in the layer's
//!   own diff. A comment no single layer accounts for (a context line, or a range across
//!   layers) falls back to the top pull request's review summary, as do review-level comments.
//! * Every other thread, and every thread for other publishers, goes into one bundle for the
//!   publisher's default target.
//!
//! Nothing here performs I/O: the client gathers attributions and hunks and passes them in.

use crate::document::{AnchorTarget, target_of};
use bcode_code_review_models::{
    DraftAnchor, DraftComment, ResolvedReviewStack, ResolvedSource, ReviewAnchorKind,
    ReviewBundleLine, ReviewBundleThread, ReviewFileHunksResponse, ReviewLineKind,
    ReviewPullRequestTarget, ReviewSource, ReviewSourceKind, ReviewStackAttributionResponse,
    RevisionRef,
};
use std::collections::BTreeMap;

/// Most hunk-context lines a bundle thread carries.
pub const MAX_HUNK_CONTEXT_LINES: usize = 60;

/// One workspace source as the publish plan sees it.
#[derive(Debug, Clone, Copy)]
pub struct PlanSource<'a> {
    /// The workspace source.
    pub source: &'a ReviewSource,
    /// What it resolved to (its first part), when it resolved.
    pub resolved: Option<&'a ResolvedSource>,
    /// Its stack, for stack and stack-layer sources.
    pub stack: Option<&'a ResolvedReviewStack>,
}

/// A thread to publish: its id and comments, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishThread {
    /// Thread id.
    pub thread_id: String,
    /// Comments, oldest first; the first carries the anchor.
    pub comments: Vec<DraftComment>,
}

impl PublishThread {
    /// The thread's persisted anchor.
    #[must_use]
    pub fn anchor(&self) -> Option<&DraftAnchor> {
        self.comments.first().map(|comment| &comment.anchor)
    }
}

/// A thread placed in a planned bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedThread {
    /// The thread.
    pub thread: PublishThread,
    /// Its anchor in the target diff (the persisted anchor unless a whole-stack comment was
    /// mapped onto its layer).
    pub anchor: DraftAnchor,
    /// Whether the publisher should put it in its summary instead of anchoring it inline.
    pub summary_only: bool,
    /// The diff whose hunks the thread's lines are read from.
    pub diff: Option<ResolvedSource>,
}

/// One bundle to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedBundle {
    /// The pull request it goes to, when the publisher publishes pull requests of the source's
    /// provider.
    pub target: Option<ReviewPullRequestTarget>,
    /// Display label (`#101 Title`, or `review`).
    pub label: String,
    /// Workspace source the bundle publishes, when it is one pull-request source.
    pub source_id: Option<String>,
    /// Threads in plan order.
    pub threads: Vec<PlannedThread>,
}

/// Where one source's threads publish to, for a publisher of `provider`'s pull requests.
#[derive(Debug, Clone)]
enum Route<'a> {
    /// The source's own pull request.
    Pull(ReviewPullRequestTarget, String),
    /// A whole stack: per-layer mapping.
    Stack {
        provider: String,
        repository: String,
        stack: &'a ResolvedReviewStack,
    },
    /// The publisher's default target.
    Default,
}

fn route<'a>(source: &PlanSource<'a>, provider: Option<&str>) -> Route<'a> {
    let Some(provider) = provider else {
        return Route::Default;
    };
    match &source.source.kind {
        ReviewSourceKind::PullRequest {
            provider: kind_provider,
            repository,
            number,
        }
        | ReviewSourceKind::StackLayer {
            provider: kind_provider,
            repository,
            number,
        } if kind_provider == provider => {
            let title = source
                .stack
                .and_then(|stack| stack.layers.iter().find(|layer| layer.number == *number))
                .map_or_else(|| source.source.label.clone(), |layer| layer.title.clone());
            Route::Pull(
                ReviewPullRequestTarget {
                    provider: provider.to_string(),
                    repository: repository.clone(),
                    number: *number,
                },
                title,
            )
        }
        ReviewSourceKind::Stack {
            provider: kind_provider,
            repository,
            ..
        } if kind_provider == provider => {
            source.stack.map_or(Route::Default, |stack| Route::Stack {
                provider: provider.to_string(),
                repository: repository.clone(),
                stack,
            })
        }
        _ => Route::Default,
    }
}

/// Files whose whole-stack attribution the plan needs: `(source id, path)` of every line
/// comment on a whole-stack source routed to a pull request.
#[must_use]
pub fn attribution_needs(
    threads: &[PublishThread],
    sources: &[PlanSource<'_>],
    provider: Option<&str>,
) -> Vec<(String, String)> {
    let mut needs = std::collections::BTreeSet::new();
    for thread in threads {
        let Some(anchor) = thread.anchor() else {
            continue;
        };
        if anchor.kind != ReviewAnchorKind::Range {
            continue;
        }
        let Some(source) = source_of(anchor, sources) else {
            continue;
        };
        if matches!(route(source, provider), Route::Stack { .. }) {
            needs.insert((source.source.id.clone(), anchor.file_path.clone()));
        }
    }
    needs.into_iter().collect()
}

fn source_of<'s, 'a>(
    anchor: &DraftAnchor,
    sources: &'s [PlanSource<'a>],
) -> Option<&'s PlanSource<'a>> {
    anchor
        .source_id
        .as_ref()
        .and_then(|id| sources.iter().find(|source| &source.source.id == id))
        .or_else(|| {
            (anchor.kind != ReviewAnchorKind::Review)
                .then(|| sources.iter().find(|source| source.source.included))
                .flatten()
        })
}

/// Plan the bundles for `threads`.
///
/// `attributions` holds the whole-stack attribution of the files [`attribution_needs`] named,
/// keyed by `(source id, path)`; a missing one makes that file's comments fall back to the top
/// pull request's summary.
#[must_use]
pub fn plan(
    threads: Vec<PublishThread>,
    sources: &[PlanSource<'_>],
    provider: Option<&str>,
    attributions: &BTreeMap<(String, String), ReviewStackAttributionResponse>,
) -> Vec<PlannedBundle> {
    let mut bundles: Vec<PlannedBundle> = Vec::new();
    // Review-level comments go to the top pull request of a whole stack, else to the last pull
    // request source, else to the default target.
    let review_target = review_level_target(sources, provider);
    for thread in threads {
        let Some(placed) = place(
            thread,
            sources,
            provider,
            attributions,
            review_target.as_ref(),
        ) else {
            continue;
        };
        let index = bundles
            .iter()
            .position(|bundle| bundle.target == placed.target)
            .unwrap_or_else(|| {
                bundles.push(PlannedBundle {
                    target: placed.target.clone(),
                    label: placed.label.clone(),
                    source_id: placed.source_id.clone(),
                    threads: Vec::new(),
                });
                bundles.len() - 1
            });
        if bundles[index].source_id != placed.source_id {
            bundles[index].source_id = None;
        }
        bundles[index].threads.push(placed.thread);
    }
    // Pull requests bottom of the stack first, the default bundle last.
    bundles.sort_by_key(|bundle| {
        bundle
            .target
            .as_ref()
            .map_or((1, u64::MAX), |target| (0, target.number))
    });
    bundles
}

/// Where one thread goes.
struct Placed {
    target: Option<ReviewPullRequestTarget>,
    label: String,
    source_id: Option<String>,
    thread: PlannedThread,
}

fn place(
    thread: PublishThread,
    sources: &[PlanSource<'_>],
    provider: Option<&str>,
    attributions: &BTreeMap<(String, String), ReviewStackAttributionResponse>,
    review_target: Option<&(ReviewPullRequestTarget, String)>,
) -> Option<Placed> {
    let anchor = thread.anchor().cloned()?;
    let source = source_of(&anchor, sources);
    let summary_only = anchor.kind != ReviewAnchorKind::Range;
    let diff = source.and_then(|source| source.resolved.cloned());
    if anchor.kind == ReviewAnchorKind::Review {
        let (target, label) = review_target.cloned().map_or_else(
            || (None, review_label()),
            |(target, label)| (Some(target), label),
        );
        return Some(Placed {
            target,
            label,
            source_id: None,
            thread: PlannedThread {
                anchor,
                thread,
                summary_only: true,
                diff: None,
            },
        });
    }
    let planned =
        |anchor: DraftAnchor, summary_only: bool, diff: Option<ResolvedSource>| PlannedThread {
            thread: thread.clone(),
            anchor,
            summary_only,
            diff,
        };
    match source.map_or(Route::Default, |source| route(source, provider)) {
        Route::Default => Some(Placed {
            target: None,
            label: review_label(),
            source_id: None,
            thread: planned(anchor, summary_only, diff),
        }),
        Route::Pull(target, title) => Some(Placed {
            label: format!("#{} {title}", target.number),
            target: Some(target),
            source_id: source.map(|source| source.source.id.clone()),
            thread: planned(anchor, summary_only, diff),
        }),
        Route::Stack {
            provider,
            repository,
            stack,
        } => {
            let top = stack.layers.last()?;
            let mapped = (!summary_only)
                .then(|| {
                    let key = (source?.source.id.clone(), anchor.file_path.clone());
                    map_to_layer(&anchor, attributions.get(&key)?)
                })
                .flatten()
                .and_then(|(number, mapped)| {
                    let layer = stack.layers.iter().find(|layer| layer.number == number)?;
                    Some((layer, mapped))
                });
            let (layer, thread) = match mapped {
                Some((layer, mapped)) => {
                    let diff = ResolvedSource {
                        source_id: stack.source_id.clone(),
                        base: RevisionRef::commit(&layer.base_oid),
                        head: RevisionRef::commit(&layer.head_oid),
                        mode: bcode_code_review_models::ResolvedSourceMode::Diff,
                        paths: Vec::new(),
                        line_range: None,
                        part: None,
                    };
                    (layer, planned(mapped, false, Some(diff)))
                }
                None => (top, planned(anchor, true, diff)),
            };
            Some(Placed {
                target: Some(ReviewPullRequestTarget {
                    provider,
                    repository,
                    number: layer.number,
                }),
                label: format!("#{} {}", layer.number, layer.title),
                source_id: None,
                thread,
            })
        }
    }
}

fn review_label() -> String {
    "review".to_string()
}

fn review_level_target(
    sources: &[PlanSource<'_>],
    provider: Option<&str>,
) -> Option<(ReviewPullRequestTarget, String)> {
    let mut pulls = Vec::new();
    for source in sources.iter().filter(|source| source.source.included) {
        match route(source, provider) {
            Route::Stack {
                provider,
                repository,
                stack,
            } => {
                let top = stack.layers.last()?;
                return Some((
                    ReviewPullRequestTarget {
                        provider,
                        repository,
                        number: top.number,
                    },
                    format!("#{} {}", top.number, top.title),
                ));
            }
            Route::Pull(target, title) => {
                let label = format!("#{} {title}", target.number);
                pulls.push((target, label));
            }
            Route::Default => {}
        }
    }
    // The topmost pull request (the highest number of a step-through stack is not necessarily
    // the top, so take the last source in workspace order).
    pulls.pop()
}

/// Map a whole-stack line anchor onto the layer that introduced (or removed) its lines.
///
/// Returns the layer's number and the anchor in that layer's own diff; `None` when no single
/// layer accounts for the anchored lines (or the service did not report the layer's line
/// numbers).
#[must_use]
pub fn map_to_layer(
    anchor: &DraftAnchor,
    attribution: &ReviewStackAttributionResponse,
) -> Option<(u64, DraftAnchor)> {
    let AnchorTarget::Lines(lines) = target_of(anchor) else {
        return None;
    };
    let map_range = |range: Option<(u32, u32)>,
                     origin: &dyn Fn(u32) -> Option<(u64, Option<u32>)>|
     -> Result<Option<(u64, u32, u32)>, ()> {
        let Some((start, end)) = range else {
            return Ok(None);
        };
        let (layer, first) = origin(start).ok_or(())?;
        let (end_layer, last) = origin(end).ok_or(())?;
        let (first, last) = (first.ok_or(())?, last.ok_or(())?);
        let span = end.checked_sub(start).ok_or(())?;
        if layer != end_layer || first.checked_add(span) != Some(last) {
            return Err(());
        }
        Ok(Some((layer, first, last)))
    };
    let old = map_range(lines.old, &|line| attribution.old_line_origin(line));
    let new = map_range(lines.new, &|line| attribution.new_line_origin(line));
    let mut mapped = anchor.clone();
    let layer = match (old, new) {
        (Ok(old), Ok(new)) => {
            let layers: Vec<u64> = old.iter().chain(new.iter()).map(|m| m.0).collect();
            let layer = *layers.first()?;
            if layers.iter().any(|other| *other != layer) {
                return single_line(anchor, attribution);
            }
            apply_side(&mut mapped, old, new);
            layer
        }
        _ => return single_line(anchor, attribution),
    };
    Some((layer, mapped))
}

/// The anchor narrowed to its last line, when that line maps onto a layer.
fn single_line(
    anchor: &DraftAnchor,
    attribution: &ReviewStackAttributionResponse,
) -> Option<(u64, DraftAnchor)> {
    let AnchorTarget::Lines(lines) = target_of(anchor) else {
        return None;
    };
    let end = lines.end_line();
    let mut mapped = anchor.clone();
    match lines.end_side {
        bcode_code_review_models::ReviewLineSide::Old => {
            let (layer, line) = attribution.old_line_origin(end)?;
            apply_side(&mut mapped, Some((layer, line?, line?)), None);
            Some((layer, mapped))
        }
        bcode_code_review_models::ReviewLineSide::New => {
            let (layer, line) = attribution.new_line_origin(end)?;
            apply_side(&mut mapped, None, Some((layer, line?, line?)));
            Some((layer, mapped))
        }
    }
}

fn apply_side(
    anchor: &mut DraftAnchor,
    old: Option<(u64, u32, u32)>,
    new: Option<(u64, u32, u32)>,
) {
    let set = |range: Option<(u64, u32, u32)>| -> (Option<u32>, Option<u32>, Option<u32>) {
        range.map_or((None, None, None), |(_, start, end)| {
            (Some(start), Some(end), Some(end))
        })
    };
    (anchor.old_start, anchor.old_end, anchor.old_line) = set(old);
    (anchor.new_start, anchor.new_end, anchor.new_line) = set(new);
    if old.is_some() && new.is_none() {
        anchor.line_kind = ReviewLineKind::Removed;
    } else if new.is_some() && old.is_none() && anchor.line_kind == ReviewLineKind::Removed {
        anchor.line_kind = ReviewLineKind::Added;
    }
    anchor.diff_row = u64::from(anchor.new_line.or(anchor.old_line).unwrap_or_default());
}

/// Build the bundle thread for `planned` from its diff's hunks of the anchored file (`None`
/// when the file has no hunks in that diff or none were loaded).
#[must_use]
pub fn bundle_thread(
    planned: &PlannedThread,
    hunks: Option<&ReviewFileHunksResponse>,
) -> ReviewBundleThread {
    let thread = &planned.thread;
    let anchor = &planned.anchor;
    let mut selected_lines = Vec::new();
    let mut hunk_context = Vec::new();
    if let (AnchorTarget::Lines(lines), Some(hunks)) = (target_of(anchor), hunks) {
        let within = |range: Option<(u32, u32)>, line: Option<u32>| {
            range
                .zip(line)
                .is_some_and(|((start, end), line)| (start..=end).contains(&line))
        };
        for hunk in &hunks.hunks {
            let mut in_hunk = false;
            for line in &hunk.lines {
                let selected = match line.kind {
                    ReviewLineKind::Removed => within(lines.old, line.old_line),
                    ReviewLineKind::Added => within(lines.new, line.new_line),
                    ReviewLineKind::Context => {
                        within(lines.new, line.new_line) || within(lines.old, line.old_line)
                    }
                };
                if selected {
                    in_hunk = true;
                    selected_lines.push(ReviewBundleLine {
                        file_path: anchor.file_path.clone(),
                        kind: line.kind,
                        old_line: line.old_line,
                        new_line: line.new_line,
                        diff_row: u64::from(line.new_line.or(line.old_line).unwrap_or_default()),
                        content: line.content.clone(),
                        surface_id: None,
                        source_id: anchor.source_id.clone(),
                    });
                }
            }
            if in_hunk && hunk_context.is_empty() {
                hunk_context.push(format!(
                    "@@ -{},{} +{},{} @@{}",
                    hunk.old_start,
                    hunk.old_count,
                    hunk.new_start,
                    hunk.new_count,
                    hunk.heading
                        .as_ref()
                        .map_or_else(String::new, |heading| format!(" {heading}"))
                ));
                hunk_context.extend(
                    hunk.lines
                        .iter()
                        .take(MAX_HUNK_CONTEXT_LINES)
                        .map(|line| format!("{}{}", marker(line.kind), line.content)),
                );
            }
        }
    }
    let selected_diff_lines = selected_lines
        .iter()
        .map(|line| format!("{}{}", marker(line.kind), line.content))
        .collect();
    ReviewBundleThread {
        thread_id: thread.thread_id.clone(),
        anchor: anchor.clone(),
        comments: thread.comments.clone(),
        session_id: thread
            .comments
            .iter()
            .find_map(|comment| comment.session_id.clone()),
        resolved_at_ms: thread
            .comments
            .iter()
            .all(|comment| comment.resolved_at_ms.is_some())
            .then(|| {
                thread
                    .comments
                    .iter()
                    .filter_map(|comment| comment.resolved_at_ms)
                    .max()
            })
            .flatten(),
        selected_lines,
        selected_diff_lines,
        hunk_context,
        summary_only: planned.summary_only,
    }
}

const fn marker(kind: ReviewLineKind) -> char {
    match kind {
        ReviewLineKind::Added => '+',
        ReviewLineKind::Removed => '-',
        ReviewLineKind::Context => ' ',
    }
}

#[cfg(test)]
mod tests;
