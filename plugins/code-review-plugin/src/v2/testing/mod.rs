//! Test-only infrastructure of the code review tests.
//!
//! * [`fixture`] generates deterministic temporary git repositories, up to the 5k-file /
//!   500k-diff-line benchmark shape.
//! * `v2_data_benchmark` measures the V2 data layer (ignored by default).
//! * [`harness`] drives plugin TUI surfaces through scripted multi-event scenarios and renders
//!   frames into BMUX test buffers.

pub mod fake_provider;
pub mod fake_publisher;
pub mod fixture;
pub mod harness;
pub mod local_backend;
mod v2_data_benchmark;

use bcode_code_review_models::ReviewTarget;

/// Branch comparison from the fixture base branch to its head branch.
#[must_use]
pub fn fixture_branch_compare() -> ReviewTarget {
    ReviewTarget::BranchCompare {
        base_branch: fixture::BASE_BRANCH.to_owned(),
        head_branch: fixture::HEAD_BRANCH.to_owned(),
        merge_base: true,
    }
}
