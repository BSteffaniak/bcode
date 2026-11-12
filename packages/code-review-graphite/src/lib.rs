#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Graphite stack discovery for code review ([`GraphiteDiscovery`], a [`DiscoveryContributor`]).
//!
//! The review links this crate and calls it in-process; the Graphite Bcode plugin serves the
//! same contributor as `bcode.review_discovery/v1` for agents and other plugins.
//!
//! Graphite records each tracked branch's parent in git itself, one JSON blob per branch under
//! `refs/branch-metadata/<branch>`. This plugin reads those blobs (one `git for-each-ref` and one
//! `git cat-file --batch`), so it needs neither the `gt` binary nor the network, and answers:
//!
//! * links: each tracked branch's parent (`parentBranchName`, with `parentBranchRevision`);
//! * `needs_restack` warnings: the parent revision Graphite recorded is no longer the parent
//!   branch's tip, the rule `gt` itself applies;
//! * `invalid_metadata` warnings: Graphite's own `validationResult` is not `VALID`.
//!
//! The format is Graphite's internal one, so it is read leniently: unknown fields are ignored and
//! a blob that does not parse is skipped. Graphite's "last submitted" record is not used, since
//! pushes made outside `gt submit` do not update it. See `docs/review-discovery.md`.

use bcode_code_review_models::{
    AnnotationSeverity, BranchAnnotation, BranchLink, REVIEW_DISCOVERY_MAX_ITEMS,
    ReviewDiscoveryManifest, ReviewDiscoveryRequest, ReviewDiscoveryResponse,
};
use bcode_code_review_provider::{DiscoveryContributor, ProviderCallError, ProviderResult};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use thiserror::Error;

/// Contributor id of Graphite discovery.
pub const GRAPHITE_DISCOVERY_ID: &str = "graphite";
/// Where Graphite keeps branch metadata.
const METADATA_NAMESPACE: &str = "refs/branch-metadata/";

/// Why Graphite's metadata could not be read.
#[derive(Debug, Error)]
pub enum GraphiteError {
    /// Git could not be run or failed.
    #[error("git: {0}")]
    Git(String),
}

/// Graphite's branch metadata as a [`DiscoveryContributor`].
#[derive(Debug, Clone, Copy, Default)]
pub struct GraphiteDiscovery;

impl DiscoveryContributor for GraphiteDiscovery {
    fn manifest(&self) -> ReviewDiscoveryManifest {
        manifest()
    }

    fn discover(
        &self,
        request: &ReviewDiscoveryRequest,
    ) -> ProviderResult<ReviewDiscoveryResponse> {
        discover(request)
            .map_err(|error| ProviderCallError::new("discovery_failed", error.to_string()))
    }
}

/// The Graphite contributor's manifest.
#[must_use]
pub fn manifest() -> ReviewDiscoveryManifest {
    ReviewDiscoveryManifest {
        id: GRAPHITE_DISCOVERY_ID.to_owned(),
        label: "Graphite".to_owned(),
    }
}

/// One branch's metadata, as far as discovery reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    #[serde(default)]
    parent_branch_name: Option<String>,
    #[serde(default)]
    parent_branch_revision: Option<String>,
    #[serde(default)]
    validation_result: Option<String>,
}

/// Graphite's view of the branches connected to `request`'s seeds (every tracked branch when
/// there are none): parent links and warnings. A repository without Graphite metadata answers
/// nothing.
///
/// # Errors
///
/// * [`GraphiteError::Git`] when git cannot list or read the metadata.
pub fn discover(
    request: &ReviewDiscoveryRequest,
) -> Result<ReviewDiscoveryResponse, GraphiteError> {
    let root = request.repo_path.as_path();
    let metadata = read_metadata(root)?;
    if metadata.is_empty() {
        return Ok(ReviewDiscoveryResponse::default());
    }
    let tracked = connected(&metadata, &request.branches);
    let tips = branch_tips(root)?;
    let mut response = ReviewDiscoveryResponse::default();
    for (branch, meta) in metadata
        .iter()
        .filter(|(branch, _)| tracked.contains(*branch))
    {
        if let Some(parent) = &meta.parent_branch_name {
            response.links.push(BranchLink {
                branch: branch.clone(),
                parent: parent.clone(),
                parent_oid: meta.parent_branch_revision.clone(),
            });
            if let (Some(recorded), Some(tip)) = (&meta.parent_branch_revision, tips.get(parent))
                && recorded != tip
            {
                response.annotations.push(BranchAnnotation {
                    branch: branch.clone(),
                    severity: AnnotationSeverity::Warning,
                    code: "needs_restack".to_owned(),
                    message: format!(
                        "needs a restack: {parent} moved since it was stacked (gt restack)"
                    ),
                });
            }
        }
        if let Some(result) = meta
            .validation_result
            .as_deref()
            .filter(|result| *result != "VALID")
        {
            response.annotations.push(BranchAnnotation {
                branch: branch.clone(),
                severity: AnnotationSeverity::Warning,
                code: "invalid_metadata".to_owned(),
                message: format!("has Graphite metadata marked {result}"),
            });
        }
    }
    response.links.truncate(REVIEW_DISCOVERY_MAX_ITEMS);
    response.annotations.truncate(REVIEW_DISCOVERY_MAX_ITEMS);
    Ok(response)
}

/// The tracked branches connected to `seeds` through parent links (all of them without seeds).
/// The walk goes only through tracked branches: the trunk (which has no parent) is where stacks
/// meet, not a path between them.
fn connected(metadata: &BTreeMap<String, Metadata>, seeds: &[String]) -> BTreeSet<String> {
    if seeds.is_empty() {
        return metadata.keys().cloned().collect();
    }
    let mut neighbors: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (branch, meta) in metadata {
        if let Some(parent) = &meta.parent_branch_name {
            neighbors.entry(branch).or_default().push(parent);
            neighbors.entry(parent).or_default().push(branch);
        }
    }
    let mut visited: BTreeSet<&str> = seeds.iter().map(String::as_str).collect();
    let mut queue: Vec<&str> = visited.iter().copied().collect();
    while let Some(branch) = queue.pop() {
        if !metadata.contains_key(branch) {
            continue;
        }
        for next in neighbors.get(branch).into_iter().flatten() {
            if visited.insert(next) {
                queue.push(next);
            }
        }
    }
    visited
        .into_iter()
        .filter(|branch| metadata.contains_key(*branch))
        .map(str::to_owned)
        .collect()
}

/// Every branch's metadata blob, parsed; blobs that do not parse are skipped.
fn read_metadata(root: &Path) -> Result<BTreeMap<String, Metadata>, GraphiteError> {
    let listed = git(
        root,
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            METADATA_NAMESPACE,
        ],
    )?;
    let refs: Vec<(&str, &str)> = listed
        .lines()
        .filter_map(|line| {
            let (oid, reference) = line.split_once(' ')?;
            Some((oid, reference.strip_prefix(METADATA_NAMESPACE)?))
        })
        .collect();
    if refs.is_empty() {
        return Ok(BTreeMap::new());
    }
    let input = refs
        .iter()
        .map(|(oid, _)| *oid)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let blobs = cat_file_batch(root, &input, refs.len())?;
    Ok(refs
        .iter()
        .zip(blobs)
        .filter_map(|((_, branch), blob)| {
            let metadata = serde_json::from_slice::<Metadata>(&blob?).ok()?;
            Some(((*branch).to_owned(), metadata))
        })
        .collect())
}

/// Local branch names and their commits.
fn branch_tips(root: &Path) -> Result<BTreeMap<String, String>, GraphiteError> {
    let listed = git(
        root,
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname:strip=2)",
            "refs/heads/",
        ],
    )?;
    Ok(listed
        .lines()
        .filter_map(|line| {
            let (oid, branch) = line.split_once(' ')?;
            Some((branch.to_owned(), oid.to_owned()))
        })
        .collect())
}

/// Read `count` objects named one per line in `input` with `git cat-file --batch`; a missing
/// object reads as `None`.
fn cat_file_batch(
    root: &Path,
    input: &str,
    count: usize,
) -> Result<Vec<Option<Vec<u8>>>, GraphiteError> {
    let mut child = Command::new("git")
        .args(["cat-file", "--batch"])
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| GraphiteError::Git(format!("could not start: {error}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| GraphiteError::Git("no stdin".to_owned()))?;
    let input = input.to_owned();
    // Write on another thread so a large batch cannot fill both pipes and stall.
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let mut reader = BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| GraphiteError::Git("no stdout".to_owned()))?,
    );
    let mut blobs = Vec::with_capacity(count);
    for _ in 0..count {
        let mut header = String::new();
        reader
            .read_line(&mut header)
            .map_err(|error| GraphiteError::Git(error.to_string()))?;
        let size = header
            .split_whitespace()
            .nth(2)
            .and_then(|size| size.parse::<usize>().ok());
        let Some(size) = size else {
            // `<object> missing`
            blobs.push(None);
            continue;
        };
        let mut blob = vec![0; size + 1];
        reader
            .read_exact(&mut blob)
            .map_err(|error| GraphiteError::Git(error.to_string()))?;
        blob.truncate(size);
        blobs.push(Some(blob));
    }
    writer
        .join()
        .map_err(|_| GraphiteError::Git("writer panicked".to_owned()))?
        .map_err(|error| GraphiteError::Git(error.to_string()))?;
    child
        .wait()
        .map_err(|error| GraphiteError::Git(error.to_string()))?;
    Ok(blobs)
}

fn git(root: &Path, args: &[&str]) -> Result<String, GraphiteError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| GraphiteError::Git(format!("could not start: {error}")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(GraphiteError::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests;
