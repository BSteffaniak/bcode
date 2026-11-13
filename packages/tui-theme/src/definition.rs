//! Versioned declarative theme definitions and deterministic resolution.
//!
//! This module is terminal-owned. It intentionally exposes no application,
//! session, provider, or tool implementation types.

use std::collections::{BTreeMap, BTreeSet};

use bcode_presentation_policy_models::{LayerSource, PresentationSection, RuleLayer};
use bmux_tui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::chrome::{RawChrome, TuiBorders, TuiChrome};
use crate::contrast::ContrastAdjustment;
use crate::expr::{self, ColorContext, Evaluated};

/// Semantic style roles every version-1 theme must resolve through definition or inheritance.
pub const REQUIRED_SEMANTIC_STYLE_ROLES: &[&str] = &[
    "canvas",
    "text.primary",
    "text.muted",
    "border.default",
    "border.focused",
    "state.info",
    "state.success",
    "state.warning",
    "state.error",
    "selection.active",
    "markdown.text",
    "markdown.heading",
    "markdown.link",
    "markdown.inline_code",
    "markdown.code_block_text",
    "markdown.code_block_border",
    "markdown.blockquote_bar",
    "markdown.alert_note",
    "markdown.alert_tip",
    "markdown.alert_important",
    "markdown.alert_warning",
    "markdown.alert_caution",
    "markdown.list_marker",
    "markdown.task_checked",
    "markdown.task_unchecked",
    "markdown.table_border",
    "markdown.horizontal_rule",
    "syntax.text",
    "syntax.comment",
    "syntax.keyword",
    "syntax.function",
    "syntax.variable",
    "syntax.string",
    "syntax.number",
    "syntax.type",
    "syntax.operator",
    "syntax.punctuation",
    "transcript.user.label",
    "transcript.assistant.label",
    "transcript.reasoning.label",
    "tool.requested.title",
    "tool.running.title",
    "tool.waiting.title",
    "tool.succeeded.title",
    "tool.failed.title",
    "tool.cancelled.title",
    "tool.timed_out.title",
];

/// Semantic container roles every version-1 theme must resolve through definition or inheritance.
pub const REQUIRED_CONTAINER_ROLES: &[&str] = &[
    "transcript.user",
    "transcript.assistant",
    "tool.requested",
    "tool.running",
    "tool.waiting",
    "tool.succeeded",
    "tool.failed",
    "tool.cancelled",
    "tool.timed_out",
];

/// Newest theme definition schema version supported by this build.
///
/// Version 2 adds palette expressions, seed derivation, `[chrome]`, and
/// `[presentation]`. Version-1 files keep their exact version-1 semantics.
pub const THEME_SCHEMA_VERSION: u32 = 2;
/// Oldest theme definition schema version supported by this build.
pub const MIN_THEME_SCHEMA_VERSION: u32 = 1;
/// Maximum accepted UTF-8 bytes in one theme file.
pub const MAX_THEME_FILE_BYTES: usize = 256 * 1024;
/// Maximum palette definitions in one theme.
pub const MAX_THEME_PALETTE_ENTRIES: usize = 256;
/// Maximum style roles in one theme.
pub const MAX_THEME_STYLE_ENTRIES: usize = 512;
/// Maximum container recipes in one theme.
pub const MAX_THEME_CONTAINER_ENTRIES: usize = 64;
/// Maximum direct parent themes or configured overlays.
pub const MAX_THEME_LAYERS: usize = 16;
/// Maximum recursive inheritance depth.
pub const MAX_THEME_INHERITANCE_DEPTH: usize = 16;
/// Maximum variable-reference depth.
pub const MAX_THEME_REFERENCE_DEPTH: usize = 32;

/// Declarative theme parsing or resolution failure.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ThemeError {
    /// A theme file exceeded the byte limit.
    #[error("theme {source_name} exceeds the {MAX_THEME_FILE_BYTES}-byte limit")]
    FileTooLarge { source_name: String },
    /// TOML could not be decoded.
    #[error("theme {source_name} is invalid TOML: {message}")]
    InvalidToml {
        source_name: String,
        message: String,
    },
    /// The external schema version is unsupported.
    #[error("theme {source_name} uses unsupported schema version {version}")]
    UnsupportedVersion { source_name: String, version: u32 },
    /// A required stable id is invalid.
    #[error("theme {source_name} has invalid id {id:?}")]
    InvalidId { source_name: String, id: String },
    /// A bounded collection exceeded its limit.
    #[error("theme {source_name} has too many {field} entries (maximum {maximum})")]
    LimitExceeded {
        source_name: String,
        field: &'static str,
        maximum: usize,
    },
    /// A requested theme was not present in the catalog.
    #[error("theme {id:?} was not found")]
    MissingTheme { id: String },
    /// Theme inheritance contains a cycle.
    #[error("theme inheritance cycle: {chain}")]
    InheritanceCycle { chain: String },
    /// Theme inheritance exceeded its bounded depth.
    #[error("theme inheritance exceeds depth {MAX_THEME_INHERITANCE_DEPTH}: {chain}")]
    InheritanceTooDeep { chain: String },
    /// A palette reference could not be found.
    #[error("theme {theme} references unknown palette value {reference:?}")]
    UnknownColorReference { theme: String, reference: String },
    /// Palette references contain a cycle.
    #[error("theme {theme} has palette reference cycle: {chain}")]
    ColorReferenceCycle { theme: String, chain: String },
    /// A color value is malformed or unsupported.
    #[error("theme {theme} has invalid color {value:?}")]
    InvalidColor { theme: String, value: String },
    /// A color expression is malformed.
    #[error("theme {theme} has invalid color expression {value:?}: {message}")]
    InvalidExpression {
        theme: String,
        value: String,
        message: String,
    },
    /// A section requires a newer schema version than the file declares.
    #[error("theme {source_name} uses [{section}], which requires schema_version = 2")]
    RequiresSchemaV2 {
        source_name: String,
        section: &'static str,
    },
}

/// What a theme definition is for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeKind {
    /// A complete selectable theme or palette.
    #[default]
    Theme,
    /// An overlay that sets chrome, structure, and containers but no colors.
    Profile,
    /// A structural building block for other themes; not selectable alone.
    Base,
}

/// Which resolved roles are raised to their contrast pairings.
///
/// Resolution measures every pair in [`crate::contrast::PAIRS`] and, for an
/// adjustable role that fails, changes only the OKLCH lightness of the failing
/// color by the minimum needed (see [`crate::contrast`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContrastPolicy {
    /// Raise only roles filled by seed derivation; authored colors are kept
    /// exactly as written.
    #[default]
    Derived,
    /// Also raise authored roles that fail their pair.
    Enforce,
}

/// Non-fatal problem found while resolving a theme.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeWarning {
    /// Theme id the warning concerns.
    pub theme: String,
    /// Human-readable message.
    pub message: String,
}

/// Light/dark branch selected while resolving a theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedThemeVariant {
    /// Do not apply a variant patch.
    Unspecified,
    /// Apply the dark variant patch when present.
    Dark,
    /// Apply the light variant patch when present.
    Light,
}

/// One parsed, validated version-1 theme definition.
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeDefinition {
    source_name: String,
    source: String,
    raw: RawTheme,
}

impl ThemeDefinition {
    /// Return the original bounded TOML source.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Return the stable theme id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.raw.id
    }

    /// Return the human-readable display name, falling back to the stable id.
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.raw
            .display_name
            .as_deref()
            .unwrap_or_else(|| self.id())
    }

    /// Return the optional one-line description shown in pickers.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.raw.description.as_deref()
    }

    /// Return the diagnostic source name.
    #[must_use]
    pub fn source_name(&self) -> &str {
        &self.source_name
    }

    /// Return what this definition is for.
    #[must_use]
    pub const fn kind(&self) -> ThemeKind {
        self.raw.kind
    }

    /// Return the declared schema version.
    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.raw.schema_version
    }

    /// Return whether this definition provides a dark variant patch.
    #[must_use]
    pub const fn has_dark_variant(&self) -> bool {
        self.raw.variants.dark.is_some()
    }

    /// Return whether this definition provides a light variant patch.
    #[must_use]
    pub const fn has_light_variant(&self) -> bool {
        self.raw.variants.light.is_some()
    }
}

/// Parse and structurally validate one bounded theme definition.
///
/// # Errors
///
/// Returns an error for oversized input, malformed TOML, unsupported schema
/// versions, invalid ids, or collection limits.
pub fn parse_theme_definition(
    source_name: impl Into<String>,
    source: &str,
) -> Result<ThemeDefinition, ThemeError> {
    let source_name = source_name.into();
    if source.len() > MAX_THEME_FILE_BYTES {
        return Err(ThemeError::FileTooLarge { source_name });
    }
    let raw = toml::from_str::<RawTheme>(source).map_err(|error| ThemeError::InvalidToml {
        source_name: source_name.clone(),
        message: error.to_string(),
    })?;
    validate_raw_theme(&source_name, &raw)?;
    Ok(ThemeDefinition {
        source_name,
        source: source.to_owned(),
        raw,
    })
}

/// Requested base theme, overlays, and resolved terminal variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeSelection {
    /// Stable base theme id.
    pub base: String,
    /// Ordered overlay theme ids, applied from left to right.
    pub overlays: Vec<String>,
    /// Already-resolved light/dark variant.
    pub variant: ResolvedThemeVariant,
}

impl ThemeSelection {
    /// Select one base theme without overlays or a variant patch.
    #[must_use]
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            overlays: Vec::new(),
            variant: ResolvedThemeVariant::Unspecified,
        }
    }

    /// Return this selection with ordered overlays.
    #[must_use]
    pub fn overlays(mut self, overlays: impl Into<Vec<String>>) -> Self {
        self.overlays = overlays.into();
        self
    }

    /// Return this selection with a resolved variant.
    #[must_use]
    pub const fn variant(mut self, variant: ResolvedThemeVariant) -> Self {
        self.variant = variant;
        self
    }
}

/// Validated theme definitions keyed by stable id.
#[derive(Debug, Clone, Default)]
pub struct ThemeCatalog {
    definitions: BTreeMap<String, ThemeDefinition>,
    role_namespaces: Vec<String>,
}

impl ThemeCatalog {
    /// Create an empty catalog.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            definitions: BTreeMap::new(),
            role_namespaces: Vec::new(),
        }
    }

    /// Create the catalog of themes embedded with Bcode.
    ///
    /// # Errors
    ///
    /// Returns an error if an embedded theme no longer satisfies the runtime
    /// schema. Callers should treat this as a build/release defect.
    pub fn bundled() -> Result<Self, ThemeError> {
        let mut catalog = Self::new();
        for (id, source) in crate::bundled::BUNDLED_THEMES {
            catalog.insert(parse_theme_definition(format!("builtin:{id}"), source)?);
        }
        Ok(catalog)
    }

    /// Iterate definitions in stable id order.
    pub fn definitions(&self) -> impl Iterator<Item = &ThemeDefinition> {
        self.definitions.values()
    }

    /// Return one definition by stable id.
    #[must_use]
    pub fn definition(&self, id: &str) -> Option<&ThemeDefinition> {
        self.definitions.get(id)
    }

    /// Return a bundled definition's original TOML source.
    #[must_use]
    pub fn bundled_source(id: &str) -> Option<&'static str> {
        crate::bundled::BUNDLED_THEMES
            .iter()
            .find(|(bundled, _)| *bundled == id)
            .map(|(_, source)| *source)
    }

    /// Register a plugin-owned role namespace so its roles are not reported as unknown.
    pub fn register_role_namespace(&mut self, namespace: impl Into<String>) {
        let namespace = namespace.into();
        if !self.role_namespaces.contains(&namespace) {
            self.role_namespaces.push(namespace);
        }
    }

    /// Insert or replace one definition by stable id.
    pub fn insert(&mut self, definition: ThemeDefinition) -> Option<ThemeDefinition> {
        self.definitions
            .insert(definition.id().to_owned(), definition)
    }

    /// Resolve one base plus ordered overlays into concrete terminal styles.
    ///
    /// # Errors
    ///
    /// Returns an error for missing layers, inheritance cycles/depth, excessive
    /// overlays, unresolved variables, or invalid colors.
    pub fn resolve(
        &self,
        selection: &ThemeSelection,
    ) -> Result<ResolvedThemeDefinition, ThemeError> {
        if selection.overlays.len() > MAX_THEME_LAYERS {
            return Err(ThemeError::LimitExceeded {
                source_name: "theme selection".to_owned(),
                field: "overlay",
                maximum: MAX_THEME_LAYERS,
            });
        }
        let mut merged = MergedTheme::default();
        let mut applied = Vec::new();
        let mut completed = BTreeSet::new();
        self.merge_theme(
            &selection.base,
            selection.variant,
            &mut Vec::new(),
            &mut completed,
            &mut merged,
            &mut applied,
        )?;
        for overlay in &selection.overlays {
            self.merge_theme(
                overlay,
                selection.variant,
                &mut Vec::new(),
                &mut completed,
                &mut merged,
                &mut applied,
            )?;
        }
        resolve_merged(&selection.base, merged, applied, &self.role_namespaces)
    }

    fn merge_theme(
        &self,
        id: &str,
        variant: ResolvedThemeVariant,
        stack: &mut Vec<String>,
        completed: &mut BTreeSet<String>,
        merged: &mut MergedTheme,
        applied: &mut Vec<String>,
    ) -> Result<(), ThemeError> {
        if completed.contains(id) {
            return Ok(());
        }
        if stack.len() >= MAX_THEME_INHERITANCE_DEPTH {
            let mut chain = stack.clone();
            chain.push(id.to_owned());
            return Err(ThemeError::InheritanceTooDeep {
                chain: chain.join(" -> "),
            });
        }
        if let Some(position) = stack.iter().position(|candidate| candidate == id) {
            let mut chain = stack[position..].to_vec();
            chain.push(id.to_owned());
            return Err(ThemeError::InheritanceCycle {
                chain: chain.join(" -> "),
            });
        }
        let definition = self
            .definitions
            .get(id)
            .ok_or_else(|| ThemeError::MissingTheme { id: id.to_owned() })?;
        stack.push(id.to_owned());
        for parent in &definition.raw.extends {
            self.merge_theme(parent, variant, stack, completed, merged, applied)?;
        }
        stack.pop();
        merged.apply_raw(&definition.raw);
        if definition.raw.schema_version >= 2 {
            merged.derive = true;
        }
        if let Some(patch) = match variant {
            ResolvedThemeVariant::Unspecified => None,
            ResolvedThemeVariant::Dark => definition.raw.variants.dark.as_ref(),
            ResolvedThemeVariant::Light => definition.raw.variants.light.as_ref(),
        } {
            merged.apply_patch(id, patch);
        }
        applied.push(id.to_owned());
        completed.insert(id.to_owned());
        Ok(())
    }
}

/// Fully resolved declarative theme data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedThemeDefinition {
    /// Requested base id.
    pub id: String,
    /// Resolved display name.
    pub display_name: String,
    /// Palette values after reference resolution.
    pub palette: BTreeMap<String, Color>,
    /// Concrete styles keyed by semantic role.
    pub styles: BTreeMap<String, Style>,
    /// Bounded container recipes keyed by semantic role.
    pub containers: BTreeMap<String, ContainerRecipe>,
    /// Plugin-owned extension values, deep-merged by namespace.
    pub extensions: BTreeMap<String, toml::Value>,
    /// Resolved terminal chrome.
    pub chrome: TuiChrome,
    /// Opaque chrome sections for other renderers, keyed by renderer.
    pub renderer_chrome: BTreeMap<String, toml::Value>,
    /// Structural presentation rule layers, one per contributing theme, in order.
    pub presentation: Vec<RuleLayer>,
    /// Roles filled by seed derivation rather than authored values.
    pub derived_roles: Vec<String>,
    /// Colors raised to meet their contrast pair, in pair-table order.
    pub contrast_adjustments: Vec<ContrastAdjustment>,
    /// Non-fatal problems such as unknown roles or approximate color math.
    pub warnings: Vec<ThemeWarning>,
    /// Theme ids applied in resolution order.
    pub applied_layers: Vec<String>,
    /// Stable fingerprint of all resolved presentation fields.
    pub fingerprint: u64,
}

impl ResolvedThemeDefinition {
    /// An empty resolution with no palette, roles, chrome, or rules.
    ///
    /// Consumers use it as a deterministic fallback when no theme resolves.
    #[must_use]
    pub fn empty(id: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display_name: display_name.into(),
            palette: BTreeMap::new(),
            styles: BTreeMap::new(),
            containers: BTreeMap::new(),
            extensions: BTreeMap::new(),
            chrome: TuiChrome::default(),
            renderer_chrome: BTreeMap::new(),
            presentation: Vec::new(),
            derived_roles: Vec::new(),
            contrast_adjustments: Vec::new(),
            warnings: Vec::new(),
            applied_layers: Vec::new(),
            fingerprint: 0,
        }
    }

    /// Resolve a palette color by name.
    #[must_use]
    pub fn color(&self, name: &str) -> Option<Color> {
        self.palette.get(name).copied()
    }

    /// Resolve a semantic style role.
    #[must_use]
    pub fn style(&self, role: &str) -> Option<Style> {
        self.styles.get(role).copied()
    }
}

/// Constrained transcript/container layout recipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContainerRecipe {
    /// Container layout family.
    pub layout: ContainerLayout,
    /// Width/fill behavior.
    pub width: ContainerWidth,
    /// Border placement.
    pub border: ContainerBorder,
    /// Horizontal padding in terminal cells.
    pub padding_x: u16,
    /// Vertical padding in terminal cells.
    pub padding_y: u16,
}

/// Supported container layout families.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerLayout {
    /// No dedicated container chrome.
    #[default]
    Plain,
    /// One status-colored leading bar.
    LeftBar,
    /// Bordered/background panel.
    Panel,
}

/// Supported container width behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerWidth {
    /// Paint only content width.
    #[default]
    Content,
    /// Paint the full available width.
    Full,
}

/// Supported container border placement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerBorder {
    /// No border.
    #[default]
    None,
    /// Left border only.
    Left,
    /// Single-line border on all sides.
    All,
    /// Horizontal rules above and below.
    Rules,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTheme {
    schema_version: u32,
    id: String,
    #[serde(default)]
    kind: ThemeKind,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    extends: Vec<String>,
    #[serde(default)]
    contrast: Option<ContrastPolicy>,
    #[serde(default)]
    palette: BTreeMap<String, RawColor>,
    #[serde(default)]
    styles: BTreeMap<String, RawStyle>,
    #[serde(default)]
    containers: BTreeMap<String, RawContainerRecipe>,
    #[serde(default)]
    variants: RawVariants,
    #[serde(default)]
    extensions: BTreeMap<String, toml::Value>,
    #[serde(default)]
    chrome: Option<RawChrome>,
    #[serde(default)]
    presentation: Option<PresentationSection>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVariants {
    #[serde(default)]
    dark: Option<RawThemePatch>,
    #[serde(default)]
    light: Option<RawThemePatch>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawThemePatch {
    #[serde(default)]
    palette: BTreeMap<String, RawColor>,
    #[serde(default)]
    styles: BTreeMap<String, RawStyle>,
    #[serde(default)]
    containers: BTreeMap<String, RawContainerRecipe>,
    #[serde(default)]
    extensions: BTreeMap<String, toml::Value>,
    #[serde(default)]
    chrome: Option<RawChrome>,
    #[serde(default)]
    presentation: Option<PresentationSection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub(crate) enum RawColor {
    Text(String),
    Index(i64),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStyle {
    #[serde(default)]
    fg: Option<RawColor>,
    #[serde(default)]
    bg: Option<RawColor>,
    #[serde(default)]
    modifiers: Vec<RawModifier>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawModifier {
    Bold,
    Dim,
    Italic,
    Underline,
    SlowBlink,
    Reversed,
    Hidden,
    CrossedOut,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawContainerRecipe {
    #[serde(default)]
    layout: ContainerLayout,
    #[serde(default)]
    width: ContainerWidth,
    #[serde(default)]
    border: ContainerBorder,
    #[serde(default)]
    padding_x: u16,
    #[serde(default)]
    padding_y: u16,
}

#[derive(Debug, Clone, Default)]
struct MergedTheme {
    display_name: Option<String>,
    palette: BTreeMap<String, RawColor>,
    styles: BTreeMap<String, RawStyle>,
    containers: BTreeMap<String, RawContainerRecipe>,
    extensions: BTreeMap<String, toml::Value>,
    chrome: RawChrome,
    presentation: Vec<RuleLayer>,
    /// Whether any applied layer opted into schema-v2 seed derivation.
    derive: bool,
    /// Contrast policy of the last layer that set one.
    contrast: ContrastPolicy,
}

impl MergedTheme {
    fn apply_raw(&mut self, raw: &RawTheme) {
        if raw.display_name.is_some() {
            self.display_name.clone_from(&raw.display_name);
        }
        if let Some(contrast) = raw.contrast {
            self.contrast = contrast;
        }
        self.palette.extend(raw.palette.clone());
        self.styles.extend(raw.styles.clone());
        self.containers.extend(raw.containers.clone());
        deep_merge_extensions(&mut self.extensions, &raw.extensions);
        if let Some(chrome) = &raw.chrome {
            self.chrome.merge(chrome);
        }
        self.push_presentation(&raw.id, raw.presentation.as_ref());
    }

    fn apply_patch(&mut self, id: &str, patch: &RawThemePatch) {
        self.palette.extend(patch.palette.clone());
        self.styles.extend(patch.styles.clone());
        self.containers.extend(patch.containers.clone());
        deep_merge_extensions(&mut self.extensions, &patch.extensions);
        if let Some(chrome) = &patch.chrome {
            self.chrome.merge(chrome);
        }
        self.push_presentation(id, patch.presentation.as_ref());
    }

    fn push_presentation(&mut self, id: &str, section: Option<&PresentationSection>) {
        if let Some(section) = section.filter(|section| !section.rules.is_empty()) {
            self.presentation.push(RuleLayer {
                source: LayerSource::Theme(id.to_owned()),
                rules: section.rules.clone(),
            });
        }
    }
}

fn validate_raw_theme(source: &str, raw: &RawTheme) -> Result<(), ThemeError> {
    if !(MIN_THEME_SCHEMA_VERSION..=THEME_SCHEMA_VERSION).contains(&raw.schema_version) {
        return Err(ThemeError::UnsupportedVersion {
            source_name: source.to_owned(),
            version: raw.schema_version,
        });
    }
    if raw.schema_version < 2 && raw.kind != ThemeKind::Theme {
        return Err(ThemeError::RequiresSchemaV2 {
            source_name: source.to_owned(),
            section: "kind",
        });
    }
    if raw.schema_version < 2 {
        let patches = [raw.variants.dark.as_ref(), raw.variants.light.as_ref()];
        let uses = |chrome: bool| {
            (if chrome {
                raw.chrome.is_some()
            } else {
                raw.presentation.is_some()
            }) || patches.iter().flatten().any(|patch| {
                if chrome {
                    patch.chrome.is_some()
                } else {
                    patch.presentation.is_some()
                }
            })
        };
        for (section, chrome) in [("chrome", true), ("presentation", false)] {
            if uses(chrome) {
                return Err(ThemeError::RequiresSchemaV2 {
                    source_name: source.to_owned(),
                    section,
                });
            }
        }
    }
    let expression_values = raw.palette.values().chain(
        raw.styles
            .values()
            .flat_map(|style| [&style.fg, &style.bg].into_iter().flatten()),
    );
    if raw.schema_version < 2
        && expression_values
            .into_iter()
            .any(|value| matches!(value, RawColor::Text(text) if expr::is_expression(text)))
    {
        return Err(ThemeError::RequiresSchemaV2 {
            source_name: source.to_owned(),
            section: "palette expressions",
        });
    }
    let presentation_rules = raw
        .presentation
        .as_ref()
        .map_or(0, |section| section.rules.len());
    if presentation_rules > bcode_presentation_policy_models::MAX_RULES_PER_LAYER {
        return Err(limit_error(
            source,
            "presentation rule",
            bcode_presentation_policy_models::MAX_RULES_PER_LAYER,
        ));
    }
    if !valid_theme_id(&raw.id) {
        return Err(ThemeError::InvalidId {
            source_name: source.to_owned(),
            id: raw.id.clone(),
        });
    }
    if raw.extends.len() > MAX_THEME_LAYERS {
        return Err(limit_error(source, "parent theme", MAX_THEME_LAYERS));
    }
    validate_patch_sizes(source, &raw.palette, &raw.styles, &raw.containers)?;
    for patch in [raw.variants.dark.as_ref(), raw.variants.light.as_ref()]
        .into_iter()
        .flatten()
    {
        validate_patch_sizes(source, &patch.palette, &patch.styles, &patch.containers)?;
    }
    Ok(())
}

fn validate_patch_sizes(
    source: &str,
    palette: &BTreeMap<String, RawColor>,
    styles: &BTreeMap<String, RawStyle>,
    containers: &BTreeMap<String, RawContainerRecipe>,
) -> Result<(), ThemeError> {
    if palette.len() > MAX_THEME_PALETTE_ENTRIES {
        return Err(limit_error(source, "palette", MAX_THEME_PALETTE_ENTRIES));
    }
    if styles.len() > MAX_THEME_STYLE_ENTRIES {
        return Err(limit_error(source, "style", MAX_THEME_STYLE_ENTRIES));
    }
    if containers.len() > MAX_THEME_CONTAINER_ENTRIES {
        return Err(limit_error(
            source,
            "container",
            MAX_THEME_CONTAINER_ENTRIES,
        ));
    }
    Ok(())
}

fn limit_error(source: &str, field: &'static str, maximum: usize) -> ThemeError {
    ThemeError::LimitExceeded {
        source_name: source.to_owned(),
        field,
        maximum,
    }
}

fn valid_theme_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn resolve_merged(
    id: &str,
    merged: MergedTheme,
    applied_layers: Vec<String>,
    role_namespaces: &[String],
) -> Result<ResolvedThemeDefinition, ThemeError> {
    let mut resolver = PaletteResolver::new(id, &merged.palette);
    for name in merged.palette.keys() {
        resolver.get(name)?;
    }
    let mut styles = BTreeMap::new();
    for (role, raw) in &merged.styles {
        styles.insert(role.clone(), resolver.style(raw)?);
    }
    let mut derived_roles = Vec::new();
    if merged.derive {
        for spec in crate::roles::ROLES {
            if styles.contains_key(spec.key) {
                continue;
            }
            if let Some(derived) = spec.derive.and_then(|derive| resolver.derived(derive)) {
                styles.insert(spec.key.to_owned(), derived);
                derived_roles.push(spec.key.to_owned());
            }
        }
    }
    let contrast_adjustments = match merged.contrast {
        ContrastPolicy::Enforce => crate::contrast::enforce(&mut styles, |_| true),
        ContrastPolicy::Derived => crate::contrast::enforce(&mut styles, |role| {
            derived_roles.iter().any(|derived| derived == role)
        }),
    };
    let mut warnings = std::mem::take(&mut resolver.warnings);
    for role in merged.styles.keys() {
        if !crate::roles::is_known_role(role, role_namespaces) {
            let hint = crate::roles::suggestion(role)
                .map(|suggestion| format!("; did you mean {suggestion:?}?"))
                .unwrap_or_default();
            warnings.push(ThemeWarning {
                theme: id.to_owned(),
                message: format!("unknown style role {role:?}{hint}"),
            });
        }
    }
    let chrome = resolve_chrome(&mut resolver, merged.chrome.tui.as_ref())?;
    let palette = resolver.resolved;
    let containers = merged
        .containers
        .into_iter()
        .map(|(role, raw)| {
            (
                role,
                ContainerRecipe {
                    layout: raw.layout,
                    width: raw.width,
                    border: raw.border,
                    padding_x: raw.padding_x,
                    padding_y: raw.padding_y,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let display_name = merged.display_name.unwrap_or_else(|| id.to_owned());
    let fingerprint = theme_fingerprint(&[
        id,
        &display_name,
        &format!("{palette:?}"),
        &format!("{styles:?}"),
        &format!("{containers:?}"),
        &format!("{:?}", merged.extensions),
        &format!("{chrome:?}"),
        &format!("{:?}", merged.chrome.other),
        &format!("{:?}", merged.presentation),
        &format!("{applied_layers:?}"),
    ]);
    Ok(ResolvedThemeDefinition {
        id: id.to_owned(),
        display_name,
        palette,
        styles,
        containers,
        extensions: merged.extensions,
        chrome,
        renderer_chrome: merged.chrome.other,
        presentation: merged.presentation,
        derived_roles,
        contrast_adjustments,
        warnings,
        applied_layers,
        fingerprint,
    })
}

fn resolve_chrome(
    resolver: &mut PaletteResolver<'_>,
    raw: Option<&crate::chrome::RawTuiChrome>,
) -> Result<TuiChrome, ThemeError> {
    let Some(raw) = raw else {
        return Ok(TuiChrome::default());
    };
    let borders = raw.borders.clone().unwrap_or_default();
    let focus = raw.focus.clone().unwrap_or_default();
    let colors = focus.colors.unwrap_or_default();
    if colors.len() > crate::chrome::MAX_FOCUS_COLORS {
        return Err(ThemeError::LimitExceeded {
            source_name: resolver.theme.to_owned(),
            field: "focus color",
            maximum: crate::chrome::MAX_FOCUS_COLORS,
        });
    }
    let focus_colors = colors
        .iter()
        .map(|color| resolver.authored(color))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(TuiChrome {
        borders: TuiBorders {
            panel: borders.panel,
            dialog: borders.dialog,
            tool: borders.tool,
            composer: borders.composer,
        },
        focus: focus.effect.unwrap_or_default(),
        focus_colors,
        icons: raw.icons.unwrap_or_default(),
        density: raw.density.unwrap_or_default(),
        separators: raw
            .separators
            .as_ref()
            .and_then(|separators| separators.status)
            .unwrap_or_default(),
        speaker: raw.speaker.unwrap_or_default(),
        surfaces: raw.surfaces.unwrap_or_default(),
        motion: raw.motion.unwrap_or_default(),
        header: raw.header.unwrap_or_default(),
        session_start: raw.session_start.unwrap_or_default(),
        status: resolved(raw.status.as_ref()),
        composer: resolved(raw.composer.as_ref()),
        markers: resolved(raw.markers.as_ref()),
        follow: raw.follow.unwrap_or_default(),
        activity: resolved(raw.activity.as_ref()),
        markdown: resolved(raw.markdown.as_ref()),
        dialogs: resolved(raw.dialogs.as_ref()),
        menus: resolved(raw.menus.as_ref()),
    })
}

/// A nested chrome table resolved with defaults for its unset fields; absent, the defaults.
fn resolved<R: crate::chrome::ResolveChrome>(raw: Option<&R>) -> R::Resolved {
    raw.map_or_else(R::Resolved::default, R::resolve)
}

/// Resolves palette references and color expressions with cycle detection.
struct PaletteResolver<'a> {
    theme: &'a str,
    raw: &'a BTreeMap<String, RawColor>,
    resolved: BTreeMap<String, Color>,
    stack: Vec<String>,
    warnings: Vec<ThemeWarning>,
    /// First structured error raised inside expression evaluation.
    failure: Option<ThemeError>,
}

impl<'a> PaletteResolver<'a> {
    const fn new(theme: &'a str, raw: &'a BTreeMap<String, RawColor>) -> Self {
        Self {
            theme,
            raw,
            resolved: BTreeMap::new(),
            stack: Vec::new(),
            warnings: Vec::new(),
            failure: None,
        }
    }

    /// Resolve an existing palette entry.
    fn get(&mut self, name: &str) -> Result<Color, ThemeError> {
        self.lookup(name)?
            .ok_or_else(|| ThemeError::UnknownColorReference {
                theme: self.theme.to_owned(),
                reference: name.to_owned(),
            })
    }

    /// Resolve a palette entry, or `None` when it is not defined.
    fn lookup(&mut self, name: &str) -> Result<Option<Color>, ThemeError> {
        if let Some(color) = self.resolved.get(name) {
            return Ok(Some(*color));
        }
        let Some(raw) = self.raw.get(name) else {
            return Ok(None);
        };
        if self.stack.len() >= MAX_THEME_REFERENCE_DEPTH
            || self.stack.iter().any(|entry| entry == name)
        {
            let mut chain = self.stack.clone();
            chain.push(name.to_owned());
            return Err(ThemeError::ColorReferenceCycle {
                theme: self.theme.to_owned(),
                chain: chain.join(" -> "),
            });
        }
        self.stack.push(name.to_owned());
        let color = self.authored(raw);
        self.stack.pop();
        let color = color?;
        self.resolved.insert(name.to_owned(), color);
        Ok(Some(color))
    }

    /// Resolve an authored value. Approximate math is accepted with a warning.
    fn authored(&mut self, raw: &RawColor) -> Result<Color, ThemeError> {
        match raw {
            RawColor::Text(value) if expr::is_expression(value) => match self.expression(value)? {
                Evaluated::Exact(color) => Ok(color),
                Evaluated::Approximate(color, note) => {
                    self.warnings.push(ThemeWarning {
                        theme: self.theme.to_owned(),
                        message: format!("{value}: {note}"),
                    });
                    Ok(color)
                }
                Evaluated::Missing(reference) => Err(ThemeError::UnknownColorReference {
                    theme: self.theme.to_owned(),
                    reference,
                }),
            },
            RawColor::Text(value) if value.starts_with('$') => {
                self.get(value.trim_start_matches('$'))
            }
            _ => parse_direct_color(self.theme, raw),
        }
    }

    fn expression(&mut self, value: &str) -> Result<Evaluated, ThemeError> {
        let parsed = expr::parse(value).map_err(|message| ThemeError::InvalidExpression {
            theme: self.theme.to_owned(),
            value: value.to_owned(),
            message,
        })?;
        let result = expr::evaluate(&parsed, self);
        if let Some(failure) = self.failure.take() {
            return Err(failure);
        }
        result.map_err(|message| ThemeError::InvalidExpression {
            theme: self.theme.to_owned(),
            value: value.to_owned(),
            message,
        })
    }

    fn style(&mut self, raw: &RawStyle) -> Result<Style, ThemeError> {
        let mut style = Style::new();
        if let Some(fg) = &raw.fg {
            style = style.fg(self.authored(fg)?);
        }
        if let Some(bg) = &raw.bg {
            style = style.bg(self.authored(bg)?);
        }
        Ok(apply_modifiers(style, &raw.modifiers))
    }

    /// Derive a role from seeds; `None` unless every input resolves exactly.
    fn derived(&mut self, derive: crate::roles::DerivedStyle) -> Option<Style> {
        let mut exact = |value: &str| match self.expression_or_reference(value) {
            Ok(Evaluated::Exact(color)) => Some(color),
            _ => None,
        };
        let mut style = Style::new();
        if let Some(fg) = derive.fg {
            style = style.fg(exact(fg)?);
        }
        if let Some(bg) = derive.bg {
            style = style.bg(exact(bg)?);
        }
        let modifiers = derive
            .modifiers
            .iter()
            .filter_map(|name| modifier_by_name(name))
            .collect::<Vec<_>>();
        Some(apply_modifiers(style, &modifiers))
    }

    fn expression_or_reference(&mut self, value: &str) -> Result<Evaluated, ThemeError> {
        if let Some(reference) = value.strip_prefix('$') {
            return Ok(self.lookup(reference)?.map_or_else(
                || Evaluated::Missing(reference.to_owned()),
                Evaluated::Exact,
            ));
        }
        self.expression(value)
    }
}

impl ColorContext for PaletteResolver<'_> {
    fn reference(&mut self, name: &str) -> Result<Option<Color>, String> {
        self.lookup(name).map_err(|error| {
            let message = error.to_string();
            self.failure.get_or_insert(error);
            message
        })
    }

    fn literal(&mut self, value: &str) -> Result<Color, String> {
        parse_direct_color(self.theme, &RawColor::Text(value.to_owned()))
            .map_err(|error| error.to_string())
    }
}

fn apply_modifiers(mut style: Style, modifiers: &[RawModifier]) -> Style {
    for modifier in modifiers {
        style = style.add_modifier(match modifier {
            RawModifier::Bold => Modifier::BOLD,
            RawModifier::Dim => Modifier::DIM,
            RawModifier::Italic => Modifier::ITALIC,
            RawModifier::Underline => Modifier::UNDERLINE,
            RawModifier::SlowBlink => Modifier::SLOW_BLINK,
            RawModifier::Reversed => Modifier::REVERSED,
            RawModifier::Hidden => Modifier::HIDDEN,
            RawModifier::CrossedOut => Modifier::CROSSED_OUT,
        });
    }
    style
}

fn modifier_by_name(name: &str) -> Option<RawModifier> {
    Some(match name {
        "bold" => RawModifier::Bold,
        "dim" => RawModifier::Dim,
        "italic" => RawModifier::Italic,
        "underline" => RawModifier::Underline,
        "slow_blink" => RawModifier::SlowBlink,
        "reversed" => RawModifier::Reversed,
        "hidden" => RawModifier::Hidden,
        "crossed_out" => RawModifier::CrossedOut,
        _ => return None,
    })
}

fn parse_direct_color(theme: &str, raw: &RawColor) -> Result<Color, ThemeError> {
    match raw {
        RawColor::Index(index) => {
            u8::try_from(*index)
                .map(Color::Indexed)
                .map_err(|_| ThemeError::InvalidColor {
                    theme: theme.to_owned(),
                    value: index.to_string(),
                })
        }
        RawColor::Text(value) if value == "terminal" || value == "default" => Ok(Color::Default),
        RawColor::Text(value) if value.starts_with('#') => parse_hex(theme, value),
        RawColor::Text(value) if value.starts_with("ansi:") => parse_ansi(theme, value),
        RawColor::Text(value) => Err(ThemeError::InvalidColor {
            theme: theme.to_owned(),
            value: value.clone(),
        }),
    }
}

fn parse_hex(theme: &str, value: &str) -> Result<Color, ThemeError> {
    let hex = value.strip_prefix('#').unwrap_or_default();
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ThemeError::InvalidColor {
            theme: theme.to_owned(),
            value: value.to_owned(),
        });
    }
    let parse = |range| u8::from_str_radix(&hex[range], 16);
    let (Ok(red), Ok(green), Ok(blue)) = (parse(0..2), parse(2..4), parse(4..6)) else {
        return Err(ThemeError::InvalidColor {
            theme: theme.to_owned(),
            value: value.to_owned(),
        });
    };
    Ok(Color::Rgb(red, green, blue))
}

fn parse_ansi(theme: &str, value: &str) -> Result<Color, ThemeError> {
    let color = match value.strip_prefix("ansi:").unwrap_or_default() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "bright_black" => Color::BrightBlack,
        "bright_red" => Color::BrightRed,
        "bright_green" => Color::BrightGreen,
        "bright_yellow" => Color::BrightYellow,
        "bright_blue" => Color::BrightBlue,
        "bright_magenta" => Color::BrightMagenta,
        "bright_cyan" => Color::BrightCyan,
        "bright_white" => Color::BrightWhite,
        _ => {
            return Err(ThemeError::InvalidColor {
                theme: theme.to_owned(),
                value: value.to_owned(),
            });
        }
    };
    Ok(color)
}

pub(crate) fn deep_merge_extensions(
    target: &mut BTreeMap<String, toml::Value>,
    source: &BTreeMap<String, toml::Value>,
) {
    for (key, value) in source {
        match (target.get_mut(key), value) {
            (Some(toml::Value::Table(target)), toml::Value::Table(source)) => {
                deep_merge_table(target, source);
            }
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

fn deep_merge_table(target: &mut toml::Table, source: &toml::Table) {
    for (key, value) in source {
        match (target.get_mut(key), value) {
            (Some(toml::Value::Table(target)), toml::Value::Table(source)) => {
                deep_merge_table(target, source);
            }
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

fn theme_fingerprint(parts: &[&str]) -> u64 {
    let mut hash = Fnv64::new();
    for part in parts {
        hash.add(part.as_bytes());
    }
    hash.finish()
}

struct Fnv64(u64);

impl Fnv64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    const fn new() -> Self {
        Self(Self::OFFSET)
    }

    fn add(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
        self.0 ^= 0xff;
        self.0 = self.0.wrapping_mul(Self::PRIME);
    }

    const fn finish(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use crate::bundled::BUNDLED_THEMES;
    use bmux_tui::style::{Color, Modifier};

    use super::{
        ContainerBorder, ContainerLayout, ContainerWidth, REQUIRED_CONTAINER_ROLES,
        REQUIRED_SEMANTIC_STYLE_ROLES, ResolvedThemeDefinition, ResolvedThemeVariant, ThemeCatalog,
        ThemeError, ThemeSelection, parse_theme_definition,
    };

    const BASE: &str = r##"
schema_version = 1
id = "base"
display_name = "Base"

[palette]
text = "terminal"
accent = "#112233"
muted = 244
alias = "$accent"

[styles."text.primary"]
fg = "$text"

[styles."accent"]
fg = "$alias"
modifiers = ["bold"]

[containers.tool]
layout = "plain"

[variants.light.palette]
accent = "#445566"
"##;

    const OVERLAY: &str = r##"
schema_version = 1
id = "structured"
extends = ["base"]

[styles."tool.failed"]
fg = "ansi:red"
bg = "#220000"

[containers.tool]
layout = "panel"
width = "full"
border = "all"
padding_x = 1

[extensions."bcode.shell"]
indicator = "exit-code"
"##;

    #[test]
    fn parses_resolves_references_variants_and_container_recipes() {
        let mut catalog = ThemeCatalog::new();
        catalog.insert(parse_theme_definition("base.toml", BASE).expect("base parses"));
        catalog.insert(parse_theme_definition("structured.toml", OVERLAY).expect("overlay parses"));

        let resolved = catalog
            .resolve(
                &ThemeSelection::new("base")
                    .overlays(vec!["structured".to_owned()])
                    .variant(ResolvedThemeVariant::Light),
            )
            .expect("theme resolves");

        assert_eq!(resolved.color("text"), Some(Color::Default));
        assert_eq!(resolved.color("accent"), Some(Color::Rgb(68, 85, 102)));
        assert_eq!(resolved.color("alias"), Some(Color::Rgb(68, 85, 102)));
        assert_eq!(
            resolved.style("tool.failed").and_then(|style| style.fg),
            Some(Color::Red)
        );
        assert!(
            resolved
                .style("accent")
                .expect("accent style")
                .modifiers
                .contains(Modifier::BOLD)
        );
        assert_eq!(
            resolved.containers["tool"],
            super::ContainerRecipe {
                layout: ContainerLayout::Panel,
                width: ContainerWidth::Full,
                border: ContainerBorder::All,
                padding_x: 1,
                padding_y: 0,
            }
        );
        assert_eq!(resolved.applied_layers, ["base", "structured"]);
        assert_ne!(resolved.fingerprint, 0);
    }

    #[test]
    fn rejects_unknown_fields_versions_and_invalid_colors() {
        let unknown = "schema_version = 1\nid = \"x\"\nunknown = true\n";
        assert!(matches!(
            parse_theme_definition("unknown.toml", unknown),
            Err(ThemeError::InvalidToml { .. })
        ));
        assert!(matches!(
            parse_theme_definition("future.toml", "schema_version = 3\nid = \"x\"\n"),
            Err(ThemeError::UnsupportedVersion { version: 3, .. })
        ));

        let invalid = "schema_version = 1\nid = \"bad\"\n[palette]\nx = \"not-a-color\"\n";
        let mut catalog = ThemeCatalog::new();
        catalog.insert(parse_theme_definition("bad.toml", invalid).expect("structure parses"));
        assert!(matches!(
            catalog.resolve(&ThemeSelection::new("bad")),
            Err(ThemeError::InvalidColor { .. })
        ));
    }

    #[test]
    fn rejects_inheritance_and_palette_cycles() {
        let mut catalog = ThemeCatalog::new();
        catalog.insert(
            parse_theme_definition(
                "a.toml",
                "schema_version = 1\nid = \"a\"\nextends = [\"b\"]\n",
            )
            .expect("a parses"),
        );
        catalog.insert(
            parse_theme_definition(
                "b.toml",
                "schema_version = 1\nid = \"b\"\nextends = [\"a\"]\n",
            )
            .expect("b parses"),
        );
        assert!(matches!(
            catalog.resolve(&ThemeSelection::new("a")),
            Err(ThemeError::InheritanceCycle { .. })
        ));

        let mut palette = ThemeCatalog::new();
        palette.insert(
            parse_theme_definition(
                "palette.toml",
                "schema_version = 1\nid = \"palette\"\n[palette]\na = \"$b\"\nb = \"$a\"\n",
            )
            .expect("palette parses"),
        );
        assert!(matches!(
            palette.resolve(&ThemeSelection::new("palette")),
            Err(ThemeError::ColorReferenceCycle { .. })
        ));
    }

    /// Every bundled theme alone and combined with every bundled profile.
    fn theme_and_profile_selections(catalog: &ThemeCatalog) -> Vec<ThemeSelection> {
        let profiles: Vec<_> = catalog
            .definitions()
            .filter(|definition| definition.kind() == super::ThemeKind::Profile)
            .map(|definition| definition.id().to_owned())
            .collect();
        assert!(profiles.len() >= 10, "{profiles:?}");
        catalog
            .definitions()
            .filter(|definition| definition.kind() == super::ThemeKind::Theme)
            .flat_map(|definition| {
                let id = definition.id().to_owned();
                std::iter::once(ThemeSelection::new(id.clone())).chain(profiles.iter().map(
                    move |profile| ThemeSelection::new(id.clone()).overlays(vec![profile.clone()]),
                ))
            })
            .collect()
    }

    #[test]
    fn bundled_catalog_resolves_every_required_role_and_recipe() {
        let catalog = ThemeCatalog::bundled().expect("bundled themes parse");
        let selections = theme_and_profile_selections(&catalog);
        for variant in [ResolvedThemeVariant::Dark, ResolvedThemeVariant::Light] {
            for selection in &selections {
                let selection = selection.clone().variant(variant);
                let id = format!("{} + {:?}", selection.base, selection.overlays);
                let resolved = catalog
                    .resolve(&selection)
                    .unwrap_or_else(|error| panic!("bundled theme {id:?} resolves: {error}"));
                let missing_styles = REQUIRED_SEMANTIC_STYLE_ROLES
                    .iter()
                    .copied()
                    .filter(|role| !resolved.styles.contains_key(*role))
                    .collect::<Vec<_>>();
                let missing_containers = REQUIRED_CONTAINER_ROLES
                    .iter()
                    .copied()
                    .filter(|role| !resolved.containers.contains_key(*role))
                    .collect::<Vec<_>>();
                assert!(
                    missing_styles.is_empty(),
                    "bundled theme {id:?} is missing semantic styles: {missing_styles:?}"
                );
                assert!(
                    missing_containers.is_empty(),
                    "bundled theme {id:?} is missing container recipes: {missing_containers:?}"
                );
                assert!(
                    resolved.warnings.is_empty(),
                    "bundled theme {id:?} has warnings: {:?}",
                    resolved.warnings
                );
            }
        }
    }

    fn json_round_trip(resolved: &ResolvedThemeDefinition) -> ResolvedThemeDefinition {
        let encoded = serde_json::to_string(resolved).expect("resolved theme serializes");
        serde_json::from_str(&encoded).expect("resolved theme deserializes")
    }

    #[test]
    fn every_bundled_resolution_round_trips_through_json() {
        let catalog = ThemeCatalog::bundled().expect("bundled themes parse");
        let mut adjusted = 0;
        for variant in [ResolvedThemeVariant::Dark, ResolvedThemeVariant::Light] {
            for selection in theme_and_profile_selections(&catalog) {
                let selection = selection.variant(variant);
                let resolved = catalog.resolve(&selection).expect("bundled theme resolves");
                adjusted += resolved.contrast_adjustments.len();
                assert_eq!(
                    json_round_trip(&resolved),
                    resolved,
                    "{} + {:?} ({variant:?})",
                    selection.base,
                    selection.overlays
                );
            }
        }
        assert!(
            adjusted > 0,
            "bundled themes must exercise contrast adjustments"
        );
    }

    #[test]
    fn authored_sections_round_trip_through_json() {
        let source = r##"
schema_version = 2
id = "wire"
extends = ["nord"]
contrast = "enforce"

[styles."bordr.focussed"]
fg = "ansi:red"

[styles."tool.failed"]
fg = 196
bg = "#220000"
modifiers = ["bold", "italic", "crossed_out"]

[containers.tool]
layout = "panel"
width = "full"
border = "all"
padding_x = 2
padding_y = 1

[extensions."bcode.shell"]
indicator = "exit-code"
ratio = 0.375
limits = [1, 2, 3]
released = 2024-01-02T03:04:05Z
nested = { enabled = true, label = "x" }

[chrome.tui]
borders = { panel = "rounded", dialog = "thick", tool = "left", composer = "double" }
focus = { effect = "sweep", colors = ["#ff00aa", "#00ffff"] }
icons = "nerd"

[chrome.web]
radius = 4
font = { family = "mono" }

[[presentation.rules]]
match = { kind = "tool" }
set = { disclosure = "expanded", detail = "verbose" }
"##;
        let mut catalog = ThemeCatalog::bundled().expect("bundled themes parse");
        catalog.insert(parse_theme_definition("wire.toml", source).expect("wire theme parses"));
        let resolved = catalog
            .resolve(&ThemeSelection::new("wire").variant(ResolvedThemeVariant::Dark))
            .expect("wire theme resolves");
        assert_ne!(resolved.warnings, []);
        assert_ne!(resolved.contrast_adjustments, []);
        assert_ne!(resolved.presentation, []);
        assert!(resolved.renderer_chrome.contains_key("web"));
        assert!(resolved.extensions.contains_key("bcode.shell"));
        let decoded = json_round_trip(&resolved);
        assert_eq!(decoded, resolved);
        for (role, style) in &resolved.styles {
            assert_eq!(decoded.style(role), Some(*style), "style role {role}");
        }
        for (role, recipe) in &resolved.containers {
            assert_eq!(
                decoded.containers.get(role),
                Some(recipe),
                "container {role}"
            );
        }
        assert_eq!(decoded.fingerprint, resolved.fingerprint);
    }

    #[test]
    fn contrast_adjustments_reject_roles_outside_the_pair_table() {
        let catalog = ThemeCatalog::bundled().expect("bundled themes parse");
        let resolved = catalog
            .resolve(&ThemeSelection::new("nord").variant(ResolvedThemeVariant::Dark))
            .expect("nord resolves");
        let mut encoded = serde_json::to_value(&resolved).expect("serializes");
        let adjustment = &mut encoded["contrast_adjustments"][0];
        assert!(adjustment.is_object(), "nord has contrast adjustments");
        adjustment["role"] = "not.a.pair".into();
        let error = serde_json::from_value::<ResolvedThemeDefinition>(encoded)
            .expect_err("unknown pair role is rejected");
        assert!(error.to_string().contains("not.a.pair"), "{error}");
    }

    #[test]
    fn bundled_catalog_exercises_supported_container_recipe_families() {
        let catalog = ThemeCatalog::bundled().expect("bundled themes parse");
        let resolved = catalog
            .definitions()
            .filter(|definition| definition.kind() == super::ThemeKind::Theme)
            .map(|definition| {
                catalog
                    .resolve(&ThemeSelection::new(definition.id()))
                    .expect("bundled theme resolves")
            })
            .collect::<Vec<_>>();
        for layout in [
            ContainerLayout::Plain,
            ContainerLayout::LeftBar,
            ContainerLayout::Panel,
        ] {
            assert!(
                resolved.iter().any(|theme| theme
                    .containers
                    .values()
                    .any(|recipe| recipe.layout == layout)),
                "bundled catalog does not exercise {layout:?}"
            );
        }
        for width in [ContainerWidth::Content, ContainerWidth::Full] {
            assert!(
                resolved.iter().any(|theme| theme
                    .containers
                    .values()
                    .any(|recipe| recipe.width == width)),
                "bundled catalog does not exercise {width:?}"
            );
        }
        for border in [
            ContainerBorder::None,
            ContainerBorder::Left,
            ContainerBorder::All,
        ] {
            assert!(
                resolved.iter().any(|theme| theme
                    .containers
                    .values()
                    .any(|recipe| recipe.border == border)),
                "bundled catalog does not exercise {border:?}"
            );
        }
    }

    #[test]
    fn bundled_ids_copy_parse_and_future_schemas_fail_closed() {
        let catalog = ThemeCatalog::bundled().expect("bundled themes parse");
        let expected = crate::bundled::BUNDLED_THEMES
            .iter()
            .map(|(id, _)| *id)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            catalog
                .definitions()
                .map(super::ThemeDefinition::id)
                .collect::<std::collections::BTreeSet<_>>(),
            expected
        );
        for id in expected {
            let source = ThemeCatalog::bundled_source(id)
                .unwrap_or_else(|| panic!("{id} has copyable bundled source"));
            let copied = parse_theme_definition(format!("copied:{id}"), source)
                .unwrap_or_else(|error| panic!("copied {id} parses: {error}"));
            assert_eq!(copied.id(), id);
        }
        assert!(ThemeCatalog::bundled_source("unknown-theme").is_none());
        assert!(matches!(
            parse_theme_definition("future.toml", "schema_version = 3\nid = \"future\"\n"),
            Err(ThemeError::UnsupportedVersion { version: 3, .. })
        ));
    }

    #[test]
    fn bundled_themes_use_the_runtime_loader() {
        let catalog = ThemeCatalog::bundled().expect("bundled themes parse");
        assert_eq!(catalog.definitions.len(), BUNDLED_THEMES.len());
        let native_definition = catalog
            .definitions
            .get("terminal-native")
            .expect("native definition");
        assert_eq!(
            native_definition.raw.display_name.as_deref(),
            Some("Terminal Native")
        );
        assert_eq!(native_definition.source_name, "builtin:terminal-native");
        let native = catalog
            .resolve(&ThemeSelection::new("terminal-native"))
            .expect("terminal-native resolves");
        assert_eq!(native.color("text"), Some(Color::Default));
        assert_eq!(native.color("background"), Some(Color::Default));
        assert_eq!(native.style("canvas").and_then(|style| style.bg), None);
        assert_eq!(
            native.style("markdown.text").and_then(|style| style.fg),
            Some(Color::Default)
        );

        let structured = catalog
            .resolve(&ThemeSelection::new("terminal-native-structured"))
            .expect("structured resolves");
        assert_eq!(
            structured.containers["tool.succeeded"].layout,
            ContainerLayout::Panel
        );

        let dark = catalog
            .resolve(&ThemeSelection::new("bcode-dark"))
            .expect("dark resolves");
        assert_eq!(
            dark.style("canvas").and_then(|style| style.bg),
            Some(Color::Rgb(11, 16, 32))
        );
        assert!(
            dark.style("diff.added_row")
                .and_then(|style| style.bg)
                .is_some()
        );

        let light = catalog
            .resolve(&ThemeSelection::new("bcode-light"))
            .expect("light resolves");
        assert_eq!(
            light.style("canvas").and_then(|style| style.bg),
            Some(Color::Rgb(248, 250, 252))
        );

        let adaptive_dark = catalog
            .resolve(&ThemeSelection::new("bcode").variant(ResolvedThemeVariant::Dark))
            .expect("adaptive dark resolves");
        assert_eq!(
            adaptive_dark.style("canvas"),
            dark.style("canvas"),
            "adaptive dark must preserve bcode-dark compatibility presentation"
        );
        assert_eq!(
            adaptive_dark.styles, dark.styles,
            "adaptive dark must preserve every compatibility semantic style"
        );
        assert_eq!(
            adaptive_dark.containers, dark.containers,
            "adaptive dark must preserve every compatibility container"
        );

        let adaptive_light = catalog
            .resolve(&ThemeSelection::new("bcode").variant(ResolvedThemeVariant::Light))
            .expect("adaptive light resolves");
        assert_eq!(
            adaptive_light.style("canvas"),
            light.style("canvas"),
            "adaptive light must preserve bcode-light compatibility presentation"
        );
        assert_eq!(
            adaptive_light.styles, light.styles,
            "adaptive light must preserve every compatibility semantic style"
        );
        assert_eq!(
            adaptive_light.containers, light.containers,
            "adaptive light must preserve every compatibility container"
        );
        assert!(catalog.definition("bcode").is_some_and(
            |definition| definition.has_dark_variant() && definition.has_light_variant()
        ));

        let monochrome = catalog
            .resolve(&ThemeSelection::new("monochrome"))
            .expect("monochrome resolves");
        assert!(
            monochrome
                .style("tool.failed.title")
                .is_some_and(|style| style.modifiers.contains(Modifier::REVERSED))
        );

        let high_contrast = catalog
            .resolve(&ThemeSelection::new("high-contrast"))
            .expect("high contrast resolves");
        assert_eq!(
            high_contrast
                .style("state.error")
                .and_then(|style| style.fg),
            Some(Color::BrightRed)
        );
    }
}

#[cfg(test)]
mod schema_v2_tests {
    use bmux_tui::style::{Color, Modifier};

    use super::{
        ResolvedThemeVariant, ThemeCatalog, ThemeError, ThemeSelection, parse_theme_definition,
    };
    use crate::chrome::{BorderStyle, FocusEffect, IconSet};

    const SEEDED: &str = r##"
schema_version = 2
id = "seeded"
display_name = "Seeded"

[palette]
background = "#1a1b26"
text = "#c0caf5"
accent = "#7aa2f7"
red = "#f7768e"
green = "#9ece6a"
yellow = "#e0af68"
blue = "#7aa2f7"
magenta = "#bb9af7"
cyan = "#7dcfff"
tint = "mix($accent, $background, 0.2)"

[styles."state.error"]
fg = "#ff0000"
"##;

    fn resolve(sources: &[&str], base: &str) -> super::ResolvedThemeDefinition {
        let mut catalog = ThemeCatalog::bundled().expect("bundled");
        for source in sources {
            catalog.insert(parse_theme_definition("test.toml", source).expect("parses"));
        }
        catalog
            .resolve(&ThemeSelection::new(base))
            .expect("resolves")
    }

    fn rgb(color: Option<Color>) -> bmux_color::Rgb {
        color.and_then(Color::rgb).expect("explicit rgb")
    }

    #[test]
    fn seeds_derive_every_required_role_with_readable_contrast() {
        let resolved = resolve(&[SEEDED], "seeded");
        for role in super::REQUIRED_SEMANTIC_STYLE_ROLES {
            assert!(resolved.style(role).is_some(), "{role} missing");
        }
        let background = rgb(resolved.color("background"));
        for role in [
            "text.primary",
            "state.info",
            "state.success",
            "state.warning",
            "syntax.keyword",
            "markdown.link",
        ] {
            let fg = rgb(resolved.style(role).and_then(|style| style.fg));
            assert!(fg.contrast_ratio(background) >= 4.5, "{role}");
        }
        let muted = rgb(resolved.style("text.muted").and_then(|style| style.fg));
        assert!(muted.contrast_ratio(background) >= 3.0);
        // Authored roles win over derivation.
        assert_eq!(
            resolved.style("state.error").and_then(|style| style.fg),
            Some(Color::Rgb(255, 0, 0))
        );
        assert!(
            !resolved
                .derived_roles
                .iter()
                .any(|role| role == "state.error")
        );
        assert!(resolved.derived_roles.iter().any(|role| role == "canvas"));
        assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
        assert!(
            resolved
                .style("tool.failed.title")
                .is_some_and(|style| style.modifiers.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn palette_expressions_resolve_through_references() {
        let resolved = resolve(&[SEEDED], "seeded");
        let tint = rgb(resolved.color("tint"));
        let expected = bmux_color::mix(
            bmux_color::Rgb::new(0x7a, 0xa2, 0xf7),
            bmux_color::Rgb::new(0x1a, 0x1b, 0x26),
            0.2,
        );
        assert_eq!(tint, expected);
    }

    #[test]
    fn terminal_seeds_skip_derivation_instead_of_guessing() {
        let native_v2 = r#"
schema_version = 2
id = "native-v2"
extends = ["terminal-native"]

[palette]
soft = "mix($accent, $text, 0.5)"
"#;
        let resolved = resolve(&[native_v2], "native-v2");
        // Inherited v1 roles remain; math over terminal colors warns and keeps the first color.
        assert_eq!(resolved.color("soft"), Some(Color::Cyan));
        assert_eq!(resolved.warnings.len(), 1, "{:?}", resolved.warnings);
        assert!(resolved.warnings[0].message.contains("terminal-owned"));
        // Roles needing RGB math were not invented.
        assert!(resolved.style("surface.raised").is_none());
    }

    #[test]
    fn unknown_roles_warn_with_suggestions() {
        let typo = r#"
schema_version = 2
id = "typo"
extends = ["terminal-native"]

[styles."bordr.focussed"]
fg = "ansi:red"

[styles."git.branch.head"]
fg = "ansi:green"
"#;
        let resolved = resolve(&[typo], "typo");
        let messages: Vec<_> = resolved
            .warnings
            .iter()
            .map(|warning| warning.message.as_str())
            .collect();
        assert_eq!(
            messages,
            ["unknown style role \"bordr.focussed\"; did you mean \"border.focused\"?"]
        );
    }

    #[test]
    fn chrome_and_presentation_resolve_through_inheritance_and_variants() {
        let parent = r##"
schema_version = 2
id = "riced"
extends = ["terminal-native"]

[chrome.tui]
borders = { panel = "rounded", dialog = "thick" }
focus = { effect = "gradient", colors = ["#ff00aa", "#00ffff"] }
icons = "nerd"

[[presentation.rules]]
match = { kind = "tool" }
set = { disclosure = "expanded", detail = "verbose" }

[variants.light.chrome.tui]
icons = "unicode"
"##;
        let child = r#"
schema_version = 2
id = "riced-ascii"
extends = ["riced"]

[chrome.tui]
borders = { panel = "ascii" }

[[presentation.rules]]
match = { tool = "read" }
set = { disclosure = "collapsed" }
"#;
        let mut catalog = ThemeCatalog::bundled().expect("bundled");
        for source in [parent, child] {
            catalog.insert(parse_theme_definition("test.toml", source).expect("parses"));
        }
        let resolved = catalog
            .resolve(&ThemeSelection::new("riced-ascii").variant(ResolvedThemeVariant::Light))
            .expect("resolves");
        assert_eq!(resolved.chrome.borders.panel, Some(BorderStyle::Ascii));
        assert_eq!(resolved.chrome.borders.dialog, Some(BorderStyle::Thick));
        assert_eq!(resolved.chrome.borders.tool, None);
        assert_eq!(resolved.chrome.focus, FocusEffect::Gradient);
        assert_eq!(
            resolved.chrome.focus_colors,
            [Color::Rgb(255, 0, 170), Color::Rgb(0, 255, 255)]
        );
        assert_eq!(resolved.chrome.icons, IconSet::Unicode);
        let sources: Vec<_> = resolved
            .presentation
            .iter()
            .map(|layer| layer.source.clone())
            .collect();
        assert_eq!(
            sources,
            [
                bcode_presentation_policy_models::LayerSource::Theme("riced".into()),
                bcode_presentation_policy_models::LayerSource::Theme("riced-ascii".into()),
            ]
        );
    }

    #[test]
    fn version_one_files_cannot_use_version_two_sections() {
        for (source, section) in [
            (
                "schema_version = 1\nid = \"x\"\n[chrome.tui]\nicons = \"nerd\"\n",
                "chrome",
            ),
            (
                "schema_version = 1\nid = \"x\"\n[[presentation.rules]]\nset = { detail = \"minimal\" }\n",
                "presentation",
            ),
            (
                "schema_version = 1\nid = \"x\"\n[palette]\na = \"mix($b, $c, 0.5)\"\n",
                "palette expressions",
            ),
            (
                "schema_version = 1\nid = \"x\"\n[variants.dark.chrome.tui]\nicons = \"nerd\"\n",
                "chrome",
            ),
        ] {
            assert!(
                matches!(
                    parse_theme_definition("v1.toml", source),
                    Err(ThemeError::RequiresSchemaV2 { section: found, .. }) if found == section
                ),
                "{source}"
            );
        }
    }

    #[test]
    fn malformed_expressions_and_cycles_fail_closed() {
        let bad = "schema_version = 2\nid = \"bad\"\n[palette]\na = \"mix($a, $a, 0.5)\"\n";
        let mut catalog = ThemeCatalog::new();
        catalog.insert(parse_theme_definition("bad.toml", bad).expect("parses"));
        assert!(matches!(
            catalog.resolve(&ThemeSelection::new("bad")),
            Err(ThemeError::ColorReferenceCycle { .. })
        ));
        let syntax =
            "schema_version = 2\nid = \"syntax\"\n[palette]\na = \"mix(#000000, #ffffff)\"\n";
        let mut catalog = ThemeCatalog::new();
        catalog.insert(parse_theme_definition("syntax.toml", syntax).expect("parses"));
        assert!(matches!(
            catalog.resolve(&ThemeSelection::new("syntax")),
            Err(ThemeError::InvalidExpression { .. })
        ));
    }
}
