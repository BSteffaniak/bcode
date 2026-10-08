//! Versioned, non-secret remembered startup choices, separate from declarative configuration.

use crate::{BcodeConfig, ConfigEnvironment, ProcessConfigEnvironment};
use serde::{Deserialize, Serialize};
use std::io::Read as _;
use std::path::{Path, PathBuf};

const VERSION: u32 = 1;
const MAX_BYTES: u64 = 16 * 1024;

/// Normalized references only. Credential values must never be stored here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupSelection {
    /// Exact provider plugin identity, resolved by the provider/catalog owner.
    pub provider_plugin_id: String,
    /// Exact provider-specific model identity, not a profile alias.
    pub model_id: String,
    /// Provider endpoint identity for compatible services; never credentials or URL userinfo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Provider-owned protocol dialect reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialect: Option<String>,
    /// Provider-owned authentication mode reference (for example a subscription mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<String>,
    /// Provider-owned catalog identity for compatible services sharing one plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_provider_id: Option<String>,
    /// Optional named authentication profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_profile: Option<String>,
    /// Optional environment source name, never its value. Authentication remains provider-owned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_variable: Option<String>,
    /// Declared authentication provider owning the selected environment source.
    /// Must be present together with variable, method, and credential references.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_auth_provider: Option<String>,
    /// Provider-declared authentication method identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_method: Option<String>,
    /// Provider-declared credential identity; never its value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_credential: Option<String>,
    /// Context identity. An unscoped choice applies only outside a named context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    selection: Option<StartupSelection>,
}

/// Recoverable startup-state failures. Diagnostics never include file contents.
#[derive(Debug, thiserror::Error)]
pub enum StartupStateError {
    /// The selected state path could not be accessed.
    #[error("startup state I/O failure at {path}: {source}")]
    Io {
        /// Selected path; no alternative location is tried.
        path: PathBuf,
        /// Underlying filesystem error.
        source: std::io::Error,
    },
    /// Malformed TOML, unknown fields, or invalid references. Existing state is preserved.
    #[error("invalid startup state; review startup.toml explicitly")]
    InvalidState,
    /// A version not understood by this artifact. Existing state is preserved.
    #[error("unsupported startup state version {version}")]
    UnsupportedVersion {
        /// Unsupported schema version.
        version: i64,
    },
    /// Bounded reads refuse oversized input.
    #[error("startup state exceeds the 16 KiB limit")]
    TooLarge,
}

/// Resolve startup state in the selected runtime state root without creating files.
#[must_use]
pub fn path() -> PathBuf {
    path_with_environment(&ProcessConfigEnvironment)
}

/// Resolve startup state with an explicit environment, honoring process state selection.
#[must_use]
pub fn path_with_environment(environment: &impl ConfigEnvironment) -> PathBuf {
    crate::default_state_dir_with_environment(environment).join("startup.toml")
}

/// Read the remembered choice without mutation.
///
/// # Errors
/// Returns typed I/O, size, schema, or unsupported-version failures.
pub fn load() -> Result<Option<StartupSelection>, StartupStateError> {
    load_from(&path())
}

/// Read a specific path. Missing state is not an error and creates nothing.
///
/// # Errors
/// Returns typed I/O, size, schema, or unsupported-version failures.
pub fn load_from(path: &Path) -> Result<Option<StartupSelection>, StartupStateError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(StartupStateError::InvalidState);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io(path, source)),
    }
    let file = std::fs::File::open(path).map_err(|source| io(path, source))?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| io(path, source))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(StartupStateError::TooLarge);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| StartupStateError::InvalidState)?;
    let raw: toml::Value = toml::from_str(text).map_err(|_| StartupStateError::InvalidState)?;
    let version = raw
        .get("version")
        .and_then(toml::Value::as_integer)
        .ok_or(StartupStateError::InvalidState)?;
    if version != i64::from(VERSION) {
        return Err(StartupStateError::UnsupportedVersion { version });
    }
    let state: State = raw
        .try_into()
        .map_err(|_| StartupStateError::InvalidState)?;
    if let Some(selection) = &state.selection {
        validate(selection)?;
    }
    Ok(state.selection)
}

fn validate(selection: &StartupSelection) -> Result<(), StartupStateError> {
    let reference = |value: &str| {
        !value.is_empty()
            && value.len() <= 1024
            && value.trim() == value
            && !value.chars().any(char::is_control)
    };
    let source = [
        selection.environment_variable.as_deref(),
        selection.environment_auth_provider.as_deref(),
        selection.environment_method.as_deref(),
        selection.environment_credential.as_deref(),
    ];
    let source_complete = source.iter().all(Option::is_some);
    let source_absent = source.iter().all(Option::is_none);
    if (!source_complete && !source_absent)
        || source.iter().flatten().any(|value| !reference(value))
        || selection
            .auth_mode
            .as_deref()
            .is_some_and(|value| !reference(value))
        || selection
            .catalog_provider_id
            .as_deref()
            .is_some_and(|value| !reference(value))
        || !reference(&selection.provider_plugin_id)
        || !reference(&selection.model_id)
        || selection
            .auth_profile
            .as_deref()
            .is_some_and(|value| !reference(value))
        || selection
            .context
            .as_deref()
            .is_some_and(|value| !reference(value))
        || selection.base_url.as_deref().is_some_and(|value| {
            !reference(value)
                || !(value.starts_with("https://") || value.starts_with("http://"))
                || value.contains('@')
                || value.contains('?')
                || value.contains('#')
        })
        || selection
            .dialect
            .as_deref()
            .is_some_and(|value| !reference(value))
        || selection
            .environment_variable
            .as_deref()
            .is_some_and(|value| {
                value.is_empty()
                    || !value.bytes().enumerate().all(|(index, byte)| {
                        byte == b'_'
                            || byte.is_ascii_alphabetic()
                            || (index > 0 && byte.is_ascii_digit())
                    })
            })
    {
        return Err(StartupStateError::InvalidState);
    }
    Ok(())
}

/// Atomically save a completed choice under a cross-process lock.
///
/// # Errors
/// Invalid selections or existing state, unsupported versions, and I/O errors are preserved/reported.
pub fn save(selection: &StartupSelection) -> Result<PathBuf, StartupStateError> {
    save_to(&path(), selection)
}

/// Atomically save at a specific state path; never overwrites unrecognized state.
///
/// # Errors
/// Returns validation, existing-state, serialization, or filesystem failures.
pub fn save_to(path: &Path, selection: &StartupSelection) -> Result<PathBuf, StartupStateError> {
    validate(selection)?;
    write(path, Some(selection.clone()))
}

/// Clear the remembered choice, retaining a versioned empty state.
///
/// # Errors
/// Returns existing-state or filesystem failures. Invalid state requires explicit user repair.
pub fn clear() -> Result<PathBuf, StartupStateError> {
    clear_from(&path())
}

/// Clear a specific path under the same lock as save.
///
/// # Errors
/// Returns existing-state or filesystem failures; unknown representations are never replaced.
pub fn clear_from(path: &Path) -> Result<PathBuf, StartupStateError> {
    write(path, None)
}

fn write(path: &Path, selection: Option<StartupSelection>) -> Result<PathBuf, StartupStateError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(StartupStateError::InvalidState)?;
    std::fs::create_dir_all(parent).map_err(|source| io(parent, source))?;
    let lock_path = path.with_extension("toml.lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| io(&lock_path, source))?;
    lock.lock().map_err(|source| io(&lock_path, source))?;
    load_from(path)?;
    let text = toml::to_string(&State {
        version: VERSION,
        selection,
    })
    .map_err(|_| StartupStateError::InvalidState)?;
    if text.len() as u64 > MAX_BYTES {
        return Err(StartupStateError::TooLarge);
    }
    crate::edit::atomic_replace(path, text.as_bytes()).map_err(|source| io(path, source))?;
    Ok(path.to_path_buf())
}

fn io(path: &Path, source: std::io::Error) -> StartupStateError {
    StartupStateError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Apply an atomic provider/model fallback only when no effective selection exists.
///
/// Context identities must match exactly. Auth mappings and environment source names are
/// not interpreted here: provider-owned authentication must validate their availability.
/// A remembered source must never authorize switching to another provider.
#[must_use]
pub fn apply_fallback(
    config: &mut BcodeConfig,
    selection: &StartupSelection,
    environment: &impl ConfigEnvironment,
) -> bool {
    if selection.context != config.active_context || validate(selection).is_err() {
        return false;
    }
    let effective = config.resolved_model_selection_with_environment(environment);
    if effective.provider_plugin_id.is_some()
        || effective.model_id.is_some()
        || effective.model_profile.is_some()
        || effective.auth_profile.is_some()
        || config.model.auth_profile.is_some()
        || config.model.auth_pool.is_some()
        || crate::model_id_and_source_from_config_environment(
            environment,
            &selection.provider_plugin_id,
        )
        .is_some()
        || [
            crate::BCODE_MODEL_PROFILE_ENV,
            "BCODE_MODEL_PROVIDER",
            "BCODE_PROVIDER",
            "BCODE_AUTH_PROFILE",
        ]
        .iter()
        .any(|name| {
            environment
                .var(name)
                .is_some_and(|value| !value.trim().is_empty())
        })
        || !config.model.settings.is_empty()
    {
        return false;
    }
    config.model.provider_plugin_id = Some(selection.provider_plugin_id.clone());
    config.model.model_id = Some(selection.model_id.clone());
    config
        .model
        .auth_profile
        .clone_from(&selection.auth_profile);
    if let Some(base_url) = &selection.base_url {
        config
            .model
            .settings
            .insert("base_url".to_owned(), base_url.clone());
    }
    if let Some(dialect) = &selection.dialect {
        config
            .model
            .settings
            .insert("dialect".to_owned(), dialect.clone());
    }
    for (key, value) in [
        ("auth_mode", &selection.auth_mode),
        ("catalog_provider_id", &selection.catalog_provider_id),
    ] {
        if let Some(value) = value {
            config.model.settings.insert(key.to_owned(), value.clone());
        }
    }
    config.startup_selection_applied = true;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConfigEnvironmentSnapshot, ConfigLoadOverrides};
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    fn selection() -> StartupSelection {
        StartupSelection {
            provider_plugin_id: "example.compatible".to_owned(),
            model_id: "example/model".to_owned(),
            base_url: Some("https://models.example/v1".to_owned()),
            dialect: Some("responses_api".to_owned()),
            auth_mode: Some("api_key".to_owned()),
            catalog_provider_id: Some("example.catalog".to_owned()),
            auth_profile: Some("chosen-account".to_owned()),
            environment_variable: Some("EXAMPLE_API_KEY".to_owned()),
            environment_auth_provider: Some("example.auth".to_owned()),
            environment_method: Some("api_key".to_owned()),
            environment_credential: Some("api_key".to_owned()),
            context: None,
        }
    }

    fn environment(root: &Path) -> ConfigEnvironmentSnapshot {
        ConfigEnvironmentSnapshot::new(
            BTreeMap::from([("BCODE_STATE_DIR".to_owned(), OsString::from(root))]),
            root.to_path_buf(),
        )
    }

    #[test]
    fn roundtrip_and_clear_preserve_declarative_config() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("startup.toml");
        let declarative = temp.path().join("bcode.toml");
        std::fs::write(&declarative, "# managed\n").unwrap();
        save_to(&path, &selection()).unwrap();
        assert_eq!(load_from(&path).unwrap(), Some(selection()));
        clear_from(&path).unwrap();
        assert_eq!(load_from(&path).unwrap(), None);
        assert_eq!(std::fs::read_to_string(declarative).unwrap(), "# managed\n");
    }

    #[test]
    fn missing_reads_do_not_create_directories_or_lock_files() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("absent");
        assert_eq!(load_from(&parent.join("startup.toml")).unwrap(), None);
        assert!(!parent.exists());
    }

    #[test]
    fn invalid_and_unknown_state_is_preserved_by_all_ordinary_operations() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("startup.toml");
        for raw in [
            "not TOML",
            "version = 2\n",
            "version = 1\nunknown = true\n",
            "version = 1\n[selection]\nprovider_plugin_id = 'example'\nmodel_id = ''\n",
        ] {
            std::fs::write(&path, raw).unwrap();
            assert!(load_from(&path).is_err());
            assert!(save_to(&path, &selection()).is_err());
            assert!(clear_from(&path).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        }
        assert!(matches!(
            load_from(&path),
            Err(StartupStateError::InvalidState)
        ));
        std::fs::write(&path, "version = 42\n").unwrap();
        assert!(matches!(
            load_from(&path),
            Err(StartupStateError::UnsupportedVersion { version: 42 })
        ));
        std::fs::write(&path, vec![b' '; usize::try_from(MAX_BYTES).unwrap() + 1]).unwrap();
        assert!(matches!(load_from(&path), Err(StartupStateError::TooLarge)));
    }

    #[test]
    fn unsafe_endpoint_and_source_references_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("startup.toml");
        for endpoint in [
            "https://user:password@example/v1",
            "https://example/v1?key=secret",
            "https://example/#secret",
        ] {
            let mut choice = selection();
            choice.base_url = Some(endpoint.to_owned());
            assert!(save_to(&path, &choice).is_err());
        }
        let mut choice = selection();
        choice.environment_variable = Some("not a source name".to_owned());
        assert!(save_to(&path, &choice).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn environment_references_are_all_or_none_and_never_materialize_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("startup.toml");
        for missing in 0..4 {
            let mut choice = selection();
            match missing {
                0 => choice.environment_variable = None,
                1 => choice.environment_auth_provider = None,
                2 => choice.environment_method = None,
                _ => choice.environment_credential = None,
            }
            assert!(matches!(
                save_to(&path, &choice),
                Err(StartupStateError::InvalidState)
            ));
        }
        let mut choice = selection();
        choice.environment_variable = None;
        choice.environment_auth_provider = None;
        choice.environment_method = None;
        choice.environment_credential = None;
        save_to(&path, &choice).unwrap();
        assert_eq!(load_from(&path).unwrap(), Some(choice));
        std::fs::write(&path, "version = 1\n[selection]\nprovider_plugin_id = 'example.compatible'\nmodel_id = 'model'\nenvironment_variable = 'EXAMPLE_API_KEY'\n").unwrap();
        let original = std::fs::read(&path).unwrap();
        assert!(matches!(
            load_from(&path),
            Err(StartupStateError::InvalidState)
        ));
        assert!(save_to(&path, &selection()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn fallback_keeps_endpoint_auth_and_model_together_and_respects_context() {
        let temp = tempfile::tempdir().unwrap();
        let env = environment(temp.path());
        let mut config = BcodeConfig::default();
        assert!(apply_fallback(&mut config, &selection(), &env));
        let resolved = config.resolved_model_selection_with_environment(&env);
        assert_eq!(
            resolved.provider_plugin_id.as_deref(),
            Some("example.compatible")
        );
        assert_eq!(resolved.model_id.as_deref(), Some("example/model"));
        assert_eq!(resolved.auth_profile.as_deref(), Some("chosen-account"));
        assert_eq!(
            resolved.settings.get("base_url"),
            selection().base_url.as_ref()
        );
        assert_eq!(
            resolved.settings.get("auth_mode"),
            selection().auth_mode.as_ref()
        );
        assert_eq!(
            resolved.settings.get("catalog_provider_id"),
            selection().catalog_provider_id.as_ref()
        );
        assert!(config.auth.profiles.is_empty());
        let mut scoped = BcodeConfig {
            active_context: Some("other".to_owned()),
            ..BcodeConfig::default()
        };
        assert!(!apply_fallback(&mut scoped, &selection(), &env));
        let mut choice = selection();
        choice.context = Some("other".to_owned());
        assert!(apply_fallback(&mut scoped, &choice, &env));
    }

    #[test]
    fn explicit_selections_and_settings_never_mix_with_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let env = environment(temp.path());
        for field in [
            "provider_plugin_id",
            "model_id",
            "profile",
            "auth_profile",
            "auth_pool",
        ] {
            let mut raw = toml::Table::new();
            raw.insert(field.to_owned(), toml::Value::String("explicit".to_owned()));
            let model = toml::Value::Table(raw).try_into().unwrap();
            let mut config = BcodeConfig {
                model,
                ..BcodeConfig::default()
            };
            assert!(!apply_fallback(&mut config, &selection(), &env), "{field}");
        }
        let mut config = BcodeConfig::default();
        config
            .model
            .settings
            .insert("base_url".to_owned(), "https://explicit.example".to_owned());
        assert!(!apply_fallback(&mut config, &selection(), &env));
        for variable in [
            "BCODE_MODEL_PROVIDER",
            "BCODE_AUTH_PROFILE",
            "BCODE_MODEL_PROFILE",
        ] {
            let env = ConfigEnvironmentSnapshot::new(
                BTreeMap::from([(variable.to_owned(), OsString::from("explicit"))]),
                temp.path().to_path_buf(),
            );
            assert!(!apply_fallback(
                &mut BcodeConfig::default(),
                &selection(),
                &env
            ));
        }
    }

    #[test]
    fn fallback_provenance_is_local_and_explicit_matching_selection_does_not_gain_it() {
        let temp = tempfile::tempdir().unwrap();
        let env = environment(temp.path());
        let choice = selection();
        let mut remembered = BcodeConfig::default();
        assert!(!remembered.startup_selection_applied);
        assert!(apply_fallback(&mut remembered, &choice, &env));
        assert!(remembered.startup_selection_applied);
        assert!(remembered.clone().startup_selection_applied);
        let encoded = crate::encode_effective_config(&remembered).unwrap();
        let mut decoded = crate::decode_effective_config(&encoded).unwrap();
        assert!(!decoded.startup_selection_applied);
        assert_eq!(decoded.model, remembered.model);
        assert!(!apply_fallback(&mut decoded, &choice, &env));
        assert!(!decoded.startup_selection_applied);
        assert!(
            toml::Value::try_from(&remembered)
                .unwrap()
                .get("startup_selection_applied")
                .is_none()
        );
        let explicit = ConfigLoadOverrides {
            cli_config_toml: Some(encoded),
            ..ConfigLoadOverrides::default()
        };
        save_to(&path_with_environment(&env), &choice).unwrap();
        let transported =
            crate::load_config_from_paths_with_overrides_and_environment(&[], &explicit, &env)
                .unwrap();
        assert_eq!(transported.model, remembered.model);
        assert!(!transported.startup_selection_applied);
    }

    #[test]
    fn normal_loading_uses_fallback_but_optional_damage_does_not_block_core() {
        let temp = tempfile::tempdir().unwrap();
        let env = environment(temp.path());
        let state = path_with_environment(&env);
        save_to(&state, &selection()).unwrap();
        let config = crate::load_config_from_paths_with_overrides_and_environment(
            &[],
            &ConfigLoadOverrides::default(),
            &env,
        )
        .unwrap();
        assert_eq!(config.model.model_id.as_deref(), Some("example/model"));
        let declarative = temp.path().join("bcode.toml");
        std::fs::write(
            &declarative,
            "[model]\nprovider_plugin_id = 'declared.provider'\nmodel_id = 'declared-model'\n",
        )
        .unwrap();
        let declared = crate::load_config_from_paths_with_overrides_and_environment(
            &[declarative],
            &ConfigLoadOverrides::default(),
            &env,
        )
        .unwrap();
        assert_eq!(declared.model.model_id.as_deref(), Some("declared-model"));
        assert!(declared.model.auth_profile.is_none());
        let explicit = ConfigLoadOverrides {
            cli_config_toml: Some(
                "[model]\nprovider_plugin_id = 'explicit.provider'\nmodel_id = 'explicit-model'\n"
                    .to_owned(),
            ),
            ..ConfigLoadOverrides::default()
        };
        let config =
            crate::load_config_from_paths_with_overrides_and_environment(&[], &explicit, &env)
                .unwrap();
        assert_eq!(config.model.model_id.as_deref(), Some("explicit-model"));
        assert!(config.model.auth_profile.is_none());
        std::fs::write(&state, "version = 99\n").unwrap();
        assert!(
            crate::load_config_from_paths_with_overrides_and_environment(
                &[],
                &ConfigLoadOverrides::default(),
                &env
            )
            .is_ok()
        );
        assert_eq!(std::fs::read_to_string(&state).unwrap(), "version = 99\n");
    }

    #[test]
    fn independent_roots_do_not_share_state() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        save_to(
            &path_with_environment(&environment(first.path())),
            &selection(),
        )
        .unwrap();
        assert_eq!(
            load_from(&path_with_environment(&environment(second.path()))).unwrap(),
            None
        );
    }

    // This same test binary is used as a real separate-process writer by the parent test.
    #[test]
    fn subprocess_writer() {
        let Some(path) = std::env::var_os("BCODE_STARTUP_TEST_PATH") else {
            return;
        };
        let path = PathBuf::from(path);
        for _ in 0..20 {
            save_to(&path, &selection()).unwrap();
            clear_from(&path).unwrap();
        }
    }

    #[test]
    fn cross_process_writers_and_nonlocking_readers_observe_only_complete_states() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("startup.toml");
        let executable = std::env::current_exe().unwrap();
        let mut children = (0..3)
            .map(|_| {
                std::process::Command::new(&executable)
                    .args(["--exact", "startup::tests::subprocess_writer"])
                    .env("BCODE_STARTUP_TEST_PATH", &path)
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        while children
            .iter_mut()
            .any(|child| child.try_wait().unwrap().is_none())
        {
            let current = load_from(&path).unwrap();
            assert!(current.is_none() || current == Some(selection()));
            std::thread::yield_now();
        }
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        assert!(load_from(&path).is_ok());
    }
}
