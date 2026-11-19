//! Accepted `bcode theme check` findings.
//!
//! A theme can carry findings its author has reviewed and accepted, such as an
//! upstream color scheme's own low contrast, instead of changing its colors.
//! Accepted findings live in a TOML file ([`ACCEPTED_FILE_NAME`]) keyed by
//! theme selection, variant, finding kind, and the finding's role pair or
//! subject. Each contrast entry records the measured ratio, so a finding that
//! gets worse is reported even though it is accepted.
//!
//! [`ThemeCheck::apply_acceptance`] compares a check with the accepted
//! findings: every current warning is accepted, improved (a contrast ratio
//! higher than recorded), new, or worse (lower than recorded); accepted entries
//! that no longer occur are stale. [`AcceptedFindings::bless`] rewrites a
//! selection's entries from a check, keeping their reasons.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{CheckVariant, ContrastOutcome, FindingCategory, Severity, ThemeCheck};

/// File name of an accepted-findings file, next to the themes it covers.
pub const ACCEPTED_FILE_NAME: &str = "check-accepted.toml";
/// Version of the accepted-findings file format.
pub const ACCEPTED_SCHEMA_VERSION: u32 = 1;
/// How far a contrast ratio may move from its recorded value before it counts
/// as worse or improved. Ratios are recorded truncated to two decimals, so any
/// change of a hundredth or more is reported.
pub const RATIO_EPSILON: f64 = 0.005;
/// Largest accepted-findings file read.
pub const MAX_ACCEPTED_FILE_BYTES: usize = 1024 * 1024;

/// Accepted findings for the bundled themes.
const BUNDLED_ACCEPTED: &str = include_str!("../../themes/check-accepted.toml");

/// Comment written at the top of every accepted-findings file.
const HEADER: &str = "\
# Accepted `bcode theme check` findings, keyed by theme, variant, kind, and
# role pair or subject. Add a `reason` to explain an entry; regenerate the
# entries with `bcode theme check --bless`, which keeps reasons. See the
# \"Accepting findings\" section of docs/tui-themes.md.
";

/// Note on a finding that cannot be accepted under the policy in force.
const DERIVED_NOTE: &str =
    "derived colors must meet their threshold; only palette colors can be accepted";

/// Errors reading an accepted-findings file.
#[derive(Debug, thiserror::Error)]
pub enum AcceptedFindingsError {
    /// The file is not valid TOML or does not match the format.
    #[error("invalid accepted findings: {0}")]
    Parse(String),
    /// The file declares a format version this build does not read.
    #[error("unsupported accepted findings schema_version {0}; expected {ACCEPTED_SCHEMA_VERSION}")]
    Version(u32),
    /// An entry lacks the fields its kind is keyed by.
    #[error("accepted finding for {theme} ({kind}) {problem}")]
    Entry {
        /// Theme of the entry.
        theme: String,
        /// Finding kind of the entry.
        kind: &'static str,
        /// What is wrong.
        problem: &'static str,
    },
    /// Two entries have the same key.
    #[error("duplicate accepted finding for {theme} ({kind}): {subject}")]
    Duplicate {
        /// Theme of the entries.
        theme: String,
        /// Finding kind of the entries.
        kind: &'static str,
        /// Pair or subject of the entries.
        subject: String,
    },
}

/// One accepted finding, or the record of a current finding in the same shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedFinding {
    /// Checked base theme id.
    pub theme: String,
    /// Overlays applied over the base, in order (profiles checked over the
    /// default theme).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overlays: Vec<String>,
    /// Variant the finding occurs in; absent for findings about the whole
    /// selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<CheckVariant>,
    /// Finding kind.
    pub kind: FindingCategory,
    /// Contrast findings: role whose foreground is measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<String>,
    /// Contrast findings: role whose background is measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    /// Other findings: the role, key, rule, or path the finding concerns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Other findings: the finding's message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Contrast findings: the measured ratio, truncated to two decimals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f64>,
    /// Contrast findings: the palette entry the text color is kept from, if
    /// any. Informational; not part of the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    /// Why the finding is accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Identity of a finding: everything but its measurement, slot, and reason.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FindingKey {
    theme: String,
    overlays: Vec<String>,
    variant: Option<CheckVariant>,
    kind: FindingCategory,
    foreground: Option<String>,
    background: Option<String>,
    subject: Option<String>,
    message: Option<String>,
}

impl AcceptedFinding {
    fn key(&self) -> FindingKey {
        FindingKey {
            theme: self.theme.clone(),
            overlays: self.overlays.clone(),
            variant: self.variant,
            kind: self.kind,
            foreground: self.foreground.clone(),
            background: self.background.clone(),
            subject: self.subject.clone(),
            message: self.message.clone(),
        }
    }

    /// Whether this entry belongs to the checked selection of `report`.
    fn covers(&self, report: &ThemeCheck) -> bool {
        self.theme == report.theme && self.overlays == report.overlays
    }

    /// The pair (`fg/bg`) or subject the entry is keyed by.
    fn label(&self) -> String {
        match (&self.foreground, &self.background, &self.subject) {
            (Some(foreground), Some(background), _) if foreground == background => {
                foreground.clone()
            }
            (Some(foreground), Some(background), _) => format!("{foreground}/{background}"),
            (_, _, Some(subject)) => subject.clone(),
            _ => String::new(),
        }
    }

    /// A one-line description: variant, kind, pair or subject, and ratio or
    /// message.
    #[must_use]
    pub fn describe(&self) -> String {
        let variant = self.variant.map_or("all", CheckVariant::name);
        let detail = self.ratio.map_or_else(
            || self.message.clone().unwrap_or_default(),
            |ratio| format!("{ratio:.2}"),
        );
        format!(
            "{variant:<5}  {:<12}  {}  {detail}",
            self.kind.name(),
            self.label()
        )
    }
}

/// What may be accepted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AcceptancePolicy {
    /// Contrast findings are acceptable only when their text color is a
    /// palette color kept exactly; colors Bcode derives must meet their
    /// threshold. Bundled themes use this policy.
    pub palette_contrast_only: bool,
}

impl AcceptancePolicy {
    /// The policy for bundled themes.
    pub const BUNDLED: Self = Self {
        palette_contrast_only: true,
    };
}

/// A set of accepted findings, as stored in an accepted-findings file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedFindings {
    /// File format version ([`ACCEPTED_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// The accepted findings.
    #[serde(default, rename = "accepted")]
    pub entries: Vec<AcceptedFinding>,
}

impl Default for AcceptedFindings {
    fn default() -> Self {
        Self {
            schema_version: ACCEPTED_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

/// Changes [`AcceptedFindings::bless`] made.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct BlessSummary {
    /// Findings accepted that had no entry.
    pub added: usize,
    /// Entries whose recorded ratio changed.
    pub updated: usize,
    /// Entries kept unchanged.
    pub kept: usize,
    /// Stale entries removed.
    pub removed: usize,
    /// Findings left unaccepted because the policy does not allow them.
    pub refused: usize,
}

impl AcceptedFindings {
    /// Parse an accepted-findings file.
    ///
    /// # Errors
    ///
    /// * [`AcceptedFindingsError::Parse`] when the source is not valid TOML or
    ///   has unknown fields;
    /// * [`AcceptedFindingsError::Version`] for another `schema_version`;
    /// * [`AcceptedFindingsError::Entry`] when a contrast entry lacks its
    ///   roles or ratio, or another entry lacks its subject or message;
    /// * [`AcceptedFindingsError::Duplicate`] when two entries share a key.
    pub fn parse(source: &str) -> Result<Self, AcceptedFindingsError> {
        let findings: Self = toml::from_str(source)
            .map_err(|error| AcceptedFindingsError::Parse(error.to_string()))?;
        if findings.schema_version != ACCEPTED_SCHEMA_VERSION {
            return Err(AcceptedFindingsError::Version(findings.schema_version));
        }
        let mut keys = std::collections::BTreeSet::new();
        for entry in &findings.entries {
            validate_entry(entry)?;
            if !keys.insert(entry.key()) {
                return Err(AcceptedFindingsError::Duplicate {
                    theme: entry.theme.clone(),
                    kind: entry.kind.name(),
                    subject: entry.label(),
                });
            }
        }
        Ok(findings)
    }

    /// The accepted findings shipped for the bundled themes.
    ///
    /// # Errors
    ///
    /// Returns the parse error when the bundled file is invalid (a build
    /// defect that the crate's tests catch).
    pub fn bundled() -> Result<Self, AcceptedFindingsError> {
        Self::parse(BUNDLED_ACCEPTED)
    }

    /// The file contents: a header comment and the entries in key order.
    #[must_use]
    pub fn to_toml(&self) -> String {
        let mut sorted = self.clone();
        sorted.sort();
        let body = toml::to_string(&sorted).unwrap_or_default();
        format!("{HEADER}\n{body}")
    }

    fn sort(&mut self) {
        self.entries.sort_by_key(AcceptedFinding::key);
    }

    /// Replace the entries for the selection `report` checked with its
    /// current warnings, keeping the reasons of entries that still occur and
    /// removing stale ones. Entries for other selections are untouched;
    /// findings `policy` does not allow are not added.
    pub fn bless(&mut self, report: &ThemeCheck, policy: AcceptancePolicy) -> BlessSummary {
        let mut previous: BTreeMap<FindingKey, AcceptedFinding> = BTreeMap::new();
        self.entries.retain(|entry| {
            if entry.covers(report) {
                previous.insert(entry.key(), entry.clone());
                false
            } else {
                true
            }
        });
        let mut summary = BlessSummary::default();
        for current in current_findings(report) {
            if !policy.allows(&current) {
                summary.refused += 1;
                continue;
            }
            let mut entry = current.record;
            match previous.remove(&entry.key()) {
                Some(prior) => {
                    if ratio_change(entry.ratio, prior.ratio).is_some() {
                        summary.updated += 1;
                    } else {
                        summary.kept += 1;
                        // Keep the recorded value when the change is within
                        // the tolerance, so blessing is idempotent.
                        entry.ratio = prior.ratio;
                    }
                    entry.reason = prior.reason;
                }
                None => summary.added += 1,
            }
            self.entries.push(entry);
        }
        summary.removed = previous.len();
        self.sort();
        summary
    }
}

impl AcceptancePolicy {
    const fn allows(self, current: &CurrentFinding) -> bool {
        !(self.palette_contrast_only && current.derived)
    }
}

fn validate_entry(entry: &AcceptedFinding) -> Result<(), AcceptedFindingsError> {
    let problem = if entry.kind == FindingCategory::Contrast {
        if entry.foreground.is_none() || entry.background.is_none() {
            Some("needs foreground and background")
        } else if entry.ratio.is_none() {
            Some("needs a ratio")
        } else {
            None
        }
    } else if entry.subject.is_none() || entry.message.is_none() {
        Some("needs a subject and message")
    } else {
        None
    };
    problem.map_or(Ok(()), |problem| {
        Err(AcceptedFindingsError::Entry {
            theme: entry.theme.clone(),
            kind: entry.kind.name(),
            problem,
        })
    })
}

/// A current warning, recorded in the accepted-findings shape.
struct CurrentFinding {
    record: AcceptedFinding,
    /// A contrast finding whose text color Bcode derives rather than keeping
    /// a palette color.
    derived: bool,
}

/// Every warning of `report`, one record per variant it occurs in: failing
/// contrast pairs, then the other warning findings.
fn current_findings(report: &ThemeCheck) -> Vec<CurrentFinding> {
    let blank = |variant: Option<CheckVariant>, kind: FindingCategory| AcceptedFinding {
        theme: report.theme.clone(),
        overlays: report.overlays.clone(),
        variant,
        kind,
        foreground: None,
        background: None,
        subject: None,
        message: None,
        ratio: None,
        slot: None,
        reason: None,
    };
    let mut current = Vec::new();
    for finding in report
        .findings
        .iter()
        .filter(|finding| finding.severity == Severity::Warning)
    {
        current.push(CurrentFinding {
            record: AcceptedFinding {
                subject: Some(finding.subject.clone()),
                message: Some(finding.message.clone()),
                ..blank(None, finding.category)
            },
            derived: false,
        });
    }
    for check in &report.variants {
        for &variant in &check.variants {
            for row in check
                .contrast
                .iter()
                .filter(|row| row.outcome == ContrastOutcome::Fail)
            {
                current.push(CurrentFinding {
                    record: AcceptedFinding {
                        foreground: Some(row.foreground.to_owned()),
                        background: Some(row.background.to_owned()),
                        ratio: row.ratio,
                        slot: row.palette_slot.clone(),
                        ..blank(Some(variant), FindingCategory::Contrast)
                    },
                    derived: row.palette_slot.is_none(),
                });
            }
            for finding in check.findings.iter().filter(|finding| {
                finding.severity == Severity::Warning
                    && finding.category != FindingCategory::Contrast
            }) {
                current.push(CurrentFinding {
                    record: AcceptedFinding {
                        subject: Some(finding.subject.clone()),
                        message: Some(finding.message.clone()),
                        ..blank(Some(variant), finding.category)
                    },
                    derived: false,
                });
            }
        }
    }
    current
}

/// How a current ratio compares with the recorded one, beyond the tolerance.
fn ratio_change(current: Option<f64>, recorded: Option<f64>) -> Option<AcceptanceStatus> {
    let (current, recorded) = (current?, recorded?);
    if current < recorded - RATIO_EPSILON {
        Some(AcceptanceStatus::Worse)
    } else if current > recorded + RATIO_EPSILON {
        Some(AcceptanceStatus::Improved)
    } else {
        None
    }
}

/// How one current finding compares with the accepted findings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceStatus {
    /// Accepted as recorded.
    Accepted,
    /// Accepted, with a higher contrast ratio than recorded.
    Improved,
    /// Accepted, with a lower contrast ratio than recorded; fails the check.
    Worse,
    /// Not accepted; fails the check.
    New,
}

impl AcceptanceStatus {
    /// Stable lowercase name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Improved => "improved",
            Self::Worse => "worse",
            Self::New => "new",
        }
    }

    /// Whether the status fails the check.
    #[must_use]
    pub const fn fails(self) -> bool {
        matches!(self, Self::Worse | Self::New)
    }
}

/// One current finding with its comparison.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AcceptanceItem {
    /// The finding as measured now, with the accepted entry's reason.
    #[serde(flatten)]
    pub finding: AcceptedFinding,
    /// The ratio the accepted entry records, for contrast findings that have
    /// an entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_ratio: Option<f64>,
    /// Why the finding cannot be accepted, when the policy forbids it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Counts of an [`Acceptance`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct AcceptanceSummary {
    /// Findings accepted as recorded.
    pub accepted: usize,
    /// Accepted findings whose contrast improved.
    pub improved: usize,
    /// Findings without an acceptable entry.
    pub new: usize,
    /// Accepted findings whose contrast got worse.
    pub worse: usize,
    /// Accepted entries that no longer occur.
    pub stale: usize,
}

/// Comparison of a check with the accepted findings. The lists are disjoint.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Acceptance {
    /// Where the accepted findings came from (a path, or `bundled`).
    pub source: String,
    /// Findings accepted as recorded.
    pub accepted: Vec<AcceptanceItem>,
    /// Accepted findings whose contrast ratio rose above the recorded one.
    pub improved: Vec<AcceptanceItem>,
    /// Findings without an acceptable entry.
    pub new: Vec<AcceptanceItem>,
    /// Accepted findings whose contrast ratio fell below the recorded one.
    pub worse: Vec<AcceptanceItem>,
    /// Accepted entries for this selection that no longer occur.
    pub stale: Vec<AcceptedFinding>,
    /// Counts of each list.
    pub summary: AcceptanceSummary,
}

impl Acceptance {
    /// Whether the check passes: no new or worse findings, and under `strict`
    /// no stale entries either.
    #[must_use]
    pub const fn passes(&self, strict: bool) -> bool {
        self.summary.new == 0 && self.summary.worse == 0 && (!strict || self.summary.stale == 0)
    }
}

impl ThemeCheck {
    /// Compare the check with `accepted` (read from `source`) under `policy`,
    /// recording the result in [`ThemeCheck::acceptance`] and marking each
    /// warning finding with its status.
    pub fn apply_acceptance(
        &mut self,
        accepted: &AcceptedFindings,
        policy: AcceptancePolicy,
        source: impl Into<String>,
    ) {
        let mut entries: BTreeMap<FindingKey, &AcceptedFinding> = accepted
            .entries
            .iter()
            .filter(|entry| entry.covers(self))
            .map(|entry| (entry.key(), entry))
            .collect();
        let mut acceptance = Acceptance {
            source: source.into(),
            accepted: Vec::new(),
            improved: Vec::new(),
            new: Vec::new(),
            worse: Vec::new(),
            stale: Vec::new(),
            summary: AcceptanceSummary::default(),
        };
        let mut statuses: BTreeMap<FindingKey, AcceptanceStatus> = BTreeMap::new();
        for current in current_findings(self) {
            let key = current.record.key();
            let allowed = policy.allows(&current);
            let prior = entries.remove(&key);
            let mut item = AcceptanceItem {
                accepted_ratio: prior.and_then(|prior| prior.ratio),
                note: None,
                finding: AcceptedFinding {
                    reason: prior.and_then(|prior| prior.reason.clone()),
                    ..current.record
                },
            };
            let status = match prior {
                _ if !allowed => {
                    item.note = Some(DERIVED_NOTE.to_owned());
                    AcceptanceStatus::New
                }
                None => AcceptanceStatus::New,
                Some(prior) => ratio_change(item.finding.ratio, prior.ratio)
                    .unwrap_or(AcceptanceStatus::Accepted),
            };
            statuses.insert(key, status);
            match status {
                AcceptanceStatus::Accepted => acceptance.accepted.push(item),
                AcceptanceStatus::Improved => acceptance.improved.push(item),
                AcceptanceStatus::Worse => acceptance.worse.push(item),
                AcceptanceStatus::New => acceptance.new.push(item),
            }
        }
        acceptance.stale = entries.into_values().cloned().collect();
        acceptance.summary = AcceptanceSummary {
            accepted: acceptance.accepted.len(),
            improved: acceptance.improved.len(),
            new: acceptance.new.len(),
            worse: acceptance.worse.len(),
            stale: acceptance.stale.len(),
        };
        self.mark_findings(&statuses);
        self.acceptance = Some(acceptance);
    }

    /// Set each warning finding's status: the worst over the variants it
    /// occurs in.
    fn mark_findings(&mut self, statuses: &BTreeMap<FindingKey, AcceptanceStatus>) {
        let key = |variant: Option<CheckVariant>, finding: &super::CheckFinding| -> FindingKey {
            let (foreground, background, subject, message) =
                if finding.category == FindingCategory::Contrast {
                    let (foreground, background) = finding
                        .subject
                        .split_once('/')
                        .unwrap_or((&finding.subject, &finding.subject));
                    (
                        Some(foreground.to_owned()),
                        Some(background.to_owned()),
                        None,
                        None,
                    )
                } else {
                    (
                        None,
                        None,
                        Some(finding.subject.clone()),
                        Some(finding.message.clone()),
                    )
                };
            FindingKey {
                theme: self.theme.clone(),
                overlays: self.overlays.clone(),
                variant,
                kind: finding.category,
                foreground,
                background,
                subject,
                message,
            }
        };
        let selection_wide: Vec<_> = self
            .findings
            .iter()
            .map(|finding| statuses.get(&key(None, finding)).copied())
            .collect();
        let per_variant: Vec<Vec<_>> = self
            .variants
            .iter()
            .map(|check| {
                check
                    .findings
                    .iter()
                    .map(|finding| {
                        check
                            .variants
                            .iter()
                            .filter_map(|variant| statuses.get(&key(Some(*variant), finding)))
                            .max()
                            .copied()
                    })
                    .collect()
            })
            .collect();
        for (finding, status) in self.findings.iter_mut().zip(selection_wide) {
            finding.acceptance = status;
        }
        for (check, statuses) in self.variants.iter_mut().zip(per_variant) {
            for (finding, status) in check.findings.iter_mut().zip(statuses) {
                finding.acceptance = status;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::check_theme;
    use crate::definition::{ThemeCatalog, parse_theme_definition};

    /// A dark theme whose palette accent and muted text are kept below their
    /// thresholds, with an unknown style role.
    const LOW: &str = r##"
schema_version = 2
id = "low"
extends = ["seeded-base"]

[palette]
background = "#1e1e1e"
text = "#dddddd"
accent = "#30306a"
muted = "#3c3c3c"

[styles."bordr.focussed"]
fg = "$accent"
"##;

    fn report() -> ThemeCheck {
        let mut catalog = ThemeCatalog::bundled().expect("bundled");
        catalog.insert(parse_theme_definition("low", LOW).expect("parse"));
        check_theme(&catalog, "low", &[], &[], |_| Vec::new()).expect("check")
    }

    fn blessed(report: &ThemeCheck) -> AcceptedFindings {
        let mut accepted = AcceptedFindings::default();
        accepted.bless(report, AcceptancePolicy::default());
        accepted
    }

    fn acceptance(report: &ThemeCheck, accepted: &AcceptedFindings) -> Acceptance {
        let mut report = report.clone();
        report.apply_acceptance(accepted, AcceptancePolicy::default(), "test");
        report.acceptance.expect("acceptance")
    }

    fn contrast_entry(accepted: &mut AcceptedFindings) -> &mut AcceptedFinding {
        accepted
            .entries
            .iter_mut()
            .find(|entry| entry.kind == FindingCategory::Contrast)
            .expect("a contrast entry")
    }

    #[test]
    fn accepted_findings_pass() {
        let report = report();
        assert!(report.warning_count() > 0, "fixture must have warnings");
        let accepted = blessed(&report);
        let result = acceptance(&report, &accepted);
        assert_eq!(result.summary.new, 0, "{result:?}");
        assert_eq!(result.summary.worse, 0);
        assert_eq!(result.summary.stale, 0);
        assert_eq!(result.summary.accepted, accepted.entries.len());
        assert!(result.passes(true));
        // Contrast and unknown-key findings are both accepted.
        let kinds: Vec<_> = result
            .accepted
            .iter()
            .map(|item| item.finding.kind)
            .collect();
        assert!(kinds.contains(&FindingCategory::Contrast), "{kinds:?}");
        assert!(kinds.contains(&FindingCategory::UnknownKey), "{kinds:?}");
    }

    #[test]
    fn new_findings_fail() {
        let report = report();
        let result = acceptance(&report, &AcceptedFindings::default());
        assert!(result.summary.new > 0);
        assert_eq!(result.summary.accepted, 0);
        assert!(!result.passes(false));
        let mut marked = report;
        marked.apply_acceptance(
            &AcceptedFindings::default(),
            AcceptancePolicy::default(),
            "test",
        );
        assert!(
            marked
                .all_findings()
                .filter(|finding| finding.severity == Severity::Warning)
                .all(|finding| finding.acceptance == Some(AcceptanceStatus::New))
        );
    }

    #[test]
    fn worse_contrast_fails_and_better_contrast_passes() {
        let report = report();
        let mut worse = blessed(&report);
        let entry = contrast_entry(&mut worse);
        let measured = entry.ratio.expect("ratio");
        entry.ratio = Some(measured + 0.5);
        let result = acceptance(&report, &worse);
        assert_eq!(result.summary.worse, 1);
        assert_eq!(result.worse[0].accepted_ratio, Some(measured + 0.5));
        assert_eq!(result.worse[0].finding.ratio, Some(measured));
        assert!(!result.passes(false));

        let mut better = blessed(&report);
        contrast_entry(&mut better).ratio = Some(measured - 0.5);
        let result = acceptance(&report, &better);
        assert_eq!(result.summary.improved, 1);
        assert_eq!(result.summary.worse, 0);
        assert!(result.passes(true));

        // Within the tolerance a ratio is unchanged.
        let mut same = blessed(&report);
        contrast_entry(&mut same).ratio = Some(measured + RATIO_EPSILON / 2.0);
        let result = acceptance(&report, &same);
        assert_eq!((result.summary.improved, result.summary.worse), (0, 0));
    }

    #[test]
    fn stale_entries_are_reported_and_fail_only_strict() {
        let report = report();
        let mut accepted = blessed(&report);
        let mut gone = accepted.entries[0].clone();
        gone.kind = FindingCategory::Presentation;
        gone.foreground = None;
        gone.background = None;
        gone.ratio = None;
        gone.subject = Some("theme:low#9".to_owned());
        gone.message = Some("no longer reported".to_owned());
        accepted.entries.push(gone.clone());
        // Entries for other selections are not this check's to judge.
        let mut other = gone;
        other.theme = "other".to_owned();
        accepted.entries.push(other);
        let result = acceptance(&report, &accepted);
        assert_eq!(result.summary.stale, 1);
        assert_eq!(result.stale[0].subject.as_deref(), Some("theme:low#9"));
        assert!(result.passes(false));
        assert!(!result.passes(true));
    }

    #[test]
    fn bless_is_deterministic_and_keeps_reasons() {
        let report = report();
        let mut accepted = blessed(&report);
        let first = accepted.to_toml();
        contrast_entry(&mut accepted).reason = Some("upstream color".to_owned());
        let stale = AcceptedFinding {
            subject: Some("gone".to_owned()),
            message: Some("gone".to_owned()),
            ..accepted
                .entries
                .iter()
                .find(|entry| entry.kind == FindingCategory::UnknownKey)
                .expect("unknown key entry")
                .clone()
        };
        accepted.entries.push(stale);
        let other = AcceptedFinding {
            theme: "another".to_owned(),
            reason: Some("kept".to_owned()),
            ..accepted.entries[0].clone()
        };
        accepted.entries.push(other);
        accepted.entries.reverse();

        let summary = accepted.bless(&report, AcceptancePolicy::default());
        assert_eq!(summary.removed, 1);
        assert_eq!(summary.added, 0);
        assert_eq!(summary.kept, accepted.entries.len() - 1);
        let toml = accepted.to_toml();
        assert!(toml.contains("reason = \"upstream color\""), "{toml}");
        assert!(toml.contains("theme = \"another\""), "{toml}");
        assert!(!toml.contains("\"gone\""), "{toml}");
        assert!(toml.starts_with("# Accepted `bcode theme check` findings"));

        // Blessing again changes nothing, and the file reads back as written.
        let mut again = AcceptedFindings::parse(&toml).expect("parse");
        assert_eq!(again, {
            let mut sorted = accepted.clone();
            sorted.sort();
            sorted
        });
        again.bless(&report, AcceptancePolicy::default());
        assert_eq!(again.to_toml(), toml);
        // Without the added entries the output equals the first bless.
        let mut plain = AcceptedFindings::parse(&toml).expect("parse");
        plain.entries.retain(|entry| entry.theme == "low");
        for entry in &mut plain.entries {
            entry.reason = None;
        }
        assert_eq!(plain.to_toml(), first);
    }

    #[test]
    fn palette_only_policy_refuses_derived_contrast() {
        let mut report = report();
        // Present a failing pair as a derived color.
        let row = report.variants[0]
            .contrast
            .iter_mut()
            .find(|row| row.outcome == ContrastOutcome::Fail)
            .expect("failing pair");
        row.palette_slot = None;
        let mut accepted = AcceptedFindings::default();
        let summary = accepted.bless(&report, AcceptancePolicy::BUNDLED);
        assert_eq!(summary.refused, report.variants[0].variants.len());
        let permissive = blessed(&report);
        let mut checked = report.clone();
        checked.apply_acceptance(&permissive, AcceptancePolicy::BUNDLED, "bundled");
        let result = checked.acceptance.expect("acceptance");
        assert!(result.summary.new >= 1);
        assert!(result.new.iter().all(|item| item.note.is_some()));
        assert!(!result.passes(false));
    }

    #[test]
    fn json_reports_each_list_and_finding_status() {
        let report = report();
        let mut accepted = blessed(&report);
        contrast_entry(&mut accepted).ratio = Some(1.0);
        let mut checked = report;
        checked.apply_acceptance(
            &accepted,
            AcceptancePolicy::default(),
            "themes/check-accepted.toml",
        );
        let value = serde_json::to_value(&checked).expect("json");
        let acceptance = &value["acceptance"];
        assert_eq!(acceptance["source"], "themes/check-accepted.toml");
        for key in ["accepted", "improved", "new", "worse", "stale"] {
            assert!(acceptance[key].is_array(), "{key}: {acceptance}");
            assert!(acceptance["summary"][key].is_u64(), "{key}");
        }
        let improved = &acceptance["improved"][0];
        assert_eq!(improved["kind"], "contrast");
        assert_eq!(improved["accepted_ratio"], 1.0);
        for key in ["theme", "variant", "foreground", "background", "ratio"] {
            assert!(improved.get(key).is_some(), "{key}: {improved}");
        }
        let statuses: Vec<_> = value["variants"]
            .as_array()
            .expect("variants")
            .iter()
            .flat_map(|variant| variant["findings"].as_array().expect("findings"))
            .filter_map(|finding| finding.get("acceptance"))
            .collect();
        assert!(statuses.contains(&&serde_json::json!("accepted")));
        assert!(statuses.contains(&&serde_json::json!("improved")));
    }

    #[test]
    fn invalid_files_are_rejected() {
        let entry = r#"
[[accepted]]
theme = "low"
variant = "dark"
kind = "contrast"
foreground = "accent"
background = "canvas"
ratio = 1.5
"#;
        let one = format!("schema_version = 1\n{entry}");
        assert_eq!(
            AcceptedFindings::parse(&one).expect("valid").entries.len(),
            1
        );
        assert!(matches!(
            AcceptedFindings::parse(&format!("{one}{entry}")),
            Err(AcceptedFindingsError::Duplicate { .. })
        ));
        assert!(matches!(
            AcceptedFindings::parse(&one.replace("ratio = 1.5", "")),
            Err(AcceptedFindingsError::Entry { .. })
        ));
        assert!(matches!(
            AcceptedFindings::parse(&one.replace("schema_version = 1", "schema_version = 7")),
            Err(AcceptedFindingsError::Version(7))
        ));
        assert!(matches!(
            AcceptedFindings::parse(&format!("{one}colour = \"x\"\n")),
            Err(AcceptedFindingsError::Parse(_))
        ));
    }
}
