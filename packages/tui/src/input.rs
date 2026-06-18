//! TUI input handling.

use bmux_keyboard::KeyStroke;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyOutcome};

use super::app::BmuxApp;
use super::helpers;
use super::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};
const TRANSCRIPT_SCROLL_ROWS: usize = 3;
const TRANSCRIPT_PAGE_ROWS: usize = 10;

/// Follow-up request produced by handling one key stroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyRequest {
    /// No follow-up request.
    #[default]
    None,
    /// Submit staged composer text.
    Submit {
        /// Placement behavior for the submitted prompt.
        placement: bcode_ipc::PromptPlacement,
    },
    /// Interrupt the active turn.
    Interrupt,
    /// Cycle to the next available agent.
    CycleAgent,
    /// Cycle to the next supported thinking effort.
    CycleThinkingEffort,
}

/// Result of handling one key stroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyOutcome {
    /// Whether the caller should redraw the UI.
    pub redraw: bool,
    /// Follow-up request for the caller to perform.
    pub request: KeyRequest,
}

/// Handle a key stroke.
pub fn handle_key(app: &mut BmuxApp, keymap: &BmuxKeyMap, stroke: KeyStroke) -> KeyOutcome {
    if let Some(outcome) = handle_chat_action(app, keymap.action_for_key(BmuxScope::Chat, stroke)) {
        return outcome;
    }
    if let Some(motion) = keymap.editor_selection_motion_for_key(stroke) {
        app.extend_composer_selection(motion);
        return KeyOutcome {
            redraw: true,
            request: KeyRequest::None,
        };
    }
    if let Some(command) = keymap.editor_command_for_key(stroke) {
        app.reset_input_history_navigation();
        app.composer_mut().apply_command(command);
        app.wake_cursor();
        return KeyOutcome {
            redraw: true,
            request: KeyRequest::None,
        };
    }

    let outcome = helpers::handle_default_text_key(
        app.composer_mut(),
        stroke,
        TextInputEnterBehavior::Submit,
    );
    match outcome {
        TextInputKeyOutcome::Submitted => submit(app, bcode_ipc::PromptPlacement::Steering),
        TextInputKeyOutcome::Edited => {
            app.reset_input_history_navigation();
            app.wake_cursor();
            KeyOutcome {
                redraw: true,
                request: KeyRequest::None,
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
                app.reset_input_history_navigation();
                app.composer_mut().clear();
                app.set_status("input cleared; press exit again to quit".to_owned());
                app.wake_cursor();
            }
            KeyOutcome {
                redraw: true,
                request: KeyRequest::None,
            }
        }
        BmuxAction::AppInterrupt => KeyOutcome {
            redraw: true,
            request: KeyRequest::Interrupt,
        },
        BmuxAction::InputSubmitSteering => submit(app, bcode_ipc::PromptPlacement::Steering),
        BmuxAction::InputSubmitFollowUp => submit(app, bcode_ipc::PromptPlacement::FollowUp),
        BmuxAction::AgentCycle => KeyOutcome {
            redraw: true,
            request: KeyRequest::CycleAgent,
        },
        BmuxAction::ThinkingEffortCycle => KeyOutcome {
            redraw: true,
            request: KeyRequest::CycleThinkingEffort,
        },
        BmuxAction::InputHistoryPrevious => history_previous(app),
        BmuxAction::InputHistoryNext => history_next(app),
        BmuxAction::TranscriptPageUp => KeyOutcome {
            redraw: app.scroll_transcript_up(TRANSCRIPT_PAGE_ROWS),
            request: KeyRequest::None,
        },
        BmuxAction::TranscriptPageDown => KeyOutcome {
            redraw: app.scroll_transcript_down(TRANSCRIPT_PAGE_ROWS),
            request: KeyRequest::None,
        },
        BmuxAction::TranscriptTop => KeyOutcome {
            redraw: app.scroll_transcript_up(usize::MAX / 2),
            request: KeyRequest::None,
        },
        BmuxAction::TranscriptBottom => KeyOutcome {
            redraw: app.scroll_transcript_to_bottom(),
            request: KeyRequest::None,
        },
        BmuxAction::TranscriptLineUp => KeyOutcome {
            redraw: app.scroll_transcript_up(TRANSCRIPT_SCROLL_ROWS),
            request: KeyRequest::None,
        },
        BmuxAction::TranscriptLineDown => KeyOutcome {
            redraw: app.scroll_transcript_down(TRANSCRIPT_SCROLL_ROWS),
            request: KeyRequest::None,
        },
        BmuxAction::ClipboardPasteImage
        | BmuxAction::CommandPaletteOpen
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

fn history_previous(app: &mut BmuxApp) -> KeyOutcome {
    KeyOutcome {
        redraw: if app.input_history_navigation_active() {
            app.move_composer_visual_up_preserving_history() || app.previous_input_history()
        } else {
            app.move_composer_visual_up() || app.previous_input_history()
        },
        request: KeyRequest::None,
    }
}

fn history_next(app: &mut BmuxApp) -> KeyOutcome {
    KeyOutcome {
        redraw: if app.input_history_navigation_active() {
            app.move_composer_visual_down_preserving_history() || app.next_input_history()
        } else {
            app.move_composer_visual_down() || app.next_input_history()
        },
        request: KeyRequest::None,
    }
}

fn submit(app: &mut BmuxApp, placement: bcode_ipc::PromptPlacement) -> KeyOutcome {
    app.stage_submission();
    app.wake_cursor();
    KeyOutcome {
        redraw: true,
        request: KeyRequest::Submit { placement },
    }
}
