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

/// Retained authority for the daemon's exact startup loader sources.
///
/// Capture from host-owned load provenance before setup edits, never from a client's
/// working directory or a refresh request. Failure disables changed-config replay only;
/// it must not replace or invalidate existing recovery references. This value is neither
/// serializable nor a permission grant, and contains no configuration contents.
#[derive(Debug, Clone)]
pub struct ConfigSourceAuthority {
    sources: ConfigRecoverySources,
    cwd: PathBuf,
    targets: Vec<PathBuf>,
    roots: Vec<PathBuf>,
    read_only: bool,
    pool_targets: std::collections::BTreeMap<PathBuf, PathBuf>,
}

impl ConfigSourceAuthority {
    /// Retain exact startup provenance and canonical parent/exact filename boundaries.
    /// Missing optional layers retain their target under the nearest existing ancestor;
    /// no directories or files are created. File contents and inode identity are not pinned.
    ///
    /// # Errors
    /// Rejects inline/unavailable provenance, excessive source bounds, traversal,
    /// missing required layers, invalid parents, and symlinked file targets.
    pub fn capture(sources: &ConfigRecoverySources) -> Result<Self, ConfigRecoveryError> {
        Self::capture_kind(sources, false)
    }

    /// Retain exact loader-selected read sources, including immutable symlink targets.
    /// This conveys no permission to edit those sources or replay arbitrary persisted paths.
    /// # Errors
    /// Rejects invalid, excessive or unavailable launch provenance.
    pub fn capture_read_only(sources: &ConfigRecoverySources) -> Result<Self, ConfigRecoveryError> {
        Self::capture_kind(sources, true)
    }

    fn capture_kind(
        sources: &ConfigRecoverySources,
        read_only: bool,
    ) -> Result<Self, ConfigRecoveryError> {
        if sources.inline_overrides || !sources.cwd.is_absolute() {
            return Err(ConfigRecoveryError::Unavailable);
        }
        let paths = authority_paths(sources)?;
        let cwd = sources
            .cwd
            .canonicalize()
            .map_err(|_| ConfigRecoveryError::InvalidSource)?;
        if !cwd.is_dir() {
            return Err(ConfigRecoveryError::InvalidSource);
        }
        let mut targets = Vec::with_capacity(paths.len());
        let mut roots = Vec::with_capacity(paths.len());
        for (path, required) in paths {
            let (target, root) = authority_target_kind(path, &sources.cwd, required, read_only)?;
            targets.push(target);
            roots.push(root);
        }
        let mut pool_targets = std::collections::BTreeMap::new();
        let mut remaining = MAX_BYTES;
        for ((path, required), target) in authority_paths(sources)?.into_iter().zip(&targets) {
            let logical = if path.is_absolute() {
                path.clone()
            } else {
                sources.cwd.join(path)
            };
            let raw = match read_bounded_source(target, &mut remaining) {
                Ok(raw) => raw,
                Err(_) if !required && !target.exists() => continue,
                Err(error) => return Err(error),
            };
            let mut layer = toml::from_str(&raw).map_err(|_| ConfigRecoveryError::InvalidSource)?;
            crate::shared_pools::expand_with_reader(&mut layer, &logical, |pool| {
                let (target, _) =
                    pool_target(pool, &sources.cwd, read_only).map_err(pool_source_error)?;
                let raw =
                    read_bounded_source(&target, &mut remaining).map_err(pool_source_error)?;
                pool_targets.insert(pool.to_owned(), target);
                Ok(raw)
            })
            .map_err(|_| ConfigRecoveryError::InvalidSource)?;
        }
        Ok(Self {
            sources: sources.clone(),
            cwd,
            targets,
            roots,
            read_only,
            pool_targets,
        })
    }

    /// Verify retained provenance and current path confinement, then use existing replay.
    /// Exact-target atomic replacement and changed contents are allowed, but the supplied
    /// effective config must match replay. Selection, order, cwd, context and profile cannot
    /// be substituted. The source root always comes from captured host startup provenance.
    ///
    /// # Errors
    /// Returns `Changed` for substituted sources or a replay/config mismatch; otherwise
    /// returns the existing recovery error for unavailable, invalid or unconfined sources.
    pub fn verify(
        &self,
        config: &BcodeConfig,
        sources: &ConfigRecoverySources,
    ) -> Result<ConfigRecoveryReference, ConfigRecoveryError> {
        if sources != &self.sources {
            return Err(ConfigRecoveryError::Changed);
        }
        if sources
            .cwd
            .canonicalize()
            .map_err(|_| ConfigRecoveryError::InvalidSource)?
            != self.cwd
        {
            return Err(ConfigRecoveryError::SourceBounds);
        }
        for ((path, required), expected) in authority_paths(sources)?.into_iter().zip(&self.targets)
        {
            let (target, _) =
                authority_target_kind(path, &self.sources.cwd, required, self.read_only)?;
            if &target != expected {
                return Err(ConfigRecoveryError::SourceBounds);
            }
        }
        if self.read_only {
            let (replayed, source_presence) = replay_inner(
                sources,
                &self.roots,
                Some(&self.targets),
                Some(&self.pool_targets),
            )?;
            let execution_digest = execution_fingerprint(config)?;
            if execution_digest != execution_fingerprint(&replayed)? {
                return Err(ConfigRecoveryError::Changed);
            }
            Ok(ConfigRecoveryReference {
                schema_version: CONFIG_RECOVERY_VERSION,
                sources: sources.clone(),
                execution_digest,
                source_presence,
            })
        } else {
            capture_with_sources(config, sources, &self.roots)
        }
    }
    /// Verify the same retained sources from another client workspace.
    /// Only absent workspace-discovery layers may differ; this grants no authority to read
    /// new files or follow new targets. Explicit sources, selectors and existing layers stay pinned.
    ///
    /// # Errors
    /// Rejects substituted sources, newly present workspace layers, changed targets or contents
    /// inconsistent with the supplied configuration.
    pub fn verify_workspace(
        &self,
        config: &BcodeConfig,
        sources: &ConfigRecoverySources,
    ) -> Result<(), ConfigRecoveryError> {
        if sources == &self.sources {
            return self.verify(config, sources).map(|_| ());
        }
        if !self.read_only || !sources.cwd.is_absolute() || !sources.cwd.is_dir() {
            return Err(ConfigRecoveryError::SourceBounds);
        }
        authority_paths(sources)?;
        let mut normalized = sources.clone();
        normalized.cwd.clone_from(&self.sources.cwd);
        // Exclusive-file loads do not consume discovered paths at all.
        if sources.exclusive_config_path.is_some() {
            normalized
                .discovered_paths
                .clone_from(&self.sources.discovered_paths);
        } else if sources.discovered_paths != self.sources.discovered_paths {
            self.verify_absent_workspace_layers(sources)?;
            normalized
                .discovered_paths
                .clone_from(&self.sources.discovered_paths);
        }
        self.verify(config, &normalized).map(|_| ())
    }

    fn verify_absent_workspace_layers(
        &self,
        sources: &ConfigRecoverySources,
    ) -> Result<(), ConfigRecoveryError> {
        let environment = crate::ConfigEnvironmentSnapshot::new(
            std::collections::BTreeMap::new(),
            sources.cwd.clone(),
        );
        let workspace =
            crate::default_config_paths_from_with_environment(&sources.cwd, &environment);
        if sources.discovered_paths.len() != self.sources.discovered_paths.len() {
            return Err(ConfigRecoveryError::Changed);
        }
        for (previous, current) in self
            .sources
            .discovered_paths
            .iter()
            .zip(&sources.discovered_paths)
        {
            if previous == current {
                continue;
            }
            if !workspace.contains(current) {
                return Err(ConfigRecoveryError::Changed);
            }
            for path in [previous, current] {
                match std::fs::symlink_metadata(path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    _ => return Err(ConfigRecoveryError::Changed),
                }
            }
        }
        Ok(())
    }
}

// References come from a captured declaring document, not client-supplied paths.
// Canonicalize the native file-relative reference (including ../) at capture; verification
// still requires the exact same resolved target before any contents are read.
fn pool_target(
    path: &Path,
    cwd: &Path,
    read_only: bool,
) -> Result<(PathBuf, PathBuf), ConfigRecoveryError> {
    if read_only {
        let target = path
            .canonicalize()
            .map_err(|_| ConfigRecoveryError::InvalidSource)?;
        authority_target_kind(&target, cwd, true, true)
    } else {
        authority_target_kind(path, cwd, true, false)
    }
}

fn pool_source_error(_: ConfigRecoveryError) -> crate::ConfigError {
    crate::ConfigError::Composition {
        message: "Shared pool source is unavailable or changed".into(),
    }
}

fn read_bounded_source(path: &Path, remaining: &mut u64) -> Result<String, ConfigRecoveryError> {
    let file = File::open(path).map_err(|_| ConfigRecoveryError::InvalidSource)?;
    let metadata = file
        .metadata()
        .map_err(|_| ConfigRecoveryError::InvalidSource)?;
    if !metadata.is_file() || metadata.len() > *remaining {
        return Err(ConfigRecoveryError::SourceBounds);
    }
    let mut raw = String::new();
    file.take(*remaining + 1)
        .read_to_string(&mut raw)
        .map_err(|_| ConfigRecoveryError::InvalidSource)?;
    *remaining = remaining
        .checked_sub(u64::try_from(raw.len()).map_err(|_| ConfigRecoveryError::SourceBounds)?)
        .ok_or(ConfigRecoveryError::SourceBounds)?;
    Ok(raw)
}

fn authority_paths(
    sources: &ConfigRecoverySources,
) -> Result<Vec<(&PathBuf, bool)>, ConfigRecoveryError> {
    // Bound before allocating, including bypassed provenance retained for exact equality.
    if sources.discovered_paths.len() > MAX_LAYERS
        || sources.cwd.as_os_str().len() > 4096
        || sources
            .discovered_paths
            .iter()
            .chain(sources.exclusive_config_path.iter())
            .chain(sources.base_config_path.iter())
            .chain(sources.env_config_path.iter())
            .chain(sources.cli_config_path.iter())
            .any(|path| path.as_os_str().len() > 4096)
        || [&sources.active_context, &sources.model_profile]
            .into_iter()
            .flatten()
            .any(|selected| selected.len() > 256)
    {
        return Err(ConfigRecoveryError::SourceBounds);
    }
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
    if paths.is_empty() {
        return Err(ConfigRecoveryError::Unavailable);
    }
    if paths.len() > MAX_LAYERS {
        return Err(ConfigRecoveryError::SourceBounds);
    }
    Ok(paths)
}

fn authority_target_kind(
    path: &Path,
    cwd: &Path,
    required: bool,
    read_only: bool,
) -> Result<(PathBuf, PathBuf), ConfigRecoveryError> {
    let selected = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    if read_only && selected.exists() {
        if selected
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err(ConfigRecoveryError::SourceBounds);
        }
        let target = selected
            .canonicalize()
            .map_err(|_| ConfigRecoveryError::InvalidSource)?;
        if !target.is_file() {
            return Err(ConfigRecoveryError::InvalidSource);
        }
        let parent = target
            .parent()
            .ok_or(ConfigRecoveryError::SourceBounds)?
            .to_path_buf();
        Ok((target, parent))
    } else {
        authority_target(path, cwd, required)
    }
}

fn authority_target(
    path: &Path,
    cwd: &Path,
    required: bool,
) -> Result<(PathBuf, PathBuf), ConfigRecoveryError> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    if path
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(ConfigRecoveryError::SourceBounds);
    }
    let name = path.file_name().ok_or(ConfigRecoveryError::SourceBounds)?;
    let parent = path.parent().ok_or(ConfigRecoveryError::SourceBounds)?;
    let mut ancestor = parent;
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ancestor = ancestor.parent().ok_or(ConfigRecoveryError::SourceBounds)?;
            }
            Err(_) => return Err(ConfigRecoveryError::InvalidSource),
        }
    }
    let root = ancestor
        .canonicalize()
        .map_err(|_| ConfigRecoveryError::InvalidSource)?;
    if !root.is_dir() {
        return Err(ConfigRecoveryError::InvalidSource);
    }
    let suffix = parent
        .strip_prefix(ancestor)
        .map_err(|_| ConfigRecoveryError::SourceBounds)?;
    let target = root.join(suffix).join(name);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(ConfigRecoveryError::SourceBounds);
        }
        Ok(metadata) if !metadata.is_file() => return Err(ConfigRecoveryError::InvalidSource),
        Ok(_) => {
            if path
                .canonicalize()
                .map_err(|_| ConfigRecoveryError::InvalidSource)?
                != target
            {
                return Err(ConfigRecoveryError::SourceBounds);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => {}
        Err(_) => return Err(ConfigRecoveryError::InvalidSource),
    }
    Ok((target, root))
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
    replay_inner(sources, allowed_roots, None, None)
}

fn replay_inner(
    sources: &ConfigRecoverySources,
    allowed_roots: &[PathBuf],
    exact_targets: Option<&[PathBuf]>,
    pool_targets: Option<&std::collections::BTreeMap<PathBuf, PathBuf>>,
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
    for (index, (path, required)) in paths.into_iter().enumerate() {
        let path = if let Some(targets) = exact_targets {
            let (target, _) = authority_target_kind(path, &sources.cwd, required, true)?;
            if targets.get(index) != Some(&target) {
                return Err(ConfigRecoveryError::Changed);
            }
            target
        } else {
            confined(path, &sources.cwd, &roots)?
        };
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
        let logical = authority_paths(sources)?[index].0;
        let layer = replay_layer(&raw, logical, sources, &roots, pool_targets, &mut remaining)?;
        crate::merge_toml_value(&mut merged, layer);
        presence.push(true);
    }
    // With every optional layer absent the durable source is the built-in defaults; any
    // non-file input that changed the effective configuration still fails the fingerprint.
    let config = resolve_replayed_config(merged, sources)?;
    Ok((config, presence))
}

fn replay_layer(
    raw: &str,
    logical: &Path,
    sources: &ConfigRecoverySources,
    roots: &[PathBuf],
    pool_targets: Option<&std::collections::BTreeMap<PathBuf, PathBuf>>,
    remaining: &mut u64,
) -> Result<toml::Value, ConfigRecoveryError> {
    let mut layer = toml::from_str(raw).map_err(|_| ConfigRecoveryError::InvalidSource)?;
    let logical = if logical.is_absolute() {
        logical.to_owned()
    } else {
        sources.cwd.join(logical)
    };
    crate::shared_pools::expand_with_reader(&mut layer, &logical, |pool| {
        let target = if let Some(pinned) = pool_targets {
            let (target, _) = pool_target(pool, &sources.cwd, true).map_err(pool_source_error)?;
            if pinned.get(pool) != Some(&target) {
                return Err(pool_source_error(ConfigRecoveryError::Changed));
            }
            target
        } else {
            confined(pool, &sources.cwd, roots).map_err(pool_source_error)?
        };
        read_bounded_source(&target, remaining).map_err(pool_source_error)
    })
    .map_err(|_| ConfigRecoveryError::InvalidSource)?;
    Ok(layer)
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

    #[cfg(unix)]
    #[test]
    fn shared_pool_replay_pins_logical_reference_and_nix_targets() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("providers")).unwrap();
        let config = dir.path().join("providers/sol.toml");
        let pool = dir.path().join("pools.toml");
        let target = store.path().join("preset.toml");
        let pool_target = store.path().join("pool.toml");
        std::fs::write(&target, "auth_pool_config='../pools.toml'\n").unwrap();
        std::fs::write(
            &pool_target,
            "schema_version=1\n[auth.pools.accounts]\nprofiles=['one']\n",
        )
        .unwrap();
        symlink(&target, &config).unwrap();
        symlink(&pool_target, &pool).unwrap();
        let source = sources(dir.path(), &["providers/sol.toml"]);
        let effective = crate::load_config_from_paths_with_overrides_and_environment(
            &source.discovered_paths,
            &ConfigLoadOverrides::default(),
            &crate::ConfigEnvironmentSnapshot::new(
                std::collections::BTreeMap::new(),
                dir.path().to_owned(),
            ),
        )
        .unwrap();
        let authority = ConfigSourceAuthority::capture_read_only(&source).unwrap();
        assert!(authority.verify(&effective, &source).is_ok());
        std::fs::write(
            &pool_target,
            "schema_version=1\n[auth.pools.accounts]\nprofiles=['two']\n",
        )
        .unwrap();
        assert!(authority.verify(&effective, &source).is_err());
        std::fs::remove_file(&pool).unwrap();
        let replacement = store.path().join("replacement.toml");
        std::fs::write(
            &replacement,
            "schema_version=1\n[auth.pools.accounts]\nprofiles=['one']\n",
        )
        .unwrap();
        symlink(&replacement, &pool).unwrap();
        assert!(authority.verify(&effective, &source).is_err());
        // Durable maintenance replay does not gain the read-only Nix target authority.
        assert!(replay(&source, &[dir.path().to_owned()]).is_err());
    }

    #[test]
    fn shared_pool_recovery_is_bounded_and_preserves_composition() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("sol.toml"),
            "auth_pool_config='pools.toml'\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pools.toml"),
            "schema_version=1\n[auth.pools.accounts]\nprofiles=['one']\n",
        )
        .unwrap();
        let source = sources(dir.path(), &["sol.toml"]);
        let effective = replay(&source, &[dir.path().to_owned()]).unwrap().0;
        assert_eq!(effective.auth.pools["accounts"].profiles, vec!["one"]);
        let authority = ConfigSourceAuthority::capture_read_only(&source).unwrap();
        assert!(authority.verify(&effective, &source).is_ok());
        std::fs::write(
            dir.path().join("pools.toml"),
            "x".repeat(usize::try_from(MAX_BYTES).unwrap() + 1),
        )
        .unwrap();
        assert!(authority.verify(&effective, &source).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn selected_read_only_symlink_is_verified_without_edit_authority() {
        let dir = tempfile::tempdir().unwrap();
        let immutable = tempfile::tempdir().unwrap();
        let first = immutable.path().join("first.toml");
        let second = immutable.path().join("second.toml");
        std::fs::write(&first, "[model]\nprofile='native'\n").unwrap();
        std::fs::write(&second, "[model]\nprofile='native'\n").unwrap();
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&first, &link).unwrap();
        let sources = sources(dir.path(), &["config.toml"]);
        let effective = replay(&sources, &[immutable.path().to_path_buf()])
            .unwrap()
            .0;
        let authority = ConfigSourceAuthority::capture_read_only(&sources).unwrap();
        assert!(authority.verify(&effective, &sources).is_ok());
        assert!(ConfigSourceAuthority::capture(&sources).is_err());
        assert!(capture(&effective, &sources, &[dir.path().to_path_buf()]).is_err());
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&second, &link).unwrap();
        assert!(authority.verify(&effective, &sources).is_err());
    }

    #[test]
    fn retained_authority_accepts_only_exact_target_replacement_and_matching_config() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.toml");
        std::fs::write(&target, "[model]\nprofile='before'\n").unwrap();
        let sources = sources(dir.path(), &["config.toml"]);
        let roots = vec![dir.path().to_owned()];
        let before = replay(&sources, &roots).unwrap().0;
        let authority = ConfigSourceAuthority::capture(&sources).unwrap();
        let original = authority.verify(&before, &sources).unwrap();
        let replacement = dir.path().join("replacement.toml");
        std::fs::write(&replacement, "[model]\nprofile='after'\n").unwrap();
        std::fs::rename(&replacement, &target).unwrap();
        let after = replay(&sources, &roots).unwrap().0;
        assert_eq!(
            authority.verify(&before, &sources),
            Err(ConfigRecoveryError::Changed)
        );
        let reference = authority.verify(&after, &sources).unwrap();
        assert_ne!(reference.execution_digest, original.execution_digest);
        assert_eq!(resolve(&reference, &roots).unwrap(), after);
        assert_eq!(
            resolve(&original, &roots),
            Err(ConfigRecoveryError::Changed)
        );
    }

    #[test]
    fn retained_authority_rejects_provenance_substitution() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["first.toml", "second.toml", "other.toml"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let original = sources(dir.path(), &["first.toml", "second.toml"]);
        let authority = ConfigSourceAuthority::capture(&original).unwrap();
        let effective = replay(&original, &[dir.path().to_owned()]).unwrap().0;
        for index in 0..10 {
            let mut changed = original.clone();
            match index {
                0 => changed.discovered_paths[0] = dir.path().join("other.toml"),
                1 => changed.discovered_paths.reverse(),
                2 => changed.active_context = Some("other".to_owned()),
                3 => changed.model_profile = Some("other".to_owned()),
                4 => changed.cwd = dir.path().join("other"),
                5 => changed.exclusive_config_path = Some(original.discovered_paths[0].clone()),
                6 => changed.base_config_path = Some(original.discovered_paths[0].clone()),
                7 => changed.env_config_path = Some(original.discovered_paths[0].clone()),
                8 => changed.cli_config_path = Some(original.discovered_paths[0].clone()),
                _ => changed.inline_overrides = true,
            }
            assert_eq!(
                authority.verify(&effective, &changed),
                Err(ConfigRecoveryError::Changed)
            );
        }
    }

    #[test]
    fn read_authority_allows_workspace_change_without_new_source_authority() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();
        let source = ConfigRecoverySources {
            cwd: first.clone(),
            discovered_paths: vec![first.join("bcode.toml"), first.join(".bcode/bcode.toml")],
            env_config_path: Some(config_path.clone()),
            ..Default::default()
        };
        let authority = ConfigSourceAuthority::capture_read_only(&source).unwrap();
        let effective = replay(&source, &[dir.path().to_owned()]).unwrap().0;
        let mut changed = source.clone();
        changed.cwd = second.clone();
        changed.discovered_paths =
            vec![second.join("bcode.toml"), second.join(".bcode/bcode.toml")];
        assert!(authority.verify_workspace(&effective, &changed).is_ok());
        assert!(authority.verify(&effective, &changed).is_err());
        std::fs::write(second.join("bcode.toml"), "").unwrap();
        assert!(authority.verify_workspace(&effective, &changed).is_err());
        changed
            .discovered_paths
            .clone_from(&source.discovered_paths);
        changed.env_config_path = Some(second.join("other.toml"));
        assert!(authority.verify_workspace(&effective, &changed).is_err());
        changed.env_config_path = Some(config_path);
        changed.model_profile = Some("substituted".into());
        assert!(authority.verify_workspace(&effective, &changed).is_err());
        let edit_authority = ConfigSourceAuthority::capture(&source).unwrap();
        changed.model_profile = None;
        assert!(
            edit_authority
                .verify_workspace(&effective, &changed)
                .is_err()
        );
    }

    #[test]
    fn retained_authority_resolves_relative_sources_against_host_launch_cwd() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "").unwrap();
        let source = ConfigRecoverySources {
            cwd: dir.path().to_owned(),
            discovered_paths: vec![PathBuf::from("config.toml")],
            ..Default::default()
        };
        let authority = ConfigSourceAuthority::capture(&source).unwrap();
        let effective = replay(&source, &[dir.path().to_owned()]).unwrap().0;
        assert!(authority.verify(&effective, &source).is_ok());
        let mut substituted = source;
        substituted.discovered_paths[0] = dir.path().join("config.toml");
        assert_eq!(
            authority.verify(&effective, &substituted),
            Err(ConfigRecoveryError::Changed)
        );
    }

    #[test]
    fn retained_authority_bounds_optional_layers_and_unavailable_provenance() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "").unwrap();
        let mut sources = sources(dir.path(), &["config.toml", "missing/nested/optional.toml"]);
        let authority = ConfigSourceAuthority::capture(&sources).unwrap();
        let roots = vec![dir.path().to_owned()];
        let effective = replay(&sources, &roots).unwrap().0;
        assert_eq!(
            authority
                .verify(&effective, &sources)
                .unwrap()
                .source_presence,
            vec![true, false]
        );
        std::fs::create_dir_all(dir.path().join("missing/nested")).unwrap();
        std::fs::write(dir.path().join("missing/nested/optional.toml"), "").unwrap();
        assert_eq!(
            authority
                .verify(&effective, &sources)
                .unwrap()
                .source_presence,
            vec![true, true]
        );
        sources.inline_overrides = true;
        assert!(matches!(
            ConfigSourceAuthority::capture(&sources),
            Err(ConfigRecoveryError::Unavailable)
        ));
        sources.inline_overrides = false;
        sources.discovered_paths = vec![dir.path().join("optional.toml"); MAX_LAYERS + 1];
        assert!(matches!(
            ConfigSourceAuthority::capture(&sources),
            Err(ConfigRecoveryError::SourceBounds)
        ));
        sources.discovered_paths.clear();
        sources.exclusive_config_path = Some(dir.path().join("absent.toml"));
        assert!(matches!(
            ConfigSourceAuthority::capture(&sources),
            Err(ConfigRecoveryError::InvalidSource)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn retained_authority_rejects_file_and_parent_symlink_escapes() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("config.toml"), "").unwrap();
        let parent = dir.path().join("selected");
        std::fs::create_dir(&parent).unwrap();
        let target = parent.join("config.toml");
        std::fs::write(&target, "").unwrap();
        let sources = sources(
            dir.path(),
            &["selected/config.toml", "optional/config.toml"],
        );
        let authority = ConfigSourceAuthority::capture(&sources).unwrap();
        let effective = replay(&sources, &[dir.path().to_owned()]).unwrap().0;
        std::fs::remove_file(&target).unwrap();
        symlink(outside.path().join("config.toml"), &target).unwrap();
        assert_eq!(
            authority.verify(&effective, &sources),
            Err(ConfigRecoveryError::SourceBounds)
        );
        assert!(matches!(
            ConfigSourceAuthority::capture(&sources),
            Err(ConfigRecoveryError::SourceBounds)
        ));
        std::fs::remove_file(&target).unwrap();
        std::fs::write(&target, "").unwrap();
        symlink(outside.path(), dir.path().join("optional")).unwrap();
        assert_eq!(
            authority.verify(&effective, &sources),
            Err(ConfigRecoveryError::SourceBounds)
        );
        std::fs::remove_file(dir.path().join("optional")).unwrap();
        std::fs::rename(&parent, dir.path().join("original")).unwrap();
        symlink(outside.path(), &parent).unwrap();
        assert_eq!(
            authority.verify(&effective, &sources),
            Err(ConfigRecoveryError::SourceBounds)
        );
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
        #[cfg_attr(not(unix), allow(unused_mut))]
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
