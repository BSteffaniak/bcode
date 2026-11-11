//! Behavioral coverage for the shared BMUX TUI composer recipe.

use super::*;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::component::{Constraints, LayoutCx};
use bmux_tui::composition::TextBlock;
use bmux_tui::geometry::Size;

#[test]
fn bmux_stack_contains_editor_and_transcript_at_tiny_sizes() {
    let ids = SessionUiIds {
        root: "pane",
        composer: "pane.composer",
        editor: "pane.editor",
        composer_size: "pane.editor.size",
    };
    let state = RefCell::new(TextInputState::new(TextEditBuffer::new()));
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
