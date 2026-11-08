//! `bcode theme explain` and `bcode theme profiles`.
//!
//! Explain resolves structural presentation for one described unit exactly as
//! the TUI does: the configured theme and profile layers, then declarative
//! `[[presentation.rules]]`, then interactive user-state rules, compiled with
//! the host section registry.

use std::path::{Path, PathBuf};

use bcode_presentation_policy::models::{
    DurationBucket, LayerSource, PresentationRule, ResolvedPresentation, SizeBucket, TurnRecency,
    UnitFacts, UnitKind, UnitStatus,
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
}

/// Parse one `key=value` fact; used as the clap value parser.
///
/// # Errors
///
/// Returns a message for a missing `=`, an unknown key, or a value that is
/// not valid for its key.
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
        _ => {
            return Err(format!(
                "unknown fact {key:?}; expected one of: {}",
                FACT_KEYS.join(", ")
            ));
        }
    })
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
    Ok(ExplainInputs {
        theme,
        profile,
        overlays,
        variant: bcode_tui::theme::resolve_variant_for_config(config.tui.theme.variant),
        roots: configured_roots(&config, &project_root()),
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
    // Stacked by the same helper as the TUI. Plugin manifest layers are not
    // loaded here (explain does not start plugins), so the plugin slot is
    // empty; see "Not yet done" in docs/presentation-architecture.md.
    let layers = structure_layers(
        &bcode_tui::theme::structure_layers_for(Some(&resolved)),
        &[],
        &inputs.config_rules,
        &inputs.user_rules,
    );
    let (policy, diagnostics) =
        CompiledPolicy::compile(&layers, &PolicyRegistry::with_host_parts());
    let explained = policy.explain(&facts);
    let properties = property_rows(&explained.presentation, &explained.origins)?;
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
        properties,
        presentation: explained.presentation,
        floor: explained.floor,
        diagnostics: diagnostics.iter().map(ExplainDiagnostic::from).collect(),
        warnings,
    })
}

/// Flatten a resolved presentation into property rows, attaching origins.
///
/// Sections expand to `sections.<name>.<field>` and plugin properties use
/// their namespaced key, matching [`PropertyOrigin::property`] names.
fn property_rows(
    presentation: &ResolvedPresentation,
    origins: &[PropertyOrigin],
) -> Result<Vec<ExplainProperty>, CliError> {
    let serde_json::Value::Object(fields) = serde_json::to_value(presentation)? else {
        return Ok(Vec::new());
    };
    let mut values = Vec::new();
    for (name, value) in fields {
        match (name.as_str(), value) {
            ("sections", serde_json::Value::Object(sections)) => {
                for (section, value) in sections {
                    if let serde_json::Value::Object(section_fields) = value {
                        for (field, value) in section_fields {
                            values.push((format!("sections.{section}.{field}"), value));
                        }
                    }
                }
            }
            ("plugin", serde_json::Value::Object(plugin)) => values.extend(plugin),
            (_, value) => values.push((name, value)),
        }
    }
    Ok(values
        .into_iter()
        .map(|(property, value)| ExplainProperty {
            origin: origins
                .iter()
                .find(|origin| origin.property == property)
                .map(|origin| ExplainOrigin {
                    source: origin.source.clone(),
                    rule: origin.rule,
                }),
            property,
            value,
        })
        .collect())
}

/// Short human label for a layer source (`theme:compact`, `config`, …).
#[must_use]
pub fn layer_label(source: &LayerSource) -> String {
    match source {
        LayerSource::Host => "host".to_owned(),
        LayerSource::Plugin(id) => format!("plugin:{id}"),
        LayerSource::Theme(id) => format!("theme:{id}"),
        LayerSource::Config => "config".to_owned(),
        LayerSource::UserState => "user_state".to_owned(),
    }
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
