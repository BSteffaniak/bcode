//! Mouse-only composer submission controls, composed from BMUX primitives.

use std::cell::{Cell, RefCell};

use bmux_tui::chrome::Border;
use bmux_tui::component::{Component, Constraints, LayoutCx, LayoutId};
use bmux_tui::composition::{Align, Row, Stack, Surface};
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitMap, HitRegion};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui_components::button::{
    Button, ButtonComponent, ButtonOutcome, ButtonState, ButtonStyles,
};
use bmux_tui_components::menu::{Menu, MenuComponent, MenuItem, MenuOutcome, MenuState};
use bmux_tui_components::text_input::{TextInputComponent, TextInputPolicy, TextInputState};

use super::render::TuiTheme;

const SEND: &str = "composer.send";
const MORE: &str = "composer.send.more";
const MENU: &str = "composer.send.menu";
const MENU_BOUNDS: &str = "composer.send.menu.bounds";

/// Ephemeral pointer interaction state; never changes keyboard bindings.
#[derive(Debug, Clone)]
pub struct ComposerSubmit {
    send: Cell<ButtonState>,
    more: Cell<ButtonState>,
    menu: Cell<MenuState>,
    open: bool,
    anchor: Cell<Option<Rect>>,
    /// Pointer-only repaint region, reset before each runtime update.
    pub pointer_damage: RefCell<Vec<Rect>>,
}

impl Default for ComposerSubmit {
    fn default() -> Self {
        Self {
            send: Cell::new(ButtonState::new()),
            more: Cell::new(ButtonState::new()),
            menu: Cell::new(MenuState::new(None)),
            open: false,
            anchor: Cell::new(None),
            pointer_damage: RefCell::new(Vec::new()),
        }
    }
}

/// Result of routing a pointer event through the composer controls.
pub struct PointerOutcome {
    pub handled: bool,
    /// True when visible state changed, independently of event consumption.
    pub redraw: bool,
    /// Opening/closing or submitting requires rebuilding the scene.
    pub structural: bool,
    pub placement: Option<bcode_ipc::PromptPlacement>,
}

impl ComposerSubmit {
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    pub fn record_layout(&self, layout: &bmux_tui::component::LayoutNode, area: Rect) {
        self.anchor
            .set(layout.find_logical_rect(&LayoutId::new(MORE)).map(|rect| {
                Rect::new(
                    area.x
                        .saturating_add(u16::try_from(rect.x).unwrap_or(u16::MAX)),
                    area.y
                        .saturating_add(u16::try_from(rect.y).unwrap_or(u16::MAX)),
                    u16::try_from(rect.width).unwrap_or(u16::MAX),
                    u16::try_from(rect.height).unwrap_or(u16::MAX),
                )
            }));
    }

    pub fn dismiss(&mut self) {
        self.open = false;
        self.send.set(ButtonState::new());
        self.more.set(ButtonState::new());
        self.menu.set(MenuState::new(None));
    }

    fn set_enabled(&self, enabled: bool) {
        for state in [&self.send, &self.more] {
            let mut value = state.get();
            value.set_disabled(!enabled);
            state.set(value);
        }
    }

    pub fn mouse(&mut self, hits: &HitMap, mouse: MouseEvent, enabled: bool) -> PointerOutcome {
        self.pointer_damage.borrow_mut().clear();
        self.set_enabled(enabled);
        let mut handled = false;
        let mut redraw = false;
        let mut structural = false;
        let mut placement = None;
        let event = Event::Mouse(mouse);
        for (id, label, state) in [(SEND, "Send", &self.send), (MORE, "▾", &self.more)] {
            let Some(hit) = hits.regions().iter().find(|hit| hit.id.as_str() == id) else {
                state.set(ButtonState::new());
                continue;
            };
            let (left, right) = control_padding(id);
            let button = Button::new(label).filled_padding(left, right);
            if hit.area.width < button.width()
                || hit.area.contains(mouse.position)
                    && hits
                        .hit_mouse(mouse)
                        .is_none_or(|top| top.id().as_str() != id)
            {
                state.set(ButtonState::new());
                continue;
            }
            let previous = state.get();
            let mut value = previous;
            let outcome = button.handle_event(hit.area, &mut value, &event);
            state.set(value);
            if previous != value {
                redraw = true;
                self.pointer_damage.borrow_mut().push(hit.area);
            }
            handled |= outcome.needs_redraw() || hit.area.contains(mouse.position);
            if outcome == ButtonOutcome::Pressed {
                if id == MORE {
                    self.open = !self.open;
                    self.menu.set(MenuState::new(None));
                } else {
                    placement = Some(bcode_ipc::PromptPlacement::Steering);
                    self.open = false;
                }
                return PointerOutcome {
                    handled: true,
                    redraw: true,
                    structural: true,
                    placement,
                };
            }
        }
        if self.open {
            if let Some(area) = hits
                .regions()
                .iter()
                .find(|hit| hit.id.as_str() == MENU_BOUNDS)
                .map(|hit| hit.area)
            {
                let previous = self.menu.get();
                let mut state = previous;
                let outcome =
                    Menu::new(&menu_items(enabled)).handle_event(area, &mut state, &event);
                self.menu.set(state);
                if state != previous {
                    redraw = true;
                    self.pointer_damage.borrow_mut().push(area);
                }
                if let MenuOutcome::Activated { index, .. } = outcome {
                    placement = Some(if index == 0 {
                        bcode_ipc::PromptPlacement::Steering
                    } else {
                        bcode_ipc::PromptPlacement::FollowUp
                    });
                    self.open = false;
                    structural = true;
                    redraw = true;
                }
                handled |=
                    area.contains(mouse.position) || !matches!(outcome, MenuOutcome::Ignored);
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    && !area.contains(mouse.position)
                {
                    self.dismiss();
                    handled = true;
                    structural = true;
                    redraw = true;
                }
            } else {
                handled = true;
            }
        }
        PointerOutcome {
            handled,
            redraw,
            structural,
            placement,
        }
    }
}

fn menu_items(enabled: bool) -> [MenuItem; 2] {
    [
        MenuItem::new("send", "Send now").disabled(!enabled),
        MenuItem::new("queue", "Queue as follow-up").disabled(!enabled),
    ]
}

fn control_padding(id: &str) -> (u16, u16) {
    if id == SEND { (1, 0) } else { (1, 1) }
}

fn controls(submit: &ComposerSubmit, styles: ButtonStyles) -> impl Component + '_ {
    let (send_left, send_right) = control_padding(SEND);
    let (more_left, more_right) = control_padding(MORE);
    Row::new()
        .child(
            ButtonComponent::new(SEND, "Send", &submit.send)
                .filled()
                .padding(send_left, send_right)
                .styles(styles),
        )
        .child(
            ButtonComponent::new(MORE, "▾", &submit.more)
                .filled()
                .padding(more_left, more_right)
                .styles(styles),
        )
}

/// Right-edge reservation derived from the same controls used for paint and hits.
#[must_use]
pub fn trailing_exclusion(size: bmux_tui::geometry::Size) -> u16 {
    let submit = ComposerSubmit::default();
    let width = controls(&submit, ButtonStyles::default())
        .layout(Constraints::new(0, u64::MAX, 0, None), &mut LayoutCx::new())
        .size
        .width;
    let available = size.width.saturating_sub(4);
    if size.height >= 3 && width < u64::from(available) {
        u16::try_from(width).unwrap_or(u16::MAX)
    } else {
        0
    }
}

/// Presentation policy for the native composer and its submission controls.
#[derive(Clone, Copy)]
pub struct ComposerPresentation {
    pub theme: TuiTheme,
    pub focused: bool,
    pub enabled: bool,
    pub size: bmux_tui::geometry::Size,
}

/// One layout owns the editor and both adjoining, independently interactive buttons.
#[must_use]
pub fn panel<'a>(
    state: &'a RefCell<TextInputState>,
    policy: &'a TextInputPolicy,
    submit: &'a ComposerSubmit,
    presentation: ComposerPresentation,
) -> impl Component + 'a {
    let ComposerPresentation {
        theme,
        focused,
        enabled,
        size,
    } = presentation;
    submit.set_enabled(enabled);
    let styles = ButtonStyles {
        normal: theme
            .focused
            .add_modifier(bmux_tui::style::Modifier::REVERSED),
        focused: theme.focused,
        hovered: theme.selection,
        pressed: theme.selection,
        disabled: theme.muted,
    };
    let buttons = controls(submit, styles);
    let exclusion = trailing_exclusion(size);
    let overlay = Align::new(buttons)
        .horizontal(bmux_tui::composition::HorizontalAlignment::End)
        .vertical(bmux_tui::composition::VerticalAlignment::End);
    let editor = TextInputComponent::new("composer.editor", state, policy)
        .trailing_exclusion(exclusion)
        .placeholder("Ask Bcode…", theme.muted)
        .style(theme.text)
        .selection_style(theme.selection)
        .focused(focused);
    bcode_tui_components::composer::composer_panel(
        bcode_tui_components::composer::ComposerStyle {
            border: theme.focused,
            surface: theme.raised,
        },
        Stack::new()
            .child(editor)
            .child(bmux_tui::composition::Visibility::new(
                overlay,
                exclusion > 0,
            )),
    )
}

/// Paint last, with BMUX occlusion removing underlying selection and pointer regions.
pub fn paint_menu(
    submit: &mut ComposerSubmit,
    area: Rect,
    enabled: bool,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
) {
    if !submit.open || !enabled {
        submit.open = false;
        return;
    }
    let items = menu_items(enabled);
    let menu = MenuComponent::new(MENU, &items, &submit.menu)
        .styles(bmux_tui_components::menu::MenuStyles {
            background: theme.raised,
            normal: theme.text,
            focused: theme.selection,
            hovered: theme.selection,
            pressed: theme.selection,
            disabled: theme.muted,
            ..Default::default()
        })
        .fallback_style(theme.raised.patch(theme.text));
    let popup = Surface::new(menu)
        .id("composer.send.popup")
        .border(Border::single().style(theme.focused))
        .background(theme.raised);
    let component = Align::new(popup)
        .horizontal(bmux_tui::composition::HorizontalAlignment::End)
        .vertical(bmux_tui::composition::VerticalAlignment::End);
    let Some(anchor) = submit.anchor.get() else {
        submit.open = false;
        return;
    };
    let available = Rect::new(
        area.x,
        area.y,
        anchor.right().saturating_sub(area.x).min(area.width),
        anchor.y.saturating_sub(area.y).min(area.height),
    );
    let layout = component.layout(Constraints::tight(available.size()), &mut LayoutCx::new());
    let Some(menu_rect) = layout.find_logical_rect(&LayoutId::new(MENU)) else {
        return;
    };
    let menu_area = Rect::new(
        available
            .x
            .saturating_add(u16::try_from(menu_rect.x).unwrap_or(u16::MAX)),
        available
            .y
            .saturating_add(u16::try_from(menu_rect.y).unwrap_or(u16::MAX)),
        u16::try_from(menu_rect.width).unwrap_or(u16::MAX),
        u16::try_from(menu_rect.height).unwrap_or(u16::MAX),
    );
    if let Some(rect) = layout.find_logical_rect(&LayoutId::new("composer.send.popup")) {
        frame.occlude(LocalRect::new(
            i32::from(available.x).saturating_add(i32::try_from(rect.x).unwrap_or(i32::MAX)),
            i64::from(available.y).saturating_add(i64::try_from(rect.y).unwrap_or(i64::MAX)),
            u16::try_from(rect.width).unwrap_or(u16::MAX),
            u16::try_from(rect.height).unwrap_or(u16::MAX),
        ));
    }
    frame.with_child(
        i32::from(available.x),
        i64::from(available.y),
        LocalRect::new(0, 0, available.width, available.height),
        |cx| component.paint(&layout, cx),
    );
    frame.push_hit(HitRegion::new(MENU_BOUNDS, menu_area).pointer_events(false));
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Point;

    fn draw(submit: &mut ComposerSubmit, area: Rect, text: &str) -> HitMap {
        let state = RefCell::new(TextInputState::default());
        state.borrow_mut().buffer_mut().insert_str(text);
        let policy = super::super::app::composer_policy();
        let theme = TuiTheme::for_theme_id("terminal-native");
        let composer = Rect::new(
            area.x,
            area.bottom().saturating_sub(5).max(area.y),
            area.width,
            area.height.min(5),
        );
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        {
            let panel = panel(
                &state,
                &policy,
                submit,
                ComposerPresentation {
                    theme,
                    focused: false,
                    enabled: !text.trim().is_empty(),
                    size: composer.size(),
                },
            );
            let layout = panel.layout(Constraints::tight(composer.size()), &mut LayoutCx::new());
            submit.record_layout(&layout, composer);
            let mut cx = PaintCx::new(&mut frame);
            cx.with_child(
                i32::from(composer.x),
                i64::from(composer.y),
                LocalRect::new(0, 0, composer.width, composer.height),
                |cx| panel.paint(&layout, cx),
            );
        }
        paint_menu(
            submit,
            area,
            !text.trim().is_empty(),
            &mut PaintCx::new(&mut frame),
            theme,
        );
        frame.hits().clone()
    }

    fn point(hits: &HitMap, id: &str) -> Point {
        let hit = hits
            .regions()
            .iter()
            .find(|hit| hit.id.as_str() == id)
            .expect("visible control");
        Point::new(hit.area.x, hit.area.y)
    }

    fn click(
        submit: &mut ComposerSubmit,
        hits: &HitMap,
        id: &str,
        enabled: bool,
    ) -> PointerOutcome {
        let point = point(hits, id);
        let down = submit.mouse(
            hits,
            MouseEvent::new(MouseEventKind::Down(MouseButton::Left), point),
            enabled,
        );
        assert!(down.placement.is_none(), "press alone never submits");
        submit.mouse(
            hits,
            MouseEvent::new(MouseEventKind::Up(MouseButton::Left), point),
            enabled,
        )
    }

    #[test]
    fn send_and_arrow_have_independent_actions_and_hover_states() {
        let mut submit = ComposerSubmit::default();
        let area = Rect::new(3, 2, 60, 15);
        let hits = draw(&mut submit, area, "hello 世界 👩‍💻 e\u{301}");
        let before = submit.more.get();
        submit.mouse(
            &hits,
            MouseEvent::new(MouseEventKind::Move, point(&hits, MORE)),
            true,
        );
        assert_ne!(before, submit.more.get());
        assert_eq!(submit.send.get(), ButtonState::new());
        assert!(click(&mut submit, &hits, MORE, true).placement.is_none());
        assert!(submit.is_open());
        let hits = draw(&mut submit, area, "hello");
        assert_eq!(
            click(&mut submit, &hits, "composer.send.menu.queue", true).placement,
            Some(bcode_ipc::PromptPlacement::FollowUp)
        );
        assert!(!submit.open);
        let hits = draw(&mut submit, area, "hello");
        assert_eq!(
            click(&mut submit, &hits, SEND, true).placement,
            Some(bcode_ipc::PromptPlacement::Steering)
        );
    }

    #[test]
    fn compact_controls_share_the_last_editor_row_without_a_footer() {
        let submit = ComposerSubmit::default();
        let state = RefCell::new(TextInputState::default());
        let policy = super::super::app::composer_policy();
        let size = bmux_tui::geometry::Size::new(30, 3);
        let component = panel(
            &state,
            &policy,
            &submit,
            ComposerPresentation {
                theme: TuiTheme::for_theme_id("terminal-native"),
                focused: true,
                enabled: true,
                size,
            },
        );
        let layout = component.layout(Constraints::tight(size), &mut LayoutCx::new());
        let editor = layout
            .find_logical_rect(&LayoutId::new("composer.editor"))
            .unwrap();
        let send = layout.find_logical_rect(&LayoutId::new(SEND)).unwrap();
        let more = layout.find_logical_rect(&LayoutId::new(MORE)).unwrap();
        assert_eq!(editor.height, 1);
        assert_eq!(send.y, editor.y);
        assert_eq!(more.x, send.x + send.width);
        assert_eq!(send.width, 5);
        assert_eq!(more.width, 3);
        assert_eq!(u64::from(trailing_exclusion(size)), send.width + more.width);
        assert_eq!(send.x + send.width + more.width, editor.x + editor.width);
    }

    #[test]
    fn editor_reflows_only_the_bottom_row_and_reclaims_space_after_deletion() {
        let submit = ComposerSubmit::default();
        let state = RefCell::new(TextInputState::default());
        let policy = super::super::app::composer_policy();
        let theme = TuiTheme::for_theme_id("terminal-native");
        for (text, height, expected_rows) in [
            ("abc", 3, 1),
            ("abcdefghijklmnopqr", 4, 2),
            ("abc", 3, 1),
            ("中👩‍💻e\u{301}", 3, 1),
        ] {
            state.borrow_mut().buffer_mut().clear();
            state.borrow_mut().buffer_mut().insert_str(text);
            let size = bmux_tui::geometry::Size::new(22, height);
            let component = panel(
                &state,
                &policy,
                &submit,
                ComposerPresentation {
                    theme,
                    focused: true,
                    enabled: true,
                    size,
                },
            );
            let layout = component.layout(Constraints::tight(size), &mut LayoutCx::new());
            let mut buffer = Buffer::empty(Rect::new(0, 0, size.width, size.height));
            let mut frame = Frame::new(&mut buffer);
            component.paint(&layout, &mut PaintCx::new(&mut frame));
            let editor = layout
                .find_logical_rect(&LayoutId::new("composer.editor"))
                .unwrap();
            let more = frame
                .hits()
                .regions()
                .iter()
                .find(|hit| hit.id.as_str() == MORE)
                .unwrap();
            let cursor = frame.cursor().unwrap().position;
            assert!(cursor.y < more.area.y || cursor.x < more.area.x - 5);
            assert_eq!(
                state.borrow().rows_with_trailing_exclusion(
                    u16::try_from(editor.width).unwrap(),
                    trailing_exclusion(size),
                    8,
                ),
                expected_rows
            );
            assert_eq!(state.borrow().vertical_scroll(), 0);
        }
    }

    #[test]
    fn repeated_menu_hover_consumes_events_without_structural_redraw() {
        let mut submit = ComposerSubmit::default();
        let area = Rect::new(0, 0, 60, 15);
        let hits = draw(&mut submit, area, "hello");
        click(&mut submit, &hits, MORE, true);
        let hits = draw(&mut submit, area, "hello");
        let position = point(&hits, "composer.send.menu.queue");
        let changed = submit.mouse(&hits, MouseEvent::new(MouseEventKind::Move, position), true);
        assert!(changed.handled);
        assert!(changed.redraw);
        assert!(!changed.structural);
        assert!(!submit.pointer_damage.borrow().is_empty());
        let unchanged = submit.mouse(&hits, MouseEvent::new(MouseEventKind::Move, position), true);
        assert!(unchanged.handled);
        assert!(!unchanged.redraw);
        assert!(!unchanged.structural);
        assert!(submit.pointer_damage.borrow().is_empty());
    }

    #[test]
    fn empty_draft_and_cancelled_drag_never_submit() {
        let mut submit = ComposerSubmit::default();
        let area = Rect::new(0, 0, 60, 15);
        let hits = draw(&mut submit, area, "   ");
        assert!(click(&mut submit, &hits, SEND, false).placement.is_none());
        assert!(click(&mut submit, &hits, MORE, false).placement.is_none());
        assert!(!submit.open);
        let hits = draw(&mut submit, area, "hello");
        submit.mouse(
            &hits,
            MouseEvent::new(MouseEventKind::Down(MouseButton::Left), point(&hits, SEND)),
            true,
        );
        assert!(
            submit
                .mouse(
                    &hits,
                    MouseEvent::new(MouseEventKind::Up(MouseButton::Left), Point::new(0, 0)),
                    true
                )
                .placement
                .is_none()
        );
        submit.dismiss();
        assert!(
            submit
                .mouse(
                    &hits,
                    MouseEvent::new(MouseEventKind::Up(MouseButton::Left), point(&hits, SEND)),
                    true
                )
                .placement
                .is_none()
        );
    }

    #[test]
    fn menu_dismisses_without_click_through_and_does_not_retain_old_geometry() {
        let mut submit = ComposerSubmit::default();
        let area = Rect::new(0, 0, 60, 15);
        let hits = draw(&mut submit, area, "hello");
        click(&mut submit, &hits, MORE, true);
        let hits = draw(&mut submit, area, "hello");
        let outcome = submit.mouse(
            &hits,
            MouseEvent::new(MouseEventKind::Down(MouseButton::Left), Point::new(0, 0)),
            true,
        );
        assert!(outcome.handled);
        assert!(!submit.open);
        click(&mut submit, &hits, MORE, true);
        let outcome = submit.mouse(
            &HitMap::new(),
            MouseEvent::new(
                MouseEventKind::Up(MouseButton::Left),
                point(&hits, "composer.send.menu.queue"),
            ),
            true,
        );
        assert!(outcome.placement.is_none());
    }

    #[test]
    fn tiny_layouts_and_unicode_editor_keep_visible_hits_bounded() {
        for width in [0, 1, 2, 5, 12, 18, 40, 80] {
            for height in [0, 1, 2, 3, 4, 8, 20] {
                let mut submit = ComposerSubmit::default();
                let area = Rect::new(0, 0, width, height);
                let hits = draw(&mut submit, area, "世界 👩‍💻 e\u{301}\nsecond line");
                for hit in hits.regions() {
                    assert_eq!(hit.area.intersection(area), hit.area);
                }
                if width <= 12 || height < 3 {
                    assert!(!hits.regions().iter().any(|hit| hit.id.as_str() == SEND));
                }
                if hits.regions().iter().any(|hit| hit.id.as_str() == MORE) {
                    click(&mut submit, &hits, MORE, true);
                    let hits = draw(&mut submit, area, "世界 👩‍💻 e\u{301}");
                    for hit in hits.regions() {
                        assert_eq!(hit.area.intersection(area), hit.area);
                    }
                }
            }
        }
    }
}
