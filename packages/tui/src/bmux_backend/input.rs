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
    /// Whether active turn interruption was requested.
    pub(super) interrupted: bool,
}

/// Handle a key stroke.
pub(super) fn handle_key(app: &mut BmuxApp, keymap: &BmuxKeyMap, stroke: KeyStroke) -> KeyOutcome {
    if let Some(outcome) = handle_chat_action(app, keymap.action_for_key(BmuxScope::Chat, stroke)) {
        return outcome;
    }
    if let Some(motion) = keymap.editor_selection_motion_for_key(stroke) {
        app.extend_composer_selection(motion);
        return KeyOutcome {
            redraw: true,
            submitted: false,
            interrupted: false,
        };
    }
    if let Some(command) = keymap.editor_command_for_key(stroke) {
        app.composer_mut().apply_command(command);
        app.wake_cursor();
        return KeyOutcome {
            redraw: true,
            submitted: false,
            interrupted: false,
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
                interrupted: false,
            }
        }
        TextInputKeyOutcome::Ignored => KeyOutcome::default(),
    }
}

fn handle_chat_action(app: &mut BmuxApp, action: Option<BmuxAction>) -> Option<KeyOutcome> {
    let outcome = match action? {
        BmuxAction::AppExit => {
            if app.composer().is_empty() {
                app.request_exit();
            } else {
                app.composer_mut().clear();
                app.set_status("input cleared; press exit again to quit".to_owned());
                app.wake_cursor();
            }
            KeyOutcome {
                redraw: true,
                submitted: false,
                interrupted: false,
            }
        }
        BmuxAction::AppInterrupt => KeyOutcome {
            redraw: true,
            submitted: false,
            interrupted: true,
        },
        BmuxAction::InputSubmit => submit(app),
        BmuxAction::InputHistoryPrevious => KeyOutcome {
            redraw: app.move_composer_visual_up() || app.previous_input_history(),
            submitted: false,
            interrupted: false,
        },
        BmuxAction::InputHistoryNext => KeyOutcome {
            redraw: app.move_composer_visual_down() || app.next_input_history(),
            submitted: false,
            interrupted: false,
        },
        BmuxAction::TranscriptPageUp => KeyOutcome {
            redraw: app.scroll_transcript_up(TRANSCRIPT_PAGE_ROWS),
            submitted: false,
            interrupted: false,
        },
        BmuxAction::TranscriptPageDown => KeyOutcome {
            redraw: app.scroll_transcript_down(TRANSCRIPT_PAGE_ROWS),
            submitted: false,
            interrupted: false,
        },
        BmuxAction::TranscriptTop => KeyOutcome {
            redraw: app.scroll_transcript_up(usize::MAX / 2),
            submitted: false,
            interrupted: false,
        },
        BmuxAction::TranscriptBottom => KeyOutcome {
            redraw: app.scroll_transcript_to_bottom(),
            submitted: false,
            interrupted: false,
        },
        BmuxAction::TranscriptLineUp => KeyOutcome {
            redraw: app.scroll_transcript_up(TRANSCRIPT_SCROLL_ROWS),
            submitted: false,
            interrupted: false,
        },
        BmuxAction::TranscriptLineDown => KeyOutcome {
            redraw: app.scroll_transcript_down(TRANSCRIPT_SCROLL_ROWS),
            submitted: false,
            interrupted: false,
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
        | BmuxAction::EditorSelectLeft
        | BmuxAction::EditorSelectRight
        | BmuxAction::EditorSelectWordLeft
        | BmuxAction::EditorSelectWordRight
        | BmuxAction::EditorSelectUp
        | BmuxAction::EditorSelectDown
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
        interrupted: false,
    }
}
