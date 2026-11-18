//! Rows of a comparison's overlay: the summary under a compared source's header, markers of
//! lines removed since the baseline, the gutter bar of lines new since it, and file badges.
//!
//! Labels choose among variants by measured fit (BMUX display width), and clickable cells come
//! from the same parts as the painted variant. Removed lines are laid out by BMUX
//! `layout_diff_row`, like every other diff line.

use super::{ActionCells, ItemAction, ItemRows};
use crate::config::compare::{MarkOptions, MarkStyle, SeenHunks};
use crate::theme::ReviewTheme;
use bcode_code_review::document::{CompareNoteItem, CompareView, HunkHeaderItem, RemovedSinceItem};
use bcode_code_review_models::{DeltaKind, FileSince};
use bmux_tui::style::Style;
use bmux_tui::text::{Line, Span};
use bmux_tui::text_width::display_width;
use bmux_tui_components::diff_viewer::{
    ChangedRange, DiffGutterMode, DiffLineKind, DiffRowItem, DiffRowLayout, DiffRowOptions,
    DiffRowText, DiffSourceSide, DiffSyntaxRange, DiffViewerStyle, intraline_changed_ranges,
    layout_diff_row,
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
    /// Show a hunk folded because nothing in it is new since the baseline, or fold it again.
    ToggleHunk,
}

/// What a comparison's rows say the changes are since, as the baseline in effect names it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Since {
    /// What the reviewer last looked at.
    #[default]
    LastLooked,
    /// The version before the stack's latest push.
    PreviousPush,
    /// A point in time the comparison menu offers, by how long ago it is in milliseconds.
    Ago(u64),
    /// The stack after this push (`0`: before the first push), picked on the timeline.
    Push(u32),
    /// This version of the one pull request whose versions were picked.
    Version(u32),
    /// Another time, pushes, or versions picked.
    Then,
    /// The last commit (`HEAD`): uncommitted changes of a working-tree file.
    Head,
}

impl Since {
    /// The words after "since" (`you looked`, `the previous push`, `yesterday`, `push 3`,
    /// `v2`, `then`).
    #[must_use]
    pub fn phrase(self) -> String {
        match self {
            Self::LastLooked => "you looked".to_owned(),
            Self::PreviousPush => "the previous push".to_owned(),
            Self::Ago(ms) => crate::compare_menu::AGO_PRESETS
                .iter()
                .find(|(preset, _)| *preset == ms)
                .map_or_else(|| "then".to_owned(), |(_, label)| (*label).to_owned()),
            Self::Push(0) => "before push 1".to_owned(),
            Self::Push(push) => format!("push {push}"),
            Self::Version(version) => format!("v{version}"),
            Self::Then => "then".to_owned(),
            Self::Head => "HEAD".to_owned(),
        }
    }

    /// A number distinguishing the variants and what they name, for cache keys.
    #[must_use]
    pub const fn index(self) -> u64 {
        match self {
            Self::LastLooked => 0,
            Self::PreviousPush => 1,
            Self::Then => 2,
            Self::Ago(ms) => 3 | ms << 3,
            Self::Push(push) => 4 | (push as u64) << 3,
            Self::Version(version) => 5 | (version as u64) << 3,
            Self::Head => 6,
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
                    Since::LastLooked => "what you last looked at".to_owned(),
                    Since::PreviousPush => "the previous push".to_owned(),
                    Since::Ago(_) | Since::Push(_) | Since::Version(_) | Since::Head => {
                        since.phrase()
                    }
                    Since::Then => "the versions picked".to_owned(),
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
                _ => "The version compared with is gone: showing the full change",
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
///
/// Shown inline (`marks.removed_inline`), the removed lines stand alone, struck through and
/// faint, with the marks of lines changed since.
#[must_use]
pub fn removed_rows(
    item: &RemovedSinceItem,
    since: Since,
    marks: MarkOptions,
    options: DiffRowOptions,
    theme: &ReviewTheme,
    width: u16,
    indent: usize,
) -> ItemRows {
    if marks.removed_inline {
        return inline_removed_rows(item, marks, options, theme, width);
    }
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

/// The removed lines of `item` as removed diff rows, their content struck through and faint.
fn inline_removed_rows(
    item: &RemovedSinceItem,
    marks: MarkOptions,
    options: DiffRowOptions,
    theme: &ReviewTheme,
    width: u16,
) -> ItemRows {
    let mut rows = ItemRows::lines(Vec::new(), Vec::new());
    for line in item.lines.iter() {
        // The strike covers the text's own graphemes, never the padding out to the edge.
        let struck = [DiffSyntaxRange::new(
            0,
            line.len(),
            theme.since.removed_inline,
        )];
        let layout = layout_diff_row(
            DiffRowItem::Unified {
                kind: DiffLineKind::Removed,
                old_line: None,
                new_line: None,
                text: DiffRowText::plain(line).with_syntax(&struck),
            },
            width,
            options,
            mark_styles(marks, true, theme),
        );
        let mut lines = layout.rows().to_vec();
        paint_marks(&mut lines, &layout, |_| true, marks, theme);
        rows.lines.extend(lines);
    }
    rows.height = rows.lines.len().max(1) as u64;
    rows
}

/// The diff styles of a row with a side `marked` new since the baseline: tinted when the mark
/// style tints, more strongly in the strong style.
#[must_use]
pub const fn mark_styles(
    marks: MarkOptions,
    marked: bool,
    theme: &ReviewTheme,
) -> &DiffViewerStyle {
    if !marked || !marks.style.tint() {
        return &theme.diff;
    }
    match marks.style {
        MarkStyle::Strong => &theme.since.strong_diff,
        _ => &theme.since.diff,
    }
}

/// The label the strong mark style paints in the gutter of a line new since the baseline.
pub const NEW_LABEL: &str = "new";

/// Paint the marks of `marks.style` on `lines` (the rows of `layout`) where a side is new since
/// the baseline.
///
/// That is the gutter bar, and in the strong style the accent bar and a `new` label on
/// the first row where the gutter has a blank run for it.
pub fn paint_marks(
    lines: &mut [Line],
    layout: &DiffRowLayout,
    marked: impl Fn(DiffSourceSide) -> bool + Copy,
    marks: MarkOptions,
    theme: &ReviewTheme,
) {
    if !marks.style.bar() {
        return;
    }
    let strong = marks.style == MarkStyle::Strong;
    let bar = if strong {
        theme.since.strong_bar
    } else {
        theme.since.bar
    };
    paint_bar(lines, layout, marked, bar, theme);
    if strong && let Some(first) = lines.first_mut() {
        for column in layout.columns() {
            if column.present
                && column.gutter == DiffGutterMode::Full
                && marked(column.side)
                && let Some(painted) =
                    label_in_gutter(first, column.cells.start + 1..column.content.start, theme)
            {
                *first = painted;
            }
        }
    }
}

/// `line` with [`NEW_LABEL`] in the first blank run of `gutter` cells that holds it and one
/// blank after it (so it never touches a line number); `None` when no run does.
fn label_in_gutter(
    line: &Line,
    gutter: std::ops::Range<usize>,
    theme: &ReviewTheme,
) -> Option<Line> {
    let label = NEW_LABEL;
    let cells = display_width(label);
    let blank = |at: usize| line.viewport(at, 1).plain_text() == " ";
    let mut run_start = None;
    let mut found = None;
    for at in gutter.start..=gutter.end {
        if at < gutter.end && blank(at) {
            run_start.get_or_insert(at);
            continue;
        }
        if let Some(start) = run_start.take()
            && at - start > cells
        {
            found = Some(at - 1 - cells);
            break;
        }
    }
    let at = found?;
    let width = line.width();
    let under = line
        .viewport(at, cells)
        .spans
        .first()
        .map_or_else(Style::new, |span| span.style);
    let mut spans = line.viewport(0, at).spans;
    spans.push(Span::styled(label, under.patch(theme.since.strong_bar)));
    spans.extend(line.viewport(at + cells, width - at - cells).spans);
    Some(Line::from_spans(spans))
}

/// The ranges of `text` that changed since `was`, the baseline line it replaced.
///
/// None when the lines share nothing (the line mark says it all) or are too long to compare (BMUX bounds
/// intraline work per line).
#[must_use]
pub fn word_ranges(was: &str, text: &str) -> Vec<ChangedRange> {
    let Some(changes) = intraline_changed_ranges(was, text) else {
        return Vec::new();
    };
    let start = text.len() - text.trim_start().len();
    let end = text.trim_end().len();
    match changes.new.as_slice() {
        [whole] if whole.start <= start && whole.end >= end => Vec::new(),
        _ => changes.new,
    }
}

/// `syntax` with `style` patched over the bytes of `words`, as sorted, disjoint ranges.
#[must_use]
pub fn with_word_marks(
    syntax: &[DiffSyntaxRange],
    words: &[ChangedRange],
    style: Style,
) -> Vec<DiffSyntaxRange> {
    if words.is_empty() {
        return syntax.to_vec();
    }
    let mut cuts: Vec<usize> = syntax
        .iter()
        .flat_map(|range| [range.start, range.end])
        .chain(words.iter().flat_map(|range| [range.start, range.end]))
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut ranges: Vec<DiffSyntaxRange> = Vec::with_capacity(cuts.len());
    for pair in cuts.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        let syntax_style = syntax
            .get(syntax.partition_point(|range| range.end <= start))
            .filter(|range| range.start <= start)
            .map(|range| range.style);
        let word = words
            .get(words.partition_point(|range| range.end <= start))
            .is_some_and(|range| range.start <= start);
        let style = match (syntax_style, word) {
            (Some(base), true) => base.patch(style),
            (None, true) => style,
            (Some(base), false) => base,
            (None, false) => continue,
        };
        match ranges.last_mut() {
            Some(last) if last.end == start && last.style == style => last.end = end,
            _ => ranges.push(DiffSyntaxRange::new(start, end, style)),
        }
    }
    ranges
}

/// The label a hunk header carries for what in the hunk is new since the baseline.
///
/// Its variants come widest first, with the action a click on it takes: the count of lines new since,
/// or, where hunks seen before fold, a button to show or fold one.
#[must_use]
pub fn hunk_label(
    item: &HunkHeaderItem,
    since: Since,
    marks: MarkOptions,
    theme: &ReviewTheme,
) -> Option<(Vec<String>, Option<CompareAction>)> {
    let phrase = since.phrase();
    let separator = theme.icons.separator;
    match item.since_lines? {
        // A comment, a draft, or the composer in the hunk keeps it open: nothing to click.
        0 if marks.seen == SeenHunks::Fold && item.held_open => Some((
            vec![
                format!("nothing new since {phrase}{separator}kept open"),
                "nothing new".to_owned(),
            ],
            None,
        )),
        0 if marks.seen == SeenHunks::Fold && item.folded => {
            let lines = plural(item.rows as usize, "line");
            Some((
                vec![
                    format!("[nothing new since {phrase}{separator}show {lines}]"),
                    format!("[show {lines}]"),
                    format!("[{}]", theme.icons.ellipsis),
                ],
                Some(CompareAction::ToggleHunk),
            ))
        }
        0 if marks.seen == SeenHunks::Fold => Some((
            vec![
                format!("[nothing new since {phrase}{separator}fold]"),
                "[fold]".to_owned(),
            ],
            Some(CompareAction::ToggleHunk),
        )),
        0 => None,
        count => {
            let lines = plural(count as usize, "line");
            Some((
                vec![
                    format!("new since {phrase}{separator}{lines}"),
                    format!("new{separator}{count}"),
                    format!("+{count}"),
                ],
                None,
            ))
        }
    }
}

/// Paint the gutter bar on `lines` (the rows of `layout`) where a side is new since the baseline.
///
/// The bar takes the leading blank cell of each marked column's gutter, which only the full
/// gutter has: narrower gutters drop the bar first and keep the row tint.
fn paint_bar(
    lines: &mut [Line],
    layout: &DiffRowLayout,
    marked: impl Fn(DiffSourceSide) -> bool,
    style: Style,
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
            if let Some(painted) = overlay_blank(line, column.cells.start, bar, style) {
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
        FileSince::ReplayConflict => Some(Span::styled(
            "replay conflict · showing full PR diff",
            theme.since.badge,
        )),
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
            uncommitted: false,
        };
        for width in [0_u16, 1, 2, 5, 12, 40] {
            let rows = removed_rows(
                &item,
                Since::LastLooked,
                MarkOptions::default(),
                options,
                &theme,
                width,
                2,
            );
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
        let rows = removed_rows(
            &collapsed,
            Since::PreviousPush,
            MarkOptions::default(),
            options,
            &theme,
            60,
            2,
        );
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
            paint_marks(
                &mut lines,
                &layout,
                |_| true,
                MarkOptions::default(),
                &theme,
            );
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

    /// Text with wide, combined, and emoji graphemes.
    const MIXED: &str = "漢字 e\u{301} 👩\u{200d}👩\u{200d}👧 text";

    fn added(text: &str, line: u32) -> DiffRowItem<'_> {
        DiffRowItem::Unified {
            kind: DiffLineKind::Added,
            old_line: None,
            new_line: Some(line),
            text: DiffRowText::plain(text),
        }
    }

    fn marks(style: MarkStyle) -> MarkOptions {
        MarkOptions {
            style,
            ..MarkOptions::default()
        }
    }

    #[test]
    fn each_mark_style_paints_as_named_and_keeps_every_rows_geometry() {
        let theme = theme();
        for digits in [1_usize, 2, 3, 5] {
            let options = DiffRowOptions::new(DiffRowGutter::new(
                digits,
                DiffGutterShape::LineNumbersThenMarker,
            ));
            for style in MarkStyle::ALL {
                for width in [0_u16, 1, 2, 3, 6, 12, 20, 40, 90] {
                    let item = added(MIXED, 7);
                    let styles = mark_styles(marks(style), true, &theme);
                    let layout = layout_diff_row(item, width, options, styles);
                    let mut lines = layout.rows().to_vec();
                    paint_marks(&mut lines, &layout, |_| true, marks(style), &theme);
                    for (painted, original) in lines.iter().zip(layout.rows()) {
                        assert_eq!(painted.width(), original.width(), "{style:?} at {width}");
                        assert!(painted.width() <= usize::from(width));
                    }
                    let full = layout
                        .columns()
                        .iter()
                        .any(|column| column.gutter == DiffGutterMode::Full);
                    let first = lines.first().map(Line::plain_text).unwrap_or_default();
                    assert_eq!(
                        first.starts_with(theme.icons.since_bar),
                        full && style.bar(),
                        "{style:?} bar at {width}: {first:?}"
                    );
                    // The tint is the row's: the strong style's deeper than the default.
                    let tinted = *styles != theme.diff;
                    assert_eq!(tinted, style.tint(), "{style:?}");
                    // The label shows only where the gutter has a blank run for it, and the
                    // line number is never covered.
                    let label = first.contains(NEW_LABEL);
                    if label {
                        assert_eq!(style, MarkStyle::Strong);
                        assert!(first.contains(&format!("{NEW_LABEL} ")), "{first:?}");
                        assert!(first.contains('7'), "the number stays: {first:?}");
                    }
                    if style == MarkStyle::Strong && full && digits >= 3 {
                        assert!(label, "{digits} digits at {width}: {first:?}");
                    }
                }
            }
        }
        assert_ne!(theme.since.strong_diff, theme.since.diff);
        assert_ne!(theme.since.strong_bar, theme.since.bar);
    }

    #[test]
    fn side_by_side_rows_mark_only_the_new_side() {
        let theme = theme();
        let options = DiffRowOptions::new(DiffRowGutter::new(
            3,
            DiffGutterShape::LineNumbersThenMarker,
        ));
        let item = DiffRowItem::SideBySide {
            old: Some(bmux_tui_components::diff_viewer::DiffRowCell::new(
                DiffLineKind::Removed,
                Some(4),
                DiffRowText::plain(MIXED),
            )),
            new: Some(bmux_tui_components::diff_viewer::DiffRowCell::new(
                DiffLineKind::Added,
                Some(4),
                DiffRowText::plain(MIXED),
            )),
        };
        for width in [0_u16, 3, 9, 30, 80] {
            let layout = layout_diff_row(item, width, options, &theme.since.diff);
            let mut lines = layout.rows().to_vec();
            paint_marks(
                &mut lines,
                &layout,
                |side| side == DiffSourceSide::New,
                marks(MarkStyle::Strong),
                &theme,
            );
            for (painted, original) in lines.iter().zip(layout.rows()) {
                assert_eq!(painted.width(), original.width());
            }
            for column in layout.columns() {
                if column.gutter != DiffGutterMode::Full {
                    continue;
                }
                let cell = lines[0].viewport(column.cells.start, 1).plain_text();
                assert_eq!(
                    cell == theme.icons.since_bar,
                    column.side == DiffSourceSide::New,
                    "{column:?} at {width}"
                );
            }
        }
    }

    #[test]
    fn changed_words_are_only_what_differs_from_the_baseline_line() {
        let was = "let total = a + b;";
        let text = "let total = a + b + c;";
        let words = word_ranges(was, text);
        assert_ne!(words.len(), 0);
        let marked: String = words
            .iter()
            .map(|range| &text[range.start..range.end])
            .collect();
        assert!(marked.contains('c'), "{marked:?}");
        assert!(!marked.contains("total"), "{marked:?}");
        // A wide grapheme changed alone is marked whole.
        let words = word_ranges("名前 = 1", "名称 = 1");
        assert!(
            words
                .iter()
                .all(|range| "名称 = 1".is_char_boundary(range.start))
        );
        assert_ne!(words.len(), 0);
        // A line rewritten whole has no words in common: the line mark says it all.
        assert_eq!(word_ranges("alpha beta", "gamma delta"), []);
        assert_eq!(word_ranges("same", "same"), []);
        // Huge or minified lines are left to the line mark: bounded work per line.
        let huge = "x ".repeat(5_000);
        let started = std::time::Instant::now();
        assert_eq!(word_ranges(&huge, &format!("{huge}y")), []);
        let minified = "a,".repeat(20_000);
        assert_eq!(
            word_ranges(&minified, &minified.replacen("a,", "b,", 1)),
            []
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn word_marks_compose_with_syntax_and_intraline_emphasis_inside_the_row() {
        let theme = theme();
        let keyword = Style::new().fg(bmux_tui::style::Color::Red);
        let text = "fn 名前(x: e\u{301}) -> 👩\u{200d}👩\u{200d}👧 {}";
        let was = "fn 名前(y: e\u{301}) -> 👩\u{200d}👩\u{200d}👧 {}";
        let syntax = [DiffSyntaxRange::new(0, 2, keyword)];
        let words = word_ranges(was, text);
        let composed = with_word_marks(&syntax, &words, theme.since.word);
        // Sorted, disjoint, and the keyword keeps its color outside the changed words.
        for pair in composed.windows(2) {
            assert!(pair[0].end <= pair[1].start, "{composed:?}");
        }
        assert_eq!(composed[0], DiffSyntaxRange::new(0, 2, keyword));
        let x = text.find('x').expect("x");
        assert!(
            composed
                .iter()
                .any(|range| range.start <= x && x < range.end && range.style == theme.since.word)
        );
        let emphasis = intraline_changed_ranges(was, text).expect("short").new;
        let options = DiffRowOptions::new(DiffRowGutter::new(
            3,
            DiffGutterShape::LineNumbersThenMarker,
        ));
        for width in [0_u16, 1, 4, 11, 16, 24, 60] {
            let layout = layout_diff_row(
                DiffRowItem::Unified {
                    kind: DiffLineKind::Added,
                    old_line: None,
                    new_line: Some(12),
                    text: DiffRowText::plain(text)
                        .with_changed_ranges(&emphasis)
                        .with_syntax(&composed),
                },
                width,
                options,
                &theme.since.diff,
            );
            let plain = layout_diff_row(added(text, 12), width, options, &theme.since.diff);
            assert_eq!(
                layout.rows().len(),
                plain.rows().len(),
                "same rows at {width}"
            );
            for (styled, unstyled) in layout.rows().iter().zip(plain.rows()) {
                assert_eq!(
                    styled.plain_text(),
                    unstyled.plain_text(),
                    "same text at {width}"
                );
                assert!(styled.width() <= usize::from(width));
            }
        }
        // The word style survives the emphasis patched over it: its underline stays.
        assert!(
            theme
                .since
                .word
                .patch(theme.since.diff.added_emphasis)
                .modifiers
                .contains(bmux_tui::style::Modifier::UNDERLINE)
        );
        assert_eq!(with_word_marks(&syntax, &[], theme.since.word), syntax);
    }

    fn hunk(since_lines: Option<u32>, folded: bool) -> HunkHeaderItem {
        HunkHeaderItem {
            file: std::sync::Arc::new(bcode_code_review::document::FileKey::new("s", None, "a")),
            old_start: 10,
            old_count: 6,
            new_start: 10,
            new_count: 7,
            heading: Some("fn main() 漢字".into()),
            layers: vec![3],
            since_lines,
            folded,
            held_open: false,
            rows: 8,
        }
    }

    #[test]
    fn hunk_labels_name_the_baseline_in_effect_and_count_what_is_new() {
        let theme = theme();
        let label = |item: &HunkHeaderItem, since: Since, seen: SeenHunks| {
            let options = MarkOptions {
                seen,
                ..MarkOptions::default()
            };
            hunk_label(item, since, options, &theme)
        };
        for (since, words) in [
            (Since::LastLooked, "new since you looked · 3 lines"),
            (Since::PreviousPush, "new since the previous push · 3 lines"),
            (Since::Push(3), "new since push 3 · 3 lines"),
            (Since::Version(2), "new since v2 · 3 lines"),
            (Since::Ago(24 * 3_600_000), "new since yesterday · 3 lines"),
            (Since::Then, "new since then · 3 lines"),
        ] {
            let (variants, action) =
                label(&hunk(Some(3), false), since, SeenHunks::Show).expect("a label");
            assert_eq!(variants[0], words);
            assert_eq!(action, None);
        }
        let (variants, _) =
            label(&hunk(Some(1), false), Since::LastLooked, SeenHunks::Show).expect("label");
        assert_eq!(variants[0], "new since you looked · 1 line");
        // Nothing marked, or nothing new without folding: no label.
        assert_eq!(
            label(&hunk(None, false), Since::LastLooked, SeenHunks::Fold),
            None
        );
        assert_eq!(
            label(&hunk(Some(0), false), Since::LastLooked, SeenHunks::Show),
            None
        );
        assert_eq!(
            label(&hunk(Some(0), false), Since::LastLooked, SeenHunks::Dim),
            None
        );
        // Folded hunks are a button to show them; shown again, a button to fold.
        let (variants, action) =
            label(&hunk(Some(0), true), Since::Push(2), SeenHunks::Fold).expect("label");
        assert_eq!(variants[0], "[nothing new since push 2 · show 8 lines]");
        assert_eq!(action, Some(CompareAction::ToggleHunk));
        let (variants, _) =
            label(&hunk(Some(0), false), Since::Push(2), SeenHunks::Fold).expect("label");
        assert_eq!(variants[0], "[nothing new since push 2 · fold]");
        // Held open by something anchored in it: said so, with no button that would do nothing.
        let held = HunkHeaderItem {
            held_open: true,
            ..hunk(Some(0), false)
        };
        let (variants, action) = label(&held, Since::LastLooked, SeenHunks::Fold).expect("label");
        assert_eq!(variants[0], "nothing new since you looked · kept open");
        assert_eq!(action, None);
        assert_eq!(label(&held, Since::LastLooked, SeenHunks::Show), None);
    }

    #[test]
    fn hunk_headers_fit_every_width_and_their_buttons_cover_the_painted_label() {
        let theme = theme();
        let held = HunkHeaderItem {
            held_open: true,
            ..hunk(Some(0), false)
        };
        for (item, seen) in [
            (hunk(Some(0), true), SeenHunks::Fold),
            (hunk(Some(0), false), SeenHunks::Fold),
            (held, SeenHunks::Fold),
            (hunk(Some(4), false), SeenHunks::Show),
            (hunk(Some(0), false), SeenHunks::Dim),
        ] {
            let options = MarkOptions {
                seen,
                ..MarkOptions::default()
            };
            for width in [0_usize, 1, 5, 12, 20, 33, 50, 80, 140] {
                let rows = super::super::hunk_header(
                    &item,
                    (Since::PreviousPush, options),
                    &theme,
                    width,
                    4,
                );
                assert_eq!(rows.lines.len(), 1);
                assert!(rows.lines[0].width() <= width, "{seen:?} at {width}");
                for action in &rows.actions {
                    let (start, end) = (
                        usize::from(action.cells.start),
                        usize::from(action.cells.end),
                    );
                    assert!(end <= width);
                    let label = rows.lines[0].viewport(start, end - start).plain_text();
                    assert!(label.starts_with('[') && label.ends_with(']'), "{label:?}");
                    assert_eq!(
                        action.action,
                        ItemAction::Compare(CompareAction::ToggleHunk)
                    );
                }
                if width >= 80 {
                    let text = painted(&rows);
                    match seen {
                        SeenHunks::Fold => {
                            assert_eq!(
                                rows.actions.len(),
                                usize::from(!item.held_open),
                                "{text:?}"
                            );
                        }
                        SeenHunks::Show => {
                            assert!(text.contains("new since the previous push · 4 lines"));
                        }
                        SeenHunks::Dim => assert!(
                            rows.lines[0].spans.iter().all(|span| span
                                .style
                                .modifiers
                                .contains(bmux_tui::style::Modifier::DIM)),
                            "dimmed: {text:?}"
                        ),
                    }
                }
            }
        }
    }

    #[test]
    fn removed_code_shows_inline_struck_through_or_behind_its_marker() {
        let theme = theme();
        let options = DiffRowOptions::new(DiffRowGutter::new(
            3,
            DiffGutterShape::LineNumbersThenMarker,
        ));
        let item = RemovedSinceItem {
            file: std::sync::Arc::new(bcode_code_review::document::FileKey::new("s", None, "a")),
            before_new_line: 3,
            lines: vec![MIXED.to_owned(), "let old = 1;".to_owned()].into(),
            expanded: false,
            uncommitted: false,
        };
        let inline = MarkOptions {
            removed_inline: true,
            ..MarkOptions::default()
        };
        for width in [0_u16, 1, 2, 7, 15, 40, 100] {
            let rows = removed_rows(&item, Since::LastLooked, inline, options, &theme, width, 2);
            assert!(rows.actions.is_empty(), "no marker to click");
            assert!(rows.height >= 2, "both lines at {width}");
            assert_eq!(rows.height, rows.lines.len() as u64);
            for line in &rows.lines {
                assert!(line.width() <= usize::from(width), "at {width}");
            }
            assert!(!painted(&rows).contains("removed since"));
        }
        let rows = removed_rows(&item, Since::LastLooked, inline, options, &theme, 60, 2);
        let text = painted(&rows);
        assert!(text.contains("let old = 1;"), "{text:?}");
        let struck = rows.lines[1]
            .spans
            .iter()
            .filter(|span| span.content.contains("old"))
            .all(|span| {
                span.style
                    .modifiers
                    .contains(bmux_tui::style::Modifier::CROSSED_OUT)
            });
        assert!(struck, "{:?}", rows.lines[1]);
        // Only the text's own cells are struck through: never the padding out to the edge,
        // including on rows a wrapped line continues on (CJK, emoji, combining marks).
        for width in [12_u16, 15, 40, 60] {
            let rows = removed_rows(&item, Since::LastLooked, inline, options, &theme, width, 2);
            let struck: usize = rows
                .lines
                .iter()
                .flat_map(|line| &line.spans)
                .filter(|span| {
                    span.style
                        .modifiers
                        .contains(bmux_tui::style::Modifier::CROSSED_OUT)
                })
                .map(|span| display_width(&span.content))
                .sum();
            let text = display_width(MIXED) + display_width("let old = 1;");
            assert!(struck <= text, "{struck} struck cells at {width}");
            if width == 60 {
                assert_eq!(struck, text, "every cell of the text at {width}");
            }
        }
        // The gutter is not struck through.
        assert!(
            !rows.lines[1].spans[0]
                .style
                .modifiers
                .contains(bmux_tui::style::Modifier::CROSSED_OUT)
        );
        // Off, the marker comes back, folded.
        let rows = removed_rows(
            &item,
            Since::LastLooked,
            MarkOptions::default(),
            options,
            &theme,
            60,
            2,
        );
        assert_eq!(rows.height, 1);
        assert!(painted(&rows).contains("2 lines removed since you looked"));
    }
}
