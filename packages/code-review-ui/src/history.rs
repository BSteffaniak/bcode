//! The stack history panel, and the work behind it.
//!
//! Loading a stack's history (the version log, the branch reflogs, and, when asked, the forge's
//! push history with old commits fetched) and turning two points of it into a review. See
//! `docs/review-stack-history.md`.
//!
//! The panel is a matrix: one row per layer, one column per push (the first column is before
//! the first push, the last is now). Each cell says how the layer changed in that push.

use super::modal;
use super::services::ReviewServices;
use super::theme::ReviewTheme;
use bcode_code_review::history::compare::{self, Comparison};
use bcode_code_review::history::{self, TimelineOptions, forge, reflog, version_at};
use bcode_code_review::provider::ProviderCallError;
use bcode_code_review_models::{
    ChangeKind, LayerTimeline, ListLayerVersionsRequest, ListLayerVersionsResponse,
    RecordLayerVersionsRequest, RecordLayerVersionsResponse, ReviewSourceKind,
    ReviewSourcePullsHistoryRequest, ReviewStack, StackTimeline,
};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::composition::TextBlock;
use bmux_tui::geometry::{Rect, Size};
use bmux_tui::paint::PaintCx;
use bmux_tui::text::{Line, Span, Text};
use bmux_tui_components::picker_frame::{PickerFrame, PickerFrameComponent, PickerFramePolicy};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const FRAME_ID: &str = "review.history";

/// Width of the layer column.
const LABEL_WIDTH: usize = 34;

/// The stack a history is about, and what this reviewer reviewed.
#[derive(Debug, Clone)]
pub struct HistoryScope {
    /// Repository.
    pub repo_path: PathBuf,
    /// Provider id.
    pub provider: String,
    /// Provider repository.
    pub repository: String,
    /// The stack now.
    pub stack: ReviewStack,
    /// Head commits this reviewer reviewed.
    pub reviewed_heads: BTreeSet<String>,
}

/// Milliseconds since the Unix epoch, now.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| u64::try_from(since.as_millis()).ok())
        .unwrap_or(0)
}

/// How long ago `at_ms` was, briefly ("5m ago").
#[must_use]
pub fn ago(at_ms: u64, now_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(at_ms) / 1000;
    match seconds {
        0..60 => "just now".to_owned(),
        60..3_600 => format!("{}m ago", seconds / 60),
        3_600..86_400 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

fn failed(message: impl Into<String>) -> ProviderCallError {
    ProviderCallError::new("history_failed", message)
}

/// Load `scope`'s history: the version log, with what the branch reflogs show (and, with
/// `forge`, the provider's push history, its old commits fetched) recorded into it first.
///
/// # Errors
///
/// Returns a message when git or the store fails; a provider that cannot report history only
/// leaves its part out.
pub async fn load(
    services: ReviewServices,
    scope: HistoryScope,
    forge: bool,
) -> Result<StackTimeline, String> {
    let registry = services.providers();
    let (repo, provider, repository) = (
        scope.repo_path.clone(),
        scope.provider.clone(),
        scope.repository.clone(),
    );
    let stack = scope.stack.clone();
    let found = services
        .blocking(move || {
            let layers: Vec<(u64, String)> = stack
                .layers
                .iter()
                .map(|layer| (layer.pull.number, layer.pull.head_branch.clone()))
                .collect();
            let mut versions = reflog::reflog_versions(&repo, &provider, &repository, &layers)
                .map_err(|error| failed(error.to_string()))?;
            let route = registry
                .source(&provider)
                .filter(|route| route.manifest.capabilities.batched.pulls_history);
            if forge && let Some(route) = route {
                let answer = route
                    .provider
                    .get_history(&ReviewSourcePullsHistoryRequest {
                        repo_path: repo.clone(),
                        repository: Some(repository.clone()),
                        numbers: layers.iter().map(|(number, _)| *number).collect(),
                        fetch: true,
                    });
                if let Ok(answer) = answer {
                    let bases: BTreeMap<u64, String> = stack
                        .layers
                        .iter()
                        .map(|layer| (layer.pull.number, layer.pull.base_branch.clone()))
                        .collect();
                    versions.extend(forge::forge_versions(
                        &provider,
                        &repository,
                        &answer.histories,
                        &bases,
                    ));
                }
            }
            Ok(versions)
        })
        .await
        .map_err(|error| error.message)?;
    if !found.is_empty() {
        let _: RecordLayerVersionsResponse = services
            .call(RecordLayerVersionsRequest {
                repo_path: scope.repo_path.clone(),
                versions: found,
            })
            .await
            .map_err(|error| error.message)?;
    }
    let listed: ListLayerVersionsResponse = services
        .call(ListLayerVersionsRequest {
            repo_path: scope.repo_path.clone(),
            provider: scope.provider.clone(),
            repository: scope.repository.clone(),
            numbers: scope
                .stack
                .layers
                .iter()
                .map(|layer| layer.pull.number)
                .collect(),
        })
        .await
        .map_err(|error| error.message)?;
    let options = TimelineOptions {
        push_window_ms: 0,
        reviewed_heads: scope.reviewed_heads,
    };
    let (repo, stack) = (scope.repo_path, scope.stack);
    services
        .blocking(move || {
            history::timeline(&repo, &stack, &listed.versions, &options)
                .map_err(|error| failed(error.to_string()))
        })
        .await
        .map_err(|error| error.message)
}

/// A comparison the panel asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryRequest {
    /// One layer between two times.
    Layer {
        /// The layer.
        number: u64,
        /// From the version current then.
        from_ms: u64,
        /// To the version current then.
        to_ms: u64,
    },
    /// Every layer whose change differs between two times (`None`: now).
    Between {
        /// From.
        from_ms: u64,
        /// To.
        to_ms: Option<u64>,
        /// Also layers that were only rebased.
        include_rebased: bool,
    },
    /// Every layer as it was at a time.
    StackAt {
        /// When.
        at_ms: u64,
    },
    /// The whole stack's change since a time.
    WholeSince {
        /// Since.
        at_ms: u64,
    },
    /// Every layer's change since this reviewer last reviewed it.
    SinceReview {
        /// Also layers that were only rebased.
        include_rebased: bool,
    },
}

/// The review `request` asks for: its title and its sources (labelled).
///
/// # Errors
///
/// Returns a message when git fails.
pub async fn comparisons(
    services: ReviewServices,
    repo_path: PathBuf,
    timeline: StackTimeline,
    request: HistoryRequest,
) -> Result<(String, Vec<Comparison>), String> {
    let now = now_ms();
    services
        .blocking(move || {
            let root = repo_path.as_path();
            let fail = |error: bcode_code_review::data::DataError| failed(error.to_string());
            let when = |at_ms: u64| {
                if at_ms == u64::MAX {
                    "now".to_owned()
                } else {
                    ago(at_ms, now)
                }
            };
            Ok(match request {
                HistoryRequest::Layer {
                    number,
                    from_ms,
                    to_ms,
                } => {
                    let layer = timeline
                        .layers
                        .iter()
                        .find(|layer| layer.number == number)
                        .ok_or_else(|| failed(format!("#{number} is not in the stack")))?;
                    let (Some(from), Some(to)) =
                        (version_at(layer, from_ms), version_at(layer, to_ms))
                    else {
                        return Ok((String::new(), Vec::new()));
                    };
                    (
                        format!("#{number} from {} to {}", when(from_ms), when(to_ms)),
                        compare::layer_changes(root, layer, from, to)
                            .map_err(fail)?
                            .into_iter()
                            .collect(),
                    )
                }
                HistoryRequest::Between {
                    from_ms,
                    to_ms,
                    include_rebased,
                } => (
                    to_ms.map_or_else(
                        || format!("Stack since {}", when(from_ms)),
                        |to_ms| format!("Stack from {} to {}", when(from_ms), when(to_ms)),
                    ),
                    compare::changes_between(root, &timeline, from_ms, to_ms, include_rebased)
                        .map_err(fail)?,
                ),
                HistoryRequest::StackAt { at_ms } => (
                    format!("Stack as it was {}", when(at_ms)),
                    compare::stack_at(&timeline, at_ms),
                ),
                HistoryRequest::WholeSince { at_ms } => (
                    format!("Whole stack since {}", when(at_ms)),
                    compare::whole_stack_since(root, &timeline, at_ms)
                        .map_err(fail)?
                        .into_iter()
                        .collect(),
                ),
                HistoryRequest::SinceReview { include_rebased } => (
                    "Stack since your last review".to_owned(),
                    compare::changes_since_review(root, &timeline, include_rebased)
                        .map_err(fail)?,
                ),
            })
        })
        .await
        .map_err(|error| error.message)
}

/// The sources of `comparisons`, as a review opens them.
#[must_use]
pub fn sources_of(comparisons: &[Comparison]) -> Vec<ReviewSourceKind> {
    comparisons
        .iter()
        .map(|comparison| comparison.source.clone())
        .collect()
}

/// What a key in the panel led to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryOutcome {
    /// Close the panel.
    Close,
    /// Draw again.
    Redraw,
    /// Not the panel's key.
    Ignored,
    /// Compute a comparison.
    Compare(HistoryRequest),
}

/// The history panel.
#[derive(Debug)]
pub struct HistoryPanel {
    timeline: Option<StackTimeline>,
    layer: usize,
    column: usize,
    /// The column a comparison starts from, when pinned.
    from: Option<usize>,
    include_rebased: bool,
    note: Option<String>,
    /// Each layer's reviews (approvals and requests for changes), from its conversation.
    reviews: BTreeMap<u64, Vec<ReviewMark>>,
}

/// A review shown on the timeline: who, what, and on which head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewMark {
    /// Reviewer.
    pub login: String,
    /// Approved (else changes requested).
    pub approved: bool,
    /// The head it was given on.
    pub commit: Option<String>,
}

impl HistoryPanel {
    /// A panel waiting for the history.
    #[must_use]
    pub fn loading() -> Self {
        Self {
            timeline: None,
            layer: 0,
            column: 0,
            from: None,
            include_rebased: false,
            note: Some("Reading the stack's history…".to_owned()),
            reviews: BTreeMap::new(),
        }
    }

    /// Show each layer's reviews against its versions.
    pub fn set_reviews(&mut self, reviews: BTreeMap<u64, Vec<ReviewMark>>) {
        self.reviews = reviews;
    }

    /// The selected layer's reviews, and whether each still applies to its newest version.
    fn review_line(&self, timeline: &StackTimeline, theme: &ReviewTheme) -> Option<Line> {
        use bcode_code_review::conversation::{Currency, review_currency};
        let layer = timeline.layers.get(self.layer)?;
        let marks = self
            .reviews
            .get(&layer.number)
            .filter(|marks| !marks.is_empty())?;
        let head = layer.versions.last()?.version.head_oid.as_str();
        let mut spans = vec![Span::styled(
            format!("#{} reviews: ", layer.number),
            theme.title,
        )];
        for (index, mark) in marks.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(theme.icons.separator, theme.muted));
            }
            let (glyph, style) = if mark.approved {
                (theme.icons.approved, theme.badge_resolved)
            } else {
                (theme.icons.changes, theme.error)
            };
            spans.push(Span::styled(format!("{glyph} {}", mark.login), style));
            let currency = mark.commit.as_deref().map_or(Currency::Unknown, |commit| {
                review_currency(layer, commit, head)
            });
            let (text, style) = match currency {
                Currency::Current => (" on the newest version", theme.muted),
                Currency::Rebased => (" before a rebase; still applies", theme.muted),
                Currency::Outdated => (" on an older version", theme.badge_outdated),
                Currency::Unknown => ("", theme.muted),
            };
            spans.push(Span::styled(text, style));
        }
        Some(Line::from_spans(spans))
    }

    /// Show `timeline`, the cursor on the newest column.
    pub fn set_timeline(&mut self, timeline: StackTimeline) {
        self.column = timeline.pushes.len();
        self.layer = self.layer.min(timeline.layers.len().saturating_sub(1));
        self.note = timeline
            .pushes
            .is_empty()
            .then(|| "No pushes recorded yet: the stack is as it was first seen.".to_owned());
        self.timeline = Some(timeline);
    }

    /// Say `note` under the matrix (a failure, or that a comparison found nothing).
    pub fn set_note(&mut self, note: impl Into<String>) {
        self.note = Some(note.into());
    }

    /// The history shown.
    #[must_use]
    pub const fn timeline(&self) -> Option<&StackTimeline> {
        self.timeline.as_ref()
    }

    /// The time column `column` shows: just before push `column`, or now for the last.
    fn time(timeline: &StackTimeline, column: usize) -> u64 {
        timeline
            .pushes
            .get(column)
            .map_or(u64::MAX, |push| push.at_ms.saturating_sub(1))
    }

    /// Answer a key.
    pub fn key(&mut self, stroke: KeyStroke) -> HistoryOutcome {
        if !stroke.modifiers.is_empty() && !matches!(stroke.key, KeyCode::Char(_)) {
            return HistoryOutcome::Ignored;
        }
        if matches!(stroke.key, KeyCode::Escape | KeyCode::Char('q')) {
            return HistoryOutcome::Close;
        }
        let Some(timeline) = &self.timeline else {
            return HistoryOutcome::Ignored;
        };
        let columns = timeline.pushes.len() + 1;
        let layers = timeline.layers.len();
        let at = |column: usize| Self::time(timeline, column);
        let from = self
            .from
            .filter(|from| *from != self.column)
            .or_else(|| self.column.checked_sub(1));
        let request = match stroke.key {
            KeyCode::Left | KeyCode::Char('h') => {
                self.column = self.column.saturating_sub(1);
                return HistoryOutcome::Redraw;
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.column = (self.column + 1).min(columns - 1);
                return HistoryOutcome::Redraw;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.layer = self.layer.saturating_sub(1);
                return HistoryOutcome::Redraw;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.layer = (self.layer + 1).min(layers.saturating_sub(1));
                return HistoryOutcome::Redraw;
            }
            KeyCode::Char('f') => {
                self.from = (self.from != Some(self.column)).then_some(self.column);
                return HistoryOutcome::Redraw;
            }
            KeyCode::Char('i') => {
                self.include_rebased = !self.include_rebased;
                return HistoryOutcome::Redraw;
            }
            KeyCode::Enter => {
                let Some(from) = from else {
                    return HistoryOutcome::Ignored;
                };
                HistoryRequest::Layer {
                    number: timeline.layers[self.layer].number,
                    from_ms: at(from.min(self.column)),
                    to_ms: at(from.max(self.column)),
                }
            }
            KeyCode::Char('p') => {
                let Some(from) = from else {
                    return HistoryOutcome::Ignored;
                };
                HistoryRequest::Between {
                    from_ms: at(from.min(self.column)),
                    to_ms: Some(at(from.max(self.column))),
                    include_rebased: self.include_rebased,
                }
            }
            KeyCode::Char('s') => HistoryRequest::Between {
                from_ms: at(self.column),
                to_ms: None,
                include_rebased: self.include_rebased,
            },
            KeyCode::Char('a') => HistoryRequest::StackAt {
                at_ms: at(self.column),
            },
            KeyCode::Char('w') => HistoryRequest::WholeSince {
                at_ms: at(self.column),
            },
            KeyCode::Char('r') => HistoryRequest::SinceReview {
                include_rebased: self.include_rebased,
            },
            _ => return HistoryOutcome::Ignored,
        };
        self.note = Some("Comparing…".to_owned());
        HistoryOutcome::Compare(request)
    }

    /// How layer `layer` changed in column `column`: a glyph, and whether that version was
    /// reviewed. Blank before the layer existed; a dot when it did not change.
    fn cell(
        timeline: &StackTimeline,
        layer: &LayerTimeline,
        column: usize,
    ) -> (&'static str, bool) {
        let Some(index) = version_at(layer, Self::time(timeline, column)) else {
            return (" ", false);
        };
        let before = column
            .checked_sub(1)
            .and_then(|before| version_at(layer, Self::time(timeline, before)));
        let version = &layer.versions[index];
        if before == Some(index) {
            return ("·", version.reviewed);
        }
        let glyph = match version.kind {
            ChangeKind::First | ChangeKind::Content => "●",
            ChangeKind::CommitsAdded => "+",
            ChangeKind::RebaseOnly => "○",
            ChangeKind::Retargeted => "⇄",
            ChangeKind::Unavailable => "?",
        };
        (glyph, version.reviewed)
    }

    fn lines(&self, theme: &ReviewTheme, width: usize) -> Vec<Line> {
        let Some(timeline) = &self.timeline else {
            return vec![Line::from_spans(vec![Span::styled(
                self.note.clone().unwrap_or_default(),
                theme.muted,
            )])];
        };
        let now = now_ms();
        let columns = timeline.pushes.len() + 1;
        // The columns that fit, keeping the cursor in view.
        let fit = (width.saturating_sub(LABEL_WIDTH + 2) / 2).max(1);
        let first = (self.column + 1)
            .saturating_sub(fit)
            .min(columns.saturating_sub(fit));
        let shown = first..(first + fit).min(columns);
        let mut lines = Vec::new();
        // What the selected column is.
        let describe = |column: usize| -> String {
            if column == 0 {
                return "before the first push".to_owned();
            }
            let push = &timeline.pushes[column - 1];
            format!(
                "after push {column} of {}, {}: {}",
                timeline.pushes.len(),
                ago(push.at_ms, now),
                history::push_summary(push)
            )
        };
        let mut header = vec![Span::styled(
            format!("{:<LABEL_WIDTH$}  ", describe(self.column)),
            theme.title,
        )];
        for column in shown.clone() {
            let mark = if column == self.column {
                "▼"
            } else if Some(column) == self.from {
                "f"
            } else {
                " "
            };
            header.push(Span::styled(format!("{mark} "), theme.accent));
        }
        lines.push(Line::from_spans(header).truncate(width));
        for (index, layer) in timeline.layers.iter().enumerate() {
            let mut label = format!("#{} {}", layer.number, layer.title);
            if label.chars().count() > LABEL_WIDTH {
                label = label.chars().take(LABEL_WIDTH - 1).collect::<String>() + "…";
            }
            let style = if index == self.layer {
                theme.title
            } else {
                theme.text
            };
            let mut spans = vec![Span::styled(format!("{label:<LABEL_WIDTH$}  "), style)];
            for column in shown.clone() {
                let (glyph, reviewed) = Self::cell(timeline, layer, column);
                let style = if index == self.layer && column == self.column {
                    theme.selection
                } else if reviewed {
                    theme.viewed
                } else if glyph == "○" || glyph == "·" {
                    theme.muted
                } else {
                    theme.text
                };
                spans.push(Span::styled(glyph.to_owned(), style));
                spans.push(Span::raw(" "));
            }
            lines.push(Line::from_spans(spans).truncate(width));
        }
        lines.push(Line::from_spans(Vec::new()));
        if let Some(reviews) = self.review_line(timeline, theme) {
            lines.push(reviews.truncate(width));
        }
        lines.push(Line::from_spans(vec![Span::styled(
            "● changed  + commits added  ○ rebased only  ⇄ retargeted  ? unavailable  · no change  \
             green: reviewed",
            theme.muted,
        )]));
        if self.include_rebased {
            lines.push(Line::from_spans(vec![Span::styled(
                "Rebased-only layers are included in comparisons (i to leave them out).",
                theme.muted,
            )]));
        }
        if let Some(note) = &self.note {
            lines.push(Line::from_spans(vec![Span::styled(
                note.clone(),
                theme.notice,
            )]));
        }
        lines
    }

    /// Paint over `area`.
    pub fn paint(&self, theme: &ReviewTheme, area: Rect, cx: &mut PaintCx<'_, '_>) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let max = Size::new(
            area.width.saturating_sub(4),
            area.height.saturating_sub(2).min(48),
        );
        let mut policy = PickerFramePolicy::palette().max_size(max);
        policy.input = false;
        policy.header = false;
        policy.margin = bmux_tui::geometry::Insets::all(1);
        policy.min_size = Size::new(area.width.min(40), area.height.min(10));
        let footer = "←→ push · ↑↓ layer · f pin from · enter layer · p push · s since · \
                      a as it was · w whole stack · r since review · i rebased · esc";
        let frame = PickerFrame::new()
            .title(" Stack history ")
            .footer(Line::from_spans(vec![Span::styled(footer, theme.muted)]))
            .policy(policy)
            .styles(theme.component.picker_frame_styles())
            .chrome(theme.component.chrome);
        let width = usize::from(max.width.saturating_sub(4));
        let body = TextBlock::new(Text::from_lines(self.lines(theme, width)));
        let component = PickerFrameComponent::new(FRAME_ID, frame, body);
        let _ = modal::paint(&component, theme, area, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::{LayerVersion, StackPush, TimelineVersion, VersionSource};

    fn version(head: &str, at: u64, kind: ChangeKind, reviewed: bool) -> TimelineVersion {
        TimelineVersion {
            version: LayerVersion {
                provider: "p".into(),
                repository: "o/r".into(),
                number: 1,
                head_oid: head.into(),
                base_oid: None,
                base_branch: None,
                seen_at_ms: at,
                actor: None,
                sources: BTreeSet::from([VersionSource::Observed]),
            },
            kind,
            fork_oid: None,
            reviewed,
        }
    }

    fn timeline() -> StackTimeline {
        let layer = |number: u64, versions: Vec<TimelineVersion>| LayerTimeline {
            number,
            title: format!("Layer {number}"),
            head_branch: format!("l{number}"),
            versions,
        };
        StackTimeline {
            layers: vec![
                layer(
                    1,
                    vec![
                        version("a", 10, ChangeKind::First, true),
                        version("b", 100, ChangeKind::Content, false),
                    ],
                ),
                layer(
                    2,
                    vec![
                        version("c", 10, ChangeKind::First, true),
                        version("d", 100, ChangeKind::RebaseOnly, false),
                        version("e", 200, ChangeKind::CommitsAdded, false),
                    ],
                ),
            ],
            pushes: vec![
                StackPush {
                    at_ms: 100,
                    layers: vec![(1, ChangeKind::Content), (2, ChangeKind::RebaseOnly)],
                },
                StackPush {
                    at_ms: 200,
                    layers: vec![(2, ChangeKind::CommitsAdded)],
                },
            ],
        }
    }

    fn key(code: KeyCode) -> KeyStroke {
        KeyStroke {
            key: code,
            modifiers: bmux_keyboard::Modifiers::default(),
        }
    }

    #[test]
    fn cells_show_how_each_layer_changed_in_each_push() {
        let timeline = timeline();
        let cells = |layer: usize| -> Vec<&str> {
            (0..3)
                .map(|column| HistoryPanel::cell(&timeline, &timeline.layers[layer], column).0)
                .collect()
        };
        assert_eq!(cells(0), ["●", "●", "·"]);
        assert_eq!(cells(1), ["●", "○", "+"]);
        // Before any push, both layers are as first reviewed.
        assert!(HistoryPanel::cell(&timeline, &timeline.layers[0], 0).1);
    }

    #[test]
    fn the_selected_layer_says_which_reviews_still_apply() {
        let theme = crate::theme::ReviewTheme::derive(None, crate::theme::IconStyle::Unicode);
        let mut panel = HistoryPanel::loading();
        panel.set_timeline(timeline());
        let mark = |login: &str, approved: bool, commit: &str| ReviewMark {
            login: login.to_owned(),
            approved,
            commit: Some(commit.to_owned()),
        };
        panel.set_reviews(BTreeMap::from([
            (1, vec![mark("ana", true, "a")]),
            (2, vec![mark("raj", false, "d"), mark("li", true, "e")]),
        ]));
        let text = |panel: &HistoryPanel| {
            panel
                .lines(&theme, 200)
                .iter()
                .map(Line::plain_text)
                .collect::<Vec<_>>()
                .join("\n")
        };
        // Layer 1 changed in content after ana approved it.
        assert!(
            text(&panel).contains("#1 reviews: ✓ ana on an older version"),
            "{}",
            text(&panel)
        );
        panel.layer = 1;
        // Layer 2 gained commits since raj's request: it no longer reads as current.
        let shown = text(&panel);
        assert!(shown.contains("✕ raj on an older version"), "{shown}");
        assert!(shown.contains("✓ li on the newest version"), "{shown}");
    }

    #[test]
    fn keys_ask_for_comparisons_between_the_cursor_and_the_column_before_or_the_pin() {
        let mut panel = HistoryPanel::loading();
        panel.set_timeline(timeline());
        // The cursor starts now; enter compares the layer with the column before (push 2).
        assert_eq!(
            panel.key(key(KeyCode::Enter)),
            HistoryOutcome::Compare(HistoryRequest::Layer {
                number: 1,
                from_ms: 199,
                to_ms: u64::MAX,
            })
        );
        // Pin the first column, move to now: the push request spans both pushes.
        for code in [
            KeyCode::Left,
            KeyCode::Left,
            KeyCode::Char('f'),
            KeyCode::Right,
            KeyCode::Right,
        ] {
            assert_eq!(panel.key(key(code)), HistoryOutcome::Redraw);
        }
        assert_eq!(
            panel.key(key(KeyCode::Char('p'))),
            HistoryOutcome::Compare(HistoryRequest::Between {
                from_ms: 99,
                to_ms: Some(u64::MAX),
                include_rebased: false,
            })
        );
        assert_eq!(
            panel.key(key(KeyCode::Char('r'))),
            HistoryOutcome::Compare(HistoryRequest::SinceReview {
                include_rebased: false
            })
        );
        assert_eq!(panel.key(key(KeyCode::Escape)), HistoryOutcome::Close);
    }
}
