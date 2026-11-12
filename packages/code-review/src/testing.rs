//! Test infrastructure shared by the code review crates.
//!
//! * [`fixture`] generates deterministic temporary git repositories, up to the 5k-file /
//!   500k-diff-line benchmark shape.
//! * [`fake_provider`] and [`fake_publisher`] stand in for review-source and publisher plugins.
//! * [`document`] synthesizes review data and serves a review document's needs without git.

pub mod document;
pub mod fake_provider;
pub mod fake_publisher;
pub mod fixture;

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
