//! Guide templates: the sections a guide must have and the sections it may add.
//!
//! Every guide has core sections (purpose, scope, reading order); adaptive sections from the
//! catalog are included only with cited evidence. Built-in templates are `default`, `explain`
//! (file-set and repository reviews with no diff) and `freeform` (only purpose is required);
//! configured templates extend them (see [`super::config`]).

use super::config::{GuideConfig, TemplateSection};
use bcode_code_review_models::SectionKind;
use serde::Serialize;

/// Section key of the purpose section.
pub const PURPOSE: &str = "purpose";
/// Section key of the scope section.
pub const SCOPE: &str = "scope";
/// Section key of the reading order section.
pub const READING_ORDER: &str = "reading_order";

/// The built-in templates.
pub const BUILT_IN: [&str; 3] = ["default", "explain", "freeform"];

/// Adaptive sections: included only with cited evidence.
pub const ADAPTIVE_SECTIONS: [(&str, &str); 15] = [
    ("risks", "Risks"),
    ("behavior_changes", "Behavior changes"),
    ("api_changes", "API and contract changes"),
    ("data_migrations", "Data and migrations"),
    ("security", "Security"),
    ("performance", "Performance"),
    ("concurrency", "Concurrency"),
    ("error_handling", "Error handling"),
    ("tests", "Tests and coverage gaps"),
    ("config_rollout", "Configuration and rollout"),
    ("dependencies", "Dependencies"),
    ("ui", "UI"),
    ("deleted_code", "Deleted code"),
    ("follow_ups", "Follow-ups"),
    ("open_questions", "Open questions"),
];

/// Extra sections the `explain` template offers.
const EXPLAIN_SECTIONS: [(&str, &str, &str); 4] = [
    (
        "architecture",
        "Architecture",
        "How the code is organized and how its parts fit together.",
    ),
    (
        "entry_points",
        "Entry points",
        "Where execution or use starts, with citations.",
    ),
    (
        "invariants",
        "Invariants",
        "What must stay true, and where the code relies on it.",
    ),
    ("risks", "Risks", "What is fragile or surprising."),
];

/// Extends chains followed at most.
const MAX_EXTENDS: usize = 8;

/// A template resolved through its extends chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedTemplate {
    /// Its name.
    pub name: String,
    /// Section keys a guide must have.
    pub required_sections: Vec<String>,
    /// Sections the template adds, with what they should say and when they apply.
    pub sections: Vec<TemplateSectionView>,
    /// Adaptive section keys and titles, included only with cited evidence.
    pub adaptive_sections: Vec<SectionChoice>,
}

/// A template section as a guide writer sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TemplateSectionView {
    /// Catalog key.
    pub key: String,
    /// Title.
    pub title: String,
    /// What it should say.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub prompt: String,
    /// Words or path globs that make it relevant; empty for always.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub when: Vec<String>,
}

/// An adaptive section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SectionChoice {
    /// Catalog key.
    pub key: String,
    /// Title.
    pub title: String,
}

impl From<&TemplateSection> for TemplateSectionView {
    fn from(section: &TemplateSection) -> Self {
        Self {
            key: section.key.clone(),
            title: section.title.clone(),
            prompt: section.prompt.clone(),
            when: section.when.clone(),
        }
    }
}

/// The catalog key of a section kind: `purpose`, `scope`, `reading_order`, or a topic's key.
#[must_use]
pub fn section_key(kind: &SectionKind) -> Option<&str> {
    match kind {
        SectionKind::Purpose => Some(PURPOSE),
        SectionKind::Scope => Some(SCOPE),
        SectionKind::ReadingOrder => Some(READING_ORDER),
        SectionKind::Topic { key } => Some(key),
        _ => None,
    }
}

/// The section kind of a catalog key.
#[must_use]
pub fn kind_of_key(key: &str) -> SectionKind {
    match key {
        PURPOSE => SectionKind::Purpose,
        SCOPE => SectionKind::Scope,
        READING_ORDER => SectionKind::ReadingOrder,
        other => SectionKind::Topic {
            key: other.to_owned(),
        },
    }
}

/// Whether `key` is an adaptive section of the catalog.
#[must_use]
pub fn is_adaptive(key: &str) -> bool {
    ADAPTIVE_SECTIONS
        .iter()
        .any(|(adaptive, _)| *adaptive == key)
}

fn built_in(name: &str) -> Option<ResolvedTemplate> {
    let core = || [PURPOSE, SCOPE, READING_ORDER].map(str::to_owned).to_vec();
    let adaptive = || {
        ADAPTIVE_SECTIONS
            .iter()
            .map(|(key, title)| SectionChoice {
                key: (*key).to_owned(),
                title: (*title).to_owned(),
            })
            .collect()
    };
    Some(match name {
        "default" => ResolvedTemplate {
            name: name.to_owned(),
            required_sections: core(),
            sections: Vec::new(),
            adaptive_sections: adaptive(),
        },
        "explain" => ResolvedTemplate {
            name: name.to_owned(),
            required_sections: core(),
            sections: EXPLAIN_SECTIONS
                .iter()
                .map(|(key, title, prompt)| TemplateSectionView {
                    key: (*key).to_owned(),
                    title: (*title).to_owned(),
                    prompt: (*prompt).to_owned(),
                    when: Vec::new(),
                })
                .collect(),
            adaptive_sections: adaptive(),
        },
        "freeform" => ResolvedTemplate {
            name: name.to_owned(),
            required_sections: vec![PURPOSE.to_owned()],
            sections: Vec::new(),
            adaptive_sections: adaptive(),
        },
        _ => return None,
    })
}

/// The template `name`, resolved through its extends chain.
///
/// Without a name it is the configured default (`explain` instead of `default` for a review
/// without a diff). The configuration's `required_sections` apply to the default template.
///
/// # Errors
///
/// * A message naming the template when it is unknown, extends an unknown template, or its
///   extends chain loops or is longer than eight.
pub fn resolve_template(
    config: &GuideConfig,
    name: Option<&str>,
    without_diff: bool,
) -> Result<ResolvedTemplate, String> {
    let requested = name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(config.template.as_str());
    let requested = if requested == "default" && without_diff && name.is_none() {
        "explain"
    } else {
        requested
    };
    let mut chain: Vec<(&str, &super::config::TemplateConfig)> = Vec::new();
    let mut current = requested;
    let mut base = loop {
        if let Some(configured) = config.templates.get(current) {
            if chain.iter().any(|(seen, _)| *seen == current) || chain.len() >= MAX_EXTENDS {
                return Err(format!(
                    "the template {requested} extends itself or too many others"
                ));
            }
            chain.push((current, configured));
            current = configured.extends.as_deref().unwrap_or("default");
            continue;
        }
        if let Some(template) = built_in(current) {
            break template;
        }
        let mut known: Vec<String> = BUILT_IN.map(str::to_owned).to_vec();
        known.extend(config.templates.keys().cloned());
        return Err(format!(
            "no guide template is named {current}; templates: {}",
            known.join(", ")
        ));
    };
    if chain.is_empty()
        && base.name == "default"
        && let Some(required) = &config.required_sections
    {
        base.required_sections.clone_from(required);
    }
    for (name, configured) in chain.into_iter().rev() {
        name.clone_into(&mut base.name);
        if let Some(required) = &configured.required_sections {
            base.required_sections.clone_from(required);
        }
        for section in &configured.sections {
            base.sections.retain(|existing| existing.key != section.key);
            base.sections.push(section.into());
        }
    }
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::super::config::TemplateConfig;
    use super::*;

    #[test]
    fn built_in_templates_require_their_core_sections() {
        let config = GuideConfig::default();
        let default = resolve_template(&config, None, false).expect("default");
        assert_eq!(
            default.required_sections,
            ["purpose", "scope", "reading_order"]
        );
        let freeform = resolve_template(&config, Some("freeform"), false).expect("freeform");
        assert_eq!(freeform.required_sections, ["purpose"]);
        let explain = resolve_template(&config, None, true).expect("explain");
        assert_eq!(
            explain.name, "explain",
            "a review without a diff is explained"
        );
        assert!(
            explain
                .sections
                .iter()
                .any(|section| section.key == "entry_points")
        );
    }

    #[test]
    fn configured_templates_extend_and_unknown_ones_are_named() {
        let mut config = GuideConfig::default();
        config.templates.insert(
            "security".to_owned(),
            TemplateConfig {
                extends: Some("default".to_owned()),
                required_sections: Some(
                    ["purpose", "scope", "reading_order", "threat_model"]
                        .map(str::to_owned)
                        .to_vec(),
                ),
                sections: vec![TemplateSection {
                    key: "threat_model".to_owned(),
                    title: "Threat model".to_owned(),
                    prompt: "Who can reach the changed code.".to_owned(),
                    when: vec!["auth".to_owned()],
                }],
            },
        );
        config.templates.insert(
            "loop".to_owned(),
            TemplateConfig {
                extends: Some("loop".to_owned()),
                ..TemplateConfig::default()
            },
        );
        let security = resolve_template(&config, Some("security"), false).expect("security");
        assert!(
            security
                .required_sections
                .contains(&"threat_model".to_owned())
        );
        assert_eq!(security.sections[0].key, "threat_model");
        assert!(resolve_template(&config, Some("loop"), false).is_err());
        let unknown = resolve_template(&config, Some("nope"), false).expect_err("unknown");
        assert!(unknown.contains("security"), "{unknown}");
    }
}
