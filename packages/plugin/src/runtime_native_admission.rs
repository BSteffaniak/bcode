//! Explicit native deployment admission into the normal Bcode plugin host.
use std::{collections::BTreeSet, path::PathBuf};

use crate::{PluginHost, PluginLoadError, PluginManifest, ResolvedPluginConfig};

impl PluginHost {
    /// Load explicitly selected shared native backends without rewriting declarations.
    ///
    /// Artifact paths are deployment selections, not manifest replacements. Config
    /// and activation authority are supplied by the owning runtime. Successful
    /// admission uses ordinary contribution publication and runtime scheduling.
    ///
    /// # Errors
    /// Returns an error for duplicate plugin identities, missing/inconsistent host
    /// authority, declaration/artifact incompatibility, failed activation, invalid
    /// contributions, or tool-presentation ownership conflicts. Failed cleanup is
    /// retained by the normal host lifecycle; no legacy artifact is substituted.
    pub fn load_native_backend_components(
        components: Vec<(PluginManifest, PathBuf, ResolvedPluginConfig)>,
    ) -> Result<Self, PluginLoadError> {
        let mut identities = BTreeSet::new();
        let mut runtime = None;
        for (manifest, _, config) in &components {
            if !identities.insert(manifest.id.clone()) {
                return Err(super::error(
                    &manifest.id,
                    "duplicate_native_backend_selection",
                ));
            }
            let activation = config
                .shared_activation
                .as_ref()
                .ok_or_else(|| super::error(&manifest.id, "shared_activation_context_required"))?;
            activation.validate(&manifest.id)?;
            if runtime.is_some_and(|selected| selected != activation.runtime) {
                return Err(super::error(
                    &manifest.id,
                    "inconsistent_shared_runtime_selection",
                ));
            }
            runtime = Some(activation.runtime);
        }
        crate::validate_tool_presentation_declarations(
            components.iter().map(|(manifest, _, _)| manifest),
        )?;
        let mut host = Self::default();
        for (manifest, artifact, config) in components {
            let loaded = crate::load_native_component_with_config(
                manifest,
                &artifact,
                config,
                bcode_plugin_sdk::definition::ComponentRole::Backend,
            )?;
            host.configs
                .insert(loaded.manifest().id.clone(), loaded.config().clone());
            host.activate_plugin(loaded)?;
        }
        host.validate_tool_presentation_ownership()?;
        Ok(host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> PluginManifest {
        toml::from_str(include_str!(
            "../../../examples/hello-plugin/bcode-plugin.toml"
        ))
        .expect("fixture declaration")
    }

    #[test]
    fn native_selection_requires_explicit_authority_before_artifact_loading() {
        let result = PluginHost::load_native_backend_components(vec![(
            manifest(),
            PathBuf::from("/missing/not-selected.dylib"),
            ResolvedPluginConfig::default(),
        )]);
        assert!(matches!(
            result,
            Err(PluginLoadError::LifecycleFailed {
                hook: "shared_activation_context_required",
                ..
            })
        ));
    }

    #[test]
    fn duplicate_native_selection_fails_before_loading_or_activating_either_artifact() {
        let declaration = manifest();
        let config = crate::shared_instance_tests::fixture_config(&declaration.id);
        let result = PluginHost::load_native_backend_components(vec![
            (
                declaration.clone(),
                PathBuf::from("/missing/first.dylib"),
                config.clone(),
            ),
            (declaration, PathBuf::from("/missing/second.dylib"), config),
        ]);
        assert!(matches!(
            result,
            Err(PluginLoadError::LifecycleFailed {
                hook: "duplicate_native_backend_selection",
                ..
            })
        ));
    }
}
