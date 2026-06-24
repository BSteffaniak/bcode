//! Generation helpers for documentation pages.

use std::fmt::Write as _;

use bcode_config::{
    BCODE_AUTH_PROFILE_ENV, BCODE_CONFIG_ENV, BCODE_CONFIG_TOML_ENV, BCODE_MODEL_PROFILE_ENV,
};

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

/// Generate a code-sourced configuration reference.
///
/// This first-pass reference documents the real top-level config tables and
/// process-level environment overlays from `bcode_config`. Nested field-level
/// docs can be migrated to `hyperchad_docs_config` derives incrementally.
pub(crate) fn generate_config_reference() -> String {
    let mut doc = String::from(
        "bcode is configured with `bcode.toml`. Configuration can be layered from \
         files, raw TOML environment overlays, active model profiles, auth profiles, \
         and plugin/provider defaults.\n\n",
    );

    doc.push_str("## Path & Env Overrides\n\n");
    doc.push_str("| Variable | Scope | Behavior |\n|----------|-------|----------|\n");
    for (variable, scope, behavior) in env_overrides() {
        writeln!(doc, "| `{variable}` | {scope} | {behavior} |")
            .expect("writing to string cannot fail");
    }

    doc.push_str("\n---\n\n");
    doc.push_str("## Top-level Tables\n\n");
    doc.push_str("| Table | Behavior |\n|-------|----------|\n");
    for (table, behavior) in root_tables() {
        writeln!(doc, "| `[{table}]` | {behavior} |").expect("writing to string cannot fail");
    }

    doc.push_str(
        "\n## Next Steps\n\n\
         This page is intentionally generated from code-owned config metadata. \
         The current baseline documents stable root tables and environment \
         overlays; nested field-level docs should be expanded by adding \
         `ConfigDoc`/`ConfigDocEnum` derives and doc comments to the matching \
         config structs.\n",
    );

    doc
}

const fn env_overrides() -> &'static [(&'static str, &'static str, &'static str)] {
    &[
        (
            BCODE_CONFIG_ENV,
            "process",
            "Path to a TOML config overlay file.",
        ),
        (
            BCODE_CONFIG_TOML_ENV,
            "process",
            "Raw TOML config overlay data.",
        ),
        (
            BCODE_MODEL_PROFILE_ENV,
            "client",
            "Selects the active model profile for this client connection.",
        ),
        (
            BCODE_AUTH_PROFILE_ENV,
            "client",
            "Selects the active auth profile for this client connection.",
        ),
    ]
}

const fn root_tables() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "composition",
            "Config composition metadata and profile selection.",
        ),
        ("plugins", "Bundled and external plugin selection."),
        (
            "model",
            "Model provider, profile, alias, and metadata settings.",
        ),
        (
            "agent",
            "Per-agent permission and tool policy configuration.",
        ),
        ("auth", "Provider authentication profiles and pools."),
        ("observability", "Logging, tracing, and telemetry controls."),
        (
            "skills",
            "Skill discovery, activation, and prompt catalog settings.",
        ),
        ("system_prompt", "System prompt mode and section controls."),
        ("tui", "Terminal UI behavior and appearance."),
        ("session_import", "External session import plugin settings."),
        ("daemon", "Daemon lifecycle and connection settings."),
        ("worktree", "Worktree creation and naming defaults."),
        ("tools", "Built-in tool behavior and environment controls."),
        (
            "web_search",
            "Provider-specific web search plugin configuration.",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_reference_uses_real_root_command() {
        let doc = generate_cli_reference();

        assert!(doc.contains("# `bcode`"));
        assert!(doc.contains("TUI-first coding agent"));
        assert!(doc.contains("## `bcode plugin`"));
        assert!(doc.contains("## `bcode model`"));
    }

    #[test]
    fn config_reference_documents_env_overrides_and_root_tables() {
        let doc = generate_config_reference();

        for (variable, _, _) in env_overrides() {
            assert!(doc.contains(variable), "missing env override: {variable}");
        }
        for (table, _) in root_tables() {
            assert!(
                doc.contains(&format!("`[{table}]`")),
                "missing table: {table}"
            );
        }
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
