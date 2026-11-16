//! A file's chrome: its header, the details under it, and the break after it. See
//! `docs/review-file-separation.md`.
//!
//! * **Header** (sticky): the status as a pill, the directory muted and the name bold, flags,
//!   layers, threads, `+/−`, the change bar, and a clickable viewed mark, on the file-header
//!   fill (the accent mixed into the canvas, unlike the header bar). Spacious screens give it
//!   a second row: the directory and a rename on their own, and the position among the files.
//! * **Details**: the file's facts (rename, language, lines, hunks, flags, layers, threads) and
//!   its buttons (Comment, Ask, Whole file, Collapse, Through the stack, Copy path; previous and
//!   next file when spacious).
//! * **End**: the break before the next file (see `docs/review-separators-and-folding.md`).
//!   Cards: the page's straight bottom edge over the desk and, with room, a lip raising the
//!   next header. Or a rule labeled with the next file's position, or blank rows. Between two
//!   folded files the break is one light row, so folded headers read as a list.

use super::{ActionCells, ItemAction, ItemRows, change_bar, count_spans, header_line};
use crate::theme::{ReviewTheme, status_letter};
use bcode_code_review::document::{FileDetailsItem, FileEndItem, FileHeaderItem};
use bcode_code_review_models::ReviewIndexFlag;
use bmux_tui::style::Style;
use bmux_tui::text::{Line, Span};
use bmux_tui::text_width::display_width;

/// How much of a file's chrome shows, by the review pane's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, PartialOrd, Ord)]
pub enum Density {
    /// One header row and a one-row break.
    #[default]
    Compact,
    /// A header row, a details row, and a two-row break.
    Comfortable,
    /// Two header rows, up to two details rows, and a three-row break.
    Spacious,
}

impl Density {
    /// Rows of a file header.
    #[must_use]
    pub const fn header_rows(self) -> u64 {
        match self {
            Self::Compact | Self::Comfortable => 1,
            Self::Spacious => 2,
        }
    }

    /// Rows of the break after a file.
    #[must_use]
    pub const fn end_rows(self) -> u64 {
        match self {
            Self::Compact => 1,
            Self::Comfortable => 2,
            Self::Spacious => 3,
        }
    }

    /// The density for a review pane of `width` x `height` cells, given the one shown now
    /// (`current`): bounds move by a few cells in the current density's favor, so a small
    /// resize does not make headers jump.
    #[must_use]
    pub fn for_size(width: u16, height: u16, current: Self) -> Self {
        // Entering a bigger density needs the full size; leaving it, a little less.
        let slack = |density: Self| -> u16 { if density as u8 <= current as u8 { 2 } else { 0 } };
        if width + slack(Self::Spacious) >= 140 && height + slack(Self::Spacious) >= 45 {
            Self::Spacious
        } else if width + slack(Self::Comfortable) >= 90 && height + slack(Self::Comfortable) >= 24
        {
            Self::Comfortable
        } else {
            Self::Compact
        }
    }
}

/// How files are separated, as drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Separation {
    /// The kind of break.
    pub style: SeparatorStyle,
    /// Rows of a break; `None` follows the density.
    pub rows: Option<u8>,
    /// A rule names the next file's position.
    pub label: bool,
    /// A card's edges keep a column of desk at each side.
    pub inset: bool,
}

/// The kind of break between files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SeparatorStyle {
    /// Files as cards on the desk: straight edges, needs a desk color.
    Card,
    /// A rule.
    #[default]
    Rule,
    /// Blank rows, on the desk when `desk`.
    Gap {
        /// The rows are desk colored.
        desk: bool,
    },
}

impl Separation {
    /// Rows of a break at `density`; a tight one (between folded files) is one row.
    #[must_use]
    pub fn height(self, density: Density, tight: bool) -> u64 {
        if tight {
            1
        } else {
            self.rows
                .map_or_else(|| density.end_rows(), |rows| u64::from(rows.clamp(1, 3)))
        }
    }
}

/// An action of a file's chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    /// Mark viewed or unviewed.
    Viewed,
    /// Comment on the file.
    Comment,
    /// Ask Bcode about the file.
    Ask,
    /// Reveal the whole file.
    WholeFile,
    /// Show only the header, or the content again.
    Collapse,
    /// The file through the stack's layers.
    Stack,
    /// Copy the path.
    CopyPath,
    /// The previous file.
    Previous,
    /// The next file.
    Next,
    /// The file's previous version (in a comparison).
    VersionPrevious,
    /// The file's next version (in a comparison).
    VersionNext,
}

impl FileAction {
    /// The review action that runs it.
    #[must_use]
    pub const fn action(self) -> &'static str {
        match self {
            Self::Viewed => "viewed",
            Self::Comment => "file_comment",
            Self::Ask => "ask_file",
            Self::WholeFile => "expand_file",
            Self::Collapse => "collapse_file",
            Self::Stack => "stack_file",
            Self::CopyPath => "copy_path",
            Self::Previous => "previous_file",
            Self::Next => "next_file",
            Self::VersionPrevious => "file_version_previous",
            Self::VersionNext => "file_version_next",
        }
    }
}

fn cells(x: usize) -> u16 {
    u16::try_from(x).unwrap_or(u16::MAX)
}

/// The directory (with its trailing slash) and the name of `path`.
fn split_path(path: &str) -> (&str, &str) {
    path.rfind('/')
        .map_or(("", path), |slash| (&path[..=slash], &path[slash + 1..]))
}

/// The viewed mark as a button: `○`/`✓`, plus `Viewed` when `label`.
fn viewed_span(viewed: bool, label: bool, theme: &ReviewTheme) -> Span {
    let (mark, style) = if viewed {
        (theme.icons.viewed, theme.viewed)
    } else {
        (theme.icons.unviewed, theme.muted)
    };
    Span::styled(
        if label {
            format!("{mark} Viewed")
        } else {
            mark.to_owned()
        },
        style,
    )
}

/// A file's versions as header spans, with the cells of its two steps from their end.
type VersionSteps = (Vec<Span>, [(usize, FileAction); 2]);

/// Cells a header's path keeps before its file's versions are left out.
const PATH_KEEP: usize = 24;

/// A file's versions in its pull request, at the widest variant that leaves the path
/// [`PATH_KEEP`] cells of `width` beside `others` cells of facts: `changed in 2 of 4 pushes ·
/// v4 ◂ ▸`, then `v4 ◂ ▸`, else nothing. Returns the spans (each ending with a space) and the
/// cells of the two steps, counted from the end of the spans.
fn version_spans(
    item: &FileHeaderItem,
    theme: &ReviewTheme,
    width: usize,
    others: usize,
) -> Option<VersionSteps> {
    let versions = item.versions?;
    let shown = versions.version;
    let step = |enabled: bool, glyph: &'static str| {
        Span::styled(glyph, if enabled { theme.accent } else { theme.muted })
    };
    let tail = vec![
        Span::styled(format!("v{shown} "), theme.badge_open),
        step(versions.previous, theme.icons.step_back),
        Span::raw(" "),
        step(versions.next, theme.icons.step_forward),
        Span::raw(" "),
    ];
    let pushes = format!(
        "changed in {} of {} push{}{}",
        versions.changed_in,
        versions.steps,
        if versions.steps == 1 { "" } else { "es" },
        theme.icons.separator
    );
    let mut full = vec![Span::styled(pushes, theme.muted)];
    full.extend(tail.clone());
    let back = display_width(theme.icons.step_back);
    let forward = display_width(theme.icons.step_forward);
    // From the end: `▸` then a space; `◂` then a space, `▸`, a space.
    let cells = [
        (forward + 1 + back + 1, FileAction::VersionPrevious),
        (forward + 1, FileAction::VersionNext),
    ];
    for spans in [full, tail] {
        let needed: usize = spans.iter().map(|span| display_width(&span.content)).sum();
        if others + needed + PATH_KEEP <= width {
            return Some((spans, cells));
        }
    }
    None
}

/// What a header says about a file's changes since `HEAD`: renamed, binary, and line counts
/// (left out when a renamed or binary file has none).
fn uncommitted_text(uncommitted: bcode_code_review::document::Uncommitted, minus: &str) -> String {
    let mut text = "uncommitted".to_owned();
    if uncommitted.renamed {
        text.push_str(" renamed");
    }
    if uncommitted.binary {
        text.push_str(" binary");
    }
    let lines = uncommitted.additions > 0 || uncommitted.deletions > 0;
    if lines || !(uncommitted.renamed || uncommitted.binary) {
        text.push_str(&format!(
            " +{} {minus}{}",
            uncommitted.additions, uncommitted.deletions
        ));
    }
    text
}

/// Spans of what a header says on the right: flags, layers, threads, counts, change bar.
fn header_facts(
    item: &FileHeaderItem,
    baseline: super::compare::Since,
    theme: &ReviewTheme,
    counts: bool,
) -> Vec<Span> {
    let mut trailing = Vec::new();
    if let Some(badge) = item
        .since
        .and_then(|since| super::compare::since_badge(since, baseline, theme))
    {
        trailing.push(badge);
        trailing.push(Span::raw(" "));
    }
    if let Some(uncommitted) = item.uncommitted {
        trailing.push(Span::styled(
            uncommitted_text(uncommitted, theme.icons.minus),
            theme.status_modified,
        ));
        trailing.push(Span::raw(" "));
    }
    for (flag, label, style) in [
        (ReviewIndexFlag::Binary, "binary", theme.status_binary),
        (ReviewIndexFlag::ModeChanged, "mode", theme.muted),
        (ReviewIndexFlag::Symlink, "symlink", theme.muted),
        (ReviewIndexFlag::Submodule, "submodule", theme.muted),
        (ReviewIndexFlag::Untracked, "untracked", theme.status_added),
        (ReviewIndexFlag::Ignored, "ignored", theme.muted),
    ] {
        if item.flags.contains(&flag) {
            trailing.push(Span::styled(label, style));
            trailing.push(Span::raw(" "));
        }
    }
    if !item.layers.is_empty() {
        let layers: Vec<String> = item
            .layers
            .iter()
            .map(|layer| format!("#{layer}"))
            .collect();
        trailing.push(Span::styled(layers.join(" "), theme.badge_open));
        trailing.push(Span::raw(" "));
    }
    if item.thread_count > 0 {
        trailing.push(Span::styled(
            format!("{}{}", theme.icons.thread, item.thread_count),
            theme.badge_open,
        ));
        trailing.push(Span::raw(" "));
    }
    if counts {
        let (additions, deletions) = (u64::from(item.additions), u64::from(item.deletions));
        trailing.extend(count_spans(additions, deletions, theme));
        trailing.push(Span::raw(" "));
        trailing.extend(change_bar(additions, deletions, theme));
        trailing.push(Span::raw(" "));
    }
    trailing
}

/// The lead of a header row: the accent bar in the status color, the fold chevron (on the
/// first row; blank on others), then `rest`.
fn lead(item: &FileHeaderItem, theme: &ReviewTheme, chevron: bool, rest: Vec<Span>) -> Vec<Span> {
    let bar = theme
        .status(item.status)
        .fg
        .map_or(theme.accent, |fg| Style::new().fg(fg));
    let fold = if !chevron {
        " ".repeat(display_width(theme.icons.menu))
    } else if item.collapsed {
        theme.icons.collapsed.to_owned()
    } else {
        theme.icons.menu.to_owned()
    };
    let mut spans = vec![
        Span::styled(theme.icons.accent_bar, bar),
        Span::raw(" "),
        Span::styled(fold, theme.muted),
        Span::raw(" "),
    ];
    spans.extend(rest);
    spans
}

/// The fold chevron's cells on a header's first row.
fn chevron_action(theme: &ReviewTheme) -> ActionCells<ItemAction> {
    let start = display_width(theme.icons.accent_bar) + 1;
    ActionCells {
        row: 0,
        cells: cells(start)..cells(start + display_width(theme.icons.menu)),
        action: ItemAction::File(FileAction::Collapse),
    }
}

/// The status pill, ` M `.
fn pill(item: &FileHeaderItem, theme: &ReviewTheme) -> Span {
    Span::styled(
        format!(" {} ", status_letter(item.status)),
        theme.status_pill(item.status),
    )
}

/// The path as directory (muted) and name (bold), with a rename's old path before it.
fn path_spans(item: &FileHeaderItem, theme: &ReviewTheme, with_dir: bool) -> Vec<Span> {
    let muted = item.viewed || item.since == Some(bcode_code_review_models::FileSince::Unchanged);
    let mut spans = Vec::new();
    if with_dir && let Some(old) = &item.old_path {
        spans.push(Span::styled(old.clone(), theme.muted));
        spans.push(Span::styled(theme.icons.rename, theme.muted));
    }
    let (dir, name) = split_path(&item.file.path);
    if with_dir && !dir.is_empty() {
        spans.push(Span::styled(dir.to_owned(), theme.muted));
    }
    spans.push(Span::styled(
        name.to_owned(),
        if muted { theme.path_viewed } else { theme.path },
    ));
    spans
}

/// Rows of a file header at `density`, with the viewed mark as a button.
#[must_use]
pub fn header_rows(
    item: &FileHeaderItem,
    baseline: super::compare::Since,
    density: Density,
    theme: &ReviewTheme,
    width: usize,
) -> ItemRows {
    let fill = theme.files.header;
    match density {
        Density::Compact | Density::Comfortable => {
            let mut content = vec![pill(item, theme), Span::raw(" ")];
            content.extend(path_spans(item, theme, true));
            let mut trailing = header_facts(item, baseline, theme, true);
            let viewed = viewed_span(item.viewed, false, theme);
            // The viewed mark and its space end the row.
            let mark_cells = display_width(&viewed.content) + 1;
            let facts: usize = trailing
                .iter()
                .map(|span| display_width(&span.content))
                .sum();
            let versions = version_spans(item, theme, width, facts + mark_cells);
            let mut actions = vec![chevron_action(theme)];
            if let Some((spans, steps)) = versions {
                for (from_end, action) in steps {
                    let at = width.saturating_sub(mark_cells + from_end);
                    actions.push(ActionCells {
                        row: 0,
                        cells: cells(at)..cells(at + 1),
                        action: ItemAction::File(action),
                    });
                }
                trailing.extend(spans);
            }
            trailing.push(viewed);
            trailing.push(Span::raw(" "));
            let line = header_line(
                &Line::from_spans(lead(item, theme, true, content)),
                &Line::from_spans(trailing),
                width,
                fill,
            );
            let mark = width.saturating_sub(2);
            actions.push(ActionCells {
                row: 0,
                cells: cells(mark)..cells(mark + 1),
                action: ItemAction::File(FileAction::Viewed),
            });
            ItemRows::lines(vec![line], actions)
        }
        Density::Spacious => {
            let mut first = vec![pill(item, theme), Span::raw(" ")];
            first.extend(path_spans(item, theme, false));
            let viewed = viewed_span(item.viewed, true, theme);
            let viewed_width = display_width(&viewed.content);
            let position = format!("   {} of {} files ", item.position, item.count);
            let trailing = vec![viewed, Span::styled(position.clone(), theme.muted)];
            let top = header_line(
                &Line::from_spans(lead(item, theme, true, first)),
                &Line::from_spans(trailing),
                width,
                fill,
            );
            let start = width.saturating_sub(display_width(&position) + viewed_width);
            let (dir, _) = split_path(&item.file.path);
            let mut second = vec![Span::raw("    ")];
            second.push(Span::styled(
                if dir.is_empty() { "./" } else { dir }.to_owned(),
                theme.muted,
            ));
            if let Some(old) = &item.old_path {
                second.push(Span::styled(
                    format!("{}renamed from {old}", theme.icons.separator),
                    theme.status_renamed,
                ));
            }
            let mut facts = header_facts(item, baseline, theme, true);
            let facts_width: usize = facts.iter().map(|span| display_width(&span.content)).sum();
            let mut actions = vec![
                chevron_action(theme),
                ActionCells {
                    row: 0,
                    cells: cells(start)..cells(start + viewed_width),
                    action: ItemAction::File(FileAction::Viewed),
                },
            ];
            if let Some((spans, steps)) = version_spans(item, theme, width, facts_width) {
                for (from_end, action) in steps {
                    let at = width.saturating_sub(from_end);
                    actions.push(ActionCells {
                        row: 1,
                        cells: cells(at)..cells(at + 1),
                        action: ItemAction::File(action),
                    });
                }
                facts.extend(spans);
            }
            let bottom = header_line(
                &Line::from_spans(lead(item, theme, false, second)),
                &Line::from_spans(facts),
                width,
                fill,
            );
            ItemRows::lines(vec![top, bottom], actions)
        }
    }
}

/// The language of `path`, by its extension or name, when it is a common one.
fn language(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    if matches!(lower.as_str(), "dockerfile" | "makefile" | "justfile") {
        return Some(match lower.as_str() {
            "dockerfile" => "Dockerfile",
            "makefile" => "Make",
            _ => "Just",
        });
    }
    let extension = lower.rsplit_once('.')?.1;
    Some(match extension {
        "rs" => "Rust",
        "ts" | "tsx" | "mts" | "cts" => "TypeScript",
        "js" | "jsx" | "mjs" | "cjs" => "JavaScript",
        "py" => "Python",
        "go" => "Go",
        "rb" => "Ruby",
        "java" => "Java",
        "kt" | "kts" => "Kotlin",
        "swift" => "Swift",
        "c" | "h" => "C",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "C++",
        "cs" => "C#",
        "php" => "PHP",
        "sh" | "bash" | "zsh" => "Shell",
        "sql" => "SQL",
        "md" | "markdown" => "Markdown",
        "json" => "JSON",
        "yaml" | "yml" => "YAML",
        "toml" => "TOML",
        "html" | "htm" => "HTML",
        "css" | "scss" | "sass" => "CSS",
        "nix" => "Nix",
        "tf" | "tfvars" | "hcl" => "Terraform",
        "proto" => "Protobuf",
        "graphql" | "gql" => "GraphQL",
        "lua" => "Lua",
        "ex" | "exs" => "Elixir",
        "dart" => "Dart",
        "vue" => "Vue",
        "svelte" => "Svelte",
        _ => return None,
    })
}

/// The facts of a file's details row, in order.
fn detail_facts(item: &FileDetailsItem) -> Vec<String> {
    let mut facts = Vec::new();
    if let Some(old) = &item.old_path {
        facts.push(format!("renamed from {old}"));
    }
    if let Some(language) = language(&item.file.path) {
        facts.push(language.to_owned());
    }
    if let Some(lines) = item.line_count {
        facts.push(format!("{lines} line{}", if lines == 1 { "" } else { "s" }));
    }
    if let Some(hunks) = item.hunks {
        facts.push(format!("{hunks} hunk{}", if hunks == 1 { "" } else { "s" }));
    }
    for (flag, label) in [
        (ReviewIndexFlag::Binary, "binary"),
        (ReviewIndexFlag::ModeChanged, "mode changed"),
        (ReviewIndexFlag::Symlink, "symbolic link"),
        (ReviewIndexFlag::Submodule, "submodule"),
    ] {
        if item.flags.contains(&flag) {
            facts.push(label.to_owned());
        }
    }
    if !item.layers.is_empty() {
        let layers: Vec<String> = item
            .layers
            .iter()
            .map(|layer| format!("#{layer}"))
            .collect();
        facts.push(format!("changed by {}", layers.join(" ")));
    }
    if item.thread_count > 0 {
        facts.push(format!(
            "{} thread{}",
            item.thread_count,
            if item.thread_count == 1 { "" } else { "s" }
        ));
    }
    facts
}

/// The details buttons, in order of importance.
fn detail_buttons(item: &FileDetailsItem, spacious: bool) -> Vec<(&'static str, FileAction)> {
    let mut buttons = vec![
        (" Comment ", FileAction::Comment),
        (" Ask Bcode ", FileAction::Ask),
        (" Whole file ", FileAction::WholeFile),
        (
            if item.collapsed {
                " Expand "
            } else {
                " Collapse "
            },
            FileAction::Collapse,
        ),
    ];
    if item.in_stack {
        buttons.push((" Through the stack ", FileAction::Stack));
    }
    buttons.push((" Copy path ", FileAction::CopyPath));
    if spacious {
        buttons.push((" ↑ prev ", FileAction::Previous));
        buttons.push((" next ↓ ", FileAction::Next));
    }
    buttons
}

/// Rows of a file's details at `density`: one row (facts left, buttons right) when comfortable,
/// facts and buttons on rows of their own when spacious.
#[must_use]
pub fn details_rows(
    item: &FileDetailsItem,
    density: Density,
    theme: &ReviewTheme,
    width: usize,
) -> ItemRows {
    const INDENT: usize = 5;
    let spacious = density == Density::Spacious;
    let facts = detail_facts(item).join(theme.icons.separator);
    let buttons = detail_buttons(item, spacious);
    // Buttons that fit, from the start; the facts take what is left (comfortable).
    let room = width.saturating_sub(INDENT);
    let mut used = 0;
    let mut kept = Vec::new();
    let keep_for_facts = if spacious { 0 } else { room / 3 };
    for (label, action) in buttons {
        let cells = display_width(label) + 1;
        if used + cells > room.saturating_sub(keep_for_facts) {
            break;
        }
        used += cells;
        kept.push((label, action));
    }
    let button_row = |row: u16, start: usize| -> (Vec<Span>, Vec<ActionCells<ItemAction>>) {
        let mut spans = Vec::new();
        let mut actions = Vec::new();
        let mut x = start;
        for (label, action) in &kept {
            let cells_wide = display_width(label);
            actions.push(ActionCells {
                row,
                cells: cells(x)..cells(x + cells_wide),
                action: ItemAction::File(*action),
            });
            spans.push(Span::styled((*label).to_owned(), theme.pill));
            spans.push(Span::raw(" "));
            x += cells_wide + 1;
        }
        (spans, actions)
    };
    let pad = Span::raw(" ".repeat(INDENT.min(width)));
    if spacious {
        let first = Line::from_spans(vec![pad.clone(), Span::styled(facts, theme.muted)]);
        let (spans, actions) = button_row(1, INDENT);
        let mut second = vec![pad];
        second.extend(spans);
        return ItemRows::lines(
            vec![
                first.truncate(width),
                Line::from_spans(second).truncate(width),
            ],
            actions,
        );
    }
    let start = width.saturating_sub(used);
    let (spans, actions) = button_row(0, start);
    let facts_room = start.saturating_sub(INDENT + 1);
    let mut line = vec![pad, Span::styled(facts, theme.muted)];
    let left = Line::from_spans(std::mem::take(&mut line)).truncate(INDENT + facts_room);
    let gap = start.saturating_sub(left.width());
    let mut spans_all = left.spans;
    spans_all.push(Span::raw(" ".repeat(gap)));
    spans_all.extend(spans);
    ItemRows::lines(vec![Line::from_spans(spans_all).truncate(width)], actions)
}

/// What a break separates: the file it ends (`None` for a source's introduction).
#[derive(Debug, Clone, Copy)]
pub struct Break<'a> {
    /// The file ending, with the next file's position.
    pub end: Option<&'a FileEndItem>,
}

/// One row of `glyph` across `width`, with a cell of `margin` at each side when `inset`.
fn edge_row(glyph: &str, style: Style, margin: Style, inset: bool, width: usize) -> Line {
    if !inset || width < 3 {
        return Line::from_spans(vec![Span::styled(glyph.repeat(width), style)]);
    }
    Line::from_spans(vec![
        Span::styled(" ", margin),
        Span::styled(glyph.repeat(width - 2), style),
        Span::styled(" ", margin),
    ])
}

/// A rule across `width`, ending in ` N of M ` when `label` names one.
fn rule_row(label: Option<String>, theme: &ReviewTheme, width: usize) -> Line {
    let glyph = theme.icons.file_rule;
    let Some(label) = label.filter(|label| display_width(label) + 6 <= width) else {
        return Line::from_spans(vec![Span::styled(glyph.repeat(width), theme.files.rule)]);
    };
    let tail = 2;
    let lead = width - display_width(&label) - tail;
    Line::from_spans(vec![
        Span::styled(glyph.repeat(lead), theme.files.rule),
        Span::styled(label, theme.files.label),
        Span::styled(glyph.repeat(tail), theme.files.rule),
    ])
}

/// Rows of a break at `density`: a card's edges, a rule, or blank rows.
#[must_use]
pub fn end_rows(
    at: Break,
    separation: Separation,
    density: Density,
    theme: &ReviewTheme,
    width: usize,
) -> ItemRows {
    let tight = at.end.is_some_and(|end| end.tight);
    let rows = usize::try_from(separation.height(density, tight)).unwrap_or(1);
    let desk = theme.files.desk;
    let blank = |style: Style| Line::from_spans(vec![Span::styled(" ".repeat(width), style)]);
    let label = at
        .end
        .filter(|_| separation.label && !tight)
        .and_then(|end| {
            end.next_position
                .map(|next| format!(" {next} of {} ", end.count))
        });
    let lines: Vec<Line> = match separation.style {
        SeparatorStyle::Gap { desk: on_desk } => {
            let style = if on_desk { desk } else { Style::new() };
            (0..rows).map(|_| blank(style)).collect()
        }
        SeparatorStyle::Rule => {
            let mut label = label;
            (0..rows)
                .map(|row| {
                    if row == rows / 2 {
                        rule_row(label.take(), theme, width)
                    } else {
                        blank(Style::new())
                    }
                })
                .collect()
        }
        // Between folded files: a sliver of desk, so the headers read as a list of cards.
        SeparatorStyle::Card if tight => vec![blank(desk)],
        SeparatorStyle::Card => {
            // A folded file's card ends at its header; an open one's at its page.
            let bottom = if at.end.is_some_and(|end| end.folded) {
                theme.files.lip
            } else {
                theme.files.page_edge
            };
            let mut lines = vec![edge_row(
                theme.icons.half_upper,
                bottom,
                desk,
                separation.inset,
                width,
            )];
            // The next card's lip, under a file with one after it (not before a source).
            let lip = at.end.is_some_and(|end| end.next_position.is_some()) && rows > 1;
            for _ in 1..rows - usize::from(lip) {
                lines.push(blank(desk));
            }
            if lip {
                lines.push(edge_row(
                    theme.icons.half_lower,
                    theme.files.lip,
                    desk,
                    separation.inset,
                    width,
                ));
            }
            lines
        }
    };
    ItemRows::lines(lines, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::super::compare::Since;
    use super::*;

    #[test]
    fn densities_follow_the_pane_with_some_slack() {
        assert_eq!(
            Density::for_size(80, 40, Density::Compact),
            Density::Compact
        );
        assert_eq!(
            Density::for_size(100, 30, Density::Compact),
            Density::Comfortable
        );
        assert_eq!(
            Density::for_size(150, 50, Density::Compact),
            Density::Spacious
        );
        // Shrinking a little keeps the current density; more drops it.
        assert_eq!(
            Density::for_size(139, 44, Density::Spacious),
            Density::Spacious
        );
        assert_eq!(
            Density::for_size(130, 44, Density::Spacious),
            Density::Comfortable
        );
        assert_eq!(
            Density::for_size(89, 30, Density::Comfortable),
            Density::Comfortable
        );
        assert_eq!(
            Density::for_size(89, 30, Density::Compact),
            Density::Compact
        );
    }

    fn header(
        path: &str,
        versions: Option<bcode_code_review::document::FileVersionsItem>,
    ) -> FileHeaderItem {
        FileHeaderItem {
            file: std::sync::Arc::new(bcode_code_review::document::FileKey::new("s", None, path)),
            status: bcode_code_review_models::ReviewFileStatus::Modified,
            old_path: None,
            additions: 12,
            deletions: 3,
            flags: std::collections::BTreeSet::new(),
            viewed: false,
            load: bcode_code_review::document::FileLoad::Loaded,
            layers: Vec::new(),
            thread_count: 0,
            since: Some(bcode_code_review_models::FileSince::Changed {
                added: 4,
                removed: 1,
            }),
            versions,
            uncommitted: None,
            position: 1,
            count: 3,
            collapsed: false,
        }
    }

    /// The text of the cells `cells` of `line`, by display width.
    fn cells_text(line: &Line, cells: std::ops::Range<u16>) -> String {
        let mut x = 0_usize;
        let mut out = String::new();
        for span in &line.spans {
            for grapheme in span.content.split_inclusive(|_: char| true) {
                let width = display_width(grapheme);
                if x >= usize::from(cells.start) && x + width <= usize::from(cells.end) {
                    out.push_str(grapheme);
                }
                x += width;
            }
        }
        out
    }

    #[test]
    fn a_files_versions_show_by_fit_and_their_steps_are_where_they_are_painted() {
        let theme = ReviewTheme::derive(None, crate::theme::IconStyle::Unicode);
        let versions = bcode_code_review::document::FileVersionsItem {
            changed_in: 2,
            steps: 4,
            version: 3,
            shown: Some(1),
            previous: true,
            next: true,
        };
        let mut seen = std::collections::BTreeSet::new();
        for path in ["src/lib.rs", "文档/很长的中文文件名称_🙂.rs"] {
            let item = header(path, Some(versions));
            for density in [Density::Compact, Density::Spacious] {
                for width in 0..=160 {
                    let rows = header_rows(&item, Since::default(), density, &theme, width);
                    for line in &rows.lines {
                        assert!(line.width() <= width.max(1), "{path} {density:?} {width}");
                    }
                    for action in &rows.actions {
                        let glyph = match action.action {
                            ItemAction::File(FileAction::VersionPrevious) => theme.icons.step_back,
                            ItemAction::File(FileAction::VersionNext) => theme.icons.step_forward,
                            _ => continue,
                        };
                        let line = &rows.lines[usize::from(action.row)];
                        assert_eq!(
                            cells_text(line, action.cells.clone()),
                            glyph,
                            "{path} {density:?} at {width}: {}",
                            line.plain_text()
                        );
                        seen.insert(line.plain_text().contains("changed in 2 of 4 pushes"));
                    }
                }
            }
        }
        // Wide headers say in how many pushes the file changed; narrower ones only step.
        assert_eq!(seen.len(), 2);
        let wide = header_rows(
            &header("src/lib.rs", Some(versions)),
            Since::default(),
            Density::Compact,
            &theme,
            160,
        );
        assert!(wide.lines[0].plain_text().contains("v3 ◂ ▸"));
        // Without versions, nothing of them shows.
        let plain = header_rows(
            &header("src/lib.rs", None),
            Since::default(),
            Density::Compact,
            &theme,
            160,
        );
        assert!(!plain.lines[0].plain_text().contains('◂'));
    }

    #[test]
    fn languages_come_from_names_and_extensions() {
        assert_eq!(language("src/main.rs"), Some("Rust"));
        assert_eq!(language("web/App.TSX"), Some("TypeScript"));
        assert_eq!(language("Dockerfile"), Some("Dockerfile"));
        assert_eq!(language("README"), None);
    }
}
