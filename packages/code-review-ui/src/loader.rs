//! Asynchronous work of the review surface: opening a review, serving document needs, reading
//! whole blobs for highlighting, change tokens, and saving presentation state.
//!
//! Every function here runs on a host-spawned task and reports through the [`Inbox`]; the
//! surface drains the inbox from `poll`, so frames never wait on a service.

use crate::services::ReviewServices;
use bcode_code_review::document::{DocumentNeed, SourceInput, SourceStack, Ticket};
use bcode_code_review::provider::resolve_request_with_providers;
use bcode_code_review_models::{
    GetReviewWorkspaceRequest, REVIEW_BLOB_LINES_MAX_COUNT, REVIEW_PATCH_IDS_MAX_PATHS,
    ResolveReviewSourcesResponse, ResolvedSource, ResolvedSourceMode, ReviewBlobLinesRequest,
    ReviewBlobLinesResponse, ReviewBlobSelector, ReviewFileHunksResponse, ReviewIndexFlag,
    ReviewIndexRequest, ReviewIndexResponse, ReviewPatchIdsRequest, ReviewPatchIdsResponse,
    ReviewSource, ReviewSourceDiagnostic, ReviewSourceKind, ReviewSourcesTokenRequest,
    ReviewSourcesTokenResponse, ReviewStackAttributionResponse, ReviewWorkspace, RevisionRef,
};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Results delivered from spawned work to the surface.
#[derive(Debug)]
pub enum Message {
    /// Index listing progress while opening.
    Progress {
        /// Index entries received so far.
        files: usize,
        /// Entries expected across every source, once known.
        total: usize,
    },
    /// The review opened.
    Opened(Box<Opened>),
    /// The review could not be opened.
    OpenFailed(String),
    /// Result of one document need.
    Need {
        /// Document generation the need was issued for.
        generation: u64,
        /// Ticket of the need.
        ticket: Ticket,
        /// Outcome.
        result: NeedResult,
    },
    /// Whole text of one blob side, for highlighting.
    BlobText {
        /// Cache key of the highlight to produce.
        key: super::syntax::SyntaxKey,
        /// Cancellation flag of the job.
        token: super::syntax::JobToken,
        /// Path used to pick the grammar.
        path: String,
        /// Text, or `None` when the blob is binary, too large, or unreadable.
        text: Option<String>,
    },
    /// Highlighted lines of one blob side: the lines done so far, or the finished highlight.
    Highlighted {
        /// Cache key of the highlight.
        key: super::syntax::SyntaxKey,
        /// Tokens, or `None` when highlighting was skipped.
        text: Option<Arc<bcode_syntax_render::HighlightedText>>,
        /// Whether `text` covers the whole blob.
        complete: bool,
        /// Cancellation flag of the job that produced it.
        token: super::syntax::JobToken,
    },
    /// A change token for refresh detection.
    Token {
        /// Token, or the failure message.
        result: Result<String, String>,
        /// Whether the user asked for the refresh (reported in the status line).
        manual: bool,
    },
    /// A workspace save finished.
    Saved(Result<(), String>),
    /// A transient review was saved as a durable one (or failed to).
    SavedAs(Box<Result<bcode_code_review_models::ReviewWorkspace, String>>),
    /// The review's annotations were listed.
    NotesLoaded {
        /// Document generation they were listed for.
        generation: u64,
        /// Annotations, or the failure message.
        result: Result<super::notes::LoadedNotes, String>,
    },
    /// The running annotation mutation finished.
    NoteDone(super::notes::MutationResult),
    /// Work of the publish flow finished.
    Publish(super::publish::FlowResult),
    /// Work of the import flow finished.
    Import(super::import::ImportResult),
    /// The re-review comparison finished.
    Rereview(Box<super::rereview::RereviewPlan>),
    /// Patches unchanged since the last pass: `(source id, path, patch id)`.
    Carried(Result<Vec<(String, String, String)>, String>),
    /// Paths matching the composer's `@word`, for the completion tagged `generation`.
    PathCompletions {
        /// Completion generation the search was made for.
        generation: u64,
        /// Matching paths, or why the search failed.
        result: Result<bcode_code_review_models::ReviewFilesSearchResponse, String>,
    },
    /// A permission answer or a stop of an AI card's session finished.
    Feed(crate::agent::FeedOutcome),
    /// An AI question finished: the linked exchange, or the exchange as recorded and the error.
    Asked(
        Box<
            Result<
                bcode_code_review_models::ReviewAiExchange,
                Box<(bcode_code_review_models::ReviewAiExchange, String)>,
            >,
        >,
    ),
}

/// Outcome of one document need.
#[derive(Debug)]
pub enum NeedResult {
    /// `review.file.hunks.get` answer.
    Hunks(Box<ReviewFileHunksResponse>),
    /// `review.blob.lines.get` answer.
    BlobLines(Box<ReviewBlobLinesResponse>),
    /// `review.stack.attribution` answer.
    Attribution(Box<ReviewStackAttributionResponse>),
    /// The call failed.
    Failed(String),
}

/// Shared queue of results.
#[derive(Debug, Clone, Default)]
pub struct Inbox(Arc<Mutex<Vec<Message>>>);

impl Inbox {
    /// Queue one result.
    pub fn push(&self, message: Message) {
        if let Ok(mut queue) = self.0.lock() {
            queue.push(message);
        }
    }

    /// Take every queued result, oldest first.
    #[must_use]
    pub fn drain(&self) -> Vec<Message> {
        self.0
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default()
    }
}

/// An opened review: document inputs plus what refresh needs.
#[derive(Debug)]
pub struct Opened {
    /// One input per resolved source (or part).
    pub inputs: Vec<SourceInput>,
    /// Per-source diagnostics.
    pub diagnostics: Vec<ReviewSourceDiagnostic>,
    /// Change token taken right after resolution.
    pub token: Option<String>,
    /// Whether any source reads the working tree (the token must cover it).
    pub working_tree: bool,
    /// Diff fingerprints and working-tree content ids of marked files, per source id, so viewed
    /// files show as viewed on the first frame.
    pub fingerprints: Vec<(String, ReviewPatchIdsResponse)>,
    /// The viewed state stored for an unsaved review's target, when asked for.
    pub stored_viewed: Option<ReviewWorkspace>,
}

/// Viewed state an opening review loads before its first frame.
#[derive(Debug, Clone, Default)]
pub struct ViewedInputs {
    /// Paths the review's viewed marks name; files at (or renamed from) them get their
    /// fingerprints.
    pub marked: BTreeSet<String>,
    /// For an unsaved review: the scope id its viewed state is stored under
    /// ([`bcode_code_review::scope::annotation_workspace_id`]).
    pub transient_scope: Option<String>,
}

/// Fingerprints loaded while opening a review at most; files beyond get theirs with their hunks.
const MAX_OPEN_FINGERPRINTS: usize = 2_000;

/// Open a review and load what its first frame needs.
///
/// Routes provider-backed sources, resolves every source, takes a change token, and lists every
/// source's index (paged), reporting progress. Then loads what the first frame's viewed state
/// needs: an unsaved review's stored viewed state, and the fingerprints of files with viewed
/// marks (bounded; failures leave them to their hunks).
///
/// # Errors
///
/// Returns a message when resolution fails as a whole; per-source failures are diagnostics.
pub async fn open(
    services: ReviewServices,
    repo_path: PathBuf,
    sources: Vec<ReviewSource>,
    inbox: Inbox,
    viewed: ViewedInputs,
) -> Result<Opened, String> {
    let labels: Vec<(String, String, bool)> = sources
        .iter()
        .map(|source| {
            (
                source.id.clone(),
                source.label.clone(),
                matches!(source.kind, ReviewSourceKind::Stack { .. }),
            )
        })
        .collect();
    let request = resolve_request_with_providers(&services.transport(), &repo_path, sources)
        .await
        .map_err(|error| error.message)?;
    let resolved: ResolveReviewSourcesResponse = services
        .call(request)
        .await
        .map_err(|error| error.message)?;
    let working_tree = resolved.sources.iter().any(reads_working_tree);
    let token = token(&services, &repo_path, working_tree).await.ok();
    let mut inputs = Vec::with_capacity(resolved.sources.len());
    let mut files = 0;
    let mut total = 0;
    for source in resolved.sources {
        let entries = index_entries(
            &services, &repo_path, &source, &inbox, &mut files, &mut total,
        )
        .await
        .map_err(|error| format!("{}: {error}", source.source_id))?;
        let (label, whole) = labels
            .iter()
            .find(|(id, _, _)| *id == source.source_id)
            .map_or_else(
                || (source.source_id.clone(), false),
                |(_, label, whole)| (label.clone(), *whole),
            );
        let stack = resolved
            .stacks
            .iter()
            .find(|stack| stack.source_id == source.source_id)
            .map(|stack| SourceStack {
                stack: stack.clone(),
                whole,
            });
        inputs.push(SourceInput {
            source,
            label,
            entries,
            stack,
        });
    }
    let stored_viewed = match viewed.transient_scope {
        Some(workspace_id) => services
            .call(GetReviewWorkspaceRequest {
                repo_path: repo_path.clone(),
                workspace_id,
            })
            .await
            .ok()
            .and_then(|response| response.workspace),
        None => None,
    };
    let mut marked = viewed.marked;
    if let Some(stored) = &stored_viewed {
        marked.extend(
            bcode_code_review::viewed::effective_marks(stored)
                .into_iter()
                .map(|mark| mark.path),
        );
    }
    let fingerprints = fingerprints(&services, &repo_path, &inputs, &marked).await;
    Ok(Opened {
        inputs,
        diagnostics: resolved.diagnostics,
        token,
        working_tree,
        fingerprints,
        stored_viewed,
    })
}

/// Fingerprints of the files of `inputs` at or renamed from `marked` paths, at most
/// [`MAX_OPEN_FINGERPRINTS`], in requests of at most [`REVIEW_PATCH_IDS_MAX_PATHS`] paths. A
/// renamed file is asked for by both of its paths in one request.
async fn fingerprints(
    services: &ReviewServices,
    repo_path: &std::path::Path,
    inputs: &[SourceInput],
    marked: &BTreeSet<String>,
) -> Vec<(String, ReviewPatchIdsResponse)> {
    let mut answers = Vec::new();
    if marked.is_empty() {
        return answers;
    }
    let mut budget = MAX_OPEN_FINGERPRINTS;
    for input in inputs {
        if input.source.mode == ResolvedSourceMode::Browse || budget == 0 {
            continue;
        }
        let mut chunks: Vec<Vec<String>> = vec![Vec::new()];
        for entry in &input.entries {
            if budget == 0 {
                break;
            }
            let old = entry.old_path.as_ref().filter(|old| **old != entry.path);
            let named = marked.contains(&entry.path) || old.is_some_and(|old| marked.contains(old));
            if !named || entry.has(ReviewIndexFlag::Binary) {
                continue;
            }
            let paths: Vec<String> = std::iter::once(entry.path.clone())
                .chain(old.cloned())
                .collect();
            if chunks
                .last()
                .is_some_and(|chunk| chunk.len() + paths.len() > REVIEW_PATCH_IDS_MAX_PATHS)
            {
                chunks.push(Vec::new());
            }
            if let Some(chunk) = chunks.last_mut() {
                chunk.extend(paths);
            }
            budget -= 1;
        }
        let mut merged = ReviewPatchIdsResponse::default();
        for paths in chunks.into_iter().filter(|paths| !paths.is_empty()) {
            let Ok(answer) = services
                .call(ReviewPatchIdsRequest {
                    repo_path: repo_path.to_path_buf(),
                    base: input.source.base.clone(),
                    head: input.source.head.clone(),
                    paths,
                })
                .await
            else {
                continue;
            };
            merged.patch_ids.extend(answer.patch_ids);
            merged.content_ids.extend(answer.content_ids);
        }
        if !merged.patch_ids.is_empty() || !merged.content_ids.is_empty() {
            answers.push((input.source.source_id.clone(), merged));
        }
    }
    answers
}

async fn index_entries(
    services: &ReviewServices,
    repo_path: &std::path::Path,
    source: &ResolvedSource,
    inbox: &Inbox,
    files: &mut usize,
    total: &mut usize,
) -> Result<Vec<bcode_code_review_models::ReviewIndexEntry>, String> {
    let mut entries = Vec::new();
    let mut cursor = None;
    let mut counted = false;
    loop {
        let page: ReviewIndexResponse = services
            .call(ReviewIndexRequest {
                repo_path: repo_path.to_path_buf(),
                source: source.clone(),
                cursor: cursor.take(),
                limit: None,
            })
            .await
            .map_err(|error| error.message)?;
        if !counted {
            counted = true;
            *total += page.total_entries as usize;
        }
        *files += page.entries.len();
        entries.extend(page.entries);
        inbox.push(Message::Progress {
            files: *files,
            total: *total,
        });
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(entries),
        }
    }
}

const fn reads_working_tree(source: &ResolvedSource) -> bool {
    matches!(source.head, RevisionRef::WorkingTree)
        || matches!(source.base, RevisionRef::WorkingTree)
}

/// Take the repository's change token.
///
/// # Errors
///
/// Returns the service error message.
pub async fn token(
    services: &ReviewServices,
    repo_path: &std::path::Path,
    include_working_tree: bool,
) -> Result<String, String> {
    let response: ReviewSourcesTokenResponse = services
        .call(ReviewSourcesTokenRequest {
            repo_path: repo_path.to_path_buf(),
            include_working_tree,
        })
        .await
        .map_err(|error| error.message)?;
    Ok(response.token)
}

/// Save `workspace` (its presentation state) with `review.workspace.update`.
///
/// # Errors
///
/// Returns the service error message.
pub async fn save_workspace(
    services: ReviewServices,
    workspace: bcode_code_review_models::ReviewWorkspace,
) -> Result<(), String> {
    services
        .call(bcode_code_review_models::UpdateReviewWorkspaceRequest {
            repo_path: workspace.repo_root.clone(),
            workspace,
        })
        .await
        .map_err(|error| error.message)?;
    Ok(())
}

/// The record an unsaved review's viewed state is stored in: its viewed files and marks under
/// its transient scope id, without presentation or review-pass state.
#[must_use]
pub fn transient_viewed_record(
    transient: &bcode_code_review_models::ReviewWorkspace,
) -> bcode_code_review_models::ReviewWorkspace {
    bcode_code_review_models::ReviewWorkspace {
        id: bcode_code_review::scope::annotation_workspace_id(transient),
        title: transient.title.clone(),
        repo_root: transient.repo_root.clone(),
        sources: transient.sources.clone(),
        created_at_ms: None,
        updated_at_ms: None,
        viewed_files: transient.viewed_files.clone(),
        archived_at_ms: None,
        viewed_marks: transient.viewed_marks.clone(),
        reviewed_revisions: std::collections::BTreeMap::new(),
        presentation: None,
    }
}

/// Save a transient review as a durable workspace and return it.
///
/// The workspace is created with the same title and sources, gets the viewed, presentation, and
/// reviewed-revision state, and the drafts, AI exchanges, and suggestions move into its scope.
///
/// # Errors
///
/// Returns the first failing call's message.
pub async fn save_as(
    services: ReviewServices,
    transient: bcode_code_review_models::ReviewWorkspace,
    notes: super::notes::LoadedNotes,
) -> Result<bcode_code_review_models::ReviewWorkspace, String> {
    let created: bcode_code_review_models::CreateReviewWorkspaceResponse = services
        .call(bcode_code_review_models::CreateReviewWorkspaceRequest {
            repo_path: transient.repo_root.clone(),
            title: Some(transient.title.clone()),
            sources: transient.sources.clone(),
        })
        .await
        .map_err(|error| error.message)?;
    let mut workspace = created.workspace;
    workspace.viewed_files.clone_from(&transient.viewed_files);
    workspace.viewed_marks.clone_from(&transient.viewed_marks);
    workspace
        .reviewed_revisions
        .clone_from(&transient.reviewed_revisions);
    workspace.presentation.clone_from(&transient.presentation);
    save_workspace(services.clone(), workspace.clone()).await?;
    // The viewed state moves with the review, like its notes.
    let mut left = transient_viewed_record(&transient);
    left.viewed_files.clear();
    left.viewed_marks.clear();
    save_workspace(services.clone(), left).await?;
    let (from, to) = (
        super::notes::NoteScope::for_workspace(&transient),
        super::notes::NoteScope::for_workspace(&workspace),
    );
    super::notes::move_drafts(services.clone(), from.clone(), to.clone(), notes.drafts).await?;
    super::notes::move_agent_notes(services, from, to, notes.exchanges, notes.suggestions).await?;
    Ok(workspace)
}

/// Serve one document need.
pub async fn serve_need(services: ReviewServices, need: DocumentNeed) -> (Ticket, NeedResult) {
    let ticket = need.ticket();
    let result = match need {
        DocumentNeed::Hunks { request, .. } => services
            .call(request)
            .await
            .map(|response| NeedResult::Hunks(Box::new(response))),
        DocumentNeed::BlobLines { request, .. } => services
            .call(request)
            .await
            .map(|response| NeedResult::BlobLines(Box::new(response))),
        DocumentNeed::Attribution { request, .. } => services
            .call(request)
            .await
            .map(|response| NeedResult::Attribution(Box::new(response))),
    };
    (
        ticket,
        result.unwrap_or_else(|error| NeedResult::Failed(error.message)),
    )
}

/// Read a whole blob as text in bounded chunks. Returns `None` when the blob is binary, has
/// more than `max_lines` lines or `max_bytes` bytes, or cannot be read.
pub async fn read_blob_text(
    services: ReviewServices,
    repo_path: PathBuf,
    selector: ReviewBlobSelector,
    max_lines: u32,
    max_bytes: usize,
) -> Option<String> {
    let mut text = String::new();
    let mut start = 1;
    loop {
        let chunk: ReviewBlobLinesResponse = services
            .call(ReviewBlobLinesRequest {
                repo_path: repo_path.clone(),
                blob: selector.clone(),
                start_line: start,
                count: REVIEW_BLOB_LINES_MAX_COUNT,
            })
            .await
            .ok()?;
        if chunk.binary || chunk.total_lines > max_lines {
            return None;
        }
        let returned = u32::try_from(chunk.lines.len()).unwrap_or(u32::MAX);
        for line in chunk.lines {
            text.push_str(&line);
            text.push('\n');
        }
        if text.len() > max_bytes {
            return None;
        }
        start = start.saturating_add(returned);
        if returned == 0 || start > chunk.total_lines {
            return Some(text);
        }
    }
}
