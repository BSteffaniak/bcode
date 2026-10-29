//! Reusable synthetic annotations; no production model, media or persistence access.
//!
//! `phrase_controls` checks fixture-specific strings only. Neither source IDs nor a
//! clean phrase report establishes entailment, completeness, order or readability.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Corpus {
    pub schema_version: u32,
    pub cases: Vec<Case>,
    pub patch_scenario: PatchScenario,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub id: String,
    pub units: Vec<SourceUnit>,
    pub obligations: Vec<Obligation>,
    pub acceptable: String,
    pub forbidden: Vec<String>,
    pub controls: Vec<Control>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceUnit {
    pub id: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub kind: SourceKind,
    pub text: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Transcript,
    VisualAnnotation,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Obligation {
    pub id: String,
    pub source_ids: Vec<String>,
    /// Independent natural-language rubric, not generated from the candidate.
    pub meaning: String,
    /// Narrow exact-phrase controls; valid paraphrases may not contain these.
    pub phrases: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Control {
    pub id: String,
    pub kind: ControlKind,
    /// Intentionally valid even when the associated prose is inadequate.
    pub source_ids: Vec<String>,
    pub text: String,
    pub missing: Vec<String>,
    pub unsupported: Vec<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    OmissionDespiteIds,
    LostCaveatNumber,
    UnsupportedAddition,
    Contradiction,
    MisleadingCaption,
    CrossBoundaryFact,
}

/// Inputs for future production patch tests, not a test-local patch engine.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchScenario {
    pub expected_revision: u64,
    pub current_revision: u64,
    pub target_block: String,
    pub untouched_text: String,
    pub untouched_image: PreservedImage,
    pub replacement_text: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreservedImage {
    pub asset_id: String,
    pub caption: String,
    pub evidence_ids: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct PhraseReport<'a> {
    /// Obligation IDs with one or more unmatched exact phrases.
    pub missing: Vec<&'a str>,
    /// Matched known-bad strings only, not all unsupported assertions.
    pub unsupported: Vec<&'a str>,
}

/// Load the synthetic, versioned annotation corpus.
#[must_use]
pub fn corpus() -> Corpus {
    serde_json::from_str(include_str!("corpus.json")).expect("valid passage annotations")
}

/// Check exact fixture phrases only; this does not certify semantic fidelity.
#[must_use]
pub fn phrase_controls<'a>(case: &'a Case, candidate: &str) -> PhraseReport<'a> {
    let candidate = candidate.to_lowercase();
    PhraseReport {
        missing: case
            .obligations
            .iter()
            .filter(|obligation| {
                obligation
                    .phrases
                    .iter()
                    .any(|phrase| !candidate.contains(&phrase.to_lowercase()))
            })
            .map(|obligation| obligation.id.as_str())
            .collect(),
        unsupported: case
            .forbidden
            .iter()
            .filter(|claim| candidate.contains(&claim.to_lowercase()))
            .map(String::as_str)
            .collect(),
    }
}
