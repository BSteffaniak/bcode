//! `[plugins.config."bcode.code_review".guide]`: how guides are planned, what context they read,
//! and their templates (see `docs/code-review-guide.md`).
//!
//! Each entry is read on its own: an invalid one is reported and its default used, so one typo
//! never disables guides.

use bcode_code_review_models::GuideDepth;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;

/// Context files and directories one configuration names at most.
pub const MAX_CONTEXT_ENTRIES: usize = 64;
/// Upper bound of `max_context_bytes`.
pub const MAX_CONTEXT_BYTES: usize = 1024 * 1024;
/// Templates one configuration defines at most.
pub const MAX_TEMPLATES: usize = 64;
/// Sections one template defines at most.
pub const MAX_TEMPLATE_SECTIONS: usize = 32;
/// Bytes of the configured notes kept.
pub const MAX_NOTES_BYTES: usize = 16 * 1024;

/// How a guide is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategyChoice {
    /// Decided by the review's size and shape (see [`super::plan`]).
    #[default]
    Auto,
    /// One agent.
    Single,
    /// Child agents per layer or area.
    FanOut,
}

/// When a verifier pass runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyMode {
    /// Never.
    Never,
    /// For deep guides.
    #[default]
    Deep,
    /// Always.
    Always,
}

/// Which citations show inline markers in the diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Markers {
    /// The current section's.
    #[default]
    Current,
    /// Every section's.
    All,
    /// None.
    Off,
}

/// Which files a reviewed section marks viewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkFilesViewed {
    /// Files whose changed hunks reviewed sections all cover.
    #[default]
    Covered,
    /// Files a reviewed section cites.
    Cited,
    /// None.
    Never,
}

/// Where context files are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAt {
    /// The working tree: the repository's conventions as they are now.
    #[default]
    Worktree,
    /// The review's head.
    Head,
}

/// A section a template adds.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateSection {
    /// Catalog key (`threat_model`).
    pub key: String,
    /// Title.
    pub title: String,
    /// What the section should say.
    #[serde(default)]
    pub prompt: String,
    /// Words or path globs that make the section relevant; empty for always.
    #[serde(default)]
    pub when: Vec<String>,
}

/// A configured template.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateConfig {
    /// The template it extends (a built-in or another configured one).
    #[serde(default)]
    pub extends: Option<String>,
    /// Section keys it requires; the extended template's when absent.
    #[serde(default)]
    pub required_sections: Option<Vec<String>>,
    /// Sections it adds.
    #[serde(default)]
    pub sections: Vec<TemplateSection>,
}

/// The `guide` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuideConfig {
    /// How guides are written.
    pub strategy: StrategyChoice,
    /// How deep a guide goes.
    pub depth: GuideDepth,
    /// Files a single agent writes a guide for at most.
    pub single_max_files: usize,
    /// Changed lines a single agent writes a guide for at most.
    pub single_max_lines: u64,
    /// Child agents of one fan-out at most.
    pub max_children: usize,
    /// Child agents running at once at most.
    pub max_parallel: usize,
    /// Estimated tokens above which a fan-out asks first.
    pub confirm_above_tokens: u64,
    /// Soft cap of a fan-out's tokens.
    pub budget_tokens: u64,
    /// When a verifier pass runs.
    pub verify: VerifyMode,
    /// Which citations show inline markers.
    pub markers: Markers,
    /// Which files reviewed sections mark viewed.
    pub mark_files_viewed: MarkFilesViewed,
    /// Whether stepping stack layers follows each layer's guide.
    pub follow_layer: bool,
    /// Repository files every guide reads as context.
    pub context_files: Vec<String>,
    /// Repository directories whose files every guide reads as context.
    pub context_dirs: Vec<String>,
    /// Where context files are read.
    pub context_at: ContextAt,
    /// Bytes of context content included at most.
    pub max_context_bytes: usize,
    /// Notes every guide reads.
    pub notes: Option<String>,
    /// The default template.
    pub template: String,
    /// Section keys every guide requires; the template's when absent.
    pub required_sections: Option<Vec<String>>,
    /// Configured templates, by name.
    pub templates: BTreeMap<String, TemplateConfig>,
}

impl Default for GuideConfig {
    fn default() -> Self {
        Self {
            strategy: StrategyChoice::default(),
            depth: GuideDepth::default(),
            single_max_files: 40,
            single_max_lines: 1_500,
            max_children: 8,
            max_parallel: 4,
            confirm_above_tokens: 250_000,
            budget_tokens: 600_000,
            verify: VerifyMode::default(),
            markers: Markers::default(),
            mark_files_viewed: MarkFilesViewed::default(),
            follow_layer: true,
            context_files: Vec::new(),
            context_dirs: Vec::new(),
            context_at: ContextAt::default(),
            max_context_bytes: 64 * 1024,
            notes: None,
            template: "default".to_owned(),
            required_sections: None,
            templates: BTreeMap::new(),
        }
    }
}

/// Keys of the `guide` table.
const KEYS: &[&str] = &[
    "strategy",
    "depth",
    "single_max_files",
    "single_max_lines",
    "max_children",
    "max_parallel",
    "confirm_above_tokens",
    "budget_tokens",
    "verify",
    "markers",
    "mark_files_viewed",
    "follow_layer",
    "context_files",
    "context_dirs",
    "context_at",
    "max_context_bytes",
    "notes",
    "template",
    "required_sections",
    "templates",
];

impl GuideConfig {
    /// The `guide` table `value` (a JSON value, as the plugin configuration arrives), with a
    /// message for each entry that could not be used (its default is used instead).
    #[must_use]
    pub fn from_value(value: Option<&serde_json::Value>) -> (Self, Vec<String>) {
        let mut config = Self::default();
        let mut problems = Vec::new();
        let Some(value) = value.filter(|value| !value.is_null()) else {
            return (config, problems);
        };
        let Some(table) = value.as_object() else {
            problems.push("guide: expected a table; using the defaults".to_owned());
            return (config, problems);
        };
        for key in table.keys() {
            if !KEYS.contains(&key.as_str()) {
                problems.push(format!("guide.{key}: unknown setting; ignored"));
            }
        }
        let mut read = Reader {
            table,
            problems: &mut problems,
        };
        read.set("strategy", &mut config.strategy);
        read.set("depth", &mut config.depth);
        read.set_bounded("single_max_files", &mut config.single_max_files, 1, 100_000);
        read.set_bounded(
            "single_max_lines",
            &mut config.single_max_lines,
            1,
            10_000_000,
        );
        read.set_bounded("max_children", &mut config.max_children, 1, 32);
        read.set_bounded("max_parallel", &mut config.max_parallel, 1, 16);
        read.set("confirm_above_tokens", &mut config.confirm_above_tokens);
        read.set("budget_tokens", &mut config.budget_tokens);
        read.set("verify", &mut config.verify);
        read.set("markers", &mut config.markers);
        read.set("mark_files_viewed", &mut config.mark_files_viewed);
        read.set("follow_layer", &mut config.follow_layer);
        read.set("context_files", &mut config.context_files);
        read.set("context_dirs", &mut config.context_dirs);
        read.set("context_at", &mut config.context_at);
        read.set_bounded(
            "max_context_bytes",
            &mut config.max_context_bytes,
            0,
            MAX_CONTEXT_BYTES,
        );
        let mut notes: Option<String> = None;
        read.set("notes", &mut notes);
        config.notes = notes
            .map(|notes| cut(notes.trim(), MAX_NOTES_BYTES))
            .filter(|notes| !notes.is_empty());
        read.set("template", &mut config.template);
        read.set("required_sections", &mut config.required_sections);
        if config.context_files.len() + config.context_dirs.len() > MAX_CONTEXT_ENTRIES {
            problems.push(format!(
                "guide: at most {MAX_CONTEXT_ENTRIES} context files and directories are read; the rest are ignored"
            ));
            config.context_files.truncate(MAX_CONTEXT_ENTRIES);
            let room = MAX_CONTEXT_ENTRIES - config.context_files.len();
            config.context_dirs.truncate(room);
        }
        if let Some(templates) = table.get("templates") {
            match templates.as_object() {
                Some(templates) => {
                    for (name, value) in templates.iter().take(MAX_TEMPLATES) {
                        match serde_json::from_value::<TemplateConfig>(value.clone()) {
                            Ok(mut template) => {
                                template.sections.truncate(MAX_TEMPLATE_SECTIONS);
                                config.templates.insert(name.clone(), template);
                            }
                            Err(error) => problems.push(format!(
                                "guide.templates.{name}: {error}; the template is ignored"
                            )),
                        }
                    }
                    if templates.len() > MAX_TEMPLATES {
                        problems.push(format!(
                            "guide.templates: at most {MAX_TEMPLATES} templates are read"
                        ));
                    }
                }
                None => problems.push("guide.templates: expected a table; ignored".to_owned()),
            }
        }
        (config, problems)
    }
}

/// Reads entries of one table, recording what cannot be used.
struct Reader<'a> {
    table: &'a serde_json::Map<String, serde_json::Value>,
    problems: &'a mut Vec<String>,
}

impl Reader<'_> {
    fn set<T: DeserializeOwned>(&mut self, key: &str, target: &mut T) {
        let Some(value) = self.table.get(key) else {
            return;
        };
        match serde_json::from_value::<T>(value.clone()) {
            Ok(read) => *target = read,
            Err(error) => self
                .problems
                .push(format!("guide.{key}: {error}; using the default")),
        }
    }

    fn set_bounded<T>(&mut self, key: &str, target: &mut T, min: T, max: T)
    where
        T: DeserializeOwned + PartialOrd + Copy + std::fmt::Display,
    {
        let mut read = *target;
        self.set(key, &mut read);
        if read < min || read > max {
            self.problems.push(format!(
                "guide.{key}: {read} is outside {min}..={max}; using the default"
            ));
            return;
        }
        *target = read;
    }
}

/// `text` cut to at most `max` bytes on a character boundary.
pub(crate) fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_absent_table_is_the_defaults() {
        let (config, problems) = GuideConfig::from_value(None);
        assert_eq!(config, GuideConfig::default());
        assert!(problems.is_empty());
    }

    #[test]
    fn valid_entries_are_read() {
        let value = json!({
            "strategy": "single",
            "depth": "deep",
            "single_max_files": 10,
            "context_files": [".bcode/review-guide.md"],
            "context_dirs": ["docs/adr/"],
            "context_at": "head",
            "notes": "  wire format matters  ",
            "template": "security",
            "templates": {"security": {
                "extends": "default",
                "required_sections": ["purpose", "threat_model"],
                "sections": [{"key": "threat_model", "title": "Threat model", "when": ["auth"]}],
            }},
        });
        let (config, problems) = GuideConfig::from_value(Some(&value));
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(config.strategy, StrategyChoice::Single);
        assert_eq!(config.depth, GuideDepth::Deep);
        assert_eq!(config.single_max_files, 10);
        assert_eq!(config.context_at, ContextAt::Head);
        assert_eq!(config.notes.as_deref(), Some("wire format matters"));
        assert_eq!(config.templates["security"].sections[0].key, "threat_model");
    }

    #[test]
    fn invalid_entries_are_reported_and_their_defaults_used() {
        let value = json!({
            "strategy": "sometimes",
            "max_children": 0,
            "markers": "all",
            "colour": "red",
            "templates": {"broken": {"sections": "no"}},
        });
        let (config, problems) = GuideConfig::from_value(Some(&value));
        assert_eq!(config.strategy, StrategyChoice::Auto);
        assert_eq!(config.max_children, 8);
        assert_eq!(config.markers, Markers::All, "valid entries still apply");
        assert!(config.templates.is_empty());
        for key in [
            "guide.strategy",
            "guide.max_children",
            "guide.colour",
            "guide.templates.broken",
        ] {
            assert!(
                problems.iter().any(|problem| problem.starts_with(key)),
                "{key}: {problems:?}"
            );
        }
    }
}
