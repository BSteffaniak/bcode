//! BMUX-native review controls, retained behind the app's mutex.
//!
//! Each frame stores accepted BMUX layouts; events use those same layouts and
//! clipping contexts. Application mutation happens only after releasing the lock.

use std::cell::{Cell, RefCell};

use bmux_keyboard::KeyCode;
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId, LayoutNode,
};
use bmux_tui::composition::{Column, Row};
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::paint::PaintCx;
use bmux_tui_components::action_row::{
    ActionButton, ActionRow, ActionRowComponent, ActionRowOutcome, ActionRowPolicy, ActionRowState,
    ActionRowStyles,
};
use bmux_tui_components::tab_bar::{
    TabBar, TabBarComponent, TabBarOutcome, TabBarPolicy, TabBarState, TabBarStyles, TabItem,
};

use crate::code_review_tui::{
    ReviewCommentAction, ReviewMouseAction, ReviewSidebarMode, ReviewThreadFilter,
};

const MODES: [(ReviewSidebarMode, &str, &str); 7] = [
    (ReviewSidebarMode::Included, "included", "Included"),
    (ReviewSidebarMode::Repository, "repository", "Repository"),
    (ReviewSidebarMode::Threads, "threads", "Threads"),
    (ReviewSidebarMode::General, "general", "General"),
    (ReviewSidebarMode::Summary, "summary", "Summary"),
    (ReviewSidebarMode::Sources, "sources", "Sources"),
    (ReviewSidebarMode::NeedsAttention, "attention", "Attention"),
];

/// Existing review action surfaces; each has independent retained interaction state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewActionGroup {
    /// Workspace header actions (tabs are composed separately).
    Header,
    /// Selected-thread actions.
    Thread,
    /// Thread list filter and new-review-note actions.
    Filters,
    /// Header thread-status shortcuts (counts remain caller-owned display data).
    ThreadStatus,
    /// Creating a comment: draft, ask, publish, cancel.
    CreateComment,
    /// Editing a comment: save and cancel.
    EditComment,
}

impl ReviewActionGroup {
    const fn index(self) -> usize {
        match self {
            Self::Header => 0,
            Self::Thread => 1,
            Self::Filters => 2,
            Self::CreateComment => 3,
            Self::ThreadStatus => 5,
            Self::EditComment => 4,
        }
    }

    const fn actions(self) -> &'static [(&'static str, ReviewMouseAction)] {
        use ReviewMouseAction as A;
        match self {
            Self::Header => &[
                ("File note", A::FileComment),
                ("+Review note", A::NewReviewComment),
            ],
            Self::Thread => &[
                ("Reply", A::ReplyThread),
                ("Resolve/reopen", A::ToggleThreadResolved),
                ("Edit", A::EditThread),
                ("Delete", A::DeleteThread),
                ("Session", A::OpenThreadSession),
                ("Ask", A::AskBcode),
            ],
            Self::Filters => &[
                ("All", A::ThreadFilter(ReviewThreadFilter::All)),
                ("Open", A::ThreadFilter(ReviewThreadFilter::Open)),
                ("Resolved", A::ThreadFilter(ReviewThreadFilter::Resolved)),
                ("+Review note", A::NewReviewComment),
            ],
            Self::ThreadStatus => &[
                ("Drafts", A::ShowDraftThreads),
                ("All threads", A::ShowAllThreads),
                ("Open threads", A::ShowOpenThreads),
                ("Resolved/hidden", A::ShowResolvedThreads),
                ("Cycle filter", A::CycleThreadFilter),
            ],
            Self::CreateComment => &[
                (
                    "Save draft",
                    A::CommentAction(ReviewCommentAction::SaveDraft),
                ),
                ("Ask Bcode", A::CommentAction(ReviewCommentAction::AskBcode)),
                ("Publish", A::CommentAction(ReviewCommentAction::Publish)),
                ("Cancel", A::CancelComment),
            ],
            Self::EditComment => &[
                (
                    "Save edit",
                    A::CommentAction(ReviewCommentAction::SaveDraft),
                ),
                ("Cancel", A::CancelComment),
            ],
        }
    }
}

/// Ephemeral controls state, never canonical review state or declarative configuration.
#[derive(Debug, Default)]
pub struct ReviewControls {
    tabs: RefCell<TabBarState>,
    rows: [Cell<ActionRowState>; 6],
    focus: Cell<Option<ControlFocus>>,
    pending: Cell<Option<ReviewMouseAction>>,
    accepted: Vec<AcceptedControls>,
    modal: bool,
}

#[derive(Debug)]
struct AcceptedControls {
    surface: ControlSurface,
    area: Rect,
    layout: LayoutNode,
    styles: ActionRowStyles,
    tab_styles: TabBarStyles,
}

#[derive(Debug, Clone, Copy)]
pub enum ControlSurface {
    /// Sidebar tabs above workspace actions, composed with measured BMUX rows.
    Header,
    /// One concrete action row.
    Actions(ReviewActionGroup),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlFocus {
    Tabs,
    Actions(ReviewActionGroup),
}

impl ReviewControls {
    /// Invalidate geometry after resize or an action changes the rendered surface.
    pub fn invalidate(&mut self) {
        self.suspend();
        self.accepted.clear();
    }

    /// Drop accepted geometry after semantic changes, retaining local keyboard focus.
    pub fn discard_layout(&mut self) {
        self.accepted.clear();
    }

    /// Whether the accepted scene belongs to a modal.
    #[must_use]
    pub const fn is_modal(&self) -> bool {
        self.modal
    }

    /// Start a fresh accepted scene. Modal transitions cancel background presses.
    pub fn begin_frame(&mut self, modal: bool) {
        if self.modal != modal {
            self.suspend();
        }
        self.modal = modal;
        self.accepted.clear();
    }

    /// Paint and retain exactly the layout subsequently used for event dispatch.
    pub fn paint_surface(
        &mut self,
        surface: ControlSurface,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
        styles: ActionRowStyles,
        tab_styles: TabBarStyles,
    ) {
        if area.is_empty() {
            return;
        }
        let layout = {
            let component = self.surface(surface, styles, tab_styles);
            let layout = component.layout(
                Constraints::tight(bmux_tui::geometry::Size::new(area.width, area.height)),
                &mut LayoutCx::new(),
            );
            frame.with_child(
                i32::from(area.x),
                i64::from(area.y),
                bmux_tui::paint::LocalRect::new(0, 0, area.width, area.height),
                |cx| component.paint(&layout, cx),
            );
            layout
        };
        let is_comment = matches!(
            surface,
            ControlSurface::Actions(
                ReviewActionGroup::CreateComment | ReviewActionGroup::EditComment
            )
        );
        if self.modal == is_comment {
            self.accepted.push(AcceptedControls {
                surface,
                area,
                layout,
                styles,
                tab_styles,
            });
        }
    }

    fn surface(
        &self,
        surface: ControlSurface,
        styles: ActionRowStyles,
        tab_styles: TabBarStyles,
    ) -> Box<dyn Component + '_> {
        match surface {
            ControlSurface::Header => Box::new(
                Column::new()
                    .id("review.header.controls")
                    .child(self.tabs("review.tabs", tab_styles))
                    .child(
                        Row::new()
                            .id("review.header.action-row")
                            .child(self.actions(
                                "review.header.actions",
                                ReviewActionGroup::Header,
                                styles,
                            ))
                            .child(self.actions(
                                "review.header.status",
                                ReviewActionGroup::ThreadStatus,
                                styles,
                            )),
                    ),
            ),
            ControlSurface::Actions(group) => {
                Box::new(self.actions(format!("review.actions.{}", group.index()), group, styles))
            }
        }
    }

    // Broadcast pointer feedback across siblings: BMUX containers short-circuit
    // handled children, but a previous sibling may also need its hover cleared.
    fn header_event(
        &self,
        event: &Event,
        layout: &LayoutNode,
        cx: &mut EventCx<'_>,
        styles: ActionRowStyles,
        tabs: TabBarStyles,
    ) -> EventOutcome {
        let mut outcome = EventOutcome::Ignored;
        for child in &layout.children {
            outcome = outcome.merge(cx.with_child(child, |cx| {
                if child.node.id.as_str() == "review.tabs" {
                    self.tabs("review.tabs", tabs).event(event, &child.node, cx)
                } else {
                    let mut result = EventOutcome::Ignored;
                    for action in &child.node.children {
                        let group = if action.node.id.as_str() == "review.header.actions" {
                            ReviewActionGroup::Header
                        } else {
                            ReviewActionGroup::ThreadStatus
                        };
                        result = result.merge(cx.with_child(action, |cx| {
                            self.actions(action.node.id.clone(), group, styles).event(
                                event,
                                &action.node,
                                cx,
                            )
                        }));
                    }
                    result
                }
            }));
        }
        outcome
    }

    fn traverse_focus(&self, backwards: bool) {
        let mut order = Vec::new();
        for accepted in &self.accepted {
            let mut cx = EventCx::new(&accepted.layout);
            cx.with_transform(
                0,
                0,
                i32::from(accepted.area.x),
                i64::from(accepted.area.y),
                accepted.area,
                |cx| {
                    let candidates = match accepted.surface {
                        ControlSurface::Header => vec![
                            ("review.tabs".to_owned(), ControlFocus::Tabs),
                            (
                                "review.header.actions".to_owned(),
                                ControlFocus::Actions(ReviewActionGroup::Header),
                            ),
                            (
                                "review.header.status".to_owned(),
                                ControlFocus::Actions(ReviewActionGroup::ThreadStatus),
                            ),
                        ],
                        ControlSurface::Actions(group) => vec![(
                            format!("review.actions.{}", group.index()),
                            ControlFocus::Actions(group),
                        )],
                    };
                    for (id, focus) in candidates {
                        if cx
                            .find_visible_rect(&LayoutId::new(id))
                            .is_some_and(|area| !area.is_empty())
                        {
                            order.push(focus);
                        }
                    }
                },
            );
        }
        let current = self
            .focus
            .get()
            .and_then(|focus| order.iter().position(|item| *item == focus));
        let next = if backwards {
            current.map_or_else(|| order.len().checked_sub(1), |index| index.checked_sub(1))
        } else {
            Some(current.map_or(0, |index| index + 1))
        };
        match next.and_then(|index| order.get(index)) {
            Some(ControlFocus::Tabs) => self.focus_tabs(),
            Some(ControlFocus::Actions(group)) => self.focus_actions(*group),
            None => self.clear_focus(),
        }
    }

    /// Dispatch only through the last painted scene. None means normal app routing.
    pub fn route(&self, event: &Event) -> Option<(bool, Option<ReviewMouseAction>)> {
        if self.accepted.is_empty() {
            return None;
        }
        if matches!(event, Event::Mouse(mouse) if matches!(mouse.kind,
            bmux_tui::event::MouseEventKind::ScrollUp | bmux_tui::event::MouseEventKind::ScrollDown
            | bmux_tui::event::MouseEventKind::ScrollLeft | bmux_tui::event::MouseEventKind::ScrollRight))
        {
            return None;
        }
        if let Event::Key(stroke) = event {
            // The editor owns all typing, action cycling, submit and cancel keys.
            // Its footer remains pointer-operable without stealing input focus.
            if self.modal {
                return None;
            }
            if stroke.key == KeyCode::Tab
                && !stroke.modifiers.ctrl
                && !stroke.modifiers.alt
                && !stroke.modifiers.super_key
                && !stroke.modifiers.hyper
                && !stroke.modifiers.meta
            {
                self.traverse_focus(stroke.modifiers.shift);
                return Some((true, None));
            }
            if stroke.key == KeyCode::Escape
                && self.focus.get().is_some()
                && stroke.modifiers.is_empty()
            {
                self.clear_focus();
                return Some((true, None));
            }
            // Every other key still reaches review keybindings/editor text handling.
            if !matches!(
                stroke.key,
                KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Home
                    | KeyCode::End
                    | KeyCode::Enter
                    | KeyCode::Space
                    | KeyCode::Char(' ')
            ) {
                return None;
            }
        }
        let pointer_inside = match event {
            Event::Mouse(mouse) => self
                .accepted
                .iter()
                .any(|item| item.area.contains(mouse.position)),
            _ => false,
        };
        let previous_focus = self.focus.get();
        if let Event::Mouse(mouse) = event
            && matches!(mouse.kind, bmux_tui::event::MouseEventKind::Down(_))
            && !pointer_inside
        {
            // A new press outside controls belongs to its destination pane.
            // Cancel our old gesture without consuming the new one merely
            // because clearing focus/pressed state changes presentation.
            self.suspend();
            return None;
        }
        let mut handled = false;
        let mut redraw = false;
        for accepted in &self.accepted {
            let component = self.surface(accepted.surface, accepted.styles, accepted.tab_styles);
            let mut cx = EventCx::new(&accepted.layout);
            let outcome = cx.with_transform(
                0,
                0,
                i32::from(accepted.area.x),
                i64::from(accepted.area.y),
                accepted.area,
                |cx| match accepted.surface {
                    ControlSurface::Header => self.header_event(
                        event,
                        &accepted.layout,
                        cx,
                        accepted.styles,
                        accepted.tab_styles,
                    ),
                    ControlSurface::Actions(_) => component.event(event, &accepted.layout, cx),
                },
            );
            handled |= outcome.is_handled();
            redraw |= outcome == EventOutcome::Redraw;
        }
        redraw |= previous_focus != self.focus.get();
        let action = self.take_action();
        if handled || pointer_inside || action.is_some() {
            Some((redraw, action))
        } else {
            None
        }
    }

    /// Reflect the editor's existing keyboard-selected submit action without
    /// taking keyboard focus away from the text field.
    pub fn sync_comment_action(&self, group: ReviewActionGroup, action: ReviewCommentAction) {
        let index = group
            .actions()
            .iter()
            .position(|(_, candidate)| *candidate == ReviewMouseAction::CommentAction(action));
        let row = &self.rows[group.index()];
        let mut state = row.get();
        state.set_focused(index);
        state.interaction.focused = self.focus.get() == Some(ControlFocus::Actions(group));
        row.set(state);
    }

    /// Synchronize the canonical sidebar selection before composing each frame.
    pub fn sync_sidebar(&self, mode: ReviewSidebarMode) {
        self.tabs
            .borrow_mut()
            .set_selected(MODES.iter().position(|entry| entry.0 == mode));
    }

    /// Explicit local keyboard focus, normally entered by the host's focus traversal.
    pub fn focus_tabs(&self) {
        self.clear_focus();
        self.focus.set(Some(ControlFocus::Tabs));
        self.tabs.borrow_mut().set_focused(true);
    }

    /// Focus the first action in a concrete surface without activating it.
    pub fn focus_actions(&self, group: ReviewActionGroup) {
        self.clear_focus();
        self.focus.set(Some(ControlFocus::Actions(group)));
        let row = &self.rows[group.index()];
        let mut state = row.get();
        state.set_focused(Some(0));
        state.interaction.focused = true;
        row.set(state);
    }

    /// Leave local controls before returning keys to content or a modal editor.
    pub fn clear_focus(&self) {
        self.focus.set(None);
        self.tabs.borrow_mut().set_focused(false);
        for row in &self.rows {
            let mut state = row.get();
            state.set_focused(None);
            state.interaction.focused = false;
            row.set(state);
        }
    }

    /// Discard stale hover/press state when controls disappear, a modal replaces
    /// them, the viewport changes, or terminal focus is lost. Preserve selection.
    pub fn suspend(&self) {
        let selected = self.tabs.borrow().selected();
        *self.tabs.borrow_mut() = TabBarState::new(selected);
        for row in &self.rows {
            row.set(ActionRowState::new());
        }
        self.focus.set(None);
        self.pending.set(None);
    }

    /// Take the semantic action after dispatch, before accepting another input event.
    pub fn take_action(&self) -> Option<ReviewMouseAction> {
        self.pending.take()
    }

    /// Construct the sidebar tab child. Its identity must be stable across frames.
    #[must_use]
    pub fn tabs(&self, id: impl Into<LayoutId>, styles: TabBarStyles) -> ReviewTabs<'_> {
        ReviewTabs {
            id: id.into(),
            controls: self,
            items: MODES
                .iter()
                .map(|(_, id, label)| TabItem::new(id, label))
                .collect(),
            styles,
        }
    }

    /// Construct one action child. Do not compose the same group twice in one tree.
    #[must_use]
    pub fn actions(
        &self,
        id: impl Into<LayoutId>,
        group: ReviewActionGroup,
        styles: ActionRowStyles,
    ) -> ReviewActions<'_> {
        ReviewActions {
            id: id.into(),
            controls: self,
            group,
            buttons: group
                .actions()
                .iter()
                .enumerate()
                .map(|(index, (label, _))| ActionButton::new(index.to_string(), *label))
                .collect(),
            styles,
        }
    }
}

/// Concrete sidebar tabs; native BMUX owns layout, paint, semantics and hit geometry.
pub struct ReviewTabs<'a> {
    id: LayoutId,
    controls: &'a ReviewControls,
    items: Vec<TabItem<'static>>,
    styles: TabBarStyles,
}

impl ReviewTabs<'_> {
    fn native(&self) -> TabBarComponent<'_, '_> {
        TabBarComponent::new(self.id.clone(), &self.items, &self.controls.tabs)
            .policy(TabBarPolicy::interactive())
            .styles(self.styles)
    }

    fn dispatch(&self, area: Rect, event: &Event) -> EventOutcome {
        if area.is_empty()
            || (matches!(event, Event::Key(_))
                && self.controls.focus.get() != Some(ControlFocus::Tabs))
        {
            return EventOutcome::Ignored;
        }
        let outcome = TabBar::new(&self.items)
            .policy(TabBarPolicy::interactive())
            .handle_event(area, &mut self.controls.tabs.borrow_mut(), event);
        match outcome {
            TabBarOutcome::Ignored => EventOutcome::Ignored,
            TabBarOutcome::Selected(index) => {
                if let Some((mode, _, _)) = MODES.get(index) {
                    self.controls
                        .pending
                        .set(Some(ReviewMouseAction::SidebarMode(*mode)));
                }
                self.controls.focus_tabs();
                EventOutcome::Redraw
            }
            TabBarOutcome::Redraw => {
                if matches!(event, Event::Mouse(mouse) if matches!(mouse.kind,
                    bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left)))
                {
                    self.controls.focus_tabs();
                }
                EventOutcome::Redraw
            }
            TabBarOutcome::Close(_) => EventOutcome::Redraw,
        }
    }
}

impl Component for ReviewTabs<'_> {
    fn revision(&self) -> ComponentRevision {
        self.native().revision()
    }
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.native().layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.native().paint(layout, cx);
    }
    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some((area, event)) = local_event(event, layout, cx) else {
            return EventOutcome::Ignored;
        };
        self.dispatch(area, &event)
    }
}

/// Concrete review action row, backed by BMUX buttons and retained row state.
pub struct ReviewActions<'a> {
    id: LayoutId,
    controls: &'a ReviewControls,
    group: ReviewActionGroup,
    buttons: Vec<ActionButton>,
    styles: ActionRowStyles,
}

impl ReviewActions<'_> {
    fn native(&self) -> ActionRowComponent<'_, '_> {
        ActionRowComponent::new(
            self.id.clone(),
            &self.buttons,
            &self.controls.rows[self.group.index()],
        )
        .policy(ActionRowPolicy::roving())
        .styles(self.styles)
    }

    fn dispatch(&self, area: Rect, event: &Event) -> EventOutcome {
        if area.is_empty()
            || (matches!(event, Event::Key(_))
                && self.controls.focus.get() != Some(ControlFocus::Actions(self.group)))
        {
            return EventOutcome::Ignored;
        }
        let cell = &self.controls.rows[self.group.index()];
        let mut state = cell.get();
        let outcome = ActionRow::new(&self.buttons)
            .policy(ActionRowPolicy::roving())
            .handle_event(area, &mut state, event);
        cell.set(state);
        match outcome {
            ActionRowOutcome::Activated { index, .. } => {
                if let Some((_, action)) = self.group.actions().get(index) {
                    self.controls.pending.set(Some(*action));
                }
                EventOutcome::Redraw
            }
            ActionRowOutcome::FocusRequested { index } => {
                self.controls.focus_actions(self.group);
                let mut state = cell.get();
                state.set_focused(Some(index));
                cell.set(state);
                EventOutcome::Redraw
            }
            ActionRowOutcome::Ignored => EventOutcome::Ignored,
            ActionRowOutcome::Handled => {
                if matches!(event, Event::Mouse(mouse) if !area.contains(mouse.position)) {
                    EventOutcome::Ignored
                } else {
                    EventOutcome::Handled
                }
            }
            ActionRowOutcome::Redraw | ActionRowOutcome::FocusMoved { .. } => EventOutcome::Redraw,
        }
    }
}

impl Component for ReviewActions<'_> {
    fn revision(&self) -> ComponentRevision {
        self.native().revision()
    }
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.native().layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.native().paint(layout, cx);
    }
    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some((area, event)) = local_event(event, layout, cx) else {
            return EventOutcome::Ignored;
        };
        self.dispatch(area, &event)
    }
}

// Translate through the accepted component context, not a stored screen rectangle.
// Native hit generation uses this same local layout size. Clipped-away cells cannot
// activate controls, including when a parent scrolls/clips the left side of a row.
fn local_event(event: &Event, layout: &LayoutNode, cx: &EventCx<'_>) -> Option<(Rect, Event)> {
    let visible = cx.find_visible_rect(&layout.id)?;
    if visible.is_empty() {
        return None;
    }
    let area = Rect::new(
        0,
        0,
        layout.size.width.try_into().unwrap_or(u16::MAX),
        layout.size.height.try_into().unwrap_or(u16::MAX),
    );
    let event = if let Event::Mouse(mouse) = event {
        let mut mouse = *mouse;
        let (x, y) = cx.local_point(mouse.position);
        mouse.position = if visible.contains(mouse.position) {
            Point::new(u16::try_from(x).ok()?, u16::try_from(y).ok()?)
        } else {
            Point::new(u16::MAX, u16::MAX)
        };
        Event::Mouse(mouse)
    } else {
        event.clone()
    };
    Some((area, event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::geometry::Size;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyStroke::simple(code))
    }

    fn mouse(kind: MouseEventKind, x: u16) -> Event {
        Event::Mouse(MouseEvent::new(kind, Point::new(x, 0)))
    }

    fn dispatch(component: &impl Component, width: u16, event: &Event) -> EventOutcome {
        let layout = component.layout(
            Constraints::tight(Size::new(width, 1)),
            &mut LayoutCx::new(),
        );
        component.event(event, &layout, &mut EventCx::new(&layout))
    }

    #[test]
    fn accepted_scene_routes_shifted_header_and_modal_isolates_background() {
        let mut controls = ReviewControls::default();
        let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 140, 20));
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
        controls.begin_frame(false);
        controls.paint_surface(
            ControlSurface::Header,
            Rect::new(3, 4, 130, 2),
            &mut PaintCx::new(&mut frame),
            ActionRowStyles::default(),
            TabBarStyles::default(),
        );
        let tab = &controls.accepted[0].layout.children[0];
        let hits = TabBar::new(&controls.tabs("review.tabs", TabBarStyles::default()).items)
            .hit_rects(Rect::new(0, 0, 130, 1));
        let x = 3 + u16::try_from(tab.x).unwrap() + hits[1].x;
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            let routed = controls.route(&Event::Mouse(MouseEvent::new(kind, Point::new(x, 4))));
            if matches!(kind, MouseEventKind::Up(_)) {
                assert_eq!(
                    routed.and_then(|(_, action)| action),
                    Some(ReviewMouseAction::SidebarMode(
                        ReviewSidebarMode::Repository
                    ))
                );
            }
        }
        controls.begin_frame(true);
        controls.paint_surface(
            ControlSurface::Header,
            Rect::new(3, 4, 130, 2),
            &mut PaintCx::new(&mut frame),
            ActionRowStyles::default(),
            TabBarStyles::default(),
        );
        assert!(controls.accepted.is_empty());
        controls.paint_surface(
            ControlSurface::Actions(ReviewActionGroup::CreateComment),
            Rect::new(3, 10, 100, 1),
            &mut PaintCx::new(&mut frame),
            ActionRowStyles::default(),
            TabBarStyles::default(),
        );
        assert_eq!(controls.route(&key(KeyCode::Enter)), None);
        assert_eq!(controls.route(&key(KeyCode::Tab)), None);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            let routed = controls.route(&Event::Mouse(MouseEvent::new(kind, Point::new(4, 10))));
            if matches!(kind, MouseEventKind::Up(_)) {
                assert_eq!(
                    routed.and_then(|(_, action)| action),
                    Some(ReviewMouseAction::CommentAction(
                        ReviewCommentAction::SaveDraft
                    ))
                );
            }
        }
        controls.invalidate();
        assert_eq!(controls.route(&key(KeyCode::Tab)), None);
    }

    #[test]
    fn focus_traversal_returns_to_content_and_preserves_other_keys() {
        let mut controls = ReviewControls::default();
        let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 140, 3));
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
        controls.paint_surface(
            ControlSurface::Header,
            Rect::new(0, 0, 140, 2),
            &mut PaintCx::new(&mut frame),
            ActionRowStyles::default(),
            TabBarStyles::default(),
        );
        for expected in [
            Some(ControlFocus::Tabs),
            Some(ControlFocus::Actions(ReviewActionGroup::Header)),
            Some(ControlFocus::Actions(ReviewActionGroup::ThreadStatus)),
            None,
        ] {
            assert_eq!(controls.route(&key(KeyCode::Tab)), Some((true, None)));
            assert_eq!(controls.focus.get(), expected);
        }
        assert_eq!(controls.route(&key(KeyCode::Char('c'))), None);
    }

    #[test]
    fn sidebar_pointer_and_keyboard_use_existing_semantic_actions() {
        let controls = ReviewControls::default();
        controls.sync_sidebar(ReviewSidebarMode::Included);
        let tabs = controls.tabs("review.tabs", TabBarStyles::default());
        let hits = TabBar::new(&tabs.items).hit_rects(Rect::new(0, 0, 100, 1));
        let x = hits[1].x;
        dispatch(
            &tabs,
            100,
            &mouse(MouseEventKind::Down(MouseButton::Left), x),
        );
        dispatch(&tabs, 100, &mouse(MouseEventKind::Up(MouseButton::Left), x));
        assert_eq!(
            controls.take_action(),
            Some(ReviewMouseAction::SidebarMode(
                ReviewSidebarMode::Repository
            ))
        );
        dispatch(&tabs, 100, &key(KeyCode::Right));
        assert_eq!(
            controls.take_action(),
            Some(ReviewMouseAction::SidebarMode(ReviewSidebarMode::Threads))
        );
        controls.clear_focus();
        assert_eq!(
            dispatch(&tabs, 100, &key(KeyCode::Right)),
            EventOutcome::Ignored
        );
        assert_eq!(controls.take_action(), None);
    }

    #[test]
    fn actions_pointer_keyboard_and_hover_feedback() {
        let controls = ReviewControls::default();
        let row = controls.actions(
            "review.header",
            ReviewActionGroup::Header,
            ActionRowStyles::default(),
        );
        assert_eq!(
            dispatch(&row, 80, &mouse(MouseEventKind::Move, 1)),
            EventOutcome::Redraw
        );
        assert_ne!(
            dispatch(&row, 80, &mouse(MouseEventKind::Move, 1)),
            EventOutcome::Redraw
        );
        assert_eq!(controls.take_action(), None);
        dispatch(&row, 80, &mouse(MouseEventKind::Down(MouseButton::Left), 1));
        dispatch(&row, 80, &mouse(MouseEventKind::Up(MouseButton::Left), 1));
        assert_eq!(controls.take_action(), Some(ReviewMouseAction::FileComment));
        controls.focus_actions(ReviewActionGroup::Header);
        dispatch(&row, 80, &key(KeyCode::Right));
        dispatch(&row, 80, &key(KeyCode::Enter));
        assert_eq!(
            controls.take_action(),
            Some(ReviewMouseAction::NewReviewComment)
        );
    }

    #[test]
    fn tiny_unicode_rows_share_native_layout_and_hit_geometry() {
        for width in [0, 1, 2, 3, 8, 32] {
            let controls = ReviewControls::default();
            let mut row = controls.actions(
                "unicode",
                ReviewActionGroup::Header,
                ActionRowStyles::default(),
            );
            row.buttons[0].label = "界👩‍💻e\u{301}".into();
            let layout = row.layout(
                Constraints::tight(Size::new(width, 1)),
                &mut LayoutCx::new(),
            );
            assert_eq!(layout.size.width, u64::from(width));
            let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, width, 1));
            let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
            row.paint(&layout, &mut PaintCx::new(&mut frame));
            let painted_hits = frame.hits().regions().to_vec();
            let areas = ActionRow::new(&row.buttons).action_areas(Rect::new(0, 0, width, 1));
            assert!(areas.iter().all(|rect| rect.right() <= width));
            assert_eq!(
                painted_hits.iter().map(|hit| hit.area).collect::<Vec<_>>(),
                areas
            );
            for x in 0..=width {
                controls.pending.set(None);
                dispatch(
                    &row,
                    width,
                    &mouse(MouseEventKind::Down(MouseButton::Left), x),
                );
                dispatch(
                    &row,
                    width,
                    &mouse(MouseEventKind::Up(MouseButton::Left), x),
                );
                let expected = areas
                    .iter()
                    .position(|rect| rect.contains(Point::new(x, 0)))
                    .map(|index| row.group.actions()[index].1);
                assert_eq!(controls.take_action(), expected, "width={width}, x={x}");
            }
        }
    }

    #[test]
    fn new_press_outside_controls_cancels_stale_press_without_consuming_destination() {
        let mut controls = ReviewControls::default();
        let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 140, 20));
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
        controls.paint_surface(
            ControlSurface::Header,
            Rect::new(0, 0, 140, 2),
            &mut PaintCx::new(&mut frame),
            ActionRowStyles::default(),
            TabBarStyles::default(),
        );
        let press = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(2, 0),
        ));
        assert!(controls.route(&press).is_some());
        assert_eq!(controls.focus.get(), Some(ControlFocus::Tabs));
        let outside = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(90, 5),
        ));
        assert_eq!(
            controls.route(&outside),
            None,
            "destination must receive this press"
        );
        assert_eq!(controls.focus.get(), None);
        let release = Event::Mouse(MouseEvent::new(
            MouseEventKind::Up(MouseButton::Left),
            Point::new(2, 0),
        ));
        assert_eq!(
            controls.route(&release).and_then(|(_, action)| action),
            None,
            "the cancelled press must not activate on a later release"
        );
        assert_eq!(controls.take_action(), None);
    }

    #[test]
    fn suspension_and_release_outside_cancel_activation() {
        let controls = ReviewControls::default();
        controls.sync_sidebar(ReviewSidebarMode::Summary);
        let row = controls.actions(
            "header",
            ReviewActionGroup::Header,
            ActionRowStyles::default(),
        );
        dispatch(&row, 80, &mouse(MouseEventKind::Down(MouseButton::Left), 1));
        dispatch(&row, 80, &mouse(MouseEventKind::Up(MouseButton::Left), 80));
        assert_eq!(controls.take_action(), None);
        dispatch(&row, 80, &mouse(MouseEventKind::Down(MouseButton::Left), 1));
        controls.suspend();
        dispatch(&row, 80, &mouse(MouseEventKind::Up(MouseButton::Left), 1));
        assert_eq!(controls.take_action(), None);
        assert_eq!(controls.tabs.borrow().selected(), Some(4));
    }

    #[test]
    fn tab_hover_changes_once_and_unicode_hits_match_paint() {
        for width in [0, 1, 2, 3, 9, 32] {
            let controls = ReviewControls::default();
            let mut tabs = controls.tabs("tabs", TabBarStyles::default());
            tabs.items[0].label = bmux_tui::text::Line::from("界👩‍💻e\u{301}");
            let layout = tabs.layout(
                Constraints::tight(Size::new(width, 1)),
                &mut LayoutCx::new(),
            );
            let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, width, 1));
            let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
            tabs.paint(&layout, &mut PaintCx::new(&mut frame));
            // TabBar publishes a parent focus region before its item regions.
            let hits = frame
                .hits()
                .regions()
                .iter()
                .filter(|hit| hit.id.as_str() != "tabs")
                .cloned()
                .collect::<Vec<_>>();
            assert!(hits.iter().all(|hit| hit.area.right() <= width));
            for x in 0..=width {
                dispatch(
                    &tabs,
                    width,
                    &mouse(MouseEventKind::Down(MouseButton::Left), x),
                );
                dispatch(
                    &tabs,
                    width,
                    &mouse(MouseEventKind::Up(MouseButton::Left), x),
                );
                let expected = hits
                    .iter()
                    .position(|hit| hit.area.contains(Point::new(x, 0)))
                    .map(|index| ReviewMouseAction::SidebarMode(MODES[index].0));
                assert_eq!(controls.take_action(), expected, "width={width}, x={x}");
            }
        }
        let controls = ReviewControls::default();
        let tabs = controls.tabs("tabs", TabBarStyles::default());
        assert_eq!(
            dispatch(&tabs, 80, &mouse(MouseEventKind::Move, 1)),
            EventOutcome::Redraw
        );
        assert_eq!(
            dispatch(&tabs, 80, &mouse(MouseEventKind::Move, 1)),
            EventOutcome::Ignored
        );
        assert_eq!(controls.take_action(), None);
    }

    #[test]
    fn clipped_translation_does_not_retarget_hidden_first_action() {
        let controls = ReviewControls::default();
        let row = controls.actions(
            "translated",
            ReviewActionGroup::Header,
            ActionRowStyles::default(),
        );
        let areas = ActionRow::new(&row.buttons).action_areas(Rect::new(0, 0, 80, 1));
        let offset = i32::from(areas[1].x);
        let layout = row.layout(Constraints::tight(Size::new(80, 1)), &mut LayoutCx::new());
        let mut cx = EventCx::new(&layout);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            cx.with_transform(0, 0, -offset, 0, Rect::new(0, 0, 5, 1), |cx| {
                row.event(&mouse(kind, 1), &layout, cx)
            });
        }
        assert_eq!(
            controls.take_action(),
            Some(ReviewMouseAction::NewReviewComment)
        );
    }
}
