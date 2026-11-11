//! Code Review V2 terminal surface (spec sections 6 and 7): one plugin surface with home, the
//! new-review wizard, and the review screen as internal routes, over the renderer-neutral review
//! document.
//!
//! * [`app`] is the surface: routes ([`home`], [`wizard`], [`surface`]) that switch without
//!   reloading anything; [`entry`] parses and resolves what the CLI asks to open.
//! * [`home`] lists reviews (grouped, filtered, previewed) and suggests new ones from repository
//!   state; [`wizard`] builds new reviews from pickers ([`picker`]).
//! * [`surface`] owns the review screen: the loading pipeline, the review chrome and panels, the
//!   virtualized document list, the file panel, input, and refresh.
//! * [`chrome`] plans and paints the header, footer, and toasts; [`panels`] decides which
//!   panels show at which size; [`files`] models the file tree.
//! * [`source`] adapts the document to a lazy BMUX `VirtualItemSource` with sticky file and hunk
//!   sections; [`rows`] renders each item through BMUX components (row-level diff layout,
//!   styled spans) with a bounded row cache.
//! * [`syntax`] highlights whole blobs off the render path; [`loader`] runs every service call
//!   on host-spawned tasks; [`backend`] abstracts the daemon so tests serve the real service
//!   in-process.
//! * [`config`] resolves the diff layout policy and glyph set; [`theme`] is the visual system
//!   shared by every route: semantic roles derived from the host theme, and the icon sets.
//! * [`notes`] keeps drafts, threads, AI exchanges, and suggestions with optimistic, serialized
//!   persistence; [`composer`] is the inline comment editor; [`agent`] asks Bcode about anchors
//!   and observes linked sessions; [`palette`] is the command palette and the keymap help.
//!
//! The app is selected with `bcode review [subcommand] --ui v2` or `BCODE_REVIEW_UI=v2` and
//! opened by the host's generic standalone-surface launcher.

pub mod agent;
pub mod app;
pub mod backend;
pub mod chrome;
pub mod composer;
pub mod config;
pub mod entry;
pub mod files;
pub mod home;
pub mod import;
pub mod loader;
pub mod modal;
pub mod notes;
pub mod options_form;
pub mod palette;
pub mod panels;
pub mod picker;
pub mod publish;
pub mod rereview;
pub mod rows;
pub mod source;
pub mod surface;
pub mod syntax;
pub mod theme;
pub mod wizard;

#[cfg(test)]
mod tests;

use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiSurfaceFactory, PluginTuiSurfaceFuture,
    PluginTuiSurfaceOpenRequest,
};
use std::sync::Arc;

pub use app::{ReviewAppV2, RouteName};
pub use surface::{Focus, ReviewSurfaceV2, SCOPE};

/// Surface kind of the V2 code review app.
pub const CODE_REVIEW_V2_SURFACE_KIND: &str = "code-review-v2";

/// Factory of the V2 code review app.
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
            let entry = entry::Entry::from_options(&request.options)?;
            let app = ReviewAppV2::new(
                Arc::new(backend::DaemonBackend::new()),
                repo_path,
                config::ReviewViewConfig::load(),
                entry,
            );
            Ok(Box::new(app) as BoxedPluginTuiSurface)
        })
    }
}
