//! Code review native TUI plugin launcher.

use std::io::Write;
use std::path::PathBuf;

use bcode_code_review_models::{ReviewSourceKind, ReviewTarget, ReviewWorkspace};
use bcode_code_review_plugin::code_review_home::ReviewHomeOutcome;
use bcode_session_models::SessionId;
use bmux_tui::terminal::Terminal;

use crate::TuiError;

/// Run the code review plugin home/picker surface.
///
/// # Errors
///
/// Returns an error when review workspaces cannot be loaded or terminal I/O fails.
#[allow(clippy::future_not_send)]
pub async fn run_home<W: Write>(
    terminal: &mut Terminal<&mut W>,
    repo_path: PathBuf,
) -> Result<ReviewHomeOutcome, TuiError> {
    bcode_code_review_plugin::code_review_home::run(terminal, repo_path)
        .await
        .map_err(plugin_tui_error_to_host)
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
    run_with_workspace(
        terminal,
        workspace.repo_root.clone(),
        target,
        Some(workspace),
        build_mode,
    )
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
    run_with_workspace(terminal, repo_path, target, None, false).await
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
    let runtime = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection::all_enabled(),
        &crate::static_bundled_plugins(),
    )
    .map_err(|error| TuiError::PluginService {
        code: "plugin_runtime_load_failed".to_string(),
        message: error.to_string(),
    })?;
    let mut surface = crate::plugin_tui::open_plugin_tui_surface(
        &runtime,
        "bcode.code_review",
        "code-review",
        bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest {
            instance_id: "code-review".to_string(),
            repo_path: Some(repo_path),
            target: None,
            options,
        },
    )
    .await
    .map_err(|error| TuiError::PluginService {
        code: "tui_surface_open_failed".to_string(),
        message: error.to_string(),
    })?;
    let close_outcome =
        crate::plugin_surface_host::run_plugin_surface(terminal, surface.as_mut()).await?;
    let session_to_open = close_outcome
        .and_then(|outcome| outcome.get("open_session").cloned())
        .and_then(|value| serde_json::from_value(value).ok());
    Ok(session_to_open)
}

fn plugin_tui_error_to_host(error: bcode_code_review_plugin::tui_host_types::TuiError) -> TuiError {
    match error {
        bcode_code_review_plugin::tui_host_types::TuiError::Client(error) => {
            TuiError::Client(error)
        }
        bcode_code_review_plugin::tui_host_types::TuiError::Io(error) => TuiError::Io(error),
        bcode_code_review_plugin::tui_host_types::TuiError::Join(error) => TuiError::Join(error),
        bcode_code_review_plugin::tui_host_types::TuiError::Json(error) => TuiError::Json(error),
        bcode_code_review_plugin::tui_host_types::TuiError::PluginService { code, message } => {
            TuiError::PluginService { code, message }
        }
    }
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
