//! Review operations on reviews as a whole: listing, creating, renaming, archiving, restoring
//! and deleting saved reviews, and marking many files viewed at once.

use super::{AssistantError, OpenReview, ReviewTools};
use crate::viewed::{EntryIdentity, ViewedReset};
use bcode_code_review_models::{
    ArchiveReviewWorkspaceRequest, CreateReviewWorkspaceRequest, GetReviewWorkspaceRequest,
    LayerLook, ListLayerLooksRequest, ListReviewWorkspacesRequest, ResolvedSourceMode,
    ReviewIndexEntry, ReviewPatchIdsRequest, ReviewSourceKind, ReviewWorkspace,
    UpdateReviewWorkspaceRequest,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Index entries with the index of the resolved source they belong to.
type SourceEntries = Vec<(usize, ReviewIndexEntry)>;

/// Reviews listed at most.
pub const MAX_LISTED_REVIEWS: usize = 200;
/// Files one bulk viewed change reports by name at most (the counts are always complete).
pub const MAX_REPORTED_PATHS: usize = 200;

/// The stable name of a source kind.
#[must_use]
pub const fn source_kind_name(kind: &ReviewSourceKind) -> &'static str {
    match kind {
        ReviewSourceKind::WorkingTreeUnstaged => "unstaged",
        ReviewSourceKind::IndexStaged => "staged",
        ReviewSourceKind::WorkingTreeAndIndex => "working_tree",
        ReviewSourceKind::WorkingTreePaths { .. } => "working_tree_paths",
        ReviewSourceKind::LastCommit => "last_commit",
        ReviewSourceKind::Commit { .. } => "commit",
        ReviewSourceKind::CommitRange { .. } => "range",
        ReviewSourceKind::BranchCompare { .. } => "branch",
        ReviewSourceKind::File { .. }
        | ReviewSourceKind::FileRange { .. }
        | ReviewSourceKind::FileSet { .. } => "files",
        ReviewSourceKind::Repository => "repository",
        ReviewSourceKind::Commits { .. } => "commits",
        ReviewSourceKind::PullRequest { .. } => "pull_request",
        ReviewSourceKind::StackLayer { .. } => "stack_layer",
        ReviewSourceKind::Stack { .. } => "stack",
        ReviewSourceKind::CurrentBranch { .. } => "current_branch",
        ReviewSourceKind::Unsupported { .. } => "unsupported",
    }
}

/// A review's kind from its sources.
///
/// `stack` (a stack held both as its layers and as one whole-stack diff), `stack_step_through`
/// (two or more stack layers), `whole_stack`, `pull_request`, a source kind name for one other
/// source, `mixed` or `empty`.
#[must_use]
pub fn review_kind(sources: &[bcode_code_review_models::ReviewSource]) -> String {
    let included: Vec<&ReviewSourceKind> = sources
        .iter()
        .filter(|source| source.included)
        .map(|source| &source.kind)
        .collect();
    let layers = included
        .iter()
        .filter(|kind| matches!(kind, ReviewSourceKind::StackLayer { .. }))
        .count();
    let whole = included
        .iter()
        .filter(|kind| matches!(kind, ReviewSourceKind::Stack { .. }))
        .count();
    if layers >= 1 && whole >= 1 && layers + whole == included.len() {
        return "stack".to_owned();
    }
    if layers >= 2 && layers == included.len() {
        return "stack_step_through".to_owned();
    }
    match included.as_slice() {
        [] => "empty".to_owned(),
        [only] => match only {
            ReviewSourceKind::Stack { .. } => "whole_stack",
            ReviewSourceKind::PullRequest { .. } | ReviewSourceKind::StackLayer { .. } => {
                "pull_request"
            }
            other => source_kind_name(other),
        }
        .to_owned(),
        _ => "mixed".to_owned(),
    }
}

/// The pull request numbers a review's sources name, in source order, each once (a stack's
/// whole-stack source names its top layer again).
#[must_use]
pub fn pull_request_numbers(sources: &[bcode_code_review_models::ReviewSource]) -> Vec<u64> {
    let mut numbers = Vec::new();
    for source in sources {
        if let ReviewSourceKind::PullRequest { number, .. }
        | ReviewSourceKind::StackLayer { number, .. }
        | ReviewSourceKind::Stack { top: number, .. } = &source.kind
            && !numbers.contains(number)
        {
            numbers.push(*number);
        }
    }
    numbers
}

/// A saved review as listed for agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewEntry {
    /// Review id (pass it as `review`).
    pub id: String,
    /// Title.
    pub title: String,
    /// Kind (see [`review_kind`]).
    pub kind: String,
    /// Source labels.
    pub sources: Vec<String>,
    /// Pull request numbers its sources name, in order (stack layers bottom first).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pull_requests: Vec<u64>,
    /// Repository root.
    pub repo_root: std::path::PathBuf,
    /// Files marked viewed.
    pub viewed_files: usize,
    /// Whether it is archived.
    pub archived: bool,
    /// Creation time, in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_ms: Option<u64>,
    /// Last update, in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at_ms: Option<u64>,
}

impl ReviewEntry {
    /// The entry of `workspace`.
    #[must_use]
    pub fn of(workspace: &ReviewWorkspace) -> Self {
        Self {
            id: workspace.id.clone(),
            title: workspace.title.clone(),
            kind: review_kind(&workspace.sources),
            sources: workspace
                .sources
                .iter()
                .filter(|source| source.included)
                .map(|source| source.label.clone())
                .collect(),
            pull_requests: pull_request_numbers(&workspace.sources),
            repo_root: workspace.repo_root.clone(),
            viewed_files: workspace.viewed_files.len(),
            archived: workspace.archived_at_ms.is_some(),
            created_at_ms: workspace.created_at_ms,
            updated_at_ms: workspace.updated_at_ms,
        }
    }
}

/// Saved reviews listed for agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewEntries {
    /// Reviews, most recently updated first.
    pub reviews: Vec<ReviewEntry>,
    /// Reviews that matched but were left out by the limit.
    pub omitted: usize,
}

/// A change to one saved review.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ReviewChange {
    /// New title.
    #[serde(default)]
    pub title: Option<String>,
    /// `true` archives, `false` restores.
    #[serde(default)]
    pub archived: Option<bool>,
}

/// Which files a bulk viewed change selects.
///
/// Selectors narrow each other: the files of the named layers (every layer when none), then the
/// named paths, the glob, or those unchanged since the reviewer last looked; `all` takes every
/// file of the selected layers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct FileSelection {
    /// Exact file paths.
    #[serde(default)]
    pub paths: Vec<String>,
    /// A glob over file paths (`src/**/*.rs`).
    #[serde(default)]
    pub glob: Option<String>,
    /// Stack layers or sources: pull request numbers (`"102"`, `"#102"`) or source ids.
    #[serde(default)]
    pub layers: Vec<String>,
    /// Every file of the selected layers.
    #[serde(default)]
    pub all: bool,
    /// Files whose change is the same as when the reviewer last looked at their layer.
    #[serde(default)]
    pub unchanged_since_look: bool,
}

impl FileSelection {
    const fn selects_something(&self) -> bool {
        !self.paths.is_empty()
            || self.glob.is_some()
            || !self.layers.is_empty()
            || self.all
            || self.unchanged_since_look
    }
}

/// What a bulk viewed change did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewedReport {
    /// What changed, in words.
    pub message: String,
    /// Files changed.
    pub changed: Vec<String>,
    /// Files selected that already had the requested state.
    pub unchanged: Vec<String>,
    /// Named paths that are not files of the review.
    pub not_found: Vec<String>,
    /// Files changed, in all.
    pub changed_count: usize,
    /// Files left out of `changed` and `unchanged` by the limit.
    #[serde(default)]
    pub omitted: usize,
}

impl ReviewTools<'_> {
    /// Saved reviews of the repository at `repo_path`, most recently updated first: archived ones
    /// too with `include_archived`, only those whose title, kind, source labels or pull request
    /// numbers contain `query` (case-insensitive) when given.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::Store`] when stored state cannot be read.
    pub fn review_entries(
        &self,
        repo_path: &Path,
        include_archived: bool,
        query: Option<&str>,
    ) -> Result<ReviewEntries, AssistantError> {
        let response = self.store.list_workspaces(&ListReviewWorkspacesRequest {
            repo_path: repo_path.to_path_buf(),
            include_archived,
        })?;
        let query = query
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .map(str::to_lowercase);
        let mut reviews: Vec<ReviewEntry> = response
            .workspaces
            .iter()
            .map(ReviewEntry::of)
            .filter(|entry| {
                query.as_ref().is_none_or(|query| {
                    entry.title.to_lowercase().contains(query)
                        || entry.kind.contains(query.as_str())
                        || entry
                            .sources
                            .iter()
                            .any(|label| label.to_lowercase().contains(query))
                        || entry
                            .pull_requests
                            .iter()
                            .any(|number| query.trim_start_matches('#') == number.to_string())
                })
            })
            .collect();
        reviews.sort_by(|a, b| {
            b.updated_at_ms
                .cmp(&a.updated_at_ms)
                .then_with(|| a.id.cmp(&b.id))
        });
        let omitted = reviews.len().saturating_sub(MAX_LISTED_REVIEWS);
        reviews.truncate(MAX_LISTED_REVIEWS);
        Ok(ReviewEntries { reviews, omitted })
    }

    /// Create a saved review of `sources` (each with an optional label) in the repository at
    /// `repo_path`, titled `title` (or after its sources).
    ///
    /// # Errors
    ///
    /// * [`AssistantError::Invalid`] without sources, or with a source of a kind this version
    ///   does not know.
    /// * [`AssistantError::Store`] when it cannot be saved.
    pub fn create_review(
        &self,
        repo_path: &Path,
        title: Option<String>,
        sources: Vec<(ReviewSourceKind, Option<String>)>,
    ) -> Result<ReviewEntry, AssistantError> {
        self.create_review_with(repo_path, title, sources, None)
    }

    /// [`Self::create_review`] with initial display settings, such as listing ignored files.
    ///
    /// # Errors
    ///
    /// As [`Self::create_review`].
    pub fn create_review_with(
        &self,
        repo_path: &Path,
        title: Option<String>,
        sources: Vec<(ReviewSourceKind, Option<String>)>,
        presentation: Option<bcode_code_review_models::ReviewPresentationState>,
    ) -> Result<ReviewEntry, AssistantError> {
        if sources.is_empty() {
            return Err(AssistantError::Invalid(
                "name at least one source to review".to_owned(),
            ));
        }
        if sources.iter().any(|(kind, _)| kind.is_unsupported()) {
            return Err(AssistantError::Invalid(
                "a source has a kind this version does not know".to_owned(),
            ));
        }
        let title = title
            .map(|title| title.trim().to_owned())
            .filter(|title| !title.is_empty());
        let labels: Vec<Option<String>> = sources.iter().map(|(_, label)| label.clone()).collect();
        let mut draft = crate::scope::transient_workspace(
            repo_path.to_path_buf(),
            None,
            sources.into_iter().map(|(kind, _)| kind).collect(),
        );
        for (source, label) in draft.sources.iter_mut().zip(labels) {
            if let Some(label) = label
                .map(|label| label.trim().to_owned())
                .filter(|label| !label.is_empty())
            {
                source.label = label;
            }
        }
        draft.title = title.unwrap_or_else(|| crate::scope::default_title(&draft.sources));
        let response = self.store.create_workspace(CreateReviewWorkspaceRequest {
            repo_path: repo_path.to_path_buf(),
            title: Some(draft.title),
            sources: draft.sources,
            presentation,
        })?;
        Ok(ReviewEntry::of(&response.workspace))
    }

    /// Rename, archive or restore the saved review `id`.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::NotFound`] when no saved review has that id.
    /// * [`AssistantError::Invalid`] for an empty title or a change that changes nothing.
    /// * [`AssistantError::Store`] when it cannot be saved.
    pub fn update_review(
        &self,
        repo_path: &Path,
        id: &str,
        change: &ReviewChange,
    ) -> Result<ReviewEntry, AssistantError> {
        let base = self.saved_or_missing(repo_path, id)?;
        let mut workspace = base.clone();
        if change.title.is_none() && change.archived.is_none() {
            return Err(AssistantError::Invalid(
                "pass `title`, `archived` or both".to_owned(),
            ));
        }
        if let Some(title) = &change.title {
            let title = title.trim();
            if title.is_empty() {
                return Err(AssistantError::Invalid("the title is empty".to_owned()));
            }
            title.clone_into(&mut workspace.title);
        }
        match change.archived {
            Some(true) if workspace.archived_at_ms.is_none() => {
                if change.title.is_some() {
                    self.save(repo_path, base, workspace)?;
                }
                self.store
                    .archive_workspace(ArchiveReviewWorkspaceRequest {
                        repo_path: repo_path.to_path_buf(),
                        workspace_id: id.to_owned(),
                    })?;
                return Ok(ReviewEntry::of(&self.saved_or_missing(repo_path, id)?));
            }
            Some(false) => workspace.archived_at_ms = None,
            _ => {}
        }
        Ok(ReviewEntry::of(&self.save(repo_path, base, workspace)?))
    }

    /// Delete the saved review `id`.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::NotFound`] when no saved review has that id.
    /// * [`AssistantError::Store`] when it cannot be deleted.
    pub fn delete_review(&self, repo_path: &Path, id: &str) -> Result<ReviewEntry, AssistantError> {
        let workspace = self.saved_or_missing(repo_path, id)?;
        self.store.delete_workspace(repo_path, id)?;
        Ok(ReviewEntry::of(&workspace))
    }

    fn saved_or_missing(
        &self,
        repo_path: &Path,
        id: &str,
    ) -> Result<ReviewWorkspace, AssistantError> {
        self.store
            .get_workspace(GetReviewWorkspaceRequest {
                repo_path: repo_path.to_path_buf(),
                workspace_id: id.to_owned(),
            })?
            .workspace
            .ok_or_else(|| {
                AssistantError::NotFound(format!(
                    "no saved review has id {id} (code_review.list_reviews lists them)"
                ))
            })
    }

    /// Write what `workspace` changed from `base`, keeping every change made elsewhere since.
    fn save(
        &self,
        repo_path: &Path,
        base: ReviewWorkspace,
        workspace: ReviewWorkspace,
    ) -> Result<ReviewWorkspace, AssistantError> {
        Ok(self
            .store
            .update_workspace(UpdateReviewWorkspaceRequest {
                repo_path: repo_path.to_path_buf(),
                workspace,
                base: Some(base),
            })?
            .workspace)
    }

    /// Mark the files `selection` selects viewed (or not viewed), at their current content.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::Invalid`] when the selection selects nothing, a glob is invalid, or a
    ///   layer is not one of the review's.
    /// * [`AssistantError::Store`] / [`AssistantError::Data`] when state or the repository
    ///   cannot be read or written.
    pub fn mark_viewed_selection(
        &mut self,
        review: &OpenReview,
        selection: &FileSelection,
        viewed: bool,
        reset: ViewedReset,
    ) -> Result<ViewedReport, AssistantError> {
        if !selection.selects_something() {
            return Err(AssistantError::Invalid(
                "select files with `paths`, `glob`, `layers`, `all` or `unchanged_since_look`"
                    .to_owned(),
            ));
        }
        let (mut selected, not_found) = self.select_files(review, selection)?;
        // The same path in several layers is one file to the reviewer's marks.
        selected.dedup_by(|a, b| a.1.path == b.1.path);
        let fingerprints = self.fingerprints_of(review, &selected)?;
        let base = self.current_viewed_record(review)?;
        let mut record = base.clone();
        let mut changed = Vec::new();
        let mut kept = Vec::new();
        for (index, entry) in &selected {
            let (patch, content) = fingerprints
                .get(&(*index, entry.path.clone()))
                .cloned()
                .unwrap_or_default();
            let identity = EntryIdentity::from_entry(entry).with_content(content.as_deref());
            let identity = identity.with_patch(patch.as_deref());
            let is_viewed = crate::viewed::viewed_state(&record, &identity, reset)
                != crate::viewed::ViewedState::Unviewed;
            if is_viewed == viewed {
                kept.push(entry.path.clone());
                continue;
            }
            if viewed {
                crate::viewed::mark_viewed(&mut record, &identity);
            } else {
                crate::viewed::unmark_viewed(&mut record, &identity);
            }
            changed.push(entry.path.clone());
        }
        if !changed.is_empty() {
            // Only these files' marks are written; marks the open review or another writer
            // changed meanwhile stay.
            self.store.update_workspace(UpdateReviewWorkspaceRequest {
                repo_path: review.repo_root.clone(),
                workspace: record,
                base: Some(base),
            })?;
        }
        let state = if viewed { "viewed" } else { "not viewed" };
        let changed_count = changed.len();
        let omitted = (changed.len() + kept.len()).saturating_sub(MAX_REPORTED_PATHS);
        changed.truncate(MAX_REPORTED_PATHS);
        kept.truncate(MAX_REPORTED_PATHS.saturating_sub(changed.len()));
        Ok(ViewedReport {
            message: format!(
                "marked {changed_count} file{} {state}; {} already {state}{}",
                if changed_count == 1 { "" } else { "s" },
                kept.len(),
                if not_found.is_empty() {
                    String::new()
                } else {
                    format!("; {} not in the review", not_found.len())
                }
            ),
            changed,
            unchanged: kept,
            not_found,
            changed_count,
            omitted,
        })
    }

    /// The files `selection` selects, and the named paths that are not files of the review.
    fn select_files(
        &self,
        review: &OpenReview,
        selection: &FileSelection,
    ) -> Result<(SourceEntries, Vec<String>), AssistantError> {
        let glob = selection
            .glob
            .as_deref()
            .map(|glob| {
                globset::GlobBuilder::new(glob)
                    .literal_separator(true)
                    .build()
                    .map(|glob| glob.compile_matcher())
                    .map_err(|error| AssistantError::Invalid(format!("invalid glob: {error}")))
            })
            .transpose()?;
        let layers = Self::selected_sources(review, &selection.layers)?;
        let entries: Vec<(usize, ReviewIndexEntry)> = self
            .entries(review)?
            .into_iter()
            .filter(|(index, _)| layers.as_ref().is_none_or(|layers| layers.contains(index)))
            .collect();
        let named: BTreeSet<String> = selection
            .paths
            .iter()
            .map(|path| path.trim().trim_start_matches("./").to_owned())
            .filter(|path| !path.is_empty())
            .collect();
        let mut not_found: Vec<String> = named
            .iter()
            .filter(|path| {
                !entries.iter().any(|(_, entry)| {
                    &entry.path == *path || entry.old_path.as_deref() == Some(path.as_str())
                })
            })
            .cloned()
            .collect();
        not_found.sort();
        let unchanged = if selection.unchanged_since_look {
            Some(self.unchanged_since_look(review, &entries)?)
        } else {
            None
        };
        let selected: Vec<(usize, ReviewIndexEntry)> = entries
            .into_iter()
            .filter(|(index, entry)| {
                let by_name = named.is_empty()
                    || named.contains(&entry.path)
                    || entry
                        .old_path
                        .as_ref()
                        .is_some_and(|old| named.contains(old));
                let by_glob = glob.as_ref().is_none_or(|glob| glob.is_match(&entry.path));
                let by_look = unchanged
                    .as_ref()
                    .is_none_or(|unchanged| unchanged.contains(&(*index, entry.path.clone())));
                by_name && by_glob && by_look
            })
            .collect();
        Ok((selected, not_found))
    }

    /// Indices of the resolved sources `layers` names (`None`: every source).
    fn selected_sources(
        review: &OpenReview,
        layers: &[String],
    ) -> Result<Option<BTreeSet<usize>>, AssistantError> {
        if layers.is_empty() {
            return Ok(None);
        }
        let mut selected = BTreeSet::new();
        for layer in layers {
            let wanted = layer.trim().trim_start_matches('#');
            let found: Vec<usize> = review
                .resolved
                .iter()
                .enumerate()
                .filter(|(_, resolved)| {
                    resolved.source_id == wanted
                        || review
                            .workspace
                            .sources
                            .iter()
                            .find(|source| source.id == resolved.source_id)
                            .is_some_and(|source| {
                                pull_request_numbers(std::slice::from_ref(source))
                                    .iter()
                                    .any(|number| number.to_string() == wanted)
                            })
                })
                .map(|(index, _)| index)
                .collect();
            if found.is_empty() {
                return Err(AssistantError::Invalid(format!(
                    "{layer} is not a layer or source of this review (code_review.summary lists them)"
                )));
            }
            selected.extend(found);
        }
        Ok(Some(selected))
    }

    /// The files of `entries` whose change is the same as when the reviewer last looked at
    /// their layer: pull request layers compare with the reviewer's latest look at the pull
    /// request, other sources with the revisions recorded when a review pass completed. A file
    /// without a look, or whose change differs, is left out.
    fn unchanged_since_look(
        &self,
        review: &OpenReview,
        entries: &[(usize, ReviewIndexEntry)],
    ) -> Result<BTreeSet<(usize, String)>, AssistantError> {
        let mut out = BTreeSet::new();
        let by_source: BTreeMap<usize, Vec<String>> =
            entries
                .iter()
                .fold(BTreeMap::new(), |mut map, (index, entry)| {
                    map.entry(*index)
                        .or_insert_with(Vec::new)
                        .push(entry.path.clone());
                    map
                });
        for (index, paths) in by_source {
            let resolved = &review.resolved[index];
            if resolved.mode == ResolvedSourceMode::Browse {
                continue;
            }
            let Some((then_base, then_head)) = self.looked_revisions(review, index)? else {
                continue;
            };
            for chunk in paths.chunks(bcode_code_review_models::REVIEW_PATCH_IDS_MAX_PATHS) {
                let now = self.data.patch_ids_get(&ReviewPatchIdsRequest {
                    repo_path: review.repo_root.clone(),
                    base: resolved.base.clone(),
                    head: resolved.head.clone(),
                    paths: chunk.to_vec(),
                })?;
                let then = self.data.patch_ids_get(&ReviewPatchIdsRequest {
                    repo_path: review.repo_root.clone(),
                    base: bcode_code_review_models::RevisionRef::commit(then_base.clone()),
                    head: bcode_code_review_models::RevisionRef::commit(then_head.clone()),
                    paths: chunk.to_vec(),
                })?;
                for path in chunk {
                    if let (Some(now), Some(then)) =
                        (now.patch_ids.get(path), then.patch_ids.get(path))
                        && now == then
                    {
                        out.insert((index, path.clone()));
                    }
                }
            }
        }
        Ok(out)
    }

    /// The base and head the reviewer last looked at for resolved source `index`.
    fn looked_revisions(
        &self,
        review: &OpenReview,
        index: usize,
    ) -> Result<Option<(String, String)>, AssistantError> {
        let resolved = &review.resolved[index];
        let source = review
            .workspace
            .sources
            .iter()
            .find(|source| source.id == resolved.source_id);
        let pull = source.and_then(|source| match &source.kind {
            ReviewSourceKind::PullRequest {
                provider,
                repository,
                number,
            }
            | ReviewSourceKind::StackLayer {
                provider,
                repository,
                number,
            } => Some((provider.clone(), repository.clone(), *number)),
            _ => None,
        });
        if let Some((provider, repository, number)) = pull {
            let looks = self.store.list_layer_looks(&ListLayerLooksRequest {
                repo_path: review.repo_root.clone(),
                provider,
                repository,
                numbers: vec![number],
            })?;
            let latest: Option<&LayerLook> = looks
                .looks
                .iter()
                .filter(|look| look.number == number)
                .max_by_key(|look| look.looked_at_ms);
            let base_now = resolved.base.commit_oid().map(str::to_owned);
            return Ok(latest.and_then(|look| {
                let base = look.fork_oid.clone().or_else(|| base_now.clone())?;
                Some((base, look.head_oid.clone()))
            }));
        }
        Ok(review
            .workspace
            .reviewed_revisions
            .get(&resolved.source_id)
            .and_then(|revision| {
                Some((
                    revision.reviewed_base_oid.clone()?,
                    revision.reviewed_head_oid.clone()?,
                ))
            }))
    }
}
