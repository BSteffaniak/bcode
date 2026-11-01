//! Secret-free Recap connection selection and user preferences.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Optional overrides; absent fields inherit Bcode's configured defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub config_path: Option<PathBuf>,
    pub profile: Option<String>,
    pub context: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub endpoint: Option<String>,
    /// Configured authentication reference, never credential material.
    pub auth_profile: Option<String>,
}

/// A locally configured connection, never credentials or raw configuration.
#[cfg(any(feature = "tui", test))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    pub label: String,
    pub selection: Selection,
}

/// Normalized model information from the Bcode SDK.
#[cfg(any(feature = "tui", test))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    pub provider: Option<String>,
    pub label: String,
    pub images: bool,
}

/// Local, no-inference discovery result.
#[cfg(any(feature = "tui", test))]
#[derive(Clone, Debug)]
pub struct Discovery {
    pub connections: Vec<Connection>,
    pub models: Vec<Model>,
    pub selected: Selection,
}

/// An actionable, secret-safe settings failure.
#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error(
        "Bcode selection is invalid or unavailable; check config path, context, profile, model, provider and authentication in Settings"
    )]
    Configuration,
    #[error("model initialization failed ({code})", code = .0.code())]
    Model(crate::bcode_adapter::ModelFailure),
    #[error(
        "custom endpoint must be an HTTP(S) URL without credentials, query or fragment; select the OpenAI-compatible provider and an explicit configured API-key auth profile (OAuth/subscriptions cannot be redirected)"
    )]
    Endpoint,
    #[error(
        "Recap preferences are unreadable, incompatible or damaged; select a different preference path or reset to automatic"
    )]
    Preferences,
    #[cfg(any(feature = "tui", test))]
    #[error(
        "provider configuration discovery exceeded its bounded limit; choose a config path explicitly"
    )]
    DiscoveryLimit,
}

const MAX_PREFERENCES_BYTES: u64 = 16 * 1024;
#[cfg(any(feature = "tui", test))]
const MAX_CONNECTIONS: usize = 128;

/// Merge CLI or interactive overrides over saved preferences before discovery or execution.
/// Automatic Bcode configuration supplies any remaining absent fields.
/// # Errors
/// Damaged preferences or invalid explicit fields are surfaced, never silently ignored.
pub fn effective_selection(explicit: &Selection) -> Result<Selection, ConnectionError> {
    effective_selection_from(explicit, &preferences_path()?)
}

/// The path-explicit equivalent of [`effective_selection`], useful for isolated callers.
/// # Errors
/// Returns an error for unreadable preferences or invalid selection fields.
pub fn effective_selection_from(
    explicit: &Selection,
    path: &std::path::Path,
) -> Result<Selection, ConnectionError> {
    explicit.validate()?;
    let selection = explicit.over(&load_preferences(path)?);
    selection.validate()?;
    Ok(selection)
}

impl Selection {
    /// Merge explicit overrides over a saved selection (which itself overrides automatic config).
    #[must_use]
    pub fn over(&self, saved: &Self) -> Self {
        let saved = if self.config_path.is_some() && self.config_path != saved.config_path {
            &Self::default()
        } else {
            saved
        };
        Self {
            config_path: self
                .config_path
                .clone()
                .or_else(|| saved.config_path.clone()),
            profile: self.profile.clone().or_else(|| saved.profile.clone()),
            context: self.context.clone().or_else(|| saved.context.clone()),
            model: self.model.clone().or_else(|| saved.model.clone()),
            provider: self.provider.clone().or_else(|| saved.provider.clone()),
            endpoint: self.endpoint.clone().or_else(|| saved.endpoint.clone()),
            auth_profile: self
                .auth_profile
                .clone()
                .or_else(|| saved.auth_profile.clone()),
        }
    }

    pub(crate) fn options(&self) -> bcode::ConfiguredModelOptions {
        let mut settings = std::collections::BTreeMap::new();
        if let Some(endpoint) = &self.endpoint {
            settings.insert("openai.base_url".into(), endpoint.clone());
        }
        bcode::ConfiguredModelOptions {
            config_path: self.config_path.clone(),
            profile: self.profile.clone(),
            context: self.context.clone(),
            model: self.model.clone(),
            provider: self.provider.clone(),
            auth_profile: self.auth_profile.clone(),
            settings,
        }
    }

    fn validate(&self) -> Result<(), ConnectionError> {
        for value in [
            &self.profile,
            &self.context,
            &self.model,
            &self.provider,
            &self.auth_profile,
        ]
        .into_iter()
        .flatten()
        {
            if value.trim().is_empty() || value.len() > 1024 || value.chars().any(char::is_control)
            {
                return Err(ConnectionError::Configuration);
            }
        }
        if self.config_path.as_ref().is_some_and(|path| {
            path.as_os_str().is_empty()
                || path
                    .to_str()
                    .is_none_or(|value| value.contains("://") || value.len() > 4096)
        }) {
            return Err(ConnectionError::Configuration);
        }
        if let Some(endpoint) = &self.endpoint {
            normalized_endpoint(endpoint)?;
        }
        Ok(())
    }
}

fn normalized_endpoint(endpoint: &str) -> Result<String, ConnectionError> {
    if endpoint.len() > 4096
        || endpoint.trim() != endpoint
        || endpoint.chars().any(char::is_control)
    {
        return Err(ConnectionError::Endpoint);
    }
    let url = url::Url::parse(endpoint).map_err(|_| ConnectionError::Endpoint)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConnectionError::Endpoint);
    }
    Ok(url.to_string())
}

/// Inspect locally configured connections and normalized bundled models. No authentication or network calls.
/// # Errors
/// Invalid explicit selections and bounded discovery failures are actionable, never substituted.
#[cfg(any(feature = "tui", test))]
pub fn discover(selection: &Selection) -> Result<Discovery, ConnectionError> {
    selection.validate()?;
    let snapshot = selection
        .options()
        .discover()
        .map_err(|_| ConnectionError::Configuration)?;
    discovery_from_snapshot(selection, snapshot)
}

#[cfg(any(feature = "tui", test))]
fn discovery_from_snapshot(
    selection: &Selection,
    snapshot: bcode::ConfiguredModelDiscovery,
) -> Result<Discovery, ConnectionError> {
    if selection.endpoint.is_some()
        && snapshot.provider.as_deref() != Some("bcode.openai-compatible")
    {
        return Err(ConnectionError::Endpoint);
    }
    if snapshot
        .profiles
        .len()
        .saturating_add(snapshot.contexts.len())
        > MAX_CONNECTIONS
    {
        return Err(ConnectionError::DiscoveryLimit);
    }
    let mut connections = vec![Connection {
        label: "Automatic (Bcode configuration)".into(),
        selection: Selection::default(),
    }];
    for profile in snapshot.profiles {
        connections.push(Connection {
            label: format!("Profile: {profile}"),
            selection: Selection {
                profile: Some(profile),
                model: None,
                provider: None,
                ..selection.clone()
            },
        });
    }
    for context in snapshot.contexts {
        connections.push(Connection {
            label: format!("Context: {context}"),
            selection: Selection {
                context: Some(context),
                profile: None,
                model: None,
                provider: None,
                ..selection.clone()
            },
        });
    }
    append_config_connections(&snapshot.provider_config_directory, &mut connections)?;
    connections[1..].sort_by(|left, right| left.label.cmp(&right.label));
    let mut models: Vec<Model> = snapshot
        .models
        .into_iter()
        .map(|model| Model {
            images: model
                .capabilities
                .contains(&bcode_model::ModelCapability::ImageInput),
            id: model.model_id,
            label: model.display_name,
            provider: snapshot.provider.clone(),
        })
        .collect();
    if let Some(id) = &snapshot.model
        && !models.iter().any(|model| &model.id == id)
    {
        models.push(Model {
            id: id.clone(),
            label: id.clone(),
            provider: snapshot.provider.clone(),
            images: false,
        });
    }
    let selected = Selection {
        profile: snapshot.profile,
        model: snapshot.model,
        provider: snapshot.provider,
        ..selection.clone()
    };
    Ok(Discovery {
        connections,
        models,
        selected,
    })
}

#[cfg(any(feature = "tui", test))]
fn append_config_connections(
    directory: &std::path::Path,
    connections: &mut Vec<Connection>,
) -> Result<(), ConnectionError> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(ConnectionError::Configuration),
    };
    for (index, entry) in entries.enumerate() {
        if index >= MAX_CONNECTIONS {
            return Err(ConnectionError::DiscoveryLimit);
        }
        let entry = entry.map_err(|_| ConnectionError::Configuration)?;
        if entry
            .file_type()
            .map_err(|_| ConnectionError::Configuration)?
            .is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "toml")
        {
            connections.push(Connection {
                label: format!("Config: {}", entry.file_name().to_string_lossy()),
                selection: Selection {
                    config_path: Some(entry.path()),
                    ..Selection::default()
                },
            });
        }
    }
    Ok(())
}

/// Resolve exactly the requested selection through the configured SDK, without inference.
/// # Errors
/// Invalid selection or unavailable selected authentication fails without fallback.
pub async fn resolve(selection: &Selection) -> Result<bcode::Bcode, ConnectionError> {
    selection.validate()?;
    let mut options = selection.options();
    if let Some(endpoint) = &selection.endpoint {
        let snapshot = options
            .discover()
            .map_err(|_| ConnectionError::Configuration)?;
        if snapshot.provider.as_deref() != Some("bcode.openai-compatible") {
            return Err(ConnectionError::Endpoint);
        }
        options
            .settings
            .insert("openai.base_url".into(), normalized_endpoint(endpoint)?);
    }
    let providers = [bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/openai-compatible-provider-plugin/bcode-plugin.toml"),
        bcode_openai_compatible_provider_plugin::static_plugin(),
    )];
    bcode::Bcode::configured_embedded_with_options(&options, &providers)
        .await
        .map_err(|error| {
            ConnectionError::Model(crate::bcode_adapter::ModelFailure::classify(&error))
        })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preferences {
    version: u32,
    selection: Selection,
}

/// Default user-state location. Never a Bcode declarative config file.
/// # Errors
/// Returns a secret-safe error if Bcode's configured state cannot be resolved.
pub fn preferences_path() -> Result<PathBuf, ConnectionError> {
    let root = bcode_config::default_state_dir();
    if !root.is_absolute() {
        return Err(ConnectionError::Preferences);
    }
    Ok(root.join("recap").join("selection.json"))
}

/// Read bounded, versioned preferences; only absence means automatic.
/// # Errors
/// Unreadable, future, damaged or invalid preferences require explicit reset.
pub fn load_preferences(path: &std::path::Path) -> Result<Selection, ConnectionError> {
    use std::io::Read as _;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Selection::default());
        }
        Err(_) => return Err(ConnectionError::Preferences),
    };
    let mut bytes = Vec::new();
    file.take(MAX_PREFERENCES_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ConnectionError::Preferences)?;
    if bytes.len() as u64 > MAX_PREFERENCES_BYTES {
        return Err(ConnectionError::Preferences);
    }
    let prefs: Preferences =
        serde_json::from_slice(&bytes).map_err(|_| ConnectionError::Preferences)?;
    if prefs.version != 1 {
        return Err(ConnectionError::Preferences);
    }
    prefs.selection.validate()?;
    Ok(prefs.selection)
}

/// Atomically save only secret-free selection fields, never credentials/config contents.
/// # Errors
/// Invalid input or an unwritable state location leaves existing preferences intact.
#[cfg(any(feature = "tui", test))]
pub fn save_preferences(
    path: &std::path::Path,
    selection: &Selection,
) -> Result<(), ConnectionError> {
    use std::io::Write as _;
    selection.validate()?;
    let bytes = serde_json::to_vec(&Preferences {
        version: 1,
        selection: selection.clone(),
    })
    .map_err(|_| ConnectionError::Preferences)?;
    if bytes.len() as u64 > MAX_PREFERENCES_BYTES {
        return Err(ConnectionError::Preferences);
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(ConnectionError::Preferences)?;
    std::fs::create_dir_all(parent).map_err(|_| ConnectionError::Preferences)?;
    let mut file =
        tempfile::NamedTempFile::new_in(parent).map_err(|_| ConnectionError::Preferences)?;
    file.write_all(&bytes)
        .map_err(|_| ConnectionError::Preferences)?;
    file.as_file()
        .sync_all()
        .map_err(|_| ConnectionError::Preferences)?;
    bcode_confined_fs::persist(file, path).map_err(|_| ConnectionError::Preferences)?;
    Ok(())
}

/// Clear only Recap's selection preference. Bcode config and credentials are untouched.
/// # Errors
/// Returns an error if the preference cannot be removed.
#[cfg(any(feature = "tui", test))]
pub fn reset_automatic(path: &std::path::Path) -> Result<(), ConnectionError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(ConnectionError::Preferences),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_projects_normalized_models_and_unknown_selected_default() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("connection.toml");
        std::fs::write(
            &path,
            r#"
[model]
provider_plugin_id = "bcode.openai-compatible"
model_id = "unknown-default"
"#,
        )
        .unwrap();
        let selection = Selection {
            config_path: Some(path),
            ..Selection::default()
        };
        let mut snapshot = selection.options().discover().unwrap();
        snapshot.provider_config_directory = directory.path().to_owned();
        let discovery = discovery_from_snapshot(&selection, snapshot).unwrap();
        assert_eq!(discovery.selected.model.as_deref(), Some("unknown-default"));
        assert_eq!(
            discovery.selected.provider.as_deref(),
            Some("bcode.openai-compatible")
        );
        let model = discovery
            .models
            .iter()
            .find(|model| model.id == "unknown-default")
            .unwrap();
        assert_eq!(model.label, "unknown-default");
        assert_eq!(model.provider, discovery.selected.provider);
        assert!(!model.images);
        assert_eq!(
            discovery.connections[0].label,
            "Automatic (Bcode configuration)"
        );
        assert_eq!(discovery.connections[0].selection, Selection::default());

        let mut snapshot = selection.options().discover().unwrap();
        snapshot.provider_config_directory = directory.path().to_owned();
        snapshot.profiles = vec!["primary".into()];
        snapshot.contexts = vec!["work".into()];
        let projected = discovery_from_snapshot(&selection, snapshot).unwrap();
        assert!(
            projected
                .connections
                .iter()
                .any(|connection| connection.label == "Profile: primary")
        );
        assert!(
            projected
                .connections
                .iter()
                .any(|connection| connection.label == "Context: work")
        );
        let invalid = Selection {
            model: Some(String::new()),
            ..Selection::default()
        };
        assert!(discover(&invalid).is_err());
    }

    #[test]
    fn current_preferences_accept_old_missing_auth_and_save_only_reference() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("selection.json");
        std::fs::write(&path, br#"{"version":1,"selection":{"profile":"primary"}}"#).unwrap();
        let mut selected = load_preferences(&path).unwrap();
        assert!(selected.auth_profile.is_none());
        selected.auth_profile = Some("configured-account".into());
        save_preferences(&path, &selected).unwrap();
        assert_eq!(load_preferences(&path).unwrap(), selected);
        let bytes = std::fs::read_to_string(&path).unwrap();
        assert!(bytes.contains("configured-account"));
        assert!(!bytes.contains("credentials"));
        assert!(!bytes.contains("access_token"));
    }

    #[test]
    fn changing_config_discards_stale_saved_account_model_and_endpoint() {
        let saved = Selection {
            config_path: Some("old.toml".into()),
            profile: Some("old".into()),
            model: Some("old-model".into()),
            auth_profile: Some("old-account".into()),
            endpoint: Some("https://old.example/v1".into()),
            ..Selection::default()
        };
        let explicit = Selection {
            config_path: Some("new.toml".into()),
            ..Selection::default()
        };
        assert_eq!(explicit.over(&saved), explicit);
        let same = Selection {
            config_path: saved.config_path.clone(),
            ..Selection::default()
        };
        assert_eq!(same.over(&saved), saved);
    }

    #[test]
    fn explicit_fields_override_saved_without_losing_other_preferences() {
        let saved = Selection {
            profile: Some("sol".into()),
            model: Some("old".into()),
            ..Selection::default()
        };
        let explicit = Selection {
            model: Some("new".into()),
            ..Selection::default()
        };
        let merged = explicit.over(&saved);
        assert_eq!(merged.model.as_deref(), Some("new"));
        assert_eq!(merged.profile.as_deref(), Some("sol"));
    }

    #[test]
    fn effective_selection_loads_saved_below_explicit_and_surfaces_invalid_state() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("selection.json");
        let saved = Selection {
            profile: Some("saved-profile".into()),
            model: Some("old".into()),
            ..Selection::default()
        };
        save_preferences(&path, &saved).unwrap();
        let explicit = Selection {
            model: Some("explicit".into()),
            ..Selection::default()
        };
        let selected = effective_selection_from(&explicit, &path).unwrap();
        assert_eq!(selected.profile, saved.profile);
        assert_eq!(selected.model, explicit.model);
        std::fs::write(&path, br#"{"version":999,"selection":{}}"#).unwrap();
        assert!(effective_selection_from(&explicit, &path).is_err());
    }

    #[test]
    fn versioned_preferences_roundtrip_and_reset_only_recap_state() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("recap/selection.json");
        let config = temp.path().join("provider.toml");
        std::fs::write(&config, "do not modify").unwrap();
        assert_eq!(load_preferences(&path).unwrap(), Selection::default());
        let selection = Selection {
            config_path: Some(config.clone()),
            profile: Some("sol".into()),
            endpoint: Some("https://example.test/v1".into()),
            ..Selection::default()
        };
        save_preferences(&path, &selection).unwrap();
        assert_eq!(load_preferences(&path).unwrap(), selection);
        reset_automatic(&path).unwrap();
        reset_automatic(&path).unwrap();
        assert_eq!(std::fs::read_to_string(config).unwrap(), "do not modify");
        assert_eq!(load_preferences(&path).unwrap(), Selection::default());
    }

    #[test]
    fn future_corrupt_unknown_and_oversized_preferences_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("selection.json");
        for bytes in [
            br#"{"version":2,"selection":{}}"#.as_slice(),
            br#"{"version":1,"selection":{"api_key":"secret"}}"#.as_slice(),
            b"not json".as_slice(),
            vec![b' '; usize::try_from(MAX_PREFERENCES_BYTES).unwrap() + 1].as_slice(),
        ] {
            std::fs::write(&path, bytes).unwrap();
            assert!(load_preferences(&path).is_err());
        }
    }

    #[test]
    fn endpoint_validation_rejects_credentials_remote_config_and_opaque_urls() {
        for endpoint in [
            "https://user:secret@example.test/v1",
            "https://example.test/v1?api_key=secret",
            "https://example.test/#secret",
            "file:///tmp/config.toml",
            "javascript:alert(1)",
            "ftp://example.test",
        ] {
            assert!(normalized_endpoint(endpoint).is_err());
        }
        assert_eq!(
            normalized_endpoint("HTTPS://EXAMPLE.TEST:443/v1").unwrap(),
            "https://example.test/v1"
        );
        assert!(normalized_endpoint("http://localhost:1234/v1").is_ok());
        let selection = Selection {
            config_path: Some("https://example.test/config.toml".into()),
            ..Selection::default()
        };
        assert!(selection.validate().is_err());
    }

    #[test]
    fn central_sdk_discovers_profiles_and_resolves_aliases_without_provider_calls() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("named-not-a-model.toml");
        std::fs::write(
            &path,
            r#"
[model]
profile = "primary"
[model.profiles.primary]
provider_plugin_id = "bcode.openai-compatible"
model_id = "configured-model"
[model.profiles.secondary]
provider_plugin_id = "bcode.openai-compatible"
model_id = "second-model"
[model.aliases.short]
provider_plugin_id = "bcode.openai-compatible"
model_id = "aliased-model"
"#,
        )
        .unwrap();
        let mut options = bcode::ConfiguredModelOptions {
            config_path: Some(path),
            ..Default::default()
        };
        let snapshot = options.discover().unwrap();
        assert_eq!(snapshot.model.as_deref(), Some("configured-model"));
        assert_eq!(snapshot.profile.as_deref(), Some("primary"));
        assert!(snapshot.profiles.contains(&"secondary".to_string()));
        assert!(
            snapshot
                .models
                .iter()
                .any(|model| model.model_id == "second-model")
        );
        options.model = Some("short".into());
        assert_eq!(
            options.discover().unwrap().model.as_deref(),
            Some("aliased-model")
        );
        options.profile = Some("missing".into());
        assert!(options.discover().is_err());
        options.profile = None;
        options.context = Some("missing".into());
        assert!(options.discover().is_err());
    }

    #[test]
    fn selected_auth_metadata_discovers_full_catalog_without_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("connection.toml");
        std::fs::write(
            &path,
            r#"
[model]
profile = "primary"
[model.profiles.primary]
provider_plugin_id = "bcode.openai-compatible"
model_id = "custom-model-not-in-catalog"
auth_profile = "account"
[auth.profiles.account]
backend = "env"
provider_id = "openai"
owner_plugin_id = "bcode.openai-compatible"
scheme = "api_key"
"#,
        )
        .unwrap();
        let snapshot = bcode::ConfiguredModelOptions {
            config_path: Some(path),
            ..Default::default()
        }
        .discover()
        .unwrap();
        let catalog = bcode_model_catalog::ModelCatalog::load_bundled().unwrap();
        let expected = catalog.provider_models_as_model_info("openai");
        assert!(!expected.is_empty());
        for model in expected {
            let discovered = snapshot
                .models
                .iter()
                .find(|item| item.model_id == model.model_id)
                .unwrap();
            assert_eq!(discovered.capabilities, model.capabilities);
        }
        assert_eq!(
            snapshot.model.as_deref(),
            Some("custom-model-not-in-catalog")
        );
        assert!(
            snapshot
                .models
                .iter()
                .any(|model| model.model_id == "custom-model-not-in-catalog")
        );
    }

    #[test]
    fn config_directory_discovery_is_bounded_and_does_not_read_contents() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("not-a-model.toml");
        std::fs::write(&path, "invalid config containing credential-like content").unwrap();
        std::fs::write(temp.path().join("ignore.txt"), "ignored").unwrap();
        let mut connections = Vec::new();
        append_config_connections(temp.path(), &mut connections).unwrap();
        assert_eq!(connections.len(), 1);
        assert_eq!(connections[0].selection.config_path.as_ref(), Some(&path));
        assert_eq!(connections[0].selection.model, None);
        assert!(!connections[0].label.contains("credential"));
        for index in 0..MAX_CONNECTIONS {
            std::fs::write(temp.path().join(format!("extra-{index}.txt")), "").unwrap();
        }
        assert!(matches!(
            append_config_connections(temp.path(), &mut Vec::new()),
            Err(ConnectionError::DiscoveryLimit)
        ));
    }

    #[test]
    fn explicit_missing_configuration_never_falls_back() {
        let temp = tempfile::tempdir().unwrap();
        let options = bcode::ConfiguredModelOptions {
            config_path: Some(temp.path().join("missing.toml")),
            ..Default::default()
        };
        assert!(options.discover().is_err());
    }

    #[test]
    fn invalid_save_does_not_replace_existing_selection() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("selection.json");
        save_preferences(&path, &Selection::default()).unwrap();
        let selection = Selection {
            endpoint: Some("https://user:password@example.test/".into()),
            ..Selection::default()
        };
        assert!(save_preferences(&path, &selection).is_err());
        assert_eq!(load_preferences(&path).unwrap(), Selection::default());
    }
}
