//! The comparison row of the review screen and the banner a changed review reopens with.
//!
//! The row reads `Changes since: [you last looked · 2d ago ▾]  3 of 21 PRs changed · 18 rebased
//! only  [│┃·│┃] now  last push 12m ago: #12 changed`: the control that chooses the baseline,
//! the summary of what the comparison found, and the push timeline (one tick per push, pushes
//! that changed some layer's own change emphasized, `[`/`]` around the pushes compared).
//!
//! Every part has narrower variants; a plan picks the widest combination that fits, measured
//! with BMUX display width, and the clickable parts come from the same plan as the painted
//! line. With fewer cells than pushes, consecutive pushes share a tick; below
//! [`MIN_TIMELINE_CELLS`] the timeline is left out.

use crate::theme::ReviewTheme;
use bmux_tui::geometry::Rect;
use bmux_tui::style::Style;
use bmux_tui::text::{Line, Span};
use bmux_tui::text_width::display_width;

/// Fewest cells the push timeline is shown in.
pub const MIN_TIMELINE_CELLS: usize = 12;

/// Cells between parts.
const GAP: usize = 2;

/// What the comparison row shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompareBar {
    /// The baseline, in full ("you last looked · 2d ago") and briefly ("last looked").
    pub baseline: [String; 2],
    /// What the comparison found, in full and briefly; empty when nothing is compared.
    pub summary: [String; 2],
    /// The stack's pushes, oldest first: whether each changed some layer's own change.
    pub pushes: Vec<bool>,
    /// The pushes compared: from column to column (column `c` is just before push `c`; the
    /// last column, `pushes.len()`, is now).
    pub range: Option<(usize, usize)>,
    /// The push the keyboard is on, while the timeline has the focus.
    pub cursor: Option<usize>,
    /// Whether the timeline shows.
    pub timeline: bool,
    /// The last push, said after the timeline when there is room.
    pub note: Option<String>,
    /// The view options on (`only what changed`), in full and briefly: said after the control
    /// so what the review shows is plain without opening the menu.
    pub chips: Vec<[String; 2]>,
}

/// A clickable part of the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarPart {
    /// The baseline control.
    Control,
    /// The summary.
    Summary,
    /// A view option that is on (`[only what changed]`).
    Chip,
    /// A tick of the timeline: the pushes `first..=last`.
    Tick {
        /// First push of the tick.
        first: usize,
        /// Last push of the tick.
        last: usize,
    },
}

/// The row laid out at a width: its line and where each part is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BarPlan {
    /// The painted line.
    pub line: Line,
    /// Parts, in cells from the row's left edge.
    pub parts: Vec<(BarPart, Rect)>,
}

impl BarPlan {
    /// The part at cell `x`.
    #[must_use]
    pub fn part_at(&self, x: u16) -> Option<BarPart> {
        self.parts
            .iter()
            .find(|(_, rect)| x >= rect.x && x < rect.x.saturating_add(rect.width))
            .map(|(part, _)| *part)
    }
}

/// Builds a line from spans, keeping each part's cells.
#[derive(Default)]
struct Row {
    spans: Vec<Span>,
    width: usize,
    parts: Vec<(BarPart, Rect)>,
}

impl Row {
    fn push(&mut self, text: impl Into<String>, style: Style) -> (usize, usize) {
        let text = text.into();
        let start = self.width;
        self.width += display_width(&text);
        self.spans.push(Span::styled(text, style));
        (start, self.width)
    }

    fn part(&mut self, part: BarPart, (start, end): (usize, usize)) {
        let cells = |value: usize| u16::try_from(value).unwrap_or(u16::MAX);
        self.parts.push((
            part,
            Rect::new(cells(start), 0, cells(end.saturating_sub(start)), 1),
        ));
    }

    fn gap(&mut self) {
        if self.width > 0 {
            self.push(" ".repeat(GAP), Style::default());
        }
    }
}

/// The control's variants, widest first.
fn control_variants(bar: &CompareBar, theme: &ReviewTheme) -> [String; 4] {
    let menu = theme.icons.menu;
    let delta = if menu.is_ascii() { "Since" } else { "Δ" };
    [
        format!("Changes since: [{} {menu}]", bar.baseline[0]),
        format!("Since: {} {menu}", bar.baseline[1]),
        format!("{delta} {} {menu}", bar.baseline[1]),
        format!("{delta}{menu}"),
    ]
}

/// The chips said with control variant `control`: in full with the widest control, briefly
/// with the next two, none with the narrowest.
fn chip_texts(bar: &CompareBar, control: usize) -> Vec<String> {
    let index = match control {
        0 => 0,
        1 | 2 => 1,
        _ => return Vec::new(),
    };
    bar.chips
        .iter()
        .map(|chip| format!("[{}]", chip[index]))
        .collect()
}

/// The pushes each tick stands for when the timeline has `cells` cells for ticks.
fn buckets(pushes: usize, cells: usize) -> Vec<(usize, usize)> {
    if pushes == 0 || cells == 0 {
        return Vec::new();
    }
    let size = pushes.div_ceil(cells.min(pushes));
    (0..pushes)
        .step_by(size)
        .map(|first| (first, (first + size).min(pushes) - 1))
        .collect()
}

/// The timeline's cells at most `room` wide, or `None` when it does not fit.
fn timeline_width(bar: &CompareBar, room: usize) -> Option<usize> {
    if !bar.timeline || bar.pushes.is_empty() || room < MIN_TIMELINE_CELLS {
        return None;
    }
    let fixed = 4 + if bar.range.is_some() { 2 } else { 0 };
    let ticks = bar.pushes.len().min(room - fixed);
    Some(fixed + buckets(bar.pushes.len(), ticks).len())
}

fn push_timeline(row: &mut Row, bar: &CompareBar, theme: &ReviewTheme, width: usize) {
    let fixed = 4 + if bar.range.is_some() { 2 } else { 0 };
    let cells = width.saturating_sub(fixed);
    let ticks = buckets(bar.pushes.len(), cells);
    let (from, to) = bar.range.unwrap_or((usize::MAX, usize::MAX));
    let len = bar.pushes.len();
    let marked = bar.range.is_some();
    // Compared from now: the markers close up after the last tick.
    let from_now = marked && from >= len;
    for &(first, last) in &ticks {
        if marked && !from_now && (first..=last).contains(&from) {
            row.push("[", theme.accent);
        }
        let changed = bar.pushes[first..=last].iter().any(|changed| *changed);
        let inside = marked && first >= from && last < to;
        let glyph = if changed {
            theme.icons.tick_changed
        } else {
            theme.icons.tick
        };
        let style = if bar
            .cursor
            .is_some_and(|cursor| (first..=last).contains(&cursor))
        {
            theme.selection
        } else if changed && inside {
            theme.since.badge
        } else if changed {
            theme.notice
        } else if inside {
            theme.text
        } else {
            theme.muted
        };
        let cells = row.push(glyph, style);
        row.part(BarPart::Tick { first, last }, cells);
        if marked && !from_now && to > 0 && (first..=last).contains(&(to - 1).min(len - 1)) {
            row.push("]", theme.accent);
        }
    }
    if from_now {
        row.push("[]", theme.accent);
    }
    row.push(" now", theme.muted);
}

/// Lay the row out in `width` cells.
#[must_use]
pub fn plan(bar: &CompareBar, theme: &ReviewTheme, width: u16) -> BarPlan {
    let width = usize::from(width);
    let controls = control_variants(bar, theme);
    // (control variant, summary variant or none, timeline, note), widest first.
    let combos: [(usize, Option<usize>, bool, bool); 9] = [
        (0, Some(0), true, true),
        (0, Some(0), true, false),
        (0, Some(1), true, false),
        (1, Some(1), true, false),
        (1, Some(1), false, false),
        (2, Some(1), false, false),
        (2, None, false, false),
        (3, None, false, false),
        (3, None, false, false),
    ];
    for (control, summary, timeline, note) in combos {
        let summary = summary
            .map(|index| bar.summary[index].as_str())
            .filter(|text| !text.is_empty());
        let note = note.then_some(bar.note.as_deref()).flatten();
        let chips = chip_texts(bar, control);
        let mut used = display_width(&controls[control]);
        for chip in &chips {
            used += 1 + display_width(chip);
        }
        if let Some(summary) = summary {
            used += GAP + display_width(summary);
        }
        if used > width {
            continue;
        }
        let timeline_cells = if timeline {
            match timeline_width(bar, width.saturating_sub(used + GAP)) {
                Some(cells) => Some(cells),
                // The timeline is wanted but does not fit: try a narrower combination, unless
                // none is wanted at all.
                None if bar.timeline && !bar.pushes.is_empty() => continue,
                None => None,
            }
        } else {
            None
        };
        let mut row = Row::default();
        let cells = row.push(controls[control].clone(), theme.accent);
        row.part(BarPart::Control, cells);
        for chip in chips {
            row.push(" ", Style::default());
            let cells = row.push(chip, theme.since.badge);
            row.part(BarPart::Chip, cells);
        }
        if let Some(summary) = summary {
            row.gap();
            let cells = row.push(summary, theme.muted);
            row.part(BarPart::Summary, cells);
        }
        if let Some(cells) = timeline_cells {
            row.gap();
            push_timeline(&mut row, bar, theme, cells);
        }
        if let Some(note) = note
            && row.width + GAP + display_width(note) <= width
        {
            row.gap();
            row.push(note, theme.muted);
        }
        return BarPlan {
            line: Line::from_spans(row.spans),
            parts: row.parts,
        };
    }
    BarPlan::default()
}

/// What the banner of a changed review says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Banner {
    /// What changed, in full and briefly.
    pub text: [String; 2],
    /// The key that switches views, and what switching shows.
    pub view: (String, String),
    /// The key that opens the baseline control.
    pub choose: String,
}

/// A clickable part of the banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerPart {
    /// Switch views.
    View,
    /// Choose what to compare with.
    Choose,
    /// Hide the banner.
    Dismiss,
}

/// The banner laid out at a width.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BannerPlan {
    /// The painted line.
    pub line: Line,
    /// Buttons, in cells from the row's left edge.
    pub parts: Vec<(BannerPart, Rect)>,
}

impl BannerPlan {
    /// The button at cell `x`.
    #[must_use]
    pub fn part_at(&self, x: u16) -> Option<BannerPart> {
        self.parts
            .iter()
            .find(|(_, rect)| x >= rect.x && x < rect.x.saturating_add(rect.width))
            .map(|(part, _)| *part)
    }
}

/// A banner variant: its text, when it has one, and its buttons.
type Variant<'a> = (Option<&'a str>, &'a [(BannerPart, String)]);

/// Lay the banner out in `width` cells: the full text and labelled buttons, else the brief text
/// and key buttons, else the key buttons alone, else the brief text cut to fit.
#[must_use]
pub fn plan_banner(banner: &Banner, theme: &ReviewTheme, width: u16) -> BannerPlan {
    let width = usize::from(width);
    let dismiss_short = if theme.icons.menu.is_ascii() {
        "[x]"
    } else {
        "[×]"
    };
    let labelled = [
        (
            BannerPart::View,
            format!("[{} · {}]", banner.view.1, banner.view.0),
        ),
        (
            BannerPart::Choose,
            format!("[Compare with{} · {}]", theme.icons.ellipsis, banner.choose),
        ),
        (BannerPart::Dismiss, "[Dismiss]".to_owned()),
    ];
    let keys = [
        (BannerPart::View, format!("[{}]", banner.view.0)),
        (BannerPart::Choose, format!("[{}]", banner.choose)),
        (BannerPart::Dismiss, dismiss_short.to_owned()),
    ];
    let variants: [Variant<'_>; 3] = [
        (Some(banner.text[0].as_str()), &labelled),
        (Some(banner.text[1].as_str()), &keys),
        (None, &keys),
    ];
    for (text, buttons) in variants {
        let mut needed = text.map_or(0, |text| display_width(text) + GAP);
        needed += buttons
            .iter()
            .map(|(_, label)| display_width(label) + 1)
            .sum::<usize>();
        if needed.saturating_sub(1) > width {
            continue;
        }
        let mut row = Row::default();
        if let Some(text) = text {
            row.push(text, theme.since.note);
            row.push(" ".repeat(GAP), Style::default());
        }
        let mut parts = Vec::new();
        for (index, (part, label)) in buttons.iter().enumerate() {
            if index > 0 {
                row.push(" ", Style::default());
            }
            let (start, end) = row.push(label.clone(), theme.accent);
            let cells = |value: usize| u16::try_from(value).unwrap_or(u16::MAX);
            parts.push((
                *part,
                Rect::new(cells(start), 0, cells(end.saturating_sub(start)), 1),
            ));
        }
        return BannerPlan {
            line: Line::from_spans(row.spans),
            parts,
        };
    }
    BannerPlan {
        line: Line::from_spans(vec![Span::styled(banner.text[1].clone(), theme.since.note)])
            .truncate(width),
        parts: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{IconStyle, ReviewTheme};

    fn theme() -> ReviewTheme {
        ReviewTheme::derive(None, IconStyle::Unicode)
    }

    fn bar() -> CompareBar {
        CompareBar {
            baseline: ["you last looked · 2d ago".into(), "last looked".into()],
            summary: [
                "3 of 21 PRs changed · 18 rebased only".into(),
                "3/21 changed".into(),
            ],
            pushes: vec![false, true, false, false, true, false, true, false],
            range: Some((4, 8)),
            cursor: None,
            timeline: true,
            note: Some("last push 12m ago: #12 changed".into()),
            chips: vec![["only what changed".into(), "changes".into()]],
        }
    }

    /// The text of the span painted from `rect`'s first cell over its width.
    fn painted(line: &Line, rect: Rect) -> String {
        let mut x = 0_usize;
        for span in &line.spans {
            let cells = display_width(&span.content);
            if x == usize::from(rect.x) && cells == usize::from(rect.width) {
                return span.content.clone();
            }
            x += cells;
        }
        String::new()
    }

    #[test]
    fn the_row_fits_every_width_and_its_parts_are_where_they_are_painted() {
        let theme = theme();
        let mut bars = vec![bar()];
        let mut cjk = bar();
        cjk.baseline = ["上次查看 · 2天前 🙂".into(), "上次 🙂".into()];
        bars.push(cjk);
        for bar in &bars {
            for width in 0..=160_u16 {
                let plan = plan(bar, &theme, width);
                assert!(plan.line.width() <= usize::from(width), "width {width}");
                let mut last_end = 0;
                for (part, rect) in &plan.parts {
                    assert!(rect.x >= last_end, "{part:?} overlaps at {width}");
                    assert!(rect.x + rect.width <= width, "{part:?} outside at {width}");
                    last_end = rect.x + rect.width;
                    let text = painted(&plan.line, *rect);
                    match part {
                        BarPart::Control => assert!(
                            control_variants(bar, &theme).contains(&text),
                            "{text:?} at {width}"
                        ),
                        BarPart::Summary => assert!(bar.summary.contains(&text), "{text:?}"),
                        BarPart::Chip => assert!(
                            bar.chips
                                .iter()
                                .flatten()
                                .any(|chip| text == format!("[{chip}]")),
                            "{text:?}"
                        ),
                        BarPart::Tick { .. } => assert!(
                            text == theme.icons.tick || text == theme.icons.tick_changed,
                            "{text:?}"
                        ),
                    }
                }
                // Something shows from the smallest control's width on.
                let smallest = display_width(&control_variants(bar, &theme)[3]);
                assert_eq!(
                    plan.parts.iter().any(|(part, _)| *part == BarPart::Control),
                    usize::from(width) >= smallest,
                    "width {width}"
                );
            }
        }
    }

    #[test]
    fn pushes_share_ticks_when_cells_are_few_and_the_timeline_hides_when_too_narrow() {
        let theme = theme();
        let mut bar = bar();
        bar.pushes = (0..40).map(|index| index % 7 == 0).collect();
        bar.summary = [String::new(), String::new()];
        bar.note = None;
        bar.range = None;
        bar.chips = Vec::new();
        let control = display_width(&control_variants(&bar, &theme)[0]);
        // Room for 10 ticks: four pushes each, and every push is covered once.
        let plan = plan(&bar, &theme, u16::try_from(control + GAP + 4 + 10).unwrap());
        let ticks: Vec<(usize, usize)> = plan
            .parts
            .iter()
            .filter_map(|(part, _)| match part {
                BarPart::Tick { first, last } => Some((*first, *last)),
                _ => None,
            })
            .collect();
        assert_eq!(ticks.len(), 10);
        assert_eq!(ticks.first(), Some(&(0, 3)));
        assert_eq!(ticks.last(), Some(&(36, 39)));
        // A tick with a changing push is emphasized.
        assert!(plan.line.plain_text().contains(theme.icons.tick_changed));
        // Too narrow for the smallest timeline beside the narrowest control with a timeline:
        // the timeline is left out.
        let brief = display_width(&control_variants(&bar, &theme)[1]);
        let narrow = super::plan(
            &bar,
            &theme,
            u16::try_from(brief + GAP + MIN_TIMELINE_CELLS - 1).unwrap(),
        );
        assert!(
            !narrow
                .parts
                .iter()
                .any(|(part, _)| matches!(part, BarPart::Tick { .. }))
        );
        assert!(
            narrow
                .parts
                .iter()
                .any(|(part, _)| *part == BarPart::Control)
        );
    }

    #[test]
    fn the_banner_degrades_to_key_buttons_and_its_buttons_are_where_they_are_painted() {
        let theme = theme();
        let banner = Banner {
            text: [
                "Changed since you looked 2d ago: 3 pull requests changed, 18 only rebased.".into(),
                "3 changed, 18 rebased".into(),
            ],
            view: ("shift+l".into(), "Only what changed".into()),
            choose: "b".into(),
        };
        let mut seen = std::collections::BTreeSet::new();
        for width in 0..=200_u16 {
            let plan = plan_banner(&banner, &theme, width);
            assert!(plan.line.width() <= usize::from(width), "width {width}");
            seen.insert(plan.parts.len());
            for (part, rect) in &plan.parts {
                assert!(rect.x + rect.width <= width);
                let text = painted(&plan.line, *rect);
                assert!(text.starts_with('[') && text.ends_with(']'), "{text:?}");
                if *part == BannerPart::Choose {
                    assert!(text.contains('b'), "{text:?}");
                }
            }
        }
        // Wide: three labelled buttons; narrow: none (the text alone).
        assert!(seen.contains(&3) && seen.contains(&0));
        let wide = plan_banner(&banner, &theme, 200);
        assert!(wide.line.plain_text().contains("[Dismiss]"));
        assert_eq!(wide.part_at(wide.parts[2].1.x), Some(BannerPart::Dismiss));
    }
}
