#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Raw Bcode configuration file discovery and layering policy.
//!
//! This crate owns which `bcode.toml` files are read, in which order, how
//! `BCODE_CONFIG`/`BCODE_CONFIG_TOML`/CLI overlays stack on top of them, and how
//! the `[composition]` section selects profiles and layer order. It produces
//! untyped TOML; typed validation and product-specific transforms stay with the
//! caller (for example `bcode_config`). [`user_state`] locates the interactive user state that
//! layers over configuration for presentation choices.
//!
//! Lightweight tools load configuration with the same semantics as Bcode itself:
//!
//! ```no_run
//! use bcode_config_layers::{ConfigLoadOverrides, load_layered};
//!
//! let start = std::env::current_dir()?;
//! let overrides = ConfigLoadOverrides::from_environment();
//! let layered = load_layered(&start, &overrides, toml::Table::new())?;
//! for path in &layered.files {
//!     println!("{}", path.display());
//! }
//! let section = layered.value.get("plugins");
//! # let _ = section;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub mod user_state;

/// Default Bcode config file name.
pub const DEFAULT_CONFIG_FILE_NAME: &str = "bcode.toml";
/// Environment variable containing a config file path overlay.
pub const BCODE_CONFIG_ENV: &str = "BCODE_CONFIG";
/// Environment variable containing raw TOML config overlay data.
pub const BCODE_CONFIG_TOML_ENV: &str = "BCODE_CONFIG_TOML";
/// Environment variable selecting the active model profile.
///
/// A non-blank value is applied as an inline `[model] profile = ...` overlay
/// after `BCODE_CONFIG_TOML`.
pub const BCODE_MODEL_PROFILE_ENV: &str = "BCODE_MODEL_PROFILE";

/// Source of environment-dependent config inputs.
pub trait ConfigEnvironment {
    /// Return an environment variable value as UTF-8 text.
    fn var(&self, name: &str) -> Option<String>;
    /// Return an environment variable value as OS-native text.
    fn var_os(&self, name: &str) -> Option<OsString>;
    /// Return the current working directory used for default config discovery.
    fn current_dir(&self) -> PathBuf;
}

/// Config environment backed by the current process.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessConfigEnvironment;

impl ConfigEnvironment for ProcessConfigEnvironment {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn var_os(&self, name: &str) -> Option<OsString> {
        std::env::var_os(name)
    }

    fn current_dir(&self) -> PathBuf {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }
}

/// Errors produced while reading or composing configuration layers.
#[derive(Debug, Error)]
pub enum LayersError {
    /// A filesystem operation needed to anchor configuration failed.
    #[error("I/O error while reading {path}: {source}")]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// A layer could not be read, parsed, or composed.
    #[error("composition error: {message}")]
    Composition {
        /// Human-readable description of the failure.
        message: String,
    },
}

impl From<bmux_config_composition::Error> for LayersError {
    fn from(error: bmux_config_composition::Error) -> Self {
        Self::Composition {
            message: error.to_string(),
        }
    }
}

fn composition(message: impl Into<String>) -> LayersError {
    LayersError::Composition {
        message: message.into(),
    }
}

/// Config loading overrides layered above discovered config files.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ConfigLoadOverrides {
    /// Load only this required file, bypassing discovered and environment layers.
    /// Explicit CLI TOML remains applicable above this file.
    pub exclusive_config_path: Option<PathBuf>,
    /// Optional base layer merged below discovered config files.
    pub base_config_path: Option<PathBuf>,
    /// Config file path from environment or caller-provided equivalent.
    pub env_config_path: Option<PathBuf>,
    /// Raw TOML config data from environment or caller-provided equivalent.
    pub env_config_toml: Option<String>,
    /// Config file path from CLI arguments.
    pub cli_config_path: Option<PathBuf>,
    /// Raw TOML config data synthesized from CLI arguments.
    pub cli_config_toml: Option<String>,
}

impl std::fmt::Debug for ConfigLoadOverrides {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfigLoadOverrides")
            .field(
                "exclusive_config_path",
                &self.exclusive_config_path.is_some(),
            )
            .field("base_config_path", &self.base_config_path.is_some())
            .field("env_config_path", &self.env_config_path.is_some())
            .field("env_config_toml", &self.env_config_toml.is_some())
            .field("cli_config_path", &self.cli_config_path.is_some())
            .field("cli_config_toml", &self.cli_config_toml.is_some())
            .finish()
    }
}

impl ConfigLoadOverrides {
    /// Build overrides from `BCODE_CONFIG`, `BCODE_CONFIG_TOML`, and
    /// `BCODE_MODEL_PROFILE` of the current process, without CLI values.
    #[must_use]
    pub fn from_environment() -> Self {
        Self::from_env_with_cli(None, None)
    }

    /// Build overrides from `BCODE_CONFIG`, `BCODE_CONFIG_TOML`, and optional CLI values.
    #[must_use]
    pub fn from_env_with_cli(
        cli_config_path: Option<PathBuf>,
        cli_config_toml: Option<String>,
    ) -> Self {
        Self::from_config_environment_with_cli(
            &ProcessConfigEnvironment,
            cli_config_path,
            cli_config_toml,
        )
    }

    /// Build overrides from an explicit config environment and optional CLI values.
    #[must_use]
    pub fn from_config_environment_with_cli(
        environment: &impl ConfigEnvironment,
        cli_config_path: Option<PathBuf>,
        cli_config_toml: Option<String>,
    ) -> Self {
        Self {
            exclusive_config_path: None,
            base_config_path: None,
            env_config_path: environment.var_os(BCODE_CONFIG_ENV).map(PathBuf::from),
            env_config_toml: merge_config_toml_overrides(
                environment.var(BCODE_CONFIG_TOML_ENV),
                environment
                    .var(BCODE_MODEL_PROFILE_ENV)
                    .filter(|profile| !profile.trim().is_empty())
                    .map(|profile| model_profile_override_toml(&profile)),
            ),
            cli_config_path,
            cli_config_toml,
        }
    }

    /// Return true when no override layers are configured.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.exclusive_config_path.is_none()
            && self.base_config_path.is_none()
            && self.env_config_path.is_none()
            && self.env_config_toml.is_none()
            && self.cli_config_path.is_none()
            && self.cli_config_toml.is_none()
    }

    /// Set base config path.
    #[must_use]
    pub fn with_base_config_path(mut self, path: Option<PathBuf>) -> Self {
        self.base_config_path = path;
        self
    }

    /// Fluent setter for the CLI raw TOML override.
    #[must_use]
    pub fn with_cli_config_toml(mut self, toml: Option<String>) -> Self {
        self.cli_config_toml = merge_config_toml_overrides(self.cli_config_toml, toml);
        self
    }
}

/// Build a TOML override selecting a model profile.
#[must_use]
pub fn model_profile_override_toml(profile: &str) -> String {
    format!("[model]\nprofile = {}\n", toml_basic_string(profile))
}

/// Quote text as a TOML basic string as used by synthesized override layers.
#[must_use]
pub fn toml_basic_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

fn merge_config_toml_overrides(left: Option<String>, right: Option<String>) -> Option<String> {
    match (left, right) {
        (Some(mut left), Some(right)) => {
            if !left.ends_with('\n') {
                left.push('\n');
            }
            left.push_str(&right);
            Some(left)
        }
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

/// Return default config paths in merge order.
#[must_use]
pub fn default_config_paths() -> Vec<PathBuf> {
    default_config_paths_with_environment(&ProcessConfigEnvironment)
}

/// Return default config paths in merge order for an explicit environment.
#[must_use]
pub fn default_config_paths_with_environment(environment: &impl ConfigEnvironment) -> Vec<PathBuf> {
    default_config_paths_from_with_environment(&environment.current_dir(), environment)
}

/// Return default config paths in merge order for a starting directory.
#[must_use]
pub fn default_config_paths_from(start: &Path) -> Vec<PathBuf> {
    default_config_paths_from_with_environment(start, &ProcessConfigEnvironment)
}

/// Return default config paths in merge order for a starting directory and environment.
///
/// The order is the user config (`$XDG_CONFIG_HOME/bcode/bcode.toml`, else
/// `$HOME/.config/bcode/bcode.toml`), then `<root>/bcode.toml`, then
/// `<root>/.bcode/bcode.toml`, where `<root>` is the nearest ancestor of
/// `start` containing `.git` (or `start` itself when there is none).
#[must_use]
pub fn default_config_paths_from_with_environment(
    start: &Path,
    environment: &impl ConfigEnvironment,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(config_home) = environment.var("XDG_CONFIG_HOME") {
        paths.push(
            PathBuf::from(config_home)
                .join("bcode")
                .join(DEFAULT_CONFIG_FILE_NAME),
        );
    } else if let Some(home) = environment.var("HOME") {
        paths.push(
            PathBuf::from(home)
                .join(".config")
                .join("bcode")
                .join(DEFAULT_CONFIG_FILE_NAME),
        );
    }
    let root = discover_config_root(start).unwrap_or_else(|| start.to_path_buf());
    paths.push(root.join(DEFAULT_CONFIG_FILE_NAME));
    paths.push(root.join(".bcode").join(DEFAULT_CONFIG_FILE_NAME));
    paths
}

/// Return the nearest ancestor of `start` (inclusive) that contains `.git`.
#[must_use]
pub fn discover_config_root(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Recursively merge TOML config values.
///
/// Tables are merged key-by-key. Non-table overlay values replace the base
/// value at the same path.
///
/// # Panics
///
/// Panics only if a `toml::Value` cannot round-trip through the composition
/// engine, which does not happen for values produced by the `toml` crate.
pub fn merge_config_values(base: &mut toml::Value, overlay: toml::Value) {
    use bmux_config_composition::TomlCodec;
    let mut value =
        TomlCodec::from_toml(base.clone()).expect("configuration values are valid TOML");
    value.overlay_replace(
        TomlCodec::from_toml(overlay).expect("configuration overlay is valid TOML"),
    );
    *base = TomlCodec::to_toml(&value).expect("composed configuration is valid TOML");
}

/// Raw configuration merged from all existing layers, before composition.
#[derive(Debug, Clone, PartialEq)]
pub struct RawLayers {
    /// Merged raw TOML, still containing any `[composition]` section.
    pub value: toml::Value,
    /// Files that existed and were read, in precedence order (lowest first).
    pub files: Vec<PathBuf>,
    /// Which layer supplied each merged value.
    pub origins: Origins,
}

/// Inline (text) layer kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineLayer {
    /// Environment TOML (for Bcode, `BCODE_CONFIG_TOML`).
    Environment,
    /// Command-line TOML.
    Cli,
}

/// Where a configuration value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerOrigin {
    /// A configuration file.
    File(PathBuf),
    /// An inline TOML layer.
    Inline(InlineLayer),
    /// Caller-supplied defaults or a value with no recorded layer.
    Defaults,
}

impl std::fmt::Display for LayerOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(path) => write!(f, "{}", path.display()),
            Self::Inline(InlineLayer::Environment) => f.write_str("environment TOML"),
            Self::Inline(InlineLayer::Cli) => f.write_str("command-line TOML"),
            Self::Defaults => f.write_str("defaults"),
        }
    }
}

/// Per-value provenance of merged raw layers. Paths use table keys only; array elements
/// report the array's origin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Origins {
    values: BTreeMap<Vec<String>, LayerOrigin>,
}

impl Origins {
    fn new(resolved: &bmux_config_composition::Resolved) -> Self {
        use bmux_config_composition::PathSegment;
        let layers: BTreeMap<_, _> = resolved
            .sources
            .iter()
            .map(|source| {
                let origin = match (&source.path, source.layer.as_str()) {
                    (Some(path), _) => LayerOrigin::File(path.clone()),
                    (None, "environment-inline") => LayerOrigin::Inline(InlineLayer::Environment),
                    (None, "cli-inline") => LayerOrigin::Inline(InlineLayer::Cli),
                    (None, _) => LayerOrigin::Defaults,
                };
                (source.id, origin)
            })
            .collect();
        let values = resolved
            .document
            .provenance
            .iter()
            .filter_map(|(path, source)| {
                let keys: Option<Vec<String>> = path
                    .iter()
                    .map(|segment| match segment {
                        PathSegment::Key(key) => Some(key.clone()),
                        PathSegment::Index(_) => None,
                    })
                    .collect();
                Some((keys?, layers.get(source)?.clone()))
            })
            .collect();
        Self { values }
    }

    /// Layer that set exactly `path`, if any did.
    #[must_use]
    pub fn exact(&self, path: &[String]) -> Option<&LayerOrigin> {
        self.values.get(path)
    }

    /// Layer that set `path` or, failing that, its nearest recorded ancestor.
    #[must_use]
    pub fn nearest(&self, path: &[String]) -> Option<&LayerOrigin> {
        (0..=path.len())
            .rev()
            .find_map(|len| self.values.get(&path[..len]))
    }
}

enum TransformError<E> {
    Layers(LayersError),
    Hook(E),
}

impl<E> From<bmux_config_composition::Error> for TransformError<E> {
    fn from(error: bmux_config_composition::Error) -> Self {
        Self::Layers(error.into())
    }
}

/// Read and merge raw configuration layers.
///
/// Precedence (lowest first) is: base config, the provided `paths` (missing
/// files are skipped), env config file, env raw TOML, CLI config file, CLI raw
/// TOML. When `exclusive_config_path` is set, only that required file and the
/// CLI raw TOML are read.
///
/// `transform` is invoked once per file-backed document with the file path and
/// its parsed value, before layers are merged. Inline TOML layers are not passed
/// to it.
///
/// # Errors
///
/// * A required layer file is missing or unreadable.
/// * A layer is not valid TOML or exceeds the composition engine's limits.
/// * The current directory is unavailable while a relative path is used.
/// * `transform` returns an error, which is passed through unchanged.
pub fn load_raw_layers<E>(
    paths: &[PathBuf],
    overrides: &ConfigLoadOverrides,
    mut transform: impl FnMut(&Path, &mut toml::Value) -> Result<(), E>,
) -> Result<RawLayers, E>
where
    E: From<LayersError>,
{
    use bmux_config_composition::{DuplicatePolicy, LoadContext, Loader, Requirement, TomlCodec};
    let builder = Loader::builder().duplicates(DuplicatePolicy::Allow);
    let builder = if let Some(path) = &overrides.exclusive_config_path {
        builder.file("exclusive", Some(path), Requirement::Required)
    } else {
        builder
            .file(
                "base",
                overrides.base_config_path.as_deref(),
                Requirement::Optional,
            )
            .files("discovered", paths.iter().cloned(), Requirement::Optional)
            .file(
                "environment-file",
                overrides.env_config_path.as_deref(),
                Requirement::Required,
            )
            .text("environment-inline", overrides.env_config_toml.as_deref())
            .file(
                "cli-file",
                overrides.cli_config_path.as_deref(),
                Requirement::Required,
            )
    }
    .text("cli-inline", overrides.cli_config_toml.as_deref());
    let cwd = configuration_anchor(paths, overrides)?;
    let context = LoadContext::new(cwd, BTreeMap::new()).map_err(LayersError::from)?;
    let captured = builder
        .build()
        .and_then(|loader| loader.capture(&context))
        .map_err(LayersError::from)?;
    let files = captured
        .sources()
        .iter()
        .filter_map(|source| source.path.clone())
        .collect();
    // File-relative references remain caller policy. Transform individual
    // captured documents, then compose the transformed layers in their exact order.
    let mapped = captured
        .try_map_documents(&TomlCodec, |source, value| {
            let mut value = TomlCodec::to_toml(&value)?;
            if let Some(path) = &source.path {
                transform(path, &mut value).map_err(TransformError::Hook)?;
            }
            Ok(TomlCodec::from_toml(value)?)
        })
        .map_err(|error| match error {
            TransformError::Layers(error) => E::from(error),
            TransformError::Hook(error) => error,
        })?;
    let resolved = mapped.resolve(&TomlCodec).map_err(LayersError::from)?;
    let origins = Origins::new(&resolved);
    let value = resolved.decode().map_err(LayersError::from)?;
    Ok(RawLayers {
        value,
        files,
        origins,
    })
}

fn configuration_anchor(
    paths: &[PathBuf],
    overrides: &ConfigLoadOverrides,
) -> Result<PathBuf, LayersError> {
    std::env::current_dir()
        .or_else(|error| {
            let relative = paths
                .iter()
                .chain(
                    [
                        overrides.exclusive_config_path.as_ref(),
                        overrides.base_config_path.as_ref(),
                        overrides.env_config_path.as_ref(),
                        overrides.cli_config_path.as_ref(),
                    ]
                    .into_iter()
                    .flatten(),
                )
                .any(|path| path.is_relative());
            if relative {
                Err(error)
            } else {
                // Absolute-file and memory-only plans do not consult this anchor.
                Ok(std::env::temp_dir())
            }
        })
        .map_err(|source| LayersError::Io {
            path: PathBuf::from("."),
            source,
        })
}

/// `[composition]` section as read by the layering policy.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Composition {
    active_profile: Option<String>,
    layer_order: Vec<String>,
    profiles: BTreeMap<String, CompositionProfile>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CompositionProfile {
    extends: Vec<String>,
    patch: toml::Table,
}

/// Config load resolution details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionResolution {
    /// Profile selected by composition, if any.
    pub selected_profile: Option<String>,
    /// Effective layer order after defaulting.
    pub layer_order: Vec<String>,
    /// Profile ids available while resolving composition.
    pub available_profiles: Vec<String>,
}

fn canonical_profile_id(profile_id: &str) -> String {
    profile_id.trim().to_ascii_lowercase()
}

fn resolve_profile_patch(
    requested_profile_id: &str,
    profiles: &BTreeMap<String, CompositionProfile>,
) -> Result<toml::Table, LayersError> {
    use bmux_config_composition::{Profile, ProfileLimits, resolve_profile_cached};
    let profiles = profiles
        .iter()
        .map(|(name, profile)| {
            (
                name.clone(),
                Profile {
                    parents: profile
                        .extends
                        .iter()
                        .map(|parent| canonical_profile_id(parent))
                        .collect(),
                    value: profile.patch.clone(),
                },
            )
        })
        .collect();
    resolve_profile_cached(
        &canonical_profile_id(requested_profile_id),
        &profiles,
        ProfileLimits {
            max_depth: 128,
            max_expansions: 65_536,
        },
        |base, overlay| {
            let mut value = toml::Value::Table(std::mem::take(base));
            merge_config_values(&mut value, toml::Value::Table(overlay));
            *base = value.as_table().cloned().unwrap_or_default();
            Ok(())
        },
    )
    .map_err(|error| composition(format!("profile inheritance: {error}")))
}

/// Apply the `[composition]` section of merged raw configuration.
///
/// The `[composition]` table is removed and the remaining configuration plus
/// selected profile patches are layered over `defaults` in the effective layer
/// order. Without an explicit `layer_order`, the order is `defaults`,
/// `profile:active` (only when `active_profile` is set), `config`.
///
/// # Errors
///
/// * `raw` is not a table or `[composition]` is malformed.
/// * Profile ids are empty or collide after normalization.
/// * `profile:active` is used without `active_profile`.
/// * A referenced profile is unknown, or profile inheritance is cyclic or too deep.
/// * The layer order names an unknown layer.
pub fn resolve_composition(
    raw: &toml::Value,
    defaults: toml::Table,
) -> Result<(toml::Table, CompositionResolution), LayersError> {
    let mut raw_table = raw
        .as_table()
        .cloned()
        .ok_or_else(|| composition("config root must be a table"))?;
    let spec = match raw_table.remove("composition") {
        Some(value) => value
            .try_into::<Composition>()
            .map_err(|source| composition(format!("invalid [composition] config: {source}")))?,
        None => Composition::default(),
    };

    let mut profiles = BTreeMap::new();
    for (profile_id, profile) in spec.profiles {
        let canonical_id = canonical_profile_id(&profile_id);
        if canonical_id.is_empty() {
            return Err(composition("composition profile id must not be empty"));
        }
        if profiles.insert(canonical_id, profile).is_some() {
            return Err(composition(
                "composition profile IDs must be distinct after normalization",
            ));
        }
    }

    let active_profile = spec.active_profile.as_deref().map(canonical_profile_id);
    let layer_order = if spec.layer_order.is_empty() {
        if active_profile.is_some() {
            vec![
                "defaults".to_string(),
                "profile:active".to_string(),
                "config".to_string(),
            ]
        } else {
            vec!["defaults".to_string(), "config".to_string()]
        }
    } else {
        spec.layer_order
    };

    let mut resolved = toml::Value::Table(defaults);
    for layer in &layer_order {
        match layer.as_str() {
            "defaults" => {}
            "config" => merge_config_values(&mut resolved, toml::Value::Table(raw_table.clone())),
            "profile:active" => {
                let Some(active_profile) = active_profile.as_deref() else {
                    return Err(composition(
                        "layer 'profile:active' requires composition.active_profile",
                    ));
                };
                let patch = resolve_profile_patch(active_profile, &profiles)?;
                merge_config_values(&mut resolved, toml::Value::Table(patch));
            }
            _ if layer.starts_with("profile:") => {
                let profile_id = layer.trim_start_matches("profile:");
                let patch = resolve_profile_patch(profile_id, &profiles)?;
                merge_config_values(&mut resolved, toml::Value::Table(patch));
            }
            unknown => {
                return Err(composition(format!(
                    "unknown composition layer '{unknown}'"
                )));
            }
        }
    }

    let toml::Value::Table(resolved) = resolved else {
        return Err(composition("composed config root must be a table"));
    };
    let available_profiles = profiles.keys().cloned().collect();
    Ok((
        resolved,
        CompositionResolution {
            selected_profile: active_profile,
            layer_order,
            available_profiles,
        },
    ))
}

/// Fully layered and composed configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Layered {
    /// Composed configuration with `[composition]` removed.
    pub value: toml::Table,
    /// Composition details used to produce `value`.
    pub resolution: CompositionResolution,
    /// Files that existed and were read, in precedence order (lowest first).
    pub files: Vec<PathBuf>,
    /// Which layer supplied each raw value.
    pub origins: Origins,
}

/// Where a composed value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueOrigin {
    /// The layer.
    pub layer: LayerOrigin,
    /// The composition profile whose patch set it, when it came from one.
    pub profile: Option<String>,
}

impl std::fmt::Display for ValueOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.profile {
            Some(profile) => write!(f, "{} (profile {profile})", self.layer),
            None => write!(f, "{}", self.layer),
        }
    }
}

impl Layered {
    /// Best-effort origin of a composed value: the layer that set it directly, else the
    /// active profile patch that set it, else the nearest ancestor's layer.
    #[must_use]
    pub fn origin(&self, path: &[String]) -> Option<ValueOrigin> {
        let direct = |layer: &LayerOrigin| ValueOrigin {
            layer: layer.clone(),
            profile: None,
        };
        if let Some(layer) = self.origins.exact(path) {
            return Some(direct(layer));
        }
        if let Some(profile) = &self.resolution.selected_profile {
            let mut patched = vec![
                "composition".to_owned(),
                "profiles".to_owned(),
                profile.clone(),
                "patch".to_owned(),
            ];
            patched.extend_from_slice(path);
            if let Some(layer) = self.origins.exact(&patched) {
                return Some(ValueOrigin {
                    layer: layer.clone(),
                    profile: Some(profile.clone()),
                });
            }
        }
        self.origins.nearest(path).map(direct)
    }
}

/// Discover, read, merge and compose configuration starting at `start`.
///
/// Uses [`default_config_paths_from`] for discovery and applies `overrides`
/// above those files; `defaults` is the lowest composition layer (pass an empty
/// table when the caller applies its own typed defaults).
///
/// # Errors
///
/// Returns any error from [`load_raw_layers`] or [`resolve_composition`].
pub fn load_layered(
    start: &Path,
    overrides: &ConfigLoadOverrides,
    defaults: toml::Table,
) -> Result<Layered, LayersError> {
    load_layered_from_paths(&default_config_paths_from(start), overrides, defaults)
}

/// Read, merge and compose configuration from explicit discovered `paths`.
///
/// # Errors
///
/// Returns any error from [`load_raw_layers`] or [`resolve_composition`].
pub fn load_layered_from_paths(
    paths: &[PathBuf],
    overrides: &ConfigLoadOverrides,
    defaults: toml::Table,
) -> Result<Layered, LayersError> {
    let raw = load_raw_layers(paths, overrides, |_, _| Ok::<(), LayersError>(()))?;
    let (value, resolution) = resolve_composition(&raw.value, defaults)?;
    Ok(Layered {
        value,
        resolution,
        files: raw.files,
        origins: raw.origins,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TestEnvironment {
        vars: BTreeMap<String, String>,
        current_dir: PathBuf,
    }

    impl ConfigEnvironment for TestEnvironment {
        fn var(&self, name: &str) -> Option<String> {
            self.vars.get(name).cloned()
        }

        fn var_os(&self, name: &str) -> Option<OsString> {
            self.vars.get(name).map(OsString::from)
        }

        fn current_dir(&self) -> PathBuf {
            self.current_dir.clone()
        }
    }

    /// Temporary layout with a user config home and a repository containing `nested/`.
    fn layout() -> (tempfile::TempDir, TestEnvironment) {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join(".bcode")).unwrap();
        fs::create_dir_all(repo.join("nested")).unwrap();
        fs::create_dir_all(root.path().join("xdg").join("bcode")).unwrap();
        let environment = TestEnvironment {
            vars: BTreeMap::from([(
                "XDG_CONFIG_HOME".to_owned(),
                root.path().join("xdg").to_string_lossy().into_owned(),
            )]),
            current_dir: repo.join("nested"),
        };
        (root, environment)
    }

    fn load(
        environment: &TestEnvironment,
        overrides: &ConfigLoadOverrides,
        defaults: toml::Table,
    ) -> Layered {
        let paths = default_config_paths_with_environment(environment);
        load_layered_from_paths(&paths, overrides, defaults).unwrap()
    }

    fn get<'a>(table: &'a toml::Table, path: &str) -> Option<&'a toml::Value> {
        let mut parts = path.split('.');
        let mut value = table.get(parts.next()?)?;
        for part in parts {
            value = value.get(part)?;
        }
        Some(value)
    }

    #[test]
    fn discovered_files_merge_deeply_in_precedence_order() {
        let (root, environment) = layout();
        let user = root.path().join("xdg/bcode/bcode.toml");
        let repo = root.path().join("repo/bcode.toml");
        let dot = root.path().join("repo/.bcode/bcode.toml");
        fs::write(&user, "[a]\nx = 1\ny = 1\n[a.deep]\nkeep = true\n").unwrap();
        fs::write(&repo, "[a]\ny = 2\n[a.deep]\nadded = 1\n").unwrap();
        fs::write(&dot, "[a]\nz = 3\n").unwrap();

        let layered = load(
            &environment,
            &ConfigLoadOverrides::default(),
            toml::Table::new(),
        );

        assert_eq!(layered.files, vec![user, repo, dot]);
        assert_eq!(get(&layered.value, "a.x"), Some(&toml::Value::Integer(1)));
        assert_eq!(get(&layered.value, "a.y"), Some(&toml::Value::Integer(2)));
        assert_eq!(get(&layered.value, "a.z"), Some(&toml::Value::Integer(3)));
        assert_eq!(
            get(&layered.value, "a.deep.keep"),
            Some(&toml::Value::Boolean(true))
        );
        assert_eq!(
            get(&layered.value, "a.deep.added"),
            Some(&toml::Value::Integer(1))
        );
    }

    #[test]
    fn environment_file_and_inline_layer_above_discovered_files() {
        let (root, mut environment) = layout();
        let repo = root.path().join("repo/bcode.toml");
        let extra = root.path().join("extra.toml");
        fs::write(&repo, "[a]\nfrom_repo = true\nv = \"repo\"\n").unwrap();
        fs::write(&extra, "[a]\nv = \"env-file\"\nw = \"env-file\"\n").unwrap();
        environment.vars.insert(
            BCODE_CONFIG_ENV.to_owned(),
            extra.to_string_lossy().into_owned(),
        );
        environment.vars.insert(
            BCODE_CONFIG_TOML_ENV.to_owned(),
            "[a]\nw = \"inline\"\n".to_owned(),
        );
        let overrides = ConfigLoadOverrides::from_config_environment_with_cli(
            &environment,
            None,
            Some("[a]\ncli = 1\n".to_owned()),
        );

        let layered = load(&environment, &overrides, toml::Table::new());

        assert_eq!(layered.files, vec![repo, extra]);
        assert_eq!(
            get(&layered.value, "a.from_repo"),
            Some(&toml::Value::Boolean(true))
        );
        assert_eq!(
            get(&layered.value, "a.v"),
            Some(&toml::Value::String("env-file".to_owned()))
        );
        assert_eq!(
            get(&layered.value, "a.w"),
            Some(&toml::Value::String("inline".to_owned()))
        );
        assert_eq!(get(&layered.value, "a.cli"), Some(&toml::Value::Integer(1)));
    }

    #[test]
    fn missing_environment_file_is_an_error() {
        let (root, mut environment) = layout();
        environment.vars.insert(
            BCODE_CONFIG_ENV.to_owned(),
            root.path()
                .join("absent.toml")
                .to_string_lossy()
                .into_owned(),
        );
        let overrides =
            ConfigLoadOverrides::from_config_environment_with_cli(&environment, None, None);
        let paths = default_config_paths_with_environment(&environment);

        assert!(matches!(
            load_layered_from_paths(&paths, &overrides, toml::Table::new()),
            Err(LayersError::Composition { .. })
        ));
    }

    #[test]
    fn exclusive_file_ignores_discovered_and_environment_layers() {
        let (root, environment) = layout();
        fs::write(root.path().join("repo/bcode.toml"), "discovered = true\n").unwrap();
        let exclusive = root.path().join("only.toml");
        fs::write(&exclusive, "only = true\n").unwrap();
        let overrides = ConfigLoadOverrides {
            exclusive_config_path: Some(exclusive.clone()),
            env_config_toml: Some("env = true\n".to_owned()),
            cli_config_toml: Some("cli = true\n".to_owned()),
            ..ConfigLoadOverrides::default()
        };

        let layered = load(&environment, &overrides, toml::Table::new());

        assert_eq!(layered.files, vec![exclusive]);
        assert!(layered.value.contains_key("only"));
        assert!(layered.value.contains_key("cli"));
        assert!(!layered.value.contains_key("discovered"));
        assert!(!layered.value.contains_key("env"));
    }

    #[test]
    fn active_profile_inherits_parents_and_applies_before_config() {
        let (root, environment) = layout();
        fs::write(
            root.path().join("repo/bcode.toml"),
            r#"
            [a]
            config = "config"

            [composition]
            active_profile = " Child "

            [composition.profiles.base.patch.a]
            base = "base"
            config = "base"
            shared = "base"

            [composition.profiles.child]
            extends = ["BASE"]
            [composition.profiles.child.patch.a]
            shared = "child"
            config = "child"
            "#,
        )
        .unwrap();
        let defaults = toml::Table::from_iter([(
            "a".to_owned(),
            toml::Value::Table(toml::Table::from_iter([
                ("default".to_owned(), toml::Value::Boolean(true)),
                ("base".to_owned(), toml::Value::String("default".to_owned())),
            ])),
        )]);

        let layered = load(&environment, &ConfigLoadOverrides::default(), defaults);

        assert_eq!(
            layered.resolution,
            CompositionResolution {
                selected_profile: Some("child".to_owned()),
                layer_order: vec![
                    "defaults".to_owned(),
                    "profile:active".to_owned(),
                    "config".to_owned()
                ],
                available_profiles: vec!["base".to_owned(), "child".to_owned()],
            }
        );
        assert!(!layered.value.contains_key("composition"));
        let string = |value: &str| Some(toml::Value::String(value.to_owned()));
        assert_eq!(
            get(&layered.value, "a.default").cloned(),
            Some(toml::Value::Boolean(true))
        );
        assert_eq!(get(&layered.value, "a.base").cloned(), string("base"));
        assert_eq!(get(&layered.value, "a.shared").cloned(), string("child"));
        assert_eq!(get(&layered.value, "a.config").cloned(), string("config"));
    }

    #[test]
    fn explicit_layer_order_places_profile_above_config() {
        let raw: toml::Value = toml::from_str(
            r#"
            value = "config"
            [composition]
            layer_order = ["defaults", "config", "profile:late"]
            [composition.profiles.late.patch]
            value = "late"
            "#,
        )
        .unwrap();

        let (value, resolution) = resolve_composition(&raw, toml::Table::new()).unwrap();

        assert_eq!(
            value.get("value").and_then(toml::Value::as_str),
            Some("late")
        );
        assert_eq!(resolution.selected_profile, None);
    }

    #[test]
    fn invalid_composition_is_rejected() {
        for raw in [
            "[composition]\nlayer_order = [\"profile:active\"]\n",
            "[composition]\nlayer_order = [\"bogus\"]\n",
            "[composition]\nactive_profile = \"missing\"\n",
            "[composition.profiles.a]\nextends = [\"b\"]\n[composition.profiles.b]\nextends = [\"a\"]\n[composition]\nactive_profile = \"a\"\n",
        ] {
            let raw: toml::Value = toml::from_str(raw).unwrap();
            assert!(resolve_composition(&raw, toml::Table::new()).is_err());
        }
    }

    #[test]
    fn transform_runs_once_per_file_and_not_for_inline_layers() {
        let (root, environment) = layout();
        let user = root.path().join("xdg/bcode/bcode.toml");
        let repo = root.path().join("repo/bcode.toml");
        fs::write(&user, "seen = []\n").unwrap();
        fs::write(&repo, "other = 1\n").unwrap();
        let overrides = ConfigLoadOverrides {
            cli_config_toml: Some("inline = true\n".to_owned()),
            ..ConfigLoadOverrides::default()
        };
        let mut calls = Vec::new();

        let raw = load_raw_layers(
            &default_config_paths_with_environment(&environment),
            &overrides,
            |path, value| {
                calls.push(path.to_owned());
                value
                    .as_table_mut()
                    .unwrap()
                    .insert("source".to_owned(), path.to_string_lossy().as_ref().into());
                Ok::<(), LayersError>(())
            },
        )
        .unwrap();

        assert_eq!(calls, vec![user, repo.clone()]);
        assert_eq!(
            raw.value.get("source").and_then(toml::Value::as_str),
            Some(repo.to_string_lossy().as_ref())
        );
        assert!(raw.value.get("inline").is_some());
    }

    #[test]
    fn transform_errors_pass_through() {
        #[derive(Debug)]
        enum Failure {
            Hook,
            Layers,
        }
        impl From<LayersError> for Failure {
            fn from(_: LayersError) -> Self {
                Self::Layers
            }
        }
        let (root, environment) = layout();
        fs::write(root.path().join("repo/bcode.toml"), "a = 1\n").unwrap();

        let result = load_raw_layers(
            &default_config_paths_with_environment(&environment),
            &ConfigLoadOverrides::default(),
            |_, _| Err(Failure::Hook),
        );

        assert!(matches!(result, Err(Failure::Hook)));
    }

    #[test]
    fn model_profile_environment_is_appended_to_inline_toml() {
        let environment = TestEnvironment {
            vars: BTreeMap::from([
                (BCODE_CONFIG_TOML_ENV.to_owned(), "a = 1".to_owned()),
                (BCODE_MODEL_PROFILE_ENV.to_owned(), "fast \"x\"".to_owned()),
            ]),
            current_dir: PathBuf::from("."),
        };
        let overrides =
            ConfigLoadOverrides::from_config_environment_with_cli(&environment, None, None);
        let value: toml::Table =
            toml::from_str(overrides.env_config_toml.as_deref().unwrap()).unwrap();

        assert_eq!(value.get("a"), Some(&toml::Value::Integer(1)));
        assert_eq!(
            get(&value, "model.profile").and_then(toml::Value::as_str),
            Some("fast \"x\"")
        );
    }

    #[test]
    fn origins_name_the_layer_and_profile_that_set_each_value() {
        let write = |dir: &Path, name: &str, text: &str| {
            let path = dir.join(name);
            fs::write(&path, text).unwrap();
            path
        };
        let dir = tempfile::tempdir().unwrap();
        let user = write(
            dir.path(),
            "user.toml",
            "[a]\nx = 1\n[composition]\nactive_profile = \"p\"\n\
             [composition.profiles.p.patch.a]\ny = 2\n",
        );
        let project = write(dir.path(), "project.toml", "[a]\nz = 3\n");
        let overrides = ConfigLoadOverrides {
            env_config_toml: Some("[a]\nw = 4\n".into()),
            ..ConfigLoadOverrides::default()
        };
        let layered = load_layered_from_paths(
            &[user.clone(), project.clone()],
            &overrides,
            toml::Table::new(),
        )
        .unwrap();
        let origin = |keys: &[&str]| {
            layered
                .origin(&keys.iter().map(|key| (*key).to_owned()).collect::<Vec<_>>())
                .map(|origin| origin.to_string())
        };
        assert_eq!(origin(&["a", "x"]), Some(user.display().to_string()));
        assert_eq!(origin(&["a", "z"]), Some(project.display().to_string()));
        assert_eq!(
            origin(&["a", "y"]),
            Some(format!("{} (profile p)", user.display()))
        );
        assert_eq!(origin(&["a", "w"]).as_deref(), Some("environment TOML"));
    }
}
