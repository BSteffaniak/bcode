//! Test-only infrastructure of the review screen tests.
//!
//! * [`harness`] drives screens through scripted multi-event scenarios and renders frames into
//!   BMUX test buffers.
//! * [`backend`] serves the real data layer in-process.
//! * Fixture repositories and the fake provider and publisher come from
//!   `bcode_code_review::testing`.

pub use bcode_code_review::testing::{
    fake_provider, fake_publisher, fixture, fixture_branch_compare,
};

pub mod backend;
pub mod harness;
