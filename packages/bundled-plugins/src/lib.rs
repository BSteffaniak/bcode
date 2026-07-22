#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Single source of truth for statically bundled Bcode plugins.

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
    #[cfg(feature = "static-bundled-question-plugin")]
    plugins.push(question_plugin());
    #[cfg(feature = "static-bundled-loop-plugin")]
    plugins.push(loop_plugin());
    #[cfg(feature = "static-bundled-ralph-plugin")]
    plugins.push(ralph_plugin());
    #[cfg(feature = "static-bundled-shell-plugin")]
    plugins.push(shell_plugin());
    #[cfg(feature = "static-bundled-skills-plugin")]
    plugins.push(skills_plugin());
    #[cfg(feature = "static-bundled-vim-edit-plugin")]
    plugins.push(vim_edit_plugin());
    #[cfg(feature = "static-bundled-web-search-plugin")]
    plugins.push(web_search_plugin());
    #[cfg(feature = "static-bundled-worktree-plugin")]
    plugins.push(worktree_plugin());
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

#[cfg(feature = "static-bundled-question-plugin")]
fn question_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/question-plugin/bcode-plugin.toml"),
        bcode_question_plugin::static_plugin(),
    )
}

#[cfg(feature = "static-bundled-loop-plugin")]
fn loop_plugin() -> bcode_plugin::StaticBundledPlugin {
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

#[cfg(feature = "static-bundled-shell-plugin")]
fn shell_plugin() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/shell-plugin/bcode-plugin.toml"),
        bcode_shell_plugin::static_plugin(),
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

#[cfg(test)]
mod tests {
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
                    if immediately_preceding != "#[cfg(not(feature = \"static-bundled\"))]" {
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

    #[cfg(feature = "static-bundled-loop-plugin")]
    #[test]
    fn disabling_loop_removes_all_manifest_contributions() {
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
        feature = "static-bundled-question-plugin",
        feature = "static-bundled-ralph-plugin",
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
    }

    #[cfg(feature = "static-bundled-vim-edit-plugin")]
    #[test]
    fn vim_edit_bundle_provides_playback_visual_without_interaction_registry() {
        let registry =
            super::tui_registry("bcode.vim-edit").expect("Vim edit TUI registry is available");
        assert!(registry.supports_visual("bcode.vim-edit.playback"));
        assert!(super::interaction_registry("bcode.vim-edit").is_none());
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
            .visual_rows(&route.schema, &payload, &context)
            .expect("filesystem TUI visual adapter renders file change payload");
        assert!(!rows.is_empty());
    }
}
