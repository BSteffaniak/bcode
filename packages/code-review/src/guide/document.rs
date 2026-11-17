//! Bounds and structure of guide documents, checked before they are stored, and section
//! fingerprints for reviewed marks.

use bcode_code_review_models::{
    Citation, GuideArea, GuideThread, GuideVersion, MAX_GUIDE_CITATIONS, MAX_GUIDE_DOCUMENT_BYTES,
    MAX_GUIDE_EXCERPT_BYTES, MAX_GUIDE_FINDING_BYTES, MAX_GUIDE_QUESTION_BYTES,
    MAX_GUIDE_SECTION_BODY_BYTES, MAX_GUIDE_SECTIONS, MAX_GUIDE_TITLE_BYTES,
    REVIEW_GUIDE_SCHEMA_VERSION, Section, all_sections,
};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

/// Why a guide document cannot be stored.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GuideDocumentError {
    /// The document's schema version is not the one this build writes.
    #[error(
        "unsupported review guide schema version {found} (this build reads and writes \
         {REVIEW_GUIDE_SCHEMA_VERSION})"
    )]
    UnsupportedSchema {
        /// The version found.
        found: u16,
    },
    /// More sections than [`MAX_GUIDE_SECTIONS`].
    #[error("the guide has {count} sections; at most {MAX_GUIDE_SECTIONS} are kept")]
    TooManySections {
        /// Sections in the tree.
        count: usize,
    },
    /// More citations than [`MAX_GUIDE_CITATIONS`].
    #[error("the guide has {count} citations; at most {MAX_GUIDE_CITATIONS} are kept")]
    TooManyCitations {
        /// Citations.
        count: usize,
    },
    /// The document is larger than [`MAX_GUIDE_DOCUMENT_BYTES`] as JSON.
    #[error("the guide document is {bytes} bytes; at most {MAX_GUIDE_DOCUMENT_BYTES} are kept")]
    TooLarge {
        /// Document bytes.
        bytes: usize,
    },
    /// A text field is over its bound.
    #[error("{what} of {owner} is {bytes} bytes; at most {max} are kept")]
    FieldTooLarge {
        /// Which field.
        what: &'static str,
        /// The section, finding, citation, or thread it belongs to.
        owner: String,
        /// Its bytes.
        bytes: usize,
        /// The bound.
        max: usize,
    },
    /// Two sections share an id.
    #[error("section id {0} is used more than once")]
    DuplicateSection(String),
    /// Two findings share an id.
    #[error("finding id {0} is used more than once")]
    DuplicateFinding(String),
    /// A citation is kept under another key than its id.
    #[error("citation {id} is kept under the key {key}")]
    CitationKeyMismatch {
        /// The key.
        key: String,
        /// The citation's id.
        id: String,
    },
    /// A section, finding, or check names a citation the document does not have.
    #[error("{owner} cites {citation}, which the guide does not have")]
    UnknownCitation {
        /// The section or finding.
        owner: String,
        /// The missing citation id.
        citation: String,
    },
    /// An id is empty.
    #[error("{0} has an empty id")]
    EmptyId(&'static str),
}

/// Check a version: its schema version, bounds, and structure.
///
/// # Errors
///
/// * [`GuideDocumentError::UnsupportedSchema`] for another schema version.
/// * See [`check_document`].
pub fn check_version(version: &GuideVersion) -> Result<(), GuideDocumentError> {
    if version.schema_version != REVIEW_GUIDE_SCHEMA_VERSION {
        return Err(GuideDocumentError::UnsupportedSchema {
            found: version.schema_version,
        });
    }
    check_document(&version.outline, &version.citations)
}

/// Check a fan-out area's document.
///
/// # Errors
///
/// * See [`check_document`].
pub fn check_area(area: &GuideArea) -> Result<(), GuideDocumentError> {
    if area.area_id.is_empty() {
        return Err(GuideDocumentError::EmptyId("the area"));
    }
    check_document(&area.outline, &area.citations)
}

/// Check an ask thread's bounds.
///
/// # Errors
///
/// * [`GuideDocumentError::EmptyId`] for an empty thread or section id.
/// * [`GuideDocumentError::FieldTooLarge`] for a question over [`MAX_GUIDE_QUESTION_BYTES`].
pub fn check_thread(thread: &GuideThread) -> Result<(), GuideDocumentError> {
    if thread.thread_id.is_empty() {
        return Err(GuideDocumentError::EmptyId("the thread"));
    }
    if thread.section_id.is_empty() {
        return Err(GuideDocumentError::EmptyId("the thread's section"));
    }
    bounded(
        "the question",
        || format!("thread {}", thread.thread_id),
        &thread.question,
        MAX_GUIDE_QUESTION_BYTES,
    )
}

/// Check a document's bounds and structure: section and citation counts, its JSON size, text
/// field sizes, unique section and finding ids, citation keys matching their ids, and every
/// cited id present.
///
/// # Errors
///
/// * The first [`GuideDocumentError`] found.
pub fn check_document(
    outline: &[Section],
    citations: &BTreeMap<String, Citation>,
) -> Result<(), GuideDocumentError> {
    let sections = all_sections(outline);
    if sections.len() > MAX_GUIDE_SECTIONS {
        return Err(GuideDocumentError::TooManySections {
            count: sections.len(),
        });
    }
    if citations.len() > MAX_GUIDE_CITATIONS {
        return Err(GuideDocumentError::TooManyCitations {
            count: citations.len(),
        });
    }
    for (key, citation) in citations {
        if citation.id.is_empty() {
            return Err(GuideDocumentError::EmptyId("a citation"));
        }
        if *key != citation.id {
            return Err(GuideDocumentError::CitationKeyMismatch {
                key: key.clone(),
                id: citation.id.clone(),
            });
        }
        bounded(
            "the excerpt",
            || format!("citation {key}"),
            &citation.excerpt,
            MAX_GUIDE_EXCERPT_BYTES,
        )?;
    }
    let mut section_ids = BTreeSet::new();
    let mut finding_ids = BTreeSet::new();
    let known = |owner: &dyn Fn() -> String, id: &String| {
        if citations.contains_key(id) {
            Ok(())
        } else {
            Err(GuideDocumentError::UnknownCitation {
                owner: owner(),
                citation: id.clone(),
            })
        }
    };
    for section in sections {
        if section.id.is_empty() {
            return Err(GuideDocumentError::EmptyId("a section"));
        }
        if !section_ids.insert(section.id.as_str()) {
            return Err(GuideDocumentError::DuplicateSection(section.id.clone()));
        }
        let owner = || format!("section {}", section.id);
        bounded("the title", owner, &section.title, MAX_GUIDE_TITLE_BYTES)?;
        bounded(
            "the body",
            owner,
            &section.body,
            MAX_GUIDE_SECTION_BODY_BYTES,
        )?;
        for id in &section.citations {
            known(&owner, id)?;
        }
        for finding in &section.findings {
            if finding.id.is_empty() {
                return Err(GuideDocumentError::EmptyId("a finding"));
            }
            if !finding_ids.insert(finding.id.as_str()) {
                return Err(GuideDocumentError::DuplicateFinding(finding.id.clone()));
            }
            let owner = || format!("finding {}", finding.id);
            bounded("the text", owner, &finding.text, MAX_GUIDE_FINDING_BYTES)?;
            for id in &finding.citations {
                known(&owner, id)?;
            }
        }
        for check in &section.checks {
            bounded("a claim", owner, &check.claim, MAX_GUIDE_FINDING_BYTES)?;
            bounded("a check note", owner, &check.note, MAX_GUIDE_FINDING_BYTES)?;
            for id in &check.citations {
                known(&owner, id)?;
            }
        }
    }
    let bytes = document_bytes(outline, citations);
    if bytes > MAX_GUIDE_DOCUMENT_BYTES {
        return Err(GuideDocumentError::TooLarge { bytes });
    }
    Ok(())
}

/// The document's size as stored JSON.
fn document_bytes(outline: &[Section], citations: &BTreeMap<String, Citation>) -> usize {
    #[derive(serde::Serialize)]
    struct Document<'a> {
        outline: &'a [Section],
        citations: &'a BTreeMap<String, Citation>,
    }
    serde_json::to_vec(&Document { outline, citations }).map_or(usize::MAX, |json| json.len())
}

fn bounded(
    what: &'static str,
    owner: impl Fn() -> String,
    text: &str,
    max: usize,
) -> Result<(), GuideDocumentError> {
    if text.len() > max {
        return Err(GuideDocumentError::FieldTooLarge {
            what,
            owner: owner(),
            bytes: text.len(),
            max,
        });
    }
    Ok(())
}

/// Fingerprint of the code a section cites.
///
/// It hashes the quote hashes of the citations the section uses (its own and its findings'), in
/// order. A reviewed mark records it; when the cited code changes, the fingerprint does, and the
/// section is no longer reviewed.
#[must_use]
pub fn section_fingerprint(section: &Section, citations: &BTreeMap<String, Citation>) -> String {
    let mut hasher = Sha256::new();
    for id in section.cited_ids() {
        hasher.update(id.as_bytes());
        hasher.update(b"\0");
        if let Some(citation) = citations.get(id) {
            hasher.update(citation.path.as_bytes());
            hasher.update(b"\0");
            hasher.update(citation.quote_hash.as_bytes());
        }
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::{
        CitationRelation, Finding, FindingKind, GuideOrigin, GuideStatus, ReviewLineInterval,
        ReviewLineSide, RevisionRef, SectionKind, SectionState, Verification,
    };

    fn section(id: &str, citations: &[&str]) -> Section {
        Section {
            id: id.to_owned(),
            kind: SectionKind::Step,
            title: format!("Step {id}"),
            body: String::new(),
            findings: Vec::new(),
            citations: citations.iter().map(|id| (*id).to_owned()).collect(),
            children: Vec::new(),
            state: SectionState::Written,
            refreshed_from: None,
            checks: Vec::new(),
        }
    }

    fn citation(id: &str, hash: &str) -> Citation {
        Citation {
            id: id.to_owned(),
            source_id: None,
            path: "src/lib.rs".to_owned(),
            side: ReviewLineSide::New,
            lines: ReviewLineInterval { start: 1, end: 2 },
            revision: RevisionRef::WorkingTree,
            blob_oid: None,
            quote_hash: hash.to_owned(),
            excerpt: String::new(),
            relation: CitationRelation::Changed,
            verification: Verification::Unverified,
        }
    }

    fn citations(ids: &[&str]) -> BTreeMap<String, Citation> {
        ids.iter()
            .map(|id| ((*id).to_owned(), citation(id, "h")))
            .collect()
    }

    fn version(schema_version: u16) -> GuideVersion {
        GuideVersion {
            guide_id: "g".to_owned(),
            version: 1,
            schema_version,
            heads: BTreeMap::new(),
            based_on: None,
            origin: GuideOrigin::default(),
            status: GuideStatus::Generating,
            generation: None,
            outline: vec![section("s1", &["c1"])],
            citations: citations(&["c1"]),
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn a_version_of_another_schema_is_rejected() {
        assert!(check_version(&version(REVIEW_GUIDE_SCHEMA_VERSION)).is_ok());
        assert_eq!(
            check_version(&version(REVIEW_GUIDE_SCHEMA_VERSION + 1)),
            Err(GuideDocumentError::UnsupportedSchema {
                found: REVIEW_GUIDE_SCHEMA_VERSION + 1
            })
        );
    }

    #[test]
    fn documents_over_their_bounds_are_rejected() {
        let many: Vec<Section> = (0..=MAX_GUIDE_SECTIONS)
            .map(|index| section(&format!("s{index}"), &[]))
            .collect();
        assert!(matches!(
            check_document(&many, &BTreeMap::new()),
            Err(GuideDocumentError::TooManySections { .. })
        ));
        let ids: Vec<String> = (0..=MAX_GUIDE_CITATIONS)
            .map(|index| format!("c{index}"))
            .collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        assert!(matches!(
            check_document(&[], &citations(&refs)),
            Err(GuideDocumentError::TooManyCitations { .. })
        ));
        let mut long = section("s1", &[]);
        long.body = "x".repeat(MAX_GUIDE_SECTION_BODY_BYTES + 1);
        assert!(matches!(
            check_document(&[long], &BTreeMap::new()),
            Err(GuideDocumentError::FieldTooLarge {
                what: "the body",
                ..
            })
        ));
        let mut cited = citations(&["c1"]);
        cited.get_mut("c1").expect("c1").excerpt = "x".repeat(MAX_GUIDE_EXCERPT_BYTES + 1);
        assert!(matches!(
            check_document(&[], &cited),
            Err(GuideDocumentError::FieldTooLarge {
                what: "the excerpt",
                ..
            })
        ));
        // Within every per-field bound, but over the document bound.
        let big: Vec<Section> = (0..80)
            .map(|index| {
                let mut section = section(&format!("s{index}"), &[]);
                section.body = "x".repeat(MAX_GUIDE_SECTION_BODY_BYTES);
                section
            })
            .collect();
        assert!(matches!(
            check_document(&big, &BTreeMap::new()),
            Err(GuideDocumentError::TooLarge { .. })
        ));
    }

    #[test]
    fn inconsistent_documents_are_rejected() {
        let mut child = section("s1", &[]);
        child.children.push(section("s1", &[]));
        assert_eq!(
            check_document(&[child], &BTreeMap::new()),
            Err(GuideDocumentError::DuplicateSection("s1".to_owned()))
        );
        assert!(matches!(
            check_document(&[section("s1", &["c9"])], &citations(&["c1"])),
            Err(GuideDocumentError::UnknownCitation { .. })
        ));
        let mut with_finding = section("s1", &[]);
        with_finding.findings.push(Finding {
            id: "f1".to_owned(),
            kind: FindingKind::Risk,
            severity: None,
            text: "races".to_owned(),
            citations: vec!["c2".to_owned()],
            drafted_thread_id: None,
        });
        assert!(matches!(
            check_document(&[with_finding], &citations(&["c1"])),
            Err(GuideDocumentError::UnknownCitation { .. })
        ));
        let mut misfiled = citations(&["c1"]);
        misfiled.insert("c2".to_owned(), citation("c3", "h"));
        assert!(matches!(
            check_document(&[], &misfiled),
            Err(GuideDocumentError::CitationKeyMismatch { .. })
        ));
    }

    #[test]
    fn a_fingerprint_changes_when_the_cited_code_does() {
        let step = section("s1", &["c1", "c2"]);
        let before = citations(&["c1", "c2"]);
        let mut after = before.clone();
        assert_eq!(
            section_fingerprint(&step, &before),
            section_fingerprint(&step, &after)
        );
        after.get_mut("c2").expect("c2").quote_hash = "other".to_owned();
        assert_ne!(
            section_fingerprint(&step, &before),
            section_fingerprint(&step, &after)
        );
    }
}
