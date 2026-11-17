//! Freshness from known facts, and against a repository whose layer was rebased, edited, and
//! deleted after a guide cited it.

use super::super::text::{excerpt, quote_hash};
use super::*;
use bcode_code_review_models::{
    CitationRelation, GuideHead, GuideOrigin, GuideStatus, REVIEW_GUIDE_SCHEMA_VERSION,
    ResolvedSourceMode, SectionKind, SectionState, Verification,
};
use std::path::PathBuf;
use std::process::Command;

fn file(text: &str) -> Vec<String> {
    text.lines().map(str::to_owned).collect()
}

fn interval(start: u32, end: u32) -> ReviewLineInterval {
    ReviewLineInterval { start, end }
}

/// A citation of `lines` of `content` at `revision`.
fn citation(content: &[String], lines: ReviewLineInterval, revision: RevisionRef) -> Citation {
    let range = &content[lines.start as usize - 1..lines.end as usize];
    Citation {
        id: "c1".to_owned(),
        source_id: Some("layer".to_owned()),
        path: "b.txt".to_owned(),
        side: ReviewLineSide::New,
        lines,
        revision,
        blob_oid: None,
        quote_hash: quote_hash(range),
        excerpt: excerpt(range),
        relation: CitationRelation::Changed,
        verification: Verification::Unverified,
    }
}

const TEN: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";

#[test]
fn known_facts_decide_freshness() {
    let before = file(TEN);
    let cited = citation(&before, interval(4, 6), RevisionRef::WorkingTree);
    let same = FreshnessInput {
        same_content: true,
        ..FreshnessInput::default()
    };
    assert_eq!(citation_freshness(&cited, same), Freshness::Fresh);
    assert_eq!(
        citation_freshness(&cited, FreshnessInput::default()),
        Freshness::Gone
    );
    let shifted = file(&format!("zero\nhalf\n{TEN}"));
    let found = |lines: &[String], changed: Option<&[LineSpan]>| {
        citation_freshness(
            &cited,
            FreshnessInput {
                same_content: false,
                lines: Some(lines),
                changed,
            },
        )
    };
    assert_eq!(
        found(&shifted, None),
        Freshness::Moved {
            lines: interval(6, 8)
        }
    );
    assert_eq!(
        found(&shifted, Some(&[(1, 2)])),
        Freshness::Moved {
            lines: interval(6, 8)
        }
    );
    // Found again, but a change since the pinned version touches it.
    assert_eq!(
        found(&before, Some(&[(5, 5)])),
        Freshness::Stale {
            reason: "line 5 changed since the guide was written".to_owned()
        }
    );
    let edited = file(&TEN.replace("five", "FIVE"));
    assert_eq!(found(&edited, None), Freshness::Gone);
    assert_eq!(
        found(&edited, Some(&[(5, 5)])),
        Freshness::Stale {
            reason: "line 5 changed since the guide was written".to_owned()
        }
    );
}

#[test]
fn text_found_twice_is_placed_nearest_its_old_lines() {
    let content = file("a\nb\nx\na\nb\n");
    let cited = citation(&content, interval(4, 5), RevisionRef::WorkingTree);
    assert_eq!(refind(&cited, &content), Some(interval(4, 5)));
    let earlier = file("a\nb\nx\nx\nx\nx\nx\nx\na\nb\n");
    assert_eq!(refind(&cited, &earlier), Some(interval(1, 2)));
}

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Repo {
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

    fn commit(&self, path: &str, text: Option<&str>) -> String {
        match text {
            Some(text) => {
                std::fs::write(self.root.join(path), text).expect("write");
                self.git(&["add", path]);
            }
            None => {
                self.git(&["rm", "-q", path]);
            }
        }
        self.git(&["commit", "-q", "-m", path]);
        self.git(&["rev-parse", "HEAD"])
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

#[test]
fn a_guide_stays_fresh_through_a_rebase_and_notices_edits_and_deletions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repo {
        root: dir.path().to_path_buf(),
        _dir: dir,
    };
    repo.git(&["init", "-q", "-b", "main"]);
    let trunk = repo.commit("a.txt", Some("a\n"));
    repo.git(&["checkout", "-q", "-b", "layer"]);
    let layer = repo.commit("b.txt", Some(TEN));
    // The guide cites lines 4-6 of the layer's new file.
    let lines = interval(4, 6);
    let cited = citation(&file(TEN), lines, RevisionRef::commit(&layer));
    let version = GuideVersion {
        guide_id: "g".to_owned(),
        version: 1,
        schema_version: REVIEW_GUIDE_SCHEMA_VERSION,
        heads: BTreeMap::from([(
            "layer".to_owned(),
            GuideHead {
                base: RevisionRef::commit(&trunk),
                head: RevisionRef::commit(&layer),
                label: "#1".to_owned(),
            },
        )]),
        based_on: None,
        origin: GuideOrigin::default(),
        status: GuideStatus::Complete,
        generation: None,
        outline: vec![Section {
            id: "s1".to_owned(),
            kind: SectionKind::Step,
            title: "Numbers".to_owned(),
            body: String::new(),
            findings: Vec::new(),
            citations: vec!["c1".to_owned()],
            children: Vec::new(),
            state: SectionState::Written,
            refreshed_from: None,
            checks: Vec::new(),
        }],
        citations: BTreeMap::from([("c1".to_owned(), cited)]),
        created_at_ms: 0,
        updated_at_ms: 0,
    };
    let data = ReviewData::default();
    let assess = |base: &str, head: &str| {
        assess_version(&data, &repo.root, &version, &[source(base, head)]).expect("assess")
    };

    // The pinned revisions themselves.
    let pinned = assess(&trunk, &layer);
    assert_eq!(pinned.citations["c1"], Freshness::Fresh);

    // The trunk moves and the layer is rebased onto it: only what moved underneath changed, and
    // the cited lines (at a new commit) are fresh by the delta.
    repo.git(&["checkout", "-q", "main"]);
    let moved_trunk = repo.commit("a.txt", Some("a\nmore\n"));
    repo.git(&["checkout", "-q", "-b", "rebased", &moved_trunk]);
    repo.git(&["cherry-pick", &layer]);
    let rebased = repo.git(&["rev-parse", "HEAD"]);
    let after_rebase = assess(&moved_trunk, &rebased);
    assert_eq!(after_rebase.citations["c1"], Freshness::Fresh);
    assert_eq!(after_rebase.stale_sections, [] as [String; 0]);

    // The author adds lines above the cited ones: moved, not stale.
    repo.git(&["checkout", "-q", "-b", "above", &layer]);
    let above = repo.commit("b.txt", Some(&format!("zero\nhalf\n{TEN}")));
    assert_eq!(
        assess(&trunk, &above).citations["c1"],
        Freshness::Moved {
            lines: interval(6, 8)
        }
    );

    // The author edits a cited line: stale, and so is its section.
    repo.git(&["checkout", "-q", "-b", "edited", &layer]);
    let edited = repo.commit("b.txt", Some(&TEN.replace("five", "FIVE")));
    let stale = assess(&trunk, &edited);
    assert_eq!(
        stale.citations["c1"],
        Freshness::Stale {
            reason: "line 5 changed since the guide was written".to_owned()
        }
    );
    assert_eq!(stale.stale_sections, ["s1"]);

    // The file is deleted: gone.
    repo.git(&["checkout", "-q", "-b", "deleted", &layer]);
    let deleted = repo.commit("b.txt", None);
    assert_eq!(assess(&trunk, &deleted).citations["c1"], Freshness::Gone);

    // The source left the review: gone.
    let elsewhere = assess_version(&data, &repo.root, &version, &[]).expect("assess");
    assert_eq!(elsewhere.citations["c1"], Freshness::Gone);
}
