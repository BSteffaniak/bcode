//! `bcode theme check`.
//!
//! The analysis lives in [`bcode_tui_theme::check`]; this module selects the
//! theme, compiles its structural rule layers for presentation diagnostics,
//! and formats the report as a table or JSON.

use std::path::Path;

use bcode_presentation_policy::models::PluginPresentationSchemas;
use bcode_presentation_policy::{CompiledPolicy, DiagnosticSeverity, PolicyRegistry};
use bcode_tui_theme::check::{
    CheckFinding, ContrastOutcome, FindingCategory, ThemeCheck, VariantCheck, check_theme,
};
use bcode_tui_theme::definition::{
    ResolvedThemeDefinition, ThemeCatalog, ThemeDefinition, ThemeKind, parse_theme_definition,
};
use bcode_tui_theme::discovery::{ThemeDiscoveryRoot, discover_themes};
use bcode_tui_theme::selection::structure_layers;

use crate::CliError;
use crate::theme_explain::layer_label;

/// Theme checked when a profile or base is checked on its own.
const DEFAULT_BASE_THEME: &str = "terminal-native";

/// What `bcode theme check` checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckTarget {
    /// The configured theme with its configured overlays and profile.
    Configured {
        /// Base theme id.
        theme: String,
        /// Overlays in application order.
        overlays: Vec<String>,
    },
    /// One theme id from the discovered catalog.
    Id(String),
    /// One theme file, checked over the discovered catalog.
    File(std::path::PathBuf),
}

impl CheckTarget {
    /// Interpret the optional `THEME` argument.
    ///
    /// An argument naming an existing file, or ending in `.toml`, is a file;
    /// anything else is a theme id.
    #[must_use]
    pub fn from_argument(argument: Option<&str>, configured: (String, Vec<String>)) -> Self {
        match argument {
            None => Self::Configured {
                theme: configured.0,
                overlays: configured.1,
            },
            Some(argument)
                if Path::new(argument)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("toml"))
                    || Path::new(argument).is_file() =>
            {
                Self::File(argument.into())
            }
            Some(argument) => Self::Id(argument.to_owned()),
        }
    }
}

/// Presentation diagnostics for one resolution: the theme's structural layers
/// (including the chrome density host layer) compiled with the host section
/// registry and the enabled plugins' declared fact and property `schemas`,
/// exactly as the TUI stacks them. Configuration and user-state rules are not
/// part of a theme and are not checked.
fn presentation_findings(
    resolved: &ResolvedThemeDefinition,
    schemas: &PluginPresentationSchemas,
) -> Vec<CheckFinding> {
    let layers = structure_layers(
        &bcode_tui::theme::structure_layers_for(Some(resolved)),
        &[],
        &[],
        &[],
    );
    let registry = PolicyRegistry::with_host_parts().with_plugin_schemas(schemas);
    let (_, diagnostics) = CompiledPolicy::compile(&layers, &registry);
    diagnostics
        .into_iter()
        .map(|diagnostic| {
            let subject = diagnostic.rule.map_or_else(
                || layer_label(&diagnostic.source),
                |rule| format!("{}#{rule}", layer_label(&diagnostic.source)),
            );
            let message = match diagnostic.severity {
                DiagnosticSeverity::Warning => diagnostic.message,
                DiagnosticSeverity::Error => format!("error: {}", diagnostic.message),
            };
            CheckFinding::warning(FindingCategory::Presentation, subject, message)
        })
        .collect()
}

/// Check `target` against the themes discovered under `roots`.
///
/// Profiles and bases are checked as overlays over the default theme. The
/// fact and property `schemas` of the enabled plugins are registered, so theme
/// rules that set a declared plugin property are type-checked instead of
/// reported as unknown.
///
/// # Errors
///
/// Returns an error when a theme file cannot be read or parsed, or the
/// selection cannot be resolved.
pub fn check_target(
    target: &CheckTarget,
    roots: &[ThemeDiscoveryRoot],
    schemas: &PluginPresentationSchemas,
) -> Result<ThemeCheck, CliError> {
    let (mut catalog, discovery) = match discover_themes(roots) {
        Ok(discovered) => (discovered.catalog, discovered.diagnostics),
        Err(_) => (
            ThemeCatalog::bundled().map_err(|error| CliError::Theme(error.to_string()))?,
            Vec::new(),
        ),
    };
    let (base, overlays) = match target {
        CheckTarget::Configured { theme, overlays } => (theme.clone(), overlays.clone()),
        CheckTarget::Id(id) => selection_for(&catalog, id),
        CheckTarget::File(path) => {
            let definition = read_theme_file(path)?;
            let id = definition.id().to_owned();
            catalog.insert(definition);
            selection_for(&catalog, &id)
        }
    };
    let mut report = check_theme(&catalog, &base, &overlays, &[], |resolved| {
        presentation_findings(resolved, schemas)
    })
    .map_err(|error| CliError::Theme(error.to_string()))?;
    report
        .findings
        .extend(discovery.into_iter().map(|diagnostic| {
            CheckFinding::info(
                FindingCategory::Discovery,
                diagnostic.path.display().to_string(),
                format!("theme skipped: {}", diagnostic.message),
            )
        }));
    report.finish();
    Ok(report)
}

/// Base and overlays that check `id`: itself for a theme, or an overlay over
/// the default theme for a profile or base. Unknown ids are checked as themes
/// so the resolver reports them.
fn selection_for(catalog: &ThemeCatalog, id: &str) -> (String, Vec<String>) {
    match catalog.definition(id).map(ThemeDefinition::kind) {
        Some(ThemeKind::Profile | ThemeKind::Base) => {
            (DEFAULT_BASE_THEME.to_owned(), vec![id.to_owned()])
        }
        Some(ThemeKind::Theme) | None => (id.to_owned(), Vec::new()),
    }
}

fn read_theme_file(path: &Path) -> Result<ThemeDefinition, CliError> {
    let bytes = crate::read_bytes_with_limit(
        std::fs::File::open(path).map_err(CliError::ThemeIo)?,
        crate::MAX_CLI_THEME_BYTES,
        "theme definition",
    )?;
    let source = String::from_utf8(bytes).map_err(|_| {
        CliError::InvalidArguments("theme definition must be valid UTF-8".to_owned())
    })?;
    parse_theme_definition(path.display().to_string(), &source)
        .map_err(|error| CliError::Theme(error.to_string()))
}

/// Write `report` as pretty JSON or as human-readable tables.
///
/// # Errors
///
/// Returns an error when writing or serializing fails.
pub fn write_check_report(
    report: &ThemeCheck,
    json: bool,
    writer: &mut impl std::io::Write,
) -> Result<(), CliError> {
    if json {
        writeln!(writer, "{}", serde_json::to_string_pretty(report)?)?;
        return Ok(());
    }
    let overlays = if report.overlays.is_empty() {
        String::new()
    } else {
        format!(" + {}", report.overlays.join(" + "))
    };
    writeln!(writer, "theme {}{overlays}", report.theme)?;
    for finding in &report.findings {
        write_finding(finding, writer)?;
    }
    for variant in &report.variants {
        write_variant(variant, writer)?;
    }
    writeln!(
        writer,
        "\n{} warning(s), {} note(s)",
        report.summary.warnings, report.summary.infos
    )?;
    Ok(())
}

fn write_variant(variant: &VariantCheck, writer: &mut impl std::io::Write) -> Result<(), CliError> {
    let names: Vec<_> = variant
        .variants
        .iter()
        .map(|variant| variant.name())
        .collect();
    writeln!(
        writer,
        "\nvariant {} ({})",
        names.join(", "),
        variant.applied_layers.join(" > ")
    )?;
    // Role keys are ASCII registry names, so padding aligns columns.
    let width = variant
        .contrast
        .iter()
        .map(|row| row.foreground.len().max(row.background.len()))
        .max()
        .unwrap_or(0);
    writeln!(
        writer,
        "  {:<9} {:<width$}  {:<width$}  {:>6}  {:>4}  result",
        "group", "foreground", "background", "ratio", "min"
    )?;
    for row in &variant.contrast {
        let ratio = row
            .ratio
            .map_or_else(|| "-".to_owned(), |ratio| format!("{ratio:.2}"));
        let background = if row.background == row.foreground {
            "(own)"
        } else {
            row.background
        };
        let note = row
            .note
            .as_deref()
            .map_or_else(String::new, |note| format!("  {note}"));
        let result = match row.outcome {
            ContrastOutcome::Fail => "FAIL",
            outcome => outcome.name(),
        };
        writeln!(
            writer,
            "  {:<9} {:<width$}  {background:<width$}  {ratio:>6}  {:>4.1}  {result}{note}",
            row.group.name(),
            row.foreground,
            row.minimum,
        )?;
    }
    let coverage = &variant.coverage;
    writeln!(
        writer,
        "  coverage  {}/{} registered roles resolve ({} authored, {} derived), {} unset",
        coverage.resolved(),
        coverage.registered,
        coverage.authored,
        coverage.derived.len(),
        coverage.unresolved.len()
    )?;
    if !coverage.derived.is_empty() {
        writeln!(writer, "  derived   {}", coverage.derived.join(", "))?;
    }
    if !coverage.unresolved.is_empty() {
        writeln!(writer, "  unset     {}", coverage.unresolved.join(", "))?;
    }
    for finding in &variant.findings {
        write_finding(finding, writer)?;
    }
    Ok(())
}

fn write_finding(finding: &CheckFinding, writer: &mut impl std::io::Write) -> Result<(), CliError> {
    writeln!(
        writer,
        "  {:<7}  {:<12}  {}: {}",
        finding.severity.name(),
        finding.category.name(),
        finding.subject,
        finding.message
    )?;
    Ok(())
}

/// Run `bcode theme check`.
///
/// Prints the report, then fails with an error when `strict` is set and the
/// report has warnings. Without `strict`, warnings never fail the command.
///
/// # Errors
///
/// Returns an error when configuration cannot be loaded, the theme cannot be
/// read or resolved, output fails, or `strict` is set and warnings exist.
pub fn run_check(
    theme: Option<&str>,
    strict: bool,
    json: bool,
    writer: &mut impl std::io::Write,
) -> Result<(), CliError> {
    let inputs = crate::theme_explain::load_explain_inputs(None, None)?;
    let target = CheckTarget::from_argument(theme, (inputs.theme, inputs.overlays));
    let report = check_target(&target, &inputs.roots, &inputs.plugin_schemas)?;
    write_check_report(&report, json, writer)?;
    strict_result(&report, strict)
}

/// The command result for `report`: an error under `strict` when it has
/// warnings.
///
/// # Errors
///
/// Returns [`CliError::Theme`] naming the warning count when `strict` is set
/// and the report has warnings.
pub fn strict_result(report: &ThemeCheck, strict: bool) -> Result<(), CliError> {
    let warnings = report.warning_count();
    if strict && warnings > 0 {
        return Err(CliError::Theme(format!(
            "theme check found {warnings} warning(s) (--strict)"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use bcode_tui_theme::check::Severity;
    use bcode_tui_theme::discovery::default_theme_roots;

    use super::*;

    fn roots() -> (tempfile::TempDir, Vec<ThemeDiscoveryRoot>) {
        let directory = tempfile::tempdir().expect("tempdir");
        let roots = default_theme_roots(directory.path(), directory.path(), &[]);
        (directory, roots)
    }

    const BAD_PROFILE: &str = r#"
schema_version = 2
id = "bad-profile"
kind = "profile"

[[presentation.rules]]
match = { kind = "tool", status = "failed" }
set = { disclosure = "hidden", sections = { error = { visible = false } } }

[[presentation.rules]]
match = { kind = "tool" }
set = { header = { fields = ["marker", "nonsense"] } }
"#;

    const BAD_THEME: &str = r##"
schema_version = 2
id = "bad-colors"
extends = ["seeded-base"]

[palette]
background = "#202020"
text = "#2a2a2a"
accent = "#303030"

[styles."text.primary"]
fg = "#262626"

[styles."bordr.focussed"]
fg = "$accent"
"##;

    fn write_fixture(
        directory: &tempfile::TempDir,
        name: &str,
        source: &str,
    ) -> std::path::PathBuf {
        let path = directory.path().join(name);
        std::fs::write(&path, source).expect("write fixture");
        path
    }

    #[test]
    fn every_bundled_theme_passes_without_strict() {
        let (_directory, roots) = roots();
        let catalog = ThemeCatalog::bundled().expect("bundled");
        let mut strict_failures = Vec::new();
        for definition in catalog.definitions() {
            let target = CheckTarget::Id(definition.id().to_owned());
            let report = check_target(&target, &roots, &PluginPresentationSchemas::default())
                .unwrap_or_else(|error| panic!("{}: {error}", definition.id()));
            assert!(strict_result(&report, false).is_ok());
            let mut output = Vec::new();
            write_check_report(&report, false, &mut output).expect("text");
            write_check_report(&report, true, &mut output).expect("json");
            if strict_result(&report, true).is_err() {
                strict_failures.push(format!("{} ({})", definition.id(), report.warning_count()));
            }
        }
        eprintln!("bundled themes failing --strict: {strict_failures:?}");
    }

    #[test]
    fn declared_plugin_properties_are_type_checked_instead_of_unknown() {
        use bcode_presentation_policy::models::{FactValue, PluginPropertySchema, PropertyType};
        const PLUGIN_PROFILE: &str = r#"
schema_version = 2
id = "git-profile"
kind = "profile"

[[presentation.rules]]
match = { kind = "tool" }
set = { plugin = { "test.git.show_destination" = false } }

[[presentation.rules]]
match = { kind = "tool" }
set = { plugin = { "test.git.max_files" = 99 } }
"#;
        let (directory, roots) = roots();
        let target = CheckTarget::File(write_fixture(
            &directory,
            "git-profile.toml",
            PLUGIN_PROFILE,
        ));
        let presentation = |report: &ThemeCheck| {
            let mut messages = report
                .all_findings()
                .filter(|finding| finding.category == FindingCategory::Presentation)
                .map(|finding| finding.message.clone())
                .collect::<Vec<_>>();
            messages.sort();
            messages.dedup();
            messages
        };

        // Without plugin schemas both properties are unknown.
        let unregistered =
            check_target(&target, &roots, &PluginPresentationSchemas::default()).expect("check");
        assert_eq!(
            presentation(&unregistered),
            [
                "unknown plugin property \"test.git.max_files\"",
                "unknown plugin property \"test.git.show_destination\"",
            ]
        );

        // With them, the valid value passes and the out-of-range one is
        // reported against its declaration.
        let schema = |value_type, default| PluginPropertySchema {
            plugin: "test.git".to_owned(),
            value_type,
            default,
        };
        let schemas = PluginPresentationSchemas {
            facts: std::collections::BTreeMap::new(),
            properties: std::collections::BTreeMap::from([
                (
                    "test.git.show_destination".to_owned(),
                    schema(PropertyType::Bool, FactValue::Bool(true)),
                ),
                (
                    "test.git.max_files".to_owned(),
                    schema(PropertyType::Int { min: 1, max: 20 }, FactValue::Int(5)),
                ),
            ]),
        };
        let registered = check_target(&target, &roots, &schemas).expect("check");
        assert_eq!(
            presentation(&registered),
            ["plugin property \"test.git.max_files\" ignored: 99 is outside 1..=20"]
        );
    }

    #[test]
    fn bad_theme_file_fails_strict_and_reports_each_category() {
        let (directory, roots) = roots();
        let path = write_fixture(&directory, "bad-colors.toml", BAD_THEME);
        let report = check_target(
            &CheckTarget::File(path),
            &roots,
            &PluginPresentationSchemas::default(),
        )
        .expect("check");
        assert!(strict_result(&report, false).is_ok());
        let Err(CliError::Theme(message)) = strict_result(&report, true) else {
            panic!("strict must fail");
        };
        assert!(message.contains("warning(s)"));
        let categories: Vec<_> = report
            .all_findings()
            .filter(|finding| finding.severity == Severity::Warning)
            .map(|finding| finding.category)
            .collect();
        assert!(categories.contains(&FindingCategory::Contrast));
        assert!(categories.contains(&FindingCategory::UnknownKey));

        let mut text = Vec::new();
        write_check_report(&report, false, &mut text).expect("text");
        let text = String::from_utf8(text).expect("utf8");
        assert!(text.contains("FAIL"), "{text}");
        assert!(text.contains("bordr.focussed"), "{text}");
    }

    #[test]
    fn bad_profile_reports_presentation_and_floor_crossing_warnings() {
        let (directory, roots) = roots();
        let path = write_fixture(&directory, "bad-profile.toml", BAD_PROFILE);
        let report = check_target(
            &CheckTarget::File(path),
            &roots,
            &PluginPresentationSchemas::default(),
        )
        .expect("check");
        assert_eq!(report.theme, DEFAULT_BASE_THEME);
        assert_eq!(report.overlays, ["bad-profile"]);
        let presentation: Vec<_> = report
            .all_findings()
            .filter(|finding| finding.category == FindingCategory::Presentation)
            .collect();
        assert!(
            presentation
                .iter()
                .any(|finding| finding.subject == "theme:bad-profile#0"),
            "{presentation:?}"
        );
        assert!(
            presentation
                .iter()
                .any(|finding| finding.subject == "theme:bad-profile#1"),
            "{presentation:?}"
        );
        assert!(strict_result(&report, true).is_err());
    }

    #[test]
    fn json_report_has_a_stable_shape() {
        let (directory, roots) = roots();
        let path = write_fixture(&directory, "bad-colors.toml", BAD_THEME);
        let report = check_target(
            &CheckTarget::File(path),
            &roots,
            &PluginPresentationSchemas::default(),
        )
        .expect("check");
        let mut output = Vec::new();
        write_check_report(&report, true, &mut output).expect("json");
        let value: serde_json::Value = serde_json::from_slice(&output).expect("valid json");
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["theme"], "bad-colors");
        let variant = &value["variants"][0];
        assert_eq!(variant["variants"][0], "dark");
        let row = &variant["contrast"][0];
        for key in [
            "group",
            "foreground",
            "background",
            "fg",
            "bg",
            "ratio",
            "minimum",
            "outcome",
            "note",
        ] {
            assert!(row.get(key).is_some(), "contrast row lacks {key}: {row}");
        }
        assert_eq!(row["outcome"], "fail");
        for key in ["registered", "authored", "derived", "unresolved"] {
            assert!(
                variant["coverage"].get(key).is_some(),
                "coverage lacks {key}"
            );
        }
        let finding = &variant["findings"][0];
        for key in ["severity", "category", "subject", "message"] {
            assert!(finding.get(key).is_some(), "finding lacks {key}");
        }
        assert_eq!(
            value["summary"]["warnings"],
            serde_json::json!(report.warning_count())
        );
    }

    #[test]
    fn contrast_adjustments_are_informational_in_text_and_json() {
        let (_directory, roots) = roots();
        let report = check_target(
            &CheckTarget::Id("nord".to_owned()),
            &roots,
            &PluginPresentationSchemas::default(),
        )
        .expect("check");
        assert!(
            strict_result(&report, true).is_ok(),
            "adjustments never fail --strict"
        );

        let mut text = Vec::new();
        write_check_report(&report, false, &mut text).expect("text");
        let text = String::from_utf8(text).expect("utf8");
        let line = text
            .lines()
            .find(|line| line.contains("state.error/canvas") && line.contains("adjusted"))
            .unwrap_or_else(|| panic!("no adjustment line in {text}"));
        assert!(line.trim_start().starts_with("info"), "{line}");
        assert!(line.contains("fg #bf616a \u{2192} #e17f87"), "{line}");

        let mut json = Vec::new();
        write_check_report(&report, true, &mut json).expect("json");
        let value: serde_json::Value = serde_json::from_slice(&json).expect("valid json");
        let adjustment = value["variants"]
            .as_array()
            .expect("variants")
            .iter()
            .flat_map(|variant| variant["adjustments"].as_array().expect("adjustments"))
            .find(|row| row["role"] == "state.error" && row["background"] == "canvas")
            .expect("state.error adjustment");
        assert_eq!(adjustment["channel"], "fg");
        assert_eq!(adjustment["before"], "#bf616a");
        assert_eq!(adjustment["after"], "#e17f87");
        for key in ["ratio_before", "ratio_after", "minimum"] {
            assert!(adjustment[key].is_f64(), "{key}: {adjustment}");
        }
        assert_eq!(value["summary"]["warnings"], 0);
    }

    #[test]
    fn arguments_select_configured_ids_and_files() {
        let configured = ("bcode".to_owned(), vec!["compact".to_owned()]);
        assert_eq!(
            CheckTarget::from_argument(None, configured.clone()),
            CheckTarget::Configured {
                theme: "bcode".to_owned(),
                overlays: vec!["compact".to_owned()],
            }
        );
        assert_eq!(
            CheckTarget::from_argument(Some("nord"), configured.clone()),
            CheckTarget::Id("nord".to_owned())
        );
        assert_eq!(
            CheckTarget::from_argument(Some("mine.toml"), configured),
            CheckTarget::File("mine.toml".into())
        );
    }

    #[test]
    fn check_subcommand_parses_and_appears_in_theme_help() {
        use clap::{CommandFactory as _, Parser as _};

        let cli =
            crate::Cli::try_parse_from(["bcode", "theme", "check", "nord", "--strict", "--json"])
                .expect("parse");
        assert!(matches!(
            cli.command,
            Some(crate::Commands::Theme {
                command: crate::ThemeCommand::Check {
                    theme: Some(ref theme),
                    strict: true,
                    json: true,
                }
            }) if theme == "nord"
        ));
        let cli = crate::Cli::try_parse_from(["bcode", "theme", "check"]).expect("parse");
        assert!(matches!(
            cli.command,
            Some(crate::Commands::Theme {
                command: crate::ThemeCommand::Check {
                    theme: None,
                    strict: false,
                    json: false,
                }
            })
        ));
        let mut command = crate::Cli::command();
        let help = command
            .find_subcommand_mut("theme")
            .expect("theme subcommand")
            .render_help()
            .to_string();
        assert!(help.contains("check"), "{help}");
    }

    #[test]
    fn unknown_theme_is_an_error() {
        let (_directory, roots) = roots();
        assert!(matches!(
            check_target(
                &CheckTarget::Id("no-such-theme".to_owned()),
                &roots,
                &PluginPresentationSchemas::default(),
            ),
            Err(CliError::Theme(_))
        ));
    }
}
