//! Behavioral coverage for the shared BMUX TUI adapter.

use super::*;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::component::{Constraints, LayoutCx};
use bmux_tui::composition::TextBlock;
use bmux_tui::geometry::Size;

#[derive(Default)]
struct ResidentHistory;

impl TranscriptHistory for ResidentHistory {
    fn has_older_history(&self) -> bool {
        false
    }
    fn has_newer_history(&self) -> bool {
        false
    }
    fn reveal_request(&self) -> Option<usize> {
        None
    }
    fn newer_reveal_request(&self) -> Option<usize> {
        None
    }
    fn cancel_newer_reveal(&mut self) {}
    fn cancel_older_reveal(&mut self) {}
    fn request_load(&mut self, _: usize) {}
    fn request_load_newer(&mut self, _: usize) {}
    fn clear_reveal_request(&mut self) {}
    fn take_ready_pages(&mut self) -> (bool, bool, bool) {
        (false, false, false)
    }
    fn consume_reveal(&mut self, _: bool, _: usize) {}
}

#[test]
fn submission_is_an_effect_and_retains_draft_until_acceptance() {
    let mut ui = SessionUi::new();
    let mut history = ResidentHistory;
    assert_eq!(
        ui.update(
            SessionUiAction::Paste("你好 👩‍💻 e\u{301}".into()),
            &mut history
        ),
        SessionUiEffect::Redraw
    );
    assert_eq!(
        ui.update(
            SessionUiAction::Key(KeyStroke::simple(KeyCode::Enter)),
            &mut history
        ),
        SessionUiEffect::SubmitRequested
    );
    assert_eq!(ui.composer.buffer().text(), "你好 👩‍💻 e\u{301}");
}

#[test]
fn manual_scroll_detaches_then_follow_resumes_on_new_rows() {
    let mut ui = SessionUi::new();
    let mut history = ResidentHistory;
    ui.viewport.sync_max(20, 0, 30, 10, false, &mut history);
    ui.update(SessionUiAction::ScrollUp(5), &mut history);
    assert_eq!(ui.viewport.top_row(30, 10), 15);
    ui.viewport.sync_max(25, 0, 35, 10, false, &mut history);
    assert_eq!(ui.viewport.top_row(35, 10), 15);
    ui.update(SessionUiAction::FollowTail, &mut history);
    assert_eq!(ui.viewport.top_row(35, 10), 25);
}

#[test]
fn bmux_stack_contains_editor_and_transcript_at_tiny_sizes() {
    let ids = SessionUiIds {
        root: "pane",
        composer: "pane.composer",
        editor: "pane.editor",
        composer_size: "pane.editor.size",
    };
    let state = RefCell::new(SessionUi::new().composer);
    let policy = composer_policy();
    let style = SessionComposerStyle {
        panel: ComposerStyle {
            border: Style::new(),
            surface: Style::new(),
        },
        text: Style::new(),
        selection: Style::new(),
        muted: Style::new(),
    };
    for width in [0, 1, 2, 12, 80] {
        for height in [0, 1, 2, 12] {
            let editor = composer(&ids, &state, &policy, style, true);
            let tree = compose(&ids, TextBlock::new("你好 👩‍💻 e\u{301}"), editor, 4);
            let layout = tree.layout(
                Constraints::tight(Size::new(width, height)),
                &mut LayoutCx::new(),
            );
            assert!(layout.size.width <= u64::from(width));
            assert!(layout.size.height <= u64::from(height));
            for child in &layout.children {
                assert!(child.x + child.node.size.width <= u64::from(width));
                assert!(child.y + child.node.size.height <= u64::from(height));
            }
        }
    }
}
