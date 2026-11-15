//! Rows of a comparison's overlay: the summary under a compared source's header, markers of
//! lines removed since the baseline, the gutter bar of lines new since it, and file badges.
//!
//! Labels choose among variants by measured fit (BMUX display width), and clickable cells come
//! from the same parts as the painted variant. Removed lines are laid out by BMUX
//! `layout_diff_row`, like every other diff line.

use super::{ActionCells, ItemAction, ItemRows};
use crate::theme::ReviewTheme;
use bcode_code_review::document::{CompareNoteItem, CompareView, RemovedSinceItem};
use bcode_code_review_models::{DeltaKind, FileSince};
use bmux_tui::style::Style;
use bmux_tui::text::{Line, Span};
use bmux_tui::text_width::display_width;
use bmux_tui_components::diff_viewer::{
    DiffGutterMode, DiffLineKind, DiffRowItem, DiffRowLayout, DiffRowOptions, DiffRowText,
    DiffSourceSide, layout_diff_row,
};

/// An action on a comparison's rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareAction {
    /// Show or hide the files the comparison hides in the source.
    ShowFiles,
    /// Switch between the full change and only what changed.
    ToggleView,
    /// Show or hide a removed-since marker's text.
    ToggleRemoval,
}

/// What a comparison's rows say the changes are since, as the baseline in effect names it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Since {
    /// What the reviewer last looked at.
    #[default]
    LastLooked,
    /// The version before the stack's latest push.
    PreviousPush,
    /// A time, pushes, or versions picked.
    Then,
}

impl Since {
    /// The words after "since" (`you looked`, `the previous push`, `then`).
    #[must_use]
    pub const fn phrase(self) -> &'static str {
        match self {
            Self::LastLooked => "you looked",
            Self::PreviousPush => "the previous push",
            Self::Then => "then",
        }
    }

    /// A small number distinguishing the variants, for cache keys.
    #[must_use]
    pub const fn index(self) -> u64 {
        match self {
            Self::LastLooked => 0,
            Self::PreviousPush => 1,
            Self::Then => 2,
        }
    }
}

/// One labelled part of a row.
struct Part {
    text: String,
    action: Option<CompareAction>,
}

fn text(text: impl Into<String>) -> Part {
    Part {
        text: text.into(),
        action: None,
    }
}

fn act(text: impl Into<String>, action: CompareAction) -> Part {
    Part {
        text: text.into(),
        action: Some(action),
    }
}

fn plural(count: usize, one: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {one}s")
    }
}

/// The summary variants, widest first.
fn note_variants(note: &CompareNoteItem, since: Since, theme: &ReviewTheme) -> Vec<Vec<Part>> {
    let phrase = since.phrase();
    let separator = theme.icons.separator;
    let minus = theme.icons.minus;
    let view_toggle = || match note.view {
        CompareView::Full => act("[only what changed]", CompareAction::ToggleView),
        CompareView::Changes => act("[full change]", CompareAction::ToggleView),
    };
    let files_toggle = |hidden: String| {
        if note.files_shown {
            act("[hide them]", CompareAction::ShowFiles)
        } else {
            act(format!("[show {hidden}]"), CompareAction::ShowFiles)
        }
    };
    match note.kind {
        None => vec![
            vec![text(format!(
                "{} Comparing with {}",
                theme.icons.ellipsis,
                match since {
                    Since::LastLooked => "what you last looked at",
                    Since::PreviousPush => "the previous push",
                    Since::Then => "the versions picked",
                }
            ))],
            vec![text(format!("{} Comparing", theme.icons.ellipsis))],
            vec![text(theme.icons.ellipsis)],
        ],
        Some(DeltaKind::RebasedOnly) => vec![
            vec![
                text(format!("Rebased, nothing changed since {phrase} ")),
                files_toggle(plural(note.unchanged_files, "file")),
            ],
            vec![text("Only rebased "), files_toggle("files".to_owned())],
            vec![text("Only rebased")],
        ],
        Some(DeltaKind::Unchanged) if note.view == CompareView::Changes => vec![
            vec![
                text(format!("Unchanged since {phrase} ")),
                files_toggle(plural(note.unchanged_files, "file")),
            ],
            vec![text("Unchanged")],
        ],
        Some(DeltaKind::Unchanged) => vec![
            vec![text(format!("Unchanged since {phrase}"))],
            vec![text("Unchanged")],
        ],
        Some(DeltaKind::NewToYou) => vec![
            vec![text("New to you: the whole change")],
            vec![text("New to you")],
        ],
        Some(DeltaKind::Unavailable) => vec![
            vec![text(match since {
                Since::LastLooked => "The version you looked at is gone: showing the full change",
                Since::PreviousPush | Since::Then => {
                    "The version compared with is gone: showing the full change"
                }
            })],
            vec![text("Baseline unavailable")],
        ],
        Some(kind @ (DeltaKind::Changed | DeltaKind::WithRebase)) => {
            let counts = format!("+{} {minus}{}", note.added, note.removed);
            let rebase = if kind == DeltaKind::WithRebase {
                " (with the rebase)"
            } else {
                ""
            };
            let dropped = if note.dropped.is_empty() {
                String::new()
            } else {
                format!("{separator}{} dropped", note.dropped.len())
            };
            let unchanged = (note.unchanged_files > 0
                && (note.view == CompareView::Changes || note.files_shown))
                .then(|| files_toggle(format!("{} unchanged", note.unchanged_files)));
            let mut full = vec![text(format!(
                "Changed since {phrase}: {} {counts}{dropped}{rebase} ",
                plural(note.changed_files, "file")
            ))];
            full.push(view_toggle());
            if let Some(toggle) = unchanged {
                full.push(text(" "));
                full.push(toggle);
            }
            vec![
                full,
                vec![
                    text(format!(
                        "Since {phrase}: {} {counts} ",
                        plural(note.changed_files, "file")
                    )),
                    view_toggle(),
                ],
                vec![text(format!("Since: {counts}"))],
                vec![text(counts)],
            ]
        }
    }
}

/// Lay `parts` out after `indent` cells at `width`: the painted line and its clickable cells.
fn place(parts: &[Part], indent: usize, width: usize, theme: &ReviewTheme) -> ItemRows {
    let mut spans = vec![Span::raw(" ".repeat(indent))];
    let mut actions = Vec::new();
    let mut x = indent;
    for part in parts {
        let cells = display_width(&part.text);
        let style = if part.action.is_some() {
            theme.gap_action
        } else {
            theme.since.note
        };
        spans.push(Span::styled(part.text.clone(), style));
        if let Some(action) = part.action
            && x + cells <= width
        {
            actions.push(ActionCells {
                row: 0,
                cells: cell(x)..cell(x + cells),
                action: ItemAction::Compare(action),
            });
        }
        x += cells;
    }
    ItemRows::lines(vec![Line::from_spans(spans).truncate(width)], actions)
}

fn cell(x: usize) -> u16 {
    u16::try_from(x).unwrap_or(u16::MAX)
}

/// Pick the widest variant that fits, preferring the aligned indent; the narrowest is
/// truncated.
fn fitted(variants: &[Vec<Part>], indent: usize, width: usize, theme: &ReviewTheme) -> ItemRows {
    let fits = |parts: &[Part], indent: usize| {
        indent
            + parts
                .iter()
                .map(|part| display_width(&part.text))
                .sum::<usize>()
            <= width
    };
    let (parts, indent) = variants
        .iter()
        .find_map(|parts| {
            if fits(parts, indent) {
                Some((parts.as_slice(), indent))
            } else {
                fits(parts, 0).then_some((parts.as_slice(), 0))
            }
        })
        .unwrap_or_else(|| (variants.last().map_or(&[][..], Vec::as_slice), 0));
    place(parts, indent, width, theme)
}

/// The summary row under a compared source's header.
#[must_use]
pub fn note_rows(
    note: &CompareNoteItem,
    since: Since,
    theme: &ReviewTheme,
    width: usize,
    indent: usize,
) -> ItemRows {
    fitted(&note_variants(note, since, theme), indent, width, theme)
}

/// A removed-since marker: its label, then, expanded, the removed lines as removed diff rows.
#[must_use]
pub fn removed_rows(
    item: &RemovedSinceItem,
    since: Since,
    options: DiffRowOptions,
    theme: &ReviewTheme,
    width: u16,
    indent: usize,
) -> ItemRows {
    let chevron = if item.expanded {
        theme.icons.menu
    } else {
        theme.icons.collapsed
    };
    let count = item.lines.len();
    let lines = if count == 1 { "line" } else { "lines" };
    let variants = [
        vec![act(
            format!("{chevron} {count} {lines} removed since {}", since.phrase()),
            CompareAction::ToggleRemoval,
        )],
        vec![act(
            format!("{chevron} {count} removed"),
            CompareAction::ToggleRemoval,
        )],
        vec![act(
            format!("{chevron}{count}"),
            CompareAction::ToggleRemoval,
        )],
    ];
    let mut rows = fitted(&variants, indent, usize::from(width), theme);
    if item.expanded {
        for line in item.lines.iter() {
            let layout = layout_diff_row(
                DiffRowItem::Unified {
                    kind: DiffLineKind::Removed,
                    old_line: None,
                    new_line: None,
                    text: DiffRowText::plain(line),
                },
                width,
                options,
                &theme.since.diff,
            );
            rows.lines.extend(layout.rows().iter().cloned());
        }
        rows.height = rows.lines.len().max(1) as u64;
    }
    rows
}

/// Paint the gutter bar on `lines` (the rows of `layout`) where a side is new since the baseline.
///
/// The bar takes the leading blank cell of each marked column's gutter, which only the full
/// gutter has: narrower gutters drop the bar first and keep the row tint.
pub fn paint_since_bar(
    lines: &mut [Line],
    layout: &DiffRowLayout,
    marked: impl Fn(DiffSourceSide) -> bool,
    theme: &ReviewTheme,
) {
    let bar = theme.icons.since_bar;
    if display_width(bar) != 1 {
        return;
    }
    for column in layout.columns() {
        if !column.present || column.gutter != DiffGutterMode::Full || !marked(column.side) {
            continue;
        }
        for line in lines.iter_mut() {
            if let Some(painted) = overlay_blank(line, column.cells.start, bar, theme.since.bar) {
                *line = painted;
            }
        }
    }
}

/// `line` with the blank cell at `at` replaced by `glyph` (one cell wide), keeping the cell's
/// background; `None` when that cell is not a blank of a run of spaces.
fn overlay_blank(line: &Line, at: usize, glyph: &str, style: Style) -> Option<Line> {
    let mut x = 0;
    let mut spans = Vec::with_capacity(line.spans.len() + 2);
    let mut done = false;
    for span in &line.spans {
        let cells = display_width(&span.content);
        if !done && at >= x && at < x + cells {
            // Only a run of spaces, one cell per byte, is split.
            if !span.content.bytes().all(|byte| byte == b' ') {
                return None;
            }
            let offset = at - x;
            if offset > 0 {
                spans.push(Span::styled(" ".repeat(offset), span.style));
            }
            spans.push(Span::styled(glyph.to_owned(), span.style.patch(style)));
            if cells > offset + 1 {
                spans.push(Span::styled(" ".repeat(cells - offset - 1), span.style));
            }
            done = true;
        } else {
            spans.push(span.clone());
        }
        x += cells;
    }
    done.then(|| Line::from_spans(spans))
}

/// The badge a file header shows for how the file changed since the baseline.
#[must_use]
pub fn since_badge(since: FileSince, baseline: Since, theme: &ReviewTheme) -> Option<Span> {
    let minus = theme.icons.minus;
    let phrase = baseline.phrase();
    match since {
        FileSince::Changed { added, removed } => Some(Span::styled(
            format!("new since {phrase} +{added} {minus}{removed}"),
            theme.since.badge,
        )),
        FileSince::New { .. } => Some(Span::styled(
            format!("new since {phrase}"),
            theme.since.badge,
        )),
        FileSince::Unchanged => Some(Span::styled(
            format!("unchanged since {phrase}"),
            theme.badge_outdated,
        )),
        FileSince::Unknown => Some(Span::styled(theme.icons.ellipsis, theme.muted)),
        FileSince::Dropped => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{IconStyle, ReviewTheme};
    use bmux_tui_components::diff_viewer::{DiffGutterShape, DiffRowGutter};

    fn theme() -> ReviewTheme {
        ReviewTheme::derive(None, IconStyle::Unicode)
    }

    fn note(kind: Option<DeltaKind>) -> CompareNoteItem {
        CompareNoteItem {
            source_id: "s".into(),
            part: None,
            kind,
            view: CompareView::Full,
            changed_files: 3,
            unchanged_files: 18,
            dropped: Vec::new(),
            added: 40,
            removed: 12,
            files_shown: false,
        }
    }

    fn painted(rows: &ItemRows) -> String {
        rows.lines
            .iter()
            .map(Line::plain_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn summaries_fit_every_width_and_their_buttons_cover_what_is_painted() {
        let theme = theme();
        for kind in [
            None,
            Some(DeltaKind::Changed),
            Some(DeltaKind::RebasedOnly),
            Some(DeltaKind::WithRebase),
            Some(DeltaKind::Unavailable),
        ] {
            for width in [0, 1, 3, 8, 20, 40, 80, 200] {
                let rows = note_rows(&note(kind), Since::LastLooked, &theme, width, 4);
                for line in &rows.lines {
                    assert!(line.width() <= width, "{kind:?} at {width}");
                }
                let text = painted(&rows);
                for action in &rows.actions {
                    let start = usize::from(action.cells.start);
                    let end = usize::from(action.cells.end);
                    assert!(end <= width);
                    let label = rows.lines[0].viewport(start, end - start).plain_text();
                    assert!(label.starts_with('['), "{label:?} in {text:?}");
                    assert!(label.ends_with(']'), "{label:?} in {text:?}");
                }
            }
        }
        let wide = note_rows(
            &note(Some(DeltaKind::Changed)),
            Since::LastLooked,
            &theme,
            200,
            0,
        );
        assert!(painted(&wide).contains("3 files +40 −12"));
        assert_eq!(wide.actions.len(), 1);
    }

    #[test]
    fn removed_lines_expand_into_diff_rows_that_stay_in_width_with_wide_text() {
        let theme = theme();
        let options = DiffRowOptions::new(DiffRowGutter::for_max_line_number(
            999,
            DiffGutterShape::LineNumbersThenMarker,
        ));
        let item = RemovedSinceItem {
            file: std::sync::Arc::new(bcode_code_review::document::FileKey::new("s", None, "a")),
            before_new_line: 3,
            lines: vec!["漢字かな交じり文 👩‍👩‍👧 e\u{301}".to_owned(), String::new()].into(),
            expanded: true,
        };
        for width in [0_u16, 1, 2, 5, 12, 40] {
            let rows = removed_rows(&item, Since::LastLooked, options, &theme, width, 2);
            assert!(rows.height >= 3, "label and two lines at {width}");
            assert_eq!(rows.height, rows.lines.len() as u64);
            for line in &rows.lines {
                assert!(line.width() <= usize::from(width), "at {width}");
            }
        }
        let collapsed = RemovedSinceItem {
            expanded: false,
            ..item
        };
        let rows = removed_rows(&collapsed, Since::PreviousPush, options, &theme, 60, 2);
        assert_eq!(rows.height, 1);
        assert!(painted(&rows).contains("2 lines removed since the previous push"));
        assert_eq!(rows.actions[0].cells.start, 2);
    }

    #[test]
    fn the_bar_takes_the_full_gutters_blank_cell_and_is_dropped_on_narrow_rows() {
        let theme = theme();
        let options = DiffRowOptions::new(DiffRowGutter::for_max_line_number(
            99,
            DiffGutterShape::LineNumbersThenMarker,
        ));
        let item = DiffRowItem::Unified {
            kind: DiffLineKind::Added,
            old_line: None,
            new_line: Some(7),
            text: DiffRowText::plain("中文 text"),
        };
        for width in [0_u16, 1, 2, 4, 9, 10, 30] {
            let layout = layout_diff_row(item, width, options, &theme.since.diff);
            let mut lines = layout.rows().to_vec();
            paint_since_bar(&mut lines, &layout, |_| true, &theme);
            for (painted, original) in lines.iter().zip(layout.rows()) {
                assert_eq!(
                    painted.width(),
                    original.width(),
                    "geometry kept at {width}"
                );
            }
            let full = layout
                .columns()
                .iter()
                .any(|column| column.gutter == DiffGutterMode::Full);
            assert_eq!(
                lines[0].plain_text().starts_with(theme.icons.since_bar),
                full,
                "bar only with the full gutter at {width}"
            );
        }
    }
}
