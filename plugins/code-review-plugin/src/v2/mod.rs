//! Code Review V2.
//!
//! V2 is a parallel rewrite of the review UI and data layer described in
//! `docs/code-review-v2-architecture.md`. It lives beside V1 so V1 can be deleted wholesale once
//! V2 reaches parity.
//!
//! * [`service`] is the lazy, revision-pinned data layer (phase 1).
//! * [`provider`] routes pull-request and stack sources to review-source provider plugins from
//!   the client.
//! * [`viewed`] matches identity-pinned viewed marks.

pub mod provider;
pub mod service;
#[cfg(test)]
pub mod testing;
pub mod viewed;
