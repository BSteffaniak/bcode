//! BMUX-owned measured picker chrome, clipping and item interaction for composer contributors.

use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui_components::picker_frame::{
    PickerFrame, PickerFrameComponent, PickerFramePolicy, PickerFrameStyles,
};
use bmux_tui_components::selectable_list::{SelectableListComponent, SelectableListStyles};

use super::composer_interaction::ComposerInteraction;
use super::render::TuiTheme;

pub fn paint(
    state: &mut ComposerInteraction,
    composer: Rect,
    cx: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
) {
    state.painted_list = None;
    state.painted_layout = None;
    if !state.visible() {
        return;
    }
    // Responsive presentation policy only; all chrome, list geometry and clipping are measured.
    let bounds = cx.clip();
    let available = bounds;
    let field = state.focused_field().map_or(composer, |field| field.anchor);
    let field = Rect::new(
        field.x.saturating_sub(bounds.x),
        field.y.saturating_sub(bounds.y),
        field.width,
        field.height,
    );
    if available.is_empty() {
        return;
    }
    state.completion_control.reconcile(&state.items);
    let list_state = state.completion_control.list_state();
    let list = SelectableListComponent::new("composer.list", &state.items, list_state).styles(
        SelectableListStyles {
            background: theme.raised,
            normal: theme.raised,
            focused: theme.raised.patch(theme.selection),
            selected: theme.raised.patch(theme.selection),
            ..SelectableListStyles::default()
        },
    );
    let shell = PickerFrame::new()
        .title(state.status())
        .policy(PickerFramePolicy {
            chrome: true,
            background: true,
            header: false,
            input: false,
            footer: false,
            margin: Insets::all(0),
            padding: Insets::new(0, 1, 0, 1),
            min_size: Size::new(0, 0),
            max_size: Size::new(available.width.min(88), available.height.min(8)),
            placement: bmux_tui_components::picker_frame::PickerFramePlacement::FieldAnchored {
                field,
                prefer_below: false,
            },
        })
        .styles(PickerFrameStyles {
            border: theme.raised.patch(theme.focused),
            background: theme.raised,
            header: theme.raised,
            input: theme.raised,
            list: theme.raised,
            footer: theme.raised,
        });
    let component = PickerFrameComponent::new("composer", shell, list);
    let layout = component.layout(Constraints::tight(available.size()), &mut LayoutCx::new());
    cx.with_child(
        i32::from(available.x),
        i64::from(available.y),
        LocalRect::new(0, 0, available.width, available.height),
        |cx| component.paint(&layout, cx),
    );
    // Hit routing uses the SAME component's measured slot, not independently guessed borders.
    if let Some(panel) = layout.children.first()
        && let Some(slot) = panel
            .node
            .children
            .iter()
            .find(|child| child.node.id.as_str() == "composer.list")
        && let (Ok(x), Ok(y)) = (
            u16::try_from(panel.x.saturating_add(slot.x)),
            u16::try_from(panel.y.saturating_add(slot.y)),
        )
    {
        let list_area = Rect::new(
            available.x.saturating_add(x),
            available.y.saturating_add(y),
            u16::try_from(slot.node.size.width).unwrap_or(u16::MAX),
            u16::try_from(slot.node.size.height).unwrap_or(u16::MAX),
        )
        .intersection(available);
        if !list_area.is_empty() {
            state.painted_layout = Some(slot.node.clone());
            state.painted_list = Some(list_area);
        }
    }
    state.list = list_state.get();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer_interaction::tests::{begin, completion};
    use bmux_text_edit::TextEditBuffer;
    use bmux_tui::buffer::Buffer;
    use bmux_tui::event::{Event, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Point;
    use bmux_tui_components::selectable_list::SelectableListOutcome;

    #[test]
    fn unicode_picker_resize_clips_hits_and_clears_old_geometry() {
        let mut state = ComposerInteraction::default();
        let buffer = TextEditBuffer::from_text("#主题👩‍💻a\u{301}");
        let request = begin(&mut state, &buffer);
        state.complete(completion(&request));
        for (width, height) in [(90, 18), (0, 0), (1, 1), (2, 5), (5, 3), (24, 10), (90, 18)] {
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);
            let mut frame = Frame::new(&mut buffer);
            paint(
                &mut state,
                Rect::new(0, height.saturating_sub(2), width, 2),
                &mut PaintCx::new(&mut frame),
                TuiTheme::for_theme_id("terminal-native"),
            );
            for hit in frame.hits().regions() {
                assert!(hit.area.right() <= area.right());
                assert!(hit.area.bottom() <= area.bottom());
                assert!(!hit.area.is_empty());
            }
            if width == 0 || height == 1 {
                assert!(state.painted_list.is_none());
            }
            if let Some(list) = state.painted_list {
                assert!(list.right() <= area.right());
                assert!(list.bottom() <= height.saturating_sub(2));
            }
        }
        let event = Event::Mouse(MouseEvent::new(MouseEventKind::Move, Point::new(0, 17)));
        assert_eq!(state.list_event(&event), SelectableListOutcome::Ignored);
        state.dismiss();
        assert!(state.painted_list.is_none());
        assert_eq!(state.list_event(&event), SelectableListOutcome::Ignored);
    }

    #[test]
    fn measured_list_navigation_and_dismissal_preserve_input_focus() {
        let mut state = ComposerInteraction::default();
        let buffer = TextEditBuffer::from_text("#topic");
        let request = begin(&mut state, &buffer);
        let mut reply = completion(&request);
        let candidates = &mut reply.snapshot.as_mut().unwrap().candidates;
        let mut second = candidates[0].clone();
        second.id = "second".into();
        second.label = "next 🦀".into();
        candidates.push(second);
        state.complete(reply);
        let area = Rect::new(0, 0, 90, 18);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        paint(
            &mut state,
            Rect::new(0, 16, 90, 2),
            &mut PaintCx::new(&mut frame),
            TuiTheme::for_theme_id("terminal-native"),
        );
        let (outcome, accept) =
            state.command(bmux_tui_components::completion::CompletionCommand::Next);
        assert_eq!(outcome, bmux_tui::event::EventOutcome::Redraw);
        assert!(!accept);
        assert_eq!(state.list.focused(), Some(1));
        assert_eq!(
            state.list_event(&Event::Key(bmux_keyboard::parse_key_stroke("x").unwrap())),
            SelectableListOutcome::Ignored
        );
        state.dismiss();
        assert_eq!(
            state.list_event(&Event::Key(
                bmux_keyboard::parse_key_stroke("down").unwrap()
            )),
            SelectableListOutcome::Ignored
        );
    }
}
