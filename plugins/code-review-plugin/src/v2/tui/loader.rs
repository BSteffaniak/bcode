//! Asynchronous work of the review surface: opening a review, serving document needs, reading
//! whole blobs for highlighting, change tokens, and saving presentation state.
//!
//! Every function here runs on a host-spawned task and reports through the [`Inbox`]; the
//! surface drains the inbox from `poll`, so frames never wait on a service.

use super::backend::{SharedBackend, call_json};
use crate::v2::document::{DocumentNeed, SourceInput, SourceStack, Ticket};
use crate::v2::provider::resolve_request_with_providers;
use bcode_code_review_models::{
    OP_REVIEW_BLOB_LINES_GET, OP_REVIEW_FILE_HUNKS_GET, OP_REVIEW_INDEX_GET,
    OP_REVIEW_SOURCES_RESOLVE, OP_REVIEW_SOURCES_TOKEN, OP_REVIEW_STACK_ATTRIBUTION,
    REVIEW_BLOB_LINES_MAX_COUNT, ResolveReviewSourcesResponse, ResolvedSource,
    ReviewBlobLinesRequest, ReviewBlobLinesResponse, ReviewBlobSelector, ReviewFileHunksResponse,
    ReviewIndexRequest, ReviewIndexResponse, ReviewSource, ReviewSourceDiagnostic,
    ReviewSourceKind, ReviewSourcesTokenRequest, ReviewSourcesTokenResponse,
    ReviewStackAttributionResponse, RevisionRef,
};
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
        /// Path used to pick the grammar.
        path: String,
        /// Text, or `None` when the blob is binary, too large, or unreadable.
        text: Option<String>,
    },
    /// A finished highlight.
    Highlighted {
        /// Cache key of the highlight.
        key: super::syntax::SyntaxKey,
        /// Tokens, or `None` when highlighting was skipped.
        text: Option<Arc<bcode_syntax_render::HighlightedText>>,
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
    /// The review's annotations were listed.
    NotesLoaded {
        /// Document generation they were listed for.
        generation: u64,
        /// Annotations, or the failure message.
        result: Result<super::notes::LoadedNotes, String>,
    },
    /// The running annotation mutation finished.
    NoteDone(super::notes::MutationResult),
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
}

/// Open a review: route provider-backed sources, resolve every source, take a change token, and
/// list every source's index (paged), reporting progress.
///
/// # Errors
///
/// Returns a message when resolution fails as a whole; per-source failures are diagnostics.
pub async fn open(
    backend: SharedBackend,
    repo_path: PathBuf,
    sources: Vec<ReviewSource>,
    inbox: Inbox,
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
    let request = resolve_request_with_providers(
        &super::backend::BackendTransport(backend.as_ref()),
        &repo_path,
        sources,
    )
    .await
    .map_err(|error| error.message)?;
    let resolved: ResolveReviewSourcesResponse =
        call_json(backend.as_ref(), OP_REVIEW_SOURCES_RESOLVE, &request)
            .await
            .map_err(|error| error.message)?;
    let working_tree = resolved.sources.iter().any(reads_working_tree);
    let token = token(&backend, &repo_path, working_tree).await.ok();
    let mut inputs = Vec::with_capacity(resolved.sources.len());
    let mut files = 0;
    let mut total = 0;
    for source in resolved.sources {
        let entries = index_entries(
            &backend, &repo_path, &source, &inbox, &mut files, &mut total,
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
        let label = match &source.part {
            Some(part) => format!("{label} · {}", part.label),
            None => label,
        };
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
    Ok(Opened {
        inputs,
        diagnostics: resolved.diagnostics,
        token,
        working_tree,
    })
}

async fn index_entries(
    backend: &SharedBackend,
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
        let page: ReviewIndexResponse = call_json(
            backend.as_ref(),
            OP_REVIEW_INDEX_GET,
            &ReviewIndexRequest {
                repo_path: repo_path.to_path_buf(),
                source: source.clone(),
                cursor: cursor.take(),
                limit: None,
            },
        )
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
    backend: &SharedBackend,
    repo_path: &std::path::Path,
    include_working_tree: bool,
) -> Result<String, String> {
    let response: ReviewSourcesTokenResponse = call_json(
        backend.as_ref(),
        OP_REVIEW_SOURCES_TOKEN,
        &ReviewSourcesTokenRequest {
            repo_path: repo_path.to_path_buf(),
            include_working_tree,
        },
    )
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
    backend: SharedBackend,
    workspace: bcode_code_review_models::ReviewWorkspace,
) -> Result<(), String> {
    let _: serde_json::Value = call_json(
        backend.as_ref(),
        bcode_code_review_models::OP_REVIEW_WORKSPACE_UPDATE,
        &bcode_code_review_models::UpdateReviewWorkspaceRequest {
            repo_path: workspace.repo_root.clone(),
            workspace,
        },
    )
    .await
    .map_err(|error| error.message)?;
    Ok(())
}

/// Serve one document need.
pub async fn serve_need(backend: SharedBackend, need: DocumentNeed) -> (Ticket, NeedResult) {
    let ticket = need.ticket();
    let result = match need {
        DocumentNeed::Hunks { request, .. } => {
            call_json(backend.as_ref(), OP_REVIEW_FILE_HUNKS_GET, &request)
                .await
                .map(|response| NeedResult::Hunks(Box::new(response)))
        }
        DocumentNeed::BlobLines { request, .. } => {
            call_json(backend.as_ref(), OP_REVIEW_BLOB_LINES_GET, &request)
                .await
                .map(|response| NeedResult::BlobLines(Box::new(response)))
        }
        DocumentNeed::Attribution { request, .. } => {
            call_json(backend.as_ref(), OP_REVIEW_STACK_ATTRIBUTION, &request)
                .await
                .map(|response| NeedResult::Attribution(Box::new(response)))
        }
    };
    (
        ticket,
        result.unwrap_or_else(|error| NeedResult::Failed(error.message)),
    )
}

/// Read a whole blob as text in bounded chunks. Returns `None` when the blob is binary, has
/// more than `max_lines` lines or `max_bytes` bytes, or cannot be read.
pub async fn read_blob_text(
    backend: SharedBackend,
    repo_path: PathBuf,
    selector: ReviewBlobSelector,
    max_lines: u32,
    max_bytes: usize,
) -> Option<String> {
    let mut text = String::new();
    let mut start = 1;
    loop {
        let chunk: ReviewBlobLinesResponse = call_json(
            backend.as_ref(),
            OP_REVIEW_BLOB_LINES_GET,
            &ReviewBlobLinesRequest {
                repo_path: repo_path.clone(),
                blob: selector.clone(),
                start_line: start,
                count: REVIEW_BLOB_LINES_MAX_COUNT,
            },
        )
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
