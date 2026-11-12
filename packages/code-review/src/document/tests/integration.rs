//! End-to-end: the document driven by the real data layer over a generated repository.
//! Expanded context must match the reviewed revision on the correct side (criterion A).

use super::super::{
    DocumentConfig, DocumentNeed, FileRef, LayoutMode, LineContent, LineKind, ReviewDocument,
    ReviewItem, SourceInput,
};
use crate::ReviewData;
use crate::testing::fixture::{BASE_BRANCH, FixtureSpec, GitFixture, HEAD_BRANCH};
use bcode_code_review_models::{
    ResolveReviewSourcesRequest, ResolvedSource, ReviewIndexFlag, ReviewIndexRequest, ReviewSource,
    ReviewSourceKind, RevisionRef,
};
use std::collections::BTreeMap;
use std::path::Path;

fn serve(service: &mut ReviewData, doc: &mut ReviewDocument) {
    for _ in 0..1_000 {
        let needs = doc.take_needs();
        if needs.is_empty() {
            return;
        }
        for need in needs {
            let ticket = need.ticket();
            let applied = match &need {
                DocumentNeed::Hunks { request, .. } => match service.file_hunks_get(request) {
                    Ok(response) => doc.accept_hunks(ticket, &response),
                    Err(error) => doc.fail(ticket, &error.to_string()),
                },
                DocumentNeed::BlobLines { request, .. } => match service.blob_lines_get(request) {
                    Ok(response) => doc.accept_blob_lines(ticket, &response),
                    Err(error) => doc.fail(ticket, &error.to_string()),
                },
                DocumentNeed::Attribution { request, .. } => {
                    match service.stack_attribution(request) {
                        Ok(response) => doc.accept_attribution(ticket, response),
                        Err(error) => doc.fail(ticket, &error.to_string()),
                    }
                }
            };
            assert!(applied, "{need:?}");
        }
    }
    panic!("document never settled");
}

/// Lines of `path` at `revision`, as the service presents them.
fn lines_at(fixture: &GitFixture, revision: &RevisionRef, path: &str) -> Vec<String> {
    let text = match revision {
        RevisionRef::Commit { oid } => fixture.git(&["show", &format!("{oid}:{path}")]),
        RevisionRef::Index => fixture.git(&["show", &format!(":{path}")]),
        RevisionRef::WorkingTree => {
            std::fs::read_to_string(fixture.root().join(path)).expect("working tree file")
        }
        RevisionRef::EmptyTree => String::new(),
    };
    text.lines()
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect()
}

fn resolve(
    service: &mut ReviewData,
    repo: &Path,
    sources: Vec<ReviewSource>,
) -> Vec<ResolvedSource> {
    service
        .resolve_sources(&ResolveReviewSourcesRequest {
            repo_path: repo.to_path_buf(),
            sources,
            provider_resolutions: Vec::new(),
        })
        .expect("resolve")
        .sources
}

fn inputs(
    service: &mut ReviewData,
    repo: &Path,
    resolved: Vec<ResolvedSource>,
) -> Vec<SourceInput> {
    resolved
        .into_iter()
        .map(|source| {
            let entries = service
                .index_get(&ReviewIndexRequest {
                    repo_path: repo.to_path_buf(),
                    source: source.clone(),
                    cursor: None,
                    limit: Some(5_000),
                })
                .expect("index")
                .entries;
            SourceInput {
                label: source.source_id.clone(),
                source,
                entries,
                stack: None,
            }
        })
        .collect()
}

#[test]
fn expanded_context_matches_the_reviewed_revision_for_every_source_kind() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let source = |id: &str, kind: ReviewSourceKind| ReviewSource {
        id: id.to_string(),
        label: kind.label(),
        kind,
        included: true,
    };
    let resolved = resolve(
        &mut service,
        repo,
        vec![
            source(
                "branch",
                ReviewSourceKind::BranchCompare {
                    base_branch: BASE_BRANCH.into(),
                    head_branch: HEAD_BRANCH.into(),
                    merge_base: true,
                },
            ),
            source("staged", ReviewSourceKind::IndexStaged),
            source("unstaged", ReviewSourceKind::WorkingTreeUnstaged),
        ],
    );
    assert_eq!(resolved.len(), 3);
    let by_id: BTreeMap<String, ResolvedSource> = resolved
        .iter()
        .map(|source| (source.source_id.clone(), source.clone()))
        .collect();
    let inputs = inputs(&mut service, repo, resolved);
    let mut doc = ReviewDocument::new(repo.to_path_buf(), inputs, DocumentConfig::default());
    let files: Vec<_> = doc.file_keys().cloned().collect();
    for file in &files {
        doc.reveal_file(file);
    }
    for _ in 0..20 {
        let len = doc.len();
        doc.set_viewport(0, len - 1);
        serve(&mut service, &mut doc);
        if doc.len() == len {
            break;
        }
    }

    let mut cache: BTreeMap<(RevisionRef, String), Vec<String>> = BTreeMap::new();
    let mut lines_at = |revision: &RevisionRef, path: &str| -> Vec<String> {
        cache
            .entry((revision.clone(), path.to_string()))
            .or_insert_with(|| lines_at(&fixture, revision, path))
            .clone()
    };
    let mut checked = 0;
    let mut gaps = 0;
    for index in 0..doc.len() {
        let item = doc.item(index).expect("item");
        if matches!(item, ReviewItem::Gap(_)) {
            gaps += 1;
        }
        let ReviewItem::Line(line) = item else {
            continue;
        };
        let entry = doc.entry(&line.file).expect("entry").clone();
        if !entry.flags.is_empty()
            && !entry
                .flags
                .contains(&bcode_code_review_models::ReviewIndexFlag::ModeChanged)
        {
            continue;
        }
        let LineContent::Text(text) = &line.content else {
            panic!("unloaded line after settling: {line:?}");
        };
        let source = &by_id[&line.file.source_id];
        if let Some(old) = line.old_line {
            let old_path = entry.old_path.as_deref().unwrap_or(&entry.path);
            let lines = lines_at(&source.base, old_path);
            assert_eq!(
                &lines[(old - 1) as usize],
                &**text,
                "{}: old {old}",
                line.file.path
            );
        }
        if let Some(new) = line.new_line {
            let lines = lines_at(&source.head, &entry.path);
            assert_eq!(
                &lines[(new - 1) as usize],
                &**text,
                "{}: new {new}",
                line.file.path
            );
        }
        checked += 1;
    }
    assert!(checked > 500, "checked {checked} lines");
    assert_eq!(gaps, 0, "every file was fully revealed");
    assert_eq!(doc.take_needs(), Vec::new());
}

/// One line of a whole-file diff: old line, new line, text.
type OracleLine = (Option<u32>, Option<u32>, String);

/// The whole-file diff of one file from `git diff -U<large>`, parsed independently of the
/// service: every line of the file with both line numbers, in diff order.
fn oracle(
    fixture: &GitFixture,
    source: &ResolvedSource,
    path: &str,
    old_path: Option<&str>,
) -> Vec<OracleLine> {
    let mut args: Vec<String> = [
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--find-renames",
        "--unified=100000000",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    match (&source.base, &source.head) {
        (RevisionRef::Commit { oid: base }, RevisionRef::Commit { oid: head }) => {
            args.extend([base.clone(), head.clone()]);
        }
        (RevisionRef::Commit { oid: base }, RevisionRef::Index) => {
            args.extend(["--cached".to_string(), base.clone()]);
        }
        (RevisionRef::Commit { oid: base }, RevisionRef::WorkingTree) => args.push(base.clone()),
        (RevisionRef::Index, RevisionRef::WorkingTree) => {}
        other => panic!("unsupported revision pair {other:?}"),
    }
    args.push("--".to_string());
    if let Some(old_path) = old_path.filter(|old| *old != path) {
        args.push(old_path.to_string());
    }
    args.push(path.to_string());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let diff = fixture.git(&args);
    let mut out = Vec::new();
    let (mut old, mut new) = (0_u32, 0_u32);
    let mut in_hunk = false;
    for line in diff.lines() {
        if let Some(header) = line.strip_prefix("@@ -") {
            let (old_part, rest) = header.split_once(" +").expect("hunk header");
            let new_part = rest.split_once(' ').expect("hunk header").0;
            let start = |part: &str| -> u32 {
                part.split(',')
                    .next()
                    .expect("start")
                    .parse()
                    .expect("number")
            };
            // An empty side `N,0` counts from after line N.
            old = start(old_part);
            new = start(new_part);
            if old_part.ends_with(",0") {
                old += 1;
            }
            if new_part.ends_with(",0") {
                new += 1;
            }
            in_hunk = true;
            continue;
        }
        if !in_hunk || line.starts_with('\\') {
            continue;
        }
        let (marker, text) = line.split_at(1);
        match marker {
            " " => {
                out.push((Some(old), Some(new), text.to_string()));
                old += 1;
                new += 1;
            }
            "-" => {
                out.push((Some(old), None, text.to_string()));
                old += 1;
            }
            "+" => {
                out.push((None, Some(new), text.to_string()));
                new += 1;
            }
            _ => in_hunk = false,
        }
    }
    out
}

/// Lines and gaps of one file in document order, gaps expanded to the lines they hide (both
/// line numbers derived by the document), with the text of shown lines.
fn shown(doc: &ReviewDocument, file: &FileRef) -> Vec<(Option<u32>, Option<u32>, Option<String>)> {
    let first = doc
        .index_of(&super::super::ItemKey::FileHeader(file.clone()))
        .expect("header");
    let mut out = Vec::new();
    for index in first + 1..doc.len() {
        let item = doc.item(index).expect("item");
        if matches!(
            item,
            ReviewItem::FileHeader(_) | ReviewItem::SourceHeader(_)
        ) {
            break;
        }
        match item {
            ReviewItem::Line(line) => {
                out.push((
                    line.old_line,
                    line.new_line,
                    line.content.text().map(str::to_string),
                ));
            }
            ReviewItem::Gap(gap) => {
                let count = gap.hidden_lines().expect("gaps are closed after settling");
                let old = gap.old_range().map(|(start, _)| start);
                let new = gap.new_range().map(|(start, _)| start);
                for offset in 0..count {
                    out.push((old.map(|o| o + offset), new.map(|n| n + offset), None));
                }
            }
            _ => {}
        }
    }
    out
}

fn settle(service: &mut ReviewData, doc: &mut ReviewDocument) {
    for _ in 0..20 {
        let len = doc.len();
        doc.set_viewport(0, len - 1);
        serve(service, doc);
        if doc.len() == len {
            break;
        }
    }
}

/// Gap line numbers, revealed context, and side-by-side rows agree with `git diff -U<large>`
/// for every text file of a branch compare, a staged, and an unstaged source.
#[test]
fn gaps_and_revealed_context_agree_with_a_whole_file_git_diff() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let repo = fixture.root();
    let mut service = ReviewData::default();
    let source = |id: &str, kind: ReviewSourceKind| ReviewSource {
        id: id.to_string(),
        label: kind.label(),
        kind,
        included: true,
    };
    let resolved = resolve(
        &mut service,
        repo,
        vec![
            source(
                "branch",
                ReviewSourceKind::BranchCompare {
                    base_branch: BASE_BRANCH.into(),
                    head_branch: HEAD_BRANCH.into(),
                    merge_base: true,
                },
            ),
            source("staged", ReviewSourceKind::IndexStaged),
            source("unstaged", ReviewSourceKind::WorkingTreeUnstaged),
        ],
    );
    let by_id: BTreeMap<String, ResolvedSource> = resolved
        .iter()
        .map(|source| (source.source_id.clone(), source.clone()))
        .collect();
    let inputs = inputs(&mut service, repo, resolved);
    let mut doc = ReviewDocument::new(repo.to_path_buf(), inputs, DocumentConfig::default());
    settle(&mut service, &mut doc);

    let files: Vec<FileRef> = doc
        .file_keys()
        .filter(|file| {
            let entry = doc.entry(file).expect("entry");
            !entry.has(ReviewIndexFlag::Binary) && !entry.has(ReviewIndexFlag::Submodule)
        })
        .cloned()
        .collect();
    let oracles: BTreeMap<FileRef, Vec<OracleLine>> = files
        .iter()
        .map(|file| {
            let entry = doc.entry(file).expect("entry");
            let source = &by_id[&file.source_id];
            let mut lines = oracle(&fixture, source, &entry.path, entry.old_path.as_deref());
            if lines.is_empty() {
                // No textual change (a mode change or a pure rename): git prints no hunk, and
                // every line is unchanged context.
                lines = (1_u32..)
                    .zip(lines_at(&fixture, &source.head, &entry.path))
                    .map(|(line, text)| (Some(line), Some(line), text))
                    .collect();
            }
            (file.clone(), lines)
        })
        .collect();
    let numbers = |lines: &[OracleLine]| -> Vec<(Option<u32>, Option<u32>)> {
        lines.iter().map(|(old, new, _)| (*old, *new)).collect()
    };

    // Unexpanded: hunk lines plus gap extents cover every line exactly once, in order, with
    // both line numbers.
    let mut gap_lines = 0;
    for file in &files {
        let shown = shown(&doc, file);
        gap_lines += shown.iter().filter(|(_, _, text)| text.is_none()).count();
        let shown: Vec<_> = shown.iter().map(|(old, new, _)| (*old, *new)).collect();
        assert_eq!(shown, numbers(&oracles[file]), "{}", file.path);
    }
    assert!(
        gap_lines > 100,
        "the fixture has hidden context ({gap_lines})"
    );

    // Fully revealed: every line with its text, exactly as git shows it.
    for file in &files {
        doc.reveal_file(file);
    }
    settle(&mut service, &mut doc);
    let mut lines = 0;
    for file in &files {
        let shown: Vec<OracleLine> = shown(&doc, file)
            .into_iter()
            .map(|(old, new, text)| (old, new, text.expect("revealed text is loaded")))
            .collect();
        lines += shown.len();
        assert_eq!(&shown, &oracles[file], "{}", file.path);
    }
    assert!(lines > 500, "checked {lines} lines");

    // Side by side: each side's lines in order, rows pairing removed and added runs.
    doc.set_layout(LayoutMode::SideBySide);
    for file in &files {
        assert_rows_match(&doc, file, &oracles[file]);
    }
}

/// Side-by-side rows of `file` show each side's lines of `oracle` in order; a row with both
/// sides pairs a removed with an added line (emphasized) or shows one unchanged line.
fn assert_rows_match(doc: &ReviewDocument, file: &FileRef, oracle: &[OracleLine]) {
    let first = doc
        .index_of(&super::super::ItemKey::FileHeader(file.clone()))
        .expect("header");
    let (mut old_side, mut new_side, mut rows) = (Vec::new(), Vec::new(), Vec::new());
    for index in first + 1..doc.len() {
        let item = doc.item(index).expect("item");
        if matches!(
            item,
            ReviewItem::FileHeader(_) | ReviewItem::SourceHeader(_)
        ) {
            break;
        }
        let ReviewItem::LinePair(pair) = item else {
            continue;
        };
        match (&pair.old, &pair.new) {
            (Some(old), Some(new)) => {
                let changed = old.kind == LineKind::Removed && new.kind == LineKind::Added;
                assert!(changed || old.kind == new.kind, "{pair:?}");
                assert_eq!(pair.emphasize, changed);
            }
            (None, None) => panic!("empty row"),
            _ => assert!(!pair.emphasize),
        }
        rows.push((
            pair.old.as_ref().map(|side| side.line),
            pair.new.as_ref().map(|side| side.line),
        ));
        if let Some(side) = &pair.old {
            old_side.push((side.line, side.content.text().expect("text").to_string()));
        }
        if let Some(side) = &pair.new {
            new_side.push((side.line, side.content.text().expect("text").to_string()));
        }
    }
    let expect_old: Vec<(u32, String)> = oracle
        .iter()
        .filter_map(|(old, _, text)| old.map(|old| (old, text.clone())))
        .collect();
    let expect_new: Vec<(u32, String)> = oracle
        .iter()
        .filter_map(|(_, new, text)| new.map(|new| (new, text.clone())))
        .collect();
    assert_eq!(old_side, expect_old, "{} old side", file.path);
    assert_eq!(new_side, expect_new, "{} new side", file.path);
    assert_eq!(rows, oracle_rows(oracle), "{} rows", file.path);
}

/// Side-by-side rows of a whole-file diff: unchanged lines alone, and each run of removed
/// lines followed by added lines paired by position.
fn oracle_rows(oracle: &[OracleLine]) -> Vec<(Option<u32>, Option<u32>)> {
    let mut rows = Vec::new();
    let (mut removed, mut added) = (Vec::new(), Vec::new());
    let flush = |rows: &mut Vec<_>, removed: &mut Vec<u32>, added: &mut Vec<u32>| {
        for row in 0..removed.len().max(added.len()) {
            rows.push((removed.get(row).copied(), added.get(row).copied()));
        }
        removed.clear();
        added.clear();
    };
    for line in oracle {
        match *line {
            (Some(old), None, _) => {
                if !added.is_empty() {
                    flush(&mut rows, &mut removed, &mut added);
                }
                removed.push(old);
            }
            (None, Some(new), _) => added.push(new),
            (old, new, _) => {
                flush(&mut rows, &mut removed, &mut added);
                rows.push((old, new));
            }
        }
    }
    flush(&mut rows, &mut removed, &mut added);
    rows
}
