#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Plugin-owned path-only composer references. Discovery is delegated to the
//! daemon filesystem service; this crate never traverses or reads local files.

pub mod grammar;
pub mod search;
mod service;
pub mod state;
pub use service::FileReferencesPlugin;

/// Native static service registration used by the central bundle.
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(
        FileReferencesPlugin,
        include_str!("../bcode-plugin.toml")
    )
}

/// Enabled plugin-local composer adapters; disabling the plugin drops this registry.
///
/// # Panics
/// Panics only if registering one contributor in a fresh empty registry fails.
#[cfg(feature = "static-bundled")]
#[must_use]
pub fn file_references_tui_registry() -> bcode_plugin_sdk::tui::PluginTuiRegistry {
    let mut registry = bcode_plugin_sdk::tui::PluginTuiRegistry::default();
    register_tui(&mut registry).expect("one fresh file-reference contributor");
    registry
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(FileReferencesPlugin, include_str!("../bcode-plugin.toml"));

use bcode_plugin_sdk::composer::{
    ComposerActivation, ComposerDocument, ComposerTextRange, PluginComposerContributor,
};

/// Single supported manifest contribution; empty legacy dispatch selects this declaration.
pub const CONTRIBUTION_ID: &str = "paths";

/// Stable local grammar kind advertised by this plugin.
pub const ACTIVATION_KIND: &str = "bcode.file-references.path";

/// Local, lightweight activation adapter; it performs no IO.
#[derive(Debug, Default)]
pub struct FileReferenceContributor;

impl PluginComposerContributor for FileReferenceContributor {
    fn kind(&self) -> &'static str {
        ACTIVATION_KIND
    }

    fn activate(&self, document: &ComposerDocument) -> Option<ComposerActivation> {
        document.validate().ok()?;
        if !document.allows_activation() {
            return None;
        }
        let active = grammar::activate(&document.text, document.cursor)?;
        Some(ComposerActivation {
            range: ComposerTextRange {
                start: active.range.start,
                end: active.range.end,
            },
            query: active.query,
        })
    }
}

/// Register local activation only when the plugin's native registry is enabled.
///
/// # Errors
/// Returns the registry's conflict or invalid-kind error.
#[cfg(feature = "static-bundled")]
pub fn register_tui(
    registry: &mut bcode_plugin_sdk::tui::PluginTuiRegistry,
) -> Result<(), bcode_plugin_sdk::composer::PluginComposerRegistryError> {
    for (id, description, key) in [
        (
            "bcode.file-references.toggle_scope",
            "Toggle file search scope",
            "ctrl+g",
        ),
        (
            "bcode.file-references.refresh",
            "Refresh file search inventory",
            "ctrl+r",
        ),
    ] {
        registry.register_keymap_action(bcode_plugin_sdk::tui::keymap::KeymapAction {
            id: id.to_owned(),
            scope: "composer_interaction".to_owned(),
            description: description.to_owned(),
            defaults: vec![key.to_owned()],
        });
    }
    registry.register_composer_contributor(Box::new(FileReferenceContributor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_text_excludes_activation() {
        let mut document = ComposerDocument {
            document_id: "selection".into(),
            revision: 0,
            text: "@src".into(),
            cursor: 4,
            selection: Some(ComposerTextRange { start: 0, end: 4 }),
        };
        assert!(FileReferenceContributor.activate(&document).is_none());
        document.selection = Some(ComposerTextRange { start: 4, end: 4 });
        assert!(FileReferenceContributor.activate(&document).is_some());
    }
}
