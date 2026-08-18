use bcode_config::{PromptProfileConfig, PromptProfileLayerConfig};
use serde::Deserialize;

const BUNDLED_PROFILE_DOCUMENTS: &[&str] = &[include_str!(
    "../profiles/anthropic-claude-opus-5-output-preservation.toml"
)];

#[derive(Debug, Deserialize)]
struct BundledProfileDocument {
    id: String,
    target: BundledProfileTarget,
    profile: PromptProfileLayerConfig,
}

#[derive(Debug, Deserialize)]
struct BundledProfileTarget {
    catalog_entry: String,
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
        merge_missing(
            config
                .catalog_entry
                .entry(document.target.catalog_entry)
                .or_default(),
            document.profile,
        );
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

    #[test]
    fn bundled_documents_have_unique_nonempty_ids_and_targets() {
        let documents = BUNDLED_PROFILE_DOCUMENTS
            .iter()
            .map(|contents| toml::from_str::<BundledProfileDocument>(contents).expect("profile"))
            .collect::<Vec<_>>();
        let ids = documents
            .iter()
            .map(|document| document.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), documents.len());
        assert!(documents.iter().all(|document| {
            !document.id.trim().is_empty()
                && !document.target.catalog_entry.trim().is_empty()
                && document.profile.system_prompt.is_some()
        }));
    }
}
