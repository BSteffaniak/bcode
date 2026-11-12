//! The git topology discovery contributor: branches related to the seeds by history alone, so
//! stacks hint well with no stacking tool.
//!
//! For each seed branch's tip: branches whose history contains it (the layers above) and
//! branches it contains that the trunk does not (the layers below), local and remote-tracking,
//! named without their remote. Positions are unknown, so these are `related` branches, not
//! links. One `git for-each-ref` per direction and seed.

use crate::data::{DataError, git};
use bcode_code_review_models::{
    REVIEW_DISCOVERY_MAX_ITEMS, ReviewDiscoveryManifest, ReviewDiscoveryRequest,
    ReviewDiscoveryResponse,
};
use bcode_code_review_provider::{DiscoveryContributor, ProviderCallError, ProviderResult};
use std::collections::BTreeSet;
use std::path::Path;

/// Contributor id of git topology discovery.
pub const GIT_DISCOVERY_ID: &str = "git";

/// The git topology contributor's manifest.
#[must_use]
pub fn manifest() -> ReviewDiscoveryManifest {
    ReviewDiscoveryManifest {
        id: GIT_DISCOVERY_ID.to_owned(),
        label: "Git".to_owned(),
    }
}

/// Git history as a [`DiscoveryContributor`].
#[derive(Debug, Clone, Copy, Default)]
pub struct GitDiscovery;

impl DiscoveryContributor for GitDiscovery {
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

/// Branches related to `request`'s seeds by history.
///
/// # Errors
///
/// * [`DataError::Git`] or [`DataError::Io`] when git cannot list references. A seed or trunk
///   that does not name a branch is skipped.
pub fn discover(request: &ReviewDiscoveryRequest) -> Result<ReviewDiscoveryResponse, DataError> {
    let root = request.repo_path.as_path();
    let trunk = match request.trunk.clone() {
        Some(trunk) => Some(trunk),
        None => default_branch(root)?,
    };
    let trunk_tip = match trunk.as_deref() {
        Some(trunk) => branch_tip(root, trunk)?,
        None => None,
    };
    let mut related: BTreeSet<String> = BTreeSet::new();
    for seed in &request.branches {
        let Some(tip) = branch_tip(root, seed)? else {
            continue;
        };
        related.extend(branches(root, &[&format!("--contains={tip}")])?);
        if let Some(trunk_tip) = &trunk_tip {
            related.extend(branches(
                root,
                &[
                    &format!("--merged={tip}"),
                    &format!("--no-merged={trunk_tip}"),
                ],
            )?);
        }
    }
    for known in request.branches.iter().chain(trunk.iter()) {
        related.remove(known);
    }
    Ok(ReviewDiscoveryResponse {
        related: related
            .into_iter()
            .take(REVIEW_DISCOVERY_MAX_ITEMS)
            .collect(),
        ..ReviewDiscoveryResponse::default()
    })
}

/// The commit of local branch `name`, else of a remote-tracking branch of that name.
fn branch_tip(root: &Path, name: &str) -> Result<Option<String>, DataError> {
    if name.is_empty() || name.starts_with('-') {
        return Ok(None);
    }
    let local = format!("refs/heads/{name}");
    let remote = format!("refs/remotes/*/{name}");
    let listed = git::output(
        root,
        &[
            "for-each-ref",
            "--format=%(objectname)",
            "--",
            &local,
            &remote,
        ],
    )?;
    Ok(String::from_utf8_lossy(&listed)
        .lines()
        .next()
        .map(str::to_owned))
}

/// The branch `origin/HEAD` names, when the clone recorded one.
fn default_branch(root: &Path) -> Result<Option<String>, DataError> {
    let Some(head) = git::optional_output(
        root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )?
    else {
        return Ok(None);
    };
    let head = String::from_utf8_lossy(&head);
    Ok(head.trim().strip_prefix("origin/").map(str::to_owned))
}

/// Local and remote-tracking branch names (without the remote) matching `filters`.
fn branches(root: &Path, filters: &[&str]) -> Result<BTreeSet<String>, DataError> {
    let mut args = vec!["for-each-ref", "--format=%(refname)"];
    args.extend_from_slice(filters);
    args.extend(["--", "refs/heads", "refs/remotes"]);
    let listed = git::output(root, &args)?;
    Ok(String::from_utf8_lossy(&listed)
        .lines()
        .filter_map(branch_name)
        .collect())
}

/// `refs/heads/<name>` or `refs/remotes/<remote>/<name>` → `<name>`; other references (and a
/// remote's `HEAD`) are not branches.
fn branch_name(reference: &str) -> Option<String> {
    if let Some(name) = reference.strip_prefix("refs/heads/") {
        return Some(name.to_owned());
    }
    let (_, name) = reference.strip_prefix("refs/remotes/")?.split_once('/')?;
    (name != "HEAD").then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn run(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn commit_on(dir: &Path, branch: &str, from: &str, file: &str) {
        run(dir, &["checkout", "--quiet", "-b", branch, from]);
        std::fs::write(dir.join(file), branch).unwrap();
        run(dir, &["add", file]);
        run(dir, &["commit", "--quiet", "-m", branch]);
    }

    #[test]
    fn branches_above_and_below_a_seed_are_related_and_others_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(root.join("base"), "base").unwrap();
        run(root, &["add", "base"]);
        run(root, &["commit", "--quiet", "-m", "base"]);
        // main ← a ← b ← c, and an unrelated d on main.
        commit_on(root, "a", "main", "a");
        commit_on(root, "b", "a", "b");
        commit_on(root, "c", "b", "c");
        commit_on(root, "d", "main", "d");
        // A remote-tracking copy of c is named without its remote.
        run(root, &["update-ref", "refs/remotes/origin/c", "c"]);
        let answer = |seed: &str, trunk: Option<&str>| {
            discover(&ReviewDiscoveryRequest {
                repo_path: root.to_path_buf(),
                branches: vec![seed.to_owned()],
                trunk: trunk.map(str::to_owned),
            })
            .unwrap()
        };
        let response = answer("b", Some("main"));
        assert_eq!(response.related, ["a", "c"]);
        assert!(response.links.is_empty() && response.annotations.is_empty());
        // Without a trunk (and no origin/HEAD) only the layers above are known.
        assert_eq!(answer("b", None).related, ["c"]);
        // Unknown and option-like seeds are skipped.
        assert_eq!(answer("nope", Some("main")).related, Vec::<String>::new());
        assert_eq!(answer("--all", Some("main")).related, Vec::<String>::new());
        assert_eq!(branch_name("refs/remotes/origin/HEAD"), None);
        assert_eq!(branch_name("refs/tags/v1"), None);
    }
}
