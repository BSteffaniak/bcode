//! `bcode theme explain` and `bcode theme profiles`.
//!
//! Explain resolves structural presentation for one described unit exactly as
//! the TUI does: host defaults, plugin manifest default rules, the configured
//! theme and profile layers, then declarative `[[presentation.rules]]`, then
//! interactive user-state rules, compiled with the host section registry and
//! the facts and properties the plugins declare. Plugin facts given on the
//! command line (`bcode.git.files=12`) are kept only on units their declaring
//! plugin produced, as the TUI derives them.
//!
//! Plugin layers come from the manifests of the enabled bundled and installed
//! plugins, read without loading any plugin library. Plugins the host's
//! pre-load checks reject ([`bcode_plugin::selected_plugin_manifests`]) are
//! reported as skipped instead; a library that passes those checks but fails
//! when opened still shows its layer, since only loading detects that.

use std::path::{Path, PathBuf};

use bcode_presentation_policy::models::{
    DurationBucket, FactType, FactValue, LayerSource, MAX_PLUGIN_FACT_TEXT_BYTES,
    PluginPresentationSchemas, PresentationRule, ResolvedPresentation, RuleLayer, SizeBucket,
    TurnRecency, UnitFacts, UnitKind, UnitStatus,
};
use bcode_presentation_policy::{
    CompiledPolicy, DiagnosticSeverity, PolicyDiagnostic, PolicyRegistry, PropertyOrigin,
};
use bcode_tui_theme::definition::{ResolvedThemeVariant, ThemeCatalog, ThemeKind, ThemeSelection};
use bcode_tui_theme::discovery::{ThemeDiscoveryRoot, default_theme_roots, discover_themes};
use bcode_tui_theme::selection::{resolve_discovered, structure_layers};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::CliError;

/// Fact keys accepted by `bcode theme explain`, in documentation order.
pub const FACT_KEYS: [&str; 14] = [
    "kind",
    "tool",
    "producer",
    "schema",
    "status",
    "is_error",
    "awaiting_decision",
    "turn",
    "depth",
    "lines",
    "items",
    "target",
    "duration",
    "last_in_turn",
];

/// One `key=value` unit fact given on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactArg {
    Kind(UnitKind),
    Tool(String),
    Producer(String),
    Schema(String),
    Status(UnitStatus),
    IsError(bool),
    AwaitingDecision(bool),
    Turn(TurnRecency),
    Depth(u16),
    Lines(SizeBucket),
    Items(SizeBucket),
    Target(String),
    Duration(DurationBucket),
    LastInTurn(bool),
    /// A plugin-declared fact, `<plugin id>.<key>=<value>`.
    Plugin(String, FactValue),
}

/// Parse one `key=value` fact; used as the clap value parser.
///
/// # Errors
///
/// Returns a message for a missing `=`, an unknown key, or a value that is
/// not valid for its key.
///
/// A namespaced key (`bcode.git.files=12`) is a plugin-declared fact; its
/// value is `true`/`false`, an integer, or one line of text, and is
/// interpreted by the fact's declared type when explaining.
pub fn parse_fact_arg(argument: &str) -> Result<FactArg, String> {
    let (key, value) = argument
        .split_once('=')
        .ok_or_else(|| format!("expected key=value, got {argument:?}"))?;
    let key = key.trim();
    let value = value.trim();
    Ok(match key {
        "kind" => FactArg::Kind(parse_named(key, value)?),
        "tool" => FactArg::Tool(non_empty(key, value)?),
        "producer" => FactArg::Producer(non_empty(key, value)?),
        "schema" => FactArg::Schema(non_empty(key, value)?),
        "status" => FactArg::Status(parse_named(key, value)?),
        "is_error" => FactArg::IsError(parse_bool(key, value)?),
        "awaiting_decision" => FactArg::AwaitingDecision(parse_bool(key, value)?),
        "turn" => FactArg::Turn(parse_named(key, value)?),
        "depth" => FactArg::Depth(
            value
                .parse()
                .map_err(|_| format!("depth must be a non-negative integer, got {value:?}"))?,
        ),
        "lines" => FactArg::Lines(parse_size(key, value)?),
        "items" => FactArg::Items(parse_size(key, value)?),
        "target" => FactArg::Target(non_empty(key, value)?),
        "duration" => FactArg::Duration(match value.parse::<u64>() {
            Ok(millis) => DurationBucket::of_millis(millis),
            Err(_) => parse_named(key, value)?,
        }),
        "last_in_turn" => FactArg::LastInTurn(parse_bool(key, value)?),
        _ if key.contains('.') => FactArg::Plugin(key.to_owned(), parse_plugin_fact(key, value)?),
        _ => {
            return Err(format!(
                "unknown fact {key:?}; expected one of: {}, or a plugin fact \
                 <plugin id>.<key>",
                FACT_KEYS.join(", ")
            ));
        }
    })
}

/// A plugin fact value: a bool, an integer, or else one line of text.
fn parse_plugin_fact(key: &str, value: &str) -> Result<FactValue, String> {
    if value.is_empty() || value.contains(['\n', '\r']) || value.len() > MAX_PLUGIN_FACT_TEXT_BYTES
    {
        return Err(format!(
            "{key} must be one non-empty line of at most {MAX_PLUGIN_FACT_TEXT_BYTES} bytes"
        ));
    }
    Ok(match value {
        "true" => FactValue::Bool(true),
        "false" => FactValue::Bool(false),
        _ => value
            .parse()
            .map_or_else(|_| FactValue::Text(value.to_owned()), FactValue::Int),
    })
}

/// `facts` with their plugin facts as the TUI would derive them under
/// `schemas`, plus a warning for each one dropped or unknown.
///
/// A declared fact is kept only when the unit's producer is its declaring
/// plugin, and only with a value of its declared type (a `string` fact also
/// takes values that parsed as bool or integer). Undeclared facts are kept so
/// rules can still be tried against them.
fn plugin_facts_for(
    mut facts: UnitFacts,
    schemas: &PluginPresentationSchemas,
) -> (UnitFacts, Vec<String>) {
    let mut warnings = Vec::new();
    let producer = facts.producer.clone();
    facts.plugin = std::mem::take(&mut facts.plugin)
        .into_iter()
        .filter_map(|(name, value)| {
            let Some(schema) = schemas.facts.get(&name) else {
                warnings.push(format!("unknown plugin fact {name:?}"));
                return Some((name, value));
            };
            if producer.as_deref() != Some(schema.plugin.as_str()) {
                warnings.push(format!(
                    "plugin fact {name:?} ignored: only units produced by {} carry it",
                    schema.plugin
                ));
                return None;
            }
            let value = match (schema.value_type, value) {
                (FactType::String, FactValue::Bool(value)) => FactValue::Text(value.to_string()),
                (FactType::String, FactValue::Int(value)) => FactValue::Text(value.to_string()),
                (_, value) => value,
            };
            if schema.value_type.admits(&value) {
                Some((name, value))
            } else {
                warnings.push(format!(
                    "plugin fact {name:?} ignored: {value:?} does not have its declared type"
                ));
                None
            }
        })
        .collect();
    (facts, warnings)
}

fn non_empty(key: &str, value: &str) -> Result<String, String> {
    if value.is_empty() {
        Err(format!("{key} must not be empty"))
    } else {
        Ok(value.to_owned())
    }
}

fn parse_bool(key: &str, value: &str) -> Result<bool, String> {
    value
        .parse()
        .map_err(|_| format!("{key} must be true or false, got {value:?}"))
}

/// Parse a serde-named enum value (`tool`, `failed`, `<1s`, …).
fn parse_named<T: DeserializeOwned>(key: &str, value: &str) -> Result<T, String> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(|_| format!("invalid {key} {value:?}"))
}

/// A size bucket name (`few`) or an exact count (`12`).
fn parse_size(key: &str, value: &str) -> Result<SizeBucket, String> {
    value.parse::<usize>().map_or_else(
        |_| parse_named(key, value),
        |count| Ok(SizeBucket::of(count)),
    )
}

/// Unit facts described by `arguments`; later duplicates win.
///
/// Without a `kind` fact the unit is a tool, the common subject of structural
/// rules.
#[must_use]
pub fn unit_facts(arguments: &[FactArg]) -> UnitFacts {
    let mut facts = UnitFacts {
        kind: Some(UnitKind::Tool),
        ..UnitFacts::default()
    };
    for argument in arguments {
        match argument.clone() {
            FactArg::Kind(kind) => facts.kind = Some(kind),
            FactArg::Tool(tool) => facts.tool = Some(tool),
            FactArg::Producer(producer) => facts.producer = Some(producer),
            FactArg::Schema(schema) => facts.schema = Some(schema),
            FactArg::Status(status) => facts.status = Some(status),
            FactArg::IsError(is_error) => facts.is_error = is_error,
            FactArg::AwaitingDecision(awaiting) => facts.awaiting_decision = awaiting,
            FactArg::Turn(turn) => facts.turn = turn,
            FactArg::Depth(depth) => facts.depth = depth,
            FactArg::Lines(lines) => facts.lines = lines,
            FactArg::Items(items) => facts.items = items,
            FactArg::Target(target) => facts.target = Some(target),
            FactArg::Duration(duration) => facts.duration = Some(duration),
            FactArg::LastInTurn(last) => facts.last_in_turn = Some(last),
            FactArg::Plugin(name, value) => {
                facts.plugin.insert(name, value);
            }
        }
    }
    facts
}

/// Everything explain needs besides the unit facts, gathered from the
/// environment by [`load_explain_inputs`] or built directly by tests.
#[derive(Debug, Clone)]
pub struct ExplainInputs {
    /// Selected base theme id.
    pub theme: String,
    /// Selected profile, when any.
    pub profile: Option<String>,
    /// Overlays in application order (profile first).
    pub overlays: Vec<String>,
    /// Resolved light/dark variant.
    pub variant: ResolvedThemeVariant,
    /// Theme discovery roots.
    pub roots: Vec<ThemeDiscoveryRoot>,
    /// Plugin manifest default rule layers (see [`plugin_rule_layers`]).
    pub plugin_layers: Vec<RuleLayer>,
    /// Facts and properties those plugins declare, registered with the
    /// policy as the TUI registers them.
    pub plugin_schemas: PluginPresentationSchemas,
    /// Selected plugins the host's pre-load checks exclude.
    pub skipped_plugins: Vec<ExplainSkippedPlugin>,
    /// Why plugin manifests or declarations were skipped.
    pub plugin_warnings: Vec<String>,
    /// Declarative `[[presentation.rules]]`.
    pub config_rules: Vec<PresentationRule>,
    /// Interactive rules persisted to user state.
    pub user_rules: Vec<PresentationRule>,
    /// Why user-state rules could not be loaded, when they could not.
    pub user_state_error: Option<String>,
}

/// Theme name, profile, and overlays after command-line overrides.
///
/// `profile = Some("")` clears the configured profile.
#[must_use]
pub fn theme_selection(
    configured: &bcode_config::TuiThemeConfig,
    theme: Option<&str>,
    profile: Option<&str>,
) -> (String, Option<String>, Vec<String>) {
    let mut selected = configured.clone();
    if let Some(theme) = theme {
        theme.clone_into(&mut selected.name);
    }
    if let Some(profile) = profile {
        selected.profile = Some(profile.to_owned());
    }
    let overlays = selected.effective_overlays();
    let profile = selected
        .profile
        .filter(|profile| !profile.trim().is_empty());
    (selected.name, profile, overlays)
}

/// Maximum plugin warnings reported, so a damaged plugin directory cannot
/// flood the output.
const MAX_PLUGIN_WARNINGS: usize = 32;

/// A selected plugin whose layer explain omits because the host's pre-load
/// checks reject it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExplainSkippedPlugin {
    /// Plugin id.
    pub plugin: String,
    /// Why the host would not load it.
    pub reason: String,
}

/// Plugin rule layers explain stacks, with what was left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginRuleLayers {
    /// Manifest default rule layers of the plugins that pass pre-load checks.
    pub layers: Vec<RuleLayer>,
    /// Facts and properties those plugins declare.
    pub schemas: PluginPresentationSchemas,
    /// Selected plugins the pre-load checks exclude (bounded).
    pub skipped: Vec<ExplainSkippedPlugin>,
    /// Warnings for manifests or declaration entries that were skipped
    /// (bounded).
    pub warnings: Vec<String>,
}

/// Plugin manifest default rule layers for the plugins `selection` enables,
/// as the TUI stacks them, read from manifests only.
///
/// Static registrations take precedence over `discovered` plugins with the
/// same id. No plugin library is loaded: plugins the host's pre-load checks
/// reject are reported as skipped, but a library that passes them and still
/// fails when opened keeps its layer.
#[must_use]
pub fn plugin_rule_layers(
    selection: &bcode_plugin::PluginSelection,
    discovered: Vec<bcode_plugin::RegisteredPlugin>,
    static_plugins: &[bcode_plugin::StaticBundledPlugin],
) -> PluginRuleLayers {
    let mut warnings = Vec::new();
    let selected = bcode_plugin::selected_plugin_manifests(selection, discovered, static_plugins)
        .unwrap_or_else(|error| {
            warnings.push(format!("plugin manifests ignored: {error}"));
            bcode_plugin::SelectedPluginManifests::default()
        });
    let declarations = bcode_plugin::presentation_declarations(&selected.manifests);
    warnings.extend(declarations.iter().flat_map(|(plugin_id, declaration)| {
        declaration
            .diagnostics
            .iter()
            .map(move |diagnostic| format!("plugin {plugin_id}: {diagnostic}"))
    }));
    warnings.truncate(MAX_PLUGIN_WARNINGS);
    PluginRuleLayers {
        layers: bcode_plugin::presentation::presentation_rule_layers(&declarations),
        schemas: bcode_plugin::presentation::presentation_schemas(&declarations),
        skipped: selected
            .skipped
            .into_iter()
            .take(MAX_PLUGIN_WARNINGS)
            .map(|skipped| ExplainSkippedPlugin {
                plugin: skipped.plugin_id,
                reason: skipped.reason,
            })
            .collect(),
        warnings,
    }
}

/// Plugin layers for the configured plugin selection: bundled registrations
/// plus manifests discovered in the default plugin roots.
fn configured_plugin_layers(config: &bcode_config::BcodeConfig) -> PluginRuleLayers {
    let (discovered, discovery_warning) = match bcode_plugin::discover_plugins() {
        Ok(discovered) => (discovered, None),
        Err(error) => (
            Vec::new(),
            Some(format!("installed plugin discovery failed: {error}")),
        ),
    };
    let mut layers = plugin_rule_layers(
        &crate::plugin_selection_for_config(config),
        discovered,
        &crate::static_bundled_plugins(),
    );
    if let Some(warning) = discovery_warning {
        layers.warnings.insert(0, warning);
        layers.warnings.truncate(MAX_PLUGIN_WARNINGS);
    }
    layers
}

fn project_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn configured_roots(
    config: &bcode_config::BcodeConfig,
    project_root: &Path,
) -> Vec<ThemeDiscoveryRoot> {
    default_theme_roots(
        &bcode_config::default_config_dir(),
        project_root,
        &config.tui.theme.paths,
    )
}

/// Load the configured theme selection and rule layers, as the TUI does.
///
/// # Errors
///
/// Returns an error when the configuration cannot be loaded. An unreadable
/// user-state file is reported in the output instead, as the TUI ignores it.
pub fn load_explain_inputs(
    theme: Option<&str>,
    profile: Option<&str>,
) -> Result<ExplainInputs, CliError> {
    let config = bcode_config::load_config()?;
    let (theme, profile, overlays) = theme_selection(&config.tui.theme, theme, profile);
    let (user_rules, user_state_error) = match bcode_config::load_tui_presentation_rules() {
        Ok(rules) => (rules, None),
        Err(error) => (Vec::new(), Some(error.to_string())),
    };
    let plugins = configured_plugin_layers(&config);
    Ok(ExplainInputs {
        theme,
        profile,
        overlays,
        variant: bcode_tui::theme::resolve_variant_for_config(config.tui.theme.variant),
        roots: configured_roots(&config, &project_root()),
        plugin_layers: plugins.layers,
        plugin_schemas: plugins.schemas,
        skipped_plugins: plugins.skipped,
        plugin_warnings: plugins.warnings,
        // Same Config layer as the TUI: the legacy `tui.thinking` mapping
        // first, then declarative `[[presentation.rules]]`.
        config_rules: config
            .tui
            .thinking
            .presentation_rules()
            .into_iter()
            .chain(config.presentation.rules)
            .collect(),
        user_rules,
        user_state_error,
    })
}

/// Stable `--json` structure for `bcode theme explain`.
#[derive(Debug, Serialize)]
pub struct ExplainReport {
    /// Report format version.
    pub schema_version: u32,
    /// Theme selection and resolution.
    pub theme: ExplainTheme,
    /// The facts the unit was resolved with.
    pub facts: UnitFacts,
    /// Rule layers, lowest precedence first.
    pub layers: Vec<ExplainLayer>,
    /// Selected plugins whose layers are omitted because the host's pre-load
    /// checks reject them.
    pub skipped_plugins: Vec<ExplainSkippedPlugin>,
    /// Every resolved property and the rule that set it.
    pub properties: Vec<ExplainProperty>,
    /// The final presentation, after the visibility floor.
    pub presentation: ResolvedPresentation,
    /// Visibility floor adjustments.
    pub floor: Vec<String>,
    /// Policy compilation diagnostics.
    pub diagnostics: Vec<ExplainDiagnostic>,
    /// Theme, discovery, and user-state warnings.
    pub warnings: Vec<String>,
}

/// Theme selection section of [`ExplainReport`].
#[derive(Debug, Serialize)]
pub struct ExplainTheme {
    pub id: String,
    pub profile: Option<String>,
    pub overlays: Vec<String>,
    pub applied_layers: Vec<String>,
}

/// One rule layer in [`ExplainReport`].
#[derive(Debug, Serialize)]
pub struct ExplainLayer {
    #[serde(flatten)]
    pub source: LayerSource,
    pub rules: usize,
}

/// One resolved property in [`ExplainReport`].
#[derive(Debug, Serialize)]
pub struct ExplainProperty {
    /// Property path (`disclosure`, `sections.output.visible`, …).
    pub property: String,
    /// Resolved value.
    pub value: serde_json::Value,
    /// Rule that set it; `None` for host defaults.
    pub origin: Option<ExplainOrigin>,
}

/// Rule provenance in [`ExplainReport`].
#[derive(Debug, Serialize)]
pub struct ExplainOrigin {
    #[serde(flatten)]
    pub source: LayerSource,
    pub rule: usize,
}

/// One compilation diagnostic in [`ExplainReport`].
#[derive(Debug, Serialize)]
pub struct ExplainDiagnostic {
    pub severity: &'static str,
    #[serde(flatten)]
    pub source: LayerSource,
    pub rule: Option<usize>,
    pub message: String,
}

impl From<&PolicyDiagnostic> for ExplainDiagnostic {
    fn from(diagnostic: &PolicyDiagnostic) -> Self {
        Self {
            severity: match diagnostic.severity {
                DiagnosticSeverity::Warning => "warning",
                DiagnosticSeverity::Error => "error",
            },
            source: diagnostic.source.clone(),
            rule: diagnostic.rule,
            message: diagnostic.message.clone(),
        }
    }
}

/// Resolve `facts` against `inputs` and describe the result.
///
/// # Errors
///
/// Returns an error when the selected theme or profile cannot be resolved.
pub fn explain(inputs: &ExplainInputs, facts: UnitFacts) -> Result<ExplainReport, CliError> {
    let resolution = resolve_discovered(
        &inputs.roots,
        &ThemeSelection::new(&inputs.theme)
            .overlays(inputs.overlays.clone())
            .variant(inputs.variant),
    );
    let resolved = resolution
        .resolved
        .map_err(|error| CliError::Theme(error.to_string()))?;
    let mut warnings: Vec<String> = resolution
        .diagnostics
        .iter()
        .map(|diagnostic| {
            format!(
                "theme skipped: {}: {}",
                diagnostic.path.display(),
                diagnostic.message
            )
        })
        .collect();
    warnings.extend(
        resolved
            .warnings
            .iter()
            .map(|warning| format!("theme {}: {}", warning.theme, warning.message)),
    );
    if let Some(error) = &inputs.user_state_error {
        warnings.push(format!("user-state rules ignored: {error}"));
    }
    warnings.extend(inputs.plugin_warnings.iter().cloned());
    // Stacked by the same helper as the TUI.
    let layers = structure_layers(
        &bcode_tui::theme::structure_layers_for(Some(&resolved)),
        &inputs.plugin_layers,
        &inputs.config_rules,
        &inputs.user_rules,
    );
    let registry = PolicyRegistry::with_host_parts().with_plugin_schemas(&inputs.plugin_schemas);
    let (policy, diagnostics) = CompiledPolicy::compile(&layers, &registry);
    let (facts, fact_warnings) = plugin_facts_for(facts, &inputs.plugin_schemas);
    warnings.extend(fact_warnings);
    let explained = policy.explain(&facts);
    let properties = property_rows(&explained.presentation, &explained.origins);
    Ok(ExplainReport {
        schema_version: 1,
        theme: ExplainTheme {
            id: resolved.id,
            profile: inputs.profile.clone(),
            overlays: inputs.overlays.clone(),
            applied_layers: resolved.applied_layers,
        },
        facts,
        layers: layers
            .iter()
            .map(|layer| ExplainLayer {
                source: layer.source.clone(),
                rules: layer.rules.len(),
            })
            .collect(),
        skipped_plugins: inputs.skipped_plugins.clone(),
        properties,
        presentation: explained.presentation,
        floor: explained.floor,
        diagnostics: diagnostics.iter().map(ExplainDiagnostic::from).collect(),
        warnings,
    })
}

/// Flatten a resolved presentation into property rows, attaching origins,
/// with the same flattening the in-TUI inspector shows
/// ([`bcode_tui::inspector::property_rows`]).
fn property_rows(
    presentation: &ResolvedPresentation,
    origins: &[PropertyOrigin],
) -> Vec<ExplainProperty> {
    bcode_tui::inspector::property_rows(presentation, origins)
        .into_iter()
        .map(|row| ExplainProperty {
            property: row.property,
            value: row.value,
            origin: row.origin.map(|origin| ExplainOrigin {
                source: origin.source,
                rule: origin.rule,
            }),
        })
        .collect()
}

/// Short human label for a layer source (`theme:compact`, `config`, …).
#[must_use]
pub fn layer_label(source: &LayerSource) -> String {
    bcode_tui::inspector::layer_label(source)
}

/// Write `report` as JSON or tab-separated text.
///
/// # Errors
///
/// Returns an error when writing or serializing fails.
pub fn write_explain_report(
    report: &ExplainReport,
    json: bool,
    writer: &mut impl std::io::Write,
) -> Result<(), CliError> {
    if json {
        writeln!(writer, "{}", serde_json::to_string_pretty(report)?)?;
        return Ok(());
    }
    writeln!(
        writer,
        "theme\t{}\t{}",
        report.theme.id,
        report.theme.applied_layers.join(" > ")
    )?;
    writeln!(writer, "facts\t{}", serde_json::to_string(&report.facts)?)?;
    for layer in &report.layers {
        writeln!(
            writer,
            "layer\t{}\t{} rule(s)",
            layer_label(&layer.source),
            layer.rules
        )?;
    }
    for skipped in &report.skipped_plugins {
        writeln!(
            writer,
            "skipped\tplugin:{}\t{}",
            skipped.plugin, skipped.reason
        )?;
    }
    for property in &report.properties {
        let origin = property.origin.as_ref().map_or_else(
            || "default".to_owned(),
            |origin| format!("{}#{}", layer_label(&origin.source), origin.rule),
        );
        writeln!(
            writer,
            "property\t{}\t{}\t{origin}",
            property.property, property.value
        )?;
    }
    for note in &report.floor {
        writeln!(writer, "floor\t{note}")?;
    }
    for diagnostic in &report.diagnostics {
        let rule = diagnostic
            .rule
            .map_or_else(String::new, |rule| format!("#{rule}"));
        writeln!(
            writer,
            "{}\t{}{rule}\t{}",
            diagnostic.severity,
            layer_label(&diagnostic.source),
            diagnostic.message
        )?;
    }
    for warning in &report.warnings {
        writeln!(writer, "warning\t{warning}")?;
    }
    Ok(())
}

/// Run `bcode theme explain` against the user's configuration.
///
/// # Errors
///
/// Returns an error when configuration cannot be loaded, the theme cannot be
/// resolved, or output fails.
pub fn run_explain(
    theme: Option<&str>,
    profile: Option<&str>,
    facts: &[FactArg],
    json: bool,
    writer: &mut impl std::io::Write,
) -> Result<(), CliError> {
    let inputs = load_explain_inputs(theme, profile)?;
    let report = explain(&inputs, unit_facts(facts))?;
    write_explain_report(&report, json, writer)
}

/// One profile row for `bcode theme profiles --json`.
#[derive(Debug, Serialize)]
struct ProfileRow<'a> {
    id: &'a str,
    display_name: &'a str,
    description: Option<&'a str>,
    configured: bool,
}

/// List the profiles in `catalog`, marking the configured one.
///
/// # Errors
///
/// Returns an error when writing or serializing fails.
pub fn write_profiles(
    catalog: &ThemeCatalog,
    configured: Option<&str>,
    json: bool,
    writer: &mut impl std::io::Write,
) -> Result<(), CliError> {
    let rows: Vec<_> = catalog
        .definitions()
        .filter(|definition| definition.kind() == ThemeKind::Profile)
        .map(|definition| ProfileRow {
            id: definition.id(),
            display_name: definition.display_name(),
            description: definition.description(),
            configured: configured == Some(definition.id()),
        })
        .collect();
    if json {
        let value = serde_json::json!({ "schema_version": 1, "profiles": rows });
        writeln!(writer, "{}", serde_json::to_string_pretty(&value)?)?;
        return Ok(());
    }
    for row in rows {
        writeln!(
            writer,
            "{}{}\t{}\t{}",
            row.id,
            if row.configured { " *" } else { "" },
            row.display_name,
            row.description.unwrap_or("")
        )?;
    }
    Ok(())
}

/// Run `bcode theme profiles` over the discovered catalog.
///
/// # Errors
///
/// Returns an error when configuration cannot be loaded, embedded themes are
/// invalid, or output fails.
pub fn run_profiles(json: bool, writer: &mut impl std::io::Write) -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    let roots = configured_roots(&config, &project_root());
    let catalog = discover_themes(&roots)
        .map(|discovered| discovered.catalog)
        .or_else(|_| ThemeCatalog::bundled())
        .map_err(|error| CliError::Theme(error.to_string()))?;
    let configured = config
        .tui
        .theme
        .profile
        .as_deref()
        .filter(|profile| !profile.trim().is_empty());
    write_profiles(&catalog, configured, json, writer)
}

#[cfg(test)]
mod tests {
    use bcode_presentation_policy::models::Disclosure;
    use clap::Parser as _;

    use super::*;

    fn inputs(profile: Option<&str>) -> (tempfile::TempDir, ExplainInputs) {
        let directory = tempfile::tempdir().expect("tempdir");
        let roots = default_theme_roots(directory.path(), directory.path(), &[]);
        let overlays = profile
            .map(|profile| vec![profile.to_owned()])
            .unwrap_or_default();
        let inputs = ExplainInputs {
            theme: "terminal-native".to_owned(),
            profile: profile.map(str::to_owned),
            overlays,
            variant: ResolvedThemeVariant::Dark,
            roots,
            plugin_layers: Vec::new(),
            plugin_schemas: PluginPresentationSchemas::default(),
            skipped_plugins: Vec::new(),
            plugin_warnings: Vec::new(),
            config_rules: Vec::new(),
            user_rules: Vec::new(),
            user_state_error: None,
        };
        (directory, inputs)
    }

    fn rules(source: &str) -> Vec<PresentationRule> {
        #[derive(serde::Deserialize)]
        struct Rules {
            rules: Vec<PresentationRule>,
        }
        toml::from_str::<Rules>(source).expect("rules").rules
    }

    fn parse(arguments: &[&str]) -> Result<crate::Cli, clap::Error> {
        crate::Cli::try_parse_from(
            ["bcode", "theme", "explain"]
                .iter()
                .chain(arguments)
                .copied(),
        )
    }

    fn explain_command(cli: crate::Cli) -> (Option<String>, Option<String>, Vec<FactArg>, bool) {
        match cli.command {
            Some(crate::Commands::Theme {
                command:
                    crate::ThemeCommand::Explain {
                        theme,
                        profile,
                        facts,
                        json,
                    },
            }) => (theme, profile, facts, json),
            other => panic!("unexpected command {other:?}"),
        }
    }

    fn property<'a>(report: &'a ExplainReport, name: &str) -> &'a ExplainProperty {
        report
            .properties
            .iter()
            .find(|property| property.property == name)
            .unwrap_or_else(|| panic!("missing property {name}"))
    }

    #[test]
    fn profile_density_is_explained_as_the_lowest_host_layer() {
        let (_directory, inputs) = inputs(Some("brutalist"));
        let facts = UnitFacts {
            kind: Some(UnitKind::Tool),
            ..UnitFacts::default()
        };
        let report = explain(&inputs, facts).expect("explain");
        assert_eq!(
            report.layers.first().map(|layer| &layer.source),
            Some(&LayerSource::Host)
        );
        let spacing = property(&report, "spacing");
        assert_eq!(
            spacing.value,
            serde_json::json!({ "before": 0, "after": 0 })
        );
        assert_eq!(
            spacing.origin.as_ref().map(|origin| &origin.source),
            Some(&LayerSource::Host)
        );
    }

    /// Explain inputs with plugin layers from a bundled plugin, an installed
    /// plugin, and an installed plugin whose library file is missing.
    fn plugin_inputs() -> (tempfile::TempDir, ExplainInputs) {
        /// Never activated: explain reads manifests only.
        #[derive(Default)]
        struct ManifestOnly;
        impl bcode_plugin_sdk::RustPlugin for ManifestOnly {}
        const BUNDLED: &str = r#"
id = "test.bundled"
name = "Bundled"
version = "0.0.1"

[[presentation.rules]]
match = { kind = "tool" }
set = { disclosure = "collapsed" }

[runtime]
type = "native"
abi_version = 1
library = "libbundled.dylib"
"#;
        let abi = format!(
            "abi_version = {}",
            bcode_plugin_sdk::CURRENT_PLUGIN_ABI_VERSION
        );
        // Only the registration's manifest is read; the vtable is never used.
        let bundled_manifest: &'static str =
            Box::leak(BUNDLED.replace("abi_version = 1", &abi).into_boxed_str());
        let bundled = [bcode_plugin::StaticBundledPlugin::new(
            bundled_manifest,
            bcode_plugin_sdk::static_plugin_vtable!(ManifestOnly, BUNDLED),
        )];
        // An existing file stands in for the installed library; nothing is
        // opened.
        let present_library = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let installed_manifest = r#"
id = "test.installed"
name = "Installed"
version = "0.0.1"

[[presentation.rules]]
match = { kind = "tool", producer = "someone.else" }
set = { indent = 4 }

[[presentation.rules]]
match = { kind = "tool" }
set = { spacing = { before = 2, after = 2 } }

[runtime]
type = "native"
abi_version = 1
library = "LIBRARY"
"#
        .replace("abi_version = 1", &abi);
        let registered = |id: &str, library: &str| bcode_plugin::RegisteredPlugin {
            manifest_path: PathBuf::from(format!("plugins/{id}/bcode-plugin.toml")),
            manifest: toml::from_str(
                &installed_manifest
                    .replace("test.installed", id)
                    .replace("LIBRARY", library),
            )
            .expect("installed manifest"),
        };
        let plugins = plugin_rule_layers(
            &bcode_plugin::PluginSelection::all_enabled(),
            vec![
                registered(
                    "test.installed",
                    present_library.to_str().expect("utf-8 path"),
                ),
                // Rejected before load: its library file does not exist.
                registered("test.broken", "libmissing.dylib"),
            ],
            &bundled,
        );
        let (directory, mut inputs) = inputs(None);
        inputs.plugin_layers = plugins.layers;
        inputs.plugin_schemas = plugins.schemas;
        inputs.skipped_plugins = plugins.skipped;
        inputs.plugin_warnings = plugins.warnings;
        (directory, inputs)
    }

    #[test]
    fn plugins_failing_preload_checks_are_listed_as_skipped() {
        let (_directory, inputs) = plugin_inputs();
        assert_eq!(
            inputs
                .skipped_plugins
                .iter()
                .map(|skipped| skipped.plugin.as_str())
                .collect::<Vec<_>>(),
            ["test.broken"]
        );
        assert!(
            inputs.skipped_plugins[0].reason.contains("does not exist"),
            "{:?}",
            inputs.skipped_plugins
        );
        let report = explain(&inputs, UnitFacts::default()).expect("explain");
        assert!(
            !report
                .layers
                .iter()
                .any(|layer| layer.source == LayerSource::Plugin("test.broken".to_owned())),
            "{:?}",
            report.layers
        );
        assert_eq!(report.skipped_plugins, inputs.skipped_plugins);
        let mut text = Vec::new();
        write_explain_report(&report, false, &mut text).expect("text");
        let text = String::from_utf8(text).expect("utf8");
        assert!(
            text.lines()
                .any(|line| line.starts_with("skipped\tplugin:test.broken\t")
                    && line.contains("does not exist")),
            "{text}"
        );
        let mut json = Vec::new();
        write_explain_report(&report, true, &mut json).expect("json");
        let value: serde_json::Value = serde_json::from_slice(&json).expect("valid json");
        assert_eq!(value["skipped_plugins"][0]["plugin"], "test.broken");
    }

    #[test]
    fn plugin_manifest_rules_are_explained_as_plugin_layers() {
        let (_directory, inputs) = plugin_inputs();
        let own = |producer: &str| UnitFacts {
            kind: Some(UnitKind::Tool),
            producer: Some(producer.to_owned()),
            ..UnitFacts::default()
        };

        let report = explain(&inputs, own("test.bundled")).expect("explain");
        let sources = report
            .layers
            .iter()
            .map(|layer| layer.source.clone())
            .collect::<Vec<_>>();
        // Plugin layers, in plugin-id order, sit after host defaults and
        // before every theme, configuration, and user-state layer.
        let plugin = |id: &str| LayerSource::Plugin(id.to_owned());
        let stacked = sources
            .iter()
            .filter(|source| **source != LayerSource::Host)
            .take(2)
            .collect::<Vec<_>>();
        assert_eq!(
            stacked,
            [&plugin("test.bundled"), &plugin("test.installed")],
            "{sources:?}"
        );
        let disclosure = property(&report, "disclosure");
        assert_eq!(disclosure.value, serde_json::json!("collapsed"));
        assert_eq!(
            disclosure.origin.as_ref().map(|origin| &origin.source),
            Some(&plugin("test.bundled"))
        );
        // The installed plugin's rule naming another producer was dropped
        // with a warning; its own rule applies only to its own units.
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.starts_with("plugin test.installed:")),
            "{:?}",
            report.warnings
        );
        let spacing = property(
            &explain(&inputs, own("test.installed")).expect("explain"),
            "spacing",
        )
        .origin
        .as_ref()
        .map(|origin| origin.source.clone());
        assert_eq!(spacing, Some(plugin("test.installed")));
        assert_ne!(
            property(&report, "spacing")
                .origin
                .as_ref()
                .map(|origin| &origin.source),
            Some(&plugin("test.installed"))
        );
    }

    /// Explain inputs with a manifest-only bundled plugin `test.git` that
    /// declares an int fact, a string fact, and a bool property.
    fn declared_plugin_inputs() -> (tempfile::TempDir, ExplainInputs) {
        /// Never activated: explain reads manifests only.
        #[derive(Default)]
        struct ManifestOnly;
        impl bcode_plugin_sdk::RustPlugin for ManifestOnly {}
        const MANIFEST: &str = r#"
id = "test.git"
name = "Git"
version = "0.0.1"

[[presentation.facts]]
name = "test.git.files"
type = "int"
payload = "files"

[[presentation.facts]]
name = "test.git.branch"
type = "string"
argument = "branch"

[[presentation.properties]]
name = "test.git.show_destination"
type = "bool"
default = true

[runtime]
type = "native"
abi_version = 1
library = "libgit.dylib"
"#;
        let manifest: &'static str = Box::leak(
            MANIFEST
                .replace(
                    "abi_version = 1",
                    &format!(
                        "abi_version = {}",
                        bcode_plugin_sdk::CURRENT_PLUGIN_ABI_VERSION
                    ),
                )
                .into_boxed_str(),
        );
        let bundled = [bcode_plugin::StaticBundledPlugin::new(
            manifest,
            bcode_plugin_sdk::static_plugin_vtable!(ManifestOnly, MANIFEST),
        )];
        let plugins = plugin_rule_layers(
            &bcode_plugin::PluginSelection::all_enabled(),
            Vec::new(),
            &bundled,
        );
        let (directory, mut inputs) = inputs(None);
        inputs.plugin_layers = plugins.layers;
        inputs.plugin_schemas = plugins.schemas;
        inputs.config_rules = rules(
            r#"
            [[rules]]
            match = { plugin = { "test.git.files" = { gt = 10 } } }
            set = { disclosure = "collapsed", plugin = { "test.git.show_destination" = false } }

            [[rules]]
            match = { plugin = { "test.git.branch" = "123" } }
            set = { detail = "minimal" }
            "#,
        );
        (directory, inputs)
    }

    #[test]
    fn plugin_facts_and_properties_are_explained_with_their_declarations() {
        let (_directory, inputs) = declared_plugin_inputs();
        let facts = |arguments: &[&str]| {
            unit_facts(
                &arguments
                    .iter()
                    .map(|argument| parse_fact_arg(argument).expect("fact"))
                    .collect::<Vec<_>>(),
            )
        };
        let show_destination = |report: &ExplainReport| {
            let row = property(report, "test.git.show_destination");
            (
                row.value.clone(),
                row.origin.as_ref().map(|origin| origin.source.clone()),
            )
        };

        // The declared property is registered: no unknown-property
        // diagnostic, and its manifest default applies to the plugin's units.
        let report = explain(&inputs, facts(&["producer=test.git"])).expect("explain");
        assert!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);
        assert_eq!(
            show_destination(&report),
            (
                serde_json::json!(true),
                Some(LayerSource::Plugin("test.git".to_owned()))
            )
        );

        // A declared fact on the plugin's own unit matches the range rule,
        // and a string fact takes a value that looks like an integer.
        let report = explain(
            &inputs,
            facts(&[
                "producer=test.git",
                "test.git.files=11",
                "test.git.branch=123",
            ]),
        )
        .expect("explain");
        assert_eq!(
            show_destination(&report),
            (serde_json::json!(false), Some(LayerSource::Config))
        );
        assert_eq!(report.presentation.disclosure, Disclosure::Collapsed);
        assert_eq!(
            property(&report, "detail").value,
            serde_json::json!("minimal")
        );
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);

        // Another producer's unit never carries the fact or the property.
        let report = explain(
            &inputs,
            facts(&["producer=test.other", "test.git.files=11"]),
        )
        .expect("explain");
        assert_eq!(report.presentation.disclosure, Disclosure::Expanded);
        assert!(report.presentation.plugin.is_empty());
        assert!(report.facts.plugin.is_empty());
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("only units produced by test.git")),
            "{:?}",
            report.warnings
        );

        // A value of the wrong type is dropped with a warning; unknown facts
        // are kept and reported.
        let report = explain(
            &inputs,
            facts(&[
                "producer=test.git",
                "test.git.files=many",
                "test.git.extra=1",
            ]),
        )
        .expect("explain");
        assert_eq!(
            report.facts.plugin,
            std::collections::BTreeMap::from([("test.git.extra".to_owned(), FactValue::Int(1))])
        );
        assert_eq!(report.warnings.len(), 2, "{:?}", report.warnings);
    }

    #[test]
    fn plugin_fact_arguments_parse_typed_values() {
        assert_eq!(
            parse_fact_arg("bcode.git.files=12"),
            Ok(FactArg::Plugin(
                "bcode.git.files".to_owned(),
                FactValue::Int(12)
            ))
        );
        assert_eq!(
            parse_fact_arg("bcode.git.dirty=true"),
            Ok(FactArg::Plugin(
                "bcode.git.dirty".to_owned(),
                FactValue::Bool(true)
            ))
        );
        assert_eq!(
            parse_fact_arg("bcode.git.repo=a=b"),
            Ok(FactArg::Plugin(
                "bcode.git.repo".to_owned(),
                FactValue::Text("a=b".to_owned())
            ))
        );
        assert!(parse_fact_arg("bcode.git.repo=").is_err());
        let long = format!(
            "bcode.git.repo={}",
            "x".repeat(MAX_PLUGIN_FACT_TEXT_BYTES + 1)
        );
        assert!(parse_fact_arg(&long).is_err());
    }

    #[test]
    fn parses_theme_profile_facts_and_json() {
        let (theme, profile, facts, json) = explain_command(
            parse(&[
                "--theme",
                "nord",
                "--profile",
                "compact",
                "tool=shell",
                "status=failed",
                "lines=12",
                "duration=1500",
                "last_in_turn=true",
                "--json",
            ])
            .expect("parse"),
        );
        assert_eq!(theme.as_deref(), Some("nord"));
        assert_eq!(profile.as_deref(), Some("compact"));
        assert!(json);
        assert_eq!(
            facts,
            [
                FactArg::Tool("shell".to_owned()),
                FactArg::Status(UnitStatus::Failed),
                FactArg::Lines(SizeBucket::Some),
                FactArg::Duration(DurationBucket::OneToTenSeconds),
                FactArg::LastInTurn(true),
            ]
        );
    }

    #[test]
    fn rejects_unknown_keys_and_invalid_values() {
        assert!(parse(&["colour=red"]).is_err());
        assert!(parse(&["tool"]).is_err());
        assert!(parse(&["status=finished"]).is_err());
        assert!(parse(&["is_error=maybe"]).is_err());
        assert!(parse(&["tool="]).is_err());
        let error = parse_fact_arg("colour=red").expect_err("unknown key");
        assert!(error.contains("last_in_turn"), "{error}");
    }

    #[test]
    fn fact_values_accept_names_counts_and_aliases() {
        assert_eq!(
            parse_fact_arg("duration=<1s"),
            Ok(FactArg::Duration(DurationBucket::UnderOneSecond))
        );
        assert_eq!(
            parse_fact_arg("duration=gt60"),
            Ok(FactArg::Duration(DurationBucket::OverSixtySeconds))
        );
        assert_eq!(
            parse_fact_arg("items=few"),
            Ok(FactArg::Items(SizeBucket::Few))
        );
        assert_eq!(
            parse_fact_arg("items=0"),
            Ok(FactArg::Items(SizeBucket::Empty))
        );
        assert_eq!(
            parse_fact_arg("target=src/a=b.rs"),
            Ok(FactArg::Target("src/a=b.rs".to_owned()))
        );
        assert_eq!(
            parse_fact_arg("kind=assistant_message"),
            Ok(FactArg::Kind(UnitKind::AssistantMessage))
        );
    }

    #[test]
    fn facts_default_to_a_tool_and_later_duplicates_win() {
        let facts = unit_facts(&[
            FactArg::Tool("read".to_owned()),
            FactArg::Tool("shell".to_owned()),
            FactArg::Target("ls".to_owned()),
        ]);
        assert_eq!(facts.kind, Some(UnitKind::Tool));
        assert_eq!(facts.tool.as_deref(), Some("shell"));
        assert_eq!(facts.target.as_deref(), Some("ls"));
        assert_eq!(
            unit_facts(&[FactArg::Kind(UnitKind::Reasoning)]).kind,
            Some(UnitKind::Reasoning)
        );
    }

    #[test]
    fn command_line_overrides_configured_theme_and_profile() {
        let configured = bcode_config::TuiThemeConfig {
            name: "nord".to_owned(),
            profile: Some("riced".to_owned()),
            overlays: vec!["extra".to_owned()],
            ..bcode_config::TuiThemeConfig::default()
        };
        assert_eq!(
            theme_selection(&configured, None, None),
            (
                "nord".to_owned(),
                Some("riced".to_owned()),
                vec!["riced".to_owned(), "extra".to_owned()]
            )
        );
        assert_eq!(
            theme_selection(&configured, Some("bcode"), Some("bare")),
            (
                "bcode".to_owned(),
                Some("bare".to_owned()),
                vec!["bare".to_owned(), "extra".to_owned()]
            )
        );
        assert_eq!(
            theme_selection(&configured, None, Some("")),
            ("nord".to_owned(), None, vec!["extra".to_owned()])
        );
    }

    #[test]
    fn reports_the_layer_and_rule_that_set_each_property() {
        let (_directory, mut inputs) = inputs(Some("compact"));
        inputs.config_rules =
            rules("[[rules]]\nmatch = { tool = \"shell\" }\nset = { detail = \"verbose\" }\n");
        inputs.user_rules = rules(
            "[[rules]]\nmatch = { tool = \"shell\" }\nset = { disclosure = \"collapsed\" }\n",
        );
        let report =
            explain(&inputs, unit_facts(&[FactArg::Tool("shell".to_owned())])).expect("explain");

        assert_eq!(report.theme.id, "terminal-native");
        assert!(report.theme.applied_layers.contains(&"compact".to_owned()));
        let sources: Vec<_> = report
            .layers
            .iter()
            .map(|layer| layer.source.clone())
            .collect();
        assert!(sources.contains(&LayerSource::Theme("compact".to_owned())));
        assert_eq!(
            &sources[sources.len() - 2..],
            [LayerSource::Config, LayerSource::UserState]
        );

        let disclosure = property(&report, "disclosure");
        assert_eq!(disclosure.value, serde_json::json!("collapsed"));
        let origin = disclosure.origin.as_ref().expect("origin");
        assert_eq!(
            (origin.source.clone(), origin.rule),
            (LayerSource::UserState, 0)
        );

        let detail = property(&report, "detail").origin.as_ref().expect("origin");
        assert_eq!(detail.source, LayerSource::Config);

        let timing = property(&report, "sections.timing.visible");
        assert_eq!(timing.value, serde_json::json!(false));
        assert_eq!(
            timing.origin.as_ref().map(|origin| origin.source.clone()),
            Some(LayerSource::Theme("compact".to_owned()))
        );
        assert!(property(&report, "indent").origin.is_none());
        assert_eq!(report.floor, Vec::<String>::new());
    }

    #[test]
    fn reports_floor_adjustments_and_policy_warnings() {
        let (_directory, mut inputs) = inputs(None);
        inputs.config_rules = rules(
            "[[rules]]\nset = { disclosure = \"hidden\", sections = { outptu = { visible = false } } }\n",
        );
        inputs.user_state_error = Some("state unreadable".to_owned());
        let report =
            explain(&inputs, unit_facts(&[FactArg::AwaitingDecision(true)])).expect("explain");
        assert_eq!(report.presentation.disclosure, Disclosure::Expanded);
        assert!(
            report.floor.iter().any(|note| note.contains("disclosure")),
            "{:?}",
            report.floor
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.severity == "warning"
                    && diagnostic.source == LayerSource::Config
                    && diagnostic.message.contains("outptu")),
            "{:?}",
            report.diagnostics
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("state unreadable"))
        );

        let mut text = Vec::new();
        write_explain_report(&report, false, &mut text).expect("write");
        let text = String::from_utf8(text).expect("utf8");
        assert!(
            text.lines().any(|line| line.starts_with("floor\t")),
            "{text}"
        );
        assert!(
            text.contains("property\tdisclosure\t\"expanded\"\tconfig#0"),
            "{text}"
        );
        assert!(
            text.lines()
                .any(|line| line.starts_with("warning\tconfig#0\t")),
            "{text}"
        );
    }

    #[test]
    fn json_output_is_a_stable_structure() {
        let (_directory, inputs) = inputs(Some("compact"));
        let report =
            explain(&inputs, unit_facts(&[FactArg::Tool("read".to_owned())])).expect("explain");
        let mut output = Vec::new();
        write_explain_report(&report, true, &mut output).expect("write");
        let value: serde_json::Value = serde_json::from_slice(&output).expect("json");
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["theme"]["profile"], "compact");
        assert_eq!(value["facts"]["tool"], "read");
        assert_eq!(value["presentation"]["disclosure"], "summary");
        let disclosure = value["properties"]
            .as_array()
            .expect("properties")
            .iter()
            .find(|property| property["property"] == "disclosure")
            .expect("disclosure");
        assert_eq!(disclosure["origin"]["source"], "theme");
        assert_eq!(disclosure["origin"]["id"], "compact");
        assert_eq!(disclosure["origin"]["rule"], 0);
        assert!(value["floor"].as_array().is_some());
        assert!(value["diagnostics"].as_array().is_some());
        assert!(value["warnings"].as_array().is_some());
    }

    #[test]
    fn unknown_profile_is_an_error() {
        let (_directory, inputs) = inputs(Some("no-such-profile"));
        assert!(matches!(
            explain(&inputs, UnitFacts::default()),
            Err(CliError::Theme(message)) if message.contains("no-such-profile")
        ));
    }

    #[test]
    fn profiles_list_descriptions_and_mark_the_configured_profile() {
        let catalog = ThemeCatalog::bundled().expect("catalog");
        let mut text = Vec::new();
        write_profiles(&catalog, Some("compact"), false, &mut text).expect("write");
        let text = String::from_utf8(text).expect("utf8");
        let compact = text
            .lines()
            .find(|line| line.starts_with("compact"))
            .expect("compact row");
        assert!(compact.starts_with("compact *\t"), "{compact}");
        assert!(compact.contains("Built for small terminals"), "{compact}");
        assert!(
            !text.lines().any(|line| line.starts_with("terminal-native")),
            "{text}"
        );

        let mut json = Vec::new();
        write_profiles(&catalog, None, true, &mut json).expect("write");
        let value: serde_json::Value = serde_json::from_slice(&json).expect("json");
        let profiles = value["profiles"].as_array().expect("profiles");
        assert!(profiles.len() >= 10);
        assert!(
            profiles
                .iter()
                .all(|profile| profile["configured"] == false)
        );
    }

    #[test]
    fn profiles_subcommand_parses() {
        assert!(matches!(
            crate::Cli::try_parse_from(["bcode", "theme", "profiles", "--json"])
                .expect("parse")
                .command,
            Some(crate::Commands::Theme {
                command: crate::ThemeCommand::Profiles { json: true }
            })
        ));
    }
}
