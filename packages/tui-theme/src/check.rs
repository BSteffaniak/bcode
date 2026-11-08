//! Theme quality analysis behind `bcode theme check`.
//!
//! [`check_theme`] resolves a selection in both light/dark variants and
//! reports, per distinct resolution:
//!
//! * WCAG contrast ratios for the role pairs that carry text and state (text
//!   on surfaces, accents, status colors, diffs, selection) against fixed
//!   thresholds;
//! * unknown style roles and container keys;
//! * roles resolved only through fallbacks (seed derivation or host defaults);
//! * role registry coverage;
//! * resolver warnings and caller-supplied structural presentation diagnostics.
//!
//! The analysis is pure and renderer-neutral: it reads resolved theme data
//! and produces a serializable [`ThemeCheck`]. Callers format it and decide
//! exit status (see [`ThemeCheck::warning_count`]).

use std::collections::BTreeSet;

use serde::Serialize;

use crate::contrast::{
    CanvasColors, PAIRS, PairBackground, PairSpec, RATIO_TOLERANCE, measurable, pair_colors,
};
use crate::definition::{
    REQUIRED_CONTAINER_ROLES, REQUIRED_SEMANTIC_STYLE_ROLES, ResolvedThemeDefinition,
    ResolvedThemeVariant, ThemeCatalog, ThemeError, ThemeSelection,
};
use crate::roles;

/// Version of the serialized [`ThemeCheck`] structure.
pub const CHECK_REPORT_VERSION: u32 = 1;
/// WCAG 2.x AA minimum contrast for body text.
pub const TEXT_CONTRAST: f64 = 4.5;
/// WCAG 2.x AA minimum contrast for secondary text and non-text UI.
pub const SECONDARY_CONTRAST: f64 = 3.0;

/// How serious a finding is. Only warnings affect `--strict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Informational; never fails a check.
    Info,
    /// A problem worth fixing; fails a strict check.
    Warning,
}

impl Severity {
    /// Stable lowercase name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
        }
    }
}

/// What a finding is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingCategory {
    /// A contrast pair below its threshold.
    Contrast,
    /// A style role or container key Bcode does not recognize.
    UnknownKey,
    /// Roles that resolve only through seed derivation or host fallbacks.
    Fallback,
    /// Required roles or containers that do not resolve.
    Coverage,
    /// A resolver warning, such as approximate color math.
    Theme,
    /// A structural presentation rule diagnostic, including floor crossings.
    Presentation,
    /// Theme discovery problems outside the checked theme.
    Discovery,
}

impl FindingCategory {
    /// Stable snake-case name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Contrast => "contrast",
            Self::UnknownKey => "unknown_key",
            Self::Fallback => "fallback",
            Self::Coverage => "coverage",
            Self::Theme => "theme",
            Self::Presentation => "presentation",
            Self::Discovery => "discovery",
        }
    }
}

/// One reported problem or observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckFinding {
    /// Severity.
    pub severity: Severity,
    /// Category.
    pub category: FindingCategory,
    /// What the finding concerns: a role, key, rule (`theme:compact#2`), or path.
    pub subject: String,
    /// Human-readable description.
    pub message: String,
}

impl CheckFinding {
    /// A warning finding.
    #[must_use]
    pub fn warning(
        category: FindingCategory,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: Severity::Warning,
            category,
            subject: subject.into(),
            message: message.into(),
        }
    }

    /// An informational finding.
    #[must_use]
    pub fn info(
        category: FindingCategory,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: Severity::Info,
            category,
            subject: subject.into(),
            message: message.into(),
        }
    }
}

/// Family a contrast pair belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContrastGroup {
    /// Body and secondary text on canvas and surfaces.
    Text,
    /// Accents, links, headings, focus, and speaker labels.
    Accent,
    /// Semantic states and tool lifecycle titles.
    Status,
    /// Diff signs, rows, and intraline emphasis.
    Diff,
    /// Selection highlights.
    Selection,
}

impl ContrastGroup {
    /// Stable lowercase name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Accent => "accent",
            Self::Status => "status",
            Self::Diff => "diff",
            Self::Selection => "selection",
        }
    }
}

/// Result of measuring one contrast pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContrastOutcome {
    /// The ratio meets the threshold.
    Pass,
    /// The ratio is below the threshold.
    Fail,
    /// The pair cannot be measured (a role is unset, or a color is owned by
    /// the terminal and its displayed value is unknown).
    Unmeasured,
}

impl ContrastOutcome {
    /// Stable lowercase name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Unmeasured => "unmeasured",
        }
    }
}

/// One measured foreground/background role pair.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContrastRow {
    /// Pair family.
    pub group: ContrastGroup,
    /// Role whose foreground is measured.
    pub foreground: &'static str,
    /// Role whose background is measured. Equal to `foreground` for roles
    /// that paint their own background (selection, diff rows, state surfaces).
    pub background: &'static str,
    /// Foreground as `#rrggbb`, when measurable.
    pub fg: Option<String>,
    /// Background as `#rrggbb`, when measurable.
    pub bg: Option<String>,
    /// WCAG contrast ratio truncated to two decimals, when measurable.
    pub ratio: Option<f64>,
    /// Threshold the ratio must meet.
    pub minimum: f64,
    /// Result.
    pub outcome: ContrastOutcome,
    /// Why the pair is unmeasured, or which fallback supplied a color.
    pub note: Option<String>,
}

/// Light/dark branch a variant check covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckVariant {
    /// Dark variant.
    Dark,
    /// Light variant.
    Light,
}

impl CheckVariant {
    /// Both variants, in report order.
    pub const ALL: [Self; 2] = [Self::Dark, Self::Light];

    /// Stable lowercase name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    /// The resolver variant this check variant selects.
    #[must_use]
    pub const fn resolved(self) -> ResolvedThemeVariant {
        match self {
            Self::Dark => ResolvedThemeVariant::Dark,
            Self::Light => ResolvedThemeVariant::Light,
        }
    }
}

/// How the registered roles resolve for one variant.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RoleCoverage {
    /// Number of registered roles.
    pub registered: usize,
    /// Registered roles set by an authored value (this theme or a parent).
    pub authored: usize,
    /// Registered roles resolved only through seed derivation.
    pub derived: Vec<String>,
    /// Registered roles with no value; renderers use their host fallback.
    pub unresolved: Vec<String>,
}

impl RoleCoverage {
    /// Registered roles that resolve to a value (authored or derived).
    #[must_use]
    pub const fn resolved(&self) -> usize {
        self.authored + self.derived.len()
    }
}

/// Analysis of one distinct variant resolution.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VariantCheck {
    /// Variants that resolve to this result (both when the theme has no
    /// distinct variant patches).
    pub variants: Vec<CheckVariant>,
    /// Theme ids applied in resolution order.
    pub applied_layers: Vec<String>,
    /// Contrast measurements, in pair-table order.
    pub contrast: Vec<ContrastRow>,
    /// Role registry coverage.
    pub coverage: RoleCoverage,
    /// Findings for this variant.
    pub findings: Vec<CheckFinding>,
}

/// Finding totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct CheckSummary {
    /// Warning findings across the report.
    pub warnings: usize,
    /// Informational findings across the report.
    pub infos: usize,
}

/// Complete `bcode theme check` result. Serialized as the stable `--json`
/// structure; fields are only ever added.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ThemeCheck {
    /// Report format version ([`CHECK_REPORT_VERSION`]).
    pub schema_version: u32,
    /// Checked base theme id.
    pub theme: String,
    /// Overlays applied over the base, in order.
    pub overlays: Vec<String>,
    /// One entry per distinct variant resolution.
    pub variants: Vec<VariantCheck>,
    /// Findings that concern the whole selection (for example discovery).
    pub findings: Vec<CheckFinding>,
    /// Finding totals, kept in sync by [`ThemeCheck::finish`].
    pub summary: CheckSummary,
}

impl ThemeCheck {
    /// Every finding: selection-wide first, then per variant.
    pub fn all_findings(&self) -> impl Iterator<Item = &CheckFinding> {
        self.findings.iter().chain(
            self.variants
                .iter()
                .flat_map(|variant| variant.findings.iter()),
        )
    }

    /// Number of warnings; a strict check fails when this is nonzero.
    #[must_use]
    pub fn warning_count(&self) -> usize {
        self.all_findings()
            .filter(|finding| finding.severity == Severity::Warning)
            .count()
    }

    /// Recompute [`ThemeCheck::summary`] after findings were added.
    pub fn finish(&mut self) {
        let warnings = self.warning_count();
        let infos = self.all_findings().count() - warnings;
        self.summary = CheckSummary { warnings, infos };
    }
}

/// Check `base` with `overlays` from `catalog` in both variants.
///
/// Variants that resolve identically are reported once. `role_namespaces`
/// lists plugin-owned style role namespaces that are not unknown.
/// `presentation` returns structural presentation diagnostics for one
/// resolution (callers compile the theme's rule layers with the policy
/// engine); it is called once per distinct resolution.
///
/// # Errors
///
/// Returns the resolver error when either variant fails to resolve (for
/// example a missing theme, an inheritance cycle, or an invalid color).
pub fn check_theme(
    catalog: &ThemeCatalog,
    base: &str,
    overlays: &[String],
    role_namespaces: &[String],
    mut presentation: impl FnMut(&ResolvedThemeDefinition) -> Vec<CheckFinding>,
) -> Result<ThemeCheck, ThemeError> {
    let mut variants: Vec<(u64, VariantCheck)> = Vec::new();
    for variant in CheckVariant::ALL {
        let resolved = catalog.resolve(
            &ThemeSelection::new(base)
                .overlays(overlays.to_vec())
                .variant(variant.resolved()),
        )?;
        if let Some((_, existing)) = variants
            .iter_mut()
            .find(|(fingerprint, _)| *fingerprint == resolved.fingerprint)
        {
            existing.variants.push(variant);
            continue;
        }
        let mut check = check_resolved(&resolved, role_namespaces);
        check.variants = vec![variant];
        check.findings.extend(presentation(&resolved));
        variants.push((resolved.fingerprint, check));
    }
    let mut report = ThemeCheck {
        schema_version: CHECK_REPORT_VERSION,
        theme: base.to_owned(),
        overlays: overlays.to_vec(),
        variants: variants.into_iter().map(|(_, check)| check).collect(),
        findings: Vec::new(),
        summary: CheckSummary::default(),
    };
    report.finish();
    Ok(report)
}

/// Analyze one resolved theme: contrast, unknown keys, fallbacks, coverage,
/// and resolver warnings. The returned `variants` list is empty; callers
/// that know the variant fill it.
#[must_use]
pub fn check_resolved(
    resolved: &ResolvedThemeDefinition,
    role_namespaces: &[String],
) -> VariantCheck {
    let mut findings = Vec::new();
    let contrast = contrast_rows(resolved);
    for row in contrast
        .iter()
        .filter(|row| row.outcome == ContrastOutcome::Fail)
    {
        findings.push(CheckFinding::warning(
            FindingCategory::Contrast,
            pair_subject(row),
            format!(
                "contrast {:.2} is below {:.1}{}",
                row.ratio.unwrap_or_default(),
                row.minimum,
                if row.foreground == row.background {
                    " (on its own background)"
                } else {
                    ""
                }
            ),
        ));
    }

    let unknown_messages = unknown_key_findings(resolved, role_namespaces, &mut findings);
    findings.extend(
        resolved
            .warnings
            .iter()
            .filter(|warning| !unknown_messages.contains(&warning.message))
            .map(|warning| {
                CheckFinding::warning(
                    FindingCategory::Theme,
                    warning.theme.clone(),
                    warning.message.clone(),
                )
            }),
    );

    let coverage = role_coverage(resolved);
    coverage_findings(resolved, &coverage, &mut findings);

    VariantCheck {
        variants: Vec::new(),
        applied_layers: resolved.applied_layers.clone(),
        contrast,
        coverage,
        findings,
    }
}

/// Subject naming a contrast pair: `fg/bg`, or just the role when it paints
/// its own background.
fn pair_subject(row: &ContrastRow) -> String {
    if row.foreground == row.background {
        row.foreground.to_owned()
    } else {
        format!("{}/{}", row.foreground, row.background)
    }
}

/// Push unknown style-role and container findings; returns the resolver
/// messages they replace so those are not reported twice.
fn unknown_key_findings(
    resolved: &ResolvedThemeDefinition,
    role_namespaces: &[String],
    findings: &mut Vec<CheckFinding>,
) -> BTreeSet<String> {
    let mut replaced = BTreeSet::new();
    for role in resolved
        .styles
        .keys()
        .filter(|role| !roles::is_known_role(role, role_namespaces))
    {
        let hint = roles::suggestion(role)
            .map(|suggestion| format!("; did you mean {suggestion:?}?"))
            .unwrap_or_default();
        let message = format!("unknown style role {role:?}{hint}");
        findings.push(CheckFinding::warning(
            FindingCategory::UnknownKey,
            role.clone(),
            message.clone(),
        ));
        replaced.insert(message);
    }
    for key in resolved
        .containers
        .keys()
        .filter(|key| !REQUIRED_CONTAINER_ROLES.contains(&key.as_str()))
    {
        findings.push(CheckFinding::warning(
            FindingCategory::UnknownKey,
            key.clone(),
            format!(
                "unknown container {key:?}; known containers: {}",
                REQUIRED_CONTAINER_ROLES.join(", ")
            ),
        ));
    }
    replaced
}

fn role_coverage(resolved: &ResolvedThemeDefinition) -> RoleCoverage {
    let derived: BTreeSet<&str> = resolved.derived_roles.iter().map(String::as_str).collect();
    let mut coverage = RoleCoverage {
        registered: roles::ROLES.len(),
        ..RoleCoverage::default()
    };
    for spec in roles::ROLES {
        if derived.contains(spec.key) {
            coverage.derived.push(spec.key.to_owned());
        } else if resolved.styles.contains_key(spec.key) {
            coverage.authored += 1;
        } else {
            coverage.unresolved.push(spec.key.to_owned());
        }
    }
    coverage
}

fn coverage_findings(
    resolved: &ResolvedThemeDefinition,
    coverage: &RoleCoverage,
    findings: &mut Vec<CheckFinding>,
) {
    for role in REQUIRED_SEMANTIC_STYLE_ROLES
        .iter()
        .filter(|role| !resolved.styles.contains_key(**role))
    {
        findings.push(CheckFinding::warning(
            FindingCategory::Coverage,
            *role,
            format!("required style role {role:?} does not resolve"),
        ));
    }
    for role in REQUIRED_CONTAINER_ROLES
        .iter()
        .filter(|role| !resolved.containers.contains_key(**role))
    {
        findings.push(CheckFinding::warning(
            FindingCategory::Coverage,
            *role,
            format!("required container {role:?} does not resolve"),
        ));
    }
    if !coverage.derived.is_empty() {
        findings.push(CheckFinding::info(
            FindingCategory::Fallback,
            "derived",
            format!(
                "{} of {} registered roles resolve only through seed derivation",
                coverage.derived.len(),
                coverage.registered
            ),
        ));
    }
    if !coverage.unresolved.is_empty() {
        findings.push(CheckFinding::info(
            FindingCategory::Fallback,
            "unresolved",
            format!(
                "{} of {} registered roles are unset and use host fallbacks",
                coverage.unresolved.len(),
                coverage.registered
            ),
        ));
    }
}

fn contrast_rows(resolved: &ResolvedThemeDefinition) -> Vec<ContrastRow> {
    let canvas = CanvasColors::of(&resolved.styles);
    PAIRS
        .iter()
        .map(|pair| measure_pair(resolved, pair, canvas))
        .collect()
}

fn measure_pair(
    resolved: &ResolvedThemeDefinition,
    pair: &PairSpec,
    canvas: CanvasColors,
) -> ContrastRow {
    let background_role = match pair.background {
        PairBackground::Surface(surface) => surface,
        PairBackground::OwnWithText(_) => pair.role,
    };
    let mut row = ContrastRow {
        group: pair.group,
        foreground: pair.role,
        background: background_role,
        fg: None,
        bg: None,
        ratio: None,
        minimum: pair.minimum,
        outcome: ContrastOutcome::Unmeasured,
        note: None,
    };
    let Some((_, colors)) = pair_colors(&resolved.styles, pair, canvas) else {
        row.note = Some(format!("{} is not set", pair.role));
        return row;
    };
    let mut notes = colors.notes;
    let measured = (
        colors
            .fg
            .ok_or("terminal default color")
            .and_then(measurable),
        colors
            .bg
            .ok_or("terminal default color")
            .and_then(measurable),
    );
    if let Ok(fg) = measured.0 {
        row.fg = Some(fg.to_hex());
    }
    if let Ok(bg) = measured.1 {
        row.bg = Some(bg.to_hex());
    }
    match measured {
        (Ok(fg), Ok(bg)) => {
            let ratio = fg.contrast_ratio(bg);
            row.ratio = Some((ratio * 100.0).floor() / 100.0);
            row.outcome = if ratio + RATIO_TOLERANCE >= pair.minimum {
                ContrastOutcome::Pass
            } else {
                ContrastOutcome::Fail
            };
        }
        (Err(reason), _) => notes.insert(0, format!("foreground is a {reason}")),
        (_, Err(reason)) => notes.insert(0, format!("background is a {reason}")),
    }
    if !notes.is_empty() {
        row.note = Some(notes.join("; "));
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::parse_theme_definition;

    fn catalog_with(source: &str) -> ThemeCatalog {
        let mut catalog = ThemeCatalog::bundled().expect("bundled");
        catalog.insert(parse_theme_definition("test", source).expect("parse"));
        catalog
    }

    fn row<'a>(variant: &'a VariantCheck, fg: &str, bg: &str) -> &'a ContrastRow {
        variant
            .contrast
            .iter()
            .find(|row| row.foreground == fg && row.background == bg)
            .unwrap_or_else(|| panic!("no row {fg}/{bg}"))
    }

    const BAD: &str = r##"
schema_version = 2
id = "bad"
extends = ["seeded-base"]

[palette]
background = "#202020"
text = "#2a2a2a"
accent = "#303030"
red = "#ff5555"
green = "#55ff55"
yellow = "#ffff55"
blue = "#5555ff"
magenta = "#ff55ff"
cyan = "#55ffff"

[styles."text.primary"]
fg = "#262626"

[styles."bordr.focussed"]
fg = "$accent"

[styles."selection.active"]
fg = "#222222"
bg = "#202020"

[containers."tool.mystery"]
layout = "panel"

[variants.light.styles."text.primary"]
fg = "#f0f0f0"

[variants.light.styles.canvas]
fg = "#000000"
bg = "#ffffff"
"##;

    #[test]
    fn bad_fixture_reports_contrast_unknown_keys_and_both_variants() {
        let catalog = catalog_with(BAD);
        let report = check_theme(&catalog, "bad", &[], &[], |_| Vec::new()).expect("check");
        assert_eq!(report.variants.len(), 2, "dark and light differ");
        let dark = &report.variants[0];
        assert_eq!(dark.variants, [CheckVariant::Dark]);
        let primary = row(dark, "text.primary", "canvas");
        assert_eq!(primary.outcome, ContrastOutcome::Fail);
        assert_eq!(primary.bg.as_deref(), Some("#202020"));
        assert!(primary.ratio.expect("ratio") < 1.2);
        assert_eq!(
            row(dark, "selection.active", "selection.active").outcome,
            ContrastOutcome::Fail
        );
        let has = |category: FindingCategory, subject: &str| {
            dark.findings.iter().any(|finding| {
                finding.category == category
                    && finding.subject == subject
                    && finding.severity == Severity::Warning
            })
        };
        assert!(has(FindingCategory::Contrast, "text.primary/canvas"));
        assert!(has(FindingCategory::UnknownKey, "bordr.focussed"));
        assert!(has(FindingCategory::UnknownKey, "tool.mystery"));
        let unknown_role_reports = dark
            .findings
            .iter()
            .filter(|finding| finding.message.contains("bordr.focussed"))
            .count();
        assert_eq!(unknown_role_reports, 1, "resolver warning deduplicated");
        assert!(
            dark.findings
                .iter()
                .any(|finding| finding.message.contains("did you mean \"border.focused\""))
        );

        // The light patch repaints canvas and text; its own pair is measured
        // against the light background.
        let light = &report.variants[1];
        assert_eq!(light.variants, [CheckVariant::Light]);
        let primary = row(light, "text.primary", "canvas");
        assert_eq!(primary.bg.as_deref(), Some("#ffffff"));
        assert_eq!(primary.outcome, ContrastOutcome::Fail);
        assert!(report.warning_count() > 0);
        assert_eq!(report.summary.warnings, report.warning_count());
    }

    #[test]
    fn identical_variants_are_reported_once() {
        let report = check_theme(
            &ThemeCatalog::bundled().expect("bundled"),
            "monochrome",
            &[],
            &[],
            |_| Vec::new(),
        )
        .expect("check");
        let mut covered: Vec<_> = report
            .variants
            .iter()
            .flat_map(|variant| variant.variants.iter().copied())
            .collect();
        covered.sort_unstable();
        assert_eq!(
            covered,
            CheckVariant::ALL,
            "both variants covered exactly once"
        );
    }

    #[test]
    fn terminal_owned_colors_are_unmeasured_not_failed() {
        let report = check_theme(
            &ThemeCatalog::bundled().expect("bundled"),
            "terminal-native",
            &[],
            &[],
            |_| Vec::new(),
        )
        .expect("check");
        let variant = &report.variants[0];
        let primary = row(variant, "text.primary", "canvas");
        assert_eq!(primary.outcome, ContrastOutcome::Unmeasured);
        assert!(
            primary
                .note
                .as_deref()
                .is_some_and(|note| note.contains("terminal"))
        );
        assert!(
            variant
                .findings
                .iter()
                .all(|finding| finding.category != FindingCategory::Contrast)
        );
    }

    #[test]
    fn seeded_theme_reports_derived_roles_as_fallbacks() {
        let report = check_theme(
            &ThemeCatalog::bundled().expect("bundled"),
            "tokyonight",
            &[],
            &[],
            |_| Vec::new(),
        )
        .expect("check");
        let variant = &report.variants[0];
        assert_ne!(variant.coverage.derived, Vec::<String>::new());
        assert!(variant.coverage.resolved() <= variant.coverage.registered);
        assert!(variant.findings.iter().any(|finding| {
            finding.category == FindingCategory::Fallback && finding.severity == Severity::Info
        }));
    }

    #[test]
    fn reversed_styles_swap_measured_colors() {
        let catalog = catalog_with(
            r##"
schema_version = 1
id = "reversed"
extends = ["bcode-dark"]

[styles."selection.active"]
fg = "#000000"
bg = "#ffffff"
modifiers = ["reversed"]
"##,
        );
        let report = check_theme(&catalog, "reversed", &[], &[], |_| Vec::new()).expect("check");
        let selection = row(&report.variants[0], "selection.active", "selection.active");
        assert_eq!(selection.fg.as_deref(), Some("#ffffff"));
        assert_eq!(selection.bg.as_deref(), Some("#000000"));
        assert_eq!(selection.outcome, ContrastOutcome::Pass);
    }

    #[test]
    fn presentation_hook_findings_are_counted_per_resolution() {
        let mut calls = 0;
        let report = check_theme(
            &ThemeCatalog::bundled().expect("bundled"),
            "monochrome",
            &[],
            &[],
            |_| {
                calls += 1;
                vec![CheckFinding::warning(
                    FindingCategory::Presentation,
                    "theme:monochrome#0",
                    "rule hides group units",
                )]
            },
        )
        .expect("check");
        assert_eq!(calls, report.variants.len());
        assert_eq!(report.summary.warnings, report.warning_count());
        assert!(report.warning_count() >= calls);
    }

    #[test]
    fn missing_theme_is_an_error() {
        let result = check_theme(
            &ThemeCatalog::bundled().expect("bundled"),
            "no-such-theme",
            &[],
            &[],
            |_| Vec::new(),
        );
        assert!(matches!(result, Err(ThemeError::MissingTheme { .. })));
    }

    /// Every bundled definition passes a strict check in both variants;
    /// selectable themes are checked alone and profiles and bases over the
    /// terminal-native theme.
    #[test]
    fn every_bundled_theme_resolves_and_reports() {
        let catalog = ThemeCatalog::bundled().expect("bundled");
        let mut strict_failures = Vec::new();
        for definition in catalog.definitions() {
            let (base, overlays) = match definition.kind() {
                crate::definition::ThemeKind::Theme => (definition.id().to_owned(), Vec::new()),
                _ => (
                    "terminal-native".to_owned(),
                    vec![definition.id().to_owned()],
                ),
            };
            let report = check_theme(&catalog, &base, &overlays, &[], |_| Vec::new())
                .unwrap_or_else(|error| panic!("{}: {error}", definition.id()));
            assert_ne!(report.variants, Vec::new());
            for variant in &report.variants {
                assert_eq!(variant.contrast.len(), PAIRS.len());
                // No bundled theme may use unknown keys or miss required roles.
                for finding in &variant.findings {
                    assert!(
                        !matches!(
                            finding.category,
                            FindingCategory::UnknownKey | FindingCategory::Coverage
                        ),
                        "{}: {finding:?}",
                        definition.id()
                    );
                }
            }
            let failures: Vec<_> = report
                .all_findings()
                .filter(|finding| finding.severity == Severity::Warning)
                .map(|finding| format!("{}: {}", finding.subject, finding.message))
                .collect();
            if !failures.is_empty() {
                strict_failures.push((definition.id().to_owned(), failures));
            }
        }
        assert_eq!(
            strict_failures,
            Vec::<(String, Vec<String>)>::new(),
            "every bundled theme passes --strict in both variants"
        );
    }
}
