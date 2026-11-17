//! Guide lint: what a writer should fix, reported with every write and check. Lint never
//! refuses a write.
//!
//! * `missing_required`: a section the template requires is absent.
//! * `planned_unwritten`: a planned section is still unwritten (reported once the version is
//!   complete).
//! * `unknown_citation_token`: a body token names no citation of the guide.
//! * `uncited_section`: a written step, topic, findings section or expansion cites nothing.
//! * `boilerplate`: an adaptive section says only that there is nothing to say ("No risks
//!   found"); adaptive sections belong in a guide only with cited evidence.

use super::template::{ResolvedTemplate, is_adaptive, section_key};
use super::write::body_tokens;
use bcode_code_review_models::{
    Citation, GuideStatus, GuideVersion, Section, SectionKind, SectionState, all_sections,
};
use serde::Serialize;
use std::collections::BTreeMap;

/// Lint items reported at most.
pub const MAX_LINT: usize = 50;

/// One lint item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LintItem {
    /// Machine-readable code.
    pub code: &'static str,
    /// The section, when it is about one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// One actionable sentence.
    pub message: String,
}

/// Phrases that make an adaptive section's body boilerplate when it says nothing else.
const BOILERPLATE: [&str; 8] = [
    "no risks",
    "none found",
    "nothing to report",
    "not applicable",
    "n/a",
    "no concerns",
    "no issues",
    "nothing notable",
];

/// Lint `version` against `template`.
#[must_use]
pub fn lint(version: &GuideVersion, template: &ResolvedTemplate) -> Vec<LintItem> {
    let mut items = Vec::new();
    let sections = all_sections(&version.outline);
    for key in &template.required_sections {
        let present = sections
            .iter()
            .any(|section| section_key(&section.kind) == Some(key.as_str()));
        if !present {
            items.push(LintItem {
                code: "missing_required",
                section: None,
                message: format!(
                    "the {} template requires a {key} section; add it with put_section",
                    template.name
                ),
            });
        }
    }
    if version.status == GuideStatus::Complete {
        for section in &sections {
            if section.state == SectionState::Planned {
                items.push(LintItem {
                    code: "planned_unwritten",
                    section: Some(section.id.clone()),
                    message: format!(
                        "{} is planned but not written; write it or remove it",
                        section.id
                    ),
                });
            }
        }
    }
    for section in sections {
        items.extend(lint_section(section, &version.citations));
    }
    items.truncate(MAX_LINT);
    items
}

/// Lint one section against the citations it may use.
#[must_use]
pub fn lint_section(section: &Section, citations: &BTreeMap<String, Citation>) -> Vec<LintItem> {
    let mut items = Vec::new();
    for token in body_tokens(&section.body) {
        if !citations.contains_key(&token) {
            items.push(LintItem {
                code: "unknown_citation_token",
                section: Some(section.id.clone()),
                message: format!(
                    "the body of {} cites [[{token}]], which the guide does not have; cite it in the section's citations",
                    section.id
                ),
            });
        }
    }
    if section.state == SectionState::Planned {
        return items;
    }
    let needs_citations = matches!(
        section.kind,
        SectionKind::Step
            | SectionKind::Topic { .. }
            | SectionKind::Findings
            | SectionKind::Expansion
    );
    if needs_citations && section.cited_ids().is_empty() && body_tokens(&section.body).is_empty() {
        items.push(LintItem {
            code: "uncited_section",
            section: Some(section.id.clone()),
            message: format!(
                "{} cites nothing; cite the lines it is about, with a quote",
                section.id
            ),
        });
    }
    if let SectionKind::Topic { key } = &section.kind
        && is_adaptive(key)
        && section.findings.is_empty()
        && section.cited_ids().is_empty()
    {
        let body = section.body.trim().to_lowercase();
        let short = body.split_whitespace().count() <= 12;
        if body.is_empty() || (short && BOILERPLATE.iter().any(|phrase| body.contains(phrase))) {
            items.push(LintItem {
                code: "boilerplate",
                section: Some(section.id.clone()),
                message: format!(
                    "{} ({key}) says nothing with evidence; remove it, since adaptive sections belong in a guide only with cited evidence",
                    section.id
                ),
            });
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guide::config::GuideConfig;
    use crate::guide::template::resolve_template;
    use bcode_code_review_models::{GuideOrigin, REVIEW_GUIDE_SCHEMA_VERSION};

    fn section(id: &str, kind: SectionKind, body: &str, citations: &[&str]) -> Section {
        Section {
            id: id.to_owned(),
            kind,
            title: id.to_owned(),
            body: body.to_owned(),
            findings: Vec::new(),
            citations: citations.iter().map(|id| (*id).to_owned()).collect(),
            children: Vec::new(),
            state: SectionState::Written,
            refreshed_from: None,
            checks: Vec::new(),
        }
    }

    #[test]
    fn missing_sections_tokens_citations_and_boilerplate_are_linted() {
        let version = GuideVersion {
            guide_id: "g".to_owned(),
            version: 1,
            schema_version: REVIEW_GUIDE_SCHEMA_VERSION,
            heads: BTreeMap::new(),
            based_on: None,
            origin: GuideOrigin::default(),
            status: GuideStatus::Generating,
            generation: None,
            outline: vec![
                section("s1", SectionKind::Purpose, "Adds a budget [[c9]].", &[]),
                section("s2", SectionKind::Step, "Unsupported.", &[]),
                section(
                    "s3",
                    SectionKind::Topic {
                        key: "risks".to_owned(),
                    },
                    "No risks found.",
                    &[],
                ),
            ],
            citations: BTreeMap::new(),
            retired_sections: Vec::new(),
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let template = resolve_template(&GuideConfig::default(), None, false).expect("template");
        let items = lint(&version, &template);
        let codes: Vec<(&str, Option<&str>)> = items
            .iter()
            .map(|item| (item.code, item.section.as_deref()))
            .collect();
        assert!(codes.contains(&("missing_required", None)));
        assert!(codes.contains(&("unknown_citation_token", Some("s1"))));
        assert!(codes.contains(&("uncited_section", Some("s2"))));
        assert!(codes.contains(&("boilerplate", Some("s3"))));
        assert!(
            !codes.contains(&("uncited_section", Some("s1"))),
            "purpose may cite nothing"
        );
    }
}
