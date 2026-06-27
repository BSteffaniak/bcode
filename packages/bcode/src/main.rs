#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

#[tokio::main]
async fn main() {
    if let Err(error) = Box::pin(bcode_cli::run_with_static_bundled(static_bundled_plugins())).await
    {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

/// Return statically bundled plugin registrations enabled at compile time.
#[must_use]
fn static_bundled_plugins() -> Vec<bcode_plugin::StaticBundledPlugin> {
    vec![
        #[cfg(feature = "static-bundled-bedrock-provider-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/bedrock-provider-plugin/bcode-plugin.toml"),
            bcode_bedrock_provider_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-blims-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/blims-plugin/bcode-plugin.toml"),
            bcode_blims_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-code-review-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/code-review-plugin/bcode-plugin.toml"),
            bcode_code_review_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-default-agents-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/default-agents-plugin/bcode-plugin.toml"),
            bcode_default_agents_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-document-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/document-plugin/bcode-plugin.toml"),
            bcode_document_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-ocr-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/ocr-plugin/bcode-plugin.toml"),
            bcode_ocr_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-fake-provider-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/fake-provider-plugin/bcode-plugin.toml"),
            bcode_fake_provider_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-filesystem-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/filesystem-plugin/bcode-plugin.toml"),
            bcode_filesystem_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-git-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/git-plugin/bcode-plugin.toml"),
            bcode_git_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-github-review-publisher-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/github-review-publisher-plugin/bcode-plugin.toml"),
            bcode_github_review_publisher_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-model-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/model-plugin/bcode-plugin.toml"),
            bcode_model_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-openai-compatible-provider-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/openai-compatible-provider-plugin/bcode-plugin.toml"),
            bcode_openai_compatible_provider_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-opencode-session-import-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/opencode-session-import-plugin/bcode-plugin.toml"),
            bcode_opencode_session_import_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-pi-session-import-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/pi-session-import-plugin/bcode-plugin.toml"),
            bcode_pi_session_import_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-shell-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/shell-plugin/bcode-plugin.toml"),
            bcode_shell_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-skills-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/skills-plugin/bcode-plugin.toml"),
            bcode_skills_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-web-search-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/web-search-plugin/bcode-plugin.toml"),
            bcode_web_search_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-worktree-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/worktree-plugin/bcode-plugin.toml"),
            bcode_worktree_plugin::static_plugin(),
        ),
    ]
}
