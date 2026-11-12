//! Review discovery interface (`bcode.review_discovery/v1`): local evidence about how branches
//! relate, and notes about them.
//!
//! Contributors (git itself, stacking tools such as Graphite) answer from local state only. Their
//! answers are hints for review-source providers, which stay authoritative, and annotations the
//! app shows; see `docs/review-discovery.md`. Every type is tool-neutral.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Review discovery service interface.
pub const REVIEW_DISCOVERY_INTERFACE_ID: &str = "bcode.review_discovery/v1";

/// Operation that returns a contributor's [`ReviewDiscoveryManifest`].
pub const OP_REVIEW_DISCOVERY_MANIFEST: &str = "review_discovery.manifest";
/// Operation that answers a [`ReviewDiscoveryRequest`].
pub const OP_REVIEW_DISCOVERY_DISCOVER: &str = "review_discovery.discover";

/// Most links, related branches, or annotations one contributor returns.
pub const REVIEW_DISCOVERY_MAX_ITEMS: usize = 1_000;

/// Response payload for `review_discovery.manifest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewDiscoveryManifest {
    /// Stable contributor id, the source of its links and annotations.
    pub id: String,
    /// Display name, shown with its annotations (for example "Graphite").
    pub label: String,
}

/// Request payload for `review_discovery.discover`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewDiscoveryRequest {
    /// Absolute path of the local repository.
    pub repo_path: PathBuf,
    /// Branches the client cares about (for example a pull request's head branch). A
    /// contributor answers about their neighborhood, and may answer more when that is as cheap.
    #[serde(default)]
    pub branches: Vec<String>,
    /// The repository's default branch, when known.
    #[serde(default)]
    pub trunk: Option<String>,
}

/// Response payload for `review_discovery.discover`. Each list holds at most
/// [`REVIEW_DISCOVERY_MAX_ITEMS`] entries.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReviewDiscoveryResponse {
    /// Parent links the contributor knows.
    #[serde(default)]
    pub links: Vec<BranchLink>,
    /// Branches likely related to the requested ones whose position is unknown.
    #[serde(default)]
    pub related: Vec<String>,
    /// Notes about branches.
    #[serde(default)]
    pub annotations: Vec<BranchAnnotation>,
}

/// `branch` is stacked on `parent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchLink {
    /// The child branch.
    pub branch: String,
    /// Its parent branch.
    pub parent: String,
    /// The parent commit the contributor recorded for the link, when it records one.
    #[serde(default)]
    pub parent_oid: Option<String>,
}

/// How much an annotation matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationSeverity {
    /// Worth knowing.
    Info,
    /// Worth acting on.
    Warning,
}

/// A note about a branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchAnnotation {
    /// The branch it is about.
    pub branch: String,
    /// How much it matters.
    pub severity: AnnotationSeverity,
    /// Stable code, unique per contributor (for example `needs_restack`).
    pub code: String,
    /// Human-readable text.
    pub message: String,
}
