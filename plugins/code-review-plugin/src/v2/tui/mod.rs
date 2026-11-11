//! Code Review V2 terminal surface (spec section 6): the BMUX adapter over the renderer-neutral
//! review document.
//!
//! * [`surface`] owns the screen: the loading pipeline, the review chrome and panels, the
//!   virtualized document list, the file panel, input, and refresh.
//! * [`chrome`] plans and paints the header, footer, and toasts; [`panels`] decides which
//!   panels show at which size; [`files`] models the file tree.
//! * [`source`] adapts the document to a lazy BMUX `VirtualItemSource` with sticky file and hunk
//!   sections; [`rows`] renders each item through BMUX components (row-level diff layout,
//!   styled spans) with a bounded row cache.
//! * [`syntax`] highlights whole blobs off the render path; [`loader`] runs every service call
//!   on host-spawned tasks; [`backend`] abstracts the daemon so tests serve the real service
//!   in-process.
//! * [`config`] resolves the diff layout policy and glyph set; [`theme`] is the review's visual
//!   system: semantic roles derived from the host theme, and the icon sets.
//! * [`notes`] keeps drafts, threads, AI exchanges, and suggestions with optimistic, serialized
//!   persistence; [`composer`] is the inline comment editor; [`agent`] asks Bcode about anchors
//!   and observes linked sessions; [`palette`] is the command palette and the keymap help.
//!
//! The surface is selected with `bcode review <subcommand> --ui v2` or `BCODE_REVIEW_UI=v2` and
//! opened by the host's generic standalone-surface launcher.

pub mod agent;
pub mod backend;
pub mod chrome;
pub mod composer;
pub mod config;
pub mod files;
pub mod loader;
pub mod notes;
pub mod palette;
pub mod panels;
pub mod rows;
pub mod source;
pub mod surface;
pub mod syntax;
pub mod theme;

#[cfg(test)]
mod tests;

use bcode_code_review_models::{ReviewTarget, ReviewWorkspace};
use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiSurfaceFactory, PluginTuiSurfaceFuture,
    PluginTuiSurfaceOpenRequest,
};
use std::sync::Arc;

pub use surface::{Focus, ReviewSurfaceV2, SCOPE};

/// Surface kind of the V2 review screen.
pub const CODE_REVIEW_V2_SURFACE_KIND: &str = "code-review-v2";

/// Factory of the V2 review surface.
#[derive(Debug, Default)]
pub struct ReviewSurfaceV2Factory;

impl PluginTuiSurfaceFactory for ReviewSurfaceV2Factory {
    fn surface_kind(&self) -> &'static str {
        CODE_REVIEW_V2_SURFACE_KIND
    }

    fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        Box::pin(async move {
            let repo_path = request
                .repo_path
                .ok_or("code review surface requires repo_path")?;
            let workspace = open_workspace(repo_path.clone(), &request.options)?;
            let surface = ReviewSurfaceV2::new(
                Arc::new(backend::DaemonBackend::new()),
                repo_path,
                workspace.sources.clone(),
                workspace.title.clone(),
                config::ReviewViewConfig::load(),
            )
            .with_workspace(workspace);
            Ok(Box::new(surface) as BoxedPluginTuiSurface)
        })
    }
}

/// The workspace a V2 surface opens: `options.workspace` when given, else a transient workspace
/// for `options.target` (a JSON value or a JSON string), else the working tree and index.
///
/// # Errors
///
/// Returns a message when an option is present but does not parse.
pub fn open_workspace(
    repo_path: std::path::PathBuf,
    options: &serde_json::Value,
) -> Result<ReviewWorkspace, String> {
    fn parse<T: serde::de::DeserializeOwned>(value: &serde_json::Value) -> Result<T, String> {
        match value {
            serde_json::Value::String(text) => serde_json::from_str(text),
            other => serde_json::from_value(other.clone()),
        }
        .map_err(|error| error.to_string())
    }
    if let Some(workspace) = options.get("workspace").filter(|value| !value.is_null()) {
        return parse(workspace).map_err(|error| format!("invalid review workspace: {error}"));
    }
    let target = match options.get("target").filter(|value| !value.is_null()) {
        Some(target) => parse(target).map_err(|error| format!("invalid review target: {error}"))?,
        None => ReviewTarget::WorkingTreeAndIndex,
    };
    Ok(ReviewWorkspace::from_target(repo_path, target))
}
