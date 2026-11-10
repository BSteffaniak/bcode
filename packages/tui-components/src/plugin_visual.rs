//! Producer-owned plugin visual contract at the BMUX boundary.
//!
//! The contract is shared by every BMUX host (the native terminal and the browser canvas)
//! and is independent of any transcript engine:
//!
//! * [`canonical`]: adapter input projected from plugin-owned artifacts, contributions,
//!   request drafts and complete requests.
//! * [`output`]: adapter output: rows with their anchors and selection rows, adapter-local
//!   images and action regions, accepted atomically.
//! * [`action`]: narrow typed intents of visible output regions.
//! * [`portable`]: the immutable registry of statically linked producer adapters.
//!
//! These are TUI-boundary types: they carry BMUX lines and geometry and never belong in
//! portable model contracts.

pub mod action;
pub mod canonical;
pub mod output;
pub mod portable;

pub use canonical::{CanonicalPluginVisual, CanonicalToolVisual};
