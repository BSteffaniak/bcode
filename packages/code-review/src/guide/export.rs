//! Guide export: a version as markdown with numbered citations.
//!
//! Citations are numbered in order of first use. A citation of a pull request source at a commit
//! links to the forge's permalink (`https://github.com/<owner>/<repo>/blob/<sha>/<path>#L12-L40`);
//! others render as `path:12-40 @ <short sha>`. Verification is written out as text.

use super::write::body_tokens;
use bcode_code_review_models::{
    Citation, CitationRelation, FindingKind, GuideVersion, ReviewLineSide, ReviewSource,
    ReviewSourceKind, RevisionRef, Section, SectionState, Verification,
};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// How citations link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkStyle {
    /// Forge permalinks where the source has a forge repository.
    #[default]
    Permalink,
    /// Repository-relative paths.
    Relative,
}

/// What an export includes and how it links.
#[derive(Debug, Clone, Default)]
pub struct ExportOptions<'a> {
    /// The guide's title.
    pub title: &'a str,
    /// Web addresses of the forge repositories of sources (`https://github.com/o/r`), by source
    /// id.
    pub repositories: BTreeMap<String, String>,
    /// How citations link.
    pub links: LinkStyle,
    /// Whether unverified citations are included.
    pub include_unverified: bool,
    /// Sections to export (with their children); every section when empty.
    pub sections: Vec<String>,
}

/// Web host of GitHub pull request sources.
const GITHUB_HOST: &str = "github.com";

/// Web addresses of the forge repositories of `sources` (GitHub pull requests, stack layers and
/// stacks), by source id, for [`ExportOptions::repositories`].
#[must_use]
pub fn forge_repositories(sources: &[ReviewSource]) -> BTreeMap<String, String> {
    sources
        .iter()
        .filter_map(|source| match &source.kind {
            ReviewSourceKind::PullRequest {
                provider,
                repository,
                ..
            }
            | ReviewSourceKind::StackLayer {
                provider,
                repository,
                ..
            }
            | ReviewSourceKind::Stack {
                provider,
                repository,
                ..
            } if provider == "github" => Some((
                source.id.clone(),
                format!("https://{GITHUB_HOST}/{repository}"),
            )),
            _ => None,
        })
        .collect()
}

/// `version` as markdown.
#[must_use]
pub fn render_markdown(version: &GuideVersion, options: &ExportOptions<'_>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# {} · v{}\n", options.title, version.version);
    if !version.heads.is_empty() {
        let heads: Vec<String> = version
            .heads
            .values()
            .map(|head| {
                head.head.commit_oid().map_or_else(
                    || format!("{} ({})", head.label, live_name(&head.head)),
                    |oid| format!("{} @ {}", head.label, short(oid)),
                )
            })
            .collect();
        let _ = writeln!(out, "_Pinned to {}._\n", heads.join(", "));
    }
    let mut numbers: BTreeMap<String, usize> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    let included = |id: &str| {
        version.citations.get(id).is_some_and(|citation| {
            options.include_unverified || citation.verification.is_verified()
        })
    };
    let roots: Vec<(Vec<usize>, &Section)> = if options.sections.is_empty() {
        version
            .outline
            .iter()
            .enumerate()
            .map(|(index, section)| (vec![index + 1], section))
            .collect()
    } else {
        numbered(&version.outline, &[])
            .into_iter()
            .filter(|(_, section)| options.sections.contains(&section.id))
            .collect()
    };
    for (number, section) in roots {
        write_section(
            &mut out,
            section,
            &number,
            &included,
            &mut numbers,
            &mut order,
        );
    }
    if !order.is_empty() {
        out.push_str("## Citations\n\n");
        for (index, id) in order.iter().enumerate() {
            if let Some(citation) = version.citations.get(id) {
                let _ = writeln!(
                    out,
                    "{}. {} · {} · {} · {}",
                    index + 1,
                    location(citation, options),
                    side_name(citation.side),
                    relation_name(citation.relation),
                    verification_name(&citation.verification)
                );
            }
        }
    }
    out
}

/// Every section with its outline number, depth first.
fn numbered<'a>(sections: &'a [Section], prefix: &[usize]) -> Vec<(Vec<usize>, &'a Section)> {
    let mut out = Vec::new();
    for (index, section) in sections.iter().enumerate() {
        let mut number = prefix.to_vec();
        number.push(index + 1);
        out.push((number.clone(), section));
        out.extend(numbered(&section.children, &number));
    }
    out
}

fn number_of(id: &str, numbers: &mut BTreeMap<String, usize>, order: &mut Vec<String>) -> usize {
    if let Some(number) = numbers.get(id) {
        return *number;
    }
    order.push(id.to_owned());
    numbers.insert(id.to_owned(), order.len());
    order.len()
}

fn write_section(
    out: &mut String,
    section: &Section,
    number: &[usize],
    included: &dyn Fn(&str) -> bool,
    numbers: &mut BTreeMap<String, usize>,
    order: &mut Vec<String>,
) {
    let level = (number.len() + 1).min(6);
    let label: Vec<String> = number.iter().map(ToString::to_string).collect();
    let provisional = if matches!(section.state, SectionState::Provisional { .. }) {
        " (provisional)"
    } else {
        ""
    };
    let _ = writeln!(
        out,
        "{} {} {}{provisional}\n",
        "#".repeat(level),
        label.join("."),
        section.title
    );
    if section.state == SectionState::Planned {
        out.push_str("_Not written yet._\n\n");
    } else {
        let body = replace_tokens(&section.body, included, numbers, order);
        if !body.trim().is_empty() {
            out.push_str(body.trim_end());
            out.push_str("\n\n");
        }
        let unmentioned: Vec<String> = section
            .citations
            .iter()
            .filter(|id| included(id) && !body_tokens(&section.body).contains(id))
            .map(|id| format!("[{}]", number_of(id, numbers, order)))
            .collect();
        if !unmentioned.is_empty() {
            let _ = writeln!(out, "See {}.\n", unmentioned.join(" "));
        }
        for finding in &section.findings {
            let kind = match finding.kind {
                FindingKind::Risk => "Risk",
                FindingKind::Question => "Question",
                FindingKind::Suggestion => "Suggestion",
                FindingKind::Note => "Note",
            };
            let severity = finding
                .severity
                .map(|severity| {
                    format!(
                        " ({})",
                        serde_json::to_value(severity)
                            .ok()
                            .and_then(|value| value.as_str().map(str::to_owned))
                            .unwrap_or_default()
                    )
                })
                .unwrap_or_default();
            let cites: Vec<String> = finding
                .citations
                .iter()
                .filter(|id| included(id))
                .map(|id| format!("[{}]", number_of(id, numbers, order)))
                .collect();
            let _ = writeln!(
                out,
                "- **{kind}**{severity}: {}{}{}",
                finding.text.trim(),
                if cites.is_empty() { "" } else { " " },
                cites.join(" ")
            );
        }
        if !section.findings.is_empty() {
            out.push('\n');
        }
    }
    for (index, child) in section.children.iter().enumerate() {
        let mut child_number = number.to_vec();
        child_number.push(index + 1);
        write_section(out, child, &child_number, included, numbers, order);
    }
}

/// `body` with `[[c7]]` tokens as numbered references; tokens of citations not included are
/// dropped.
fn replace_tokens(
    body: &str,
    included: &dyn Fn(&str) -> bool,
    numbers: &mut BTreeMap<String, usize>,
    order: &mut Vec<String>,
) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let id = after[..end].trim();
        if included(id) {
            let _ = write!(out, "[{}]", number_of(id, numbers, order));
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

fn location(citation: &Citation, options: &ExportOptions<'_>) -> String {
    let lines = if citation.lines.start == citation.lines.end {
        citation.lines.start.to_string()
    } else {
        format!("{}-{}", citation.lines.start, citation.lines.end)
    };
    let text = format!("{}:{lines}", citation.path);
    let anchor = if citation.lines.start == citation.lines.end {
        format!("#L{}", citation.lines.start)
    } else {
        format!("#L{}-L{}", citation.lines.start, citation.lines.end)
    };
    match (options.links, citation.revision.commit_oid()) {
        (LinkStyle::Permalink, Some(oid)) => {
            let repository = citation
                .source_id
                .as_ref()
                .and_then(|source| options.repositories.get(source))
                .or_else(|| {
                    // Repository context outside the sources links through the only forge
                    // repository the review has, when it has one.
                    let mut all = options.repositories.values();
                    match (all.next(), all.next()) {
                        (Some(only), None) if citation.source_id.is_none() => Some(only),
                        _ => None,
                    }
                });
            repository.map_or_else(
                || format!("`{text}` @ {}", short(oid)),
                |web| format!("[{text}]({web}/blob/{oid}/{}{anchor})", citation.path),
            )
        }
        (LinkStyle::Relative, Some(oid)) => {
            format!("[{text}]({}{anchor}) @ {}", citation.path, short(oid))
        }
        (_, None) => format!("`{text}` ({})", live_name(&citation.revision)),
    }
}

fn short(oid: &str) -> String {
    oid.chars().take(7).collect()
}

const fn live_name(revision: &RevisionRef) -> &'static str {
    match revision {
        RevisionRef::WorkingTree => "working tree",
        RevisionRef::Index => "index",
        RevisionRef::EmptyTree => "empty tree",
        RevisionRef::Commit { .. } => "commit",
    }
}

const fn side_name(side: ReviewLineSide) -> &'static str {
    match side {
        ReviewLineSide::New => "new",
        ReviewLineSide::Old => "old",
    }
}

const fn relation_name(relation: CitationRelation) -> &'static str {
    match relation {
        CitationRelation::Changed => "changed",
        CitationRelation::HunkContext => "hunk context",
        CitationRelation::Unchanged => "unchanged",
        CitationRelation::InReview => "in review",
        CitationRelation::OutsideReview => "outside the review",
    }
}

const fn verification_name(verification: &Verification) -> &'static str {
    match verification {
        Verification::Verified { .. } => "verified",
        Verification::Unverified => "unverified",
        Verification::Problem { .. } => "unverified (problem)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::{
        Finding, GuideHead, GuideOrigin, GuideStatus, REVIEW_GUIDE_SCHEMA_VERSION,
        ReviewLineInterval, ReviewThreadSeverity, SectionKind,
    };

    const OID: &str = "0123456789abcdef0123456789abcdef01234567";

    fn citation(id: &str, source: Option<&str>, verified: bool) -> Citation {
        Citation {
            id: id.to_owned(),
            source_id: source.map(str::to_owned),
            path: "net/budget.rs".to_owned(),
            side: ReviewLineSide::New,
            lines: ReviewLineInterval { start: 12, end: 40 },
            revision: RevisionRef::commit(OID),
            blob_oid: None,
            quote_hash: String::new(),
            excerpt: String::new(),
            relation: CitationRelation::Changed,
            verification: if verified {
                Verification::Verified {
                    at_ms: 1,
                    adjusted_from: None,
                }
            } else {
                Verification::Unverified
            },
        }
    }

    fn version() -> GuideVersion {
        GuideVersion {
            guide_id: "g".to_owned(),
            version: 2,
            schema_version: REVIEW_GUIDE_SCHEMA_VERSION,
            heads: BTreeMap::from([(
                "pr".to_owned(),
                GuideHead {
                    base: RevisionRef::commit(OID),
                    head: RevisionRef::commit(OID),
                    label: "#7".to_owned(),
                },
            )]),
            based_on: None,
            origin: GuideOrigin::default(),
            status: GuideStatus::Complete,
            generation: None,
            outline: vec![Section {
                id: "s1".to_owned(),
                kind: SectionKind::Step,
                title: "Budget".to_owned(),
                body: "A token bucket [[c1]], refilled [[c2]].".to_owned(),
                findings: vec![Finding {
                    id: "f1".to_owned(),
                    kind: FindingKind::Risk,
                    severity: Some(ReviewThreadSeverity::Warning),
                    text: "Refill races.".to_owned(),
                    citations: vec!["c1".to_owned()],
                    drafted_thread_id: None,
                }],
                citations: vec!["c1".to_owned(), "c2".to_owned()],
                children: Vec::new(),
                state: SectionState::Written,
                refreshed_from: None,
                checks: Vec::new(),
            }],
            citations: BTreeMap::from([
                ("c1".to_owned(), citation("c1", Some("pr"), true)),
                ("c2".to_owned(), citation("c2", None, false)),
            ]),
            retired_sections: Vec::new(),
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn citations_render_as_permalinks_numbered_by_first_use() {
        let options = ExportOptions {
            title: "Retry budget",
            repositories: BTreeMap::from([("pr".to_owned(), "https://github.com/o/r".to_owned())]),
            include_unverified: true,
            ..ExportOptions::default()
        };
        let markdown = render_markdown(&version(), &options);
        assert!(markdown.starts_with("# Retry budget · v2"), "{markdown}");
        assert!(
            markdown.contains("A token bucket [1], refilled [2]."),
            "{markdown}"
        );
        assert!(
            markdown.contains("- **Risk** (warning): Refill races. [1]"),
            "{markdown}"
        );
        assert!(
            markdown.contains(&format!(
                "1. [net/budget.rs:12-40](https://github.com/o/r/blob/{OID}/net/budget.rs#L12-L40) · new · changed · verified"
            )),
            "{markdown}"
        );
        assert!(
            markdown.contains("2. [net/budget.rs:12-40](https://github.com/o/r/blob/"),
            "repository context links through the only repository: {markdown}"
        );
        assert!(markdown.contains("unverified"), "{markdown}");
    }

    #[test]
    fn unverified_citations_can_be_left_out_and_others_render_with_short_shas() {
        let options = ExportOptions {
            title: "Retry budget",
            include_unverified: false,
            ..ExportOptions::default()
        };
        let markdown = render_markdown(&version(), &options);
        assert!(
            markdown.contains("A token bucket [1], refilled ."),
            "{markdown}"
        );
        assert!(
            markdown.contains("1. `net/budget.rs:12-40` @ 0123456"),
            "{markdown}"
        );
        assert!(!markdown.contains("2. "), "{markdown}");
    }

    #[test]
    fn github_pull_request_sources_link_to_their_repository() {
        let source = |id: &str, kind: ReviewSourceKind| ReviewSource {
            id: id.to_owned(),
            kind,
            label: id.to_owned(),
            included: true,
        };
        let repositories = forge_repositories(&[
            source(
                "pr",
                ReviewSourceKind::PullRequest {
                    provider: "github".to_owned(),
                    repository: "o/r".to_owned(),
                    number: 7,
                },
            ),
            source(
                "layer",
                ReviewSourceKind::StackLayer {
                    provider: "github".to_owned(),
                    repository: "o/s".to_owned(),
                    number: 8,
                },
            ),
            source(
                "elsewhere",
                ReviewSourceKind::PullRequest {
                    provider: "gitlab".to_owned(),
                    repository: "o/r".to_owned(),
                    number: 9,
                },
            ),
            source("commit", ReviewSourceKind::LastCommit),
        ]);
        assert_eq!(
            repositories,
            BTreeMap::from([
                ("layer".to_owned(), "https://github.com/o/s".to_owned()),
                ("pr".to_owned(), "https://github.com/o/r".to_owned()),
            ])
        );
    }
}
