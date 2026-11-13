//! Source resolution (`review.sources.resolve`) and change tokens (`review.sources.token`).

use super::{DataError, git};
use crate::cache::{CacheKind, ReviewCache};
use bcode_code_review_models::{
    REVIEW_DIAGNOSTIC_PROVIDER_FAILED, REVIEW_DIAGNOSTIC_PROVIDER_REQUIRED,
    REVIEW_DIAGNOSTIC_UNRESOLVED_REVISION, REVIEW_DIAGNOSTIC_UNSUPPORTED_SOURCE,
    REVIEW_ERROR_PATH_REJECTED, REVIEW_REPO_STATE_MAX_AHEAD, ResolveReviewSourcesResponse,
    ResolvedReviewStack, ResolvedSource, ResolvedSourceMode, ResolvedSourcePart,
    ResolvedStackLayer, ReviewChangeCount, ReviewLineInterval, ReviewPullFetchResult,
    ReviewRepoStateResponse, ReviewSource, ReviewSourceDiagnostic, ReviewSourceDiagnosticSeverity,
    ReviewSourceKind, ReviewSourceProviderOutcome, ReviewSourceProviderResolution,
    ReviewSourcesTokenResponse, ReviewStack, RevisionRef, source_kind_requires_provider,
};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

/// Default-branch candidates tried when no upstream or `origin/HEAD` exists.
const DEFAULT_BRANCH_CANDIDATES: [&str; 3] = ["main", "master", "trunk"];

/// Outcome of resolving one source.
enum Resolution {
    Resolved(Vec<ResolvedSource>),
    /// A stack or stack-layer source: its revision pair plus its pinned layer structure.
    Stack(Box<(ResolvedSource, ResolvedReviewStack)>),
    Diagnostic {
        severity: ReviewSourceDiagnosticSeverity,
        code: &'static str,
        message: String,
    },
}

fn unresolved(message: impl Into<String>) -> Resolution {
    Resolution::Diagnostic {
        severity: ReviewSourceDiagnosticSeverity::Error,
        code: REVIEW_DIAGNOSTIC_UNRESOLVED_REVISION,
        message: message.into(),
    }
}

/// Resolve the included sources in workspace order. Provider-backed sources use the matching
/// entry of `provider_resolutions` (by source id).
pub fn resolve_sources(
    root: &Path,
    sources: &[ReviewSource],
    provider_resolutions: &[ReviewSourceProviderResolution],
    merge_bases: &MergeBases,
) -> Result<ResolveReviewSourcesResponse, DataError> {
    // Only local sources are relative to HEAD; a review of pull requests never reads it.
    let head = if sources.iter().any(|source| {
        source.included && !bcode_code_review_models::source_kind_requires_provider(&source.kind)
    }) {
        git::resolve_commit(root, "HEAD")?
    } else {
        None
    };
    let mut memo = GitMemo::new(merge_bases);
    memo.prime(root, provider_resolutions);
    let mut response = ResolveReviewSourcesResponse {
        sources: Vec::new(),
        diagnostics: Vec::new(),
        stacks: Vec::new(),
    };
    for source in sources.iter().filter(|source| source.included) {
        let provided = provider_resolutions
            .iter()
            .find(|resolution| resolution.source_id == source.id);
        let resolution = match resolve_source(root, head.as_deref(), source, provided, &mut memo) {
            Ok(resolution) => resolution,
            // Invalid user-provided revisions and paths are a per-source problem, not a request
            // failure: a failing source never fails the others.
            Err(
                DataError::InvalidRequest(message)
                | DataError::Git(message)
                | DataError::NotFound(message)
                | DataError::TooLarge(message),
            ) => unresolved(message),
            Err(error @ DataError::PathRejected(_)) => Resolution::Diagnostic {
                severity: ReviewSourceDiagnosticSeverity::Error,
                code: REVIEW_ERROR_PATH_REJECTED,
                message: format!("{error}; edit the source to use a repository-relative path"),
            },
            Err(error @ (DataError::CursorStale | DataError::Io(_))) => return Err(error),
        };
        match resolution {
            Resolution::Resolved(resolved) => response.sources.extend(resolved),
            Resolution::Stack(resolved) => {
                let (resolved, stack) = *resolved;
                response.sources.push(resolved);
                response.stacks.push(stack);
            }
            Resolution::Diagnostic {
                severity,
                code,
                message,
            } => response.diagnostics.push(ReviewSourceDiagnostic {
                source_id: source.id.clone(),
                severity,
                code: code.to_string(),
                message,
            }),
        }
    }
    Ok(response)
}

fn diff(source: &ReviewSource, base: RevisionRef, head: RevisionRef) -> Resolution {
    Resolution::Resolved(vec![diff_source(source, base, head)])
}

fn diff_source(source: &ReviewSource, base: RevisionRef, head: RevisionRef) -> ResolvedSource {
    ResolvedSource {
        source_id: source.id.clone(),
        base,
        head,
        mode: ResolvedSourceMode::Diff,
        paths: Vec::new(),
        line_range: None,
        part: None,
    }
}

fn browse(
    source: &ReviewSource,
    paths: Vec<String>,
    line_range: Option<ReviewLineInterval>,
) -> Resolution {
    Resolution::Resolved(vec![ResolvedSource {
        source_id: source.id.clone(),
        base: RevisionRef::WorkingTree,
        head: RevisionRef::WorkingTree,
        mode: ResolvedSourceMode::Browse,
        paths,
        line_range,
        part: None,
    }])
}

fn head_or_empty(head: Option<&str>) -> RevisionRef {
    head.map_or(RevisionRef::EmptyTree, RevisionRef::commit)
}

fn resolve_source(
    root: &Path,
    head: Option<&str>,
    source: &ReviewSource,
    provided: Option<&ReviewSourceProviderResolution>,
    memo: &mut GitMemo<'_>,
) -> Result<Resolution, DataError> {
    Ok(match &source.kind {
        ReviewSourceKind::WorkingTreeUnstaged => {
            diff(source, RevisionRef::Index, RevisionRef::WorkingTree)
        }
        ReviewSourceKind::IndexStaged => diff(source, head_or_empty(head), RevisionRef::Index),
        ReviewSourceKind::WorkingTreeAndIndex => {
            diff(source, head_or_empty(head), RevisionRef::WorkingTree)
        }
        ReviewSourceKind::LastCommit => match head {
            Some(head) => single_commit(root, source, head)?,
            None => unresolved("the repository has no commits yet"),
        },
        ReviewSourceKind::Commit { rev } => match git::resolve_commit(root, rev)? {
            Some(oid) => single_commit(root, source, &oid)?,
            None => unresolved(format!("revision {rev} does not exist locally")),
        },
        ReviewSourceKind::CommitRange {
            base,
            head: head_rev,
            merge_base,
        } => range(root, source, base, head_rev, *merge_base)?,
        ReviewSourceKind::BranchCompare {
            base_branch,
            head_branch,
            merge_base,
        } => range(root, source, base_branch, head_branch, *merge_base)?,
        ReviewSourceKind::File { path } => {
            git::validate_relative_path(path)?;
            browse(source, vec![path.clone()], None)
        }
        ReviewSourceKind::FileRange { path, start, end } => {
            git::validate_relative_path(path)?;
            browse(
                source,
                vec![path.clone()],
                Some(ReviewLineInterval {
                    start: *start.min(end),
                    end: *start.max(end),
                }),
            )
        }
        ReviewSourceKind::Repository => browse(source, Vec::new(), None),
        ReviewSourceKind::Commits { revs } => commits(root, source, revs)?,
        ReviewSourceKind::CurrentBranch { base } => {
            current_branch(root, source, head, base.as_deref())?
        }
        kind @ (ReviewSourceKind::PullRequest { .. }
        | ReviewSourceKind::StackLayer { .. }
        | ReviewSourceKind::Stack { .. }) => {
            debug_assert!(source_kind_requires_provider(kind));
            match provided.map(|resolution| &resolution.outcome) {
                None => Resolution::Diagnostic {
                    severity: ReviewSourceDiagnosticSeverity::Error,
                    code: REVIEW_DIAGNOSTIC_PROVIDER_REQUIRED,
                    message: format!(
                        "{} needs a review-source provider plugin to resolve its revisions",
                        kind.label()
                    ),
                },
                Some(ReviewSourceProviderOutcome::Failed { code, message }) => {
                    Resolution::Diagnostic {
                        severity: ReviewSourceDiagnosticSeverity::Error,
                        code: REVIEW_DIAGNOSTIC_PROVIDER_FAILED,
                        message: format!("{}: provider error {code}: {message}", kind.label()),
                    }
                }
                Some(ReviewSourceProviderOutcome::Resolved { stack, fetched }) => {
                    provider_source(root, source, stack.as_ref(), fetched, memo)?
                }
            }
        }
        ReviewSourceKind::Unsupported { kind, .. } => Resolution::Diagnostic {
            severity: ReviewSourceDiagnosticSeverity::Warning,
            code: REVIEW_DIAGNOSTIC_UNSUPPORTED_SOURCE,
            message: format!(
                "source kind {kind:?} is not supported by this version; it is preserved unchanged"
            ),
        },
    })
}

/// Resolve a provider-backed source from the provider's stack and fetch results.
///
/// * `PullRequest`: merge base of the base branch and the head → head (the pull request's diff).
/// * `StackLayer`: merge base of the layer below's head (the trunk for the bottom layer) and the
///   layer's head → the layer's head; exactly the layer's own changes.
/// * `Stack`: merge base of the trunk and the top layer → the top layer's head.
fn provider_source(
    root: &Path,
    source: &ReviewSource,
    stack: Option<&ReviewStack>,
    fetched: &[ReviewPullFetchResult],
    memo: &mut GitMemo<'_>,
) -> Result<Resolution, DataError> {
    match &source.kind {
        ReviewSourceKind::PullRequest { number, .. } => {
            pull_request_source(root, source, *number, fetched, memo)
        }
        ReviewSourceKind::StackLayer { number, .. } => {
            stack_source(root, source, stack, fetched, *number, false, memo)
        }
        ReviewSourceKind::Stack { top, .. } => {
            stack_source(root, source, stack, fetched, *top, true, memo)
        }
        _ => Ok(unresolved("not a provider-backed source")),
    }
}

fn pull_request_source(
    root: &Path,
    source: &ReviewSource,
    number: u64,
    fetched: &[ReviewPullFetchResult],
    memo: &mut GitMemo<'_>,
) -> Result<Resolution, DataError> {
    let Some(fetch) = fetched_pull(fetched, number) else {
        return Ok(unresolved(format!(
            "the provider did not fetch pull request #{number}"
        )));
    };
    let (Some(base), Some(head)) = (
        memo.local_commit(root, &fetch.base_oid)?,
        memo.local_commit(root, &fetch.head_oid)?,
    ) else {
        return Ok(unresolved(format!(
            "the commits of pull request #{number} are not in the local repository"
        )));
    };
    let Some(base) = memo.merge_base(root, &base, &head)? else {
        return Ok(unresolved(format!(
            "pull request #{number} shares no history with {}",
            fetch.base_branch
        )));
    };
    Ok(diff(
        source,
        RevisionRef::commit(base),
        RevisionRef::commit(head),
    ))
}

/// A stack layer (`whole_stack == false`) or a whole stack up to layer `number`.
fn stack_source(
    root: &Path,
    source: &ReviewSource,
    stack: Option<&ReviewStack>,
    fetched: &[ReviewPullFetchResult],
    number: u64,
    whole_stack: bool,
    memo: &mut GitMemo<'_>,
) -> Result<Resolution, DataError> {
    let Some(stack) = stack else {
        return Ok(unresolved(format!(
            "the provider did not resolve the stack of pull request #{number}"
        )));
    };
    let Some(layers) = stack.layers_through(number) else {
        return Ok(unresolved(format!(
            "pull request #{number} is not a layer of its resolved stack"
        )));
    };
    let Some(bottom) = fetched_pull(fetched, layers[0].pull.number) else {
        return Ok(unresolved(format!(
            "the provider did not fetch pull request #{}",
            layers[0].pull.number
        )));
    };
    let Some(trunk_oid) = memo.local_commit(root, &bottom.base_oid)? else {
        return Ok(unresolved(format!(
            "the trunk commit of {} is not in the local repository",
            stack.trunk
        )));
    };
    let mut resolved_layers = Vec::with_capacity(layers.len());
    let mut below = trunk_oid.clone();
    for layer in layers {
        let layer_number = layer.pull.number;
        let Some(head) = fetched_pull(fetched, layer_number)
            .map(|fetch| memo.local_commit(root, &fetch.head_oid))
            .transpose()?
            .flatten()
        else {
            return Ok(unresolved(format!(
                "the head of stack layer #{layer_number} is not in the local repository"
            )));
        };
        let Some(base) = memo.merge_base(root, &below, &head)? else {
            return Ok(unresolved(format!(
                "stack layer #{layer_number} shares no history with the layer below it"
            )));
        };
        resolved_layers.push(ResolvedStackLayer {
            number: layer_number,
            title: layer.pull.title.clone(),
            base_oid: base,
            head_oid: head.clone(),
            base_branch: layer.pull.base_branch.clone(),
            head_branch: layer.pull.head_branch.clone(),
        });
        below = head;
    }
    let Some(stack_base) = memo.merge_base(root, &trunk_oid, &below)? else {
        return Ok(unresolved(format!(
            "the stack shares no history with {}",
            stack.trunk
        )));
    };
    let Some(layer) = resolved_layers.last() else {
        return Ok(unresolved("the resolved stack has no layers"));
    };
    let base = if whole_stack {
        stack_base.clone()
    } else {
        layer.base_oid.clone()
    };
    let resolved = diff_source(
        source,
        RevisionRef::commit(base),
        RevisionRef::commit(layer.head_oid.clone()),
    );
    Ok(Resolution::Stack(Box::new((
        resolved,
        ResolvedReviewStack {
            source_id: source.id.clone(),
            trunk: stack.trunk.clone(),
            trunk_oid,
            base_oid: stack_base,
            layers: resolved_layers,
        },
    ))))
}

fn fetched_pull(fetched: &[ReviewPullFetchResult], number: u64) -> Option<&ReviewPullFetchResult> {
    fetched.iter().find(|fetch| fetch.number == number)
}

/// Most merge bases [`MergeBases`] keeps before it starts over.
const MAX_MERGE_BASES: usize = 50_000;

/// Merge bases between commits, shared by every request: a merge base of two commit ids never
/// changes, so it is computed once, kept in memory (bounded by [`MAX_MERGE_BASES`]) and on disk.
#[derive(Debug, Default)]
pub struct MergeBases {
    memory: std::sync::Mutex<BTreeMap<(String, String), MergeBase>>,
    disk: Arc<ReviewCache>,
}

/// A known merge base of two commits.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MergeBase {
    /// Their merge base.
    Found(String),
    /// They share no history.
    Unrelated,
}

impl MergeBase {
    fn of(base: Option<String>) -> Self {
        base.map_or(Self::Unrelated, Self::Found)
    }

    fn into_option(self) -> Option<String> {
        match self {
            Self::Found(base) => Some(base),
            Self::Unrelated => None,
        }
    }
}

impl MergeBases {
    /// Merge bases backed by `disk`.
    pub fn with_disk(disk: Arc<ReviewCache>) -> Self {
        Self {
            memory: std::sync::Mutex::default(),
            disk,
        }
    }

    fn get(&self, key: &(String, String)) -> Option<MergeBase> {
        self.lock().get(key).cloned()
    }

    /// Load the merge bases of `pairs` the disk holds into memory, in one read.
    fn load(&self, pairs: &[(String, String)]) {
        if !self.disk.enabled() || pairs.is_empty() {
            return;
        }
        let keys: Vec<String> = pairs.iter().map(disk_key).collect();
        let found = self.disk.get_many(CacheKind::MergeBase, &keys);
        let mut memory = self.lock();
        for (pair, key) in pairs.iter().zip(&keys) {
            if let Some(entry) = found.get(key) {
                memory.insert(pair.clone(), MergeBase::Found(entry.value.clone()));
            }
        }
    }

    /// Keep `answers` in memory, and the found ones on disk. That two commits share no history
    /// stays in memory only: a shallow clone may simply lack it.
    fn insert_all(&self, answers: impl IntoIterator<Item = ((String, String), Option<String>)>) {
        let mut stored = Vec::new();
        {
            let mut memory = self.lock();
            for (key, base) in answers {
                if memory.len() >= MAX_MERGE_BASES {
                    memory.clear();
                }
                if let Some(base) = &base {
                    stored.push((disk_key(&key), base.clone()));
                }
                memory.insert(key, MergeBase::of(base));
            }
        }
        self.disk.put_many(CacheKind::MergeBase, &stored);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<(String, String), MergeBase>> {
        self.memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The on-disk key of a pair of commits.
fn disk_key((left, right): &(String, String)) -> String {
    format!("{left}\0{right}")
}

/// Git answers one resolve request reuses: commits checked once per request, merge bases from
/// the shared [`MergeBases`]. The sources of one stack (one per layer when stepping through it)
/// all walk the same layers, so without this a stack of `n` layers cost about `n²` git
/// processes.
#[derive(Debug)]
struct GitMemo<'a> {
    commits: BTreeMap<String, Option<String>>,
    merge_bases: &'a MergeBases,
}

/// Merge bases computed at once while priming a [`GitMemo`].
const PRIME_WORKERS: usize = 8;

impl<'a> GitMemo<'a> {
    const fn new(merge_bases: &'a MergeBases) -> Self {
        Self {
            commits: BTreeMap::new(),
            merge_bases,
        }
    }

    /// Answer up front what the provider-backed sources will ask: whether every fetched commit
    /// exists (one `git cat-file --batch-check`), and each layer's merge bases with the layer
    /// below and with the trunk (computed in parallel). Anything left out is asked on demand,
    /// and a failure here only leaves it to be asked again.
    fn prime(&mut self, root: &Path, resolutions: &[ReviewSourceProviderResolution]) {
        let mut oids: BTreeSet<&str> = BTreeSet::new();
        let mut pairs: BTreeSet<(String, String)> = BTreeSet::new();
        let mut trunks: BTreeSet<String> = BTreeSet::new();
        for resolution in resolutions {
            let ReviewSourceProviderOutcome::Resolved { stack, fetched } = &resolution.outcome
            else {
                continue;
            };
            oids.extend(
                fetched
                    .iter()
                    .flat_map(|fetch| [fetch.head_oid.as_str(), fetch.base_oid.as_str()]),
            );
            let Some(stack) = stack else {
                continue;
            };
            let heads: Vec<&str> = stack
                .layers
                .iter()
                .filter_map(|layer| fetched_pull(fetched, layer.pull.number))
                .map(|fetch| fetch.head_oid.as_str())
                .collect();
            let Some(trunk) = stack
                .layers
                .first()
                .and_then(|bottom| fetched_pull(fetched, bottom.pull.number))
                .map(|fetch| fetch.base_oid.as_str())
            else {
                continue;
            };
            trunks.insert(trunk.to_owned());
            let mut below = trunk;
            for head in heads {
                pairs.insert((below.to_owned(), head.to_owned()));
                pairs.insert((trunk.to_owned(), head.to_owned()));
                below = head;
            }
        }
        let pairs: Vec<(String, String)> = pairs
            .into_iter()
            .filter(|pair| self.merge_bases.get(pair).is_none())
            .collect();
        // What an earlier process computed is read back in one go; what one walk of the stacks'
        // history answers needs no more git; only the rest runs `git merge-base` per pair.
        self.merge_bases.load(&pairs);
        let pairs: Vec<(String, String)> = pairs
            .into_iter()
            .filter(|pair| self.merge_bases.get(pair).is_none())
            .collect();
        let ancestry = merge_bases_by_ancestry(root, &trunks, &pairs);
        self.merge_bases.insert_all(ancestry.answers);
        // The walk proved the commits it named exist; only the others are checked.
        for oid in ancestry.existing {
            self.commits.insert(oid.clone(), Some(oid));
        }
        let oids: Vec<&str> = oids
            .into_iter()
            .filter(|oid| git::validate_oid(oid).is_ok() && !self.commits.contains_key(*oid))
            .collect();
        if !oids.is_empty() {
            let input = oids.join("\n") + "\n";
            if let Ok(output) = git::output_with_input(
                root,
                &["cat-file", "--batch-check=%(objectname) %(objecttype)"],
                &input,
            ) {
                let output = String::from_utf8_lossy(&output);
                let commits: BTreeSet<&str> = output
                    .lines()
                    .filter_map(|line| line.strip_suffix(" commit"))
                    .collect();
                for oid in oids {
                    let found = commits.contains(oid).then(|| oid.to_owned());
                    self.commits.insert(oid.to_owned(), found);
                }
            }
        }

        let pairs: Vec<(String, String)> = pairs
            .into_iter()
            .filter(|pair| self.merge_bases.get(pair).is_none())
            .collect();
        let chunk = pairs.len().div_ceil(PRIME_WORKERS).max(1);
        let answers: Vec<((String, String), Option<String>)> = std::thread::scope(|scope| {
            // Every worker starts before any is joined, so they run at once.
            let mut workers = Vec::new();
            for pairs in pairs.chunks(chunk) {
                workers.push(scope.spawn(move || {
                    pairs
                        .iter()
                        .filter_map(|(left, right)| {
                            git::merge_base(root, left, right)
                                .ok()
                                .map(|base| ((left.clone(), right.clone()), base))
                        })
                        .collect::<Vec<_>>()
                }));
            }
            workers
                .into_iter()
                .filter_map(|worker| worker.join().ok())
                .flatten()
                .collect()
        });
        self.merge_bases.insert_all(answers);
    }

    /// [`local_commit`], once per commit.
    fn local_commit(&mut self, root: &Path, oid: &str) -> Result<Option<String>, DataError> {
        if let Some(known) = self.commits.get(oid) {
            return Ok(known.clone());
        }
        let found = local_commit(root, oid)?;
        self.commits.insert(oid.to_owned(), found.clone());
        Ok(found)
    }

    /// [`git::merge_base`], once per pair.
    fn merge_base(
        &self,
        root: &Path,
        left: &str,
        right: &str,
    ) -> Result<Option<String>, DataError> {
        let key = (left.to_owned(), right.to_owned());
        if self.merge_bases.get(&key).is_none() {
            self.merge_bases.load(std::slice::from_ref(&key));
        }
        if let Some(known) = self.merge_bases.get(&key) {
            return Ok(known.into_option());
        }
        let found = git::merge_base(root, left, right)?;
        self.merge_bases.insert_all([(key, found.clone())]);
        Ok(found)
    }
}

/// Most commits [`merge_bases_by_ancestry`] reads before leaving every pair to `git merge-base`.
const ANCESTRY_MAX_COMMITS: usize = 50_000;

/// The merge bases of `pairs` that one walk of their history settles (and the commits it proved
/// exist), read with a single
/// `git rev-list --parents --boundary` of the right-hand commits, excluding the `trunks`:
///
/// * `(left, right)` with `left` an ancestor of `right` (a layer on the layer below it, as stacks
///   are built): `left`.
/// * `(trunk, right)` with one trunk, when the history of `right` leaves the trunk's at one
///   commit: that commit. Several (a merge from the trunk, say) are left to `git merge-base`.
///
/// Pairs it does not settle are left out, as is everything when the walk fails or would read
/// more than [`ANCESTRY_MAX_COMMITS`] commits (an unrelated history, say).
fn merge_bases_by_ancestry(
    root: &Path,
    trunks: &BTreeSet<String>,
    pairs: &[(String, String)],
) -> Ancestry {
    if pairs.is_empty() || trunks.is_empty() {
        return Ancestry::default();
    }
    let rights: BTreeSet<&str> = pairs.iter().map(|(_, right)| right.as_str()).collect();
    let limit = (ANCESTRY_MAX_COMMITS + 1).to_string();
    let mut args = vec!["rev-list", "--parents", "--boundary", "--max-count", &limit];
    let excluded: Vec<String> = trunks.iter().map(|trunk| format!("^{trunk}")).collect();
    args.extend(rights.iter().copied());
    args.extend(excluded.iter().map(String::as_str));
    args.push("--");
    let Ok(output) = git::output(root, &args) else {
        return Ancestry::default();
    };
    // The walk succeeded, so every commit it was given exists.
    let existing: Vec<String> = rights
        .iter()
        .map(|right| (*right).to_owned())
        .chain(trunks.iter().cloned())
        .collect();
    let output = String::from_utf8_lossy(&output);
    // Commits between the trunks and the right-hand commits, with their parents; boundary
    // commits (marked `-`) are where that history meets the trunks'.
    let mut parents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut boundary: BTreeSet<&str> = BTreeSet::new();
    for line in output.lines() {
        if let Some(commit) = line.strip_prefix('-') {
            boundary.insert(commit.split(' ').next().unwrap_or_default());
            continue;
        }
        let mut ids = line.split(' ');
        if let Some(commit) = ids.next() {
            parents.insert(commit, ids.collect());
        }
    }
    if parents.len() > ANCESTRY_MAX_COMMITS {
        return Ancestry {
            answers: Vec::new(),
            existing,
        };
    }
    // Every commit reachable from `start` without crossing the boundary, and the boundary
    // commits it reaches.
    let reach = |start: &str| -> (BTreeSet<&str>, BTreeSet<&str>) {
        let mut seen = BTreeSet::new();
        let mut met = BTreeSet::new();
        let mut queue = vec![start];
        while let Some(commit) = queue.pop() {
            if let Some((commit, next)) = parents.get_key_value(commit) {
                if seen.insert(*commit) {
                    queue.extend(next.iter().copied());
                }
            } else if let Some(commit) = boundary.get(commit) {
                met.insert(*commit);
            }
        }
        (seen, met)
    };
    let mut reached: BTreeMap<&str, (BTreeSet<&str>, BTreeSet<&str>)> = BTreeMap::new();
    let mut answers = Vec::new();
    for (left, right) in pairs {
        let (history, met) = reached
            .entry(right.as_str())
            .or_insert_with(|| reach(right));
        if left == right || history.contains(left.as_str()) {
            answers.push(((left.clone(), right.clone()), Some(left.clone())));
        } else if trunks.len() == 1
            && trunks.contains(left)
            && met.len() == 1
            && let Some(meets) = met.iter().next()
        {
            answers.push(((left.clone(), right.clone()), Some((*meets).to_owned())));
        }
    }
    Ancestry { answers, existing }
}

/// What [`merge_bases_by_ancestry`] settled, and the commits its walk proved exist.
#[derive(Debug, Default)]
struct Ancestry {
    answers: Vec<((String, String), Option<String>)>,
    existing: Vec<String>,
}

/// The full id of commit `oid` when it exists locally.
///
/// # Errors
///
/// * [`DataError::InvalidRequest`] when `oid` is not a hexadecimal object id.
fn local_commit(root: &Path, oid: &str) -> Result<Option<String>, DataError> {
    git::validate_oid(oid)?;
    git::resolve_commit(root, oid)
}

/// A commit against its first parent (the empty tree for a root commit).
fn single_commit(root: &Path, source: &ReviewSource, oid: &str) -> Result<Resolution, DataError> {
    let parent = git::resolve_commit(root, &format!("{oid}^1"))?;
    Ok(diff(
        source,
        head_or_empty(parent.as_deref()),
        RevisionRef::commit(oid),
    ))
}

fn range(
    root: &Path,
    source: &ReviewSource,
    base: &str,
    head: &str,
    merge_base: bool,
) -> Result<Resolution, DataError> {
    let Some(base_oid) = git::resolve_commit(root, base)? else {
        return Ok(unresolved(format!(
            "base revision {base} does not exist locally"
        )));
    };
    let Some(head_oid) = git::resolve_commit(root, head)? else {
        return Ok(unresolved(format!(
            "head revision {head} does not exist locally"
        )));
    };
    let base_oid = if merge_base {
        match git::merge_base(root, &base_oid, &head_oid)? {
            Some(oid) => oid,
            None => {
                return Ok(unresolved(format!(
                    "{base} and {head} share no history, so no merge base exists"
                )));
            }
        }
    } else {
        base_oid
    };
    Ok(diff(
        source,
        RevisionRef::commit(base_oid),
        RevisionRef::commit(head_oid),
    ))
}

struct CommitInfo {
    oid: String,
    short: String,
    parent: Option<String>,
    subject: String,
}

/// Resolve a commit selection. A selection that forms one first-parent chain resolves to a single
/// range; otherwise each commit becomes one part against its first parent, in selection order.
fn commits(root: &Path, source: &ReviewSource, revs: &[String]) -> Result<Resolution, DataError> {
    let mut oids = Vec::new();
    let mut seen = BTreeSet::new();
    for rev in revs {
        let Some(oid) = git::resolve_commit(root, rev)? else {
            return Ok(unresolved(format!("revision {rev} does not exist locally")));
        };
        if seen.insert(oid.clone()) {
            oids.push(oid);
        }
    }
    if oids.is_empty() {
        return Ok(unresolved("no commits selected"));
    }
    let mut args = vec![
        "log",
        "--no-walk=unsorted",
        "-z",
        "--format=%H%x1f%h%x1f%P%x1f%s",
        "--end-of-options",
    ];
    args.extend(oids.iter().map(String::as_str));
    let output = git::output(root, &args)?;
    let infos: BTreeMap<String, CommitInfo> = git::split_nul(&output)
        .into_iter()
        .filter_map(|record| {
            let mut fields = record.trim_start_matches('\n').split('\x1f');
            let oid = fields.next()?.to_string();
            let short = fields.next()?.to_string();
            let parent = fields.next()?.split_whitespace().next().map(str::to_string);
            let subject = fields.next().unwrap_or_default().to_string();
            Some((
                oid.clone(),
                CommitInfo {
                    oid,
                    short,
                    parent,
                    subject,
                },
            ))
        })
        .collect();

    if let Some((oldest, newest)) = first_parent_chain(&oids, &infos) {
        return Ok(diff(
            source,
            head_or_empty(infos[&oldest].parent.as_deref()),
            RevisionRef::commit(newest),
        ));
    }

    let count = u32::try_from(oids.len()).unwrap_or(u32::MAX);
    let parts = oids
        .iter()
        .enumerate()
        .filter_map(|(index, oid)| {
            let info = infos.get(oid)?;
            Some(ResolvedSource {
                source_id: source.id.clone(),
                base: head_or_empty(info.parent.as_deref()),
                head: RevisionRef::commit(info.oid.clone()),
                mode: ResolvedSourceMode::Diff,
                paths: Vec::new(),
                line_range: None,
                part: Some(ResolvedSourcePart {
                    index: u32::try_from(index).unwrap_or(u32::MAX),
                    count,
                    label: format!("{} {}", info.short, info.subject),
                }),
            })
        })
        .collect();
    Ok(Resolution::Resolved(parts))
}

/// When the commits form one first-parent chain, return its `(oldest, newest)` ends.
fn first_parent_chain(
    oids: &[String],
    infos: &BTreeMap<String, CommitInfo>,
) -> Option<(String, String)> {
    let selected: BTreeSet<&str> = oids.iter().map(String::as_str).collect();
    let child_of: BTreeMap<&str, &str> = infos
        .values()
        .filter_map(|info| {
            let parent = info.parent.as_deref()?;
            selected
                .contains(parent)
                .then_some((parent, info.oid.as_str()))
        })
        .collect();
    let oldest = oids.iter().find(|oid| {
        infos
            .get(oid.as_str())
            .is_some_and(|info| !info.parent.as_deref().is_some_and(|p| selected.contains(p)))
    })?;
    let mut newest = oldest.as_str();
    let mut length = 1;
    while let Some(child) = child_of.get(newest) {
        newest = child;
        length += 1;
    }
    (length == oids.len()).then(|| (oldest.clone(), newest.to_string()))
}

fn current_branch(
    root: &Path,
    source: &ReviewSource,
    head: Option<&str>,
    base: Option<&str>,
) -> Result<Resolution, DataError> {
    let Some(head) = head else {
        return Ok(unresolved("the repository has no commits yet"));
    };
    let base_rev = match base {
        Some(base) => Some(base.to_string()),
        None => detect_base(root)?,
    };
    let Some(base_rev) = base_rev else {
        return Ok(unresolved(
            "no upstream or default branch found; choose a base branch",
        ));
    };
    range(root, source, &base_rev, head, true)
}

/// The current branch's upstream, else `origin/HEAD`, else the first existing default-branch
/// candidate other than the current branch.
fn detect_base(root: &Path) -> Result<Option<String>, DataError> {
    let trimmed = |output: Option<Vec<u8>>| {
        output
            .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
            .filter(|value| !value.is_empty())
    };
    let current = trimmed(
        git::optional_output(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .ok()
            .flatten(),
    );
    if let Some(branch) = &current {
        let reference = format!("refs/heads/{branch}");
        let upstream = trimmed(
            git::optional_output(
                root,
                &["for-each-ref", "--format=%(upstream:short)", &reference],
            )
            .ok()
            .flatten(),
        );
        if upstream.is_some() {
            return Ok(upstream);
        }
    }
    let origin_head = trimmed(
        git::optional_output(
            root,
            &[
                "symbolic-ref",
                "--quiet",
                "--short",
                "refs/remotes/origin/HEAD",
            ],
        )
        .ok()
        .flatten(),
    );
    if origin_head.is_some() {
        return Ok(origin_head);
    }
    for candidate in DEFAULT_BRANCH_CANDIDATES {
        if current.as_deref() == Some(candidate) {
            continue;
        }
        if git::resolve_commit(root, &format!("refs/heads/{candidate}"))?.is_some() {
            return Ok(Some(candidate.to_string()));
        }
    }
    Ok(None)
}

/// Repository state for review suggestions: the branch, the base `CurrentBranch` detects and how
/// far `HEAD` is ahead of it, and unstaged and staged change counts. Bounded: two
/// `git diff --shortstat` calls, the base detection, one `merge-base`, and one capped
/// `rev-list --count`.
pub fn repo_state(root: &Path) -> Result<ReviewRepoStateResponse, DataError> {
    let head_oid = git::resolve_commit(root, "HEAD")?;
    let branch = git::optional_output(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .flatten()
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
        .filter(|branch| !branch.is_empty());
    let unstaged = shortstat(root, &["diff", "--shortstat"]);
    let staged = shortstat(root, &["diff", "--cached", "--shortstat"]);
    let mut state = ReviewRepoStateResponse {
        branch,
        head_oid: head_oid.clone(),
        unstaged,
        staged,
        ..ReviewRepoStateResponse::default()
    };
    let Some(head) = head_oid else {
        return Ok(state);
    };
    state.base = detect_base(root)?;
    let Some(base) = state.base.as_deref() else {
        return Ok(state);
    };
    let Some(base_oid) = git::resolve_commit(root, base)? else {
        return Ok(state);
    };
    state.merge_base_oid = git::merge_base(root, &base_oid, &head)?;
    if let Some(merge_base) = &state.merge_base_oid {
        let range = format!("{merge_base}..{head}");
        let cap = format!("--max-count={REVIEW_REPO_STATE_MAX_AHEAD}");
        state.ahead = git::optional_output(root, &["rev-list", "--count", &cap, &range])?
            .and_then(|bytes| String::from_utf8_lossy(&bytes).trim().parse().ok());
    }
    Ok(state)
}

/// Files and lines of a `git diff --shortstat` call; nothing when git reports nothing or fails
/// (an unborn branch has no `HEAD` to compare the index with).
fn shortstat(root: &Path, args: &[&str]) -> ReviewChangeCount {
    let Ok(Some(output)) = git::optional_output(root, args) else {
        return ReviewChangeCount::default();
    };
    parse_shortstat(&String::from_utf8_lossy(&output))
}

/// Parse ` 3 files changed, 10 insertions(+), 2 deletions(-)`.
fn parse_shortstat(text: &str) -> ReviewChangeCount {
    let mut count = ReviewChangeCount::default();
    for part in text.trim().split(',') {
        let mut words = part.split_whitespace();
        let (Some(number), Some(label)) = (words.next(), words.next()) else {
            continue;
        };
        let Ok(number) = number.parse::<u64>() else {
            continue;
        };
        if label.starts_with("file") {
            count.files = u32::try_from(number).unwrap_or(u32::MAX);
        } else if label.starts_with("insertion") {
            count.additions = number;
        } else if label.starts_with("deletion") {
            count.deletions = number;
        }
    }
    count
}

/// Token over `HEAD`, the staged index content (`git ls-files --stage`), and (optionally) the
/// unstaged working-tree state: `git status --porcelain=v2` plus the size and modification time
/// of every path with unstaged changes (status lines alone do not change when an already
/// modified file is edited again).
///
/// Index stat data is deliberately excluded: git rewrites the index file whenever it refreshes
/// stat information (including racily-clean entries right after a write), which would make the
/// token change without any reviewable change.
pub fn sources_token(
    root: &Path,
    include_working_tree: bool,
) -> Result<ReviewSourcesTokenResponse, DataError> {
    let head_oid = git::resolve_commit(root, "HEAD")?;
    let mut hasher = Sha256::new();
    hasher.update(b"head\0");
    hasher.update(head_oid.as_deref().unwrap_or("unborn").as_bytes());

    if include_working_tree {
        let status = git::output(
            root,
            &[
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=no",
                "--ignore-submodules=none",
            ],
        )?;
        hasher.update(b"\0status\0");
        hasher.update(&status);
        for path in unstaged_status_paths(&status) {
            hasher.update(path.as_bytes());
            hasher.update(b"\0");
            hash_file_stat(&mut hasher, &root.join(&path));
        }
    }
    // The staged content (paths, modes, object ids, stages), not the index file's stat data:
    // git rewrites the index to refresh stat information, which must not change the token.
    let staged = git::output(root, &["ls-files", "--stage", "-z"])?;
    hasher.update(b"\0index\0");
    hasher.update(&staged);

    Ok(ReviewSourcesTokenResponse {
        token: hex::encode(hasher.finalize()),
        head_oid,
    })
}

fn hash_file_stat(hasher: &mut Sha256, path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            hasher.update(metadata.len().to_le_bytes());
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_nanos());
            hasher.update(modified.to_le_bytes());
        }
        Err(_) => hasher.update(b"missing"),
    }
}

/// Paths with unstaged changes from `git status --porcelain=v2 -z` output.
fn unstaged_status_paths(status: &[u8]) -> Vec<String> {
    let tokens = git::split_nul(status);
    let mut paths = Vec::new();
    let mut tokens = tokens.into_iter();
    while let Some(token) = tokens.next() {
        let (fields, has_original) = match token.as_bytes().first() {
            Some(b'1') => (8, false),
            Some(b'2') => (9, true),
            Some(b'u') => (10, false),
            _ => continue,
        };
        let mut parts = token.splitn(fields + 1, ' ');
        let worktree_changed = parts
            .nth(1)
            .is_some_and(|xy| xy.as_bytes().get(1).is_some_and(|y| *y != b'.'));
        if let Some(path) = parts.nth(fields - 2)
            && worktree_changed
        {
            paths.push(path.to_string());
        }
        if has_original {
            tokens.next();
        }
    }
    paths
}
