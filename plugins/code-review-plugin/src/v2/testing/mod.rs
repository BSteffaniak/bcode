//! Test-only infrastructure shared by V1 baseline and V2 tests.
//!
//! * [`fixture`] generates deterministic temporary git repositories, up to the 5k-file /
//!   500k-diff-line benchmark shape.
//! * [`harness`] drives plugin TUI surfaces through scripted multi-event scenarios and renders
//!   frames into BMUX test buffers.

pub mod fixture;
pub mod harness;
mod v1_baseline;
mod v1_smoke;

use bcode_code_review_models::{
    MaterializeReviewWorkspaceRequest, MaterializeReviewWorkspaceResponse, ReviewTarget,
    ReviewWorkspace,
};
use std::path::Path;

/// Materialize a single-target V1 workspace through the plugin's service implementation.
///
/// # Panics
///
/// Panics when materialization fails.
#[must_use]
pub fn materialize_v1(repo: &Path, target: ReviewTarget) -> MaterializeReviewWorkspaceResponse {
    crate::materialize_review_workspace_for_request(MaterializeReviewWorkspaceRequest {
        repo_path: repo.to_path_buf(),
        workspace: ReviewWorkspace::from_target(repo.to_path_buf(), target),
    })
    .expect("materialize V1 review workspace")
}

/// Branch comparison from the fixture base branch to its head branch.
#[must_use]
pub fn fixture_branch_compare() -> ReviewTarget {
    ReviewTarget::BranchCompare {
        base_branch: fixture::BASE_BRANCH.to_owned(),
        head_branch: fixture::HEAD_BRANCH.to_owned(),
        merge_base: true,
    }
}
