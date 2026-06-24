//! Generation helpers for documentation pages.

use bcode_config::{
    BCODE_AUTH_PROFILE_ENV, BCODE_CONFIG_ENV, BCODE_CONFIG_TOML_ENV, BCODE_MODEL_PROFILE_ENV,
    BcodeConfig,
};
use hyperchad_docs_site::EnvOverrideDoc;

/// Extract a section from a markdown document by heading.
pub(crate) fn extract_section_for(
    markdown: &str,
    start_heading: &str,
    end_prefix: Option<&str>,
) -> String {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut start_idx = None;
    let mut end_idx = lines.len();

    for (i, line) in lines.iter().enumerate() {
        if *line == start_heading {
            start_idx = Some(i + 1);
            continue;
        }
        if let Some(start) = start_idx
            && i > start
            && let Some(prefix) = end_prefix
            && line.starts_with(prefix)
            && *line != start_heading
        {
            end_idx = i;
            break;
        }
    }

    start_idx.map_or_else(
        || markdown.to_string(),
        |start| lines[start..end_idx].join("\n"),
    )
}

/// Generate the CLI reference markdown from the actual clap command tree.
pub(crate) fn generate_cli_reference() -> String {
    hyperchad_docs_site::CliReference::new("bcode", bcode_cli::root_command()).render()
}

/// Generate the full configuration reference markdown from `BcodeConfig`'s
/// code-owned `ConfigDocSchema` implementation.
pub(crate) fn generate_config_reference() -> String {
    hyperchad_docs_site::ConfigReference::<BcodeConfig>::new()
        .intro(
            "bcode is configured with `bcode.toml`. Configuration can be layered from \
             files, raw TOML environment overlays, active model profiles, auth profiles, \
             and plugin/provider defaults.\n\n\
             Dynamic plugin/provider tables are documented at their stable boundaries; \
             provider-specific keys remain owned by the relevant plugin.\n\n\
             ---",
        )
        .env_overrides(env_overrides().iter().cloned())
        .toml_table_headings()
        .option_column_label("Option")
        .section_appendix("model", model_config_examples())
        .section_appendix("auth", auth_config_examples())
        .section_appendix("agent", agent_config_examples())
        .section_appendix("skills", skills_config_examples())
        .render()
}

const fn env_overrides() -> &'static [EnvOverrideDoc] {
    &[
        EnvOverrideDoc {
            variable: BCODE_CONFIG_ENV,
            scope: "process",
            description: "Path to a TOML config overlay file.",
        },
        EnvOverrideDoc {
            variable: BCODE_CONFIG_TOML_ENV,
            scope: "process",
            description: "Raw TOML config overlay data.",
        },
        EnvOverrideDoc {
            variable: BCODE_MODEL_PROFILE_ENV,
            scope: "client",
            description: "Selects the active model profile for this client connection.",
        },
        EnvOverrideDoc {
            variable: BCODE_AUTH_PROFILE_ENV,
            scope: "client",
            description: "Selects the active auth profile for this client connection.",
        },
    ]
}

fn model_config_examples() -> String {
    String::from(
        "### Model Profile Example\n\n\
         ```toml\n\
         [model]\n\
         profile = \"daily\"\n\
\n\
         [model.profiles.daily]\n\
         provider_plugin_id = \"bcode.openai-compatible\"\n\
         model_id = \"gpt-5-codex\"\n\
         auth_profile = \"openai\"\n\
\n\
         [model.profiles.daily.request]\n\
         temperature = 0.2\n\
         ```\n",
    )
}

fn auth_config_examples() -> String {
    String::from(
        "### Auth Profile Example\n\n\
         ```toml\n\
         [auth]\n\
         active_profile = \"openai\"\n\
\n\
         [auth.profiles.openai]\n\
         provider_plugin_id = \"bcode.openai-compatible\"\n\
         api_key_env = \"OPENAI_API_KEY\"\n\
         ```\n",
    )
}

fn agent_config_examples() -> String {
    String::from(
        "### Agent Policy Example\n\n\
         ```toml\n\
         [agent.plan.permissions]\n\
         read = \"allow\"\n\
         write = \"ask\"\n\
         execute = \"deny\"\n\
\n\
         [agent.build.tools.shell]\n\
         execute = \"ask\"\n\
         ```\n",
    )
}

fn skills_config_examples() -> String {
    String::from(
        "### Skills Example\n\n\
         ```toml\n\
         [skills]\n\
         auto_activate = \"suggest\"\n\
         roots = [\"~/.config/bcode/skills\"]\n\
\n\
         [skills.prompt]\n\
         catalog = \"summary\"\n\
         include_sources = true\n\
         ```\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_config::ConfigDocSchema;

    #[test]
    fn cli_reference_uses_real_root_command() {
        let doc = generate_cli_reference();

        assert!(doc.contains("# `bcode`"));
        assert!(doc.contains("TUI-first coding agent"));
        assert!(doc.contains("## `bcode plugin`"));
        assert!(doc.contains("## `bcode model`"));
    }

    #[test]
    fn config_reference_renders_all_root_sections() {
        let doc = generate_config_reference();

        for field in BcodeConfig::field_docs() {
            let heading = format!("## `[{}]`", field.toml_key);
            assert!(doc.contains(&heading), "missing heading: {heading}");
        }
    }

    #[test]
    fn config_reference_documents_env_overrides() {
        let doc = generate_config_reference();

        for override_doc in env_overrides() {
            assert!(
                doc.contains(override_doc.variable),
                "missing env override: {}",
                override_doc.variable
            );
        }
    }

    #[test]
    fn config_reference_documents_nested_model_profiles() {
        let doc = generate_config_reference();

        assert!(doc.contains("profiles.<profile>.provider_plugin_id"));
        assert!(doc.contains("profiles.<profile>.request.temperature"));
        assert!(doc.contains("reasoning.effort"));
        assert!(doc.contains("conversation_reuse.enabled"));
    }

    #[test]
    fn config_reference_documents_auth_and_agent_policy() {
        let doc = generate_config_reference();

        assert!(doc.contains("profiles.<profile>.api_key_env"));
        assert!(doc.contains("pools.<pool>.strategy"));
        assert!(doc.contains("tools.<tool-name>.execute"));
        assert!(doc.contains("permissions.network"));
    }

    #[test]
    fn config_reference_documents_enum_values() {
        let doc = generate_config_reference();

        assert!(doc.contains("`off`"));
        assert!(doc.contains("`suggest`"));
        assert!(doc.contains("`round_robin`"));
        assert!(doc.contains("`ask`"));
    }

    #[test]
    fn markdown_sections_used_by_docs_routes_exist() {
        let readme = include_str!("../../../../../README.md");
        assert!(
            readme.contains("## TUI keybindings"),
            "README heading '## TUI keybindings' missing"
        );
    }
}
