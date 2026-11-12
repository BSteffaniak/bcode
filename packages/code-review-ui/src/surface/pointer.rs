//! Pointer interaction over the document list: action clicks, the hovered line's comment
//! affordance, drag selection of line ranges, and keyed wheel motion.
//!
//! Items report what the pointer did through the list's routing (only visible, painted items
//! receive events, so hover and clicks always refer to what is on screen). A drag keeps going
//! when the pointer leaves the list: the list scrolls a row toward the pointer and the range
//! extends to the line at that edge.

use super::{Review, ReviewSurface, Selection};
use crate::action::Action;
use crate::composer::ComposerMode;
use crate::rows::{ItemAction, ItemClick, PointerHit, PointerKind, RowKey};
use bcode_code_review::document::{ItemKey, ReviewItem};
use bmux_tui::event::MouseEvent;
use bmux_tui::style::Easing;
use bmux_tui_components::virtual_list::{ListAnchor, NoSourceMap, ScrollTarget};
use std::time::{Duration, Instant};

/// Frame interval motions are sampled at.
const MOTION_FRAME: Duration = Duration::from_millis(16);

/// Advance the running wheel motion to now; forget its target once it completed.
pub(super) fn advance_motion(review: &mut Review) {
    let running = {
        let mut list = review.list.borrow_mut();
        if list.motion().is_some() {
            list.advance_motion(Instant::now(), MOTION_FRAME, &NoSourceMap);
        }
        list.motion().is_some()
    };
    if !running {
        review.wheel_target = None;
    }
}

/// Whether `key` is a diff line or row.
fn is_line(review: &Review, key: &ItemKey) -> bool {
    matches!(key, ItemKey::Line { .. } | ItemKey::LinePair { .. })
        && review.doc.index_of(key).is_some()
}

impl ReviewSurface {
    /// Wheel over the document: start, or extend, a keyed scroll motion by `rows`.
    pub(super) fn wheel(&mut self, rows: i64) -> bool {
        let duration = self.motion;
        let Some(review) = self.review.as_mut() else {
            return false;
        };
        let viewport = review.viewport_height.max(1);
        let target = {
            let mut list = review.list.borrow_mut();
            let Some(top) = list.resolved().map(|resolved| resolved.top_offset) else {
                return false;
            };
            let base = review.wheel_target.unwrap_or(top);
            let max = list.total_height().saturating_sub(viewport);
            let target = if rows < 0 {
                base.saturating_sub(rows.unsigned_abs())
            } else {
                base.saturating_add(rows.unsigned_abs())
            }
            .min(max);
            if target == base {
                return false;
            }
            let Some(key) = list.key_at_offset(target).cloned() else {
                return false;
            };
            let start = list.item_offset(&key).unwrap_or(target);
            list.start_motion(
                ScrollTarget::Anchor(ListAnchor::row(key, target.saturating_sub(start))),
                Easing::EaseOut,
                duration,
                Instant::now(),
            );
            target
        };
        review.wheel_target = Some(target);
        review.follow_viewport = true;
        true
    }

    /// Whether a wheel motion is running (frames are needed until it completes).
    pub(super) fn animating(&self) -> bool {
        self.review
            .as_ref()
            .is_some_and(|review| review.list.borrow().motion().is_some())
    }

    /// Apply what routing `mouse` through the list collected. Returns whether anything changed
    /// and an action for the host (opening a session).
    pub(super) fn after_list_event(
        &mut self,
        mouse: MouseEvent,
        in_list: bool,
    ) -> (bool, Option<Action>) {
        let Some(review) = self.review.as_mut() else {
            return (false, None);
        };
        let clicks: Vec<ItemClick> = review.interaction.clicks.borrow_mut().drain(..).collect();
        let pointer: Vec<PointerHit> = review.interaction.pointer.borrow_mut().drain(..).collect();
        let mut changed = !clicks.is_empty();
        let mut action = None;
        for click in clicks {
            if let Some(result) = self.run_click(&click) {
                action = Some(result);
            }
        }
        changed |= self.track_pointer(mouse, in_list, &pointer);
        (changed, action)
    }

    /// Hover, drag start, drag extension (with edge scrolling), and drag end.
    fn track_pointer(&mut self, mouse: MouseEvent, in_list: bool, hits: &[PointerHit]) -> bool {
        use bmux_tui::event::{MouseButton, MouseEventKind};
        let list_area = self.list_area();
        let Some(review) = self.review.as_mut() else {
            return false;
        };
        let hit = hits.last();
        let line = hit.filter(|hit| hit.diff && is_line(review, &hit.key));
        let mut changed = false;
        match mouse.kind {
            MouseEventKind::Move => {
                let hover = line.map(|hit| hit.key.clone());
                if review.hover != hover {
                    review.hover = hover;
                    changed = true;
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                review.drag = line.map(|hit| (hit.key.clone(), hit.key.clone()));
                // Pressing on a line puts the cursor there (the drag's fixed end).
                if let Some(hit) = line
                    && let Some(index) = review.doc.index_of(&hit.key)
                {
                    review.cursor_index = index;
                    review.cursor = Some(hit.key.clone());
                    changed = true;
                }
                if review.selection.take().is_some() {
                    changed = true;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some((anchor, _)) = review.drag.clone() else {
                    return false;
                };
                let end = match line {
                    Some(hit) if hit.kind == PointerKind::Drag => Some(hit.key.clone()),
                    _ if !in_list => list_area.and_then(|area| {
                        let rows = if mouse.position.y < area.y {
                            -1
                        } else if mouse.position.y >= area.bottom() {
                            1
                        } else {
                            return None;
                        };
                        edge_line(review, rows)
                    }),
                    _ => None,
                };
                if let Some(end) = end.filter(|end| end.file() == anchor.file()) {
                    review.drag = Some((anchor.clone(), end.clone()));
                    let differs = anchor != end;
                    review.selection = differs.then_some(Selection {
                        anchor,
                        end,
                        follow: false,
                    });
                    changed = true;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some((anchor, end)) = review.drag.take()
                    && anchor != end
                {
                    let range = review.doc.range_anchor(&anchor, &end);
                    if let Some(range) = range {
                        review.selection = Some(Selection {
                            anchor,
                            end: end.clone(),
                            follow: false,
                        });
                        if let Some(index) = review.doc.index_of(&end) {
                            review.cursor_index = index;
                            review.cursor = Some(end);
                        }
                        self.open_composer(range, ComposerMode::Comment, "");
                        return true;
                    }
                }
            }
            _ => {}
        }
        changed
    }

    /// Run one clicked item action.
    fn run_click(&mut self, click: &ItemClick) -> Option<Action> {
        let review = self.review.as_mut()?;
        if let Some(index) = review.doc.index_of(&click.key)
            && !matches!(click.key, ItemKey::Composer)
        {
            review.cursor_index = index;
            review.cursor = Some(click.key.clone());
        }
        match click.action {
            ItemAction::Gap(action) => {
                Self::apply_gap_click(review, &click.key, action);
                None
            }
            ItemAction::Thread(action) => self.thread_action(action),
            ItemAction::Agent(action) => self.agent_action(action),
            ItemAction::Composer(button) => {
                self.composer_button(button);
                None
            }
            ItemAction::Comment => {
                let anchor = review.doc.anchor_for(&click.key)?;
                review.selection = None;
                self.open_composer(anchor, ComposerMode::Comment, "");
                None
            }
        }
    }
}

/// Scroll the list one row toward `rows` and return the diff line now on that edge's first
/// uncovered row: the bottom row, or the first row below the pinned section headers.
fn edge_line(review: &Review, rows: i64) -> Option<ItemKey> {
    let viewport = review.viewport_height.max(1);
    let mut list = review.list.borrow_mut();
    // The resolved top is the last layout's; the scroll lands one row past it when it moved.
    let top = list.resolved()?.top_offset;
    let top = if list.scroll_by(rows, viewport) {
        top.saturating_add_signed(rows.signum())
    } else {
        top
    };
    let offset = if rows < 0 {
        top + list.sticky_header_rows()
    } else {
        top + viewport - 1
    };
    let key: RowKey = list.key_at_offset(offset)?.clone();
    drop(list);
    matches!(
        review.doc.item(review.doc.index_of(&key.0)?)?,
        ReviewItem::Line(_) | ReviewItem::LinePair(_)
    )
    .then_some(key.0)
}
