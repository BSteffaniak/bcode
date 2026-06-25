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
    let mut doc =
        hyperchad_docs_site::CliReference::new("bcode", bcode_cli::root_command()).render();
    doc.push_str(cli_examples());
    doc
}

const fn cli_examples() -> &'static str {
    "\n## Examples\n\n\
     ### Start the TUI\n\n\
     ```bash\n\
     bcode tui\n\
     ```\n\n\
     ### Attach to a recent session\n\n\
     ```bash\n\
     bcode attach --recent\n\
     ```\n\n\
     ### Send a prompt from the shell\n\n\
     ```bash\n\
     bcode send \"Summarize the current repository\"\n\
     ```\n\n\
     ### Select a model profile\n\n\
     ```bash\n\
     bcode --profile daily tui\n\
     ```\n\n\
     ### Inspect configured plugins\n\n\
     ```bash\n\
     bcode plugin list\n\
     ```\n\n\
     ### Manage provider authentication\n\n\
     ```bash\n\
     bcode auth status\n\
     ```\n\n\
     ### Cancel active work\n\n\
     ```bash\n\
     bcode cancel\n\
     ```\n"
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
        .section_appendix("worktree", worktree_config_examples())
        .section_appendix("tools", tools_config_examples())
        .section_appendix("session_import", session_import_config_examples())
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
        "### Model Profile Examples\n\n\
         OpenAI-compatible profile:\n\n\
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
         ```\n\n\
         Bedrock profile:\n\n\
         ```toml\n\
         [model.profiles.bedrock]\n\
         provider_plugin_id = \"bcode.bedrock\"\n\
         model_id = \"anthropic.claude-sonnet-4-5-20250929-v1:0\"\n\
         auth_profile = \"aws\"\n\
         ```\n",
    )
}

fn auth_config_examples() -> String {
    String::from(
        "### Auth Profile Examples\n\n\
         API key auth profile:\n\n\
         ```toml\n\
         [auth]\n\
         active_profile = \"openai\"\n\
\n\
         [auth.profiles.openai]\n\
         provider_plugin_id = \"bcode.openai-compatible\"\n\
         api_key_env = \"OPENAI_API_KEY\"\n\
         ```\n\n\
         Auth pool/failover profile set:\n\n\
         ```toml\n\
         [auth.pools.openai-failover]\n\
         strategy = \"failover\"\n\
         profiles = [\"openai-primary\", \"openai-secondary\"]\n\
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
\n\
         [skills.sources]\n\
         paths = [\"~/.config/bcode/skills\"]\n\
\n\
         [skills.prompt]\n\
         catalog = \"summary\"\n\
         include_sources = true\n\
         ```\n",
    )
}

fn worktree_config_examples() -> String {
    String::from(
        "### Worktree Defaults Example\n\n\
         ```toml\n\
         [worktree]\n\
         root = \".bcode/worktrees\"\n\
         branch_prefix = \"bcode/\"\n\
         base_ref = \"default_branch\"\n\
\n\
         [worktree.setup]\n\
         enabled = true\n\
         profile = \"native\"\n\
         ```\n",
    )
}

fn tools_config_examples() -> String {
    String::from(
        "### Shell Environment Example\n\n\
         ```toml\n\
         [tools.shell]\n\
         max_output_bytes = 10485760\n\
         inline_output_bytes = 16384\n\
\n\
         [tools.shell.env]\n\
         mode = \"auto\"\n\
         auto_fallback = \"error\"\n\
         ```\n",
    )
}

fn session_import_config_examples() -> String {
    String::from(
        "### Session Import Example\n\n\
         ```toml\n\
         [session_import]\n\
         enabled = true\n\
         auto_discover_on_startup = true\n\
\n\
         [session_import.pi]\n\
         enabled = true\n\
         path_mode = \"defaults_and_custom\"\n\
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
        assert!(doc.contains("## Examples"));
        assert!(doc.contains("bcode --profile daily tui"));
        assert!(doc.contains("bcode auth status"));
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
        assert!(doc.contains("conversation_reuse.mode"));
        assert!(doc.contains("retry.max_overload_retries"));
    }

    #[test]
    fn config_reference_documents_auth_and_agent_policy() {
        let doc = generate_config_reference();

        assert!(doc.contains("profiles.<profile>.map.<credential>.env"));
        assert!(doc.contains("pools.<pool>.strategy"));
        assert!(doc.contains("<agent-id>.tools.<tool-id>.enabled"));
        assert!(doc.contains("<agent-id>.permission.external_directory"));
    }

    #[test]
    fn config_reference_documents_derived_stable_sections() {
        let doc = generate_config_reference();

        assert!(doc.contains("enabled"));
        assert!(doc.contains("prompt.catalog"));
        assert!(doc.contains("persist_tool_io"));
        assert!(doc.contains("idle_shutdown_after_secs"));
        assert!(doc.contains("setup.profile"));
        assert!(doc.contains("shell.env.mode"));
        assert!(doc.contains("shell.env.auto_fallback"));
    }

    #[test]
    fn config_reference_includes_practical_examples() {
        let doc = generate_config_reference();

        assert!(doc.contains("bcode.bedrock"));
        assert!(doc.contains("openai-failover"));
        assert!(doc.contains("[worktree.setup]"));
        assert!(doc.contains("[tools.shell.env]"));
        assert!(doc.contains("[session_import.pi]"));
    }

    #[test]
    fn config_reference_documents_enum_values() {
        let doc = generate_config_reference();

        assert!(doc.contains("`off`"));
        assert!(doc.contains("`suggest`"));
        assert!(doc.contains("`failover`"));
        assert!(doc.contains("`ask`"));
    }

    #[test]
    fn config_reference_renders_defaults() {
        let doc = generate_config_reference();

        assert_row_default(&doc, "level", "standard");
        assert_row_default(&doc, "enabled", "true");
        assert_row_default(&doc, "retry.max_overload_retries", "5");
        assert_row_default(&doc, "compaction.mode", "on_overflow");
        assert_row_default(&doc, "mouse.scroll_rows", "3");
        assert_row_default(&doc, "pools.<pool>.strategy", "failover");
    }

    fn assert_row_default(doc: &str, key: &str, default: &str) {
        let expected_key = format!("`{key}`");
        let expected_default = format!("`{default}`");
        assert!(
            doc.lines()
                .any(|line| line.contains(&expected_key) && line.contains(&expected_default)),
            "missing default {default} for {key}"
        );
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
