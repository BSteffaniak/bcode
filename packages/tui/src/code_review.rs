//! Full-screen local code review TUI mode.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};

use bcode_client::BcodeClient;
use bcode_code_review_models::{
    CODE_REVIEW_SERVICE_INTERFACE_ID, MaterializeReviewWorkspaceRequest,
    MaterializeReviewWorkspaceResponse, OP_REVIEW_BUNDLE_GET, OP_REVIEW_PUBLISH_PREVIEW,
    OP_REVIEW_PUBLISH_SUBMIT, OP_REVIEW_PUBLISHER_MANIFEST, OP_REVIEW_PUBLISHER_PREVIEW,
    OP_REVIEW_PUBLISHER_SUBMIT, OP_REVIEW_PUBLISHERS_LIST, OP_REVIEW_REPO_FILE_GET,
    OP_REVIEW_WORKSPACE_MATERIALIZE, OP_REVIEW_WORKSPACE_UPDATE, REVIEW_PUBLISHER_INTERFACE_ID,
    ReviewSource, ReviewSourceKind, ReviewSurface, ReviewSurfaceKind, ReviewWorkspace,
    UpdateReviewWorkspaceRequest,
};
use bcode_ipc::PluginServiceResponse;
use bcode_session_models::SessionId;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::TextEditBuffer;
use bmux_tui::event::{Event, FocusEvent, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyOutcome};
use bmux_tui::terminal::Terminal;
use serde::{Deserialize, Serialize};

use super::terminal_events::TuiInput;
use super::{TuiError, helpers};

const SERVICE_INTERFACE_ID: &str = CODE_REVIEW_SERVICE_INTERFACE_ID;
const CREATE_REVIEW_OPERATION: &str = "create_review";
const LIST_DRAFTS_OPERATION: &str = "draft.list";
const SAVE_DRAFT_OPERATION: &str = "draft.save";
const DELETE_DRAFT_OPERATION: &str = "draft.delete";
const UPDATE_DRAFT_OPERATION: &str = "draft.update";
const LINK_THREAD_SESSION_OPERATION: &str = "thread.link_session";
const PUBLISH_SUBMIT_OPERATION: &str = OP_REVIEW_PUBLISH_SUBMIT;
const PUBLISHERS_LIST_OPERATION: &str = OP_REVIEW_PUBLISHERS_LIST;
const PUBLISH_PREVIEW_OPERATION: &str = OP_REVIEW_PUBLISH_PREVIEW;
const REVIEW_BUNDLE_GET_OPERATION: &str = OP_REVIEW_BUNDLE_GET;
const REVIEW_REPO_FILE_GET_OPERATION: &str = OP_REVIEW_REPO_FILE_GET;
const REVIEW_WORKSPACE_MATERIALIZE_OPERATION: &str = OP_REVIEW_WORKSPACE_MATERIALIZE;
const REVIEW_WORKSPACE_UPDATE_OPERATION: &str = OP_REVIEW_WORKSPACE_UPDATE;
const REVIEW_PUBLISHER_MANIFEST_OPERATION: &str = OP_REVIEW_PUBLISHER_MANIFEST;
const REVIEW_PUBLISHER_PREVIEW_OPERATION: &str = OP_REVIEW_PUBLISHER_PREVIEW;
const REVIEW_PUBLISHER_SUBMIT_OPERATION: &str = OP_REVIEW_PUBLISHER_SUBMIT;
const DEFAULT_PUBLISHER_ID: &str = "markdown_file";
const FILE_SIDEBAR_WIDTH: u16 = 34;

/// Local Git target to open in review mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewOpenTarget {
    /// Review unstaged working-tree changes.
    WorkingTreeUnstaged,
    /// Review staged index changes.
    IndexStaged,
    /// Review staged and unstaged changes together.
    WorkingTreeAndIndex,
    /// Review the last commit.
    LastCommit,
    /// Review a commit range.
    CommitRange {
        /// Base revision.
        base: String,
        /// Head revision.
        head: String,
        /// Whether to use merge-base semantics.
        merge_base: bool,
    },
    /// Review a branch comparison.
    BranchCompare {
        /// Base branch.
        base_branch: String,
        /// Head branch.
        head_branch: String,
        /// Whether to use merge-base semantics.
        merge_base: bool,
    },
    /// Browse and review repository files.
    Repository,
}

/// Run a full-screen local Git review from a durable workspace.
///
/// # Errors
///
/// Returns an error when review data cannot be loaded or terminal I/O fails.
pub async fn run_workspace<W: Write>(
    terminal: &mut Terminal<&mut W>,
    workspace: ReviewWorkspace,
) -> Result<Option<SessionId>, TuiError> {
    let target = target_from_workspace(&workspace);
    run_with_workspace(
        terminal,
        workspace.repo_root.clone(),
        target,
        Some(workspace),
    )
    .await
}

/// Run a full-screen local Git review.
///
/// # Errors
///
/// Returns an error when review data cannot be loaded or terminal I/O fails.
pub async fn run<W: Write>(
    terminal: &mut Terminal<&mut W>,
    repo_path: PathBuf,
    target: ReviewOpenTarget,
) -> Result<Option<SessionId>, TuiError> {
    run_with_workspace(terminal, repo_path, target, None).await
}

async fn run_with_workspace<W: Write>(
    terminal: &mut Terminal<&mut W>,
    repo_path: PathBuf,
    target: ReviewOpenTarget,
    workspace: Option<ReviewWorkspace>,
) -> Result<Option<SessionId>, TuiError> {
    let client = BcodeClient::default_endpoint();
    let review_target: ReviewTarget = target.into();
    let mut input = TuiInput::start();
    let mut app =
        load_review_app(&client, repo_path.clone(), review_target.clone(), workspace).await?;
    let mut needs_redraw = true;

    while !app.should_exit {
        if helpers::resize_from_terminal(terminal)? {
            needs_redraw = true;
        }
        if needs_redraw {
            terminal.draw(|frame| super::code_review_render::render(&mut app, frame))?;
            needs_redraw = false;
        }
        let Some(event) = input.recv().await? else {
            continue;
        };
        if handle_event(&mut app, terminal, &event) {
            needs_redraw = true;
        }
        if handle_pending_file_load(&client, &repo_path, &mut app).await {
            needs_redraw = true;
        }
        if app.take_pending_workspace_save() {
            match save_workspace(&client, app.workspace.clone()).await {
                Ok(()) => app.status_message = Some("saved review workspace".to_string()),
                Err(error) => {
                    app.pending_workspace_save = true;
                    app.status_message = Some(format!("workspace save failed: {error}"));
                }
            }
            needs_redraw = true;
        }
        if handle_pending_workspace_reload(&client, &repo_path, &review_target, &mut app).await {
            needs_redraw = true;
        }
        if handle_pending_draft_save(&client, &repo_path, &review_target, &mut app).await {
            needs_redraw = true;
        }
        if let Some(delete) = app.take_pending_draft_delete() {
            match delete_draft(&client, repo_path.clone(), delete.clone()).await {
                Ok(()) => {
                    app.status_message = Some("deleted draft comment".to_string());
                }
                Err(error) => {
                    app.restore_deleted_draft(delete);
                    app.status_message =
                        Some(format!("delete failed; restored local draft: {error}"));
                }
            }
            needs_redraw = true;
        }
        if let Some(update) = app.take_pending_draft_update() {
            match update_draft(&client, repo_path.clone(), update.clone()).await {
                Ok(()) => {
                    app.status_message = Some("updated draft comment".to_string());
                }
                Err(error) => {
                    app.restore_updated_draft(update);
                    app.status_message =
                        Some(format!("update failed; restored local draft: {error}"));
                }
            }
            needs_redraw = true;
        }
        if let Some(ask) = app.take_pending_agent_session() {
            handle_pending_agent_session(
                &client,
                repo_path.clone(),
                review_target.clone(),
                &mut app,
                ask,
            )
            .await;
            needs_redraw = true;
        }
        if let Some(request) = app.take_publish_request() {
            handle_publish_request(
                &client,
                repo_path.clone(),
                review_target.clone(),
                &mut app,
                request,
            )
            .await;
            needs_redraw = true;
        }
    }

    Ok(app.take_session_to_open())
}

async fn handle_pending_workspace_reload(
    client: &BcodeClient,
    repo_path: &Path,
    fallback_target: &ReviewTarget,
    app: &mut ReviewApp,
) -> bool {
    if !app.take_pending_workspace_reload() {
        return false;
    }
    match load_workspace_review(
        client,
        repo_path.to_path_buf(),
        fallback_target.clone(),
        app.workspace.clone(),
    )
    .await
    {
        Ok(review) => {
            app.replace_review(review);
            app.status_message = Some("updated review content".to_string());
        }
        Err(error) => {
            app.pending_workspace_reload = true;
            app.status_message = Some(format!("review reload failed: {error}"));
        }
    }
    true
}

async fn handle_pending_draft_save(
    client: &BcodeClient,
    repo_path: &Path,
    review_target: &ReviewTarget,
    app: &mut ReviewApp,
) -> bool {
    let Some(save) = app.take_pending_draft_save() else {
        return false;
    };
    match save_draft(client, repo_path.to_path_buf(), review_target.clone(), save).await {
        Ok(()) => app.status_message = Some("saved draft comment".to_string()),
        Err(error) => {
            app.status_message = Some(format!("saved locally; draft persistence failed: {error}"));
        }
    }
    true
}

async fn handle_pending_file_load(
    client: &BcodeClient,
    repo_path: &Path,
    app: &mut ReviewApp,
) -> bool {
    let Some(path) = app.take_pending_file_load() else {
        return false;
    };
    match load_repository_file(client, repo_path.to_path_buf(), path.clone()).await {
        Ok(file) => app.store_loaded_file(file),
        Err(error) => app.status_message = Some(format!("failed to load {path}: {error}")),
    }
    true
}

async fn load_review_app(
    client: &BcodeClient,
    repo_path: PathBuf,
    review_target: ReviewTarget,
    workspace: Option<ReviewWorkspace>,
) -> Result<ReviewApp, TuiError> {
    let review = if let Some(workspace) = workspace.clone() {
        load_workspace_review(client, repo_path.clone(), review_target.clone(), workspace).await?
    } else {
        load_review(client, repo_path.clone(), review_target.clone()).await?
    };
    let drafts = load_drafts(client, repo_path, review_target).await;
    let mut app = ReviewApp::new(review);
    if app.workspace.sources.is_empty()
        && let Some(workspace) = workspace
    {
        app.workspace = workspace;
    }
    app.queue_selected_file_load();
    match drafts {
        Ok(drafts) => app.load_persisted_drafts(drafts),
        Err(error) => {
            app.status_message = Some(format!("failed to load persisted drafts: {error}"));
        }
    }
    Ok(app)
}

async fn save_workspace(client: &BcodeClient, workspace: ReviewWorkspace) -> Result<(), TuiError> {
    let payload = serde_json::to_vec(&UpdateReviewWorkspaceRequest {
        repo_path: workspace.repo_root.clone(),
        workspace,
    })
    .map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            REVIEW_WORKSPACE_UPDATE_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    Ok(())
}

async fn handle_pending_agent_session(
    client: &BcodeClient,
    repo_path: PathBuf,
    review_target: ReviewTarget,
    app: &mut ReviewApp,
    ask: PendingAgentSession,
) {
    if let Some(session_id) = app.session_id_for_anchor(&ask.anchor) {
        if let Ok(session_id) = session_id.parse::<SessionId>() {
            let prompt = app.agent_session_prompt(&ask);
            match client
                .send_user_message(session_id, prompt, bcode_ipc::PromptPlacement::FollowUp)
                .await
            {
                Ok(_) => {
                    app.status_message = Some(format!(
                        "sent review follow-up to linked session {session_id}"
                    ));
                }
                Err(error) => {
                    app.status_message = Some(format!("failed to send review follow-up: {error}"));
                }
            }
        } else {
            app.status_message = Some("linked session id is invalid".to_string());
        }
    } else {
        match create_agent_session(client, repo_path, review_target, app, ask.clone()).await {
            Ok(session_id) => {
                app.mark_thread_session(&ask.anchor, session_id.to_string());
                app.status_message = Some(format!("created Bcode session {session_id}"));
            }
            Err(error) => {
                app.status_message = Some(format!("failed to create Bcode session: {error}"));
            }
        }
    }
}

async fn handle_publish_request(
    client: &BcodeClient,
    repo_path: PathBuf,
    target: ReviewTarget,
    app: &mut ReviewApp,
    request: PendingPublishRequest,
) {
    match request {
        PendingPublishRequest::ListPublishers => match list_publishers(client).await {
            Ok(mut publishers) => {
                match list_external_publishers(client).await {
                    Ok(external) => publishers.extend(external),
                    Err(error) => {
                        app.status_message =
                            Some(format!("external publisher discovery failed: {error}"));
                    }
                }
                app.show_publishers(publishers);
            }
            Err(error) => app.status_message = Some(format!("publisher list failed: {error}")),
        },
        PendingPublishRequest::Preview {
            publisher_id,
            options,
        } => match preview_review(
            client,
            repo_path,
            target,
            Some(app.workspace.clone()),
            app.publisher_for_id(&publisher_id)
                .and_then(|publisher| publisher.route.clone()),
            publisher_id,
            options,
        )
        .await
        {
            Ok(response) => app.show_publish_preview(response.publisher_id, response.preview),
            Err(error) => app.status_message = Some(format!("publish preview failed: {error}")),
        },
        PendingPublishRequest::Submit {
            publisher_id,
            options,
        } => match publish_review(
            client,
            repo_path,
            target,
            Some(app.workspace.clone()),
            app.publisher_for_id(&publisher_id)
                .and_then(|publisher| publisher.route.clone()),
            publisher_id,
            options,
        )
        .await
        {
            Ok(response) => app.finish_publish(response),
            Err(error) => app.status_message = Some(format!("publish failed: {error}")),
        },
    }
}

async fn list_publishers(client: &BcodeClient) -> Result<Vec<ReviewPublisherManifest>, TuiError> {
    let payload = serde_json::to_vec(&serde_json::json!({})).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            PUBLISHERS_LIST_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let response: ListReviewPublishersResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    Ok(response.publishers)
}

async fn preview_review(
    client: &BcodeClient,
    repo_path: PathBuf,
    target: ReviewTarget,
    workspace: Option<ReviewWorkspace>,
    route: Option<ReviewPublisherRoute>,
    publisher_id: String,
    options: Vec<ReviewPublishOption>,
) -> Result<PublishReviewPreviewResponse, TuiError> {
    let options = options_json(options);
    if let Some(route) = route {
        let bundle = load_review_bundle(client, repo_path, target, workspace.clone()).await?;
        let response = invoke_external_publisher(
            client,
            route,
            REVIEW_PUBLISHER_PREVIEW_OPERATION.to_string(),
            bundle,
            options,
        )
        .await?;
        if let Some(error) = response.error {
            return Err(TuiError::PluginService {
                code: error.code,
                message: error.message,
            });
        }
        return serde_json::from_slice(&response.payload).map_err(TuiError::Json);
    }
    let request = PublishReviewRequest {
        repo_path,
        target,
        workspace,
        publisher_id,
        options,
    };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            PUBLISH_PREVIEW_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    serde_json::from_slice(&response.payload).map_err(TuiError::Json)
}

async fn publish_review(
    client: &BcodeClient,
    repo_path: PathBuf,
    target: ReviewTarget,
    workspace: Option<ReviewWorkspace>,
    route: Option<ReviewPublisherRoute>,
    publisher_id: String,
    options: Vec<ReviewPublishOption>,
) -> Result<PublishReviewResponse, TuiError> {
    let options = options_json(options);
    if let Some(route) = route {
        let bundle = load_review_bundle(client, repo_path, target, workspace.clone()).await?;
        let response = invoke_external_publisher(
            client,
            route,
            REVIEW_PUBLISHER_SUBMIT_OPERATION.to_string(),
            bundle,
            options,
        )
        .await?;
        if let Some(error) = response.error {
            return Err(TuiError::PluginService {
                code: error.code,
                message: error.message,
            });
        }
        return serde_json::from_slice(&response.payload).map_err(TuiError::Json);
    }
    let request = PublishReviewRequest {
        repo_path,
        target,
        workspace,
        publisher_id,
        options,
    };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            PUBLISH_SUBMIT_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    serde_json::from_slice(&response.payload).map_err(TuiError::Json)
}

fn options_from_schema(schema: &serde_json::Value) -> Vec<ReviewPublishOption> {
    schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .map(|properties| {
            properties
                .iter()
                .filter_map(|(name, schema)| {
                    let option_type = schema
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    (option_type == "string").then(|| {
                        let mut label = schema
                            .get("description")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or(name)
                            .to_string();
                        if schema
                            .get("required")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false)
                        {
                            label.push_str(" [required]");
                        }
                        if let Some(values) =
                            schema.get("enum").and_then(serde_json::Value::as_array)
                        {
                            let choices = values
                                .iter()
                                .filter_map(serde_json::Value::as_str)
                                .collect::<Vec<_>>()
                                .join("|");
                            if !choices.is_empty() {
                                let _ = write!(label, " [{choices}]");
                            }
                        }
                        let value = schema
                            .get("default")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        ReviewPublishOption {
                            name: name.clone(),
                            label,
                            value,
                        }
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn options_json(options: Vec<ReviewPublishOption>) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    for option in options {
        if !option.value.is_empty() {
            object.insert(option.name, serde_json::Value::String(option.value));
        }
    }
    serde_json::Value::Object(object)
}

async fn list_external_publishers(
    client: &BcodeClient,
) -> Result<Vec<ReviewPublisherManifest>, TuiError> {
    let services = client.plugin_services().await?;
    let mut publishers = Vec::new();
    for service in services
        .into_iter()
        .filter(|service| service.interface_id == REVIEW_PUBLISHER_INTERFACE_ID)
    {
        let response = client
            .invoke_plugin_service(
                service.plugin_id.clone(),
                service.interface_id.clone(),
                REVIEW_PUBLISHER_MANIFEST_OPERATION.to_string(),
                serde_json::to_vec(&serde_json::json!({})).map_err(TuiError::Json)?,
            )
            .await?;
        if let Some(error) = response.error {
            return Err(TuiError::PluginService {
                code: error.code,
                message: error.message,
            });
        }
        let mut publisher: ReviewPublisherManifest =
            serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
        publisher.route = Some(ReviewPublisherRoute {
            plugin_id: service.plugin_id,
            interface_id: service.interface_id,
        });
        publishers.push(publisher);
    }
    Ok(publishers)
}

async fn load_review_bundle(
    client: &BcodeClient,
    repo_path: PathBuf,
    target: ReviewTarget,
    workspace: Option<ReviewWorkspace>,
) -> Result<serde_json::Value, TuiError> {
    let request = ReviewBundleRequest {
        repo_path,
        target,
        workspace,
    };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            REVIEW_BUNDLE_GET_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    serde_json::from_slice(&response.payload).map_err(TuiError::Json)
}

async fn invoke_external_publisher(
    client: &BcodeClient,
    route: ReviewPublisherRoute,
    operation: String,
    bundle: serde_json::Value,
    options: serde_json::Value,
) -> Result<PluginServiceResponse, TuiError> {
    let payload = serde_json::to_vec(&serde_json::json!({
        "bundle": bundle,
        "options": options,
    }))
    .map_err(TuiError::Json)?;
    client
        .invoke_plugin_service(route.plugin_id, route.interface_id, operation, payload)
        .await
        .map_err(TuiError::from)
}

async fn load_workspace_review(
    client: &BcodeClient,
    repo_path: PathBuf,
    fallback_target: ReviewTarget,
    workspace: ReviewWorkspace,
) -> Result<ReviewSummary, TuiError> {
    let payload = serde_json::to_vec(&MaterializeReviewWorkspaceRequest {
        repo_path: repo_path.clone(),
        workspace,
    })
    .map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            REVIEW_WORKSPACE_MATERIALIZE_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let response: MaterializeReviewWorkspaceResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    let materialization = response.materialization;
    let mut files = Vec::new();
    let mut surfaces = Vec::new();
    for surface in materialization.surfaces {
        if let Some(file) = surface.file.clone() {
            files.push(ReviewFile::from_model(file));
            surfaces.push(surface);
        }
    }
    if files.is_empty() {
        return load_review(client, repo_path, fallback_target).await;
    }
    Ok(ReviewSummary {
        title: materialization.workspace.title.clone(),
        repo_root: materialization.workspace.repo_root.clone(),
        files,
        additions: materialization.additions,
        deletions: materialization.deletions,
        workspace: Some(materialization.workspace),
        surfaces,
    })
}

async fn load_review(
    client: &BcodeClient,
    repo_path: PathBuf,
    target: ReviewTarget,
) -> Result<ReviewSummary, TuiError> {
    let request = CreateReviewRequest { repo_path, target };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            CREATE_REVIEW_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let mut summary: ReviewSummary =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    summary.surfaces = summary.surfaces();
    Ok(summary)
}

async fn load_repository_file(
    client: &BcodeClient,
    repo_path: PathBuf,
    file_path: String,
) -> Result<CachedReviewFile, TuiError> {
    let payload = serde_json::to_vec(&serde_json::json!({
        "repo_path": repo_path,
        "file_path": file_path,
    }))
    .map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            REVIEW_REPO_FILE_GET_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let response: RepositoryFileResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    Ok(CachedReviewFile::from_response(response))
}

async fn load_drafts(
    client: &BcodeClient,
    repo_path: PathBuf,
    target: ReviewTarget,
) -> Result<Vec<DraftComment>, TuiError> {
    let request = ListDraftsRequest { repo_path, target };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            LIST_DRAFTS_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let response: ListDraftsResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    Ok(response.drafts)
}

async fn save_draft(
    client: &BcodeClient,
    repo_path: PathBuf,
    target: ReviewTarget,
    save: PendingDraftSave,
) -> Result<(), TuiError> {
    let request = SaveDraftRequest {
        repo_path,
        target,
        anchor: save.anchor.into(),
        body: save.body,
    };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            SAVE_DRAFT_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let _: SaveDraftResponse = serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    Ok(())
}

async fn delete_draft(
    client: &BcodeClient,
    repo_path: PathBuf,
    delete: PendingDraftDelete,
) -> Result<(), TuiError> {
    let Some(comment_id) = delete.comment.id else {
        return Ok(());
    };
    let request = DeleteDraftRequest {
        repo_path,
        comment_id,
    };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            DELETE_DRAFT_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let _: DeleteDraftResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    Ok(())
}

async fn update_draft(
    client: &BcodeClient,
    repo_path: PathBuf,
    update: PendingDraftUpdate,
) -> Result<(), TuiError> {
    let request = UpdateDraftRequest {
        repo_path,
        comment_id: update.comment_id,
        body: update.new_body,
    };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            UPDATE_DRAFT_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let _: UpdateDraftResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    Ok(())
}

async fn create_agent_session(
    client: &BcodeClient,
    repo_path: PathBuf,
    target: ReviewTarget,
    app: &ReviewApp,
    ask: PendingAgentSession,
) -> Result<SessionId, TuiError> {
    let session = client
        .create_session_in_working_directory(
            Some(format!("Review: {}", ask.anchor.path)),
            app.review.repo_root.clone(),
        )
        .await?;
    let prompt = app.agent_session_prompt(&ask);
    client
        .send_user_message(session.id, prompt, bcode_ipc::PromptPlacement::FollowUp)
        .await?;
    link_thread_session(client, repo_path, target, ask.anchor, session.id).await?;
    Ok(session.id)
}

async fn link_thread_session(
    client: &BcodeClient,
    repo_path: PathBuf,
    target: ReviewTarget,
    anchor: ReviewCommentAnchor,
    session_id: SessionId,
) -> Result<(), TuiError> {
    let request = LinkThreadSessionRequest {
        repo_path,
        target,
        anchor: anchor.into(),
        session_id: session_id.to_string(),
    };
    let payload = serde_json::to_vec(&request).map_err(TuiError::Json)?;
    let response = client
        .call_plugin_service(
            SERVICE_INTERFACE_ID.to_string(),
            LINK_THREAD_SESSION_OPERATION.to_string(),
            payload,
        )
        .await?;
    if let Some(error) = response.error {
        return Err(TuiError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let _: LinkThreadSessionResponse =
        serde_json::from_slice(&response.payload).map_err(TuiError::Json)?;
    Ok(())
}

fn handle_publish_event(app: &mut ReviewApp, event: &Event) -> bool {
    match event {
        Event::Key(stroke) => handle_publish_key(app, *stroke),
        Event::Paste(text) => app.insert_publish_option_text(text),
        Event::Resize(_) | Event::Focus(_) | Event::Tick => true,
        Event::Mouse(_) | Event::User(_) => false,
    }
}

fn handle_publish_key(app: &mut ReviewApp, stroke: KeyStroke) -> bool {
    if app.publish_options_active() {
        if stroke.key == KeyCode::Escape && stroke.modifiers.is_empty() {
            app.publish_state = None;
            return true;
        }
        if stroke.key == KeyCode::Enter && stroke.modifiers.is_empty() {
            return app.confirm_publish_selection();
        }
        if stroke.key == KeyCode::Tab && stroke.modifiers.is_empty() {
            return app.publish_down(1);
        }
        if stroke.key == KeyCode::Tab && stroke.modifiers.shift {
            return app.publish_up(1);
        }
        return app.edit_publish_option(stroke);
    }
    if !stroke.modifiers.is_empty() {
        return false;
    }
    match stroke.key {
        KeyCode::Escape => {
            app.publish_state = None;
            true
        }
        KeyCode::Char('j') | KeyCode::Down => app.publish_down(1),
        KeyCode::Char('k') | KeyCode::Up => app.publish_up(1),
        KeyCode::Enter => app.confirm_publish_selection(),
        _ => false,
    }
}

fn handle_event<W: Write>(
    app: &mut ReviewApp,
    terminal: &mut Terminal<&mut W>,
    event: &Event,
) -> bool {
    if app.prompt_state.is_some() {
        return handle_prompt_event(app, event);
    }
    if app.publish_state.is_some() {
        return handle_publish_event(app, event);
    }
    if app.comment_editor.is_some() {
        return handle_comment_editor_event(app, event);
    }
    match event {
        Event::Resize(size) => {
            terminal.resize(Rect::new(0, 0, size.width, size.height));
            true
        }
        Event::Key(stroke) => handle_key(app, *stroke),
        Event::Mouse(mouse) => handle_mouse(app, *mouse),
        Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick => true,
        Event::Paste(_) | Event::User(_) => false,
    }
}

fn handle_prompt_event(app: &mut ReviewApp, event: &Event) -> bool {
    match event {
        Event::Key(stroke) => handle_prompt_key(app, *stroke),
        Event::Paste(text) => {
            if let Some(prompt) = &mut app.prompt_state {
                prompt.buffer.insert_str(text);
                prompt.selected = 0;
                return true;
            }
            false
        }
        Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::Resize(_) => {
            true
        }
        Event::Mouse(_) | Event::User(_) => false,
    }
}

fn handle_prompt_key(app: &mut ReviewApp, stroke: KeyStroke) -> bool {
    if stroke.key == KeyCode::Escape && stroke.modifiers.is_empty() {
        app.cancel_prompt();
        return true;
    }
    if stroke.key == KeyCode::Enter && stroke.modifiers.is_empty() {
        app.submit_prompt();
        return true;
    }
    if matches!(stroke.key, KeyCode::Down | KeyCode::Char('j')) && stroke.modifiers.is_empty() {
        return app.move_prompt_selection_down();
    }
    if matches!(stroke.key, KeyCode::Up | KeyCode::Char('k')) && stroke.modifiers.is_empty() {
        return app.move_prompt_selection_up();
    }
    if let Some(prompt) = &mut app.prompt_state {
        let outcome = helpers::handle_default_text_key(
            &mut prompt.buffer,
            stroke,
            TextInputEnterBehavior::Submit,
        );
        if matches!(outcome, TextInputKeyOutcome::Edited) {
            prompt.selected = 0;
        }
        return matches!(
            outcome,
            TextInputKeyOutcome::Edited | TextInputKeyOutcome::Submitted
        );
    }
    false
}

fn handle_comment_editor_event(app: &mut ReviewApp, event: &Event) -> bool {
    match event {
        Event::Key(stroke) => handle_comment_editor_key(app, *stroke),
        Event::Paste(text) => {
            if let Some(editor) = &mut app.comment_editor {
                editor.buffer.insert_str(text);
                return true;
            }
            false
        }
        Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::Resize(_) => {
            true
        }
        Event::Mouse(_) | Event::User(_) => false,
    }
}

fn handle_comment_editor_key(app: &mut ReviewApp, stroke: KeyStroke) -> bool {
    if stroke.key == KeyCode::Escape && stroke.modifiers.is_empty() {
        app.comment_editor = None;
        app.status_message = Some("comment draft canceled".to_string());
        return true;
    }
    if stroke.key == KeyCode::Char('s') && stroke.modifiers.ctrl {
        app.save_comment_editor();
        return true;
    }
    if stroke.key == KeyCode::Enter && !stroke.modifiers.ctrl && !stroke.modifiers.alt {
        app.save_comment_editor();
        return true;
    }
    if let Some(editor) = &mut app.comment_editor {
        return matches!(
            helpers::handle_default_text_key(
                &mut editor.buffer,
                stroke,
                TextInputEnterBehavior::InsertNewline,
            ),
            TextInputKeyOutcome::Edited | TextInputKeyOutcome::Submitted
        );
    }
    false
}

fn handle_key(app: &mut ReviewApp, stroke: KeyStroke) -> bool {
    if !stroke.modifiers.is_empty() {
        if stroke.key == KeyCode::Char('p') && stroke.modifiers.ctrl {
            return app.open_file_picker();
        }
        return false;
    }
    match stroke.key {
        KeyCode::Char('q') => {
            app.should_exit = true;
            true
        }
        KeyCode::Escape => {
            let cleared = app.clear_range_selection();
            if !cleared {
                app.should_exit = true;
            }
            true
        }
        KeyCode::Char('b') => {
            app.sidebar_visible = !app.sidebar_visible;
            true
        }
        KeyCode::Char('m') => app.toggle_ux_mode(),
        KeyCode::Char('+') => app.add_selected_file_to_workspace(),
        KeyCode::Char('A') => app.open_add_source_prompt(),
        KeyCode::Char('-') => app.remove_selected_build_source(),
        KeyCode::Char('t') => app.toggle_sidebar_mode(),
        KeyCode::Char('f') => app.open_file_picker(),
        KeyCode::Char(':') => app.open_jump_to_line_prompt(),
        KeyCode::Char('/') => app.open_file_search_prompt(),
        KeyCode::Char('N') => app.search_previous_match(),
        KeyCode::Enter => {
            if app.sidebar_mode == ReviewSidebarMode::Repository
                && app.review.is_repository_review()
            {
                app.activate_selected_tree_row()
            } else {
                app.jump_to_selected_thread()
            }
        }
        KeyCode::Char('j') | KeyCode::Down => app.move_down(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_up(1),
        KeyCode::Char('g') => app.scroll_to_top(),
        KeyCode::Char('G') => app.scroll_to_bottom(),
        KeyCode::Char('n') => {
            if app.has_active_search() {
                app.search_next_match()
            } else {
                app.select_next_file()
            }
        }
        KeyCode::Right => app.expand_selected_tree_row(),
        KeyCode::Char('p') => app.select_previous_file(),
        KeyCode::Left => app.collapse_selected_tree_row(),
        KeyCode::Char('J') => app.select_next_hunk(),
        KeyCode::Char('K') => app.select_previous_hunk(),
        KeyCode::Char('v') => app.toggle_range_selection(),
        KeyCode::Char('c') => app.open_comment_editor(),
        KeyCode::Char('e') => app.open_latest_draft_editor(),
        KeyCode::Char('D') => app.delete_latest_draft_at_selection(),
        KeyCode::Char('x') => app.publish_review(),
        KeyCode::Char('a') => app.ask_bcode_about_selection(),
        KeyCode::Char('o') => app.open_linked_session_at_selection(),
        KeyCode::Char('?') => {
            app.help_visible = !app.help_visible;
            true
        }
        _ => false,
    }
}

fn handle_mouse(app: &mut ReviewApp, mouse: MouseEvent) -> bool {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if app.file_area_contains(mouse.position.x, mouse.position.y) {
                if app.sidebar_mode == ReviewSidebarMode::Threads {
                    app.select_previous_thread(3)
                } else {
                    app.scroll_files_up(3)
                }
            } else {
                app.scroll_up(3)
            }
        }
        MouseEventKind::ScrollDown => {
            if app.file_area_contains(mouse.position.x, mouse.position.y) {
                if app.sidebar_mode == ReviewSidebarMode::Threads {
                    app.select_next_thread(3)
                } else {
                    app.scroll_files_down(3)
                }
            } else {
                app.scroll_down(3)
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if app.sidebar_mode == ReviewSidebarMode::Threads {
                app.thread_index_at(mouse.position.x, mouse.position.y)
                    .is_some_and(|index| {
                        app.select_thread(index);
                        app.jump_to_selected_thread()
                    })
            } else if app.review.is_repository_review() {
                match app.file_tree_row_at(mouse.position.x, mouse.position.y) {
                    Some(ReviewFileTreeRow::Directory { path, .. }) => {
                        app.toggle_file_tree_directory(&path)
                    }
                    Some(ReviewFileTreeRow::File { index, .. }) => app.select_file(index),
                    None => false,
                }
            } else if let Some(index) = app.file_index_at(mouse.position.x, mouse.position.y) {
                app.select_file(index)
            } else if let Some(index) = app.diff_line_index_at(mouse.position.x, mouse.position.y) {
                app.select_diff_line(index)
            } else {
                false
            }
        }
        MouseEventKind::Down(MouseButton::Right | MouseButton::Middle | MouseButton::Other(_))
        | MouseEventKind::Up(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::Move
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ListReviewPublishersResponse {
    publishers: Vec<ReviewPublisherManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewPublisherManifest {
    /// Publisher id.
    pub id: String,
    /// Human-readable label.
    pub label: String,
    /// Human-readable description.
    pub description: String,
    /// Publisher capabilities.
    pub capabilities: ReviewPublisherCapabilities,
    /// Publisher option schema.
    pub options_schema: serde_json::Value,
    /// Optional external plugin route.
    #[serde(default)]
    pub route: Option<ReviewPublisherRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReviewPublisherRoute {
    /// Plugin id for external publisher.
    pub plugin_id: String,
    /// Service interface id.
    pub interface_id: String,
}

impl ReviewPublisherManifest {
    /// Return compact capability labels.
    #[must_use]
    pub fn capability_labels(&self) -> Vec<&'static str> {
        let mut labels = Vec::new();
        if self.capabilities.preview {
            labels.push("preview");
        }
        if self.capabilities.supports_threads {
            labels.push("threads");
        }
        if self.capabilities.supports_ranges {
            labels.push("ranges");
        }
        if self.capabilities.supports_inline_comments {
            labels.push("inline");
        }
        labels
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ReviewPublisherCapabilities {
    preview: bool,
    submit: bool,
    update_existing: bool,
    supports_threads: bool,
    supports_ranges: bool,
    supports_inline_comments: bool,
    supports_summary_comment: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct PublishReviewPreviewResponse {
    publisher_id: String,
    preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ReviewBundleRequest {
    repo_path: PathBuf,
    target: ReviewTarget,
    #[serde(default)]
    workspace: Option<ReviewWorkspace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PublishReviewRequest {
    repo_path: PathBuf,
    target: ReviewTarget,
    #[serde(default)]
    workspace: Option<ReviewWorkspace>,
    publisher_id: String,
    #[serde(default)]
    options: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PublishReviewResponse {
    publisher_id: String,
    submitted: bool,
    output: Option<String>,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct CreateReviewRequest {
    repo_path: PathBuf,
    target: ReviewTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReviewTarget {
    WorkingTreeUnstaged,
    IndexStaged,
    WorkingTreeAndIndex,
    LastCommit,
    CommitRange {
        base: String,
        head: String,
        #[serde(default)]
        merge_base: bool,
    },
    BranchCompare {
        base_branch: String,
        head_branch: String,
        #[serde(default)]
        merge_base: bool,
    },
    Repository,
}

fn target_from_workspace(workspace: &ReviewWorkspace) -> ReviewOpenTarget {
    workspace
        .sources
        .iter()
        .find(|source| source.included)
        .map_or(ReviewOpenTarget::Repository, |source| {
            target_from_source_kind(&source.kind)
        })
}

fn target_from_source_kind(kind: &ReviewSourceKind) -> ReviewOpenTarget {
    match kind {
        ReviewSourceKind::WorkingTreeUnstaged => ReviewOpenTarget::WorkingTreeUnstaged,
        ReviewSourceKind::IndexStaged => ReviewOpenTarget::IndexStaged,
        ReviewSourceKind::WorkingTreeAndIndex => ReviewOpenTarget::WorkingTreeAndIndex,
        ReviewSourceKind::LastCommit => ReviewOpenTarget::LastCommit,
        ReviewSourceKind::CommitRange {
            base,
            head,
            merge_base,
        } => ReviewOpenTarget::CommitRange {
            base: base.clone(),
            head: head.clone(),
            merge_base: *merge_base,
        },
        ReviewSourceKind::BranchCompare {
            base_branch,
            head_branch,
            merge_base,
        } => ReviewOpenTarget::BranchCompare {
            base_branch: base_branch.clone(),
            head_branch: head_branch.clone(),
            merge_base: *merge_base,
        },
        ReviewSourceKind::Commit { rev } => ReviewOpenTarget::CommitRange {
            base: format!("{rev}^"),
            head: rev.clone(),
            merge_base: false,
        },
        ReviewSourceKind::File { .. }
        | ReviewSourceKind::FileRange { .. }
        | ReviewSourceKind::Repository => ReviewOpenTarget::Repository,
    }
}

impl From<ReviewOpenTarget> for ReviewTarget {
    fn from(target: ReviewOpenTarget) -> Self {
        match target {
            ReviewOpenTarget::WorkingTreeUnstaged => Self::WorkingTreeUnstaged,
            ReviewOpenTarget::IndexStaged => Self::IndexStaged,
            ReviewOpenTarget::WorkingTreeAndIndex => Self::WorkingTreeAndIndex,
            ReviewOpenTarget::LastCommit => Self::LastCommit,
            ReviewOpenTarget::CommitRange {
                base,
                head,
                merge_base,
            } => Self::CommitRange {
                base,
                head,
                merge_base,
            },
            ReviewOpenTarget::BranchCompare {
                base_branch,
                head_branch,
                merge_base,
            } => Self::BranchCompare {
                base_branch,
                head_branch,
                merge_base,
            },
            ReviewOpenTarget::Repository => Self::Repository,
        }
    }
}

impl ReviewTarget {
    fn to_model(&self) -> bcode_code_review_models::ReviewTarget {
        match self {
            Self::WorkingTreeUnstaged => {
                bcode_code_review_models::ReviewTarget::WorkingTreeUnstaged
            }
            Self::IndexStaged => bcode_code_review_models::ReviewTarget::IndexStaged,
            Self::WorkingTreeAndIndex => {
                bcode_code_review_models::ReviewTarget::WorkingTreeAndIndex
            }
            Self::LastCommit => bcode_code_review_models::ReviewTarget::LastCommit,
            Self::CommitRange {
                base,
                head,
                merge_base,
            } => bcode_code_review_models::ReviewTarget::CommitRange {
                base: base.clone(),
                head: head.clone(),
                merge_base: *merge_base,
            },
            Self::BranchCompare {
                base_branch,
                head_branch,
                merge_base,
            } => bcode_code_review_models::ReviewTarget::BranchCompare {
                base_branch: base_branch.clone(),
                head_branch: head_branch.clone(),
                merge_base: *merge_base,
            },
            Self::Repository => bcode_code_review_models::ReviewTarget::Repository,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ListDraftsRequest {
    repo_path: PathBuf,
    target: ReviewTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct SaveDraftRequest {
    repo_path: PathBuf,
    target: ReviewTarget,
    anchor: DraftAnchor,
    body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct DeleteDraftRequest {
    repo_path: PathBuf,
    comment_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct DeleteDraftResponse {
    deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct UpdateDraftRequest {
    repo_path: PathBuf,
    comment_id: String,
    body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct UpdateDraftResponse {
    updated: bool,
    updated_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct LinkThreadSessionRequest {
    repo_path: PathBuf,
    target: ReviewTarget,
    anchor: DraftAnchor,
    session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct LinkThreadSessionResponse {
    thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DraftAnchor {
    file_path: String,
    diff_row: u64,
    old_line: Option<u32>,
    start_diff_row: Option<u64>,
    end_diff_row: Option<u64>,
    old_start: Option<u32>,
    old_end: Option<u32>,
    new_start: Option<u32>,
    new_end: Option<u32>,
    new_line: Option<u32>,
    line_kind: ReviewLineKind,
    #[serde(default)]
    is_file_anchor: bool,
    /// Surface id for normalized mixed-surface anchors.
    #[serde(default)]
    surface_id: Option<String>,
    /// Source id for normalized mixed-surface anchors.
    #[serde(default)]
    source_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct DraftComment {
    comment_id: String,
    thread_id: String,
    anchor: DraftAnchor,
    body: String,
    created_at_ms: u64,
    updated_at_ms: u64,
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ListDraftsResponse {
    drafts: Vec<DraftComment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct SaveDraftResponse {
    draft: DraftComment,
}

impl From<ReviewCommentAnchor> for DraftAnchor {
    fn from(anchor: ReviewCommentAnchor) -> Self {
        Self {
            file_path: anchor.path.clone(),
            diff_row: u64::try_from(anchor.diff_row).unwrap_or(u64::MAX),
            start_diff_row: Some(u64::try_from(anchor.start_diff_row()).unwrap_or(u64::MAX)),
            end_diff_row: Some(u64::try_from(anchor.end_diff_row()).unwrap_or(u64::MAX)),
            old_start: anchor.old_start,
            old_end: anchor.old_end,
            new_start: anchor.new_start,
            new_end: anchor.new_end,
            old_line: anchor.old_line,
            new_line: anchor.new_line,
            line_kind: anchor.line_kind,
            is_file_anchor: anchor.is_file_anchor,
            surface_id: anchor.surface_id,
            source_id: anchor.source_id,
        }
    }
}

/// Review interaction mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewUxMode {
    /// Build the review by adding/removing sources.
    Build,
    /// Review/comment/publish included and context files.
    Review,
}

/// Full review summary displayed by the TUI.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewSummary {
    /// Human-readable review title.
    pub title: String,
    /// Git repository root.
    pub repo_root: PathBuf,
    /// Changed files.
    pub files: Vec<ReviewFile>,
    /// Total additions.
    pub additions: u32,
    /// Total deletions.
    pub deletions: u32,
    /// Workspace that owns this review session.
    #[serde(default)]
    pub workspace: Option<ReviewWorkspace>,
    /// Materialized review surfaces corresponding to files.
    #[serde(default)]
    pub surfaces: Vec<ReviewSurface>,
}

impl ReviewSummary {
    /// Return true when this review is browsing repository files instead of a diff.
    #[must_use]
    pub fn is_repository_review(&self) -> bool {
        self.title == "Repository Review"
    }

    /// Return workspace, creating a transient workspace for legacy single-target reviews.
    #[must_use]
    pub fn workspace(&self) -> ReviewWorkspace {
        self.workspace.clone().unwrap_or_else(|| {
            let target = if self.is_repository_review() {
                ReviewTarget::Repository.to_model()
            } else {
                ReviewTarget::WorkingTreeAndIndex.to_model()
            };
            ReviewWorkspace::from_target(self.repo_root.clone(), target)
        })
    }

    /// Return normalized surfaces visible for this review.
    #[must_use]
    pub fn surfaces(&self) -> Vec<ReviewSurface> {
        if !self.surfaces.is_empty() {
            return self.surfaces.clone();
        }
        self.files
            .iter()
            .enumerate()
            .map(|(index, file)| ReviewSurface {
                id: format!("surface-{index}"),
                source_id: "source-1".to_string(),
                path: file.display_path().to_string(),
                kind: if self.is_repository_review() {
                    ReviewSurfaceKind::File
                } else {
                    ReviewSurfaceKind::Diff
                },
                file: None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RepositoryFileResponse {
    file_path: String,
    content: Option<String>,
    size_bytes: u64,
    #[serde(default)]
    mtime_ms: Option<u64>,
    is_binary: bool,
    #[serde(default)]
    unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedReviewFile {
    pub path: String,
    pub content: String,
    pub line_spans: Vec<(usize, usize)>,
    pub size_bytes: u64,
    pub mtime_ms: Option<u64>,
    pub is_binary: bool,
    pub unavailable_reason: Option<String>,
}

impl CachedReviewFile {
    #[must_use]
    fn from_response(response: RepositoryFileResponse) -> Self {
        let content = response.content.unwrap_or_default();
        let line_spans = line_spans(&content);
        Self {
            path: response.file_path,
            content,
            line_spans,
            size_bytes: response.size_bytes,
            mtime_ms: response.mtime_ms,
            is_binary: response.is_binary,
            unavailable_reason: response.unavailable_reason,
        }
    }

    #[must_use]
    pub fn line(&self, index: usize) -> Option<&str> {
        let (start, end) = *self.line_spans.get(index)?;
        self.content.get(start..end)
    }
}

fn line_spans(content: &str) -> Vec<(usize, usize)> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut start = 0;
    for line in content.split_inclusive('\n') {
        let end = start + line.trim_end_matches('\n').trim_end_matches('\r').len();
        spans.push((start, end));
        start += line.len();
    }
    if !content.ends_with('\n') && spans.is_empty() {
        spans.push((0, content.len()));
    }
    spans
}

#[derive(Debug, Clone, Default)]
pub struct ReviewFileCache {
    entries: BTreeMap<String, CachedReviewFile>,
    lru: Vec<String>,
    total_bytes: usize,
}

impl ReviewFileCache {
    const MAX_FILES: usize = 128;
    const MAX_BYTES: usize = 32 * 1024 * 1024;

    #[must_use]
    pub fn get(&self, path: &str) -> Option<&CachedReviewFile> {
        self.entries.get(path)
    }

    pub fn insert(&mut self, file: CachedReviewFile) {
        let path = file.path.clone();
        if let Some(existing) = self.entries.remove(&path) {
            self.total_bytes = self.total_bytes.saturating_sub(existing.content.len());
        }
        self.total_bytes = self.total_bytes.saturating_add(file.content.len());
        self.entries.insert(path.clone(), file);
        self.lru.retain(|entry| entry != &path);
        self.lru.push(path);
        self.evict();
    }

    fn evict(&mut self) {
        while self.entries.len() > Self::MAX_FILES || self.total_bytes > Self::MAX_BYTES {
            let Some(path) = self.lru.first().cloned() else {
                break;
            };
            self.lru.remove(0);
            if let Some(existing) = self.entries.remove(&path) {
                self.total_bytes = self.total_bytes.saturating_sub(existing.content.len());
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchDirection {
    Next,
    Previous,
}

fn find_file_match(
    file: &CachedReviewFile,
    query: &str,
    selected_line: usize,
    direction: SearchDirection,
) -> Option<usize> {
    let len = file.line_spans.len();
    if len == 0 {
        return None;
    }
    match direction {
        SearchDirection::Next => {
            let start = selected_line.saturating_add(1).min(len);
            (start..len)
                .chain(0..start)
                .find(|index| file.line(*index).is_some_and(|line| line.contains(query)))
        }
        SearchDirection::Previous => {
            let start = selected_line.min(len.saturating_sub(1));
            (0..=start)
                .rev()
                .chain((start.saturating_add(1)..len).rev())
                .find(|index| file.line(*index).is_some_and(|line| line.contains(query)))
        }
    }
}

/// Sidebar file-tree row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewFileTreeRow {
    /// Directory row.
    Directory {
        /// Directory path.
        path: PathBuf,
        /// Nesting depth.
        depth: usize,
    },
    /// File row.
    File {
        /// File index in review files.
        index: usize,
        /// Nesting depth.
        depth: usize,
    },
}

/// Changed file displayed by the TUI.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewFile {
    /// Old path.
    pub old_path: Option<String>,
    /// New path.
    pub new_path: Option<String>,
    /// File status.
    pub status: ReviewFileStatus,
    /// Additions.
    pub additions: u32,
    /// Deletions.
    pub deletions: u32,
    /// Hunks.
    pub hunks: Vec<ReviewHunk>,
    /// Binary marker.
    pub is_binary: bool,
}

impl ReviewFile {
    fn from_model(file: bcode_code_review_models::ReviewFile) -> Self {
        Self {
            old_path: file.old_path,
            new_path: file.new_path,
            status: ReviewFileStatus::from_model(file.status),
            additions: file.additions,
            deletions: file.deletions,
            hunks: file.hunks.into_iter().map(ReviewHunk::from_model).collect(),
            is_binary: file.is_binary,
        }
    }

    /// Return the display path.
    #[must_use]
    pub fn display_path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("<unknown>")
    }
}

/// Review file status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFileStatus {
    /// Modified file.
    Modified,
    /// Added file.
    Added,
    /// Deleted file.
    Deleted,
    /// Renamed file.
    Renamed,
    /// Copied file.
    Copied,
    /// Unknown status.
    Unknown,
}

impl ReviewFileStatus {
    const fn from_model(status: bcode_code_review_models::ReviewFileStatus) -> Self {
        match status {
            bcode_code_review_models::ReviewFileStatus::Modified => Self::Modified,
            bcode_code_review_models::ReviewFileStatus::Added => Self::Added,
            bcode_code_review_models::ReviewFileStatus::Deleted => Self::Deleted,
            bcode_code_review_models::ReviewFileStatus::Renamed => Self::Renamed,
            bcode_code_review_models::ReviewFileStatus::Copied => Self::Copied,
            bcode_code_review_models::ReviewFileStatus::Unknown => Self::Unknown,
        }
    }

    /// Return a compact status label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Modified => "M",
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Copied => "C",
            Self::Unknown => "?",
        }
    }
}

/// Review hunk.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewHunk {
    /// Old start line.
    pub old_start: u32,
    /// Old line count.
    pub old_count: u32,
    /// New start line.
    pub new_start: u32,
    /// New line count.
    pub new_count: u32,
    /// Optional heading.
    pub heading: Option<String>,
    /// Lines.
    pub lines: Vec<ReviewLine>,
}

impl ReviewHunk {
    fn from_model(hunk: bcode_code_review_models::ReviewHunk) -> Self {
        Self {
            old_start: hunk.old_start,
            old_count: hunk.old_count,
            new_start: hunk.new_start,
            new_count: hunk.new_count,
            heading: hunk.heading,
            lines: hunk.lines.into_iter().map(ReviewLine::from_model).collect(),
        }
    }
}

/// Review diff line.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewLine {
    /// Line kind.
    pub kind: ReviewLineKind,
    /// Old line number.
    pub old_line: Option<u32>,
    /// New line number.
    pub new_line: Option<u32>,
    /// Content without diff marker.
    pub content: String,
}

impl ReviewLine {
    fn from_model(line: bcode_code_review_models::ReviewLine) -> Self {
        Self {
            kind: ReviewLineKind::from_model(line.kind),
            old_line: line.old_line,
            new_line: line.new_line,
            content: line.content,
        }
    }
}

/// Review diff line kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewLineKind {
    /// Context line.
    Context,
    /// Added line.
    Added,
    /// Removed line.
    Removed,
}

impl ReviewLineKind {
    const fn from_model(kind: bcode_code_review_models::ReviewLineKind) -> Self {
        match kind {
            bcode_code_review_models::ReviewLineKind::Context => Self::Context,
            bcode_code_review_models::ReviewLineKind::Added => Self::Added,
            bcode_code_review_models::ReviewLineKind::Removed => Self::Removed,
        }
    }
}

/// Draft comment line anchor.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReviewCommentAnchor {
    /// File index in the current review.
    pub file_index: usize,
    /// Display path for the commented file.
    pub path: String,
    /// Rendered diff row in the selected file.
    pub diff_row: usize,
    /// Rendered diff range end row in the selected file.
    pub end_diff_row: Option<usize>,
    /// Old line number, when present.
    pub old_line: Option<u32>,
    /// New line number, when present.
    pub new_line: Option<u32>,
    /// Old range start line, when present.
    pub old_start: Option<u32>,
    /// Old range end line, when present.
    pub old_end: Option<u32>,
    /// New range start line, when present.
    pub new_start: Option<u32>,
    /// New range end line, when present.
    pub new_end: Option<u32>,
    /// Anchored diff line kind.
    pub line_kind: ReviewLineKind,
    /// Whether this comment points at a file surface line rather than a diff row.
    pub is_file_anchor: bool,
    /// Surface id for normalized mixed-surface anchors.
    pub surface_id: Option<String>,
    /// Source id for normalized mixed-surface anchors.
    pub source_id: Option<String>,
}

impl ReviewCommentAnchor {
    /// Return the first rendered diff row for this anchor.
    #[must_use]
    pub const fn start_diff_row(&self) -> usize {
        self.diff_row
    }

    /// Return the final rendered diff row for this anchor.
    #[must_use]
    pub const fn end_diff_row(&self) -> usize {
        match self.end_diff_row {
            Some(row) => row,
            None => self.diff_row,
        }
    }
}

/// Review draft comment metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDraftComment {
    /// Persisted comment id, when saved in the plugin database.
    pub id: Option<String>,
    /// Draft body.
    pub body: String,
    /// Whether the draft is known to be persisted.
    pub persisted: bool,
    /// Creation timestamp in milliseconds since Unix epoch.
    pub created_at_ms: Option<u64>,
    /// Last update timestamp in milliseconds since Unix epoch.
    pub updated_at_ms: Option<u64>,
    /// Linked Bcode session id.
    pub session_id: Option<String>,
}

/// Pending draft comment persistence request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDraftSave {
    /// Saved anchor.
    pub anchor: ReviewCommentAnchor,
    /// Saved body.
    pub body: String,
}

/// Pending draft comment delete request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDraftDelete {
    /// Deleted anchor.
    pub anchor: ReviewCommentAnchor,
    /// Deleted comment.
    pub comment: ReviewDraftComment,
}

/// Pending draft comment update request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDraftUpdate {
    /// Edited anchor.
    pub anchor: ReviewCommentAnchor,
    /// Persisted comment id.
    pub comment_id: String,
    /// Previous body for failure restore.
    pub previous_body: String,
    /// New body.
    pub new_body: String,
}

/// Pending Bcode agent session request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAgentSession {
    /// Thread anchor.
    pub anchor: ReviewCommentAnchor,
    /// Optional selected draft body.
    pub draft_body: Option<String>,
}

/// Draft comment editor mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewCommentEditorMode {
    /// Creating a new draft.
    Create,
    /// Editing an existing persisted draft.
    Edit {
        /// Existing comment id.
        comment_id: String,
        /// Previous body for failure restore.
        previous_body: String,
    },
}

/// Active draft comment editor.
#[derive(Debug, Clone)]
pub struct ReviewCommentEditor {
    /// Anchor being commented on.
    pub anchor: ReviewCommentAnchor,
    /// Editable comment buffer.
    pub buffer: TextEditBuffer,
    /// Editor mode.
    pub mode: ReviewCommentEditorMode,
}

impl ReviewCommentEditor {
    /// Create an editor for an anchor.
    #[must_use]
    pub const fn new(anchor: ReviewCommentAnchor) -> Self {
        Self {
            anchor,
            buffer: TextEditBuffer::new(),
            mode: ReviewCommentEditorMode::Create,
        }
    }

    /// Create an editor for updating an existing draft.
    #[must_use]
    pub fn edit(anchor: ReviewCommentAnchor, comment_id: String, body: String) -> Self {
        Self {
            anchor,
            buffer: TextEditBuffer::from_text(&body),
            mode: ReviewCommentEditorMode::Edit {
                comment_id,
                previous_body: body,
            },
        }
    }
}

/// Review sidebar mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewSidebarMode {
    /// Included review workspace sources/sidebar.
    Included,
    /// Full repository file browser.
    Repository,
    /// Review thread list sidebar.
    Threads,
    /// Review source list sidebar.
    Sources,
}

impl ReviewSidebarMode {
    /// Return a user-facing sidebar label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Included => "included",
            Self::Repository => "repo",
            Self::Threads => "threads",
            Self::Sources => "sources",
        }
    }
}

/// Review thread row summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewThreadSummary {
    /// Thread anchor.
    pub anchor: ReviewCommentAnchor,
    /// Number of draft comments.
    pub draft_count: usize,
    /// Latest draft body.
    pub latest_body: String,
    /// Linked Bcode session id, when present.
    pub session_id: Option<String>,
}

/// Pending publish request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingPublishRequest {
    /// List available publishers.
    ListPublishers,
    /// Preview a publisher output.
    Preview {
        /// Publisher id.
        publisher_id: String,
        /// Publisher options.
        options: Vec<ReviewPublishOption>,
    },
    /// Submit publisher output.
    Submit {
        /// Publisher id.
        publisher_id: String,
        /// Publisher options.
        options: Vec<ReviewPublishOption>,
    },
}

/// Publish option field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewPublishOption {
    /// Option name.
    pub name: String,
    /// Option label.
    pub label: String,
    /// Option value.
    pub value: String,
}

/// Active review prompt kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewPromptKind {
    /// Fuzzy file picker.
    FilePicker,
    /// Jump to a line number.
    JumpToLine,
    /// Search within the current file.
    FileSearch,
    /// Add a source to the workspace.
    AddSourceKind,
    /// Add a commit source to the workspace.
    AddCommitSource,
    /// Add a commit range source to the workspace.
    AddCommitRangeSource,
    /// Add a file source to the workspace.
    AddFileSource,
    /// Add a file range source to the workspace.
    AddFileRangeSource,
}

/// Active one-line prompt state.
#[derive(Debug, Clone)]
pub struct ReviewPromptState {
    /// Prompt kind.
    pub kind: ReviewPromptKind,
    /// Editable prompt buffer.
    pub buffer: TextEditBuffer,
    /// Selected match index.
    pub selected: usize,
}

impl ReviewPromptState {
    /// Create a prompt.
    #[must_use]
    pub const fn new(kind: ReviewPromptKind) -> Self {
        Self {
            kind,
            buffer: TextEditBuffer::new(),
            selected: 0,
        }
    }
}

/// Publish modal state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewPublishState {
    /// Publisher picker.
    Picker,
    /// Options editor.
    Options {
        /// Publisher id.
        publisher_id: String,
        /// Options.
        options: Vec<ReviewPublishOption>,
        /// Selected option index.
        selected: usize,
    },
    /// Preview content.
    Preview {
        /// Publisher id.
        publisher_id: String,
        /// Publisher options.
        options: Vec<ReviewPublishOption>,
        /// Preview text.
        preview: String,
        /// Top visible preview line.
        scroll: usize,
    },
}

/// Stateful review app model.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct ReviewApp {
    /// Workspace that owns this review session.
    pub workspace: ReviewWorkspace,
    /// Review data.
    pub review: ReviewSummary,
    /// Current review UX mode.
    pub ux_mode: ReviewUxMode,
    /// Selected file index.
    pub selected_file: usize,
    /// Top visible file row.
    pub file_scroll: usize,
    /// Top visible rendered diff row.
    pub diff_scroll: usize,
    /// Selected rendered diff row.
    pub selected_diff_line: usize,
    /// Whether the file sidebar is visible.
    pub sidebar_visible: bool,
    /// Active sidebar mode.
    pub sidebar_mode: ReviewSidebarMode,
    /// Selected review thread index.
    pub selected_thread: usize,
    /// Top visible review thread row.
    pub thread_scroll: usize,
    /// Whether help is visible.
    pub help_visible: bool,
    /// Whether to exit.
    pub should_exit: bool,
    /// Last transient status message.
    pub status_message: Option<String>,
    /// Draft comments keyed by anchor.
    pub draft_comments: BTreeMap<ReviewCommentAnchor, Vec<ReviewDraftComment>>,
    /// Active draft editor, if open.
    pub comment_editor: Option<ReviewCommentEditor>,
    /// Draft comment awaiting persistence.
    pub pending_draft_save: Option<PendingDraftSave>,
    /// Draft comment awaiting deletion.
    pub pending_draft_delete: Option<PendingDraftDelete>,
    /// Draft comment awaiting update.
    pub pending_draft_update: Option<PendingDraftUpdate>,
    /// Pending publish request.
    pub pending_publish_request: Option<PendingPublishRequest>,
    /// Available publishers.
    pub publishers: Vec<ReviewPublisherManifest>,
    /// Repository file cache for read-only file browsing.
    pub file_cache: ReviewFileCache,
    /// Repository file path awaiting lazy load.
    pub pending_file_load: Option<String>,
    /// Selected publisher index.
    pub selected_publisher: usize,
    /// Active publish UI state.
    pub publish_state: Option<ReviewPublishState>,
    /// Active one-line prompt, if any.
    pub prompt_state: Option<ReviewPromptState>,
    /// Last current-file search query.
    pub last_search_query: Option<String>,
    /// Expanded repository sidebar directories.
    pub expanded_dirs: BTreeSet<PathBuf>,
    /// Selected repository file-tree row.
    pub selected_tree_row: usize,
    /// Active build-mode row.
    pub selected_build_row: usize,
    /// Whether workspace changes should be persisted.
    pub pending_workspace_save: bool,
    /// Whether workspace content should be rematerialized.
    pub pending_workspace_reload: bool,
    /// Review thread awaiting Bcode session creation.
    pub pending_agent_session: Option<PendingAgentSession>,
    /// Active range selection start row, if any.
    pub range_selection_start: Option<usize>,
    /// Session id to open after leaving review mode.
    pub session_to_open: Option<SessionId>,
    last_file_area: Option<Rect>,
    last_diff_area: Option<Rect>,
}

impl ReviewApp {
    /// Create a new review app.
    #[must_use]
    pub fn new(review: ReviewSummary) -> Self {
        let workspace = review.workspace();
        Self {
            workspace,
            review,
            ux_mode: ReviewUxMode::Review,
            selected_file: 0,
            file_scroll: 0,
            diff_scroll: 0,
            selected_diff_line: 0,
            sidebar_visible: true,
            sidebar_mode: ReviewSidebarMode::Included,
            selected_thread: 0,
            thread_scroll: 0,
            help_visible: false,
            should_exit: false,
            status_message: None,
            draft_comments: BTreeMap::new(),
            comment_editor: None,
            pending_draft_save: None,
            pending_draft_delete: None,
            pending_draft_update: None,
            pending_publish_request: None,
            publishers: Vec::new(),
            file_cache: ReviewFileCache::default(),
            pending_file_load: None,
            selected_publisher: 0,
            publish_state: None,
            prompt_state: None,
            last_search_query: None,
            expanded_dirs: BTreeSet::new(),
            selected_tree_row: 0,
            selected_build_row: 0,
            pending_workspace_save: false,
            pending_workspace_reload: false,
            pending_agent_session: None,
            range_selection_start: None,
            session_to_open: None,
            last_file_area: None,
            last_diff_area: None,
        }
    }

    /// Toggle between build and review UX modes.
    pub fn toggle_ux_mode(&mut self) -> bool {
        self.ux_mode = match self.ux_mode {
            ReviewUxMode::Build => ReviewUxMode::Review,
            ReviewUxMode::Review => ReviewUxMode::Build,
        };
        self.status_message = Some(match self.ux_mode {
            ReviewUxMode::Build => "build mode: assemble review sources".to_string(),
            ReviewUxMode::Review => "review mode: comment, ask, publish".to_string(),
        });
        true
    }

    /// Open add-source prompt appropriate for the selected build row.
    pub fn open_add_source_prompt(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            self.status_message = Some("switch to build mode with m to add sources".to_string());
            return true;
        }
        self.prompt_state = Some(ReviewPromptState::new(ReviewPromptKind::AddSourceKind));
        self.status_message = Some(
            "add source kind: file, range, commit, file-range, staged, unstaged, working-tree"
                .to_string(),
        );
        true
    }

    /// Open fuzzy file picker prompt.
    pub fn open_file_picker(&mut self) -> bool {
        self.prompt_state = Some(ReviewPromptState::new(ReviewPromptKind::FilePicker));
        self.status_message = Some("file picker: type path, enter open, esc cancel".to_string());
        true
    }

    /// Open jump-to-line prompt.
    pub fn open_jump_to_line_prompt(&mut self) -> bool {
        self.prompt_state = Some(ReviewPromptState::new(ReviewPromptKind::JumpToLine));
        self.status_message = Some("jump to line".to_string());
        true
    }

    /// Open current-file search prompt.
    pub fn open_file_search_prompt(&mut self) -> bool {
        self.prompt_state = Some(ReviewPromptState::new(ReviewPromptKind::FileSearch));
        self.status_message = Some("search current file".to_string());
        true
    }

    /// Cancel active prompt.
    pub fn cancel_prompt(&mut self) {
        self.prompt_state = None;
        self.status_message = Some("prompt canceled".to_string());
    }

    /// Submit active prompt.
    pub fn submit_prompt(&mut self) -> bool {
        let Some(prompt) = self.prompt_state.take() else {
            return false;
        };
        let text = prompt.buffer.text().trim().to_string();
        match prompt.kind {
            ReviewPromptKind::FilePicker => self.submit_file_picker(&text, prompt.selected),
            ReviewPromptKind::JumpToLine => self.submit_jump_to_line(&text),
            ReviewPromptKind::FileSearch => self.submit_file_search(&text),
            ReviewPromptKind::AddSourceKind => self.submit_add_source_kind(&text),
            ReviewPromptKind::AddCommitSource => self.submit_add_commit_source(&text),
            ReviewPromptKind::AddCommitRangeSource => self.submit_add_commit_range_source(&text),
            ReviewPromptKind::AddFileSource => self.submit_add_file_source(&text),
            ReviewPromptKind::AddFileRangeSource => self.submit_add_file_range_source(&text),
        }
    }

    fn submit_add_source_kind(&mut self, text: &str) -> bool {
        match text.trim().to_ascii_lowercase().as_str() {
            "file" | "f" => {
                self.prompt_state = Some(ReviewPromptState::new(ReviewPromptKind::AddFileSource));
                self.status_message = Some(
                    "add file source: enter repo path, or empty for selected file".to_string(),
                );
            }
            "file-range" | "range-file" | "fr" => {
                self.prompt_state =
                    Some(ReviewPromptState::new(ReviewPromptKind::AddFileRangeSource));
                self.status_message = Some("add file range: path:start-end".to_string());
            }
            "commit" | "c" => {
                self.prompt_state = Some(ReviewPromptState::new(ReviewPromptKind::AddCommitSource));
                self.status_message = Some("add commit: enter revision".to_string());
            }
            "range" | "commit-range" | "r" => {
                self.prompt_state = Some(ReviewPromptState::new(
                    ReviewPromptKind::AddCommitRangeSource,
                ));
                self.status_message =
                    Some("add commit range: base..head or base...head".to_string());
            }
            "staged" | "index" => {
                return self.push_workspace_source(ReviewSourceKind::IndexStaged);
            }
            "unstaged" => {
                return self.push_workspace_source(ReviewSourceKind::WorkingTreeUnstaged);
            }
            "working-tree" | "worktree" | "working" | "all" => {
                return self.push_workspace_source(ReviewSourceKind::WorkingTreeAndIndex);
            }
            "last" | "last-commit" => {
                return self.push_workspace_source(ReviewSourceKind::LastCommit);
            }
            "repo" | "repository" => {
                return self.push_workspace_source(ReviewSourceKind::Repository);
            }
            _ => {
                self.status_message = Some(
                    "unknown source kind; use file, file-range, commit, range, staged, unstaged, working-tree".to_string(),
                );
            }
        }
        true
    }

    fn submit_add_commit_source(&mut self, text: &str) -> bool {
        if text.is_empty() {
            self.status_message = Some("enter a commit revision".to_string());
            return true;
        }
        self.push_workspace_source(ReviewSourceKind::Commit {
            rev: text.to_string(),
        })
    }

    fn submit_add_commit_range_source(&mut self, text: &str) -> bool {
        let Some((base, head)) = text.split_once("...").or_else(|| text.split_once("..")) else {
            self.status_message = Some("enter range as base..head or base...head".to_string());
            return true;
        };
        let merge_base = text.contains("...");
        self.push_workspace_source(ReviewSourceKind::CommitRange {
            base: base.trim().to_string(),
            head: head.trim().to_string(),
            merge_base,
        })
    }

    fn submit_add_file_source(&mut self, text: &str) -> bool {
        if text.is_empty() {
            return self.add_selected_file_to_workspace();
        }
        self.push_workspace_source(ReviewSourceKind::File {
            path: text.to_string(),
        })
    }

    fn submit_add_file_range_source(&mut self, text: &str) -> bool {
        let Some((path, range)) = text.rsplit_once(':') else {
            self.status_message = Some("enter file range as path:start-end".to_string());
            return true;
        };
        let Some((start, end)) = range.split_once('-') else {
            self.status_message = Some("enter file range as path:start-end".to_string());
            return true;
        };
        let (Ok(start), Ok(end)) = (start.parse::<u32>(), end.parse::<u32>()) else {
            self.status_message = Some("file range line numbers must be numeric".to_string());
            return true;
        };
        self.push_workspace_source(ReviewSourceKind::FileRange {
            path: path.to_string(),
            start,
            end,
        })
    }

    fn push_workspace_source(&mut self, kind: ReviewSourceKind) -> bool {
        if self
            .workspace
            .sources
            .iter()
            .any(|source| source.kind == kind)
        {
            self.status_message = Some(format!("{} is already included", kind.label()));
            return true;
        }
        let label = kind.label();
        let source_id = format!("source-{}", self.workspace.sources.len().saturating_add(1));
        self.workspace.sources.push(ReviewSource {
            id: source_id,
            kind,
            label: label.clone(),
            included: true,
        });
        self.pending_workspace_save = true;
        self.pending_workspace_reload = true;
        self.status_message = Some(format!("added {label}"));
        true
    }

    fn submit_file_picker(&mut self, query: &str, selected: usize) -> bool {
        let matches = self.file_picker_matches(query);
        let Some(index) = matches
            .get(selected)
            .copied()
            .or_else(|| matches.first().copied())
        else {
            self.status_message = Some(format!("no file matches `{query}`"));
            return true;
        };
        self.select_file(index);
        self.status_message = Some(format!(
            "opened {}",
            self.review.files[index].display_path()
        ));
        true
    }

    fn submit_jump_to_line(&mut self, text: &str) -> bool {
        let Ok(line) = text.parse::<usize>() else {
            self.status_message = Some(format!("invalid line number `{text}`"));
            return true;
        };
        self.select_diff_line(line.saturating_sub(1));
        self.status_message = Some(format!("jumped to line {line}"));
        true
    }

    fn submit_file_search(&mut self, query: &str) -> bool {
        if query.is_empty() {
            self.status_message = Some("empty search query".to_string());
            return true;
        }
        let Some(file) = self.selected_file_data() else {
            return false;
        };
        let Some(cached) = self.file_cache.get(file.display_path()) else {
            self.status_message = Some("current file is not loaded".to_string());
            return true;
        };
        let next = find_file_match(
            cached,
            query,
            self.selected_diff_line,
            SearchDirection::Next,
        );
        match next {
            Some(index) => {
                self.last_search_query = Some(query.to_string());
                self.select_diff_line(index);
                self.status_message = Some(format!("found `{query}`"));
            }
            None => self.status_message = Some(format!("no match for `{query}`")),
        }
        true
    }

    /// Return true when file search has an active query.
    #[must_use]
    pub const fn has_active_search(&self) -> bool {
        self.last_search_query.is_some()
    }

    /// Jump to next current-file search match.
    pub fn search_next_match(&mut self) -> bool {
        self.search_match(SearchDirection::Next)
    }

    /// Jump to previous current-file search match.
    pub fn search_previous_match(&mut self) -> bool {
        self.search_match(SearchDirection::Previous)
    }

    fn search_match(&mut self, direction: SearchDirection) -> bool {
        let Some(query) = self.last_search_query.clone() else {
            self.status_message = Some("no active search; press / first".to_string());
            return true;
        };
        let Some(file) = self.selected_file_data() else {
            return false;
        };
        let Some(cached) = self.file_cache.get(file.display_path()) else {
            self.status_message = Some("current file is not loaded".to_string());
            return true;
        };
        match find_file_match(cached, &query, self.selected_diff_line, direction) {
            Some(index) => {
                self.select_diff_line(index);
                self.status_message = Some(format!("found `{query}`"));
            }
            None => self.status_message = Some(format!("no match for `{query}`")),
        }
        true
    }

    /// Move prompt selected row down.
    pub fn move_prompt_selection_down(&mut self) -> bool {
        let Some(prompt) = &self.prompt_state else {
            return false;
        };
        if prompt.kind != ReviewPromptKind::FilePicker {
            return false;
        }
        let max = self
            .file_picker_matches(prompt.buffer.text())
            .len()
            .saturating_sub(1);
        let Some(prompt) = &mut self.prompt_state else {
            return false;
        };
        prompt.selected = prompt.selected.saturating_add(1).min(max);
        true
    }

    /// Move prompt selected row up.
    pub fn move_prompt_selection_up(&mut self) -> bool {
        let Some(prompt) = &mut self.prompt_state else {
            return false;
        };
        if prompt.kind != ReviewPromptKind::FilePicker {
            return false;
        }
        prompt.selected = prompt.selected.saturating_sub(1);
        true
    }

    /// Return file picker matches for a query.
    #[must_use]
    pub fn file_picker_matches(&self, query: &str) -> Vec<usize> {
        let query = query.to_lowercase();
        self.review
            .files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| {
                let path = file.display_path().to_lowercase();
                (query.is_empty() || path.contains(&query)).then_some(index)
            })
            .take(12)
            .collect()
    }

    /// Return visible file-tree rows for repository review.
    #[must_use]
    pub fn file_tree_rows(&self) -> Vec<ReviewFileTreeRow> {
        let mut rows = Vec::new();
        self.push_tree_rows(Path::new(""), 0, &mut rows);
        rows
    }

    fn push_tree_rows(&self, prefix: &Path, depth: usize, rows: &mut Vec<ReviewFileTreeRow>) {
        let mut dirs = BTreeSet::new();
        let mut files = Vec::new();
        for (index, file) in self.review.files.iter().enumerate() {
            let path = Path::new(file.display_path());
            let rest = if prefix.as_os_str().is_empty() {
                path
            } else if let Ok(rest) = path.strip_prefix(prefix) {
                rest
            } else {
                continue;
            };
            let mut components = rest.components();
            let Some(first) = components.next() else {
                continue;
            };
            if components.next().is_some() {
                dirs.insert(prefix.join(first.as_os_str()));
            } else {
                files.push(index);
            }
        }
        for dir in dirs {
            rows.push(ReviewFileTreeRow::Directory {
                path: dir.clone(),
                depth,
            });
            if self.expanded_dirs.contains(&dir) {
                self.push_tree_rows(&dir, depth.saturating_add(1), rows);
            }
        }
        for index in files {
            rows.push(ReviewFileTreeRow::File { index, depth });
        }
    }

    /// Activate the selected repository tree row.
    pub fn activate_selected_tree_row(&mut self) -> bool {
        let rows = self.file_tree_rows();
        match rows.get(self.selected_tree_row).cloned() {
            Some(ReviewFileTreeRow::Directory { path, .. }) => {
                self.toggle_file_tree_directory(&path)
            }
            Some(ReviewFileTreeRow::File { index, .. }) => self.select_file(index),
            None => false,
        }
    }

    /// Toggle a directory in the repository sidebar.
    pub fn toggle_file_tree_directory(&mut self, path: &Path) -> bool {
        if self.expanded_dirs.remove(path) {
            return true;
        }
        self.expanded_dirs.insert(path.to_path_buf());
        true
    }

    /// Store the current file hit area.
    pub const fn set_file_area(&mut self, area: Option<Rect>) {
        self.last_file_area = area;
    }

    /// Store the current diff hit area.
    pub const fn set_diff_area(&mut self, area: Rect) {
        self.last_diff_area = Some(area);
    }

    /// Return currently selected surface.
    #[must_use]
    pub fn selected_surface(&self) -> Option<ReviewSurface> {
        self.review.surfaces().get(self.selected_file).cloned()
    }

    fn selected_surface_ids(&self) -> (Option<String>, Option<String>) {
        self.selected_surface().map_or((None, None), |surface| {
            (Some(surface.id), Some(surface.source_id))
        })
    }

    /// Return currently selected file.
    #[must_use]
    pub fn selected_file_data(&self) -> Option<&ReviewFile> {
        self.review.files.get(self.selected_file)
    }

    /// Replace review content after rematerialization.
    pub fn replace_review(&mut self, review: ReviewSummary) {
        self.review = review;
        self.workspace = self.review.workspace();
        self.selected_file = self
            .selected_file
            .min(self.review.files.len().saturating_sub(1));
        self.file_scroll = self.file_scroll.min(self.selected_file);
        self.diff_scroll = 0;
        self.selected_diff_line = 0;
        self.queue_selected_file_load();
    }

    /// Toggle sidebar between included, repository, threads, and sources.
    pub fn toggle_sidebar_mode(&mut self) -> bool {
        self.sidebar_mode = match self.sidebar_mode {
            ReviewSidebarMode::Included => ReviewSidebarMode::Repository,
            ReviewSidebarMode::Repository => ReviewSidebarMode::Threads,
            ReviewSidebarMode::Threads => ReviewSidebarMode::Sources,
            ReviewSidebarMode::Sources => ReviewSidebarMode::Included,
        };
        self.sidebar_visible = true;
        self.status_message = Some(format!("sidebar: {}", self.sidebar_mode.label()));
        true
    }

    /// Move the active selection down.
    pub fn move_down(&mut self, rows: usize) -> bool {
        if self.ux_mode == ReviewUxMode::Build {
            self.select_next_build_row(rows)
        } else if self.sidebar_mode == ReviewSidebarMode::Threads && self.sidebar_visible {
            self.select_next_thread(rows)
        } else if self.review.is_repository_review()
            && self.sidebar_mode == ReviewSidebarMode::Repository
            && self.sidebar_visible
        {
            self.select_next_tree_row(rows)
        } else {
            self.scroll_down(rows)
        }
    }

    /// Move the active selection up.
    pub fn move_up(&mut self, rows: usize) -> bool {
        if self.ux_mode == ReviewUxMode::Build {
            self.select_previous_build_row(rows)
        } else if self.sidebar_mode == ReviewSidebarMode::Threads && self.sidebar_visible {
            self.select_previous_thread(rows)
        } else if self.review.is_repository_review()
            && self.sidebar_mode == ReviewSidebarMode::Repository
            && self.sidebar_visible
        {
            self.select_previous_tree_row(rows)
        } else {
            self.scroll_up(rows)
        }
    }

    /// Return number of rows in build mode.
    #[must_use]
    pub const fn build_row_count(&self) -> usize {
        self.workspace
            .sources
            .len()
            .saturating_add(self.review.files.len())
    }

    /// Select next build row.
    pub fn select_next_build_row(&mut self, rows: usize) -> bool {
        let max = self.build_row_count().saturating_sub(1);
        let next = self.selected_build_row.saturating_add(rows).min(max);
        if next == self.selected_build_row {
            return false;
        }
        self.selected_build_row = next;
        true
    }

    /// Select previous build row.
    pub const fn select_previous_build_row(&mut self, rows: usize) -> bool {
        let next = self.selected_build_row.saturating_sub(rows);
        if next == self.selected_build_row {
            return false;
        }
        self.selected_build_row = next;
        true
    }

    /// Take pending workspace save flag.
    pub const fn take_pending_workspace_save(&mut self) -> bool {
        let pending = self.pending_workspace_save;
        self.pending_workspace_save = false;
        pending
    }

    /// Take pending workspace reload flag.
    pub const fn take_pending_workspace_reload(&mut self) -> bool {
        let pending = self.pending_workspace_reload;
        self.pending_workspace_reload = false;
        pending
    }

    /// Add the selected file as an included workspace source.
    pub fn add_selected_file_to_workspace(&mut self) -> bool {
        let Some(path) = self
            .selected_file_data()
            .map(|file| file.display_path().to_string())
        else {
            return false;
        };
        if self
            .workspace
            .sources
            .iter()
            .any(|source| matches!(&source.kind, ReviewSourceKind::File { path: source_path } if source_path == &path))
        {
            self.status_message = Some(format!("{path} is already included"));
            return true;
        }
        self.push_workspace_source(ReviewSourceKind::File { path })
    }

    /// Remove selected build source from the workspace.
    pub fn remove_selected_build_source(&mut self) -> bool {
        if self.ux_mode != ReviewUxMode::Build {
            return false;
        }
        if self.selected_build_row >= self.workspace.sources.len() {
            self.status_message = Some("select an included source to remove".to_string());
            return true;
        }
        let source = self.workspace.sources.remove(self.selected_build_row);
        self.selected_build_row = self
            .selected_build_row
            .min(self.build_row_count().saturating_sub(1));
        self.status_message = Some(format!("removed {}", source.label));
        self.pending_workspace_save = true;
        self.pending_workspace_reload = true;
        true
    }

    /// Select next repository tree row.
    pub fn select_next_tree_row(&mut self, rows: usize) -> bool {
        let max = self.file_tree_rows().len().saturating_sub(1);
        let next = self.selected_tree_row.saturating_add(rows).min(max);
        if next == self.selected_tree_row {
            return false;
        }
        self.selected_tree_row = next;
        self.select_tree_row_file_if_present();
        true
    }

    /// Select previous repository tree row.
    pub fn select_previous_tree_row(&mut self, rows: usize) -> bool {
        let next = self.selected_tree_row.saturating_sub(rows);
        if next == self.selected_tree_row {
            return false;
        }
        self.selected_tree_row = next;
        self.select_tree_row_file_if_present();
        true
    }

    fn select_tree_row_file_if_present(&mut self) {
        if let Some(ReviewFileTreeRow::File { index, .. }) =
            self.file_tree_rows().get(self.selected_tree_row).cloned()
        {
            let _ = self.select_file(index);
        }
    }

    /// Expand selected directory row.
    pub fn expand_selected_tree_row(&mut self) -> bool {
        match self.file_tree_rows().get(self.selected_tree_row).cloned() {
            Some(ReviewFileTreeRow::Directory { path, .. }) => self.expanded_dirs.insert(path),
            _ => false,
        }
    }

    /// Collapse selected directory row or selected file parent.
    pub fn collapse_selected_tree_row(&mut self) -> bool {
        match self.file_tree_rows().get(self.selected_tree_row).cloned() {
            Some(ReviewFileTreeRow::Directory { path, .. }) => self.expanded_dirs.remove(&path),
            Some(ReviewFileTreeRow::File { .. }) => {
                let parent = self.selected_file_data().and_then(|file| {
                    Path::new(file.display_path())
                        .parent()
                        .map(Path::to_path_buf)
                });
                let Some(parent) = parent else {
                    return false;
                };
                self.expanded_dirs.remove(&parent)
            }
            None => false,
        }
    }

    /// Return review thread summaries in deterministic order.
    #[must_use]
    pub fn thread_summaries(&self) -> Vec<ReviewThreadSummary> {
        self.draft_comments
            .iter()
            .filter_map(|(anchor, comments)| {
                let latest = comments.last()?;
                Some(ReviewThreadSummary {
                    anchor: anchor.clone(),
                    draft_count: comments.len(),
                    latest_body: latest.body.clone(),
                    session_id: latest.session_id.clone(),
                })
            })
            .collect()
    }

    /// Select next thread.
    pub fn select_next_thread(&mut self, rows: usize) -> bool {
        let max = self.thread_summaries().len().saturating_sub(1);
        let next = self.selected_thread.saturating_add(rows).min(max);
        if next == self.selected_thread {
            return false;
        }
        self.selected_thread = next;
        true
    }

    /// Select a thread by absolute index.
    pub fn select_thread(&mut self, index: usize) -> bool {
        if index >= self.thread_summaries().len() || index == self.selected_thread {
            return false;
        }
        self.selected_thread = index;
        true
    }

    /// Select previous thread.
    pub const fn select_previous_thread(&mut self, rows: usize) -> bool {
        let next = self.selected_thread.saturating_sub(rows);
        if next == self.selected_thread {
            return false;
        }
        self.selected_thread = next;
        true
    }

    /// Jump to the selected thread in the diff.
    pub fn jump_to_selected_thread(&mut self) -> bool {
        if self.sidebar_mode != ReviewSidebarMode::Threads {
            return false;
        }
        let Some(thread) = self.thread_summaries().get(self.selected_thread).cloned() else {
            self.status_message = Some("no review thread selected".to_string());
            return true;
        };
        self.select_anchor(&thread.anchor);
        self.status_message = Some("jumped to review thread".to_string());
        true
    }

    /// Select an anchor in the diff.
    pub fn select_anchor(&mut self, anchor: &ReviewCommentAnchor) {
        self.selected_file = anchor.file_index;
        self.selected_diff_line = anchor.diff_row;
        self.ensure_selected_diff_line_visible();
    }

    /// Select a file by index.
    pub fn select_file(&mut self, index: usize) -> bool {
        if index >= self.review.files.len() || index == self.selected_file {
            return false;
        }
        self.selected_file = index;
        self.diff_scroll = 0;
        self.selected_diff_line = 0;
        self.range_selection_start = None;
        self.sidebar_mode = ReviewSidebarMode::Included;
        self.queue_selected_file_load();
        self.expand_selected_file_dirs();
        self.sync_tree_row_to_selected_file();
        true
    }

    /// Take pending repository file load request.
    pub const fn take_pending_file_load(&mut self) -> Option<String> {
        self.pending_file_load.take()
    }

    /// Store a lazily loaded repository file.
    pub fn store_loaded_file(&mut self, file: CachedReviewFile) {
        self.file_cache.insert(file);
    }

    /// Sync selected tree row to selected file.
    pub fn sync_tree_row_to_selected_file(&mut self) {
        if let Some(row) = self.file_tree_rows().iter().position(|row| {
            matches!(row, ReviewFileTreeRow::File { index, .. } if *index == self.selected_file)
        }) {
            self.selected_tree_row = row;
        }
    }

    /// Expand ancestor directories for the selected file.
    pub fn expand_selected_file_dirs(&mut self) {
        let Some(path) = self
            .selected_file_data()
            .map(|file| file.display_path().to_string())
        else {
            return;
        };
        let path = Path::new(&path);
        let mut prefix = PathBuf::new();
        if let Some(parent) = path.parent() {
            for component in parent.components() {
                prefix.push(component.as_os_str());
                self.expanded_dirs.insert(prefix.clone());
            }
        }
    }

    /// Queue selected repository file for loading when needed.
    pub fn queue_selected_file_load(&mut self) {
        if !self.review.is_repository_review() {
            return;
        }
        let Some(path) = self
            .selected_file_data()
            .map(|file| file.display_path().to_string())
        else {
            return;
        };
        if self.file_cache.get(&path).is_none() {
            self.pending_file_load = Some(path);
        }
    }

    /// Select next file.
    pub fn select_next_file(&mut self) -> bool {
        self.select_file((self.selected_file + 1).min(self.review.files.len().saturating_sub(1)))
    }

    /// Scroll file sidebar down.
    pub fn scroll_files_down(&mut self, rows: usize) -> bool {
        let max = self.review.files.len().saturating_sub(
            self.last_file_area
                .map_or(1, |area| usize::from(area.height).max(1)),
        );
        let next = self.file_scroll.saturating_add(rows).min(max);
        if next == self.file_scroll {
            return false;
        }
        self.file_scroll = next;
        true
    }

    /// Scroll file sidebar up.
    pub const fn scroll_files_up(&mut self, rows: usize) -> bool {
        let next = self.file_scroll.saturating_sub(rows);
        if next == self.file_scroll {
            return false;
        }
        self.file_scroll = next;
        true
    }

    /// Select previous file.
    pub fn select_previous_file(&mut self) -> bool {
        self.select_file(self.selected_file.saturating_sub(1))
    }

    /// Scroll diff down.
    pub fn scroll_down(&mut self, rows: usize) -> bool {
        let max = self.max_diff_scroll();
        let next = self.diff_scroll.saturating_add(rows).min(max);
        if next == self.diff_scroll {
            return false;
        }
        self.diff_scroll = next;
        self.selected_diff_line = self.selected_diff_line.max(self.diff_scroll);
        true
    }

    /// Scroll diff up.
    pub fn scroll_up(&mut self, rows: usize) -> bool {
        let next = self.diff_scroll.saturating_sub(rows);
        if next == self.diff_scroll {
            return false;
        }
        self.diff_scroll = next;
        self.selected_diff_line = self.selected_diff_line.min(
            self.diff_scroll.saturating_add(
                self.last_diff_area
                    .map_or(1, |area| usize::from(area.height).max(1))
                    .saturating_sub(1),
            ),
        );
        true
    }

    /// Scroll to top.
    pub const fn scroll_to_top(&mut self) -> bool {
        if self.diff_scroll == 0 {
            return false;
        }
        self.diff_scroll = 0;
        true
    }

    /// Scroll to bottom.
    pub fn scroll_to_bottom(&mut self) -> bool {
        let max = self.max_diff_scroll();
        if self.diff_scroll == max {
            return false;
        }
        self.diff_scroll = max;
        true
    }

    /// Select next hunk.
    pub fn select_next_hunk(&mut self) -> bool {
        let Some(next) = self
            .hunk_render_rows()
            .into_iter()
            .find(|row| *row > self.selected_diff_line)
        else {
            return false;
        };
        self.selected_diff_line = next;
        self.ensure_selected_diff_line_visible();
        true
    }

    /// Select previous hunk.
    pub fn select_previous_hunk(&mut self) -> bool {
        let Some(previous) = self
            .hunk_render_rows()
            .into_iter()
            .rev()
            .find(|row| *row < self.selected_diff_line)
        else {
            return false;
        };
        self.selected_diff_line = previous;
        self.ensure_selected_diff_line_visible();
        true
    }

    /// Select a visible diff line by rendered row index.
    pub fn select_diff_line(&mut self, index: usize) -> bool {
        let clamped = index.min(self.rendered_diff_len().saturating_sub(1));
        if clamped == self.selected_diff_line {
            return false;
        }
        self.selected_diff_line = clamped;
        self.ensure_selected_diff_line_visible();
        true
    }

    /// Return whether file sidebar contains terminal coordinates.
    #[must_use]
    pub fn file_area_contains(&self, x: u16, y: u16) -> bool {
        self.last_file_area
            .is_some_and(|area| x >= area.x && x < area.right() && y >= area.y && y < area.bottom())
    }

    /// Return visible file tree row under terminal coordinates.
    #[must_use]
    pub fn file_tree_row_at(&self, x: u16, y: u16) -> Option<ReviewFileTreeRow> {
        let area = self.last_file_area?;
        if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
            return None;
        }
        let index = self.file_scroll + usize::from(y.saturating_sub(area.y));
        self.file_tree_rows().get(index).cloned()
    }

    /// Return visible file index under terminal coordinates.
    #[must_use]
    pub fn file_index_at(&self, x: u16, y: u16) -> Option<usize> {
        let area = self.last_file_area?;
        if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
            return None;
        }
        let index = self.file_scroll + usize::from(y.saturating_sub(area.y));
        (index < self.review.files.len()).then_some(index)
    }

    /// Return visible thread index under terminal coordinates.
    #[must_use]
    pub fn thread_index_at(&self, x: u16, y: u16) -> Option<usize> {
        let area = self.last_file_area?;
        if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
            return None;
        }
        let index = self.thread_scroll + usize::from(y.saturating_sub(area.y));
        (index < self.thread_summaries().len()).then_some(index)
    }

    /// Return visible diff row index under terminal coordinates.
    #[must_use]
    pub fn diff_line_index_at(&self, x: u16, y: u16) -> Option<usize> {
        let area = self.last_diff_area?;
        if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
            return None;
        }
        Some(self.diff_scroll + usize::from(y.saturating_sub(area.y)))
    }

    /// Return total draft comment count.
    #[must_use]
    pub fn draft_comment_count(&self) -> usize {
        self.draft_comments.values().map(Vec::len).sum()
    }

    /// Return draft comment count for a file.
    #[must_use]
    pub fn draft_comment_count_for_file(&self, file_index: usize) -> usize {
        self.draft_comments
            .iter()
            .filter(|(anchor, _)| anchor.file_index == file_index)
            .map(|(_, comments)| comments.len())
            .sum()
    }

    /// Clear active range selection.
    pub fn clear_range_selection(&mut self) -> bool {
        if self.range_selection_start.is_none() {
            return false;
        }
        self.range_selection_start = None;
        self.status_message = Some("cleared range selection".to_string());
        true
    }

    /// Toggle range selection from the selected diff line.
    pub fn toggle_range_selection(&mut self) -> bool {
        if self.range_selection_start.is_some() {
            self.range_selection_start = None;
            self.status_message = Some("cleared range selection".to_string());
            return true;
        }
        if self.selected_comment_anchor().is_none() {
            self.status_message = Some("select a diff line to start range selection".to_string());
            return true;
        }
        self.range_selection_start = Some(self.selected_diff_line);
        self.status_message =
            Some("range selection started; move then c comment or a ask Bcode".to_string());
        true
    }

    /// Return selected range bounds, if active.
    #[must_use]
    pub fn selected_range_bounds(&self) -> Option<(usize, usize)> {
        let start = self.range_selection_start?;
        Some(if start <= self.selected_diff_line {
            (start, self.selected_diff_line)
        } else {
            (self.selected_diff_line, start)
        })
    }

    /// Return true when a rendered row is within the active range selection.
    #[must_use]
    pub fn is_row_in_range_selection(&self, file_index: usize, diff_row: usize) -> bool {
        if file_index != self.selected_file {
            return false;
        }
        let Some((start, end)) = self.selected_range_bounds() else {
            return false;
        };
        (start..=end).contains(&diff_row)
    }

    /// Return a status label for an active range selection.
    #[must_use]
    pub fn range_selection_label(&self) -> Option<String> {
        let (start, end) = self.selected_range_bounds()?;
        Some(format!("range {start}-{end} selected"))
    }

    /// Return true when a diff row has draft comments.
    #[must_use]
    pub fn has_draft_comment_at(&self, file_index: usize, diff_row: usize) -> bool {
        self.draft_comments.keys().any(|anchor| {
            anchor.file_index == file_index
                && (anchor.start_diff_row()..=anchor.end_diff_row()).contains(&diff_row)
        })
    }

    /// Return the draft marker for a rendered diff row.
    #[must_use]
    pub fn draft_marker_at(&self, file_index: usize, diff_row: usize) -> Option<String> {
        let mut count = 0usize;
        let mut linked = false;
        for (anchor, comments) in &self.draft_comments {
            if anchor.file_index != file_index
                || !(anchor.start_diff_row()..=anchor.end_diff_row()).contains(&diff_row)
            {
                continue;
            }
            count = count.saturating_add(comments.len());
            linked |= comments.iter().any(|comment| comment.session_id.is_some());
        }
        if count == 0 {
            None
        } else if linked {
            Some(if count > 1 {
                format!("🤖💬{count}")
            } else {
                "🤖💬".to_string()
            })
        } else {
            Some(if count > 1 {
                format!("💬{count}")
            } else {
                "💬".to_string()
            })
        }
    }

    /// Open the draft comment editor for the selected diff line.
    pub fn open_comment_editor(&mut self) -> bool {
        let Some(anchor) = self.selected_comment_anchor() else {
            self.status_message =
                Some("select an added, removed, or context line to comment".to_string());
            return true;
        };
        self.comment_editor = Some(ReviewCommentEditor::new(anchor));
        self.sync_selected_thread_to_anchor();
        self.status_message =
            Some("editing draft comment; enter/ctrl+s saves, esc cancels".to_string());
        true
    }

    /// Open the latest persisted draft for editing.
    pub fn open_latest_draft_editor(&mut self) -> bool {
        let Some(anchor) = self.selected_comment_anchor() else {
            self.status_message = Some("select a commented line to edit a draft".to_string());
            return true;
        };
        let Some(comment) = self
            .draft_comments
            .get(&anchor)
            .and_then(|comments| comments.last())
        else {
            self.status_message = Some("no draft comment at selected line".to_string());
            return true;
        };
        let Some(comment_id) = comment.id.clone() else {
            self.status_message =
                Some("draft is not persisted yet; try again after save".to_string());
            return true;
        };
        self.comment_editor = Some(ReviewCommentEditor::edit(
            anchor,
            comment_id,
            comment.body.clone(),
        ));
        self.sync_selected_thread_to_anchor();
        self.status_message =
            Some("editing draft comment; enter/ctrl+s saves, esc cancels".to_string());
        true
    }

    /// Save the active draft comment editor.
    pub fn save_comment_editor(&mut self) -> bool {
        let Some(editor) = self.comment_editor.take() else {
            return false;
        };
        let text = editor.buffer.text().trim().to_string();
        if text.is_empty() {
            self.status_message = Some("empty comment discarded".to_string());
            return true;
        }
        let anchor = editor.anchor;
        match editor.mode {
            ReviewCommentEditorMode::Create => {
                self.draft_comments
                    .entry(anchor.clone())
                    .or_default()
                    .push(ReviewDraftComment {
                        id: None,
                        body: text.clone(),
                        persisted: false,
                        created_at_ms: None,
                        updated_at_ms: None,
                        session_id: None,
                    });
                self.pending_draft_save = Some(PendingDraftSave { anchor, body: text });
                self.sync_selected_thread_to_anchor();
                let count = self.draft_comment_count();
                self.status_message = Some(format!("saved draft comment ({count} total)"));
            }
            ReviewCommentEditorMode::Edit {
                comment_id,
                previous_body,
            } => {
                self.update_local_draft_body(&anchor, &comment_id, text.clone());
                self.pending_draft_update = Some(PendingDraftUpdate {
                    anchor,
                    comment_id,
                    previous_body,
                    new_body: text,
                });
                self.status_message = Some("updated draft comment".to_string());
            }
        }
        true
    }

    /// Take the pending draft save request, if present.
    pub const fn take_pending_draft_save(&mut self) -> Option<PendingDraftSave> {
        self.pending_draft_save.take()
    }

    /// Take the pending draft delete request, if present.
    pub const fn take_pending_draft_delete(&mut self) -> Option<PendingDraftDelete> {
        self.pending_draft_delete.take()
    }

    /// Take the pending draft update request, if present.
    pub const fn take_pending_draft_update(&mut self) -> Option<PendingDraftUpdate> {
        self.pending_draft_update.take()
    }

    /// Take the pending Bcode agent session request, if present.
    pub const fn take_pending_agent_session(&mut self) -> Option<PendingAgentSession> {
        self.pending_agent_session.take()
    }

    /// Return linked session id for an anchor.
    #[must_use]
    pub fn session_id_for_anchor(&self, anchor: &ReviewCommentAnchor) -> Option<&str> {
        self.draft_comments
            .get(anchor)?
            .last()?
            .session_id
            .as_deref()
    }

    /// Mark the latest draft at an anchor as linked to a Bcode session.
    pub fn mark_thread_session(&mut self, anchor: &ReviewCommentAnchor, session_id: String) {
        if let Some(comment) = self
            .draft_comments
            .get_mut(anchor)
            .and_then(|comments| comments.last_mut())
        {
            comment.session_id = Some(session_id);
        } else {
            self.draft_comments
                .entry(anchor.clone())
                .or_default()
                .push(ReviewDraftComment {
                    id: None,
                    body: String::new(),
                    persisted: false,
                    created_at_ms: None,
                    updated_at_ms: None,
                    session_id: Some(session_id),
                });
        }
    }

    /// Restore a locally updated draft after persistence failure.
    pub fn restore_updated_draft(&mut self, update: PendingDraftUpdate) {
        self.update_local_draft_body(&update.anchor, &update.comment_id, update.previous_body);
        self.sync_selected_thread_to_anchor();
    }

    /// Restore a locally deleted draft after persistence failure.
    pub fn restore_deleted_draft(&mut self, delete: PendingDraftDelete) {
        self.draft_comments
            .entry(delete.anchor)
            .or_default()
            .push(delete.comment);
        self.sync_selected_thread_to_anchor();
    }

    fn update_local_draft_body(
        &mut self,
        anchor: &ReviewCommentAnchor,
        comment_id: &str,
        body: String,
    ) {
        let Some(comment) = self.draft_comments.get_mut(anchor).and_then(|comments| {
            comments
                .iter_mut()
                .rev()
                .find(|comment| comment.id.as_deref() == Some(comment_id))
        }) else {
            return;
        };
        comment.body = body;
        comment.persisted = false;
    }

    fn sync_selected_thread_to_anchor(&mut self) {
        let Some(anchor) = self.selected_comment_anchor() else {
            return;
        };
        if let Some(index) = self
            .thread_summaries()
            .iter()
            .position(|thread| thread.anchor == anchor)
        {
            self.selected_thread = index;
        }
    }

    /// Queue generic review publish.
    pub fn publish_review(&mut self) -> bool {
        self.pending_publish_request = Some(PendingPublishRequest::ListPublishers);
        self.status_message = Some("loading review publishers".to_string());
        true
    }

    /// Show loaded publishers.
    pub fn show_publishers(&mut self, publishers: Vec<ReviewPublisherManifest>) {
        self.publishers = publishers;
        self.selected_publisher = self
            .publishers
            .iter()
            .position(|publisher| publisher.id == DEFAULT_PUBLISHER_ID)
            .unwrap_or(0);
        self.publish_state = Some(ReviewPublishState::Picker);
        self.status_message = None;
    }

    /// Return publisher for id.
    #[must_use]
    pub fn publisher_for_id(&self, publisher_id: &str) -> Option<&ReviewPublisherManifest> {
        self.publishers
            .iter()
            .find(|publisher| publisher.id == publisher_id)
    }

    /// Show publisher preview.
    pub fn show_publish_preview(&mut self, publisher_id: String, preview: String) {
        self.publish_state = Some(ReviewPublishState::Preview {
            publisher_id,
            options: self.current_publish_options(),
            preview,
            scroll: 0,
        });
        self.status_message = None;
    }

    /// Finish publish flow.
    pub fn finish_publish(&mut self, response: PublishReviewResponse) {
        self.publish_state = None;
        self.status_message = Some(response.message);
    }

    /// Move publish UI selection down.
    pub fn publish_down(&mut self, rows: usize) -> bool {
        match &mut self.publish_state {
            Some(ReviewPublishState::Picker) => {
                let max = self.publishers.len().saturating_sub(1);
                let next = self.selected_publisher.saturating_add(rows).min(max);
                if next == self.selected_publisher {
                    return false;
                }
                self.selected_publisher = next;
                true
            }
            Some(ReviewPublishState::Options {
                selected, options, ..
            }) => {
                let max = options.len().saturating_sub(1);
                let next = selected.saturating_add(rows).min(max);
                if next == *selected {
                    return false;
                }
                *selected = next;
                true
            }
            Some(ReviewPublishState::Preview {
                scroll, preview, ..
            }) => {
                let max = preview.lines().count().saturating_sub(1);
                let next = scroll.saturating_add(rows).min(max);
                if next == *scroll {
                    return false;
                }
                *scroll = next;
                true
            }
            None => false,
        }
    }

    /// Move publish UI selection up.
    pub const fn publish_up(&mut self, rows: usize) -> bool {
        match &mut self.publish_state {
            Some(ReviewPublishState::Picker) => {
                let next = self.selected_publisher.saturating_sub(rows);
                if next == self.selected_publisher {
                    return false;
                }
                self.selected_publisher = next;
                true
            }
            Some(ReviewPublishState::Options { selected, .. }) => {
                let next = selected.saturating_sub(rows);
                if next == *selected {
                    return false;
                }
                *selected = next;
                true
            }
            Some(ReviewPublishState::Preview { scroll, .. }) => {
                let next = scroll.saturating_sub(rows);
                if next == *scroll {
                    return false;
                }
                *scroll = next;
                true
            }
            None => false,
        }
    }

    /// Return true when publisher options are active.
    #[must_use]
    pub const fn publish_options_active(&self) -> bool {
        matches!(self.publish_state, Some(ReviewPublishState::Options { .. }))
    }

    /// Return selected publisher option index.
    #[must_use]
    pub const fn selected_publish_option_index(&self) -> usize {
        match &self.publish_state {
            Some(ReviewPublishState::Options { selected, .. }) => *selected,
            _ => 0,
        }
    }

    /// Insert text into the selected publisher option.
    pub fn insert_publish_option_text(&mut self, text: &str) -> bool {
        let Some(ReviewPublishState::Options {
            options, selected, ..
        }) = &mut self.publish_state
        else {
            return false;
        };
        let Some(option) = options.get_mut(*selected) else {
            return false;
        };
        option.value.push_str(text);
        true
    }

    /// Edit selected publisher option.
    pub fn edit_publish_option(&mut self, stroke: KeyStroke) -> bool {
        let Some(ReviewPublishState::Options {
            options, selected, ..
        }) = &mut self.publish_state
        else {
            return false;
        };
        let Some(option) = options.get_mut(*selected) else {
            return false;
        };
        match stroke.key {
            KeyCode::Char(ch) if stroke.modifiers.is_empty() => {
                option.value.push(ch);
                true
            }
            KeyCode::Backspace if stroke.modifiers.is_empty() => {
                option.value.pop();
                true
            }
            _ => false,
        }
    }

    fn current_publish_options(&self) -> Vec<ReviewPublishOption> {
        match &self.publish_state {
            Some(
                ReviewPublishState::Options { options, .. }
                | ReviewPublishState::Preview { options, .. },
            ) => options.clone(),
            _ => Vec::new(),
        }
    }

    /// Confirm current publish UI selection.
    pub fn confirm_publish_selection(&mut self) -> bool {
        match &self.publish_state {
            Some(ReviewPublishState::Picker) => {
                let Some(publisher) = self.publishers.get(self.selected_publisher) else {
                    self.status_message = Some("no publisher selected".to_string());
                    return true;
                };
                let options = options_from_schema(&publisher.options_schema);
                if options.is_empty() {
                    self.pending_publish_request = Some(PendingPublishRequest::Preview {
                        publisher_id: publisher.id.clone(),
                        options,
                    });
                    self.status_message = Some(format!("previewing publisher {}", publisher.label));
                } else {
                    self.publish_state = Some(ReviewPublishState::Options {
                        publisher_id: publisher.id.clone(),
                        options,
                        selected: 0,
                    });
                    self.status_message = None;
                }
                true
            }
            Some(ReviewPublishState::Options {
                publisher_id,
                options,
                ..
            }) => {
                self.pending_publish_request = Some(PendingPublishRequest::Preview {
                    publisher_id: publisher_id.clone(),
                    options: options.clone(),
                });
                self.status_message = Some(format!("previewing publisher {publisher_id}"));
                true
            }
            Some(ReviewPublishState::Preview {
                publisher_id,
                options,
                ..
            }) => {
                self.pending_publish_request = Some(PendingPublishRequest::Submit {
                    publisher_id: publisher_id.clone(),
                    options: options.clone(),
                });
                self.status_message = Some(format!("submitting review via {publisher_id}"));
                true
            }
            None => false,
        }
    }

    /// Take pending review publish request.
    pub const fn take_publish_request(&mut self) -> Option<PendingPublishRequest> {
        self.pending_publish_request.take()
    }

    /// Take pending linked session open request.
    pub const fn take_session_to_open(&mut self) -> Option<SessionId> {
        self.session_to_open.take()
    }

    /// Open linked session for the selected thread.
    pub fn open_linked_session_at_selection(&mut self) -> bool {
        let Some(anchor) = self.selected_comment_anchor() else {
            self.status_message = Some("select a linked review thread to open".to_string());
            return true;
        };
        let Some(session_id) = self.session_id_for_anchor(&anchor) else {
            self.status_message = Some("no linked session for selected thread".to_string());
            return true;
        };
        match session_id.parse::<SessionId>() {
            Ok(session_id) => {
                self.session_to_open = Some(session_id);
                self.should_exit = true;
            }
            Err(_) => {
                self.status_message = Some("linked session id is invalid".to_string());
            }
        }
        true
    }

    /// Ask Bcode about the selected review line/thread.
    pub fn ask_bcode_about_selection(&mut self) -> bool {
        let Some(anchor) = self.selected_comment_anchor() else {
            self.status_message = Some("select a diff line to ask Bcode".to_string());
            return true;
        };
        let existing_session = self.session_id_for_anchor(&anchor).map(ToString::to_string);
        let draft_body = self
            .draft_comments
            .get(&anchor)
            .and_then(|comments| comments.last())
            .map(|comment| comment.body.clone());
        self.pending_agent_session = Some(PendingAgentSession { anchor, draft_body });
        self.status_message = Some(existing_session.map_or_else(
            || "creating Bcode session for review thread".to_string(),
            |session_id| format!("sending review follow-up to linked session {session_id}"),
        ));
        true
    }

    /// Return a prompt for a pending Bcode agent session.
    #[must_use]
    pub fn agent_session_prompt(&self, ask: &PendingAgentSession) -> String {
        let hunk = self.hunk_context_for_anchor(&ask.anchor);
        let selected_lines = self.selected_lines_for_anchor(&ask.anchor);
        let other_comment_count = self.draft_comment_count().saturating_sub(usize::from(
            self.draft_comments
                .get(&ask.anchor)
                .is_some_and(|comments| !comments.is_empty()),
        ));
        format!(
            "You are helping with a local code review in Bcode.\n\nReview: {}\nRepository: {}\nFile: {}\nDiff rows: {}-{}\nOld range: {}-{}\nNew range: {}-{}\nLine kind: {:?}\nOther draft comment threads in this review: {}\n\nCurrent draft/comment:\n{}\n\nSelected diff lines:\n```diff\n{}\n```\n\nNearby diff hunk/context:\n```diff\n{}\n```\n\nReview context is also available through the bundled code-review plugin service. The relevant interface is `bcode.code_review/v1`; useful operations are `review.context.get`, `review.comments.list`, `review.thread.get`, and `review.diff.get`. Request payloads include `repo_path` plus the review `target`; `review.thread.get` accepts `thread_id` or `anchor`, and `review.diff.get` accepts optional `file_path`.\n\nPlease analyze this review thread. Keep the anchored file and line context in mind. If broader context is needed, inspect the repository from the session working directory.",
            self.review.title,
            self.review.repo_root.display(),
            ask.anchor.path,
            ask.anchor.start_diff_row(),
            ask.anchor.end_diff_row(),
            ask.anchor
                .old_start
                .map_or_else(|| "none".to_string(), |line| line.to_string()),
            ask.anchor
                .old_end
                .map_or_else(|| "none".to_string(), |line| line.to_string()),
            ask.anchor
                .new_start
                .map_or_else(|| "none".to_string(), |line| line.to_string()),
            ask.anchor
                .new_end
                .map_or_else(|| "none".to_string(), |line| line.to_string()),
            ask.anchor.line_kind,
            other_comment_count,
            ask.draft_body.as_deref().unwrap_or("(no draft body yet)"),
            selected_lines,
            hunk,
        )
    }

    fn selected_lines_for_anchor(&self, anchor: &ReviewCommentAnchor) -> String {
        let Some(file) = self.review.files.get(anchor.file_index) else {
            return String::new();
        };
        let rows = rendered_rows_for_prompt(file);
        let start = anchor.start_diff_row();
        let end = anchor.end_diff_row();
        rows.into_iter()
            .enumerate()
            .filter_map(|(index, row)| (start..=end).contains(&index).then_some(row))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn hunk_context_for_anchor(&self, anchor: &ReviewCommentAnchor) -> String {
        let Some(file) = self.review.files.get(anchor.file_index) else {
            return String::new();
        };
        let mut row = 0usize;
        for hunk in &file.hunks {
            let hunk_start_row = row;
            row = row.saturating_add(1);
            let hunk_end_row = row.saturating_add(hunk.lines.len());
            if anchor.diff_row < hunk_start_row || anchor.diff_row >= hunk_end_row {
                row = hunk_end_row;
                continue;
            }
            let mut lines = Vec::with_capacity(hunk.lines.len().saturating_add(1));
            lines.push(format!(
                "@@ -{},{} +{},{} @@{}",
                hunk.old_start,
                hunk.old_count,
                hunk.new_start,
                hunk.new_count,
                hunk.heading
                    .as_ref()
                    .map_or(String::new(), |heading| format!(" {heading}")),
            ));
            lines.extend(hunk.lines.iter().map(|line| {
                let marker = match line.kind {
                    ReviewLineKind::Context => ' ',
                    ReviewLineKind::Added => '+',
                    ReviewLineKind::Removed => '-',
                };
                format!("{marker}{}", line.content)
            }));
            return lines.join("\n");
        }
        String::new()
    }

    /// Delete the latest draft comment at the selected line.
    pub fn delete_latest_draft_at_selection(&mut self) -> bool {
        let Some(anchor) = self.selected_comment_anchor() else {
            self.status_message = Some("select a commented line to delete a draft".to_string());
            return true;
        };
        let Some(comments) = self.draft_comments.get_mut(&anchor) else {
            self.status_message = Some("no draft comment at selected line".to_string());
            return true;
        };
        let Some(comment) = comments.pop() else {
            self.status_message = Some("no draft comment at selected line".to_string());
            return true;
        };
        if comments.is_empty() {
            self.draft_comments.remove(&anchor);
        }
        self.pending_draft_delete = Some(PendingDraftDelete { anchor, comment });
        self.status_message = Some("deleted draft comment".to_string());
        true
    }

    /// Return a footer preview for the selected thread.
    #[must_use]
    pub fn selected_thread_preview(&self) -> Option<String> {
        let thread = self.thread_summaries().get(self.selected_thread)?.clone();
        let range = if thread.anchor.start_diff_row() == thread.anchor.end_diff_row() {
            format!("@{}", thread.anchor.start_diff_row())
        } else {
            format!(
                "@{}-{}",
                thread.anchor.start_diff_row(),
                thread.anchor.end_diff_row()
            )
        };
        let linked = thread
            .session_id
            .as_deref()
            .map_or(String::new(), |session_id| format!("  🤖 {session_id}"));
        Some(format!(
            " thread {} {range} x{}:{linked} {}  Enter jump  a ask/follow up  o open ",
            thread.anchor.path, thread.draft_count, thread.latest_body
        ))
    }

    /// Return a short preview for the selected line's latest draft comment.
    #[must_use]
    pub fn selected_draft_preview(&self) -> Option<String> {
        let anchor = self.selected_comment_anchor()?;
        let comments = self.draft_comments.get(&anchor)?;
        let latest = comments.last()?;
        Some(format!("{} draft: {}", comments.len(), latest.body))
    }

    /// Return linked session id for the selected line's latest draft comment.
    #[must_use]
    pub fn selected_draft_session_id(&self) -> Option<&str> {
        let anchor = self.selected_comment_anchor()?;
        self.draft_comments
            .get(&anchor)?
            .last()?
            .session_id
            .as_deref()
    }

    /// Load persisted draft comments into local state.
    fn load_persisted_drafts(&mut self, drafts: Vec<DraftComment>) {
        for draft in drafts {
            if let Some(anchor) = self.anchor_from_persisted_draft(&draft) {
                self.draft_comments
                    .entry(anchor)
                    .or_default()
                    .push(ReviewDraftComment {
                        id: Some(draft.comment_id),
                        body: draft.body,
                        persisted: true,
                        created_at_ms: Some(draft.created_at_ms),
                        updated_at_ms: Some(draft.updated_at_ms),
                        session_id: draft.session_id,
                    });
            }
        }
    }

    fn anchor_from_persisted_draft(&self, draft: &DraftComment) -> Option<ReviewCommentAnchor> {
        let diff_row = usize::try_from(draft.anchor.diff_row).ok()?;
        let end_diff_row = draft
            .anchor
            .end_diff_row
            .and_then(|row| usize::try_from(row).ok())
            .filter(|row| *row != diff_row);
        let file_index = self
            .review
            .files
            .iter()
            .position(|file| file.display_path() == draft.anchor.file_path)?;
        Some(ReviewCommentAnchor {
            file_index,
            path: draft.anchor.file_path.clone(),
            diff_row,
            end_diff_row,
            old_line: draft.anchor.old_line,
            new_line: draft.anchor.new_line,
            old_start: draft.anchor.old_start.or(draft.anchor.old_line),
            old_end: draft.anchor.old_end.or(draft.anchor.old_line),
            new_start: draft.anchor.new_start.or(draft.anchor.new_line),
            new_end: draft.anchor.new_end.or(draft.anchor.new_line),
            line_kind: draft.anchor.line_kind,
            is_file_anchor: draft.anchor.is_file_anchor,
            surface_id: draft.anchor.surface_id.clone(),
            source_id: draft.anchor.source_id.clone(),
        })
    }

    /// Return the selected diff line comment anchor, if the selected row is commentable.
    #[must_use]
    pub fn selected_comment_anchor(&self) -> Option<ReviewCommentAnchor> {
        self.comment_anchor_for_row(self.selected_diff_line)
    }

    /// Return a comment anchor for a rendered diff row.
    #[must_use]
    pub fn comment_anchor_for_row(&self, diff_row: usize) -> Option<ReviewCommentAnchor> {
        let file = self.selected_file_data()?;
        let (surface_id, source_id) = self.selected_surface_ids();
        if self.review.is_repository_review()
            || self
                .selected_surface()
                .is_some_and(|surface| surface.kind == ReviewSurfaceKind::File)
        {
            let (start_row, end_row) = self.selected_range_bounds().unwrap_or((diff_row, diff_row));
            let start_line = u32::try_from(start_row.saturating_add(1)).ok()?;
            let end_line = u32::try_from(end_row.saturating_add(1)).ok()?;
            return Some(ReviewCommentAnchor {
                file_index: self.selected_file,
                path: file.display_path().to_string(),
                diff_row: start_row,
                end_diff_row: (end_row != start_row).then_some(end_row),
                old_line: None,
                new_line: Some(start_line),
                old_start: None,
                old_end: None,
                new_start: Some(start_line),
                new_end: Some(end_line),
                line_kind: ReviewLineKind::Context,
                is_file_anchor: true,
                surface_id,
                source_id,
            });
        }
        let (start_row, end_row) = self.selected_range_bounds().unwrap_or((diff_row, diff_row));
        let start_line = self.diff_line_for_render_row(start_row)?;
        let end_line = self.diff_line_for_render_row(end_row)?;
        let (surface_id, source_id) = self.selected_surface_ids();
        Some(ReviewCommentAnchor {
            file_index: self.selected_file,
            path: file.display_path().to_string(),
            diff_row: start_row,
            end_diff_row: (end_row != start_row).then_some(end_row),
            old_line: start_line.old_line,
            new_line: start_line.new_line,
            old_start: start_line.old_line.or(end_line.old_line),
            old_end: end_line.old_line.or(start_line.old_line),
            new_start: start_line.new_line.or(end_line.new_line),
            new_end: end_line.new_line.or(start_line.new_line),
            line_kind: start_line.kind,
            is_file_anchor: false,
            surface_id,
            source_id,
        })
    }

    fn diff_line_for_render_row(&self, diff_row: usize) -> Option<&ReviewLine> {
        let file = self.selected_file_data()?;
        if file.is_binary {
            return None;
        }
        let mut row = 0usize;
        for hunk in &file.hunks {
            if diff_row == row {
                return None;
            }
            row = row.saturating_add(1);
            let hunk_line_index = diff_row.checked_sub(row)?;
            if hunk_line_index < hunk.lines.len() {
                return hunk.lines.get(hunk_line_index);
            }
            row = row.saturating_add(hunk.lines.len());
        }
        None
    }

    /// Return current hunk position as one-based `(current, total)`.
    #[must_use]
    pub fn hunk_position(&self) -> (usize, usize) {
        let rows = self.hunk_render_rows();
        let total = rows.len();
        let current = rows
            .iter()
            .position(|row| *row > self.selected_diff_line)
            .unwrap_or(total)
            .max(1);
        (current, total)
    }

    fn ensure_selected_diff_line_visible(&mut self) {
        let height = self
            .last_diff_area
            .map_or(1, |area| usize::from(area.height).max(1));
        if self.selected_diff_line < self.diff_scroll {
            self.diff_scroll = self.selected_diff_line;
        } else if self.selected_diff_line >= self.diff_scroll.saturating_add(height) {
            self.diff_scroll = self
                .selected_diff_line
                .saturating_sub(height.saturating_sub(1));
        }
        self.diff_scroll = self.diff_scroll.min(self.max_diff_scroll());
    }

    fn max_diff_scroll(&self) -> usize {
        self.rendered_diff_len().saturating_sub(
            self.last_diff_area
                .map_or(1, |area| usize::from(area.height).max(1)),
        )
    }

    fn rendered_diff_len(&self) -> usize {
        let Some(file) = self.selected_file_data() else {
            return 1;
        };
        if self.review.is_repository_review() {
            return self
                .file_cache
                .get(file.display_path())
                .map_or(1, |file| file.line_spans.len().max(1));
        }
        if file.is_binary {
            return 1;
        }
        file.hunks
            .iter()
            .map(|hunk| hunk.lines.len().saturating_add(1))
            .sum::<usize>()
            .max(1)
    }

    fn hunk_render_rows(&self) -> Vec<usize> {
        let Some(file) = self.selected_file_data() else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        let mut row = 0usize;
        for hunk in &file.hunks {
            rows.push(row);
            row = row.saturating_add(hunk.lines.len()).saturating_add(1);
        }
        rows
    }
}

/// Return current sidebar width for an app and terminal width.
#[must_use]
pub fn sidebar_width(app: &ReviewApp, width: u16) -> u16 {
    if app.sidebar_visible && width >= 80 {
        FILE_SIDEBAR_WIDTH.min(width.saturating_sub(30))
    } else {
        0
    }
}

fn rendered_rows_for_prompt(file: &ReviewFile) -> Vec<String> {
    let mut rows = Vec::new();
    for hunk in &file.hunks {
        rows.push(format!(
            "@@ -{},{} +{},{} @@{}",
            hunk.old_start,
            hunk.old_count,
            hunk.new_start,
            hunk.new_count,
            hunk.heading
                .as_ref()
                .map_or(String::new(), |heading| format!(" {heading}")),
        ));
        rows.extend(hunk.lines.iter().map(|line| {
            let marker = match line.kind {
                ReviewLineKind::Context => ' ',
                ReviewLineKind::Added => '+',
                ReviewLineKind::Removed => '-',
            };
            format!("{marker}{}", line.content)
        }));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_app() -> ReviewApp {
        ReviewApp::new(ReviewSummary {
            title: "test".to_string(),
            repo_root: PathBuf::from("/repo"),
            additions: 2,
            deletions: 1,
            workspace: None,
            surfaces: Vec::new(),
            files: vec![
                ReviewFile {
                    old_path: Some("a.rs".to_string()),
                    new_path: Some("a.rs".to_string()),
                    status: ReviewFileStatus::Modified,
                    additions: 2,
                    deletions: 1,
                    is_binary: false,
                    hunks: vec![
                        ReviewHunk {
                            old_start: 1,
                            old_count: 1,
                            new_start: 1,
                            new_count: 2,
                            heading: None,
                            lines: vec![
                                ReviewLine {
                                    kind: ReviewLineKind::Removed,
                                    old_line: Some(1),
                                    new_line: None,
                                    content: "old".to_string(),
                                },
                                ReviewLine {
                                    kind: ReviewLineKind::Added,
                                    old_line: None,
                                    new_line: Some(1),
                                    content: "new".to_string(),
                                },
                            ],
                        },
                        ReviewHunk {
                            old_start: 10,
                            old_count: 1,
                            new_start: 11,
                            new_count: 1,
                            heading: Some("next".to_string()),
                            lines: vec![ReviewLine {
                                kind: ReviewLineKind::Context,
                                old_line: Some(10),
                                new_line: Some(11),
                                content: "ctx".to_string(),
                            }],
                        },
                    ],
                },
                ReviewFile {
                    old_path: Some("b.rs".to_string()),
                    new_path: Some("b.rs".to_string()),
                    status: ReviewFileStatus::Modified,
                    additions: 0,
                    deletions: 0,
                    is_binary: false,
                    hunks: Vec::new(),
                },
            ],
        })
    }

    #[test]
    fn file_navigation_resets_diff_state() {
        let mut app = sample_app();
        app.diff_scroll = 2;
        app.selected_diff_line = 2;

        assert!(app.select_next_file());

        assert_eq!(app.selected_file, 1);
        assert_eq!(app.diff_scroll, 0);
        assert_eq!(app.selected_diff_line, 0);
    }

    #[test]
    fn hunk_navigation_tracks_selected_line() {
        let mut app = sample_app();
        app.set_diff_area(Rect::new(0, 0, 80, 2));

        assert!(app.select_next_hunk());

        assert_eq!(app.selected_diff_line, 3);
        assert_eq!(app.diff_scroll, 2);
        assert_eq!(app.hunk_position(), (2, 2));
    }

    #[test]
    fn creates_anchor_for_selected_diff_line() {
        let mut app = sample_app();
        app.selected_diff_line = 2;

        let anchor = app
            .selected_comment_anchor()
            .expect("added line should be commentable");

        assert_eq!(anchor.path, "a.rs");
        assert_eq!(anchor.diff_row, 2);
        assert_eq!(anchor.old_line, None);
        assert_eq!(anchor.new_line, Some(1));
        assert_eq!(anchor.line_kind, ReviewLineKind::Added);
    }

    #[test]
    fn hunk_header_is_not_commentable() {
        let app = sample_app();

        assert_eq!(app.comment_anchor_for_row(0), None);
    }

    #[test]
    fn saves_in_memory_draft_comment() {
        let mut app = sample_app();
        app.selected_diff_line = 2;

        assert!(app.open_comment_editor());
        app.comment_editor
            .as_mut()
            .expect("editor should open")
            .buffer
            .insert_str("Needs a test");
        assert!(app.save_comment_editor());

        assert_eq!(app.draft_comment_count(), 1);
        assert!(app.has_draft_comment_at(0, 2));
        assert_eq!(app.draft_comment_count_for_file(0), 1);
        let pending = app
            .take_pending_draft_save()
            .expect("draft should be pending persistence");
        assert_eq!(pending.anchor.diff_row, 2);
        assert_eq!(pending.body, "Needs a test");
    }

    #[test]
    fn edits_persisted_draft_comment() {
        let mut app = sample_app();
        app.selected_diff_line = 2;
        app.load_persisted_drafts(vec![DraftComment {
            comment_id: "comment-1".to_string(),
            thread_id: "thread-1".to_string(),
            anchor: DraftAnchor {
                file_path: "a.rs".to_string(),
                diff_row: 2,
                start_diff_row: Some(2),
                end_diff_row: Some(2),
                old_start: None,
                old_end: None,
                new_start: Some(1),
                new_end: Some(1),
                old_line: None,
                new_line: Some(1),
                line_kind: ReviewLineKind::Added,
                is_file_anchor: false,
                surface_id: None,
                source_id: None,
            },
            body: "Before".to_string(),
            created_at_ms: 1,
            updated_at_ms: 1,
            session_id: None,
        }]);

        assert!(app.open_latest_draft_editor());
        let editor = app.comment_editor.as_mut().expect("editor should open");
        editor.buffer = TextEditBuffer::from_text("After");
        assert!(app.save_comment_editor());

        let pending = app
            .take_pending_draft_update()
            .expect("update should be pending persistence");
        assert_eq!(pending.comment_id, "comment-1");
        assert_eq!(pending.previous_body, "Before");
        assert_eq!(pending.new_body, "After");
        assert_eq!(
            app.selected_draft_preview().as_deref(),
            Some("1 draft: After")
        );
    }

    #[test]
    fn loads_persisted_drafts_into_local_state() {
        let mut app = sample_app();

        app.load_persisted_drafts(vec![DraftComment {
            comment_id: "comment-1".to_string(),
            thread_id: "thread-1".to_string(),
            anchor: DraftAnchor {
                file_path: "a.rs".to_string(),
                diff_row: 2,
                start_diff_row: Some(2),
                end_diff_row: Some(2),
                old_start: None,
                old_end: None,
                new_start: Some(1),
                new_end: Some(1),
                old_line: None,
                new_line: Some(1),
                line_kind: ReviewLineKind::Added,
                is_file_anchor: false,
                surface_id: None,
                source_id: None,
            },
            body: "Persisted".to_string(),
            created_at_ms: 1,
            updated_at_ms: 1,
            session_id: None,
        }]);

        assert_eq!(app.draft_comment_count(), 1);
        assert!(app.has_draft_comment_at(0, 2));
    }

    #[test]
    fn thread_sidebar_toggle_and_jump() {
        let mut app = sample_app();
        app.selected_diff_line = 2;

        assert!(app.open_comment_editor());
        app.comment_editor
            .as_mut()
            .expect("editor should open")
            .buffer
            .insert_str("Needs a test");
        assert!(app.save_comment_editor());
        assert!(app.toggle_sidebar_mode());

        assert_eq!(app.sidebar_mode, ReviewSidebarMode::Threads);
        assert_eq!(app.thread_summaries().len(), 1);
        app.selected_diff_line = 0;
        assert!(app.jump_to_selected_thread());
        assert_eq!(app.selected_diff_line, 2);
    }

    #[test]
    fn thread_preview_shows_linked_session() {
        let mut app = sample_app();
        app.selected_diff_line = 2;
        assert!(app.open_comment_editor());
        app.comment_editor
            .as_mut()
            .expect("editor should open")
            .buffer
            .insert_str("Needs a test");
        assert!(app.save_comment_editor());
        let anchor = app.selected_comment_anchor().expect("anchor");
        app.mark_thread_session(&anchor, SessionId::new().to_string());
        app.toggle_sidebar_mode();

        let preview = app.selected_thread_preview().expect("thread preview");
        assert!(preview.contains("🤖"));
        assert!(preview.contains("Needs a test"));
    }
}
