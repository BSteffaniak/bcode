//! Combined palette × profile picker rendering and hit testing.
//!
//! One layout definition, [`appearance_layout`], places the two axis columns,
//! the preview pane, and the diagnostic row. Painting and mouse hit testing
//! both derive their rectangles from it, so a click always lands on what was
//! painted, and a wheel notch always scrolls the column under the pointer.
//! Narrow modals degrade explicitly: first the preview is dropped, then the
//! columns stack vertically, and finally only the active axis column is
//! shown.

use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::composition::TextBlock;
use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::{Insets, Point, Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::Modifier;
use bmux_tui_components::modal_frame::{
    ModalFrame, ModalFrameComponent, ModalPlacement, ModalSizing,
};

use super::appearance_picker::{APPEARANCE_PICKER_SCOPE, AppearanceAxis, AppearancePickerState};
use super::render::TuiTheme;
use super::theme::ThemeCatalogEntry;

/// Narrowest axis column, in cells.
const COLUMN_MIN_WIDTH: u16 = 14;
/// Widest axis column when the preview is shown, in cells.
const COLUMN_MAX_WIDTH: u16 = 26;
/// Gap between the two axis columns.
const COLUMN_GAP: u16 = 1;
/// Gap between the columns and the preview pane.
const PREVIEW_GAP: u16 = 2;
/// Narrowest preview pane, in cells.
const PREVIEW_MIN_WIDTH: u16 = 36;
/// Shortest body that still shows the preview: the preview header (names,
/// catalog details, swatch, rule) and at least three sample rows.
const PREVIEW_MIN_HEIGHT: u16 = 7;
/// Shortest body that stacks both columns (a header and one row each).
const STACKED_MIN_HEIGHT: u16 = 4;

/// How the body degrades with the available area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppearanceLayoutMode {
    /// Both columns side by side plus the preview pane.
    Wide,
    /// Both columns side by side, no preview.
    Columns,
    /// Palette column above the profile column, no preview.
    Stacked,
    /// Only the active axis column.
    Single,
}

/// One painted axis column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppearanceColumn {
    /// Axis this column selects.
    pub axis: AppearanceAxis,
    /// Column header row, when the column has room for one.
    pub header: Option<Rect>,
    /// List rows.
    pub list: Rect,
}

impl AppearanceColumn {
    const fn new(axis: AppearanceAxis, area: Rect) -> Self {
        if area.height >= 2 {
            Self {
                axis,
                header: Some(Rect::new(area.x, area.y, area.width, 1)),
                list: Rect::new(area.x, area.y + 1, area.width, area.height - 1),
            }
        } else {
            Self {
                axis,
                header: None,
                list: area,
            }
        }
    }

    /// Return the whole column area (header plus list).
    #[must_use]
    pub fn area(&self) -> Rect {
        self.header.map_or(self.list, |header| {
            Rect::new(
                header.x,
                header.y,
                header.width,
                self.list.bottom().saturating_sub(header.y),
            )
        })
    }
}

/// Body geometry shared by painting and hit testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppearanceLayout {
    /// Degradation mode chosen for the area.
    pub mode: AppearanceLayoutMode,
    /// Palette column, unless only the profile column fits.
    pub palette: Option<AppearanceColumn>,
    /// Profile column, unless only the palette column fits.
    pub profile: Option<AppearanceColumn>,
    /// Preview pane.
    pub preview: Option<Rect>,
    /// Diagnostic row for skipped catalog entries.
    pub diagnostic: Option<Rect>,
}

impl AppearanceLayout {
    /// Return the painted columns, palette first.
    pub fn columns(&self) -> impl Iterator<Item = AppearanceColumn> {
        self.palette.into_iter().chain(self.profile)
    }
}

/// Lay out the picker body inside the modal content area.
///
/// `active` decides which column survives when only one fits;
/// `diagnostics` reserves the bottom row for the first skipped-entry
/// diagnostic when the body keeps at least one other row.
#[must_use]
pub fn appearance_layout(
    content: Rect,
    active: AppearanceAxis,
    diagnostics: bool,
) -> AppearanceLayout {
    let (body, diagnostic) = if diagnostics && content.height >= 2 {
        (
            Rect::new(content.x, content.y, content.width, content.height - 1),
            Some(Rect::new(content.x, content.bottom() - 1, content.width, 1)),
        )
    } else {
        (content, None)
    };
    let columns_width = COLUMN_MIN_WIDTH * 2 + COLUMN_GAP;
    let side_by_side = |area: Rect| {
        let left = area.width.saturating_sub(COLUMN_GAP) / 2;
        (
            AppearanceColumn::new(
                AppearanceAxis::Palette,
                Rect::new(area.x, area.y, left, area.height),
            ),
            AppearanceColumn::new(
                AppearanceAxis::Profile,
                Rect::new(
                    area.x + left + COLUMN_GAP,
                    area.y,
                    area.width.saturating_sub(left + COLUMN_GAP),
                    area.height,
                ),
            ),
        )
    };
    if body.width >= columns_width + PREVIEW_GAP + PREVIEW_MIN_WIDTH
        && body.height >= PREVIEW_MIN_HEIGHT
    {
        let column = ((body.width - COLUMN_GAP - PREVIEW_GAP - PREVIEW_MIN_WIDTH) / 2)
            .clamp(COLUMN_MIN_WIDTH, COLUMN_MAX_WIDTH);
        let columns_area = Rect::new(body.x, body.y, column * 2 + COLUMN_GAP, body.height);
        let (palette, profile) = side_by_side(columns_area);
        let preview_x = columns_area.right() + PREVIEW_GAP;
        return AppearanceLayout {
            mode: AppearanceLayoutMode::Wide,
            palette: Some(palette),
            profile: Some(profile),
            preview: Some(Rect::new(
                preview_x,
                body.y,
                body.right().saturating_sub(preview_x),
                body.height,
            )),
            diagnostic,
        };
    }
    if body.width >= columns_width {
        let (palette, profile) = side_by_side(body);
        return AppearanceLayout {
            mode: AppearanceLayoutMode::Columns,
            palette: Some(palette),
            profile: Some(profile),
            preview: None,
            diagnostic,
        };
    }
    if body.height >= STACKED_MIN_HEIGHT {
        let top = body.height / 2;
        return AppearanceLayout {
            mode: AppearanceLayoutMode::Stacked,
            palette: Some(AppearanceColumn::new(
                AppearanceAxis::Palette,
                Rect::new(body.x, body.y, body.width, top),
            )),
            profile: Some(AppearanceColumn::new(
                AppearanceAxis::Profile,
                Rect::new(body.x, body.y + top, body.width, body.height - top),
            )),
            preview: None,
            diagnostic,
        };
    }
    let column = AppearanceColumn::new(active, body);
    AppearanceLayout {
        mode: AppearanceLayoutMode::Single,
        palette: (active == AppearanceAxis::Palette).then_some(column),
        profile: (active == AppearanceAxis::Profile).then_some(column),
        preview: None,
        diagnostic,
    }
}

/// Return the body layout for a terminal frame area, as painted.
#[must_use]
pub fn appearance_picker_layout(
    picker: &AppearancePickerState,
    frame_area: Rect,
    theme: TuiTheme,
) -> AppearanceLayout {
    appearance_layout(
        appearance_picker_modal(theme).content_area(frame_area),
        picker.axis(),
        !picker.palettes().diagnostics().is_empty(),
    )
}

/// Render the appearance picker with hints from effective bindings.
pub fn render_appearance_picker_with_keymap(
    picker: &mut AppearancePickerState,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
    keymap: &super::keymap::BmuxKeyMap,
) {
    let hint = |action| keymap.hint(APPEARANCE_PICKER_SCOPE, action);
    let modal = appearance_picker_modal(theme).title(format!(
        " Appearance · {}/{} axis · {} change · {} apply · {} cancel ",
        hint("tui.control.left"),
        hint("tui.control.right"),
        hint("tui.control.down"),
        hint("tui.control.submit"),
        hint("tui.control.cancel"),
    ));
    let area = Rect::new(0, 0, frame.area().width, frame.area().height);
    let component = ModalFrameComponent::new("appearance.modal", modal, TextBlock::new(""));
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    component.paint(&layout, frame);
    let body = appearance_picker_layout(picker, area, theme);
    for column in body.columns() {
        render_column(picker, column, frame, theme);
    }
    if let Some(preview) = body.preview {
        render_preview(picker, preview, frame, theme);
    }
    let diagnostics = picker.palettes().diagnostics();
    if let (Some(row), Some(diagnostic)) = (body.diagnostic, diagnostics.first()) {
        let more = match diagnostics.len() - 1 {
            0 => String::new(),
            others => format!(" (+{others} more)"),
        };
        frame.write_line_with_fallback_style(
            LocalRect::terminal(row),
            &Line::from_spans(vec![Span::styled(
                format!("Skipped: {diagnostic}{more}"),
                theme.muted,
            )]),
            theme.overlay,
        );
    }
}

/// Short variant summary of a palette catalog entry.
const fn variants_label(entry: &ThemeCatalogEntry) -> &'static str {
    match (entry.has_dark_variant, entry.has_light_variant) {
        (true, true) => "dark + light",
        (true, false) => "dark variant",
        (false, true) => "light variant",
        (false, false) => "fixed palette",
    }
}

/// Catalog details of a palette: id, origin, variants, and any validation
/// state other than plain acceptance.
fn palette_details(entry: &ThemeCatalogEntry) -> String {
    let mut details = format!(
        "{} · {} · {}",
        entry.id,
        entry.source,
        variants_label(entry)
    );
    if entry.validation != "valid" {
        details.push_str(" · ");
        details.push_str(&entry.validation);
    }
    details
}

fn render_column(
    picker: &mut AppearancePickerState,
    column: AppearanceColumn,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
) {
    let active = picker.axis() == column.axis;
    if let Some(header) = column.header {
        let (marker, style) = if active {
            ("› ", theme.focused.add_modifier(Modifier::BOLD))
        } else {
            ("  ", theme.muted)
        };
        frame.write_line_with_fallback_style(
            LocalRect::terminal(header),
            &Line::from_spans(vec![Span::styled(
                format!("{marker}{}", column.axis.label()),
                style,
            )]),
            theme.overlay,
        );
    }
    // The inactive column keeps its selection visible without competing
    // with the active one.
    let selection = if active {
        theme.selection
    } else {
        theme.muted.add_modifier(Modifier::REVERSED)
    };
    // Rows carry a muted tag: "current" for the active selection, otherwise
    // a palette's non-bundled origin.
    let row = |label: String, tag: Option<&str>| {
        let mut spans = vec![Span::styled(label, theme.text)];
        if let Some(tag) = tag {
            spans.push(Span::styled(format!("  {tag}"), theme.muted));
        }
        Line::from_spans(spans)
    };
    match column.axis {
        AppearanceAxis::Palette => {
            let items = picker
                .palettes()
                .entries()
                .iter()
                .map(|entry| {
                    let tag = if entry.selected {
                        Some("current")
                    } else {
                        (entry.source != "bundled").then_some(entry.source.as_str())
                    };
                    row(entry.display_name.clone(), tag)
                })
                .collect::<Vec<_>>();
            let state = picker.palettes_mut().list_render_state(column.list.height);
            super::picker_render::render_picker_list_with_selection(
                &items,
                state,
                column.list,
                frame,
                theme,
                selection,
            );
        }
        AppearanceAxis::Profile => {
            let original = picker.original_profile().map(str::to_owned);
            let items = picker
                .profiles()
                .entries()
                .iter()
                .map(|entry| {
                    row(
                        entry.display_name.clone(),
                        (entry.id == original).then_some("current"),
                    )
                })
                .collect::<Vec<_>>();
            let state = picker.profiles_mut().list_render_state(column.list.height);
            super::picker_render::render_picker_list_with_selection(
                &items,
                state,
                column.list,
                frame,
                theme,
                selection,
            );
        }
    }
}

/// Paint the preview header and, below it, the sample transcript laid out by
/// the transcript engine's producers at the pane width (the picker's
/// [`super::appearance_picker_sample::SamplePreview`]). The sample is clipped
/// to the pane and registers no interactions: the preview is not interactive.
fn render_preview(
    picker: &mut AppearancePickerState,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
) {
    frame.fill(LocalRect::terminal(area), " ", theme.raised);
    let palette = picker.palettes().selected_entry();
    let profile = picker.profiles().selected_entry();
    let header = [
        Line::from_spans(vec![
            Span::styled(
                palette.map_or_else(String::new, |entry| entry.display_name.clone()),
                theme.focused.add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", theme.muted),
            Span::styled(
                profile.map_or_else(String::new, |entry| entry.display_name.clone()),
                theme.focused.add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from_spans(vec![Span::styled(
            palette.map_or_else(String::new, palette_details),
            theme.muted,
        )]),
        Line::from_spans(vec![
            Span::styled("██", theme.focused),
            Span::styled(" ██", theme.info),
            Span::styled(" ██", theme.success),
            Span::styled(" ██", theme.warning),
            Span::styled(" ██", theme.error),
            Span::styled(" ", theme.raised),
            Span::styled(" selected ", theme.selection),
            Span::styled(
                profile
                    .and_then(|entry| entry.description.clone())
                    .map_or_else(String::new, |description| format!("  {description}")),
                theme.muted,
            ),
        ]),
        Line::from_spans(vec![Span::styled(
            "─".repeat(usize::from(area.width)),
            theme.border,
        )]),
    ];
    let mut header_rows = 0_u16;
    for row in header.iter().take(usize::from(area.height)) {
        frame.write_line_with_fallback_style(
            LocalRect::terminal(Rect::new(area.x, area.y + header_rows, area.width, 1)),
            row,
            theme.raised,
        );
        header_rows += 1;
    }
    let sample_rows = area.height - header_rows;
    if sample_rows == 0 || area.width == 0 {
        return;
    }
    let Some(sample) = picker.preview_mut().layout(area.width) else {
        return;
    };
    frame.with_child(
        i32::from(area.x),
        i64::from(area.y + header_rows),
        LocalRect::new(0, 0, area.width, sample_rows),
        |cx| sample.paint(cx),
    );
}

/// What a mouse press landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppearanceHit {
    /// A column outside its rows (header or empty space): focus its axis.
    Axis(AppearanceAxis),
    /// One absolute row of an axis.
    Row(AppearanceAxis, usize),
}

/// Resolve a left mouse press against the painted layout.
///
/// Only presses count: releasing never applies, so a click can adjust one
/// axis and then the other before Enter persists both.
#[must_use]
pub fn appearance_picker_hit(
    picker: &AppearancePickerState,
    mouse: MouseEvent,
    frame_area: Rect,
    theme: TuiTheme,
) -> Option<AppearanceHit> {
    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return None;
    }
    hit_in_layout(
        picker,
        &appearance_picker_layout(picker, frame_area, theme),
        mouse.position,
    )
}

/// Resolve a vertical wheel notch against the painted layout: the column
/// under the pointer and whether the notch moves down.
#[must_use]
pub fn appearance_picker_wheel(
    picker: &AppearancePickerState,
    mouse: MouseEvent,
    frame_area: Rect,
    theme: TuiTheme,
) -> Option<(AppearanceAxis, bool)> {
    let forward = match mouse.kind {
        MouseEventKind::ScrollDown => true,
        MouseEventKind::ScrollUp => false,
        _ => return None,
    };
    appearance_picker_layout(picker, frame_area, theme)
        .columns()
        .find(|column| column.area().contains(mouse.position))
        .map(|column| (column.axis, forward))
}

fn hit_in_layout(
    picker: &AppearancePickerState,
    layout: &AppearanceLayout,
    point: Point,
) -> Option<AppearanceHit> {
    let column = layout
        .columns()
        .find(|column| column.area().contains(point))?;
    if !column.list.contains(point) {
        return Some(AppearanceHit::Axis(column.axis));
    }
    let row = picker
        .list_offset(column.axis)
        .saturating_add(usize::from(point.y - column.list.y));
    Some(if row < picker.row_count(column.axis) {
        AppearanceHit::Row(column.axis, row)
    } else {
        AppearanceHit::Axis(column.axis)
    })
}

fn appearance_picker_modal(theme: TuiTheme) -> ModalFrame {
    ModalFrame::new(
        ModalSizing::new(Size::new(40, 8), Size::new(150, 40), Insets::all(2)),
        theme.modal_theme(),
    )
    .chrome(crate::render::dialog_chrome())
    .title(" Appearance ")
    .padding(Insets::new(1, 1, 1, 1))
    .placement(ModalPlacement::Centered)
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;

    use super::*;
    use crate::appearance_picker::tests::{palette, picker, profile};

    fn render_into(picker: &mut AppearancePickerState, area: Rect) -> (Buffer, String) {
        let theme = TuiTheme::for_theme_id("bcode-dark");
        let definition = bcode_tui_theme::definition::ThemeCatalog::bundled()
            .unwrap()
            .resolve(&bcode_tui_theme::definition::ThemeSelection::new("bcode"))
            .unwrap();
        let structure = crate::appearance_picker_sample::theme_structure_config(&definition);
        picker
            .preview_mut()
            .set_inputs(std::sync::Arc::new(definition), structure);
        let mut buffer = Buffer::empty(area);
        render_appearance_picker_with_keymap(
            picker,
            &mut PaintCx::new(&mut Frame::new(&mut buffer)),
            theme,
            &crate::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
        );
        let text = (0..area.height)
            .filter_map(|row| buffer.row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n");
        (buffer, text)
    }

    fn press(x: u16, y: u16) -> MouseEvent {
        MouseEvent::new(MouseEventKind::Down(MouseButton::Left), Point::new(x, y))
    }

    #[test]
    fn layout_degrades_from_preview_to_columns_to_stacked_to_single() {
        let cases = [
            (Rect::new(0, 0, 100, 20), AppearanceLayoutMode::Wide),
            (Rect::new(0, 0, 100, 5), AppearanceLayoutMode::Columns),
            (Rect::new(0, 0, 50, 20), AppearanceLayoutMode::Columns),
            (Rect::new(0, 0, 20, 20), AppearanceLayoutMode::Stacked),
            (Rect::new(0, 0, 20, 3), AppearanceLayoutMode::Single),
            (Rect::new(0, 0, 0, 0), AppearanceLayoutMode::Single),
        ];
        for (content, mode) in cases {
            for diagnostics in [false, true] {
                let layout = appearance_layout(content, AppearanceAxis::Profile, diagnostics);
                assert_eq!(layout.mode, mode, "{content:?}");
                let mut painted = layout
                    .columns()
                    .map(|column| column.area())
                    .chain(layout.preview)
                    .chain(layout.diagnostic)
                    .collect::<Vec<_>>();
                for rect in &painted {
                    assert!(
                        rect.x >= content.x
                            && rect.y >= content.y
                            && rect.right() <= content.right()
                            && rect.bottom() <= content.bottom(),
                        "{rect:?} escapes {content:?}"
                    );
                }
                // Painted regions never overlap.
                while let Some(rect) = painted.pop() {
                    assert!(
                        painted
                            .iter()
                            .all(|other| rect.intersection(*other).is_empty()),
                        "{rect:?} overlaps in {content:?}"
                    );
                }
            }
        }
        let single = appearance_layout(Rect::new(0, 0, 20, 3), AppearanceAxis::Profile, false);
        assert!(single.palette.is_none());
        assert_eq!(
            single.profile.map(|column| column.axis),
            Some(AppearanceAxis::Profile)
        );
    }

    /// Text of `rect` in `buffer`, one string per row, trailing blanks trimmed.
    fn region_text(buffer: &Buffer, rect: Rect) -> Vec<String> {
        (rect.y..rect.bottom())
            .map(|y| {
                (rect.x..rect.right())
                    .filter_map(|x| buffer.get(Point::new(x, y)))
                    .map(|cell| cell.symbol.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn preview_paints_the_engine_sample_below_its_header_clipped_to_the_pane() {
        for area in [Rect::new(0, 0, 130, 44), Rect::new(0, 0, 100, 20)] {
            let mut picker = picker(AppearanceAxis::Palette);
            let (buffer, _) = render_into(&mut picker, area);
            let theme = TuiTheme::for_theme_id("bcode-dark");
            let preview = appearance_picker_layout(&picker, area, theme)
                .preview
                .expect("wide enough for the preview");
            let sample_area = Rect::new(
                preview.x,
                preview.y + 4,
                preview.width,
                preview.height.saturating_sub(4),
            );
            // The same layout the preview retains, painted on its own.
            let layout = picker.preview_mut().layout(preview.width).unwrap();
            let mut expected =
                Buffer::empty(Rect::new(0, 0, sample_area.width, sample_area.height));
            layout.paint(&mut PaintCx::new(&mut Frame::new(&mut expected)));
            let expected = region_text(
                &expected,
                Rect::new(0, 0, sample_area.width, sample_area.height),
            );
            assert_eq!(region_text(&buffer, sample_area), expected, "{area:?}");
            assert!(
                expected.iter().any(|row| row.contains("You")),
                "{expected:?}"
            );
        }
    }

    #[test]
    fn preview_without_resolved_inputs_paints_only_its_header() {
        let mut picker = picker(AppearanceAxis::Palette);
        let area = Rect::new(0, 0, 130, 44);
        let mut buffer = Buffer::empty(area);
        render_appearance_picker_with_keymap(
            &mut picker,
            &mut PaintCx::new(&mut Frame::new(&mut buffer)),
            TuiTheme::for_theme_id("bcode-dark"),
            &crate::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
        );
        let text = (0..area.height)
            .filter_map(|row| buffer.row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("dusk · zen"), "{text}");
        assert!(!text.contains("let row = 1"), "{text}");
    }

    #[test]
    fn wide_picker_paints_both_columns_and_the_live_preview() {
        let (_, text) = render_into(
            &mut picker(AppearanceAxis::Palette),
            Rect::new(0, 0, 130, 44),
        );
        assert!(text.contains("› Palette"), "{text}");
        assert!(text.contains("  Profile"), "{text}");
        assert!(text.contains("dawn") && text.contains("bare"), "{text}");
        assert!(text.contains("dusk · zen"), "{text}");
        assert!(text.contains("██ ██"), "{text}");
        assert!(text.contains("zen profile"), "{text}");
        // The sample transcript renders through the transcript engine's producers.
        assert!(text.contains("let row = 1"), "{text}");
    }

    #[test]
    fn narrow_pickers_drop_the_preview_then_stack_and_stay_in_bounds() {
        for (area, stacked) in [
            (Rect::new(0, 0, 60, 20), false),
            (Rect::new(0, 0, 30, 20), true),
            (Rect::new(0, 0, 8, 3), true),
            (Rect::new(0, 0, 1, 1), true),
        ] {
            let mut picker = picker(AppearanceAxis::Profile);
            let (buffer, text) = render_into(&mut picker, area);
            assert!(!text.contains("let row = 1"), "{area:?}: {text}");
            let layout =
                appearance_picker_layout(&picker, area, TuiTheme::for_theme_id("bcode-dark"));
            assert!(layout.preview.is_none());
            if stacked {
                assert!(matches!(
                    layout.mode,
                    AppearanceLayoutMode::Stacked | AppearanceLayoutMode::Single
                ));
            }
            assert!(
                buffer
                    .get(Point::new(area.right() - 1, area.bottom() - 1))
                    .is_some()
            );
        }
    }

    #[test]
    fn wide_glyph_catalog_names_stay_inside_their_column() {
        let mut picker = AppearancePickerState::new(
            vec![palette("夜明けの海のテーマ🌊é\u{301}", true)],
            Vec::new(),
            vec![profile("集中モード👩‍💻")],
            None,
            AppearanceAxis::Palette,
        );
        for width in [40_u16, 64, 100] {
            let area = Rect::new(0, 0, width, 16);
            let (buffer, _) = render_into(&mut picker, area);
            let layout =
                appearance_picker_layout(&picker, area, TuiTheme::for_theme_id("bcode-dark"));
            let (Some(palette), Some(profile)) = (layout.palette, layout.profile) else {
                continue;
            };
            if layout.mode == AppearanceLayoutMode::Stacked {
                continue;
            }
            // The gap between the columns stays unpainted by either list.
            let gap = palette.list.right();
            assert!(gap < profile.list.x);
            let cell = buffer.get(Point::new(gap, palette.list.y)).unwrap();
            assert_eq!(
                cell.symbol, " ",
                "width {width}: list text bled into the gap"
            );
        }
    }

    #[test]
    fn hit_testing_agrees_with_painted_rows_and_headers() {
        let theme = TuiTheme::for_theme_id("bcode-dark");
        for area in [
            Rect::new(0, 0, 130, 44),
            Rect::new(0, 0, 50, 20),
            Rect::new(0, 0, 30, 20),
        ] {
            let mut picker = picker(AppearanceAxis::Palette);
            let (buffer, _) = render_into(&mut picker, area);
            let layout = appearance_picker_layout(&picker, area, theme);
            for column in layout.columns() {
                let header = column.header.expect("tall enough for headers");
                assert_eq!(
                    appearance_picker_hit(&picker, press(header.x, header.y), area, theme),
                    Some(AppearanceHit::Axis(column.axis))
                );
                // Every listed row is clickable where its name was painted.
                for row in 0..picker.row_count(column.axis) {
                    let y = column.list.y + u16::try_from(row).unwrap();
                    let painted = buffer.row_symbols(y).unwrap();
                    let name = match (column.axis, row) {
                        (AppearanceAxis::Palette, 0) => "dawn",
                        (AppearanceAxis::Palette, _) => "dusk",
                        (AppearanceAxis::Profile, 0) => "Theme default",
                        (AppearanceAxis::Profile, 1) => "bare",
                        (AppearanceAxis::Profile, _) => "zen",
                    };
                    assert!(painted.contains(name), "{area:?} row {row}: {painted}");
                    assert_eq!(
                        appearance_picker_hit(&picker, press(column.list.x, y), area, theme),
                        Some(AppearanceHit::Row(column.axis, row))
                    );
                }
                // Space below the rows focuses the axis without selecting.
                let below = column.list.y + u16::try_from(picker.row_count(column.axis)).unwrap();
                if below < column.list.bottom() {
                    assert_eq!(
                        appearance_picker_hit(&picker, press(column.list.x, below), area, theme),
                        Some(AppearanceHit::Axis(column.axis))
                    );
                }
            }
            if let Some(preview) = layout.preview {
                assert_eq!(
                    appearance_picker_hit(&picker, press(preview.x, preview.y), area, theme),
                    None
                );
            }
            let release = MouseEvent::new(MouseEventKind::Up(MouseButton::Left), Point::new(0, 0));
            assert_eq!(appearance_picker_hit(&picker, release, area, theme), None);
        }
    }

    #[test]
    fn hit_testing_uses_the_painted_scroll_offset() {
        let theme = TuiTheme::for_theme_id("bcode-dark");
        let area = Rect::new(0, 0, 60, 12);
        let palettes = (0..30)
            .map(|index| palette(&format!("theme-{index}"), index == 29))
            .collect();
        let mut picker = AppearancePickerState::new(
            palettes,
            Vec::new(),
            Vec::new(),
            None,
            AppearanceAxis::Palette,
        );
        let _ = render_into(&mut picker, area);
        let column = appearance_picker_layout(&picker, area, theme)
            .palette
            .unwrap();
        let offset = picker.list_offset(AppearanceAxis::Palette);
        assert!(offset > 0);
        assert_eq!(
            appearance_picker_hit(&picker, press(column.list.x, column.list.y), area, theme),
            Some(AppearanceHit::Row(AppearanceAxis::Palette, offset))
        );
    }

    fn picker_with_diagnostics(diagnostics: &[&str]) -> AppearancePickerState {
        let mut user = palette("tide", false);
        user.source = "user".to_owned();
        user.has_light_variant = true;
        AppearancePickerState::new(
            vec![palette("dawn", false), palette("dusk", true), user],
            diagnostics.iter().map(|&text| text.to_owned()).collect(),
            vec![profile("zen")],
            None,
            AppearanceAxis::Palette,
        )
    }

    #[test]
    fn preview_and_rows_show_catalog_details() {
        let mut picker = picker_with_diagnostics(&[]);
        let area = Rect::new(0, 0, 130, 44);
        let (_, text) = render_into(&mut picker, area);
        assert!(text.contains("dusk · bundled · dark variant"), "{text}");
        assert!(text.contains("dusk  current"), "{text}");
        // Non-bundled origins are tagged in the list.
        assert!(text.contains("tide  user"), "{text}");
        let _ = picker.select_row(AppearanceAxis::Palette, 2);
        let (_, text) = render_into(&mut picker, area);
        assert!(text.contains("tide · user · dark + light"), "{text}");
    }

    #[test]
    fn skipped_themes_get_a_bounded_unclickable_diagnostic_row() {
        let theme = TuiTheme::for_theme_id("bcode-dark");
        for area in [Rect::new(0, 0, 130, 44), Rect::new(0, 0, 50, 10)] {
            let mut picker = picker_with_diagnostics(&[
                "themes/broken.toml: a deliberately long rejected-candidate diagnostic",
                "themes/other.toml: invalid",
            ]);
            let (buffer, text) = render_into(&mut picker, area);
            assert!(text.contains("Skipped: themes/broken.toml"), "{text}");
            let layout = appearance_picker_layout(&picker, area, theme);
            let row = layout.diagnostic.expect("diagnostic row reserved");
            if area.width > 100 {
                assert!(text.contains("(+1 more)"), "{text}");
            }
            assert_eq!(
                appearance_picker_hit(&picker, press(row.x, row.y), area, theme),
                None
            );
            assert!(
                layout
                    .columns()
                    .all(|column| column.area().intersection(row).is_empty())
            );
            assert!(
                buffer
                    .get(Point::new(area.right() - 1, area.bottom() - 1))
                    .is_some()
            );
        }
    }

    #[test]
    fn wheel_notches_resolve_to_the_column_under_the_pointer() {
        let theme = TuiTheme::for_theme_id("bcode-dark");
        let area = Rect::new(0, 0, 130, 44);
        let mut picker = picker(AppearanceAxis::Palette);
        let _ = render_into(&mut picker, area);
        let layout = appearance_picker_layout(&picker, area, theme);
        let wheel = |kind, point| {
            appearance_picker_wheel(&picker, MouseEvent::new(kind, point), area, theme)
        };
        let profile = layout.profile.unwrap().list;
        assert_eq!(
            wheel(MouseEventKind::ScrollDown, Point::new(profile.x, profile.y)),
            Some((AppearanceAxis::Profile, true))
        );
        let palette = layout.palette.unwrap().list;
        assert_eq!(
            wheel(MouseEventKind::ScrollUp, Point::new(palette.x, palette.y)),
            Some((AppearanceAxis::Palette, false))
        );
        let preview = layout.preview.unwrap();
        assert_eq!(
            wheel(MouseEventKind::ScrollDown, Point::new(preview.x, preview.y)),
            None
        );
        assert_eq!(
            wheel(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(palette.x, palette.y)
            ),
            None
        );
    }

    #[test]
    fn modal_title_keeps_its_border_and_surfaces_stay_layered() {
        let theme = TuiTheme::for_theme_id("bcode-dark");
        let area = Rect::new(0, 0, 130, 44);
        let mut picker = picker(AppearanceAxis::Palette);
        let (buffer, _) = render_into(&mut picker, area);
        let panel = appearance_picker_modal(theme).panel_area(area);
        let top = buffer.row_symbols(panel.y).expect("modal top row");
        assert!(top.contains(" Appearance "), "{top}");
        assert!(top.contains("─╮"), "title erased border: {top}");
        let layout = appearance_picker_layout(&picker, area, theme);
        let preview = layout.preview.expect("wide picker has a preview");
        let gap = Point::new(layout.profile.unwrap().list.right(), preview.y);
        assert_eq!(buffer.get(gap).unwrap().style.bg, theme.overlay.bg);
        assert_eq!(
            buffer
                .get(Point::new(preview.x, preview.y))
                .unwrap()
                .style
                .bg,
            theme.raised.bg
        );
        assert!(buffer.cells().iter().any(|cell| {
            cell.style.bg == theme.selection.bg && cell.style.fg == theme.selection.fg
        }));
    }

    #[test]
    fn single_column_layout_hits_only_the_active_axis() {
        let content = Rect::new(0, 0, 20, 3);
        let picker = picker(AppearanceAxis::Profile);
        let layout = appearance_layout(content, picker.axis(), false);
        let column = layout.profile.unwrap();
        assert_eq!(
            hit_in_layout(&picker, &layout, Point::new(column.list.x, column.list.y)),
            Some(AppearanceHit::Row(AppearanceAxis::Profile, 0))
        );
        assert_eq!(layout.palette, None);
    }
}
