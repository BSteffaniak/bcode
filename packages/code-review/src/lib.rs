#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Code review data layer for Bcode.
//!
//! The typed operations behind the `bcode.code_review/v1` plugin service, callable directly from
//! Rust. The code review plugin adapts them to JSON service operations; a review UI in the same
//! process calls them without encoding.
//!
//! * [`document`] is the renderer-neutral review document: the keyed item sequence a review
//!   front end virtualizes, with lazy per-file content, context expansion, annotations, viewed
//!   state, and navigation. It performs no I/O; its owner serves the needs it reports.
//! * [`publish_plan`] decides which publish target each thread goes to and builds bundle
//!   threads from target hunks, also without I/O.
//! * [`data`] reads repositories lazily and revision-pinned: source resolution, file indexes,
//!   hunks, blob line ranges, search, stack attribution, and re-review comparisons.
//! * [`store`] persists workspaces, drafts and threads, AI exchanges, suggestions, publish
//!   records, and external imports, and publishes with the built-in file publishers.
//! * [`provider`] routes pull-request and stack sources to review-source provider plugins.
//! * [`viewed`] matches identity-pinned viewed marks.
//! * [`scope`] builds transient workspaces and derives the scope annotations are stored under.

pub mod data;
pub mod document;
pub mod provider;
pub mod publish_plan;
pub mod scope;
pub mod store;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod viewed;

pub use data::{DataError, ReviewData};
pub use store::{CodeReviewConfig, CodeReviewStateLocation, ReviewStore, StoreError};
