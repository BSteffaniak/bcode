#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Single source of truth for statically bundled Bcode plugins.

/// Return statically bundled native TUI extension registrations.
#[must_use]
#[allow(clippy::missing_const_for_fn, clippy::vec_init_then_push)] // Feature-selected builds push function pointers at runtime.
pub fn static_tui_extensions() -> Vec<bcode_plugin_sdk::tui::StaticPluginTuiExtension> {
    #[allow(unused_mut)]
    let mut extensions = Vec::new();
    #[cfg(feature = "static-bundled-code-review-plugin")]
    extensions.push(bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
        "bcode.code_review",
        bcode_code_review_plugin::tui::tui_registry,
    ));
    #[cfg(feature = "static-bundled-document-plugin")]
    extensions.push(bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
        "bcode.document",
        bcode_document_plugin::document_tui_registry,
    ));
    #[cfg(feature = "static-bundled-eval-plugin")]
    extensions.push(bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
        "bcode.eval",
        bcode_eval_plugin::tui::tui_registry,
    ));
    #[cfg(feature = "static-bundled-filesystem-plugin")]
    extensions.push(bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
        "bcode.filesystem",
        bcode_filesystem_plugin::filesystem_tui_registry,
    ));
    #[cfg(feature = "static-bundled-git-plugin")]
    extensions.push(bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
        "bcode.git",
        bcode_git_plugin::git_tui_registry,
    ));
    #[cfg(feature = "static-bundled-loop-plugin")]
    extensions.push(bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
        "bcode.loop",
        bcode_loop_plugin::tui_registry,
    ));
    #[cfg(feature = "static-bundled-metrics-plugin")]
    extensions.push(bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
        "bcode.metrics",
        bcode_metrics_plugin::tui::tui_registry,
    ));
    #[cfg(feature = "static-bundled-model-plugin")]
    extensions.push(bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
        "bcode.model",
        bcode_model_plugin::model_tui_registry,
    ));
    #[cfg(feature = "static-bundled-ocr-plugin")]
    extensions.push(bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
        "bcode.ocr",
        bcode_ocr_plugin::ocr_tui_registry,
    ));
    #[cfg(feature = "static-bundled-question-plugin")]
    extensions.push(bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
        "bcode.question",
        bcode_question_plugin::question_tui_registry,
    ));
    #[cfg(feature = "static-bundled-ralph-plugin")]
    extensions.push(bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
        "bcode.ralph",
        bcode_ralph_plugin::tui_registry,
    ));
    #[cfg(feature = "static-bundled-shell-plugin")]
    extensions.push(bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
        "bcode.shell",
        bcode_shell_plugin::shell_tui_registry,
    ));
    #[cfg(feature = "static-bundled-skills-plugin")]
    extensions.push(bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
        "bcode.skills",
        bcode_skills_plugin::skills_tui_registry,
    ));
    #[cfg(feature = "static-bundled-vim-edit-plugin")]
    extensions.push(bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
        "bcode.vim-edit",
        bcode_vim_edit_plugin::vim_edit_tui_registry,
    ));
    #[cfg(feature = "static-bundled-web-search-plugin")]
    extensions.push(bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
        "bcode.web-search",
        bcode_web_search_plugin::web_search_tui_registry,
    ));
    #[cfg(feature = "static-bundled-workflow-plugin")]
    extensions.push(bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
        "bcode.workflow",
        bcode_workflow_plugin::tui::tui_registry,
    ));
    #[cfg(feature = "static-bundled-worktree-plugin")]
    extensions.push(bcode_plugin_sdk::tui::StaticPluginTuiExtension::new(
        "bcode.worktree",
        bcode_worktree_plugin::worktree_tui_registry,
    ));
    extensions
}

/// Return a native TUI registry for one enabled statically bundled plugin.
#[must_use]
#[allow(clippy::missing_const_for_fn)]
pub fn tui_registry(plugin_id: &str) -> Option<bcode_plugin_sdk::tui::PluginTuiRegistry> {
    match plugin_id {
        #[cfg(feature = "static-bundled-code-review-plugin")]
        "bcode.code_review" => Some(bcode_code_review_plugin::tui::tui_registry()),
        #[cfg(feature = "static-bundled-document-plugin")]
        "bcode.document" => Some(bcode_document_plugin::document_tui_registry()),
        #[cfg(feature = "static-bundled-eval-plugin")]
        "bcode.eval" => Some(bcode_eval_plugin::tui::tui_registry()),
        #[cfg(feature = "static-bundled-filesystem-plugin")]
        "bcode.filesystem" => Some(bcode_filesystem_plugin::filesystem_tui_registry()),
        #[cfg(feature = "static-bundled-git-plugin")]
        "bcode.git" => Some(bcode_git_plugin::git_tui_registry()),
        #[cfg(feature = "static-bundled-loop-plugin")]
        "bcode.loop" => Some(bcode_loop_plugin::tui_registry()),
        #[cfg(feature = "static-bundled-metrics-plugin")]
        "bcode.metrics" => Some(bcode_metrics_plugin::tui::tui_registry()),
        #[cfg(feature = "static-bundled-model-plugin")]
        "bcode.model" => Some(bcode_model_plugin::model_tui_registry()),
        #[cfg(feature = "static-bundled-ocr-plugin")]
        "bcode.ocr" => Some(bcode_ocr_plugin::ocr_tui_registry()),
        #[cfg(feature = "static-bundled-question-plugin")]
        "bcode.question" => Some(bcode_question_plugin::question_tui_registry()),
        #[cfg(feature = "static-bundled-ralph-plugin")]
        "bcode.ralph" => Some(bcode_ralph_plugin::tui_registry()),
        #[cfg(feature = "static-bundled-shell-plugin")]
        "bcode.shell" => Some(bcode_shell_plugin::shell_tui_registry()),
        #[cfg(feature = "static-bundled-skills-plugin")]
        "bcode.skills" => Some(bcode_skills_plugin::skills_tui_registry()),
        #[cfg(feature = "static-bundled-vim-edit-plugin")]
        "bcode.vim-edit" => Some(bcode_vim_edit_plugin::vim_edit_tui_registry()),
        #[cfg(feature = "static-bundled-web-search-plugin")]
        "bcode.web-search" => Some(bcode_web_search_plugin::web_search_tui_registry()),
        #[cfg(feature = "static-bundled-workflow-plugin")]
        "bcode.workflow" => Some(bcode_workflow_plugin::tui::tui_registry()),
        #[cfg(feature = "static-bundled-worktree-plugin")]
        "bcode.worktree" => Some(bcode_worktree_plugin::worktree_tui_registry()),
        _ => None,
    }
}

/// Return all renderer interaction adapters enabled in this static bundle.
#[must_use]
#[allow(clippy::missing_const_for_fn)]
pub fn interaction_adapters(
    platform_id: &str,
) -> Vec<bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability> {
    #[cfg(feature = "static-bundled-question-plugin")]
    {
        vec![bcode_question_plugin::question_interaction_adapter(
            platform_id,
        )]
    }
    #[cfg(not(feature = "static-bundled-question-plugin"))]
    {
        let _ = platform_id;
        Vec::new()
    }
}

/// Select the highest-priority renderer interaction adapter for an opaque exchange.
#[must_use]
pub fn interaction_adapter(
    producer_id: &str,
    schema: &str,
    schema_version: u32,
    platform_id: &str,
) -> Option<bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability> {
    let adapters = interaction_adapters(platform_id);
    bcode_plugin_sdk::interaction::select_interaction_adapter(
        &adapters,
        producer_id,
        schema,
        schema_version,
        platform_id,
    )
    .cloned()
}

/// Return a renderer-neutral interaction registry for one enabled statically bundled plugin.
#[must_use]
#[allow(clippy::missing_const_for_fn)]
pub fn interaction_registry(
    plugin_id: &str,
) -> Option<bcode_plugin_sdk::interaction::PluginInteractionRegistry> {
    match plugin_id {
        #[cfg(feature = "static-bundled-question-plugin")]
        "bcode.question" => Some(bcode_question_plugin::question_interaction_registry()),
        _ => None,
    }
}

/// Return statically bundled plugin registrations enabled by this crate's feature set.
#[must_use]
pub fn static_bundled_plugins() -> Vec<bcode_plugin::StaticBundledPlugin> {
    let mut plugins = Vec::new();
    append_static_bundled_plugins(&mut plugins);
    plugins
}

fn append_static_bundled_plugins(plugins: &mut Vec<bcode_plugin::StaticBundledPlugin>) {
    plugins.reserve(0);
    #[cfg(feature = "static-bundled-bedrock-provider-plugin")]
    plugins.push(bedrock_provider_plugin());
    #[cfg(feature = "static-bundled-blims-plugin")]
    plugins.push(blims_plugin());
    #[cfg(feature = "static-bundled-code-review-plugin")]
    plugins.push(code_review_plugin());
    #[cfg(feature = "static-bundled-default-agents-plugin")]
    plugins.push(default_agents_plugin());
    #[cfg(feature = "static-bundled-document-plugin")]
    plugins.push(document_plugin());
    #[cfg(feature = "static-bundled-eval-plugin")]
    plugins.push(eval_plugin());
    #[cfg(feature = "static-bundled-metrics-plugin")]
    plugins.push(metrics_plugin());
    #[cfg(feature = "static-bundled-ocr-plugin")]
    plugins.push(ocr_plugin());
    #[cfg(feature = "static-bundled-fake-provider-plugin")]
    plugins.push(fake_provider_plugin());
    #[cfg(feature = "static-bundled-filesystem-plugin")]
    plugins.push(filesystem_plugin());
    #[cfg(feature = "static-bundled-git-plugin")]
    plugins.push(git_plugin());
    #[cfg(feature = "static-bundled-github-review-publisher-plugin")]
    plugins.push(github_review_publisher_plugin());
    #[cfg(feature = "static-bundled-model-plugin")]
    plugins.push(model_plugin());
    #[cfg(feature = "static-bundled-openai-compatible-provider-plugin")]
    plugins.push(openai_compatible_provider_plugin());
    #[cfg(feature = "static-bundled-opencode-session-import-plugin")]
    plugins.push(opencode_session_import_plugin());
    #[cfg(feature = "static-bundled-pi-session-import-plugin")]
    plugins.push(pi_session_import_plugin());
    #[cfg(feature = "static-bundled-progress-doc-plugin")]
    plugins.push(progress_doc_plugin());
    #[cfg(feature = "static-bundled-question-plugin")]
    plugins.push(question_plugin());
    #[cfg(feature = "static-bundled-loop-plugin")]
    plugins.push(static_loop_plugin());
    #[cfg(feature = "static-bundled-ralph-plugin")]
    plugins.push(ralph_plugin());
    #[cfg(feature = "static-bundled-read-plugin")]
    plugins.push(read_plugin());
    #[cfg(feature = "static-bundled-shell-plugin")]
    plugins.push(shell_plugin());
    #[cfg(feature = "static-bundled-compressed-session-search-plugin")]
    plugins.push(compressed_session_search_plugin());
    #[cfg(feature = "static-bundled-tantivy-session-search-plugin")]
    plugins.push(tantivy_session_search_plugin());
    #[cfg(feature = "static-bundled-skills-plugin")]
    plugins.push(skills_plugin());
    #[cfg(feature = "static-bundled-vim-edit-plugin")]
    plugins.push(vim_edit_plugin());
    #[cfg(feature = "static-bundled-web-search-plugin")]
    plugins.push(web_search_plugin());
    #[cfg(feature = "static-bundled-worktree-plugin")]
    plugins.push(worktree_plugin());
    #[cfg(feature = "static-bundled-workflow-plugin")]
    plugins.push(workflow_plugin());
}

#[cfg(feature = "static-bundled-bedrock-provider-plugin")]
fn bedrock_provider_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/bedrock-provider-plugin/bcode-plugin.toml"),
        bcode_bedrock_provider_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-blims-plugin")]
fn blims_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/blims-plugin/bcode-plugin.toml"),
        bcode_blims_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-code-review-plugin")]
fn code_review_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/code-review-plugin/bcode-plugin.toml"),
        bcode_code_review_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-default-agents-plugin")]
fn default_agents_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/default-agents-plugin/bcode-plugin.toml"),
        bcode_default_agents_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-document-plugin")]
fn document_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/document-plugin/bcode-plugin.toml"),
        bcode_document_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-eval-plugin")]
fn eval_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/eval-plugin/bcode-plugin.toml"),
        bcode_eval_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-metrics-plugin")]
fn metrics_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/metrics-plugin/bcode-plugin.toml"),
        bcode_metrics_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-ocr-plugin")]
fn ocr_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/ocr-plugin/bcode-plugin.toml"),
        bcode_ocr_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-fake-provider-plugin")]
fn fake_provider_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/fake-provider-plugin/bcode-plugin.toml"),
        bcode_fake_provider_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-filesystem-plugin")]
fn filesystem_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/filesystem-plugin/bcode-plugin.toml"),
        bcode_filesystem_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-git-plugin")]
fn git_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/git-plugin/bcode-plugin.toml"),
        bcode_git_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-github-review-publisher-plugin")]
fn github_review_publisher_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/github-review-publisher-plugin/bcode-plugin.toml"),
        bcode_github_review_publisher_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-model-plugin")]
fn model_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/model-plugin/bcode-plugin.toml"),
        bcode_model_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-openai-compatible-provider-plugin")]
fn openai_compatible_provider_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/openai-compatible-provider-plugin/bcode-plugin.toml"),
        bcode_openai_compatible_provider_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-opencode-session-import-plugin")]
fn opencode_session_import_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/opencode-session-import-plugin/bcode-plugin.toml"),
        bcode_opencode_session_import_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-pi-session-import-plugin")]
fn pi_session_import_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/pi-session-import-plugin/bcode-plugin.toml"),
        bcode_pi_session_import_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-progress-doc-plugin")]
fn progress_doc_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/progress-doc-plugin/bcode-plugin.toml"),
        bcode_progress_doc_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-question-plugin")]
fn question_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/question-plugin/bcode-plugin.toml"),
        bcode_question_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-loop-plugin")]
pub fn static_loop_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/loop-plugin/bcode-plugin.toml"),
        bcode_loop_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-ralph-plugin")]
fn ralph_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/ralph-plugin/bcode-plugin.toml"),
        bcode_ralph_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-read-plugin")]
fn read_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/read-plugin/bcode-plugin.toml"),
        bcode_read_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-shell-plugin")]
fn shell_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/shell-plugin/bcode-plugin.toml"),
        bcode_shell_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-compressed-session-search-plugin")]
fn compressed_session_search_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/compressed-session-search-plugin/bcode-plugin.toml"),
        bcode_compressed_session_search_plugin::static_plugin(),
    )
    .with_default_activation(bcode_plugin::PluginDefaultActivation::Disabled)
}

#[cfg(feature = "static-bundled-tantivy-session-search-plugin")]
fn tantivy_session_search_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/tantivy-session-search-plugin/bcode-plugin.toml"),
        bcode_tantivy_session_search_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-skills-plugin")]
fn skills_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/skills-plugin/bcode-plugin.toml"),
        bcode_skills_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-vim-edit-plugin")]
fn vim_edit_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/vim-edit-plugin/bcode-plugin.toml"),
        bcode_vim_edit_plugin::static_plugin(),
    )
    .with_default_activation(bcode_plugin::PluginDefaultActivation::Disabled)
}

#[cfg(feature = "static-bundled-web-search-plugin")]
fn web_search_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/web-search-plugin/bcode-plugin.toml"),
        bcode_web_search_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-worktree-plugin")]
fn worktree_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/worktree-plugin/bcode-plugin.toml"),
        bcode_worktree_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-workflow-plugin")]
fn workflow_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/workflow-plugin/bcode-plugin.toml"),
        bcode_workflow_plugin::static_plugin(),
    )
    .with_package_root(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../plugins/workflow-plugin"
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn bundled_tui_extension_catalog_constructs_exact_registered_adapters() {
        let extensions = super::static_tui_extensions();
        for (plugin_id, adapter_id, schema) in [
            (
                "bcode.filesystem",
                "filesystem-request-card",
                "bcode.filesystem.request",
            ),
            (
                "bcode.shell",
                "shell-run-request-card",
                "bcode.tool.request.shell.run",
            ),
        ] {
            if let Some(extension) = extensions
                .iter()
                .find(|extension| extension.plugin_id() == plugin_id)
            {
                assert!(
                    extension
                        .registry()
                        .supports_visual_adapter(adapter_id, schema),
                    "{plugin_id}/{adapter_id}"
                );
            }
        }
    }

    #[cfg(feature = "static-bundled-workflow-plugin")]
    #[test]
    fn disabling_workflow_removes_commands_status_and_tui() {
        assert!(super::tui_registry("bcode.workflow").is_some());
        let static_plugins = super::static_bundled_plugins();
        let disabled = bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::All,
            enabled: std::collections::BTreeSet::new(),
            disabled: std::collections::BTreeSet::from(["bcode.workflow".to_string()]),
        };
        let selected = bcode_plugin::filter_selected_static_plugins(&static_plugins, &disabled)
            .expect("manifest selection");
        assert!(
            selected
                .iter()
                .all(|(manifest, _)| manifest.id != "bcode.workflow")
        );
        let host = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &disabled,
            &static_plugins,
        )
        .expect("host");
        assert!(!host.plugin_ids().iter().any(|id| id == "bcode.workflow"));
        assert!(
            host.registry().workflow_templates().is_empty(),
            "disabled workflow plugin must contribute no templates"
        );
        assert!(
            host.registered_command_contributions(&bcode_command::CommandSurface::Palette)
                .iter()
                .all(|command| !command.id.starts_with("workflow"))
        );
    }

    #[test]
    fn bundled_plugin_sources_gate_dynamic_abi_exports() {
        let mut offenders = Vec::new();
        for source in plugin_source_paths() {
            let contents = std::fs::read_to_string(&source)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", source.display()));
            for export_macro in ["export_plugin!", "export_concurrent_plugin!"] {
                let mut search_start = 0;
                while let Some(relative_index) = contents[search_start..].find(export_macro) {
                    let index = search_start + relative_index;
                    let preceding = &contents[..index];
                    let immediately_preceding = preceding
                        .rsplit_once('\n')
                        .map_or(preceding, |(before_line, _)| before_line)
                        .rsplit_once('\n')
                        .map_or(preceding, |(_, line)| line)
                        .trim();
                    let allowed_guards = [
                        "#[cfg(not(feature = \"static-bundled\"))]",
                        "#[cfg(all(feature = \"dynamic-export\", not(feature = \"static-bundled\")))]",
                    ];
                    if !allowed_guards.contains(&immediately_preceding) {
                        offenders.push(format!("{}:{export_macro}", source.display()));
                    }
                    search_start = index + export_macro.len();
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "static bundled plugins must not export duplicate dynamic ABI symbols: {offenders:#?}"
        );
    }

    fn plugin_source_paths() -> Vec<std::path::PathBuf> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let mut paths = std::fs::read_dir(root)
            .expect("plugin root should be readable")
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("src/lib.rs"))
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    #[cfg(feature = "static-bundled-progress-doc-plugin")]
    #[test]
    fn progress_document_bundle_is_disableable_without_affecting_other_plugins() {
        let static_plugins = super::static_bundled_plugins();
        let enabled = bcode_plugin::filter_selected_static_plugins(
            &static_plugins,
            &bcode_plugin::PluginSelection::all_enabled(),
        )
        .expect("static plugin manifests parse");
        let manifest = enabled
            .iter()
            .find_map(|(manifest, _)| (manifest.id == "bcode.progress-doc").then_some(manifest))
            .expect("progress-document plugin is included in the static bundle");
        assert_eq!(manifest.services[0].workflow_blocks.len(), 4);

        let selection = bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::All,
            enabled: std::collections::BTreeSet::new(),
            disabled: std::collections::BTreeSet::from(["bcode.progress-doc".to_owned()]),
        };
        let selected = bcode_plugin::filter_selected_static_plugins(&static_plugins, &selection)
            .expect("disabled static plugin selection parses");
        assert!(
            selected
                .iter()
                .all(|(manifest, _)| manifest.id != "bcode.progress-doc")
        );
        let host = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &selection,
            &static_plugins,
        )
        .expect("disabled static plugin host should load");
        assert!(
            !host
                .plugin_ids()
                .iter()
                .any(|id| id == "bcode.progress-doc")
        );
    }

    #[cfg(feature = "static-bundled-loop-plugin")]
    #[test]
    fn disabling_loop_removes_all_manifest_contributions() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test Tokio runtime");
        let _guard = runtime.enter();
        let static_plugins = super::static_bundled_plugins();
        let selected = bcode_plugin::filter_selected_static_plugins(
            &static_plugins,
            &bcode_plugin::PluginSelection::all_enabled(),
        )
        .expect("static plugin manifests parse");
        let loop_manifest = selected
            .iter()
            .find_map(|(manifest, _)| (manifest.id == "bcode.loop").then_some(manifest))
            .expect("loop plugin is included in the static bundle");
        assert!(!loop_manifest.services.is_empty());
        let host = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection::all_enabled(),
            &static_plugins,
        )
        .expect("enabled static plugin host should load");
        assert!(host
            .registered_command_contributions(&bcode_command::CommandSurface::Palette)
            .iter()
            .any(|contribution| matches!(
                &contribution.action,
                bcode_command::CommandAction::Plugin { plugin_id, .. } if plugin_id == "bcode.loop"
            )));
        assert!(
            host.registered_command_contributions(&bcode_command::CommandSurface::Slash)
                .iter()
                .any(|contribution| {
                    contribution.id == "loop"
                        && matches!(
                            &contribution.action,
                            bcode_command::CommandAction::Plugin { plugin_id, .. }
                                if plugin_id == "bcode.loop"
                        )
                })
        );

        let selection = bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::All,
            enabled: std::collections::BTreeSet::new(),
            disabled: std::collections::BTreeSet::from(["bcode.loop".to_owned()]),
        };
        let selected = bcode_plugin::filter_selected_static_plugins(&static_plugins, &selection)
            .expect("disabled static plugin selection should parse");

        assert!(
            selected
                .iter()
                .all(|(manifest, _)| manifest.id != "bcode.loop")
        );
        let host = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &selection,
            &static_plugins,
        )
        .expect("disabled static plugin host should load");
        assert!(host
            .registered_command_contributions(&bcode_command::CommandSurface::Palette)
            .iter()
            .all(|contribution| !matches!(
                &contribution.action,
                bcode_command::CommandAction::Plugin { plugin_id, .. } if plugin_id == "bcode.loop"
            )));
        assert!(host
            .registered_command_contributions(&bcode_command::CommandSurface::Slash)
            .iter()
            .all(|contribution| !matches!(
                &contribution.action,
                bcode_command::CommandAction::Plugin { plugin_id, .. } if plugin_id == "bcode.loop"
            )));
        assert!(
            host.service_summaries()
                .iter()
                .all(|(plugin_id, _)| plugin_id != "bcode.loop")
        );
    }

    #[cfg(not(any(
        feature = "static-bundled-bedrock-provider-plugin",
        feature = "static-bundled-blims-plugin",
        feature = "static-bundled-code-review-plugin",
        feature = "static-bundled-default-agents-plugin",
        feature = "static-bundled-document-plugin",
        feature = "static-bundled-eval-plugin",
        feature = "static-bundled-fake-provider-plugin",
        feature = "static-bundled-filesystem-plugin",
        feature = "static-bundled-git-plugin",
        feature = "static-bundled-github-review-publisher-plugin",
        feature = "static-bundled-loop-plugin",
        feature = "static-bundled-model-plugin",
        feature = "static-bundled-ocr-plugin",
        feature = "static-bundled-openai-compatible-provider-plugin",
        feature = "static-bundled-opencode-session-import-plugin",
        feature = "static-bundled-pi-session-import-plugin",
        feature = "static-bundled-progress-doc-plugin",
        feature = "static-bundled-question-plugin",
        feature = "static-bundled-ralph-plugin",
        feature = "static-bundled-read-plugin",
        feature = "static-bundled-shell-plugin",
        feature = "static-bundled-skills-plugin",
        feature = "static-bundled-vim-edit-plugin",
        feature = "static-bundled-web-search-plugin",
        feature = "static-bundled-worktree-plugin"
    )))]
    #[test]
    fn bundled_plugins_are_opt_in() {
        assert!(super::static_bundled_plugins().is_empty());
        assert!(super::tui_registry("bcode.filesystem").is_none());
        assert!(super::interaction_registry("bcode.question").is_none());
        assert!(super::interaction_adapter("bcode.question", "request", 1, "tui").is_none());
        assert!(super::interaction_adapters("tui").is_empty());
    }

    #[cfg(feature = "static-bundled-question-plugin")]
    #[test]
    fn question_bundle_provides_platform_interaction_registry() {
        let registry = super::interaction_registry("bcode.question")
            .expect("question interaction registry is available");
        assert!(registry.supports("bcode.question"));
        let mut adapters = super::interaction_adapters("tui");
        assert_eq!(adapters.len(), 1);
        let adapter = adapters.pop().expect("question adapter");
        assert_eq!(adapter.producer_id, "bcode.question");
        assert_eq!(adapter.platform_id, "tui");
        assert_eq!(adapter.priority, 100);
        assert_eq!(adapter.min_schema_version, 1);
        assert_eq!(adapter.max_schema_version, 1);
        assert!(adapter.supports("bcode.question.request", 1));
        assert_eq!(adapter.interaction_kind, "bcode.question");
        assert_eq!(
            adapter.tui_surface_kind.as_deref(),
            Some("bcode.question.inline")
        );

        let web = super::interaction_adapter("bcode.question", "bcode.question.request", 1, "web")
            .expect("web question adapter");
        assert_eq!(web.platform_id, "web");
        assert_eq!(web.interaction_kind, adapter.interaction_kind);
        assert_eq!(web.tui_surface_kind, None);
        assert!(
            super::interaction_adapter(
                "bcode.question",
                "bcode.question.request",
                1,
                "unknown-platform",
            )
            .is_some_and(|unknown| {
                unknown.platform_id == "unknown-platform" && unknown.tui_surface_kind.is_none()
            })
        );
    }

    #[cfg(all(
        feature = "static-bundled-tantivy-session-search-plugin",
        feature = "static-bundled-compressed-session-search-plugin"
    ))]
    #[test]
    fn session_search_providers_have_safe_distribution_defaults() {
        let static_plugins = super::static_bundled_plugins();
        let all_ids = bcode_plugin::static_bundled_plugin_ids(&static_plugins)
            .expect("static plugin manifests parse");
        let default_ids = bcode_plugin::static_bundled_default_plugin_ids(&static_plugins)
            .expect("static plugin manifests parse");

        assert!(
            all_ids
                .iter()
                .any(|id| id == "bcode.tantivy-session-search")
        );
        assert!(
            all_ids
                .iter()
                .any(|id| id == "bcode.compressed-session-search")
        );
        assert!(
            default_ids
                .iter()
                .any(|id| id == "bcode.tantivy-session-search")
        );
        assert!(
            default_ids
                .iter()
                .all(|id| id != "bcode.compressed-session-search")
        );

        let default_selection = bcode_config::plugin_selection_with_default_plugin_ids(
            &bcode_config::BcodeConfig::default(),
            &default_ids,
        );
        assert!(default_selection.is_enabled("bcode.tantivy-session-search"));
        assert!(!default_selection.is_enabled("bcode.compressed-session-search"));

        let compressed_config = bcode_config::BcodeConfig {
            plugins: bcode_config::PluginConfig {
                enabled: std::collections::BTreeSet::from([
                    "bcode.compressed-session-search".to_owned()
                ]),
                ..bcode_config::PluginConfig::default()
            },
            ..bcode_config::BcodeConfig::default()
        };
        let compressed_selection = bcode_config::plugin_selection_with_default_plugin_ids(
            &compressed_config,
            &default_ids,
        );
        assert!(compressed_selection.is_enabled("bcode.compressed-session-search"));

        let disabled_config = bcode_config::BcodeConfig {
            plugins: bcode_config::PluginConfig {
                disabled: std::collections::BTreeSet::from([
                    "bcode.tantivy-session-search".to_owned()
                ]),
                ..bcode_config::PluginConfig::default()
            },
            ..bcode_config::BcodeConfig::default()
        };
        let disabled_selection =
            bcode_config::plugin_selection_with_default_plugin_ids(&disabled_config, &default_ids);
        assert!(!disabled_selection.is_enabled("bcode.tantivy-session-search"));

        let all_config = bcode_config::BcodeConfig {
            plugins: bcode_config::PluginConfig {
                default: bcode_config::PluginDefaultMode::All,
                disabled: std::collections::BTreeSet::from([
                    "bcode.compressed-session-search".to_owned()
                ]),
                ..bcode_config::PluginConfig::default()
            },
            ..bcode_config::BcodeConfig::default()
        };
        let all_selection =
            bcode_config::plugin_selection_with_default_plugin_ids(&all_config, &default_ids);
        assert!(all_selection.is_enabled("bcode.tantivy-session-search"));
        assert!(!all_selection.is_enabled("bcode.compressed-session-search"));
    }

    #[cfg(feature = "static-bundled-vim-edit-plugin")]
    #[test]
    fn vim_edit_is_available_but_not_a_distribution_default() {
        let static_plugins = super::static_bundled_plugins();
        let all_ids = bcode_plugin::static_bundled_plugin_ids(&static_plugins)
            .expect("static plugin manifests parse");
        let default_ids = bcode_plugin::static_bundled_default_plugin_ids(&static_plugins)
            .expect("static plugin manifests parse");

        assert!(all_ids.iter().any(|id| id == "bcode.vim-edit"));
        assert!(!default_ids.iter().any(|id| id == "bcode.vim-edit"));

        let default_selection = bcode_config::plugin_selection_with_default_plugin_ids(
            &bcode_config::BcodeConfig::default(),
            &default_ids,
        );
        let selected =
            bcode_plugin::filter_selected_static_plugins(&static_plugins, &default_selection)
                .expect("default static plugin selection parses");
        assert!(
            selected
                .iter()
                .all(|(manifest, _)| manifest.id != "bcode.vim-edit")
        );
        let default_host = bcode_plugin::PluginHost::load_static_plugins(&selected)
            .expect("default-selected static plugins load");
        assert!(
            !default_host
                .loaded_plugins()
                .iter()
                .any(|plugin| plugin.manifest().id == "bcode.vim-edit")
        );
        assert!(default_host.loaded_plugins().iter().all(|plugin| {
            plugin
                .manifest()
                .services
                .iter()
                .all(|service| service.interface_id != "bcode.tool/v1")
                || plugin.manifest().id != "bcode.vim-edit"
        }));
        assert!(
            default_host
                .visual_adapter(
                    "bcode.vim-edit.request.preview",
                    1,
                    "tui",
                    Some("bcode.vim-edit")
                )
                .is_none()
        );

        let mut explicitly_enabled_config = bcode_config::BcodeConfig::default();
        explicitly_enabled_config
            .plugins
            .enabled
            .insert("bcode.vim-edit".to_owned());
        let explicitly_enabled = bcode_config::plugin_selection_with_default_plugin_ids(
            &explicitly_enabled_config,
            &default_ids,
        );
        let selected =
            bcode_plugin::filter_selected_static_plugins(&static_plugins, &explicitly_enabled)
                .expect("explicit static plugin selection parses");
        assert!(
            selected
                .iter()
                .any(|(manifest, _)| manifest.id == "bcode.vim-edit")
        );
        let explicitly_enabled_host = bcode_plugin::PluginHost::load_static_plugins(&selected)
            .expect("explicitly selected Vim plugin loads");
        assert!(
            explicitly_enabled_host
                .loaded_plugins()
                .iter()
                .any(|plugin| plugin.manifest().id == "bcode.vim-edit")
        );
        assert!(
            explicitly_enabled_host
                .loaded_plugins()
                .iter()
                .find(|plugin| plugin.manifest().id == "bcode.vim-edit")
                .expect("Vim plugin is loaded")
                .manifest()
                .services
                .iter()
                .any(|service| service.interface_id == "bcode.tool/v1")
        );
        assert!(
            explicitly_enabled_host
                .visual_adapter(
                    "bcode.vim-edit.request.preview",
                    1,
                    "tui",
                    Some("bcode.vim-edit")
                )
                .is_some()
        );

        let all_config = bcode_config::BcodeConfig {
            plugins: bcode_config::PluginConfig {
                default: bcode_config::PluginDefaultMode::All,
                ..bcode_config::PluginConfig::default()
            },
            ..bcode_config::BcodeConfig::default()
        };
        let all_selection =
            bcode_config::plugin_selection_with_default_plugin_ids(&all_config, &default_ids);
        let selected =
            bcode_plugin::filter_selected_static_plugins(&static_plugins, &all_selection)
                .expect("all static plugin selection parses");
        assert!(
            selected
                .iter()
                .any(|(manifest, _)| manifest.id == "bcode.vim-edit")
        );

        let disabled_config = bcode_config::BcodeConfig {
            plugins: bcode_config::PluginConfig {
                default: bcode_config::PluginDefaultMode::All,
                enabled: std::collections::BTreeSet::from(["bcode.vim-edit".to_owned()]),
                disabled: std::collections::BTreeSet::from(["bcode.vim-edit".to_owned()]),
                ..bcode_config::PluginConfig::default()
            },
            ..bcode_config::BcodeConfig::default()
        };
        let disabled_selection =
            bcode_config::plugin_selection_with_default_plugin_ids(&disabled_config, &default_ids);
        let selected =
            bcode_plugin::filter_selected_static_plugins(&static_plugins, &disabled_selection)
                .expect("disabled static plugin selection parses");
        assert!(
            selected
                .iter()
                .all(|(manifest, _)| manifest.id != "bcode.vim-edit")
        );
    }

    #[cfg(all(
        feature = "static-bundled-vim-edit-plugin",
        feature = "static-bundled-filesystem-plugin"
    ))]
    #[test]
    fn ordinary_bundled_plugins_remain_distribution_defaults() {
        let static_plugins = super::static_bundled_plugins();
        let default_ids = bcode_plugin::static_bundled_default_plugin_ids(&static_plugins)
            .expect("static plugin manifests parse");

        assert!(default_ids.iter().any(|id| id == "bcode.filesystem"));
        assert!(!default_ids.iter().any(|id| id == "bcode.vim-edit"));
    }

    #[cfg(feature = "static-bundled-vim-edit-plugin")]
    #[test]
    fn vim_edit_bundle_provides_playback_visual_without_interaction_registry() {
        let registry =
            super::tui_registry("bcode.vim-edit").expect("Vim edit TUI registry is available");
        assert!(registry.supports_visual("bcode.vim-edit.playback"));
        assert!(registry.supports_visual("bcode.vim-edit.request-draft.preview"));
        assert!(registry.supports_visual("bcode.vim-edit.request-draft.apply"));
        assert!(super::interaction_registry("bcode.vim-edit").is_none());
    }

    #[cfg(feature = "static-bundled-read-plugin")]
    #[test]
    fn read_bundle_contributes_local_cli_command() {
        let static_plugins = super::static_bundled_plugins();
        let plugin = static_plugins
            .iter()
            .find(|plugin| plugin.manifest_toml.contains("bcode.read"))
            .expect("read plugin is included in the static bundle");
        let registration = plugin
            .cli_registration()
            .expect("read plugin contributes a CLI command");

        assert_eq!((registration.command)().get_name(), "read");
        assert!(!registration.requires_daemon);
    }

    #[cfg(feature = "static-bundled-openai-compatible-provider-plugin")]
    #[test]
    fn openai_compatible_bundle_registers_api_key_auth_providers() {
        let static_plugins = super::static_bundled_plugins();
        let selected = bcode_plugin::filter_selected_static_plugins(
            &static_plugins,
            &bcode_plugin::PluginSelection::all_enabled(),
        )
        .expect("static plugin manifests parse");
        let mut host = bcode_plugin::PluginHost::load_static_plugins_best_effort(&selected);
        let openai = host.auth_provider("openai").expect("OpenAI auth provider");
        assert_eq!(openai.plugin_id, "bcode.openai-compatible");
        assert_eq!(openai.contribution.methods[0].method_id(), "api_key");
        assert_eq!(openai.contribution.methods[1].method_id(), "chatgpt");
        assert_eq!(openai.contribution.methods[2].method_id(), "device");
        let xai = host.auth_provider("xai").expect("xAI auth provider");
        assert_eq!(xai.plugin_id, "bcode.openai-compatible");
        assert_eq!(xai.contribution.methods.len(), 1);
        assert_eq!(xai.contribution.methods[0].method_id(), "api_key");
        host.deactivate_all().expect("deactivate static plugins");
    }

    #[cfg(feature = "static-bundled-web-search-plugin")]
    #[test]
    fn disabled_web_search_bundle_does_not_register_exa_auth_provider() {
        let static_plugins = super::static_bundled_plugins();
        let selection = bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::All,
            enabled: std::collections::BTreeSet::new(),
            disabled: std::collections::BTreeSet::from(["bcode.web-search".to_owned()]),
        };
        let selected = bcode_plugin::filter_selected_static_plugins(&static_plugins, &selection)
            .expect("static plugin manifests parse");
        let host = bcode_plugin::PluginHost::load_static_plugins_best_effort(&selected);
        assert!(host.auth_provider("exa").is_none());
    }

    #[cfg(feature = "static-bundled-web-search-plugin")]
    #[test]
    fn web_search_bundle_registers_exa_auth_provider() {
        let static_plugins = super::static_bundled_plugins();
        let selected = bcode_plugin::filter_selected_static_plugins(
            &static_plugins,
            &bcode_plugin::PluginSelection::all_enabled(),
        )
        .expect("static plugin manifests parse");
        let mut host = bcode_plugin::PluginHost::load_static_plugins_best_effort(&selected);
        let provider = host.auth_provider("exa").expect("Exa auth provider");
        assert_eq!(provider.plugin_id, "bcode.web-search");
        assert_eq!(provider.contribution.methods.len(), 1);
        assert_eq!(provider.contribution.methods[0].method_id(), "api_key");
        host.deactivate_all().expect("deactivate static plugins");
    }

    #[cfg(feature = "static-bundled-filesystem-plugin")]
    #[test]
    fn filesystem_bundle_provides_tui_file_change_visual_adapter() {
        let static_plugins = super::static_bundled_plugins();
        let selected = bcode_plugin::filter_selected_static_plugins(
            &static_plugins,
            &bcode_plugin::PluginSelection::all_enabled(),
        )
        .expect("static plugin manifests parse");
        assert!(
            selected
                .iter()
                .any(|(manifest, _)| manifest.id == "bcode.filesystem"),
            "filesystem plugin is included in the static bundle"
        );

        let host = bcode_plugin::PluginHost::load_static_plugins_best_effort(&selected);
        let route = host
            .visual_adapter(
                "bcode.filesystem.change",
                1,
                "tui",
                Some("bcode.filesystem"),
            )
            .expect("filesystem file-change visual adapter route");
        assert_eq!(route.plugin_id, "bcode.filesystem");

        let registry =
            super::tui_registry(&route.plugin_id).expect("filesystem TUI registry is available");
        let payload = serde_json::json!({
            "path": "src/lib.rs",
            "old_text": "before\n",
            "new_text": "after\n"
        });
        let context = bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
            80,
            bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
            None,
        );
        let rows = registry
            .visual_rows(&route.adapter_id, &route.schema, &payload, &context)
            .expect("filesystem TUI visual adapter renders file change payload");
        assert!(!rows.is_empty());
    }
}
