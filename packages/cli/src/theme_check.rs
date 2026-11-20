//! `bcode theme check`.
//!
//! The analysis lives in [`bcode_tui_theme::check`]; this module selects the
//! theme, compiles its structural rule layers for presentation diagnostics,
//! compares the findings with the accepted ones, and formats the report as a
//! table or JSON.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bcode_presentation_policy::models::PluginPresentationSchemas;
use bcode_presentation_policy::{CompiledPolicy, DiagnosticSeverity, PolicyRegistry};
use bcode_tui_theme::adjustment::ContrastPreferences;
use bcode_tui_theme::check::accepted::{ACCEPTED_FILE_NAME, MAX_ACCEPTED_FILE_BYTES};
use bcode_tui_theme::check::{
    Acceptance, AcceptanceItem, AcceptancePolicy, AcceptedFindings, BlessSummary, CheckFinding,
    ContrastOutcome, FindingCategory, ThemeCheck, VariantCheck, check_theme_adjusted,
};
use bcode_tui_theme::definition::{
    ResolvedThemeDefinition, ThemeCatalog, ThemeDefinition, ThemeKind, parse_theme_definition,
};
use bcode_tui_theme::discovery::{
    ThemeDiscoveryDiagnostic, ThemeDiscoveryRoot, ThemeExternalSource, discover_themes,
};
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

/// Where a checked theme's definition comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeOrigin {
    /// Shipped with Bcode.
    Bundled,
    /// A theme file (discovered, or given on the command line).
    File(PathBuf),
}

impl ThemeOrigin {
    /// The policy for accepting this theme's findings: bundled themes may
    /// accept contrast findings only on palette colors kept exactly.
    #[must_use]
    pub const fn policy(&self) -> AcceptancePolicy {
        match self {
            Self::Bundled => AcceptancePolicy::BUNDLED,
            Self::File(_) => AcceptancePolicy {
                palette_contrast_only: false,
            },
        }
    }

    /// The accepted-findings file for this theme: `override_path` when given,
    /// else `check-accepted.toml` next to the theme file, else the bundled
    /// accepted findings (`None`).
    #[must_use]
    pub fn accepted_path(&self, override_path: Option<&Path>) -> Option<PathBuf> {
        override_path.map(Path::to_path_buf).or_else(|| match self {
            Self::Bundled => None,
            Self::File(path) => Some(
                path.parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(ACCEPTED_FILE_NAME),
            ),
        })
    }
}

/// One checked selection with the origin of its checked definition.
#[derive(Debug, Clone)]
pub struct CheckedTheme {
    /// The report.
    pub report: ThemeCheck,
    /// Origin of the checked theme (or profile).
    pub origin: ThemeOrigin,
}

/// The discovered catalog with each external theme's source.
struct CheckCatalog {
    catalog: ThemeCatalog,
    sources: BTreeMap<String, ThemeExternalSource>,
    discovery: Vec<ThemeDiscoveryDiagnostic>,
}

impl CheckCatalog {
    fn discover(roots: &[ThemeDiscoveryRoot]) -> Result<Self, CliError> {
        Ok(match discover_themes(roots) {
            Ok(discovered) => Self {
                catalog: discovered.catalog,
                sources: discovered.sources,
                discovery: discovered.diagnostics,
            },
            Err(_) => Self {
                catalog: ThemeCatalog::bundled()
                    .map_err(|error| CliError::Theme(error.to_string()))?,
                sources: BTreeMap::new(),
                discovery: Vec::new(),
            },
        })
    }

    fn origin(&self, id: &str) -> ThemeOrigin {
        self.sources.get(id).map_or(ThemeOrigin::Bundled, |source| {
            ThemeOrigin::File(source.path.clone())
        })
    }

    fn check(
        &self,
        base: &str,
        overlays: &[String],
        schemas: &PluginPresentationSchemas,
        contrast: Option<&ContrastPreferences>,
    ) -> Result<ThemeCheck, CliError> {
        let settings = contrast.map(|contrast| contrast.settings_for(base));
        let mut report = check_theme_adjusted(
            &self.catalog,
            base,
            overlays,
            &[],
            settings.as_ref(),
            |resolved| presentation_findings(resolved, schemas),
        )
        .map_err(|error| CliError::Theme(error.to_string()))?;
        report
            .findings
            .extend(self.discovery.iter().map(|diagnostic| {
                CheckFinding::info(
                    FindingCategory::Discovery,
                    diagnostic.path.display().to_string(),
                    format!("theme skipped: {}", diagnostic.message),
                )
            }));
        report.finish();
        Ok(report)
    }
}

/// Check `target` against the themes discovered under `roots`, reporting
/// where the checked definition comes from.
///
/// Profiles and bases are checked as overlays over the default theme. The
/// fact and property `schemas` of the enabled plugins are registered, so theme
/// rules that set a declared plugin property are type-checked instead of
/// reported as unknown. With `contrast`, the colors its settings for the
/// checked theme produce are measured instead of the authored ones.
///
/// # Errors
///
/// Returns an error when a theme file cannot be read or parsed, or the
/// selection cannot be resolved.
pub fn check_target_with_origin(
    target: &CheckTarget,
    roots: &[ThemeDiscoveryRoot],
    schemas: &PluginPresentationSchemas,
    contrast: Option<&ContrastPreferences>,
) -> Result<CheckedTheme, CliError> {
    let mut catalog = CheckCatalog::discover(roots)?;
    let (base, overlays, origin) = match target {
        CheckTarget::Configured { theme, overlays } => {
            (theme.clone(), overlays.clone(), catalog.origin(theme))
        }
        CheckTarget::Id(id) => {
            let (base, overlays) = selection_for(&catalog.catalog, id);
            (base, overlays, catalog.origin(id))
        }
        CheckTarget::File(path) => {
            let definition = read_theme_file(path)?;
            let id = definition.id().to_owned();
            catalog.catalog.insert(definition);
            let (base, overlays) = selection_for(&catalog.catalog, &id);
            (base, overlays, ThemeOrigin::File(path.clone()))
        }
    };
    let report = catalog.check(&base, &overlays, schemas, contrast)?;
    Ok(CheckedTheme { report, origin })
}

/// Check every definition in the catalog: themes alone, profiles and bases
/// over the default theme, in catalog order, measuring the colors `contrast`
/// produces when it is set.
///
/// # Errors
///
/// Returns an error when a selection cannot be resolved.
pub fn check_all(
    roots: &[ThemeDiscoveryRoot],
    schemas: &PluginPresentationSchemas,
    contrast: Option<&ContrastPreferences>,
) -> Result<Vec<CheckedTheme>, CliError> {
    let catalog = CheckCatalog::discover(roots)?;
    let ids: Vec<String> = catalog
        .catalog
        .definitions()
        .map(|definition| definition.id().to_owned())
        .collect();
    ids.iter()
        .map(|id| {
            let (base, overlays) = selection_for(&catalog.catalog, id);
            Ok(CheckedTheme {
                report: catalog.check(&base, &overlays, schemas, contrast)?,
                origin: catalog.origin(id),
            })
        })
        .collect()
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
    if let Some(adjustments) = &report.adjustments {
        writeln!(
            writer,
            "measured with contrast {:.2}, minimum contrast {}, parts {}",
            adjustments.contrast,
            adjustments
                .minimum_contrast
                .map_or_else(|| "off".to_owned(), |ratio| format!("{ratio:.1}")),
            if adjustments.parts.is_empty() {
                "none".to_owned()
            } else {
                adjustments.parts.join(", ")
            }
        )?;
        writeln!(
            writer,
            "measured with vibrance {:.2}, richness {:.2}, background tint {:.2}, color parts {}",
            adjustments.vibrance,
            adjustments.richness,
            adjustments.background_tint,
            if adjustments.color_parts.is_empty() {
                "none".to_owned()
            } else {
                adjustments.color_parts.join(", ")
            }
        )?;
    }
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
    if let Some(acceptance) = &report.acceptance {
        write_acceptance(acceptance, writer)?;
    }
    Ok(())
}

fn write_acceptance(
    acceptance: &Acceptance,
    writer: &mut impl std::io::Write,
) -> Result<(), CliError> {
    let summary = acceptance.summary;
    writeln!(
        writer,
        "accepted findings ({}): {} accepted, {} improved, {} new, {} worse, {} stale",
        acceptance.source,
        summary.accepted,
        summary.improved,
        summary.new,
        summary.worse,
        summary.stale
    )?;
    let recorded = |item: &AcceptanceItem| {
        item.accepted_ratio
            .map_or_else(String::new, |ratio| format!(" (accepted {ratio:.2})"))
    };
    for item in &acceptance.new {
        let note = item
            .note
            .as_deref()
            .map_or_else(String::new, |note| format!(" ({note})"));
        writeln!(writer, "  new       {}{note}", item.finding.describe())?;
    }
    for item in &acceptance.worse {
        writeln!(
            writer,
            "  worse     {}{}",
            item.finding.describe(),
            recorded(item)
        )?;
    }
    for item in &acceptance.improved {
        writeln!(
            writer,
            "  improved  {}{}",
            item.finding.describe(),
            recorded(item)
        )?;
    }
    for entry in &acceptance.stale {
        writeln!(writer, "  stale     {}", entry.describe())?;
    }
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
    let status = finding
        .acceptance
        .map_or_else(String::new, |status| format!("  [{}]", status.name()));
    writeln!(
        writer,
        "  {:<7}  {:<12}  {}: {}{status}",
        finding.severity.name(),
        finding.category.name(),
        finding.subject,
        finding.message
    )?;
    Ok(())
}

/// Which selections `bcode theme check` checks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CheckSelection<'a> {
    /// The configured theme with its profile and overlays.
    #[default]
    Configured,
    /// One theme id, profile id, or theme file.
    Theme(&'a str),
    /// Every definition in the catalog.
    All,
}

impl<'a> CheckSelection<'a> {
    /// The selection for the `THEME` argument and `--all` flag.
    #[must_use]
    pub const fn new(theme: Option<&'a str>, all: bool) -> Self {
        match (theme, all) {
            (_, true) => Self::All,
            (Some(theme), false) => Self::Theme(theme),
            (None, false) => Self::Configured,
        }
    }
}

/// Options of `bcode theme check`.
// One field per command-line flag.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CheckOptions<'a> {
    /// What to check.
    pub selection: CheckSelection<'a>,
    /// Fail on stale accepted entries too.
    pub strict: bool,
    /// Emit JSON.
    pub json: bool,
    /// Accept the current findings.
    pub bless: bool,
    /// Accepted-findings file to use instead of each theme's own.
    pub accepted: Option<&'a Path>,
    /// Measure the colors the configured contrast settings produce instead of the authored
    /// ones. Accepted findings describe authored themes, so they are not compared.
    pub with_adjustments: bool,
}

/// Run `bcode theme check`.
///
/// Prints the report, then fails when a finding is new or worse than
/// accepted, or (under `strict`) an accepted entry is stale. With `bless`,
/// the current findings are written to the accepted-findings files first.
///
/// # Errors
///
/// Returns an error when configuration cannot be loaded, the theme cannot be
/// read or resolved, an accepted-findings file cannot be read or written,
/// `bless` targets the bundled accepted findings without `--accepted`, `bless`
/// is combined with `with_adjustments`, output fails, or the check fails.
pub fn run_check(
    options: &CheckOptions<'_>,
    writer: &mut impl std::io::Write,
) -> Result<(), CliError> {
    if options.bless && options.with_adjustments {
        return Err(CliError::InvalidArguments(
            "--bless accepts findings of authored themes; drop --with-adjustments".to_owned(),
        ));
    }
    let inputs = crate::theme_explain::load_explain_inputs(None, None)?;
    let contrast = options.with_adjustments.then_some(&inputs.contrast);
    let all = options.selection == CheckSelection::All;
    let mut checked = if all {
        check_all(&inputs.roots, &inputs.plugin_schemas, contrast)?
    } else {
        let theme = match options.selection {
            CheckSelection::Theme(theme) => Some(theme),
            CheckSelection::Configured | CheckSelection::All => None,
        };
        let target = CheckTarget::from_argument(theme, (inputs.theme, inputs.overlays));
        vec![check_target_with_origin(
            &target,
            &inputs.roots,
            &inputs.plugin_schemas,
            contrast,
        )?]
    };
    let mut store = AcceptedStore::default();
    if options.bless {
        bless(&checked, options, &mut store, writer)?;
    }
    if !options.with_adjustments {
        apply_accepted(&mut checked, options.accepted, &mut store)?;
    }
    let reports: Vec<&ThemeCheck> = checked.iter().map(|theme| &theme.report).collect();
    if options.json {
        if all {
            writeln!(writer, "{}", serde_json::to_string_pretty(&reports)?)?;
        } else {
            writeln!(writer, "{}", serde_json::to_string_pretty(reports[0])?)?;
        }
    } else {
        for (index, report) in reports.iter().enumerate() {
            if index > 0 {
                writeln!(writer)?;
            }
            write_check_report(report, false, writer)?;
        }
    }
    if options.with_adjustments {
        return adjusted_check_result(&reports);
    }
    check_result(&reports, options.strict)
}

/// Fail when the colors the user's contrast settings produce have warnings. There is nothing
/// to accept: accepted findings describe authored themes.
fn adjusted_check_result(reports: &[&ThemeCheck]) -> Result<(), CliError> {
    let warnings: usize = reports.iter().map(|report| report.warning_count()).sum();
    if warnings == 0 {
        return Ok(());
    }
    Err(CliError::Theme(format!(
        "theme check found {warnings} finding(s) with the configured contrast adjustments"
    )))
}

/// Compare each checked selection with its accepted findings (from
/// `accepted` when given).
fn apply_accepted(
    checked: &mut [CheckedTheme],
    accepted: Option<&Path>,
    store: &mut AcceptedStore,
) -> Result<(), CliError> {
    for theme in checked {
        let path = theme.origin.accepted_path(accepted);
        let source = path
            .as_ref()
            .map_or_else(|| "bundled".to_owned(), |path| path.display().to_string());
        let findings = store.load(path.as_deref())?;
        theme
            .report
            .apply_acceptance(findings, theme.origin.policy(), source);
    }
    Ok(())
}

/// Accepted-findings files read so far, by path (`None` is the bundled set).
#[derive(Default)]
struct AcceptedStore {
    files: BTreeMap<Option<PathBuf>, AcceptedFindings>,
}

impl AcceptedStore {
    fn load(&mut self, path: Option<&Path>) -> Result<&mut AcceptedFindings, CliError> {
        let key = path.map(Path::to_path_buf);
        if !self.files.contains_key(&key) {
            let findings = read_accepted(path)?;
            self.files.insert(key.clone(), findings);
        }
        Ok(self.files.get_mut(&key).expect("inserted above"))
    }
}

/// Read the accepted findings at `path`, or the bundled ones. A missing file
/// accepts nothing.
fn read_accepted(path: Option<&Path>) -> Result<AcceptedFindings, CliError> {
    let Some(path) = path else {
        return AcceptedFindings::bundled().map_err(|error| CliError::Theme(error.to_string()));
    };
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AcceptedFindings::default());
        }
        Err(error) => return Err(CliError::ThemeIo(error)),
    };
    let bytes = crate::read_bytes_with_limit(file, MAX_ACCEPTED_FILE_BYTES, "accepted findings")?;
    let source = String::from_utf8(bytes).map_err(|_| {
        CliError::InvalidArguments("accepted findings must be valid UTF-8".to_owned())
    })?;
    AcceptedFindings::parse(&source)
        .map_err(|error| CliError::Theme(format!("{}: {error}", path.display())))
}

/// Write each checked selection's current findings to its accepted-findings
/// file. Bundled themes are written only to an explicit `--accepted` file:
/// checking one bundled theme without it is an error, and `--all` skips them
/// with a note.
fn bless(
    checked: &[CheckedTheme],
    options: &CheckOptions<'_>,
    store: &mut AcceptedStore,
    writer: &mut impl std::io::Write,
) -> Result<(), CliError> {
    let mut written: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    let mut skipped = 0;
    for theme in checked {
        let Some(path) = theme.origin.accepted_path(options.accepted) else {
            if options.selection != CheckSelection::All {
                return Err(CliError::InvalidArguments(format!(
                    "{} is a bundled theme; its accepted findings ship with Bcode. \
                     Pass --accepted <PATH> to write them to a file",
                    theme.report.theme
                )));
            }
            skipped += 1;
            continue;
        };
        let summary = store
            .load(Some(&path))?
            .bless(&theme.report, theme.origin.policy());
        if summary == BlessSummary::default() {
            written.entry(path).or_default();
            continue;
        }
        let mut selection = theme.report.theme.clone();
        for overlay in &theme.report.overlays {
            selection.push_str(" + ");
            selection.push_str(overlay);
        }
        let refused = if summary.refused == 0 {
            String::new()
        } else {
            format!(
                ", {} refused (derived colors must meet their threshold)",
                summary.refused
            )
        };
        written.entry(path).or_default().push(format!(
            "{selection}: {} added, {} updated, {} kept, {} removed{refused}",
            summary.added, summary.updated, summary.kept, summary.removed
        ));
    }
    for (path, lines) in &written {
        let findings = store.load(Some(path))?;
        std::fs::write(path, findings.to_toml()).map_err(CliError::ThemeIo)?;
        if !options.json {
            writeln!(writer, "blessed {}", path.display())?;
            for line in lines {
                writeln!(writer, "  {line}")?;
            }
        }
    }
    if skipped > 0 && !options.json {
        writeln!(
            writer,
            "skipped {skipped} bundled selection(s): pass --accepted <PATH> to write their accepted findings"
        )?;
    }
    if !options.json && !written.is_empty() {
        writeln!(writer)?;
    }
    Ok(())
}

/// The command result for `reports` compared with their accepted findings:
/// an error naming the new and worse findings, and under `strict` the stale
/// entries. A report without a comparison fails on any warning.
///
/// # Errors
///
/// Returns [`CliError::Theme`] when any report fails.
pub fn check_result(reports: &[&ThemeCheck], strict: bool) -> Result<(), CliError> {
    let (mut new, mut worse, mut stale) = (0, 0, 0);
    for report in reports {
        match &report.acceptance {
            Some(acceptance) => {
                new += acceptance.summary.new;
                worse += acceptance.summary.worse;
                stale += acceptance.summary.stale;
            }
            None => new += report.warning_count(),
        }
    }
    let stale_fails = strict && stale > 0;
    if new == 0 && worse == 0 && !stale_fails {
        return Ok(());
    }
    let mut parts = Vec::new();
    if new > 0 {
        parts.push(format!("{new} new"));
    }
    if worse > 0 {
        parts.push(format!("{worse} worse"));
    }
    if stale_fails {
        parts.push(format!("{stale} stale (--strict)"));
    }
    Err(CliError::Theme(format!(
        "theme check found {} finding(s); review them and accept with --bless",
        parts.join(", ")
    )))
}

#[cfg(test)]
mod tests {
    use bcode_tui_theme::check::Severity;
    use bcode_tui_theme::discovery::default_theme_roots;

    use super::*;

    fn check_target(
        target: &CheckTarget,
        roots: &[ThemeDiscoveryRoot],
        schemas: &PluginPresentationSchemas,
    ) -> Result<ThemeCheck, CliError> {
        check_target_with_origin(target, roots, schemas, None).map(|checked| checked.report)
    }

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
    fn every_bundled_theme_passes_strict_with_the_bundled_accepted_findings() {
        let (_directory, roots) = roots();
        let mut checked =
            check_all(&roots, &PluginPresentationSchemas::default(), None).expect("check all");
        assert_eq!(
            checked.len(),
            ThemeCatalog::bundled()
                .expect("bundled")
                .definitions()
                .count()
        );
        assert!(
            checked
                .iter()
                .all(|theme| theme.origin == ThemeOrigin::Bundled)
        );
        apply_accepted(&mut checked, None, &mut AcceptedStore::default()).expect("accepted");
        let reports: Vec<_> = checked.iter().map(|theme| &theme.report).collect();
        check_result(&reports, true).expect("bundled themes pass --strict");
        // Bundled themes with upstream low contrast pass through acceptance.
        let accepted: usize = reports
            .iter()
            .map(|report| {
                report
                    .acceptance
                    .as_ref()
                    .expect("compared")
                    .summary
                    .accepted
            })
            .sum();
        assert!(accepted > 0);
        for report in reports {
            let mut output = Vec::new();
            write_check_report(report, false, &mut output).expect("text");
            write_check_report(report, true, &mut output).expect("json");
        }
    }

    /// A user theme file whose kept palette accent is below its threshold.
    const LOW_ACCENT_THEME: &str = r##"
schema_version = 2
id = "low-accent"
extends = ["seeded-base"]

[palette]
background = "#1e1e1e"
text = "#dddddd"
accent = "#30306a"
"##;

    fn check_file(path: &Path, roots: &[ThemeDiscoveryRoot]) -> CheckedTheme {
        check_target_with_origin(
            &CheckTarget::File(path.to_path_buf()),
            roots,
            &PluginPresentationSchemas::default(),
            None,
        )
        .expect("check")
    }

    fn compared(path: &Path, roots: &[ThemeDiscoveryRoot]) -> CheckedTheme {
        let mut checked = [check_file(path, roots)];
        apply_accepted(&mut checked, None, &mut AcceptedStore::default()).expect("accepted");
        let [checked] = checked;
        checked
    }

    fn bless_options() -> CheckOptions<'static> {
        CheckOptions {
            bless: true,
            ..CheckOptions::default()
        }
    }

    #[test]
    fn user_theme_findings_are_accepted_from_the_file_next_to_it() {
        let (directory, roots) = roots();
        let path = write_fixture(&directory, "low-accent.toml", LOW_ACCENT_THEME);
        let accepted_path = directory.path().join(ACCEPTED_FILE_NAME);

        // Without accepted findings every warning is new and fails.
        let checked = compared(&path, &roots);
        assert_eq!(checked.origin, ThemeOrigin::File(path.clone()));
        let error = check_result(&[&checked.report], false).expect_err("new findings fail");
        assert!(error.to_string().contains("new"), "{error}");

        // Blessing writes the file next to the theme; then the check passes.
        let mut output = Vec::new();
        bless(
            &[check_file(&path, &roots)],
            &bless_options(),
            &mut AcceptedStore::default(),
            &mut output,
        )
        .expect("bless");
        let output = String::from_utf8(output).expect("utf8");
        assert!(output.contains("blessed"), "{output}");
        assert!(output.contains("low-accent:"), "{output}");
        let written = std::fs::read_to_string(&accepted_path).expect("accepted file");
        assert!(written.contains("foreground = \"accent\""), "{written}");
        let checked = compared(&path, &roots);
        check_result(&[&checked.report], true).expect("accepted findings pass");
        let mut text = Vec::new();
        write_check_report(&checked.report, false, &mut text).expect("text");
        let text = String::from_utf8(text).expect("utf8");
        assert!(text.contains("[accepted]"), "{text}");
        assert!(text.contains("0 new, 0 worse, 0 stale"), "{text}");

        // A reason survives blessing again.
        let with_reason = written.replacen(
            "foreground = \"accent\"",
            "foreground = \"accent\"\nreason = \"chosen\"",
            1,
        );
        std::fs::write(&accepted_path, &with_reason).expect("write");
        bless(
            &[check_file(&path, &roots)],
            &bless_options(),
            &mut AcceptedStore::default(),
            &mut Vec::new(),
        )
        .expect("bless again");
        let reblessed = std::fs::read_to_string(&accepted_path).expect("accepted file");
        assert!(reblessed.contains("reason = \"chosen\""), "{reblessed}");
    }

    #[test]
    fn worse_new_and_stale_findings_are_reported() {
        let (directory, roots) = roots();
        let path = write_fixture(&directory, "low-accent.toml", LOW_ACCENT_THEME);
        bless(
            &[check_file(&path, &roots)],
            &bless_options(),
            &mut AcceptedStore::default(),
            &mut Vec::new(),
        )
        .expect("bless");

        // A darker accent lowers the accepted ratios and adds no entry.
        std::fs::write(&path, LOW_ACCENT_THEME.replace("#30306a", "#262650")).expect("write");
        let checked = compared(&path, &roots);
        let acceptance = checked.report.acceptance.as_ref().expect("compared");
        assert!(acceptance.summary.worse > 0, "{acceptance:?}");
        assert!(check_result(&[&checked.report], false).is_err());
        let mut json = Vec::new();
        write_check_report(&checked.report, true, &mut json).expect("json");
        let value: serde_json::Value = serde_json::from_slice(&json).expect("json");
        assert!(value["acceptance"]["worse"][0]["accepted_ratio"].is_f64());

        // A new unknown key is new; the original accent entries are stale once
        // the accent reads.
        std::fs::write(
            &path,
            LOW_ACCENT_THEME.replace("#30306a", "#9ab8ff")
                + "\n[styles.\"bordr.focussed\"]\nfg = \"$accent\"\n",
        )
        .expect("write");
        let checked = compared(&path, &roots);
        let acceptance = checked.report.acceptance.as_ref().expect("compared");
        assert_eq!(acceptance.summary.worse, 0);
        assert!(acceptance.summary.stale > 0, "{acceptance:?}");
        assert!(
            acceptance
                .new
                .iter()
                .any(|item| item.finding.kind == FindingCategory::UnknownKey),
            "{acceptance:?}"
        );
        let mut text = Vec::new();
        write_check_report(&checked.report, false, &mut text).expect("text");
        let text = String::from_utf8(text).expect("utf8");
        assert!(text.contains("  new       "), "{text}");
        assert!(text.contains("  stale     "), "{text}");
        assert!(text.contains("[new]"), "{text}");
    }

    #[test]
    fn stale_entries_fail_only_strict() {
        let (directory, roots) = roots();
        let path = write_fixture(&directory, "low-accent.toml", LOW_ACCENT_THEME);
        bless(
            &[check_file(&path, &roots)],
            &bless_options(),
            &mut AcceptedStore::default(),
            &mut Vec::new(),
        )
        .expect("bless");
        std::fs::write(&path, LOW_ACCENT_THEME.replace("#30306a", "#9ab8ff")).expect("write");
        let checked = compared(&path, &roots);
        assert!(
            checked
                .report
                .acceptance
                .as_ref()
                .expect("compared")
                .summary
                .stale
                > 0
        );
        check_result(&[&checked.report], false).expect("stale entries warn");
        let error = check_result(&[&checked.report], true).expect_err("strict fails");
        assert!(error.to_string().contains("stale"), "{error}");
    }

    #[test]
    fn with_adjustments_measures_the_colors_the_contrast_settings_produce() {
        let (_directory, roots) = roots();
        let contrast_findings = |contrast: Option<&ContrastPreferences>| {
            let report = check_target_with_origin(
                &CheckTarget::Id("solarized".to_owned()),
                &roots,
                &PluginPresentationSchemas::default(),
                contrast,
            )
            .expect("check")
            .report;
            let count = report
                .all_findings()
                .filter(|finding| finding.category == FindingCategory::Contrast)
                .count();
            (report, count)
        };
        let (authored, authored_count) = contrast_findings(None);
        assert!(authored.adjustments.is_none());
        assert!(authored_count > 0, "solarized has upstream low contrast");
        let floor: ContrastPreferences =
            toml::from_str("minimum_contrast = 7.0").expect("preferences");
        let (adjusted, adjusted_count) = contrast_findings(Some(&floor));
        assert!(
            adjusted_count < authored_count,
            "{adjusted_count} < {authored_count}"
        );
        let measured = adjusted
            .adjustments
            .as_ref()
            .expect("measured with adjustments");
        assert_eq!(measured.minimum_contrast, Some(7.0));
        assert_eq!(measured.parts, ["text", "syntax", "fills", "ansi"]);
        let mut text = Vec::new();
        write_check_report(&adjusted, false, &mut text).expect("text");
        let text = String::from_utf8(text).expect("utf8");
        assert!(
            text.contains("measured with contrast 1.00, minimum contrast 7.0"),
            "{text}"
        );
        assert!(
            text.contains("measured with vibrance 0.00, richness 0.00, background tint 1.00"),
            "{text}"
        );

        // The color settings are measured too: greying the canvas changes what is measured.
        let grey: ContrastPreferences =
            toml::from_str("background_tint = 0.0\nvibrance = 0.5").expect("preferences");
        let (colored, _) = contrast_findings(Some(&grey));
        let colored_measured = colored.adjustments.as_ref().expect("measured");
        assert_eq!(colored_measured.background_tint, 0.0);
        assert_eq!(colored_measured.vibrance, 0.5);
        assert_eq!(
            colored_measured.color_parts,
            ["syntax", "accents", "fills", "ansi"]
        );
        let canvas = |report: &bcode_tui_theme::check::ThemeCheck| {
            serde_json::to_value(report).expect("json")["variants"][0]["contrast"]
                .as_array()
                .expect("rows")
                .iter()
                .find(|row| row["background"] == "canvas")
                .map(|row| row["bg"].clone())
        };
        assert_ne!(
            canvas(&colored),
            canvas(&authored),
            "the canvas is measured greyed"
        );
        let error = run_check(
            &CheckOptions {
                bless: true,
                with_adjustments: true,
                ..CheckOptions::default()
            },
            &mut Vec::new(),
        )
        .expect_err("bless needs authored themes");
        assert!(error.to_string().contains("--with-adjustments"), "{error}");
    }

    #[test]
    fn blessing_a_bundled_theme_needs_an_explicit_file() {
        let (directory, roots) = roots();
        let checked = || {
            check_target_with_origin(
                &CheckTarget::Id("solarized".to_owned()),
                &roots,
                &PluginPresentationSchemas::default(),
                None,
            )
            .expect("check")
        };
        let error = bless(
            &[checked()],
            &bless_options(),
            &mut AcceptedStore::default(),
            &mut Vec::new(),
        )
        .expect_err("bundled accepted findings are not writable");
        assert!(error.to_string().contains("--accepted"), "{error}");

        let explicit = directory.path().join("bundled-accepted.toml");
        let options = CheckOptions {
            accepted: Some(&explicit),
            ..bless_options()
        };
        bless(
            &[checked()],
            &options,
            &mut AcceptedStore::default(),
            &mut Vec::new(),
        )
        .expect("bless to an explicit file");
        // The blessed entries equal the bundled ones for this theme.
        let written =
            AcceptedFindings::parse(&std::fs::read_to_string(&explicit).expect("written"))
                .expect("parse");
        let bundled = AcceptedFindings::bundled().expect("bundled");
        let solarized: Vec<_> = bundled
            .entries
            .iter()
            .filter(|entry| entry.theme == "solarized")
            .cloned()
            .collect();
        assert_ne!(solarized.len(), 0);
        let strip = |entries: Vec<bcode_tui_theme::check::AcceptedFinding>| {
            entries
                .into_iter()
                .map(|entry| bcode_tui_theme::check::AcceptedFinding {
                    reason: None,
                    ..entry
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(strip(written.entries), strip(solarized));
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
        let Err(CliError::Theme(message)) = check_result(&[&report], false) else {
            panic!("unaccepted warnings must fail");
        };
        assert!(message.contains("new"), "{message}");
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
        assert!(check_result(&[&report], false).is_err());
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
        let (directory, roots) = roots();
        // A user theme that opts its authored colors into contrast enforcement, with an error
        // red below 4.5:1 on its canvas.
        std::fs::create_dir_all(directory.path().join("themes")).expect("themes dir");
        write_fixture(
            &directory,
            "themes/enforced.toml",
            r##"
schema_version = 1
id = "enforced"
extends = ["terminal-native-structured"]
contrast = "enforce"

[palette]
background = "#2e3440"
error = "#bf616a"
text = "#eceff4"

[styles.canvas]
bg = "$background"
fg = "$text"

[styles."state.error"]
fg = "$error"
"##,
        );
        let report = check_target(
            &CheckTarget::Id("enforced".to_owned()),
            &roots,
            &PluginPresentationSchemas::default(),
        )
        .expect("check");
        assert!(
            check_result(&[&report], true).is_ok(),
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
                    all: false,
                    strict: true,
                    json: true,
                    bless: false,
                    accepted: None,
                    with_adjustments: false,
                }
            }) if theme == "nord"
        ));
        let cli = crate::Cli::try_parse_from(["bcode", "theme", "check"]).expect("parse");
        assert!(matches!(
            cli.command,
            Some(crate::Commands::Theme {
                command: crate::ThemeCommand::Check {
                    theme: None,
                    all: false,
                    strict: false,
                    json: false,
                    bless: false,
                    accepted: None,
                    with_adjustments: false,
                }
            })
        ));
        let cli = crate::Cli::try_parse_from([
            "bcode",
            "theme",
            "check",
            "solarized",
            "--with-adjustments",
        ])
        .expect("parse");
        assert!(matches!(
            cli.command,
            Some(crate::Commands::Theme {
                command: crate::ThemeCommand::Check {
                    with_adjustments: true,
                    bless: false,
                    ..
                }
            })
        ));
        assert!(
            crate::Cli::try_parse_from([
                "bcode",
                "theme",
                "check",
                "--bless",
                "--with-adjustments",
            ])
            .is_err(),
            "--bless and --with-adjustments conflict"
        );
        let cli = crate::Cli::try_parse_from([
            "bcode",
            "theme",
            "check",
            "--all",
            "--bless",
            "--accepted",
            "accepted.toml",
        ])
        .expect("parse");
        assert!(matches!(
            cli.command,
            Some(crate::Commands::Theme {
                command: crate::ThemeCommand::Check {
                    theme: None,
                    all: true,
                    bless: true,
                    accepted: Some(ref path),
                    ..
                }
            }) if path == Path::new("accepted.toml")
        ));
        assert!(
            crate::Cli::try_parse_from(["bcode", "theme", "check", "nord", "--all"]).is_err(),
            "a theme and --all conflict"
        );
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
