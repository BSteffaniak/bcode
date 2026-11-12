//! Graphite discovery over a real repository with Graphite-shaped branch metadata.

use super::*;
use std::path::PathBuf;

fn run(dir: &Path, args: &[&str], input: Option<&str>) -> String {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.unwrap_or_default().as_bytes())
        .expect("write");
    let output = child.wait_with_output().expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn commit_on(dir: &Path, branch: &str, from: &str) -> String {
    run(dir, &["checkout", "--quiet", "-B", branch, from], None);
    let path = dir.join(branch);
    let before = std::fs::read_to_string(&path).unwrap_or_default();
    std::fs::write(&path, format!("{before}{branch}\n")).unwrap();
    run(dir, &["add", branch], None);
    run(dir, &["commit", "--quiet", "-m", branch], None);
    run(dir, &["rev-parse", "HEAD"], None)
}

/// Record Graphite metadata for `branch`.
fn track(dir: &Path, branch: &str, json: &str) {
    let oid = run(dir, &["hash-object", "-w", "--stdin"], Some(json));
    run(
        dir,
        &[
            "update-ref",
            &format!("refs/branch-metadata/{branch}"),
            &oid,
        ],
        None,
    );
}

fn meta(parent: &str, parent_revision: &str, validation: &str) -> String {
    serde_json::json!({
        "children": [],
        "branchRevision": "unused",
        "parentBranchName": parent,
        "parentBranchRevision": parent_revision,
        "validationResult": validation,
        "lastSubmittedVersion": { "headSha": "x", "baseSha": "y", "baseName": parent },
        "someFutureField": true,
    })
    .to_string()
}

/// main ← a ← b (both tracked), an untracked branch, and an unrelated tracked stack x on main.
fn repo() -> (tempfile::TempDir, PathBuf, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    run(&root, &["init", "--quiet", "--initial-branch=main"], None);
    std::fs::write(root.join("base"), "base").unwrap();
    run(&root, &["add", "base"], None);
    run(&root, &["commit", "--quiet", "-m", "base"], None);
    let main = run(&root, &["rev-parse", "HEAD"], None);
    let a = commit_on(&root, "a", &main);
    commit_on(&root, "b", &a);
    commit_on(&root, "x", &main);
    commit_on(&root, "untracked", &main);
    track(&root, "a", &meta("main", &main, "VALID"));
    track(&root, "b", &meta("a", &a, "VALID"));
    track(&root, "x", &meta("main", &main, "VALID"));
    (dir, root, main)
}

fn ask(root: &Path, seeds: &[&str]) -> ReviewDiscoveryResponse {
    discover(&ReviewDiscoveryRequest {
        repo_path: root.to_path_buf(),
        branches: seeds.iter().map(|seed| (*seed).to_owned()).collect(),
        trunk: None,
    })
    .unwrap()
}

fn links(response: &ReviewDiscoveryResponse) -> Vec<(&str, &str)> {
    response
        .links
        .iter()
        .map(|link| (link.branch.as_str(), link.parent.as_str()))
        .collect()
}

#[test]
fn links_follow_recorded_parents_within_the_seeds_stack() {
    let (_dir, root, main) = repo();
    let response = ask(&root, &["b"]);
    // The seed's stack only: not the unrelated x.
    assert_eq!(links(&response), [("a", "main"), ("b", "a")]);
    assert_eq!(response.links[0].parent_oid.as_deref(), Some(main.as_str()));
    assert!(
        response.annotations.is_empty(),
        "{:?}",
        response.annotations
    );
    assert_eq!(response.related, Vec::<String>::new());
    // Without seeds, every tracked branch.
    assert_eq!(
        links(&ask(&root, &[])),
        [("a", "main"), ("b", "a"), ("x", "main")]
    );
    // An untracked seed has no Graphite stack.
    assert_eq!(ask(&root, &["untracked"]).links, Vec::new());
}

#[test]
fn a_moved_parent_needs_a_restack_and_invalid_metadata_is_flagged() {
    let (_dir, root, _) = repo();
    // a gains a commit after b was stacked on it: b needs a restack.
    commit_on(&root, "a", "a");
    track(&root, "x", &meta("main", "0000000", "BAD_PARENT_NAME"));
    let response = ask(&root, &[]);
    let codes: Vec<(&str, &str)> = response
        .annotations
        .iter()
        .map(|annotation| (annotation.branch.as_str(), annotation.code.as_str()))
        .collect();
    assert_eq!(
        codes,
        [
            ("b", "needs_restack"),
            ("x", "needs_restack"),
            ("x", "invalid_metadata")
        ]
    );
    assert!(response.annotations[0].message.contains("a moved"));
    assert!(
        response
            .annotations
            .iter()
            .all(|annotation| annotation.severity == AnnotationSeverity::Warning)
    );
}

#[test]
fn unreadable_metadata_is_skipped_and_repositories_without_graphite_answer_nothing() {
    let (_dir, root, _) = repo();
    track(&root, "b", "not json");
    assert_eq!(links(&ask(&root, &[])), [("a", "main"), ("x", "main")]);

    let bare = tempfile::tempdir().unwrap();
    run(
        bare.path(),
        &["init", "--quiet", "--initial-branch=main"],
        None,
    );
    assert_eq!(
        ask(bare.path(), &["main"]),
        ReviewDiscoveryResponse::default()
    );
}

#[test]
fn the_service_answers_its_manifest_and_discovery() {
    let (_dir, root, _) = repo();
    let request = |operation: &str, payload: Vec<u8>| ServiceRequest {
        interface_id: REVIEW_DISCOVERY_INTERFACE_ID.into(),
        operation: operation.into(),
        payload,
    };
    let response = invoke(&request(OP_REVIEW_DISCOVERY_MANIFEST, b"{}".to_vec()));
    let manifest: ReviewDiscoveryManifest = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(manifest.id, GRAPHITE_DISCOVERY_ID);
    let payload = serde_json::to_vec(&ReviewDiscoveryRequest {
        repo_path: root,
        branches: vec!["a".into()],
        trunk: None,
    })
    .unwrap();
    let response = invoke(&request(OP_REVIEW_DISCOVERY_DISCOVER, payload));
    assert!(response.error.is_none(), "{:?}", response.error);
    let answer: ReviewDiscoveryResponse = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(links(&answer), [("a", "main"), ("b", "a")]);
}
