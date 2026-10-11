//! Code review native TUI plugin launcher.

use std::io::Write;
use std::path::PathBuf;

use bcode_code_review_models::{ReviewSourceKind, ReviewTarget, ReviewWorkspace};
use bcode_session_models::SessionId;
use bmux_tui::terminal::Terminal;
use serde::Deserialize;

use crate::TuiError;

const CODE_REVIEW_PLUGIN_ID: &str = "bcode.code_review";

/// Review home outcome returned by the code review home plugin surface.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum ReviewHomeOutcome {
    /// Open an existing or newly created review target.
    OpenWorkspace {
        /// Review workspace to open.
        workspace: ReviewWorkspace,
        /// Whether to open directly in build/source-composition mode.
        build_mode: bool,
    },
    /// Exit without opening a review.
    Exit,
}

/// Run the code review plugin home/picker surface.
///
/// # Errors
///
/// Returns an error when the code review plugin cannot be loaded/opened or terminal I/O fails.
#[allow(clippy::future_not_send)]
pub async fn run_home<W: Write>(
    terminal: &mut Terminal<&mut W>,
    repo_path: PathBuf,
) -> Result<ReviewHomeOutcome, TuiError> {
    let prepared = prepare_home(repo_path).await?;
    Box::pin(run_prepared_home(terminal, prepared)).await
}

/// An opened plugin surface and the runtime that must outlive its interactive execution.
/// Preparation owns no terminal or input reader and must be drained if exit is requested.
pub struct PreparedReviewSurface {
    runtime: bcode_plugin::PluginRuntimeHost,
    surface: bcode_plugin_sdk::tui::BoxedPluginTuiSurface,
    presentation: crate::runtime::PreparedPresentation,
}

/// Load and open review home without taking terminal/input ownership.
///
/// # Errors
/// Returns plugin load, worker, or surface-open errors.
pub async fn prepare_home(repo_path: PathBuf) -> Result<PreparedReviewSurface, TuiError> {
    prepare_surface("code-review-home", repo_path, serde_json::Value::Null).await
}

/// Load and open a review workspace without taking terminal/input ownership.
///
/// # Errors
/// Returns plugin load, worker, or surface-open errors.
pub async fn prepare_workspace(
    workspace: ReviewWorkspace,
    build_mode: bool,
) -> Result<PreparedReviewSurface, TuiError> {
    let target = target_from_workspace(&workspace);
    prepare_surface(
        "code-review",
        workspace.repo_root.clone(),
        serde_json::json!({
            "build_mode": build_mode, "workspace": workspace, "target": target,
        }),
    )
    .await
}

/// Load and open a review target without taking terminal/input ownership.
///
/// # Errors
/// Returns plugin load, worker, or surface-open errors.
pub async fn prepare_review(
    repo_path: PathBuf,
    target: ReviewTarget,
) -> Result<PreparedReviewSurface, TuiError> {
    prepare_surface(
        "code-review",
        repo_path,
        serde_json::json!({
            "build_mode": false, "workspace": null, "target": target,
        }),
    )
    .await
}

async fn prepare_surface(
    kind: &'static str,
    repo_path: PathBuf,
    options: serde_json::Value,
) -> Result<PreparedReviewSurface, TuiError> {
    let handle = tokio::runtime::Handle::current();
    let (runtime, surface) = tokio::task::spawn_blocking(move || {
        let runtime = load_code_review_tui_runtime()?;
        // Plugin factories may perform synchronous native initialization before yielding.
        // Keep that preparation off the terminal scheduler; retain and drain its result.
        let surface = handle
            .block_on(crate::plugin_tui::open_plugin_tui_surface(
                &runtime,
                CODE_REVIEW_PLUGIN_ID,
                kind,
                bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest {
                    instance_id: kind.to_owned(),
                    repo_path: Some(repo_path),
                    session_id: None,
                    target: None,
                    options,
                },
            ))
            .map_err(|error| map_surface_open_error(&error))?;
        Ok::<_, TuiError>((runtime, surface))
    })
    .await??;
    let presentation =
        crate::runtime::prepare_presentation(&crate::static_bundled_plugins()).await?;
    Ok(PreparedReviewSurface {
        runtime,
        surface,
        presentation,
    })
}

/// Run an already-opened review home after preparation input is fully stopped.
///
/// # Errors
/// Returns interactive runtime errors.
#[allow(clippy::future_not_send)]
pub async fn run_prepared_home<W: Write>(
    terminal: &mut Terminal<&mut W>,
    prepared: PreparedReviewSurface,
) -> Result<ReviewHomeOutcome, TuiError> {
    Ok(parse_review_home_outcome(
        run_prepared_surface(terminal, prepared).await?,
    ))
}

/// Run an already-opened review workspace/target after preparation input is fully stopped.
///
/// # Errors
/// Returns interactive runtime errors.
#[allow(clippy::future_not_send)]
pub async fn run_prepared_review<W: Write>(
    terminal: &mut Terminal<&mut W>,
    prepared: PreparedReviewSurface,
) -> Result<Option<SessionId>, TuiError> {
    run_prepared_surface(terminal, prepared).await?;
    Ok(None)
}

#[allow(clippy::future_not_send)]
async fn run_prepared_surface<W: Write>(
    terminal: &mut Terminal<&mut W>,
    prepared: PreparedReviewSurface,
) -> Result<Option<serde_json::Value>, TuiError> {
    let PreparedReviewSurface {
        runtime,
        surface,
        presentation,
    } = prepared;
    let result = Box::pin(crate::runtime::run_prepared_plugin_surface(
        terminal,
        CODE_REVIEW_PLUGIN_ID,
        surface,
        presentation,
    ))
    .await;
    drop(runtime);
    result
}

/// Run a full-screen local Git review from a durable workspace.
///
/// # Errors
///
/// Returns an error when the code review plugin cannot be loaded/opened or terminal I/O fails.
#[allow(clippy::future_not_send)]
pub async fn run_workspace<W: Write>(
    terminal: &mut Terminal<&mut W>,
    workspace: ReviewWorkspace,
    build_mode: bool,
) -> Result<Option<SessionId>, TuiError> {
    let prepared = prepare_workspace(workspace, build_mode).await?;
    Box::pin(run_prepared_review(terminal, prepared)).await
}

/// Run a full-screen local Git review from a target.
///
/// # Errors
/// Returns plugin preparation or interactive runtime errors.
#[allow(clippy::future_not_send)]
pub async fn run<W: Write>(
    terminal: &mut Terminal<&mut W>,
    repo_path: PathBuf,
    target: ReviewTarget,
) -> Result<Option<SessionId>, TuiError> {
    let prepared = prepare_review(repo_path, target).await?;
    Box::pin(run_prepared_review(terminal, prepared)).await
}

fn load_code_review_tui_runtime() -> Result<bcode_plugin::PluginRuntimeHost, TuiError> {
    bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection::all_enabled(),
        &crate::static_bundled_plugins(),
    )
    .map_err(|error| TuiError::PluginService {
        code: "plugin_runtime_load_failed".to_string(),
        message: error.to_string(),
    })
}

fn map_surface_open_error(error: &impl ToString) -> TuiError {
    TuiError::PluginService {
        code: "tui_surface_open_failed".to_string(),
        message: error.to_string(),
    }
}

fn parse_review_home_outcome(outcome: Option<serde_json::Value>) -> ReviewHomeOutcome {
    outcome
        .and_then(|value| value.get("review_home").cloned())
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or(ReviewHomeOutcome::Exit)
}

fn target_from_workspace(workspace: &ReviewWorkspace) -> ReviewTarget {
    workspace
        .sources
        .iter()
        .find(|source| source.included)
        .map_or(ReviewTarget::Repository, |source| {
            target_from_source_kind(&source.kind)
        })
}

fn target_from_source_kind(kind: &ReviewSourceKind) -> ReviewTarget {
    match kind {
        ReviewSourceKind::WorkingTreeUnstaged => ReviewTarget::WorkingTreeUnstaged,
        ReviewSourceKind::IndexStaged => ReviewTarget::IndexStaged,
        ReviewSourceKind::WorkingTreeAndIndex => ReviewTarget::WorkingTreeAndIndex,
        ReviewSourceKind::LastCommit => ReviewTarget::LastCommit,
        ReviewSourceKind::CommitRange {
            base,
            head,
            merge_base,
        } => ReviewTarget::CommitRange {
            base: base.clone(),
            head: head.clone(),
            merge_base: *merge_base,
        },
        ReviewSourceKind::BranchCompare {
            base_branch,
            head_branch,
            merge_base,
        } => ReviewTarget::BranchCompare {
            base_branch: base_branch.clone(),
            head_branch: head_branch.clone(),
            merge_base: *merge_base,
        },
        ReviewSourceKind::Commit { rev } => ReviewTarget::CommitRange {
            base: format!("{rev}^"),
            head: rev.clone(),
            merge_base: false,
        },
        ReviewSourceKind::File { .. }
        | ReviewSourceKind::FileRange { .. }
        | ReviewSourceKind::Repository => ReviewTarget::Repository,
    }
}
