#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Interactive terminal code review app.
//!
//! The app runs its own BMUX runtime loop and calls the `bcode_code_review` data layer directly.
//! It runs standalone (`bcode-review`) and as `bcode review`, and reads the same configuration
//! as bcode. Home, the new-review wizard, and the review screen are routes of one app over the
//! renderer-neutral review document (`bcode_code_review::document`).
//!
//! * [`app`] holds the routes ([`home`], [`wizard`], [`surface`]) that switch without reloading
//!   anything; [`entry`] parses and resolves what the command line asks to open; [`cli`] is the
//!   command line shared by `bcode review` and `bcode-review`.
//! * [`home`] lists reviews (grouped, filtered, previewed) and suggests new ones from repository
//!   state; [`wizard`] builds new reviews from pickers ([`picker`]).
//! * [`surface`] owns the review screen: the loading pipeline, the review chrome and panels, the
//!   virtualized document list, the file panel, input, and refresh.
//! * [`chrome`] plans and paints the header, footer, and toasts; [`panels`] decides which
//!   panels show at which size; [`files`] models the file tree.
//! * [`source`] adapts the document to a lazy BMUX `VirtualItemSource` with sticky file and hunk
//!   sections; [`rows`] renders each item through BMUX components (row-level diff layout,
//!   styled spans) with a bounded row cache.
//! * [`syntax`] highlights whole blobs off the render path; [`loader`] runs every data call on
//!   host-spawned tasks through [`services`], the typed handle on the data layer.
//! * [`config`] reads the bcode configuration (the review's plugin table, keybinding overrides,
//!   theme, and diff layout); [`keymap`] resolves the review's keymap actions; [`theme`] is the
//!   visual system shared by every route: semantic roles derived from the host theme, and the
//!   icon sets.
//! * [`notes`] keeps drafts, threads, AI exchanges, and suggestions with optimistic, serialized
//!   persistence; [`composer`] is the inline comment editor; [`agent`] asks Bcode about anchors
//!   and observes linked sessions; [`palette`] is the command palette and the keymap help;
//!   [`appearance`] is the review's appearance picker, which previews palettes and profiles
//!   on the whole app and saves the review's own choice.
//! * [`host`] is the boundary to whatever hosts the UI (theme, Bcode sessions, plugin services,
//!   clipboard, tasks, redraws); [`context`] pairs it with the keymap for the screens, and
//!   [`action`] is what screens ask of the run loop.

pub mod action;
pub mod agent;
pub mod app;
pub mod appearance;
pub mod chrome;
pub mod cli;
pub mod composer;
pub mod config;
pub mod context;
pub mod entry;
pub mod files;
pub mod history;
pub mod home;
pub mod host;
pub mod import;
pub mod keymap;
pub mod loader;
pub mod modal;
pub mod notes;
pub mod options_form;
pub mod palette;
pub mod panels;
pub mod path_completion;
pub mod picker;
pub mod publish;
pub mod rereview;
pub mod rows;
mod run;
pub mod services;
pub mod source;
pub mod surface;
pub mod syntax;
pub mod theme;
pub mod view;
pub mod wizard;

#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

pub use app::{ReviewApp, RouteName};
pub use run::{RunError, RunOptions, build_app, render_text, run};
pub use surface::{Focus, ReviewSurface, SCOPE};
