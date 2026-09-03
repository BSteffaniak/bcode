use bcode_config::{PromptProfileConfig, PromptProfileLayerConfig};
use serde::Deserialize;

const BUNDLED_PROFILE_DOCUMENTS: &[&str] = &[include_str!(
    "../profiles/anthropic-claude-output-preservation.toml"
)];

#[derive(Debug, Deserialize)]
struct BundledProfileDocument {
    id: String,
    target: BundledProfileTarget,
    profile: PromptProfileLayerConfig,
}

/// Exact catalog-resolved identity a bundled profile applies to.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BundledProfileTarget {
    /// Every model whose catalog family matches.
    Family(String),
    /// One stable catalog entry.
    CatalogEntry(String),
}

pub fn install(config: &mut PromptProfileConfig, diagnostics: &mut Vec<String>) {
    for contents in BUNDLED_PROFILE_DOCUMENTS {
        let document = match toml::from_str::<BundledProfileDocument>(contents) {
            Ok(document) => document,
            Err(error) => {
                diagnostics.push(format!("invalid bundled prompt profile: {error}"));
                continue;
            }
        };
        if config.bundled.disabled.contains(&document.id) {
            continue;
        }
        let target = match document.target {
            BundledProfileTarget::Family(family) => config.family.entry(family).or_default(),
            BundledProfileTarget::CatalogEntry(entry) => {
                config.catalog_entry.entry(entry).or_default()
            }
        };
        merge_missing(target, document.profile);
    }
}

fn merge_missing(target: &mut PromptProfileLayerConfig, bundled: PromptProfileLayerConfig) {
    if target.system_prompt.is_none() {
        target.system_prompt_mode = bundled.system_prompt_mode;
        target.system_prompt = bundled.system_prompt;
    }
    for (tool, description) in bundled.tool_description {
        target.tool_description.entry(tool).or_insert(description);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn documents() -> Vec<BundledProfileDocument> {
        BUNDLED_PROFILE_DOCUMENTS
            .iter()
            .map(|contents| toml::from_str::<BundledProfileDocument>(contents).expect("profile"))
            .collect()
    }

    #[test]
    fn bundled_documents_have_unique_nonempty_ids_and_targets() {
        let documents = documents();
        let ids = documents
            .iter()
            .map(|document| document.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), documents.len());
        assert!(documents.iter().all(|document| {
            let target = match &document.target {
                BundledProfileTarget::Family(value) | BundledProfileTarget::CatalogEntry(value) => {
                    value
                }
            };
            !document.id.trim().is_empty()
                && !target.trim().is_empty()
                && (document.profile.system_prompt.is_some()
                    || !document.profile.tool_description.is_empty())
        }));
    }

    #[test]
    fn family_targets_install_into_family_layer() {
        let mut config = PromptProfileConfig::default();
        let mut diagnostics = Vec::new();
        install(&mut config, &mut diagnostics);
        assert!(diagnostics.is_empty());
        let layer = config
            .family
            .get("claude")
            .expect("bundled Claude family layer");
        assert!(layer.system_prompt.is_none());
        assert!(layer.tool_description.contains_key("shell.run"));
        assert!(config.catalog_entry.is_empty());
    }

    #[test]
    fn user_layers_take_precedence_over_bundled_text() {
        let mut config = PromptProfileConfig::default();
        config
            .family
            .entry("claude".to_string())
            .or_default()
            .tool_description
            .insert(
                "shell.run".to_string(),
                bcode_config::ToolDescriptionOverrideConfig {
                    mode: bcode_config::PromptProfileTextMode::Replace,
                    text: "user".to_string(),
                },
            );
        install(&mut config, &mut Vec::new());
        assert_eq!(
            config.family["claude"].tool_description["shell.run"].text,
            "user"
        );
    }
}
