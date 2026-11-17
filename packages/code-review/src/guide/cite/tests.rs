//! Citation checks against an in-memory review, and once against a repository.

use super::*;
use bcode_code_review_models::ReviewLine;

const BUDGET: &str = "\
//! Retry budgets.
use std::time::Duration;

pub struct RetryBudget {
    tokens: u32,
    max: u32,
}

impl RetryBudget {
    pub fn new(max: u32) -> Self {
        Self { tokens: max, max }
    }

    pub fn take(&mut self) -> bool {
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }

    pub fn refill(&mut self) {
        self.tokens = self.max;
    }
}

pub fn reset(budget: &mut RetryBudget) {
    budget.refill();
}

pub fn drain(budget: &mut RetryBudget) {
    budget.refill();
}
";

/// A review of one layer `#102` whose head adds `take` (lines 14-20) to `net/budget.rs`,
/// renames `net/old.rs` to `net/new.rs`, and deletes `net/gone.rs`; `net/http.rs` exists at
/// both sides but is not in the diff.
struct Fake {
    sources: Vec<CiteSource>,
    files: BTreeMap<String, Vec<DiffFile>>,
    hunks: BTreeMap<String, Vec<ReviewHunk>>,
    contents: BTreeMap<(RevisionRef, String), Vec<String>>,
    symbols: Vec<SymbolSite>,
}

fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_owned).collect()
}

fn base() -> RevisionRef {
    RevisionRef::commit("a".repeat(40))
}

fn head() -> RevisionRef {
    RevisionRef::commit("b".repeat(40))
}

fn diff_file(path: &str, old_path: Option<&str>, status: ReviewFileStatus) -> DiffFile {
    DiffFile {
        path: path.to_owned(),
        old_path: old_path.map(str::to_owned),
        status,
        old_blob: Some("old-blob".to_owned()),
        new_blob: Some("new-blob".to_owned()),
    }
}

fn layer(id: &str, number: u64) -> CiteSource {
    CiteSource {
        source_id: id.to_owned(),
        label: format!("#{number}"),
        number: Some(number),
        base: base(),
        head: head(),
        browse: false,
    }
}

fn fake() -> Fake {
    let budget = lines(BUDGET);
    let mut old_budget = budget.clone();
    old_budget.drain(13..20);
    let added: Vec<ReviewLine> = (14..=20)
        .map(|line| ReviewLine {
            kind: ReviewLineKind::Added,
            old_line: None,
            new_line: Some(line),
            content: budget[usize::try_from(line).expect("line") - 1].clone(),
        })
        .collect();
    let mut hunk_lines = vec![ReviewLine {
        kind: ReviewLineKind::Context,
        old_line: Some(12),
        new_line: Some(12),
        content: budget[11].clone(),
    }];
    hunk_lines.push(ReviewLine {
        kind: ReviewLineKind::Context,
        old_line: Some(13),
        new_line: Some(13),
        content: budget[12].clone(),
    });
    hunk_lines.extend(added);
    hunk_lines.push(ReviewLine {
        kind: ReviewLineKind::Context,
        old_line: Some(14),
        new_line: Some(21),
        content: budget[20].clone(),
    });
    let mut contents = BTreeMap::new();
    contents.insert((head(), "net/budget.rs".to_owned()), budget);
    contents.insert((base(), "net/budget.rs".to_owned()), old_budget);
    contents.insert(
        (head(), "net/new.rs".to_owned()),
        lines("fn renamed() {}\n"),
    );
    contents.insert(
        (base(), "net/old.rs".to_owned()),
        lines("fn renamed() {}\n"),
    );
    contents.insert((base(), "net/gone.rs".to_owned()), lines("fn gone() {}\n"));
    let http = lines("fn get() {\n    let budget = RetryBudget::new(3);\n}\n");
    contents.insert((head(), "net/http.rs".to_owned()), http.clone());
    contents.insert((base(), "net/http.rs".to_owned()), http);
    Fake {
        sources: vec![layer("pr-102", 102)],
        files: BTreeMap::from([(
            "pr-102".to_owned(),
            vec![
                diff_file("net/budget.rs", None, ReviewFileStatus::Modified),
                diff_file("net/new.rs", Some("net/old.rs"), ReviewFileStatus::Renamed),
                diff_file("net/gone.rs", None, ReviewFileStatus::Deleted),
            ],
        )]),
        hunks: BTreeMap::from([(
            "net/budget.rs".to_owned(),
            vec![ReviewHunk {
                old_start: 12,
                old_count: 3,
                new_start: 12,
                new_count: 10,
                heading: None,
                lines: hunk_lines,
            }],
        )]),
        contents,
        symbols: Vec::new(),
    }
}

impl CitationReader for Fake {
    fn sources(&self) -> &[CiteSource] {
        &self.sources
    }

    fn diff_files(&self, source_id: &str) -> Result<Vec<DiffFile>, String> {
        Ok(self.files.get(source_id).cloned().unwrap_or_default())
    }

    fn hunks(&self, _source_id: &str, file: &DiffFile) -> Result<Vec<ReviewHunk>, String> {
        Ok(self.hunks.get(&file.path).cloned().unwrap_or_default())
    }

    fn file(&self, revision: &RevisionRef, path: &str) -> Result<Option<Vec<String>>, String> {
        Ok(self
            .contents
            .get(&(revision.clone(), path.to_owned()))
            .cloned())
    }

    fn path_suggestions(&self, _revision: &RevisionRef, path: &str) -> Vec<String> {
        let name = path.rsplit('/').next().unwrap_or(path);
        self.contents
            .keys()
            .filter(|(_, candidate)| fuzzy_score(candidate, &name[..3]).is_some())
            .map(|(_, candidate)| candidate.clone())
            .collect()
    }

    fn symbol_sites(&self, _revision: &RevisionRef, symbol: &str) -> Vec<SymbolSite> {
        self.symbols
            .iter()
            .filter(|site| site.text.contains(symbol))
            .cloned()
            .collect()
    }
}

fn cite(path: &str, start_line: u32, end_line: u32, quote: Option<&str>) -> CitationRequest {
    CitationRequest {
        source: None,
        layer: None,
        path: path.to_owned(),
        side: ReviewLineSide::New,
        start_line,
        end_line,
        quote: quote.map(str::to_owned),
        symbol: None,
        relation_hint: None,
    }
}

fn codes(problems: &[CitationProblem]) -> Vec<&str> {
    problems
        .iter()
        .map(|problem| problem.code.as_str())
        .collect()
}

fn interval(start: u32, end: u32) -> ReviewLineInterval {
    ReviewLineInterval { start, end }
}

#[test]
fn a_quote_at_the_cited_lines_verifies_and_records_what_identifies_them() {
    let fake = fake();
    let check = check_citation(
        &fake,
        &cite(
            "net/budget.rs",
            14,
            20,
            Some("    pub fn take(&mut self) -> bool {\n        if self.tokens == 0 {"),
        ),
        7,
    );
    assert_eq!(check.status, CitationCheckStatus::Verified, "{check:?}");
    let citation = &check.citation;
    assert_eq!(citation.source_id.as_deref(), Some("pr-102"));
    assert_eq!(citation.revision, head());
    assert_eq!(citation.blob_oid.as_deref(), Some("new-blob"));
    assert_eq!(citation.lines, interval(14, 20));
    assert_eq!(citation.relation, CitationRelation::Changed);
    assert_eq!(
        citation.verification,
        Verification::Verified {
            at_ms: 7,
            adjusted_from: None
        }
    );
    let budget = lines(BUDGET);
    assert_eq!(citation.quote_hash, quote_hash(&budget[13..20]));
    assert!(
        citation
            .excerpt
            .starts_with("    pub fn take(&mut self) -> bool {")
    );
    assert_eq!(check.problems, []);
}

#[test]
fn a_quote_found_once_elsewhere_moves_the_citation_there() {
    let fake = fake();
    let check = check_citation(
        &fake,
        &cite("net/budget.rs", 3, 4, Some("pub fn refill(&mut self) {")),
        1,
    );
    assert_eq!(check.status, CitationCheckStatus::Adjusted, "{check:?}");
    assert_eq!(check.citation.lines, interval(22, 22));
    assert_eq!(
        check.citation.verification,
        Verification::Verified {
            at_ms: 1,
            adjusted_from: Some(interval(3, 4))
        }
    );
    assert_eq!(codes(&check.warnings), [GUIDE_CITE_ADJUSTED]);
}

#[test]
fn a_quote_found_several_times_elsewhere_lists_the_candidates() {
    let fake = fake();
    let check = check_citation(
        &fake,
        &cite("net/budget.rs", 1, 2, Some("budget.refill();")),
        1,
    );
    assert_eq!(check.status, CitationCheckStatus::Problem);
    assert_eq!(codes(&check.problems), [GUIDE_CITE_QUOTE_AMBIGUOUS]);
    assert_eq!(
        check.problems[0].suggestion["candidates"],
        json!([interval(28, 28), interval(32, 32)])
    );
    // Kept, at the lines the writer gave.
    assert_eq!(check.citation.lines, interval(1, 2));
    assert!(matches!(
        check.citation.verification,
        Verification::Problem { ref code, .. } if code == GUIDE_CITE_QUOTE_AMBIGUOUS
    ));
}

#[test]
fn a_near_quote_offers_the_closest_text() {
    let fake = fake();
    let check = check_citation(
        &fake,
        &cite(
            "net/budget.rs",
            1,
            1,
            Some("pub fn new(max: u64) -> Self {"),
        ),
        1,
    );
    assert_eq!(
        codes(&check.problems),
        [GUIDE_CITE_QUOTE_MISMATCH],
        "{check:?}"
    );
    let best = &check.problems[0].suggestion["best"];
    assert_eq!(best["range"], json!(interval(10, 10)));
    assert!(best["similarity"].as_f64().expect("similarity") >= QUOTE_SIMILARITY_THRESHOLD);
    assert_eq!(best["text"], "    pub fn new(max: u32) -> Self {");
}

#[test]
fn a_missing_quote_suggests_where_its_symbol_is_defined() {
    let mut fake = fake();
    fake.symbols.push(SymbolSite {
        path: "net/budget.rs".to_owned(),
        line: 3,
        text: "pub struct RetryBudget {".to_owned(),
    });
    let mut request = cite("net/budget.rs", 1, 1, Some("struct TokenBucket;"));
    request.symbol = Some("RetryBudget".to_owned());
    let check = check_citation(&fake, &request, 1);
    assert_eq!(codes(&check.problems), [GUIDE_CITE_QUOTE_NOT_FOUND]);
    assert_eq!(
        check.problems[0].suggestion["symbol_sites"][0]["line"],
        json!(3)
    );
}

#[test]
fn a_range_past_the_end_names_the_file_length() {
    let fake = fake();
    let check = check_citation(&fake, &cite("net/budget.rs", 30, 99, None), 1);
    assert_eq!(codes(&check.problems), [GUIDE_CITE_RANGE_OUT_OF_BOUNDS]);
    assert_eq!(check.problems[0].suggestion["file_lines"], json!(33));
    let reversed = check_citation(&fake, &cite("net/budget.rs", 5, 4, None), 1);
    assert_eq!(codes(&reversed.problems), [GUIDE_CITE_INVALID_RANGE]);
}

#[test]
fn renamed_and_deleted_files_say_which_path_and_side_to_cite() {
    let fake = fake();
    let renamed = check_citation(&fake, &cite("net/old.rs", 1, 1, None), 1);
    assert_eq!(codes(&renamed.problems), [GUIDE_CITE_RENAMED]);
    assert_eq!(renamed.problems[0].suggestion["to"], "net/new.rs");
    // The old side of a renamed file reads its old path.
    let mut old_side = cite("net/new.rs", 1, 1, Some("fn renamed() {}"));
    old_side.side = ReviewLineSide::Old;
    let old_side = check_citation(&fake, &old_side, 1);
    assert_eq!(
        old_side.status,
        CitationCheckStatus::Verified,
        "{old_side:?}"
    );
    assert_eq!(old_side.citation.path, "net/old.rs");
    assert_eq!(old_side.citation.revision, base());
    let deleted = check_citation(&fake, &cite("net/gone.rs", 1, 1, None), 1);
    assert_eq!(codes(&deleted.problems), [GUIDE_CITE_DELETED_ON_SIDE]);
    assert_eq!(deleted.problems[0].suggestion["use_side"], "old");
}

#[test]
fn an_unknown_path_suggests_paths_that_exist() {
    let fake = fake();
    let check = check_citation(&fake, &cite("net/budgte.rs", 1, 1, None), 1);
    assert_eq!(codes(&check.problems), [GUIDE_CITE_PATH_NOT_FOUND]);
    let suggestions = check.problems[0].suggestion["suggestions"]
        .as_array()
        .expect("suggestions");
    assert!(suggestions.contains(&json!("net/budget.rs")));
}

#[test]
fn code_outside_the_diff_is_outside_the_review() {
    let fake = fake();
    let check = check_citation(
        &fake,
        &cite(
            "net/http.rs",
            2,
            2,
            Some("let budget = RetryBudget::new(3);"),
        ),
        1,
    );
    assert_eq!(check.status, CitationCheckStatus::Verified, "{check:?}");
    assert_eq!(check.citation.relation, CitationRelation::OutsideReview);
    // Repository context reads the primary source's head, with no source of its own.
    assert_eq!(check.citation.source_id, None);
    assert_eq!(check.citation.revision, head());
}

#[test]
fn diff_lines_without_changes_warn_but_are_kept() {
    let fake = fake();
    let mut context = cite("net/budget.rs", 12, 13, None);
    context.relation_hint = Some(CitationRelationHint::Diff);
    let check = check_citation(&fake, &context, 1);
    assert_eq!(check.citation.relation, CitationRelation::HunkContext);
    assert_eq!(check.status, CitationCheckStatus::Unverified);
    assert_eq!(
        codes(&check.warnings),
        [GUIDE_CITE_ADD_QUOTE, GUIDE_CITE_NOT_IN_DIFF]
    );
    let mut far = cite("net/budget.rs", 2, 2, Some("use std::time::Duration;"));
    far.relation_hint = Some(CitationRelationHint::Diff);
    let far = check_citation(&fake, &far, 1);
    assert_eq!(far.status, CitationCheckStatus::Verified);
    assert_eq!(far.citation.relation, CitationRelation::Unchanged);
    assert_eq!(codes(&far.warnings), [GUIDE_CITE_NOT_IN_DIFF]);
}

#[test]
fn sources_are_named_by_layer_and_ambiguity_is_reported() {
    let mut fake = fake();
    fake.sources.push(layer("pr-103", 103));
    fake.files.insert(
        "pr-103".to_owned(),
        vec![diff_file("net/budget.rs", None, ReviewFileStatus::Modified)],
    );
    let ambiguous = check_citation(&fake, &cite("net/budget.rs", 1, 1, None), 1);
    assert_eq!(codes(&ambiguous.problems), [GUIDE_CITE_AMBIGUOUS_SOURCE]);
    assert_eq!(
        ambiguous.problems[0].suggestion["candidates"],
        json!(["#102", "#103"])
    );
    let mut named = cite("net/budget.rs", 1, 1, None);
    named.layer = Some("#103".to_owned());
    let named = check_citation(&fake, &named, 1);
    assert!(named.problems.is_empty(), "{named:?}");
    assert_eq!(named.citation.source_id.as_deref(), Some("pr-103"));
    let mut unknown = cite("net/budget.rs", 1, 1, None);
    unknown.layer = Some("#7".to_owned());
    let unknown = check_citation(&fake, &unknown, 1);
    assert_eq!(codes(&unknown.problems), [GUIDE_CITE_UNKNOWN_LAYER]);
    assert_eq!(
        unknown.problems[0].suggestion["layers"],
        json!(["#102", "#103"])
    );
}

#[test]
fn quotes_compare_without_trailing_whitespace_or_indentation() {
    let fake = fake();
    let check = check_citation(
        &fake,
        &cite(
            "net/budget.rs",
            15,
            17,
            Some("\nif self.tokens == 0 {   \r\n  return false;\n\n"),
        ),
        1,
    );
    assert_eq!(check.status, CitationCheckStatus::Verified, "{check:?}");
}

#[test]
fn citations_are_read_from_a_repository_at_the_pinned_revisions() {
    use std::process::Command;
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().to_path_buf();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git");
        assert!(output.status.success(), "{args:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(root.join("lib.rs"), "fn one() {}\n").expect("write");
    std::fs::write(root.join("other.rs"), "fn other() {}\n").expect("write");
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "base"]);
    let base = git(&["rev-parse", "HEAD"]);
    std::fs::write(root.join("lib.rs"), "fn one() {}\nfn two() {}\n").expect("write");
    git(&["commit", "-q", "-am", "head"]);
    let head = git(&["rev-parse", "HEAD"]);
    let source = ResolvedSource {
        source_id: "range".to_owned(),
        base: RevisionRef::commit(base),
        head: RevisionRef::commit(head.clone()),
        mode: ResolvedSourceMode::Diff,
        paths: Vec::new(),
        line_range: None,
        part: None,
        selection: None,
    };
    let data = ReviewData::default();
    let reader = ReviewDataReader::new(
        &data,
        root.clone(),
        &[source],
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let checks = check_citations(
        &reader,
        &[
            cite("lib.rs", 2, 2, Some("fn two() {}")),
            cite("other.rs", 1, 1, Some("fn other() {}")),
            cite("missing.rs", 1, 1, None),
        ],
        1,
    );
    assert_eq!(
        checks[0].status,
        CitationCheckStatus::Verified,
        "{:?}",
        checks[0]
    );
    assert_eq!(checks[0].citation.relation, CitationRelation::Changed);
    assert_eq!(checks[0].citation.revision, RevisionRef::commit(head));
    assert!(checks[0].citation.blob_oid.is_some());
    assert_eq!(checks[1].citation.relation, CitationRelation::OutsideReview);
    assert_eq!(checks[1].status, CitationCheckStatus::Verified);
    assert_eq!(codes(&checks[2].problems), [GUIDE_CITE_PATH_NOT_FOUND]);
}

#[test]
fn a_quote_longer_than_is_compared_is_kept_unverified_not_verified_by_its_start() {
    let mut fake = fake();
    let file: Vec<String> = (1..=260)
        .map(|line| format!("let x{line} = {line};"))
        .collect();
    let mut quote: Vec<String> = file[..200].to_vec();
    quote.extend((0..50).map(|line| format!("invented {line}")));
    fake.contents
        .insert((head(), "net/long.rs".to_owned()), file);
    let check = check_citation(
        &fake,
        &cite("net/long.rs", 1, 250, Some(&quote.join("\n"))),
        7,
    );
    assert_eq!(check.status, CitationCheckStatus::Problem, "{check:?}");
    assert_eq!(codes(&check.problems), ["quote_too_long"]);
    assert!(
        matches!(&check.citation.verification, Verification::Problem { code, .. } if code == "quote_too_long"),
        "{:?}",
        check.citation.verification
    );
    assert_eq!(
        check.citation.lines,
        interval(1, 250),
        "the citation is kept as asked"
    );
}
