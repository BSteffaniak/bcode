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
    let runtime = load_code_review_tui_runtime()?;
    let surface = crate::plugin_tui::open_plugin_tui_surface(
        &runtime,
        CODE_REVIEW_PLUGIN_ID,
        "code-review-home",
        bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest {
            instance_id: "code-review-home".to_string(),
            repo_path: Some(repo_path),
            session_id: None,
            target: None,
            options: serde_json::Value::Null,
        },
    )
    .await
    .map_err(|error| map_surface_open_error(&error))?;
    let outcome = Box::pin(crate::runtime::run_standalone_plugin_surface(
        terminal,
        CODE_REVIEW_PLUGIN_ID,
        surface,
    ))
    .await?;
    Ok(parse_review_home_outcome(outcome))
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
    let target = target_from_workspace(&workspace);
    Box::pin(run_with_workspace(
        terminal,
        workspace.repo_root.clone(),
        target,
        Some(workspace),
        build_mode,
    ))
    .await
}

/// Run a full-screen local Git review.
///
/// # Errors
///
/// Returns an error when the code review plugin cannot be loaded/opened or terminal I/O fails.
#[allow(clippy::future_not_send)]
pub async fn run<W: Write>(
    terminal: &mut Terminal<&mut W>,
    repo_path: PathBuf,
    target: ReviewTarget,
) -> Result<Option<SessionId>, TuiError> {
    Box::pin(run_with_workspace(terminal, repo_path, target, None, false)).await
}

#[allow(clippy::future_not_send)]
async fn run_with_workspace<W: Write>(
    terminal: &mut Terminal<&mut W>,
    repo_path: PathBuf,
    target: ReviewTarget,
    workspace: Option<ReviewWorkspace>,
    build_mode: bool,
) -> Result<Option<SessionId>, TuiError> {
    let options = serde_json::json!({
        "build_mode": build_mode,
        "workspace": workspace,
        "target": target,
    });
    let runtime = load_code_review_tui_runtime()?;
    let surface = crate::plugin_tui::open_plugin_tui_surface(
        &runtime,
        CODE_REVIEW_PLUGIN_ID,
        "code-review",
        bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest {
            instance_id: "code-review".to_string(),
            repo_path: Some(repo_path),
            session_id: None,
            target: None,
            options,
        },
    )
    .await
    .map_err(|error| map_surface_open_error(&error))?;
    let _outcome = Box::pin(crate::runtime::run_standalone_plugin_surface(
        terminal,
        CODE_REVIEW_PLUGIN_ID,
        surface,
    ))
    .await?;
    Ok(None)
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
