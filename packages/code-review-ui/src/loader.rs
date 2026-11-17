//! Asynchronous work of the review surface: opening a review, serving document needs, reading
//! whole blobs for highlighting, change tokens, and saving presentation state.
//!
//! Every function here runs on a host-spawned task and reports through the [`Inbox`]; the
//! surface drains the inbox from `poll`, so frames never wait on a service.

use crate::services::ReviewServices;
use bcode_code_review::document::{DocumentNeed, SourceInput, SourceStack, Ticket};
use bcode_code_review::provider::ProviderCallError;
use bcode_code_review::session;
use bcode_code_review_models::{
    GetReviewWorkspaceRequest, REVIEW_BLOB_LINES_MAX_COUNT, REVIEW_PATCH_IDS_MAX_PATHS,
    ResolvedSource, ResolvedSourceMode, ReviewBlobLinesRequest, ReviewBlobLinesResponse,
    ReviewBlobSelector, ReviewFileHunksResponse, ReviewIndexFlag, ReviewPatchIdsRequest,
    ReviewPatchIdsResponse, ReviewSource, ReviewSourceDiagnostic, ReviewSourceKind,
    ReviewSourcesTokenRequest, ReviewSourcesTokenResponse, ReviewStackAttributionResponse,
    ReviewWorkspace, RevisionRef,
};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Results delivered from spawned work to the surface.
#[derive(Debug)]
pub enum Message {
    /// What opening is doing, for the loading screen.
    Stage(String),
    /// The review opened (possibly with sources still loading; see [`Opened::complete`]).
    Opened(Box<Opened>),
    /// The review could not be opened.
    OpenFailed(String),
    /// A source arrived after the first frame: listed, or pending (see [`Arrived`]).
    Arrived(Box<Arrived>),
    /// A source failed after the first frame.
    SourceFailed(ReviewSourceDiagnostic),
    /// The review's stack, as its provider resolved or confirmed it (or as remembered).
    StackConfirmed(Box<session::StackNews>),
    /// What is still loading after the first frame (`None`: nothing).
    Filling(Option<String>),
    /// The stack's history (quietly loaded, for the layer strip), or why it could not be.
    History(Result<bcode_code_review_models::StackTimeline, String>),
    /// The stack's history for the history panel, or why it could not be read.
    HistoryPanel(Result<bcode_code_review_models::StackTimeline, String>),
    /// Pull request conversation work finished.
    Conversation(Box<super::surface::ConversationMessage>),
    /// Every source arrived: the change token taken after them.
    Filled {
        /// Change token, when it could be taken.
        token: Option<String>,
        /// Whether a source reads the working tree.
        working_tree: bool,
    },
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
    /// A workspace save finished, with the record it wrote.
    Saved(Result<Box<bcode_code_review_models::ReviewWorkspace>, crate::surface::SaveFailure>),
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
    /// The reviewer's looks at the review's layers were listed.
    Looks(
        Vec<u64>,
        Result<Vec<bcode_code_review_models::LayerLook>, String>,
    ),
    /// Layers were compared with their baselines.
    Compared(Result<Vec<bcode_code_review_models::LayerDelta>, String>),
    /// A layer's version steps (each pair of consecutive available versions compared).
    CompareSteps(Box<StepsArrived>),
    /// Paths matching the composer's `@word`, for the completion tagged `generation`.
    PathCompletions {
        /// Completion generation the search was made for.
        generation: u64,
        /// Matching paths, or why the search failed.
        result: Result<bcode_code_review_models::ReviewFilesSearchResponse, String>,
    },
    /// A permission answer or a stop of an AI card's session finished.
    Feed(crate::agent::FeedOutcome),
    /// Reflection work for finished review tool calls finished.
    Tools(Box<super::surface::ToolsResult>),
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
    /// `review.compare.marks` answer.
    Marks(Box<bcode_code_review_models::ReviewCompareMarksResponse>),
    /// `review.worktree.marks` answer.
    WorktreeMarks(Box<bcode_code_review_models::ReviewWorktreeMarksResponse>),
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
    /// Every source is listed; when `false`, some inputs are loading groups and a complete
    /// [`Opened`] follows.
    pub complete: bool,
    /// What each source cut to its file limit says (see
    /// [`bcode_code_review_models::listing_truncated_message`]).
    pub truncated: Vec<String>,
    /// Stack confirmations retained when an atomic refresh does not stream events.
    pub stacks: Vec<session::StackNews>,
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

/// A layer's version steps, computed for the head the layer showed when they were asked for.
#[derive(Debug)]
pub struct StepsArrived {
    /// Pull request number.
    pub number: u64,
    /// The head the layer showed.
    pub head: String,
    /// The number (1-based) of the version the layer showed.
    pub current_version: usize,
    /// Each step asked: its pair, and its older and newer version numbers (1-based).
    pub versions: Vec<(bcode_code_review_models::LayerPair, usize, usize)>,
    /// The steps compared.
    pub result: Result<Vec<bcode_code_review_models::LayerDelta>, String>,
}

/// A source that arrived after the first frame.
#[derive(Debug)]
pub struct Arrived {
    /// Its document group (loading while it is only pending).
    pub input: SourceInput,
    /// Its index among the review's included sources (workspace order).
    pub position: usize,
    /// Fingerprints of its files with viewed marks.
    pub fingerprints: Option<(String, ReviewPatchIdsResponse)>,
    /// What it says when its listing was cut to its file limit.
    pub truncated: Option<String>,
}

/// How [`open`] runs.
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// What the first frame's viewed state needs.
    pub viewed: ViewedInputs,
    /// The source to make ready first.
    pub first: Option<String>,
    /// Push the review through the inbox as it arrives (see [`open`]).
    pub stream: bool,
    /// Set when the review no longer wants the open.
    pub cancel: bcode_code_review::provider::Cancel,
    /// How browse sources list: ignored files, and the file limit.
    pub browse: bcode_code_review_models::BrowseOptions,
}

/// Open a review progressively ([`bcode_code_review::session::open`]).
///
/// The session runs on a blocking thread and reports through a channel. Before the first frame,
/// stages become [`Message::Stage`]. With [`OpenOptions::stream`], the first frame is pushed as an
/// incomplete [`Message::Opened`] as soon as the first source is listed (with the others known so
/// far as loading groups); every later source as [`Message::Arrived`] or
/// [`Message::SourceFailed`]; what is still loading as [`Message::Filling`]; stacks as
/// [`Message::StackConfirmed`]; and the end as [`Message::Filled`]. The answer is then `None`.
/// Without it (a refresh, which keeps showing the old review until the new one is whole) nothing
/// is pushed and the answer is the whole review; so it is too when no source could be listed.
///
/// Each [`Opened`] carries what its first frame's viewed state needs: an unsaved review's stored
/// viewed state and the fingerprints of files with viewed marks (bounded; failures leave them to
/// their hunks). [`OpenOptions::cancel`] stops the open (and providers that check it) when the
/// review no longer wants it.
///
/// # Errors
///
/// Returns a message when opening fails as a whole; per-source failures are diagnostics.
pub async fn open(
    services: ReviewServices,
    repo_path: PathBuf,
    sources: Vec<ReviewSource>,
    inbox: Inbox,
    options: OpenOptions,
) -> Result<Option<Opened>, String> {
    let labels: Vec<(String, String, bool)> = sources
        .iter()
        .filter(|source| source.included)
        .map(|source| {
            (
                source.id.clone(),
                source.label.clone(),
                matches!(source.kind, ReviewSourceKind::Stack { .. }),
            )
        })
        .collect();
    let request = session::OpenRequest {
        repo_path: repo_path.clone(),
        sources,
        first: options.first,
        max_layers: services.stack_limit().resolve_limit(),
        cancel: options.cancel,
        browse: options.browse,
    };
    let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
    // Opening a provider-backed review confirms remote state even when the local repository
    // and the registry's recent snapshot have not changed.
    let (data, registry) = (
        services.data(),
        services
            .providers()
            .with_stack_freshness(std::time::Duration::ZERO),
    );
    let work = services.blocking(move || {
        // Once the review is closed or reopened, the receiver is gone and the open stops.
        session::open(&data, &registry, &request, |event| {
            if sender.send(event).is_ok() {
                std::ops::ControlFlow::Continue(())
            } else {
                std::ops::ControlFlow::Break(())
            }
        })
        .map_err(|error| ProviderCallError::new(error.code(), error.to_string()))
    });
    let consume = async {
        let (stored_viewed, marked) = viewed_state(&services, &repo_path, &options.viewed).await;
        let mut streamer = Streamer {
            services: &services,
            repo_path: &repo_path,
            labels: &labels,
            inbox: &inbox,
            stream: options.stream,
            stored_viewed,
            marked,
            known: Known::default(),
            stacks: Vec::new(),
            shown: false,
        };
        while let Some(event) = events.recv().await {
            streamer.handle(event).await;
        }
        streamer
    };
    let (result, streamer) = tokio::join!(work, consume);
    result.map_err(|error| error.message)?;
    streamer.finish().await
}

/// What [`open`] made of the session's events so far, and where it pushes them.
struct Streamer<'a> {
    services: &'a ReviewServices,
    repo_path: &'a std::path::Path,
    labels: &'a [(String, String, bool)],
    inbox: &'a Inbox,
    stream: bool,
    stored_viewed: Option<ReviewWorkspace>,
    marked: BTreeSet<String>,
    known: Known,
    /// Stacks reported before the first frame, pushed with it.
    stacks: Vec<session::StackNews>,
    /// Whether the first frame was pushed.
    shown: bool,
}

impl Streamer<'_> {
    async fn handle(&mut self, event: session::OpenEvent) {
        let live = self.shown && self.stream;
        match event {
            session::OpenEvent::Stage(stage) => {
                if !self.shown {
                    self.inbox.push(Message::Stage(stage_text(&stage)));
                } else if self.stream {
                    self.inbox.push(Message::Filling(Some(stage_text(&stage))));
                }
            }
            session::OpenEvent::Stack(news) => {
                // Every stack seen is a version of each of its layers, kept in the version log.
                record_observed(self.services, self.repo_path, &news);
                if live {
                    self.inbox.push(Message::StackConfirmed(Box::new(news)));
                } else {
                    self.stacks.push(news);
                }
            }
            session::OpenEvent::Pending(opened) => {
                if self.known.listed(&opened.source.source_id) {
                    return;
                }
                self.known.put(opened.clone(), true);
                if live {
                    self.arrived(opened, true).await;
                }
            }
            session::OpenEvent::Ready(opened) => {
                self.known.put(opened.clone(), false);
                if live {
                    self.arrived(opened, false).await;
                } else if self.stream {
                    self.first_frame().await;
                }
            }
            session::OpenEvent::Failed(diagnostic) => {
                self.known.forget(&diagnostic.source_id);
                if live {
                    self.inbox.push(Message::SourceFailed(diagnostic.clone()));
                }
                self.known.diagnostics.push(diagnostic);
            }
            session::OpenEvent::Done => {}
        }
    }

    /// Push the first frame: what is listed, and what is known to be loading.
    async fn first_frame(&mut self) {
        let mut opened = opened_from(
            self.services,
            self.repo_path,
            self.known.inputs(self.labels),
            self.known.diagnostics.clone(),
            &self.marked,
            self.stored_viewed.clone(),
            false,
        )
        .await;
        opened.truncated = self.known.truncated(self.labels);
        self.inbox.push(Message::Opened(Box::new(opened)));
        for news in std::mem::take(&mut self.stacks) {
            self.inbox.push(Message::StackConfirmed(Box::new(news)));
        }
        self.shown = true;
    }

    /// Push a source that arrived after the first frame.
    async fn arrived(&self, opened: session::OpenedSource, pending: bool) {
        let truncated = truncation(self.labels, &opened);
        let input = document_input(self.labels, opened, pending);
        let fingerprints = if pending {
            None
        } else {
            fingerprints(
                self.services,
                self.repo_path,
                std::slice::from_ref(&input),
                &self.marked,
            )
            .await
            .pop()
        };
        self.inbox.push(Message::Arrived(Box::new(Arrived {
            position: position(self.labels, &input.source.source_id),
            input,
            fingerprints,
            truncated,
        })));
    }

    /// End the open: when streamed, push the change token and answer `None`; else answer the
    /// whole review.
    async fn finish(self) -> Result<Option<Opened>, String> {
        if self.shown {
            let working_tree = self
                .known
                .sources
                .iter()
                .any(|(opened, _)| reads_working_tree(&opened.source));
            let token = token(self.services, self.repo_path, working_tree)
                .await
                .ok();
            self.inbox.push(Message::Filling(None));
            self.inbox.push(Message::Filled {
                token,
                working_tree,
            });
            return Ok(None);
        }
        if self.known.sources.is_empty() && self.known.diagnostics.is_empty() {
            return Err("the review opened no sources".to_owned());
        }
        let inputs = self.known.inputs(self.labels);
        let complete = !inputs.iter().any(|input| input.loading);
        let truncated = self.known.truncated(self.labels);
        let mut opened = opened_from(
            self.services,
            self.repo_path,
            inputs,
            self.known.diagnostics,
            &self.marked,
            self.stored_viewed,
            complete,
        )
        .await;
        opened.truncated = truncated;
        opened.stacks = self.stacks;
        Ok(Some(opened))
    }
}

/// Record the versions of `news`'s layers in the version log, without waiting.
fn record_observed(
    services: &ReviewServices,
    repo_path: &std::path::Path,
    news: &session::StackNews,
) {
    let at_ms = news
        .confirmed_at
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|since| u64::try_from(since.as_millis()).ok())
        .unwrap_or(0);
    let versions = bcode_code_review::history::observed_versions(
        &news.provider,
        &news.repository,
        &news.stack,
        at_ms,
    );
    if versions.is_empty() {
        return;
    }
    let (services, repo_path) = (services.clone(), repo_path.to_path_buf());
    tokio::spawn(async move {
        let _: Result<bcode_code_review_models::RecordLayerVersionsResponse, _> = services
            .call(bcode_code_review_models::RecordLayerVersionsRequest {
                repo_path,
                versions,
            })
            .await;
    });
}

/// What an open reported so far: each source with whether it is only pending, and failures.
#[derive(Debug, Default)]
struct Known {
    sources: Vec<(session::OpenedSource, bool)>,
    diagnostics: Vec<ReviewSourceDiagnostic>,
}

impl Known {
    /// Whether source `id` was listed (a pending placeholder never replaces it).
    fn listed(&self, id: &str) -> bool {
        self.sources
            .iter()
            .any(|(source, pending)| source.source.source_id == id && !pending)
    }

    fn put(&mut self, source: session::OpenedSource, pending: bool) {
        self.forget(&source.source.source_id);
        self.sources.push((source, pending));
    }

    fn forget(&mut self, id: &str) {
        self.sources
            .retain(|(source, _)| source.source.source_id != id);
    }

    /// What every listed source cut to its file limit says, in workspace order.
    fn truncated(&self, labels: &[(String, String, bool)]) -> Vec<String> {
        let mut cut: Vec<(usize, String)> = self
            .sources
            .iter()
            .filter(|(_, pending)| !pending)
            .filter_map(|(source, _)| {
                truncation(labels, source)
                    .map(|text| (position(labels, &source.source.source_id), text))
            })
            .collect();
        cut.sort();
        cut.into_iter().map(|(_, text)| text).collect()
    }

    /// The sources as document groups, in workspace order.
    fn inputs(&self, labels: &[(String, String, bool)]) -> Vec<SourceInput> {
        let mut inputs: Vec<SourceInput> = self
            .sources
            .iter()
            .map(|(source, pending)| document_input(labels, source.clone(), *pending))
            .collect();
        inputs.sort_by_key(|input| position(labels, &input.source.source_id));
        inputs
    }
}

/// Index of source `id` among the review's included sources.
fn position(labels: &[(String, String, bool)], id: &str) -> usize {
    labels
        .iter()
        .position(|(label, _, _)| label == id)
        .unwrap_or(usize::MAX)
}

/// What the loading screen says for `stage`.
fn stage_text(stage: &session::OpenStage) -> String {
    match stage {
        session::OpenStage::AskingProviders => "finding pull requests and stacks".to_owned(),
        session::OpenStage::OpeningFirst => "listing files".to_owned(),
        session::OpenStage::Confirming => "checking the stack with its provider".to_owned(),
        session::OpenStage::Listing { done, total } => {
            format!("listing files of every layer: {done} of {total}")
        }
    }
}

/// What `source` says when its listing was cut to its file limit.
fn truncation(labels: &[(String, String, bool)], source: &session::OpenedSource) -> Option<String> {
    let total = source.untruncated_entries?;
    let label = labels
        .iter()
        .find(|(id, _, _)| *id == source.source.source_id)
        .map_or(source.source.source_id.as_str(), |(_, label, _)| {
            label.as_str()
        });
    Some(bcode_code_review_models::listing_truncated_message(
        label,
        total,
        u32::try_from(source.entries.len()).unwrap_or(u32::MAX),
    ))
}

/// `source` as a document group.
fn document_input(
    labels: &[(String, String, bool)],
    source: session::OpenedSource,
    loading: bool,
) -> SourceInput {
    let (label, whole) = labels
        .iter()
        .find(|(id, _, _)| *id == source.source.source_id)
        .map_or_else(
            || (source.source.source_id.clone(), false),
            |(_, label, whole)| (label.clone(), *whole),
        );
    SourceInput {
        stack: source.stack.map(|stack| SourceStack { stack, whole }),
        source: source.source,
        label,
        entries: source.entries,
        loading,
    }
}

/// An unsaved review's stored viewed state, and every path a viewed mark names.
async fn viewed_state(
    services: &ReviewServices,
    repo_path: &std::path::Path,
    viewed: &ViewedInputs,
) -> (Option<ReviewWorkspace>, BTreeSet<String>) {
    let stored_viewed = match viewed.transient_scope.clone() {
        Some(workspace_id) => services
            .call(GetReviewWorkspaceRequest {
                repo_path: repo_path.to_path_buf(),
                workspace_id,
            })
            .await
            .ok()
            .and_then(|response| response.workspace),
        None => None,
    };
    let mut marked = viewed.marked.clone();
    if let Some(stored) = &stored_viewed {
        marked.extend(
            bcode_code_review::viewed::effective_marks(stored)
                .into_iter()
                .map(|mark| mark.path),
        );
    }
    (stored_viewed, marked)
}

/// An [`Opened`] of `inputs`, with the change token and the fingerprints of marked files.
async fn opened_from(
    services: &ReviewServices,
    repo_path: &std::path::Path,
    inputs: Vec<SourceInput>,
    diagnostics: Vec<ReviewSourceDiagnostic>,
    marked: &BTreeSet<String>,
    stored_viewed: Option<ReviewWorkspace>,
    complete: bool,
) -> Opened {
    let working_tree = inputs.iter().any(|input| reads_working_tree(&input.source));
    let token = token(services, repo_path, working_tree).await.ok();
    let fingerprints = fingerprints(services, repo_path, &inputs, marked).await;
    Opened {
        inputs,
        diagnostics,
        token,
        working_tree,
        fingerprints,
        stored_viewed,
        complete,
        truncated: Vec::new(),
        stacks: Vec::new(),
    }
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
            base: None,
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
        viewed_threads: transient.viewed_threads.clone(),
        reviewed_revisions: std::collections::BTreeMap::new(),
        // Only what an unsaved review remembers besides its viewed files: dismissed notices.
        presentation: transient
            .presentation
            .as_ref()
            .filter(|state| !state.dismissed_notices.is_empty())
            .map(|state| bcode_code_review_models::ReviewPresentationState {
                dismissed_notices: state.dismissed_notices.clone(),
                ..Default::default()
            }),
        stack_snapshots: Vec::new(),
    }
}

/// Save a transient review as a durable workspace and return the record written.
///
/// The workspace is created with the same title and sources and gets the whole local record
/// (viewed, presentation, reviewed-revision and stack state); the drafts, AI exchanges, and
/// suggestions move into its scope.
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
            presentation: None,
        })
        .await
        .map_err(|error| error.message)?;
    let workspace = bcode_code_review_models::ReviewWorkspace {
        id: created.workspace.id,
        repo_root: created.workspace.repo_root,
        created_at_ms: created.workspace.created_at_ms,
        updated_at_ms: created.workspace.updated_at_ms,
        archived_at_ms: None,
        ..transient.clone()
    };
    let written: bcode_code_review_models::UpdateReviewWorkspaceResponse = services
        .call(bcode_code_review_models::UpdateReviewWorkspaceRequest {
            repo_path: workspace.repo_root.clone(),
            workspace,
            base: None,
        })
        .await
        .map_err(|error| error.message)?;
    let workspace = written.workspace;
    // The viewed state moves with the review, like its notes.
    let mut left = transient_viewed_record(&transient);
    left.viewed_files.clear();
    left.viewed_marks.clear();
    left.viewed_threads.clear();
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
        DocumentNeed::Marks { request, .. } => services
            .call(request)
            .await
            .map(|response| NeedResult::Marks(Box::new(response))),
        DocumentNeed::WorktreeMarks { request, .. } => services
            .call(request)
            .await
            .map(|response| NeedResult::WorktreeMarks(Box::new(response))),
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
