//! Refresh forks and since checks against a repository whose layer was pushed again after a
//! guide cited it.

use super::super::freshness::assess_version;
use super::super::text::{excerpt, quote_hash};
use super::*;
use bcode_code_review_models::{
    GuideOrigin, REVIEW_GUIDE_SCHEMA_VERSION, ResolvedSourceMode, ReviewLineInterval, SectionKind,
    SectionState, Verification, VersionPoint,
};
use std::path::PathBuf;
use std::process::Command;

const TEN: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Self {
            root: dir.path().to_path_buf(),
            _dir: dir,
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
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
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn commit(&self, path: &str, text: &str) -> String {
        std::fs::write(self.root.join(path), text).expect("write");
        self.git(&["add", path]);
        self.git(&["commit", "-q", "-m", path]);
        self.git(&["rev-parse", "HEAD"])
    }
}

fn file(text: &str) -> Vec<String> {
    text.lines().map(str::to_owned).collect()
}

fn citation(id: &str, content: &[String], start: u32, end: u32, revision: &str) -> Citation {
    let range = &content[start as usize - 1..end as usize];
    Citation {
        id: id.to_owned(),
        source_id: Some("layer".to_owned()),
        path: "b.txt".to_owned(),
        side: ReviewLineSide::New,
        lines: ReviewLineInterval { start, end },
        revision: RevisionRef::commit(revision),
        blob_oid: None,
        quote_hash: quote_hash(range),
        excerpt: excerpt(range),
        relation: CitationRelation::Changed,
        verification: Verification::Unverified,
    }
}

fn section(id: &str, citations: &[&str]) -> Section {
    Section {
        id: id.to_owned(),
        kind: SectionKind::Step,
        title: id.to_owned(),
        body: String::new(),
        findings: Vec::new(),
        citations: citations.iter().map(|id| (*id).to_owned()).collect(),
        children: Vec::new(),
        state: SectionState::Written,
        refreshed_from: None,
        checks: Vec::new(),
    }
}

fn source(base: &str, head: &str) -> ResolvedSource {
    ResolvedSource {
        source_id: "layer".to_owned(),
        base: RevisionRef::commit(base),
        head: RevisionRef::commit(head),
        mode: ResolvedSourceMode::Diff,
        paths: Vec::new(),
        line_range: None,
        part: None,
        selection: None,
    }
}

/// A guide of two sections: `s1` cites lines 2-3, `s2` cites lines 7-8.
fn guide(trunk: &str, layer: &str) -> GuideVersion {
    let content = file(TEN);
    GuideVersion {
        guide_id: "g".to_owned(),
        version: 1,
        schema_version: REVIEW_GUIDE_SCHEMA_VERSION,
        heads: heads_of(&[source(trunk, layer)], &BTreeMap::new()),
        based_on: None,
        origin: GuideOrigin {
            session_id: Some("writer".to_owned()),
            ..GuideOrigin::default()
        },
        status: GuideStatus::Complete,
        generation: None,
        outline: vec![section("s1", &["c1"]), section("s2", &["c2"])],
        citations: BTreeMap::from([
            ("c1".to_owned(), citation("c1", &content, 2, 3, layer)),
            ("c2".to_owned(), citation("c2", &content, 7, 8, layer)),
        ]),
        retired_sections: Vec::new(),
        created_at_ms: 0,
        updated_at_ms: 0,
    }
}

#[test]
fn a_push_changing_cited_lines_stales_only_their_sections_and_a_refresh_forks_the_rest() {
    let repo = Repo::new();
    let trunk = repo.commit("a.txt", "a\n");
    repo.git(&["checkout", "-q", "-b", "layer"]);
    let layer = repo.commit("b.txt", TEN);
    let v1 = guide(&trunk, &layer);
    let data = ReviewData::default();

    // A rebase-only push keeps the whole guide fresh.
    repo.git(&["checkout", "-q", "main"]);
    let moved_trunk = repo.commit("a.txt", "a\nmore\n");
    repo.git(&["checkout", "-q", "-b", "rebased", &moved_trunk]);
    repo.git(&["cherry-pick", &layer]);
    let rebased = repo.git(&["rev-parse", "HEAD"]);
    let after_rebase =
        assess_version(&data, &repo.root, &v1, &[source(&moved_trunk, &rebased)]).expect("assess");
    assert!(after_rebase.stale_sections.is_empty(), "{after_rebase:?}");

    // A push adds a line on top and edits line 8: s2 is stale, s1 only moved.
    repo.git(&["checkout", "-q", "-b", "pushed", &layer]);
    let pushed_text = format!("zero\n{}", TEN.replace("eight", "EIGHT"));
    let pushed = repo.commit("b.txt", &pushed_text);
    let current = [source(&trunk, &pushed)];
    let freshness = assess_version(&data, &repo.root, &v1, &current).expect("assess");
    assert_eq!(freshness.stale_sections, ["s2"]);
    assert_eq!(
        freshness.citations["c1"],
        Freshness::Moved {
            lines: ReviewLineInterval { start: 3, end: 4 }
        }
    );
    assert!(!pinned_to(&v1, &heads_of(&current, &BTreeMap::new())));

    // Refreshing forks v2: pinned to the push, s1's citation remapped, s2 kept to rewrite.
    let (v2, stale) = refresh_fork(&v1, &freshness, &current, &BTreeMap::new(), 9);
    assert_eq!(stale, ["s2"]);
    assert_eq!(v2.version, 2);
    assert_eq!(v2.based_on, Some(1));
    assert_eq!(v2.origin.session_id, None);
    assert!(pinned_to(&v2, &heads_of(&current, &BTreeMap::new())));
    assert_eq!(v2.outline[0].refreshed_from, Some(1));
    assert_eq!(v2.outline[1].refreshed_from, None);
    let remapped = &v2.citations["c1"];
    assert_eq!(remapped.revision, RevisionRef::commit(&pushed));
    assert_eq!(remapped.lines, ReviewLineInterval { start: 3, end: 4 });
    assert_eq!(v2.citations["c2"], v1.citations["c2"]);
    // v1 is untouched (it stays viewable as it was).
    assert_eq!(v1.citations["c1"].revision, RevisionRef::commit(&layer));

    // v2 against the push: the remapped section is fresh; the kept one is still stale.
    let v2_fresh = assess_version(&data, &repo.root, &v2, &current).expect("assess");
    assert_eq!(v2_fresh.citations["c1"], Freshness::Fresh);
    assert_eq!(v2_fresh.stale_sections, ["s2"]);
}

#[test]
fn a_since_guide_cites_only_lines_new_since_the_baseline() {
    let repo = Repo::new();
    let trunk = repo.commit("a.txt", "a\n");
    repo.git(&["checkout", "-q", "-b", "layer"]);
    let looked = repo.commit("b.txt", TEN);
    let pushed_text = TEN.replace("eight", "EIGHT");
    let pushed = repo.commit("b.txt", &pushed_text);
    let pair = LayerPair {
        number: 7,
        from: Some(VersionPoint {
            head_oid: looked,
            fork_oid: Some(trunk.clone()),
        }),
        to: VersionPoint {
            head_oid: pushed.clone(),
            fork_oid: Some(trunk),
        },
    };
    let data = ReviewData::default();
    let numbers = BTreeMap::from([("layer".to_owned(), 7)]);
    let since = SinceDelta::compute(&data, &repo.root, std::slice::from_ref(&pair), &numbers)
        .expect("delta");
    let content = file(&pushed_text);
    let new_line = citation("c1", &content, 7, 8, &pushed);
    assert_eq!(since.outside(&data, &repo.root, &new_line, None), None);
    let old_line = citation("c2", &content, 2, 3, &pushed);
    let refused = since
        .outside(&data, &repo.root, &old_line, None)
        .expect("not new since the baseline");
    assert!(refused.contains("new lines there: 8"), "{refused}");
    // A guide written for the code last looked at is the one a since guide links; a since
    // guide is pinned to the code now.
    let earlier = guide(
        &trunk_of(&pair),
        pair.from.as_ref().map_or("", |from| &from.head_oid),
    );
    assert!(written_for_baseline(&earlier, std::slice::from_ref(&pair)));
    let (since_row, since_version) = since_guide(
        "review",
        GuideScope::Since {
            baseline: bcode_code_review_models::ReviewCompareBaseline::LastLooked,
            pairs: vec![pair.clone()],
        },
        "Changes since you last looked",
        (&[source(&trunk_of(&pair), &pushed)], &BTreeMap::new()),
        None,
        5,
    );
    assert_eq!(since_row.guide_id, since_version.guide_id);
    assert_eq!(since_version.status, GuideStatus::Planning);
    assert!(!written_for_baseline(
        &since_version,
        std::slice::from_ref(&pair)
    ));
    let mut elsewhere = new_line;
    elsewhere.source_id = Some("other".to_owned());
    assert!(since.outside(&data, &repo.root, &elsewhere, None).is_some());
}

fn trunk_of(pair: &LayerPair) -> String {
    pair.to.fork_oid.clone().unwrap_or_default()
}
