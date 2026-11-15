//! `[plugins.config."bcode.code_review".compare]`: how a review of pull requests compares each
//! one with an earlier version. See `docs/review-stack-history.md`.
//!
//! ```toml
//! [plugins.config."bcode.code_review".compare]
//! default_baseline = "last_looked"  # or "previous_push", a duration such as "24h", or "none"
//! baseline_look = "engaged"          # or "opened": glances count as looking
//! dwell_seconds = 8                  # seconds on a layer that count as opening it
//! start_view = "full"                # or "changes": only what changed
//! rebased_layers = "collapse"        # or "hide", or "show"
//! timeline = "auto"                  # or "always"
//! mark_style = "both"                # or "bar", "tint", "strong": how lines new since show
//! word_marks = true                  # mark the changed words of lines modified since
//! seen_hunks = "show"                # or "dim", "fold": hunks with nothing new since
//! removed_inline = false             # show code removed since inline, struck through
//! ```

use bcode_code_review_models::ReviewCompareBaseline;
use serde::Deserialize;

/// What a review compares with when nothing else was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DefaultBaseline {
    /// What the reviewer last looked at.
    #[default]
    LastLooked,
    /// Each layer's version before the latest push.
    PreviousPush,
    /// Each layer's version this long ago, in milliseconds.
    Ago(u64),
    /// No comparison.
    None,
}

impl DefaultBaseline {
    /// Read `"last_looked"`, `"previous_push"`, `"none"`, or a duration (`"30m"`, `"24h"`,
    /// `"7d"`).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "last_looked" => Some(Self::LastLooked),
            "previous_push" => Some(Self::PreviousPush),
            "none" => Some(Self::None),
            duration => parse_duration_ms(duration).map(Self::Ago),
        }
    }

    /// The baseline this default picks at `now_ms`.
    #[must_use]
    pub const fn baseline(self, now_ms: u64) -> ReviewCompareBaseline {
        match self {
            Self::LastLooked => ReviewCompareBaseline::LastLooked,
            Self::PreviousPush => ReviewCompareBaseline::PreviousPush,
            Self::Ago(ms) => ReviewCompareBaseline::Between {
                from_ms: now_ms.saturating_sub(ms),
                to_ms: None,
            },
            Self::None => ReviewCompareBaseline::Off,
        }
    }
}

/// A duration such as `"90s"`, `"30m"`, `"24h"`, or `"7d"`, in milliseconds.
#[must_use]
pub fn parse_duration_ms(text: &str) -> Option<u64> {
    let text = text.trim();
    let unit = text.chars().last()?;
    let scale: u64 = match unit {
        's' => 1_000,
        'm' => 60_000,
        'h' => 3_600_000,
        'd' => 86_400_000,
        'w' => 7 * 86_400_000,
        _ => return None,
    };
    let number: u64 = text[..text.len() - unit.len_utf8()].trim().parse().ok()?;
    (number > 0).then(|| number.saturating_mul(scale))
}

/// Which looks count for "since you last looked".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaselineLook {
    /// Viewing a file, commenting, asking, a completed pass, or a review on the forge; opening
    /// a layer counts only when there is nothing else.
    #[default]
    Engaged,
    /// Opening a layer for the dwell time counts too.
    Opened,
}

/// The view a comparison starts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartView {
    /// The full change, with what is new marked.
    #[default]
    Full,
    /// Only what changed.
    Changes,
}

/// How layers that were only rebased show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebasedLayers {
    /// Collapsed to one line.
    #[default]
    Collapse,
    /// Left out (as with "only pull requests with real changes").
    Hide,
    /// Shown with their files folded.
    Show,
}

/// When the push timeline shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineMode {
    /// When the stack has two or more pushes and a comparison is active.
    #[default]
    Auto,
    /// Whenever the stack has pushes.
    Always,
}

/// How lines new since the baseline are marked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkStyle {
    /// A bar in the gutter only.
    Bar,
    /// A tint behind the row only.
    Tint,
    /// The bar and the tint.
    #[default]
    Both,
    /// An accent bar, a stronger tint, and a `new` label in the gutter where it has room.
    Strong,
}

impl MarkStyle {
    /// Every style, in the order the comparison menu lists them.
    pub const ALL: [Self; 4] = [Self::Both, Self::Bar, Self::Tint, Self::Strong];

    /// Whether marked rows paint the gutter bar.
    #[must_use]
    pub const fn bar(self) -> bool {
        matches!(self, Self::Bar | Self::Both | Self::Strong)
    }

    /// Whether marked rows are tinted.
    #[must_use]
    pub const fn tint(self) -> bool {
        matches!(self, Self::Tint | Self::Both | Self::Strong)
    }

    /// A small number distinguishing the styles, for cache keys.
    #[must_use]
    pub const fn index(self) -> u64 {
        match self {
            Self::Both => 0,
            Self::Bar => 1,
            Self::Tint => 2,
            Self::Strong => 3,
        }
    }
}

/// How hunks with nothing new since the baseline show while their file is marked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeenHunks {
    /// Like every other hunk.
    #[default]
    Show,
    /// Dimmed.
    Dim,
    /// Folded to their header.
    Fold,
}

impl SeenHunks {
    /// Every choice, in the order the comparison menu lists them.
    pub const ALL: [Self; 3] = [Self::Show, Self::Dim, Self::Fold];

    /// A small number distinguishing the choices, for cache keys.
    #[must_use]
    pub const fn index(self) -> u64 {
        match self {
            Self::Show => 0,
            Self::Dim => 1,
            Self::Fold => 2,
        }
    }
}

/// How what is new since the baseline is marked in the full change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkOptions {
    /// How marked lines show.
    pub style: MarkStyle,
    /// Whether the changed words of a line modified since the baseline are marked.
    pub words: bool,
    /// How hunks with nothing new show.
    pub seen: SeenHunks,
    /// Whether code removed since the baseline shows inline instead of behind its marker.
    pub removed_inline: bool,
}

impl Default for MarkOptions {
    fn default() -> Self {
        Self {
            style: MarkStyle::default(),
            words: true,
            seen: SeenHunks::default(),
            removed_inline: false,
        }
    }
}

impl MarkOptions {
    /// A number that differs whenever the options do, for cache keys and revisions.
    #[must_use]
    pub const fn stamp(self) -> u64 {
        self.style.index()
            | (self.words as u64) << 2
            | self.seen.index() << 3
            | (self.removed_inline as u64) << 5
    }
}

/// The `compare` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompareConfig {
    /// What a review compares with when nothing else was chosen.
    pub default_baseline: DefaultBaseline,
    /// Which looks count.
    pub baseline_look: BaselineLook,
    /// Seconds on a layer that count as opening it.
    pub dwell_seconds: u64,
    /// The view a comparison starts in.
    pub start_view: StartView,
    /// How layers that were only rebased show.
    pub rebased_layers: RebasedLayers,
    /// When the push timeline shows.
    pub timeline: TimelineMode,
    /// How what is new since the baseline is marked.
    pub marks: MarkOptions,
}

impl Default for CompareConfig {
    fn default() -> Self {
        Self {
            default_baseline: DefaultBaseline::default(),
            baseline_look: BaselineLook::default(),
            dwell_seconds: 8,
            start_view: StartView::default(),
            rebased_layers: RebasedLayers::default(),
            timeline: TimelineMode::default(),
            marks: MarkOptions::default(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawCompare {
    default_baseline: Option<String>,
    baseline_look: Option<BaselineLook>,
    dwell_seconds: Option<u64>,
    start_view: Option<StartView>,
    rebased_layers: Option<RebasedLayers>,
    timeline: Option<TimelineMode>,
    mark_style: Option<MarkStyle>,
    word_marks: Option<bool>,
    seen_hunks: Option<SeenHunks>,
    removed_inline: Option<bool>,
}

impl CompareConfig {
    /// The `compare` table of the plugin `table`, with a diagnostic when part of it is
    /// unreadable (the defaults are used for that part).
    #[must_use]
    pub fn from_plugin_table(table: &toml::Table) -> (Self, Option<String>) {
        let Some(value) = table.get("compare") else {
            return (Self::default(), None);
        };
        let problem = |message: String| {
            Some(format!(
                "plugins.config.\"{}\".compare: {message}",
                super::PLUGIN_ID
            ))
        };
        let raw = match value.clone().try_into::<RawCompare>() {
            Ok(raw) => raw,
            Err(error) => {
                return (
                    Self::default(),
                    problem(
                        error
                            .to_string()
                            .lines()
                            .next()
                            .unwrap_or_default()
                            .to_owned(),
                    ),
                );
            }
        };
        let defaults = Self::default();
        let mut config = Self {
            default_baseline: defaults.default_baseline,
            baseline_look: raw.baseline_look.unwrap_or(defaults.baseline_look),
            dwell_seconds: raw.dwell_seconds.unwrap_or(defaults.dwell_seconds),
            start_view: raw.start_view.unwrap_or(defaults.start_view),
            rebased_layers: raw.rebased_layers.unwrap_or(defaults.rebased_layers),
            timeline: raw.timeline.unwrap_or(defaults.timeline),
            marks: MarkOptions {
                style: raw.mark_style.unwrap_or(defaults.marks.style),
                words: raw.word_marks.unwrap_or(defaults.marks.words),
                seen: raw.seen_hunks.unwrap_or(defaults.marks.seen),
                removed_inline: raw.removed_inline.unwrap_or(defaults.marks.removed_inline),
            },
        };
        let mut diagnostic = None;
        if let Some(text) = raw.default_baseline {
            match DefaultBaseline::parse(&text) {
                Some(baseline) => config.default_baseline = baseline,
                None => {
                    diagnostic = problem(format!(
                        "default_baseline must be \"last_looked\", \"previous_push\", \"none\", \
                         or a duration such as \"24h\" (not {text:?})"
                    ));
                }
            }
        }
        (config, diagnostic)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str) -> (CompareConfig, Option<String>) {
        let table: toml::Table = toml::from_str(text).expect("toml");
        CompareConfig::from_plugin_table(&table)
    }

    #[test]
    fn the_compare_table_reads_each_setting_and_reports_what_it_cannot() {
        assert_eq!(read(""), (CompareConfig::default(), None));
        let (config, problem) = read(
            "[compare]\ndefault_baseline = \"24h\"\nbaseline_look = \"opened\"\n\
             dwell_seconds = 3\nstart_view = \"changes\"\nrebased_layers = \"hide\"\n\
             timeline = \"always\"",
        );
        assert_eq!(problem, None);
        assert_eq!(config.default_baseline, DefaultBaseline::Ago(86_400_000));
        assert_eq!(config.baseline_look, BaselineLook::Opened);
        assert_eq!(config.dwell_seconds, 3);
        assert_eq!(config.start_view, StartView::Changes);
        assert_eq!(config.rebased_layers, RebasedLayers::Hide);
        assert_eq!(config.timeline, TimelineMode::Always);
        let (config, problem) = read("[compare]\ndefault_baseline = \"lately\"");
        assert_eq!(config.default_baseline, DefaultBaseline::LastLooked);
        assert!(problem.is_some_and(|problem| problem.contains("default_baseline")));
        let (_, problem) = read("[compare]\nunknown = 1");
        assert!(problem.is_some());
    }

    #[test]
    fn mark_options_read_from_the_compare_table() {
        let (config, problem) = read("[compare]\ndwell_seconds = 3");
        assert_eq!(problem, None);
        assert_eq!(config.marks, MarkOptions::default());
        assert_eq!(config.marks.style, MarkStyle::Both);
        assert!(config.marks.words);
        assert_eq!(config.marks.seen, SeenHunks::Show);
        assert!(!config.marks.removed_inline);
        let (config, problem) = read(
            "[compare]\nmark_style = \"strong\"\nword_marks = false\nseen_hunks = \"fold\"\n\
             removed_inline = true",
        );
        assert_eq!(problem, None);
        assert_eq!(
            config.marks,
            MarkOptions {
                style: MarkStyle::Strong,
                words: false,
                seen: SeenHunks::Fold,
                removed_inline: true,
            }
        );
        let (config, problem) = read("[compare]\nmark_style = \"loud\"");
        assert_eq!(config.marks, MarkOptions::default());
        assert!(problem.is_some());
        let mut stamps: Vec<u64> = Vec::new();
        for style in MarkStyle::ALL {
            for seen in SeenHunks::ALL {
                for words in [false, true] {
                    for removed_inline in [false, true] {
                        stamps.push(
                            MarkOptions {
                                style,
                                words,
                                seen,
                                removed_inline,
                            }
                            .stamp(),
                        );
                    }
                }
            }
        }
        let count = stamps.len();
        stamps.sort_unstable();
        stamps.dedup();
        assert_eq!(stamps.len(), count, "every option set has its own stamp");
    }

    #[test]
    fn durations_read_with_their_unit() {
        assert_eq!(parse_duration_ms("90s"), Some(90_000));
        assert_eq!(parse_duration_ms("2w"), Some(14 * 86_400_000));
        assert_eq!(parse_duration_ms("0h"), None);
        assert_eq!(parse_duration_ms("5"), None);
        assert_eq!(parse_duration_ms("h"), None);
        assert_eq!(
            DefaultBaseline::Ago(1_000).baseline(5_000),
            ReviewCompareBaseline::Between {
                from_ms: 4_000,
                to_ms: None
            }
        );
    }
}
