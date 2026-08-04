//! Versioned declarative theme definitions and deterministic resolution.
//!
//! This module is terminal-owned. It intentionally exposes no application,
//! session, provider, or tool implementation types.

use std::collections::{BTreeMap, BTreeSet};

use bmux_tui::style::{Color, Modifier, Style};
use serde::Deserialize;
use thiserror::Error;

/// Theme definition schema version supported by this build.
pub const THEME_SCHEMA_VERSION: u32 = 1;
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
    raw: RawTheme,
}

impl ThemeDefinition {
    /// Return the stable theme id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.raw.id
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
    Ok(ThemeDefinition { source_name, raw })
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
}

impl ThemeCatalog {
    /// Create an empty catalog.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            definitions: BTreeMap::new(),
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
        catalog.insert(parse_theme_definition(
            "builtin:terminal-native",
            include_str!("../../themes/terminal-native.toml"),
        )?);
        catalog.insert(parse_theme_definition(
            "builtin:terminal-native-structured",
            include_str!("../../themes/terminal-native-structured.toml"),
        )?);
        Ok(catalog)
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
        resolve_merged(&selection.base, merged, applied)
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
        if let Some(patch) = match variant {
            ResolvedThemeVariant::Unspecified => None,
            ResolvedThemeVariant::Dark => definition.raw.variants.dark.as_ref(),
            ResolvedThemeVariant::Light => definition.raw.variants.light.as_ref(),
        } {
            merged.apply_patch(patch);
        }
        applied.push(id.to_owned());
        completed.insert(id.to_owned());
        Ok(())
    }
}

/// Fully resolved declarative theme data.
#[derive(Debug, Clone, PartialEq)]
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
    /// Theme ids applied in resolution order.
    pub applied_layers: Vec<String>,
    /// Stable fingerprint of all resolved presentation fields.
    pub fingerprint: u64,
}

impl ResolvedThemeDefinition {
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize)]
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerWidth {
    /// Paint only content width.
    #[default]
    Content,
    /// Paint the full available width.
    Full,
}

/// Supported container border placement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerBorder {
    /// No border.
    #[default]
    None,
    /// Left border only.
    Left,
    /// Single-line border on all sides.
    All,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTheme {
    schema_version: u32,
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    extends: Vec<String>,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum RawColor {
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
}

impl MergedTheme {
    fn apply_raw(&mut self, raw: &RawTheme) {
        if raw.display_name.is_some() {
            self.display_name.clone_from(&raw.display_name);
        }
        self.palette.extend(raw.palette.clone());
        self.styles.extend(raw.styles.clone());
        self.containers.extend(raw.containers.clone());
        deep_merge_extensions(&mut self.extensions, &raw.extensions);
    }

    fn apply_patch(&mut self, patch: &RawThemePatch) {
        self.palette.extend(patch.palette.clone());
        self.styles.extend(patch.styles.clone());
        self.containers.extend(patch.containers.clone());
        deep_merge_extensions(&mut self.extensions, &patch.extensions);
    }
}

fn validate_raw_theme(source: &str, raw: &RawTheme) -> Result<(), ThemeError> {
    if raw.schema_version != THEME_SCHEMA_VERSION {
        return Err(ThemeError::UnsupportedVersion {
            source_name: source.to_owned(),
            version: raw.schema_version,
        });
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
) -> Result<ResolvedThemeDefinition, ThemeError> {
    let mut palette = BTreeMap::new();
    for name in merged.palette.keys() {
        let color =
            resolve_palette_color(id, name, &merged.palette, &mut palette, &mut Vec::new())?;
        palette.insert(name.clone(), color);
    }
    let mut styles = BTreeMap::new();
    for (role, raw) in &merged.styles {
        styles.insert(role.clone(), resolve_style(id, raw, &palette)?);
    }
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
    let fingerprint = theme_fingerprint(
        id,
        &display_name,
        &palette,
        &styles,
        &containers,
        &merged.extensions,
        &applied_layers,
    );
    Ok(ResolvedThemeDefinition {
        id: id.to_owned(),
        display_name,
        palette,
        styles,
        containers,
        extensions: merged.extensions,
        applied_layers,
        fingerprint,
    })
}

fn resolve_palette_color(
    theme: &str,
    name: &str,
    raw_palette: &BTreeMap<String, RawColor>,
    resolved: &mut BTreeMap<String, Color>,
    stack: &mut Vec<String>,
) -> Result<Color, ThemeError> {
    if let Some(color) = resolved.get(name) {
        return Ok(*color);
    }
    if stack.len() >= MAX_THEME_REFERENCE_DEPTH || stack.iter().any(|entry| entry == name) {
        let mut chain = stack.clone();
        chain.push(name.to_owned());
        return Err(ThemeError::ColorReferenceCycle {
            theme: theme.to_owned(),
            chain: chain.join(" -> "),
        });
    }
    let raw = raw_palette
        .get(name)
        .ok_or_else(|| ThemeError::UnknownColorReference {
            theme: theme.to_owned(),
            reference: name.to_owned(),
        })?;
    stack.push(name.to_owned());
    let color = resolve_color(theme, raw, raw_palette, resolved, stack)?;
    stack.pop();
    resolved.insert(name.to_owned(), color);
    Ok(color)
}

fn resolve_style(
    theme: &str,
    raw: &RawStyle,
    palette: &BTreeMap<String, Color>,
) -> Result<Style, ThemeError> {
    let mut style = Style::new();
    if let Some(fg) = &raw.fg {
        style = style.fg(resolve_flat_color(theme, fg, palette)?);
    }
    if let Some(bg) = &raw.bg {
        style = style.bg(resolve_flat_color(theme, bg, palette)?);
    }
    for modifier in &raw.modifiers {
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
    Ok(style)
}

fn resolve_flat_color(
    theme: &str,
    raw: &RawColor,
    palette: &BTreeMap<String, Color>,
) -> Result<Color, ThemeError> {
    match raw {
        RawColor::Text(value) if value.strip_prefix('$').is_some() => {
            let reference = value.trim_start_matches('$');
            palette
                .get(reference)
                .copied()
                .ok_or_else(|| ThemeError::UnknownColorReference {
                    theme: theme.to_owned(),
                    reference: reference.to_owned(),
                })
        }
        _ => parse_direct_color(theme, raw),
    }
}

fn resolve_color(
    theme: &str,
    raw: &RawColor,
    raw_palette: &BTreeMap<String, RawColor>,
    resolved: &mut BTreeMap<String, Color>,
    stack: &mut Vec<String>,
) -> Result<Color, ThemeError> {
    if let RawColor::Text(value) = raw
        && let Some(reference) = value.strip_prefix('$')
    {
        return resolve_palette_color(theme, reference, raw_palette, resolved, stack);
    }
    parse_direct_color(theme, raw)
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

fn deep_merge_extensions(
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

fn theme_fingerprint(
    id: &str,
    display_name: &str,
    palette: &BTreeMap<String, Color>,
    styles: &BTreeMap<String, Style>,
    containers: &BTreeMap<String, ContainerRecipe>,
    extensions: &BTreeMap<String, toml::Value>,
    layers: &[String],
) -> u64 {
    let mut hash = Fnv64::new();
    hash.add(id.as_bytes());
    hash.add(display_name.as_bytes());
    hash.add(format!("{palette:?}").as_bytes());
    hash.add(format!("{styles:?}").as_bytes());
    hash.add(format!("{containers:?}").as_bytes());
    hash.add(format!("{extensions:?}").as_bytes());
    hash.add(format!("{layers:?}").as_bytes());
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
    use bmux_tui::style::{Color, Modifier};

    use super::{
        ContainerBorder, ContainerLayout, ContainerWidth, ResolvedThemeVariant, ThemeCatalog,
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
            parse_theme_definition("future.toml", "schema_version = 2\nid = \"x\"\n"),
            Err(ThemeError::UnsupportedVersion { version: 2, .. })
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

    #[test]
    fn bundled_themes_use_the_runtime_loader() {
        let catalog = ThemeCatalog::bundled().expect("bundled themes parse");
        assert_eq!(catalog.definitions.len(), 2);
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
    }
}
