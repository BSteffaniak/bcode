#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Single source of truth for statically bundled Bcode plugins.

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

    #[cfg(not(any(
        feature = "static-bundled-bedrock-provider-plugin",
        feature = "static-bundled-blims-plugin",
        feature = "static-bundled-code-review-plugin",
        feature = "static-bundled-default-agents-plugin",
        feature = "static-bundled-document-plugin",
        feature = "static-bundled-fake-provider-plugin",
        feature = "static-bundled-filesystem-plugin",
        feature = "static-bundled-git-plugin",
        feature = "static-bundled-github-review-publisher-plugin",
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

        let registry = host
            .tui_registry(&route.plugin_id)
            .expect("filesystem TUI registry is available");
        let payload = serde_json::json!({
            "path": "src/lib.rs",
            "old_text": "before\n",
            "new_text": "after\n"
        });
        let rows = registry
            .visual_rows(&route.schema, &payload, 80)
            .expect("filesystem TUI visual adapter renders file change payload");
        assert!(!rows.is_empty());
    }
}
