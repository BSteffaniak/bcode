//! Secret-free references to verified, bounded declarative configuration replay.
//!
//! Callers supply provenance retained when loading, never reconstructed from the
//! current process environment. References contain paths and a digest, not TOML,
//! environment values, provider request settings, or materialized credentials.

use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::{BcodeConfig, ConfigLoadOverrides};

/// Compatibility boundary of configuration recovery references.
pub const CONFIG_RECOVERY_VERSION: u32 = 1;
const MAX_LAYERS: usize = 64;
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Complete load-time source provenance, supplied by the initiating client.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigRecoverySources {
    /// Original absolute working directory, including relative-path interpretation.
    pub cwd: PathBuf,
    /// Discovered layers in application order, including absent optional layers.
    pub discovered_paths: Vec<PathBuf>,
    /// Exclusive file; when present all other file layers are bypassed.
    pub exclusive_config_path: Option<PathBuf>,
    /// Explicit layer below discovered files.
    pub base_config_path: Option<PathBuf>,
    /// Explicit environment file overlay (path only, never environment contents).
    pub env_config_path: Option<PathBuf>,
    /// Explicit command-line file overlay.
    pub cli_config_path: Option<PathBuf>,
    /// Selected active context, if explicitly applied at load time.
    pub active_context: Option<String>,
    /// Selected named model profile, if explicitly applied at load time.
    pub model_profile: Option<String>,
    /// Any other inline, request-only, or dynamic configuration was applied.
    pub inline_overrides: bool,
}

/// Durable secret-free configuration reference. Session models should own their
/// portable representation and convert these fields at the application boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigRecoveryReference {
    /// Supported format version; future versions are rejected before file access.
    pub schema_version: u32,
    /// Exact load-time provenance, not daemon startup defaults.
    pub sources: ConfigRecoverySources,
    /// SHA-256 of execution-affecting typed configuration.
    pub execution_digest: String,
    /// Presence of each applied file layer, retaining optional-layer absence.
    pub source_presence: Vec<bool>,
}

/// Public failures deliberately omit parser excerpts, paths and secret values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ConfigRecoveryError {
    /// Unknown compatibility boundary.
    #[error("unsupported configuration recovery version")]
    UnsupportedVersion,
    /// No faithful durable source exists.
    #[error(
        "configuration recovery unavailable: inline or dynamic configuration has no durable source"
    )]
    Unavailable,
    /// Path is not inside an explicitly authorized root or work exceeds bounds.
    #[error("configuration recovery source is outside authorized bounds")]
    SourceBounds,
    /// Previously present file is absent, unreadable, or invalid.
    #[error("configuration recovery source is missing, unreadable, or invalid")]
    InvalidSource,
    /// Supplied sources do not reproduce effective config or have changed.
    #[error("configuration recovery source does not match the captured effective configuration")]
    Changed,
}

/// Fingerprint normalized execution policy, excluding presentation and transport.
///
/// Provider/plugin settings may
/// themselves contain sensitive values: only this opaque verification digest is
/// retained, never the serialized input. It is not a general redaction API.
/// Materialized/refreshed credentials are outside `BcodeConfig`.
///
/// # Errors
/// Returns `InvalidSource` if the configuration cannot be serialized.
pub fn execution_fingerprint(config: &BcodeConfig) -> Result<String, ConfigRecoveryError> {
    let mut value = serde_json::to_value(config).map_err(|_| ConfigRecoveryError::InvalidSource)?;
    let table = value
        .as_object_mut()
        .ok_or(ConfigRecoveryError::InvalidSource)?;
    for field in [
        "tui",
        "client",
        "daemon",
        "onboarding",
        "observability",
        "metrics",
    ] {
        table.remove(field);
    }
    let bytes = serde_json::to_vec(&value).map_err(|_| ConfigRecoveryError::InvalidSource)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

/// Capture only after exact source replay matches the actual effective config.
/// `allowed_roots` is caller authority, not supplied by a persisted reference.
///
/// # Errors
/// Rejects unsupported dynamic inputs, unconfined/oversized sources, invalid
/// files, or source provenance that does not reproduce the supplied config.
pub fn capture_with_sources(
    effective: &BcodeConfig,
    sources: &ConfigRecoverySources,
    allowed_roots: &[PathBuf],
) -> Result<ConfigRecoveryReference, ConfigRecoveryError> {
    let (replayed, source_presence) = replay(sources, allowed_roots)?;
    let execution_digest = execution_fingerprint(effective)?;
    if execution_digest != execution_fingerprint(&replayed)? {
        return Err(ConfigRecoveryError::Changed);
    }
    Ok(ConfigRecoveryReference {
        schema_version: CONFIG_RECOVERY_VERSION,
        sources: sources.clone(),
        execution_digest,
        source_presence,
    })
}

/// Replay only captured sources, verifying file presence and effective digest.
/// Never discovers new default paths, reads ambient env, or loads startup state.
///
/// # Errors
/// Rejects future versions, unavailable or invalid sources, confinement/budget
/// failures, changed layer presence, and changed execution configuration.
pub fn resolve_with_roots(
    reference: &ConfigRecoveryReference,
    allowed_roots: &[PathBuf],
) -> Result<BcodeConfig, ConfigRecoveryError> {
    if reference.schema_version != CONFIG_RECOVERY_VERSION {
        return Err(ConfigRecoveryError::UnsupportedVersion);
    }
    let (config, presence) = replay(&reference.sources, allowed_roots)?;
    if presence != reference.source_presence
        || execution_fingerprint(&config)? != reference.execution_digest
    {
        return Err(ConfigRecoveryError::Changed);
    }
    Ok(config)
}

fn confined(path: &Path, cwd: &Path, roots: &[PathBuf]) -> Result<PathBuf, ConfigRecoveryError> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    // For absent optional layers, canonicalize the nearest existing ancestor.
    // This also rejects traversal and symlink escapes, including missing tails.
    if path
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(ConfigRecoveryError::SourceBounds);
    }
    let mut ancestor = path.as_path();
    while !ancestor.exists() {
        ancestor = ancestor.parent().ok_or(ConfigRecoveryError::SourceBounds)?;
    }
    let canonical = ancestor
        .canonicalize()
        .map_err(|_| ConfigRecoveryError::InvalidSource)?;
    if !roots.iter().any(|root| canonical.starts_with(root)) {
        return Err(ConfigRecoveryError::SourceBounds);
    }
    if ancestor == path {
        Ok(canonical)
    } else {
        Ok(path)
    }
}

fn replay(
    sources: &ConfigRecoverySources,
    allowed_roots: &[PathBuf],
) -> Result<(BcodeConfig, Vec<bool>), ConfigRecoveryError> {
    if sources.inline_overrides || !sources.cwd.is_absolute() {
        return Err(ConfigRecoveryError::Unavailable);
    }
    if sources.discovered_paths.len() > MAX_LAYERS || allowed_roots.len() > MAX_LAYERS {
        return Err(ConfigRecoveryError::SourceBounds);
    }
    let roots = allowed_roots
        .iter()
        .map(|root| {
            if !root.is_absolute() {
                return Err(ConfigRecoveryError::SourceBounds);
            }
            root.canonicalize()
                .map_err(|_| ConfigRecoveryError::SourceBounds)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let paths = sources.exclusive_config_path.as_ref().map_or_else(
        || {
            sources
                .base_config_path
                .iter()
                .map(|path| (path, true))
                .chain(sources.discovered_paths.iter().map(|path| (path, false)))
                .chain(sources.env_config_path.iter().map(|path| (path, true)))
                .chain(sources.cli_config_path.iter().map(|path| (path, true)))
                .collect::<Vec<_>>()
        },
        |exclusive| vec![(exclusive, true)],
    );
    if paths.is_empty() || paths.len() > MAX_LAYERS {
        return Err(ConfigRecoveryError::Unavailable);
    }
    if sources.cwd.as_os_str().len() > 4096
        || paths.iter().any(|(path, _)| path.as_os_str().len() > 4096)
        || [&sources.active_context, &sources.model_profile]
            .into_iter()
            .flatten()
            .any(|selected| selected.len() > 256)
    {
        return Err(ConfigRecoveryError::SourceBounds);
    }
    let mut merged = toml::Value::Table(toml::Table::new());
    let mut presence = Vec::with_capacity(paths.len());
    let mut remaining = MAX_BYTES;
    for (path, required) in paths {
        let path = confined(path, &sources.cwd, &roots)?;
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => {
                presence.push(false);
                continue;
            }
            Err(_) => return Err(ConfigRecoveryError::InvalidSource),
        };
        let metadata = file
            .metadata()
            .map_err(|_| ConfigRecoveryError::InvalidSource)?;
        if !metadata.is_file() || metadata.len() > remaining {
            return Err(ConfigRecoveryError::SourceBounds);
        }
        let mut raw = String::new();
        (&mut file)
            .take(remaining + 1)
            .read_to_string(&mut raw)
            .map_err(|_| ConfigRecoveryError::InvalidSource)?;
        let length = u64::try_from(raw.len()).map_err(|_| ConfigRecoveryError::SourceBounds)?;
        remaining = remaining
            .checked_sub(length)
            .ok_or(ConfigRecoveryError::SourceBounds)?;
        let layer = toml::from_str(&raw).map_err(|_| ConfigRecoveryError::InvalidSource)?;
        crate::merge_toml_value(&mut merged, layer);
        presence.push(true);
    }
    if !presence.iter().any(|present| *present) {
        return Err(ConfigRecoveryError::Unavailable);
    }
    let config = resolve_replayed_config(merged, sources)?;
    Ok((config, presence))
}

fn resolve_replayed_config(
    mut merged: toml::Value,
    sources: &ConfigRecoverySources,
) -> Result<BcodeConfig, ConfigRecoveryError> {
    let mut patch = toml::Table::new();
    if let Some(context) = &sources.active_context {
        patch.insert(
            "contexts".to_owned(),
            toml::Value::Table(toml::Table::from_iter([(
                "active".to_owned(),
                toml::Value::String(context.clone()),
            )])),
        );
    }
    if let Some(profile) = &sources.model_profile {
        patch.insert(
            "model".to_owned(),
            toml::Value::Table(toml::Table::from_iter([(
                "profile".to_owned(),
                toml::Value::String(profile.clone()),
            )])),
        );
    }
    let overrides = ConfigLoadOverrides {
        cli_config_toml: Some(
            toml::to_string(&patch).map_err(|_| ConfigRecoveryError::InvalidSource)?,
        ),
        ..Default::default()
    };
    crate::merge_toml_value(&mut merged, toml::Value::Table(patch));
    let (mut resolved, _) = crate::resolve_composed_config_value(&merged)
        .map_err(|_| ConfigRecoveryError::InvalidSource)?;
    crate::contexts::apply_profile_override(&mut resolved, &overrides)
        .map_err(|_| ConfigRecoveryError::InvalidSource)?;
    crate::validate_config_value(resolved, "recovery")
        .map_err(|_| ConfigRecoveryError::InvalidSource)
}

/// Load current configuration and retain provenance in the same operation.
///
/// Uses the same effective process overrides as `crate::load_config`, including
/// scoped exclusive configuration and typed context/profile selections.
///
/// # Errors
/// Returns ordinary configuration load errors for invalid source layers.
pub fn load_current_config_with_recovery_context()
-> Result<(BcodeConfig, crate::ConfigLaunchContext), crate::ConfigError> {
    load_current_config_with_environment(&crate::ProcessConfigEnvironment)
}

fn load_current_config_with_environment(
    environment: &impl crate::ConfigEnvironment,
) -> Result<(BcodeConfig, crate::ConfigLaunchContext), crate::ConfigError> {
    let overrides = crate::effective_config_overrides(environment);
    load_config_with_recovery_context(environment, &overrides)
}

/// Load with source provenance retained from the same environment and overrides.
///
/// This is the preferred initiating-client capture boundary;
/// callers must not infer recovery sources from ambient environment afterward.
/// Capture still replays and verifies against the actual resulting config.
///
/// # Errors
/// Returns ordinary configuration load errors if a layer cannot be loaded.
pub fn load_config_with_recovery_context(
    environment: &impl crate::ConfigEnvironment,
    overrides: &ConfigLoadOverrides,
) -> Result<(BcodeConfig, crate::ConfigLaunchContext), crate::ConfigError> {
    let cwd = environment.current_dir();
    let paths = crate::default_config_paths_with_environment(environment);
    let sources = sources_from_overrides(&cwd, &paths, overrides);
    let config = crate::load_config_from_paths_with_overrides_and_environment(
        &paths,
        overrides,
        environment,
    )?;
    let launch = crate::ConfigLaunchContext {
        config_files: sources
            .exclusive_config_path
            .iter()
            .chain(sources.base_config_path.iter())
            .chain(sources.env_config_path.iter())
            .chain(sources.cli_config_path.iter())
            .cloned()
            .collect(),
        inline_overrides: sources.inline_overrides,
        provider_config_directory: crate::default_config_dir().join("providers"),
        recovery_sources: Some(sources),
    };
    Ok((config, launch))
}

/// Retain load-time sources and recognize only typed context/profile patches.
/// Other inline keys remain explicitly unavailable; their values are discarded.
#[must_use]
pub fn sources_from_overrides(
    cwd: &Path,
    discovered_paths: &[PathBuf],
    overrides: &ConfigLoadOverrides,
) -> ConfigRecoverySources {
    let absolute = |path: &PathBuf| {
        if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        }
    };
    let mut sources = ConfigRecoverySources {
        cwd: cwd.to_owned(),
        discovered_paths: discovered_paths.iter().map(absolute).collect(),
        exclusive_config_path: overrides.exclusive_config_path.as_ref().map(absolute),
        base_config_path: overrides.base_config_path.as_ref().map(absolute),
        env_config_path: overrides.env_config_path.as_ref().map(absolute),
        cli_config_path: overrides.cli_config_path.as_ref().map(absolute),
        ..Default::default()
    };
    for raw in [
        overrides
            .exclusive_config_path
            .is_none()
            .then_some(overrides.env_config_toml.as_deref())
            .flatten(),
        overrides.cli_config_toml.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let Ok(toml::Value::Table(table)) = toml::from_str(raw) else {
            sources.inline_overrides = true;
            continue;
        };
        for (key, value) in table {
            let target = match key.as_str() {
                "contexts" => Some(("active", &mut sources.active_context)),
                "model" => Some(("profile", &mut sources.model_profile)),
                _ => None,
            };
            if let Some((field, target)) = target
                && let Some(table) = value.as_table()
                && table.len() == 1
                && let Some(selected) = table.get(field).and_then(toml::Value::as_str)
            {
                *target = Some(selected.to_owned());
            } else {
                sources.inline_overrides = true;
            }
        }
    }
    sources
}

/// Default independently authorized recovery roots: session working directory
/// and this process's user configuration directory, never recipe-provided roots.
///
/// # Errors
/// Rejects relative, missing, or unresolvable working directories. A nonexistent
/// user configuration directory contributes no authority.
pub fn default_allowed_recovery_roots(cwd: &Path) -> Result<Vec<PathBuf>, ConfigRecoveryError> {
    if !cwd.is_absolute() {
        return Err(ConfigRecoveryError::SourceBounds);
    }
    let mut roots = vec![
        cwd.canonicalize()
            .map_err(|_| ConfigRecoveryError::SourceBounds)?,
    ];
    let config_dir = crate::default_config_dir();
    if config_dir.exists() {
        roots.push(
            config_dir
                .canonicalize()
                .map_err(|_| ConfigRecoveryError::SourceBounds)?,
        );
    }
    Ok(roots)
}

/// Capture a launch reference only if complete provenance reproduces config.
///
/// # Errors
/// Returns `Unavailable` for legacy launch contexts or unrepeatable overrides;
/// other errors report bounded source verification failures without excerpts.
pub fn capture(
    effective: &BcodeConfig,
    launch: Option<&crate::ConfigLaunchContext>,
) -> Result<ConfigRecoveryReference, ConfigRecoveryError> {
    let sources = launch
        .and_then(|launch| launch.recovery_sources.as_ref())
        .ok_or(ConfigRecoveryError::Unavailable)?;
    capture_with_sources(
        effective,
        sources,
        &default_allowed_recovery_roots(&sources.cwd)?,
    )
}

/// Resolve using the current process working directory as independent authority.
///
/// Servers must instead use `resolve_with_roots` with the session directory
/// obtained from canonical session state, never `reference.sources.cwd`.
///
/// # Errors
/// Returns a secret-safe version, source, bounds, or fingerprint failure.
pub fn resolve(reference: &ConfigRecoveryReference) -> Result<BcodeConfig, ConfigRecoveryError> {
    if reference.schema_version != CONFIG_RECOVERY_VERSION {
        return Err(ConfigRecoveryError::UnsupportedVersion);
    }
    let cwd = std::env::current_dir().map_err(|_| ConfigRecoveryError::SourceBounds)?;
    resolve_with_roots(reference, &default_allowed_recovery_roots(&cwd)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::{capture_with_sources as capture, resolve_with_roots as resolve};

    fn sources(directory: &Path, names: &[&str]) -> ConfigRecoverySources {
        ConfigRecoverySources {
            cwd: directory.to_owned(),
            discovered_paths: names.iter().map(|name| directory.join(name)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn layer_order_changes_missing_and_future_are_verified() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("base.toml"), "[model]\nprofile='first'\n").unwrap();
        std::fs::write(dir.path().join("last.toml"), "[model]\nprofile='last'\n").unwrap();
        let sources = sources(dir.path(), &["base.toml", "last.toml", "optional.toml"]);
        let roots = vec![dir.path().to_owned()];
        let effective = replay(&sources, &roots).unwrap().0;
        let reference = capture(&effective, &sources, &roots).unwrap();
        assert_eq!(resolve(&reference, &roots).unwrap(), effective);
        let mut future = reference.clone();
        future.schema_version += 1;
        assert_eq!(
            resolve(&future, &[]),
            Err(ConfigRecoveryError::UnsupportedVersion)
        );
        std::fs::write(dir.path().join("optional.toml"), "").unwrap();
        assert_eq!(
            resolve(&reference, &roots),
            Err(ConfigRecoveryError::Changed)
        );
        std::fs::write(dir.path().join("last.toml"), "[model]\nprofile='changed'\n").unwrap();
        assert_eq!(
            resolve(&reference, &roots),
            Err(ConfigRecoveryError::Changed)
        );
        std::fs::remove_file(dir.path().join("last.toml")).unwrap();
        assert_eq!(
            resolve(&reference, &roots),
            Err(ConfigRecoveryError::Changed)
        );
    }

    #[test]
    fn exclusive_inline_no_files_and_redaction() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("only.toml");
        std::fs::write(&path, "[model]\nprofile='only'\n").unwrap();
        let mut source = sources(dir.path(), &["ignored.toml"]);
        source.exclusive_config_path = Some(path.clone());
        let roots = vec![dir.path().to_owned()];
        let effective = replay(&source, &roots).unwrap().0;
        let reference = capture(&effective, &source, &roots).unwrap();
        std::fs::write(dir.path().join("ignored.toml"), "not TOML secret-marker").unwrap();
        assert_eq!(resolve(&reference, &roots).unwrap(), effective);
        source.inline_overrides = true;
        assert_eq!(
            capture(&effective, &source, &roots),
            Err(ConfigRecoveryError::Unavailable)
        );
        assert_eq!(
            replay(&sources(dir.path(), &[]), &roots),
            Err(ConfigRecoveryError::Unavailable)
        );
        std::fs::write(&path, "secret-marker not TOML").unwrap();
        let error = resolve(&reference, &roots).unwrap_err();
        assert!(!format!("{error:?} {error}").contains("secret-marker"));
        assert!(
            !serde_json::to_string(&reference)
                .unwrap()
                .contains("secret-marker")
        );
    }

    #[test]
    fn required_layers_and_escaping_paths_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "").unwrap();
        let roots = vec![dir.path().to_owned()];
        for field in ["base", "env", "cli"] {
            let mut source = sources(dir.path(), &["config.toml"]);
            let missing = Some(dir.path().join("missing.toml"));
            match field {
                "base" => source.base_config_path = missing,
                "env" => source.env_config_path = missing,
                _ => source.cli_config_path = missing,
            }
            assert_eq!(
                replay(&source, &roots),
                Err(ConfigRecoveryError::InvalidSource)
            );
        }
        let mut source = sources(dir.path(), &["../outside.toml"]);
        assert_eq!(
            replay(&source, &roots),
            Err(ConfigRecoveryError::SourceBounds)
        );
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            let target = outside.path().join("outside.toml");
            std::fs::write(&target, "").unwrap();
            std::os::unix::fs::symlink(target, dir.path().join("link.toml")).unwrap();
            source.discovered_paths = vec![dir.path().join("link.toml")];
            assert_eq!(
                replay(&source, &roots),
                Err(ConfigRecoveryError::SourceBounds)
            );
            std::os::unix::fs::symlink(
                dir.path().join("config.toml"),
                dir.path().join("inside.toml"),
            )
            .unwrap();
            source.discovered_paths = vec![dir.path().join("inside.toml")];
            assert!(replay(&source, &roots).is_ok());
        }
    }

    #[test]
    fn execution_policy_not_presentation_changes_digest() {
        let mut config = BcodeConfig::default();
        let digest = execution_fingerprint(&config).unwrap();
        config.tui.theme.name = "changed".to_owned();
        assert_eq!(execution_fingerprint(&config).unwrap(), digest);
        config
            .model
            .settings
            .insert("private_setting".to_owned(), "secret-marker".to_owned());
        assert_ne!(execution_fingerprint(&config).unwrap(), digest);
        assert!(
            !execution_fingerprint(&config)
                .unwrap()
                .contains("secret-marker")
        );
    }

    #[test]
    fn typed_profile_and_context_patches_replay_without_environment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[contexts.entries.work.model.profiles.fast]\nprovider_plugin_id='owned.provider'\nmodel_id='fast-model'\n").unwrap();
        let overrides = ConfigLoadOverrides {
            cli_config_toml: Some(
                "[contexts]\nactive='work'\n[model]\nprofile='fast'\n".to_owned(),
            ),
            ..Default::default()
        };
        let source = sources_from_overrides(dir.path(), &[path], &overrides);
        assert!(!source.inline_overrides);
        let config = replay(&source, &[dir.path().to_owned()]).unwrap().0;
        assert_eq!(config.active_context.as_deref(), Some("work"));
        assert_eq!(config.model.profile.as_deref(), Some("fast"));
        let expected = crate::load_config_from_paths_with_overrides_and_environment(
            &source.discovered_paths,
            &overrides,
            &crate::ConfigEnvironmentSnapshot::new(
                std::collections::BTreeMap::new(),
                dir.path().to_owned(),
            ),
        )
        .unwrap();
        assert_eq!(
            execution_fingerprint(&config).unwrap(),
            execution_fingerprint(&expected).unwrap()
        );
    }

    #[test]
    fn explicit_layers_and_capture_wrapper_retain_load_time_sources() {
        let dir = tempfile::tempdir().unwrap();
        for (name, profile) in [
            ("base", "base"),
            ("discovered", "discovered"),
            ("env", "env"),
            ("cli", "cli"),
        ] {
            std::fs::write(
                dir.path().join(format!("{name}.toml")),
                format!("[model]\nprofile='{profile}'\n"),
            )
            .unwrap();
        }
        let overrides = ConfigLoadOverrides {
            base_config_path: Some(dir.path().join("base.toml")),
            env_config_path: Some(dir.path().join("env.toml")),
            cli_config_path: Some(dir.path().join("cli.toml")),
            ..Default::default()
        };
        let source = sources_from_overrides(
            dir.path(),
            &[dir.path().join("discovered.toml")],
            &overrides,
        );
        let roots = vec![dir.path().to_owned()];
        let actual = crate::load_config_from_paths_with_overrides_and_environment(
            &source.discovered_paths,
            &overrides,
            &crate::ConfigEnvironmentSnapshot::new(
                std::collections::BTreeMap::new(),
                dir.path().to_owned(),
            ),
        )
        .unwrap();
        assert_eq!(actual.model.profile.as_deref(), Some("cli"));
        let launch = crate::ConfigLaunchContext {
            recovery_sources: Some(source.clone()),
            ..Default::default()
        };
        let reference = super::capture(&actual, Some(&launch)).unwrap();
        assert_eq!(resolve(&reference, &roots).unwrap(), actual);
        assert_eq!(reference.source_presence, vec![true; 4]);
        assert_eq!(
            super::capture(&actual, None),
            Err(ConfigRecoveryError::Unavailable)
        );
        assert_eq!(
            super::capture(&actual, Some(&crate::ConfigLaunchContext::default())),
            Err(ConfigRecoveryError::Unavailable)
        );
        let mut transient = overrides;
        transient.cli_config_toml = Some("[model.settings]\nprivate='secret-marker'\n".to_owned());
        let unavailable = sources_from_overrides(dir.path(), &source.discovered_paths, &transient);
        assert!(unavailable.inline_overrides);
        assert!(!format!("{unavailable:?}").contains("secret-marker"));
    }

    #[test]
    fn file_budget_and_auth_policy_changes_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[auth.pools.allowed]\nprofiles=['one']\n").unwrap();
        let source = sources(dir.path(), &["config.toml"]);
        let roots = vec![dir.path().to_owned()];
        let config = replay(&source, &roots).unwrap().0;
        let reference = capture(&config, &source, &roots).unwrap();
        std::fs::write(&path, "[auth.pools.allowed]\nprofiles=['one','two']\n").unwrap();
        assert_eq!(
            resolve(&reference, &roots),
            Err(ConfigRecoveryError::Changed)
        );
        File::create(path).unwrap().set_len(MAX_BYTES + 1).unwrap();
        assert_eq!(
            resolve(&reference, &roots),
            Err(ConfigRecoveryError::SourceBounds)
        );
    }

    #[test]
    fn fidelity_and_confinement_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "[model]\nprofile='named'\n").unwrap();
        let source = sources(dir.path(), &["config.toml"]);
        let roots = vec![dir.path().to_owned()];
        let effective = replay(&source, &roots).unwrap().0;
        assert_eq!(
            capture(&BcodeConfig::default(), &source, &roots),
            Err(ConfigRecoveryError::Changed)
        );
        assert_eq!(
            capture(&effective, &source, &[]),
            Err(ConfigRecoveryError::SourceBounds)
        );
        let mut changed = source;
        changed.model_profile = Some("different".to_owned());
        assert_eq!(
            capture(&effective, &changed, &roots),
            Err(ConfigRecoveryError::Changed)
        );
    }
}
