//! The file panel's comparison legend: a few rows over the file tree saying what the panel shows
//! under the comparison in effect.
//!
//! * The first row names the comparison (`Changes since you last looked · 4m ago`) and the view
//!   options on; a click opens the comparison menu.
//! * With one pull request (or one layer at a time) the next row says what its comparison found,
//!   plainly when nothing changed (`No changes since you last looked (4m ago) · only rebased`);
//!   with every layer it sums the layers up.
//! * Rows then say what the comparison hides (pull requests without real changes, files the same
//!   as at the baseline) and how to show it again; a click on such a row does.
//!
//! Each row is its own BMUX text block with its own layout id: the row wraps within the panel,
//! and its click target is exactly the rect it was laid out in.

use super::ReviewSurface;
use super::stack::StackView;
use bcode_code_review_models::{DeltaKind, ReviewCompareBaseline as Baseline};
use bmux_tui::geometry::Point;
use bmux_tui::text::{Line, Span};

/// Layout id prefix of the legend's rows (followed by the row's index).
pub(super) const LEGEND_ROW_ID: &str = "review.files.legend.";

/// What a click on a legend row does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LegendAction {
    /// Open the comparison menu.
    OpenMenu,
    /// Show the files the comparison hides.
    ShowFiles,
    /// Hide them again.
    HideFiles,
    /// Show every pull request again (turn off "only pull requests with real changes").
    ShowAllPulls,
}

/// One row of the legend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LegendRow {
    /// The row's content.
    pub line: Line,
    /// What a click on it does.
    pub action: Option<LegendAction>,
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

impl ReviewSurface {
    /// The sources the file panel shows (one when one layer shows at a time).
    fn panel_sources(&self) -> Vec<String> {
        self.review
            .as_ref()
            .map(|review| review.files.sources().to_vec())
            .unwrap_or_default()
    }

    /// What "since" refers to, for sentences (`you last looked (4m ago)`, `the previous push`).
    fn since_phrase(&self) -> String {
        if self.comparing.stack_at().is_some() || !self.comparing.picks().is_empty() {
            return "the versions picked".to_owned();
        }
        match self.comparing.baseline() {
            Baseline::LastLooked => self.last_looked_detail().map_or_else(
                || "you last looked".to_owned(),
                |when| format!("you last looked ({when})"),
            ),
            Baseline::PreviousPush => "the previous push".to_owned(),
            Baseline::Between { .. } => self.baseline_labels()[0].clone(),
            Baseline::Off => "the baseline".to_owned(),
        }
    }

    /// The legend's rows; empty when the review compares nothing it could.
    pub(super) fn compare_legend(&self) -> Vec<LegendRow> {
        if self.review.is_none() || !self.compares_pulls() {
            return Vec::new();
        }
        let theme = &self.theme;
        let separator = theme.icons.separator;
        let off = self.comparing.baseline() == Baseline::Off
            && self.comparing.picks().is_empty()
            && self.comparing.stack_at().is_none();
        let mut rows = Vec::new();
        if off {
            let key = self
                .action_keys
                .get("compare_baseline")
                .map_or_else(String::new, |key| format!(" ({key} to compare)"));
            let mut spans = vec![Span::styled(
                format!("Full change: no comparison{key}"),
                theme.muted,
            )];
            // Options set while nothing is compared apply once something is.
            for [chip, _] in self.compare_chips() {
                spans.push(Span::styled(format!("{separator}{chip}"), theme.muted));
            }
            rows.push(LegendRow {
                line: Line::from_spans(spans),
                action: Some(LegendAction::OpenMenu),
            });
            return rows;
        }
        // What is compared, and the view options on.
        let mut spans = vec![Span::styled(
            format!("Changes since {}", self.baseline_labels()[0]),
            theme.accent,
        )];
        for [chip, _] in self.compare_chips() {
            spans.push(Span::styled(
                format!("{separator}{chip}"),
                theme.since.badge,
            ));
        }
        rows.push(LegendRow {
            line: Line::from_spans(spans),
            action: Some(LegendAction::OpenMenu),
        });
        let sources = self.panel_sources();
        let every_layer =
            self.shown_stack_view() == Some(StackView::AllLayers) && self.steps_through_stack();
        match sources.as_slice() {
            [source] if !every_layer => rows.extend(self.source_legend(source)),
            _ => rows.extend(self.sources_legend()),
        }
        rows.extend(self.filtered_legend());
        rows.extend(self.hidden_files_legend());
        rows
    }

    /// What one source's comparison found.
    fn source_legend(&self, source: &str) -> Option<LegendRow> {
        let theme = &self.theme;
        let doc = &self.review.as_ref()?.doc;
        let since = self.since_phrase();
        let Some(comparison) = doc.comparison(source) else {
            let text = if self.compare_settling() {
                format!(
                    "{} Reading what you looked at{}",
                    theme.icons.ellipsis, theme.icons.ellipsis
                )
            } else {
                "Not compared: no earlier version to compare with".to_owned()
            };
            return Some(LegendRow {
                line: Line::from_spans(vec![Span::styled(text, theme.muted)]),
                action: None,
            });
        };
        let minus = theme.icons.minus;
        let (text, style) = match comparison.delta.as_ref().map(|delta| delta.kind) {
            None => (
                format!("{} Comparing{}", theme.icons.ellipsis, theme.icons.ellipsis),
                theme.muted,
            ),
            Some(kind @ (DeltaKind::Changed | DeltaKind::WithRebase)) => {
                let summary = doc.comparison_summary(source).unwrap_or_default();
                let total = self
                    .review
                    .as_ref()
                    .map_or(0, |review| review.files.total_files());
                let rebase = if kind == DeltaKind::WithRebase {
                    " (rebase not taken out)"
                } else {
                    ""
                };
                (
                    format!(
                        "{} of {} changed since {since}: +{} {minus}{}{rebase}",
                        summary.files,
                        plural(total, "file", "files"),
                        summary.added,
                        summary.removed
                    ),
                    theme.since.badge,
                )
            }
            Some(DeltaKind::Unchanged) => (format!("No changes since {since}"), theme.text),
            Some(DeltaKind::RebasedOnly) => (
                format!("No changes since {since}: only rebased"),
                theme.text,
            ),
            Some(DeltaKind::NewToYou) => ("New to you: every file is new".to_owned(), theme.accent),
            Some(DeltaKind::Unavailable) => (
                "The baseline's commits are gone: showing the full change".to_owned(),
                theme.notice,
            ),
        };
        Some(LegendRow {
            line: Line::from_spans(vec![Span::styled(text, style)]),
            action: None,
        })
    }

    /// What every source's comparison found, and the pull requests left out.
    fn sources_legend(&self) -> Vec<LegendRow> {
        let theme = &self.theme;
        let counts = self.compare_counts();
        let mut rows = Vec::new();
        if self.comparing.active() {
            let text = super::compare_view::summary_text(counts);
            let style = if counts.changed > 0 {
                theme.since.badge
            } else {
                theme.text
            };
            rows.push(LegendRow {
                line: Line::from_spans(vec![Span::styled(text, style)]),
                action: None,
            });
        }
        rows
    }

    /// The pull requests "only pull requests with real changes" leaves out: hidden with every
    /// layer shown, skipped by `.` and `,` one at a time.
    fn filtered_legend(&self) -> Option<LegendRow> {
        let theme = &self.theme;
        let filtered = self.compare_filtered().len();
        if filtered == 0 {
            return None;
        }
        let pulls = plural(filtered, "pull request", "pull requests");
        let text = if self.shown_stack_view() == Some(StackView::AllLayers) {
            format!("{pulls} hidden: no real changes ")
        } else {
            format!("{pulls} without real changes skipped ")
        };
        Some(LegendRow {
            line: Line::from_spans(vec![
                Span::styled(text, theme.muted),
                Span::styled("[show all]", theme.gap_action),
            ]),
            action: Some(LegendAction::ShowAllPulls),
        })
    }

    /// The files the comparison hides in the panel's sources, and how to show them.
    fn hidden_files_legend(&self) -> Option<LegendRow> {
        let theme = &self.theme;
        let review = self.review.as_ref()?;
        let hidden = review.files.hidden_files();
        if hidden > 0 {
            return Some(LegendRow {
                line: Line::from_spans(vec![
                    Span::styled(
                        format!(
                            "{} hidden: unchanged since then ",
                            plural(hidden, "file", "files")
                        ),
                        theme.muted,
                    ),
                    Span::styled("[show]", theme.gap_action),
                ]),
                action: Some(LegendAction::ShowFiles),
            });
        }
        let shown = review.files.hideable_shown();
        (shown > 0).then(|| LegendRow {
            line: Line::from_spans(vec![
                Span::styled(
                    format!(
                        "{} the comparison would hide are shown, dimmed ",
                        plural(shown, "file", "files")
                    ),
                    theme.muted,
                ),
                Span::styled("[hide]", theme.gap_action),
            ]),
            action: Some(LegendAction::HideFiles),
        })
    }

    /// Run a click on the legend at `position`; returns whether it was on an active row.
    pub(super) fn legend_click(&mut self, position: Point) -> bool {
        // The rows as built into the panel last laid out: each action with its painted rect.
        let actions = self.legend_actions.borrow().clone();
        let Some(action) = actions.iter().enumerate().find_map(|(index, action)| {
            let area = self.body_area(&format!("{LEGEND_ROW_ID}{index}"))?;
            area.contains(position).then_some(*action).flatten()
        }) else {
            return false;
        };
        self.legend_action(action);
        true
    }

    /// Run a legend action.
    pub(super) fn legend_action(&mut self, action: LegendAction) {
        match action {
            LegendAction::OpenMenu => self.open_compare_menu(),
            LegendAction::ShowAllPulls => {
                if self.comparing.only_changed() {
                    self.toggle_only_changed();
                }
            }
            LegendAction::ShowFiles => self.show_compare_hidden(true),
            LegendAction::HideFiles => self.show_compare_hidden(false),
        }
    }

    /// Show (or hide again) the files the comparison hides in the panel's sources.
    pub(super) fn show_compare_hidden(&mut self, shown: bool) {
        let sources = self.panel_sources();
        let (theme, file_view) = (self.theme, self.file_view);
        let Some(review) = self.review.as_mut() else {
            return;
        };
        let mut changed = false;
        for source in &sources {
            if review.doc.comparison(source).is_some() {
                changed |= review.doc.set_compare_files_shown(source, shown);
            }
        }
        if changed {
            review.note_splices();
            review.rebuild_files(&theme, file_view);
            // Rows already measured may lay out differently beside the files shown (or hidden)
            // again: measure them again.
            self.layout_revision += 1;
        }
    }
}
