//! Which review files a guide's reviewed sections account for.
//!
//! Marking a section reviewed marks files viewed through the review's ordinary viewed path,
//! by `guide.mark_files_viewed`:
//!
//! * `cited`: every file in the diff a reviewed section cites (changed or hunk-context lines).
//! * `covered`: of those, the files whose changed lines (added on the new side, removed on the
//!   old side) all fall in ranges the reviewed sections cite on that side.
//! * `never`: none.
//!
//! The other direction, un-viewing a file, un-reviews the sections that cite it
//! ([`sections_citing`]).

use super::config::MarkFilesViewed;
use bcode_code_review_models::{
    CitationRelation, GuideVersion, ReviewHunk, ReviewLineKind, ReviewLineSide, Section,
};
use std::collections::{BTreeMap, BTreeSet};

/// A file of the review: its source and path.
pub type ReviewFile = (String, String);

/// Whether a citation's relation puts its file in the diff.
const fn in_diff(relation: CitationRelation) -> bool {
    matches!(
        relation,
        CitationRelation::Changed | CitationRelation::HunkContext
    )
}

/// Every section of `outline`, parents before children.
fn walk<'a>(outline: &'a [Section], out: &mut Vec<&'a Section>) {
    for section in outline {
        out.push(section);
        walk(&section.children, out);
    }
}

/// The in-diff files the section `id` of `version` cites.
#[must_use]
pub fn cited_files(version: &GuideVersion, id: &str) -> BTreeSet<ReviewFile> {
    let Some(section) = version.section(id) else {
        return BTreeSet::new();
    };
    section
        .cited_ids()
        .into_iter()
        .filter_map(|cited| version.citations.get(cited))
        .filter(|citation| in_diff(citation.relation))
        .filter_map(|citation| {
            citation
                .source_id
                .clone()
                .map(|source| (source, citation.path.clone()))
        })
        .collect()
}

/// The sections of `version` that cite any of `files` in the diff, in reading order.
#[must_use]
pub fn sections_citing(version: &GuideVersion, files: &BTreeSet<ReviewFile>) -> Vec<String> {
    let mut sections = Vec::new();
    walk(&version.outline, &mut sections);
    sections
        .into_iter()
        .filter(|section| !cited_files(version, &section.id).is_disjoint(files))
        .map(|section| section.id.clone())
        .collect()
}

/// The files the `reviewed` sections of `version` mark viewed under `mode`. `hunks` answers a
/// file's hunks (`None` when they cannot be read, which leaves the file unmarked under
/// `covered`).
pub fn files_to_view(
    version: &GuideVersion,
    reviewed: &BTreeSet<String>,
    mode: MarkFilesViewed,
    mut hunks: impl FnMut(&ReviewFile) -> Option<Vec<ReviewHunk>>,
) -> BTreeSet<ReviewFile> {
    if mode == MarkFilesViewed::Never {
        return BTreeSet::new();
    }
    let mut cited: BTreeSet<ReviewFile> = BTreeSet::new();
    // Lines each file's reviewed citations cover, by side.
    let mut covered: BTreeMap<(ReviewFile, ReviewLineSide), Vec<(u32, u32)>> = BTreeMap::new();
    for id in reviewed {
        let Some(section) = version.section(id) else {
            continue;
        };
        for citation in section
            .cited_ids()
            .into_iter()
            .filter_map(|cited| version.citations.get(cited))
            .filter(|citation| in_diff(citation.relation))
        {
            let Some(source) = citation.source_id.clone() else {
                continue;
            };
            let file = (source, citation.path.clone());
            covered
                .entry((file.clone(), citation.side))
                .or_default()
                .push((citation.lines.start, citation.lines.end));
            cited.insert(file);
        }
    }
    if mode == MarkFilesViewed::Cited {
        return cited;
    }
    let inside = |file: &ReviewFile, side: ReviewLineSide, line: u32| {
        covered
            .get(&(file.clone(), side))
            .is_some_and(|ranges| ranges.iter().any(|(a, b)| (*a..=*b).contains(&line)))
    };
    cited
        .into_iter()
        .filter(|file| {
            hunks(file).is_some_and(|hunks| {
                hunks.iter().flat_map(|hunk| &hunk.lines).all(|line| {
                    match (line.kind, line.new_line, line.old_line) {
                        (ReviewLineKind::Added, Some(number), _) => {
                            inside(file, ReviewLineSide::New, number)
                        }
                        (ReviewLineKind::Removed, _, Some(number)) => {
                            inside(file, ReviewLineSide::Old, number)
                        }
                        _ => true,
                    }
                })
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::{
        Citation, GuideOrigin, GuideStatus, REVIEW_GUIDE_SCHEMA_VERSION, ReviewLine,
        ReviewLineInterval, RevisionRef, SectionKind, SectionState, Verification,
    };

    fn citation(
        id: &str,
        path: &str,
        side: ReviewLineSide,
        lines: (u32, u32),
        relation: CitationRelation,
    ) -> Citation {
        Citation {
            id: id.to_owned(),
            source_id: Some("src".to_owned()),
            path: path.to_owned(),
            side,
            lines: ReviewLineInterval {
                start: lines.0,
                end: lines.1,
            },
            revision: RevisionRef::WorkingTree,
            blob_oid: None,
            quote_hash: String::new(),
            excerpt: String::new(),
            relation,
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

    fn version() -> GuideVersion {
        let citations = [
            citation(
                "c1",
                "a.rs",
                ReviewLineSide::New,
                (1, 5),
                CitationRelation::Changed,
            ),
            citation(
                "c2",
                "a.rs",
                ReviewLineSide::New,
                (10, 12),
                CitationRelation::Changed,
            ),
            citation(
                "c3",
                "b.rs",
                ReviewLineSide::New,
                (1, 2),
                CitationRelation::Changed,
            ),
            citation(
                "c4",
                "c.rs",
                ReviewLineSide::New,
                (1, 2),
                CitationRelation::OutsideReview,
            ),
            citation(
                "c5",
                "b.rs",
                ReviewLineSide::Old,
                (3, 3),
                CitationRelation::Changed,
            ),
        ];
        GuideVersion {
            guide_id: "g".to_owned(),
            version: 1,
            schema_version: REVIEW_GUIDE_SCHEMA_VERSION,
            heads: BTreeMap::new(),
            based_on: None,
            origin: GuideOrigin::default(),
            status: GuideStatus::Complete,
            generation: None,
            outline: vec![
                section("s1", &["c1", "c3", "c4"]),
                section("s2", &["c2"]),
                section("s3", &["c5"]),
            ],
            citations: citations
                .into_iter()
                .map(|citation| (citation.id.clone(), citation))
                .collect(),
            retired_sections: Vec::new(),
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn line(kind: ReviewLineKind, old: Option<u32>, new: Option<u32>) -> ReviewLine {
        ReviewLine {
            kind,
            old_line: old,
            new_line: new,
            content: String::new(),
        }
    }

    fn hunks(file: &ReviewFile) -> Option<Vec<ReviewHunk>> {
        let lines = match file.1.as_str() {
            // Added lines 2 and 11.
            "a.rs" => vec![
                line(ReviewLineKind::Context, Some(1), Some(1)),
                line(ReviewLineKind::Added, None, Some(2)),
                line(ReviewLineKind::Added, None, Some(11)),
            ],
            // Added line 1, removed old line 3.
            "b.rs" => vec![
                line(ReviewLineKind::Added, None, Some(1)),
                line(ReviewLineKind::Removed, Some(3), None),
            ],
            _ => return None,
        };
        Some(vec![ReviewHunk {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
            heading: None,
            lines,
        }])
    }

    fn file(path: &str) -> ReviewFile {
        ("src".to_owned(), path.to_owned())
    }

    fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn cited_marks_every_in_diff_file_a_reviewed_section_cites() {
        let files = files_to_view(&version(), &set(&["s1"]), MarkFilesViewed::Cited, hunks);
        assert_eq!(files, [file("a.rs"), file("b.rs")].into_iter().collect());
        assert!(files_to_view(&version(), &set(&["s1"]), MarkFilesViewed::Never, hunks).is_empty());
    }

    #[test]
    fn covered_marks_only_files_whose_changed_lines_reviewed_sections_all_cite() {
        let version = version();
        // a.rs line 11 is cited only by s2; b.rs's removed line only by s3.
        assert!(files_to_view(&version, &set(&["s1"]), MarkFilesViewed::Covered, hunks).is_empty());
        assert_eq!(
            files_to_view(
                &version,
                &set(&["s1", "s2"]),
                MarkFilesViewed::Covered,
                hunks
            ),
            [file("a.rs")].into_iter().collect()
        );
        assert_eq!(
            files_to_view(
                &version,
                &set(&["s1", "s2", "s3"]),
                MarkFilesViewed::Covered,
                hunks
            ),
            [file("a.rs"), file("b.rs")].into_iter().collect()
        );
        assert!(
            files_to_view(
                &version,
                &set(&["s1", "s2"]),
                MarkFilesViewed::Covered,
                |_| None
            )
            .is_empty(),
            "unreadable hunks mark nothing"
        );
    }

    #[test]
    fn un_viewing_a_file_names_the_sections_citing_it() {
        let version = version();
        assert_eq!(
            sections_citing(&version, &[file("a.rs")].into_iter().collect()),
            ["s1", "s2"]
        );
        assert_eq!(
            sections_citing(&version, &[file("c.rs")].into_iter().collect()),
            Vec::<String>::new(),
            "outside-review citations do not count"
        );
    }
}
