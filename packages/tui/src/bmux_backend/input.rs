//! BMUX backend input handling.

use bmux_keyboard::KeyStroke;
use bmux_text_edit::keyboard::TextKeymap;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyHandler, TextInputKeyOutcome};

use super::app::BmuxApp;
use super::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};
const TRANSCRIPT_SCROLL_ROWS: usize = 3;
const TRANSCRIPT_PAGE_ROWS: usize = 10;

/// Result of handling one key stroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct KeyOutcome {
    /// Whether the caller should redraw the UI.
    pub(super) redraw: bool,
    /// Whether the composer was submitted.
    pub(super) submitted: bool,
}

/// Handle a key stroke.
pub(super) fn handle_key(app: &mut BmuxApp, keymap: &BmuxKeyMap, stroke: KeyStroke) -> KeyOutcome {
    if let Some(outcome) = handle_chat_action(app, keymap.action_for_key(BmuxScope::Chat, stroke)) {
        return outcome;
    }
    if let Some(command) = keymap.editor_command_for_key(stroke) {
        app.composer_mut().apply_command(command);
        app.wake_cursor();
        return KeyOutcome {
            redraw: true,
            submitted: false,
        };
    }

    let outcome = TextInputKeyHandler::new(TextKeymap::default(), TextInputEnterBehavior::Submit)
        .handle_key(app.composer_mut(), stroke);
    match outcome {
        TextInputKeyOutcome::Submitted => submit(app),
        TextInputKeyOutcome::Edited => {
            app.wake_cursor();
            KeyOutcome {
                redraw: true,
                submitted: false,
            }
        }
        TextInputKeyOutcome::Ignored => KeyOutcome::default(),
    }
}

fn handle_chat_action(app: &mut BmuxApp, action: Option<BmuxAction>) -> Option<KeyOutcome> {
    let outcome = match action? {
        BmuxAction::AppExit | BmuxAction::AppInterrupt => {
            app.request_exit();
            KeyOutcome {
                redraw: true,
                submitted: false,
            }
        }
        BmuxAction::InputSubmit => submit(app),
        BmuxAction::InputHistoryPrevious => KeyOutcome {
            redraw: app.move_composer_visual_up() || app.previous_input_history(),
            submitted: false,
        },
        BmuxAction::InputHistoryNext => KeyOutcome {
            redraw: app.move_composer_visual_down() || app.next_input_history(),
            submitted: false,
        },
        BmuxAction::TranscriptPageUp => KeyOutcome {
            redraw: app.scroll_transcript_up(TRANSCRIPT_PAGE_ROWS),
            submitted: false,
        },
        BmuxAction::TranscriptPageDown => KeyOutcome {
            redraw: app.scroll_transcript_down(TRANSCRIPT_PAGE_ROWS),
            submitted: false,
        },
        BmuxAction::TranscriptTop => KeyOutcome {
            redraw: app.scroll_transcript_up(usize::MAX / 2),
            submitted: false,
        },
        BmuxAction::TranscriptBottom => KeyOutcome {
            redraw: app.scroll_transcript_to_bottom(),
            submitted: false,
        },
        BmuxAction::TranscriptLineUp => KeyOutcome {
            redraw: app.scroll_transcript_up(TRANSCRIPT_SCROLL_ROWS),
            submitted: false,
        },
        BmuxAction::TranscriptLineDown => KeyOutcome {
            redraw: app.scroll_transcript_down(TRANSCRIPT_SCROLL_ROWS),
            submitted: false,
        },
        BmuxAction::CommandPaletteOpen
        | BmuxAction::PermissionApprove
        | BmuxAction::PermissionDeny
        | BmuxAction::SelectUp
        | BmuxAction::SelectDown
        | BmuxAction::SelectConfirm
        | BmuxAction::SelectCancel
        | BmuxAction::SessionNew
        | BmuxAction::SessionRename
        | BmuxAction::SessionDelete
        | BmuxAction::InputNewLine
        | BmuxAction::EditorMoveLeft
        | BmuxAction::EditorMoveRight
        | BmuxAction::EditorMoveWordLeft
        | BmuxAction::EditorMoveWordRight
        | BmuxAction::EditorMoveStart
        | BmuxAction::EditorMoveEnd
        | BmuxAction::EditorDeleteBackward
        | BmuxAction::EditorDeleteForward
        | BmuxAction::EditorDeleteWordBackward
        | BmuxAction::EditorDeleteWordForward
        | BmuxAction::EditorDeleteToStart
        | BmuxAction::EditorDeleteToEnd
        | BmuxAction::SkillInvoke
        | BmuxAction::SkillActivate
        | BmuxAction::SkillDeactivate
        | BmuxAction::SkillHelp => return None,
    };
    Some(outcome)
}

fn submit(app: &mut BmuxApp) -> KeyOutcome {
    app.stage_submission();
    app.wake_cursor();
    KeyOutcome {
        redraw: true,
        submitted: true,
    }
}
