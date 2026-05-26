//! Skill picker and skill action flow for the TUI.

use std::io::Write;

use super::runtime_context::{TuiIo, TuiServices};
use bcode_client::BcodeClient;
use bcode_skill_models::SkillId;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyOutcome};

use super::helpers;
use super::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};
use super::picker_mouse::picker_row_from_mouse;
use super::{TuiError, session_flow::ActiveChat, skill_picker, skill_picker_render};

/// Pick and perform a skill action for the active session.
pub async fn pick_skill_for_session<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let skills = services.client.list_skills().await?;
    if skills.skills.is_empty() {
        chat.app.set_status("no skills available".to_owned());
        chat.app
            .push_system_note("No skills are available.".to_owned());
        return Ok(());
    }
    let mut picker = skill_picker::SkillPickerApp::new(skills.skills);
    loop {
        io.terminal.resize(helpers::terminal_area()?);
        io.terminal
            .draw(|frame| skill_picker_render::render_skill_picker(&mut picker, frame))?;
        let Some(event) = io.input.recv().await? else {
            continue;
        };
        match event {
            Event::Resize(size) => io.terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => match picker.mode() {
                skill_picker::SkillPickerMode::Filter => {
                    picker.filter_mut().insert_str(&text);
                    picker.refresh_filter();
                }
                skill_picker::SkillPickerMode::Argument => picker.argument_mut().insert_str(&text),
            },
            Event::Key(stroke) => {
                match handle_skill_picker_key(&mut picker, services.keymap, stroke) {
                    skill_picker::SkillPickerAction::Continue => {}
                    skill_picker::SkillPickerAction::Cancel => return Ok(()),
                    skill_picker::SkillPickerAction::Help(skill_id) => {
                        if let Err(error) = describe_skill(services.client, chat, skill_id).await {
                            helpers::report_client_error(
                                &mut chat.app,
                                "skill help failed",
                                &error,
                            );
                        }
                        return Ok(());
                    }
                    skill_picker::SkillPickerAction::Activate(skill_id) => {
                        if let Err(error) = activate_skill(services.client, chat, skill_id).await {
                            helpers::report_client_error(
                                &mut chat.app,
                                "skill activation failed",
                                &error,
                            );
                        }
                        return Ok(());
                    }
                    skill_picker::SkillPickerAction::Deactivate(skill_id) => {
                        if let Err(error) = deactivate_skill(services.client, chat, skill_id).await
                        {
                            helpers::report_client_error(
                                &mut chat.app,
                                "skill deactivation failed",
                                &error,
                            );
                        }
                        return Ok(());
                    }
                    skill_picker::SkillPickerAction::Invoke {
                        skill_id,
                        arguments,
                    } => {
                        if let Err(error) =
                            invoke_skill(services.client, chat, skill_id, arguments).await
                        {
                            helpers::report_client_error(
                                &mut chat.app,
                                "skill invocation failed",
                                &error,
                            );
                        }
                        return Ok(());
                    }
                }
            }
            Event::Mouse(mouse) => {
                if let Some(row) = picker_row_from_mouse(mouse)
                    && picker.select_visible(row)
                {
                    picker.start_argument();
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
    }
}

/// Show active skills in the transcript.
pub async fn show_active_skills(
    client: &BcodeClient,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    let skills = client.active_skills(session_id).await?;
    let mut lines = vec![format!("Active skills: {}", skills.len())];
    lines.extend(skills.iter().map(|skill| {
        let suffix = if skill.truncated { " truncated" } else { "" };
        format!(
            "* {} — {} bytes{} from {}",
            skill.skill_id, skill.bytes_loaded, suffix, skill.source.label
        )
    }));
    chat.app
        .set_status(format!("active skills: {}", skills.len()));
    chat.app.push_system_note(lines.join("\n"));
    Ok(())
}

fn handle_skill_picker_key(
    picker: &mut skill_picker::SkillPickerApp,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) -> skill_picker::SkillPickerAction {
    match picker.mode() {
        skill_picker::SkillPickerMode::Filter => handle_skill_filter_key(picker, keymap, stroke),
        skill_picker::SkillPickerMode::Argument => {
            handle_skill_argument_key(picker, keymap, stroke)
        }
    }
}

fn handle_skill_filter_key(
    picker: &mut skill_picker::SkillPickerApp,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) -> skill_picker::SkillPickerAction {
    if let Some(action) = keymap.action_for_key(BmuxScope::SkillPicker, stroke) {
        return handle_skill_picker_action(picker, action);
    }
    match stroke.key {
        KeyCode::Escape => skill_picker::SkillPickerAction::Cancel,
        KeyCode::Enter => {
            if picker.selected_skill_id().is_some() {
                picker.start_argument();
            }
            skill_picker::SkillPickerAction::Continue
        }
        KeyCode::Up if stroke.modifiers.is_empty() => {
            picker.select_previous();
            skill_picker::SkillPickerAction::Continue
        }
        KeyCode::Down if stroke.modifiers.is_empty() => {
            picker.select_next();
            skill_picker::SkillPickerAction::Continue
        }
        KeyCode::Char('a') if stroke.modifiers.is_empty() => picker.selected_skill_id().map_or(
            skill_picker::SkillPickerAction::Continue,
            skill_picker::SkillPickerAction::Activate,
        ),
        KeyCode::Char('d') if stroke.modifiers.is_empty() => picker.selected_skill_id().map_or(
            skill_picker::SkillPickerAction::Continue,
            skill_picker::SkillPickerAction::Deactivate,
        ),
        KeyCode::Char('?') if stroke.modifiers.is_empty() => picker.selected_skill_id().map_or(
            skill_picker::SkillPickerAction::Continue,
            skill_picker::SkillPickerAction::Help,
        ),
        _ => {
            let outcome = helpers::handle_text_buffer_key(
                picker.filter_mut(),
                keymap,
                stroke,
                TextInputEnterBehavior::InsertNewline,
            );
            if outcome == TextInputKeyOutcome::Edited {
                picker.refresh_filter();
            }
            skill_picker::SkillPickerAction::Continue
        }
    }
}

fn handle_skill_picker_action(
    picker: &mut skill_picker::SkillPickerApp,
    action: BmuxAction,
) -> skill_picker::SkillPickerAction {
    match action {
        BmuxAction::SelectCancel => skill_picker::SkillPickerAction::Cancel,
        BmuxAction::SelectUp => {
            picker.select_previous();
            skill_picker::SkillPickerAction::Continue
        }
        BmuxAction::SelectDown => {
            picker.select_next();
            skill_picker::SkillPickerAction::Continue
        }
        BmuxAction::SelectConfirm | BmuxAction::SkillInvoke => {
            if picker.selected_skill_id().is_some() {
                picker.start_argument();
            }
            skill_picker::SkillPickerAction::Continue
        }
        BmuxAction::SkillActivate => picker.selected_skill_id().map_or(
            skill_picker::SkillPickerAction::Continue,
            skill_picker::SkillPickerAction::Activate,
        ),
        BmuxAction::SkillDeactivate => picker.selected_skill_id().map_or(
            skill_picker::SkillPickerAction::Continue,
            skill_picker::SkillPickerAction::Deactivate,
        ),
        BmuxAction::SkillHelp => picker.selected_skill_id().map_or(
            skill_picker::SkillPickerAction::Continue,
            skill_picker::SkillPickerAction::Help,
        ),
        BmuxAction::InputSubmit
        | BmuxAction::InputHistoryPrevious
        | BmuxAction::InputHistoryNext
        | BmuxAction::AppExit
        | BmuxAction::AppInterrupt
        | BmuxAction::ClipboardPasteImage
        | BmuxAction::CommandPaletteOpen
        | BmuxAction::TranscriptPageUp
        | BmuxAction::TranscriptPageDown
        | BmuxAction::TranscriptTop
        | BmuxAction::TranscriptBottom
        | BmuxAction::TranscriptLineUp
        | BmuxAction::TranscriptLineDown
        | BmuxAction::PermissionApprove
        | BmuxAction::PermissionDeny
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
        | BmuxAction::EditorDeleteToEnd => skill_picker::SkillPickerAction::Continue,
    }
}

fn handle_skill_argument_key(
    picker: &mut skill_picker::SkillPickerApp,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) -> skill_picker::SkillPickerAction {
    match stroke.key {
        KeyCode::Escape => skill_picker::SkillPickerAction::Cancel,
        KeyCode::Enter => picker.selected_skill_id().map_or(
            skill_picker::SkillPickerAction::Continue,
            |skill_id| skill_picker::SkillPickerAction::Invoke {
                skill_id,
                arguments: picker.argument().text().to_owned(),
            },
        ),
        _ => {
            let _outcome = helpers::handle_text_buffer_key(
                picker.argument_mut(),
                keymap,
                stroke,
                TextInputEnterBehavior::InsertNewline,
            );
            skill_picker::SkillPickerAction::Continue
        }
    }
}

async fn describe_skill(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    skill_id: SkillId,
) -> Result<(), TuiError> {
    let manifest = client.describe_skill(skill_id.clone()).await?;
    let description = manifest
        .summary
        .description
        .as_deref()
        .unwrap_or("no description");
    chat.app.push_system_note(format!(
        "Skill: {}\nName: {}\nDescription: {description}\nSource: {}\nInstructions:\n{}",
        manifest.summary.id,
        manifest.summary.name,
        manifest.summary.source.label,
        truncate_for_status(&manifest.instructions, 2_000)
    ));
    chat.app.set_status(format!("shown skill {skill_id}"));
    Ok(())
}

async fn activate_skill(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    skill_id: SkillId,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    client.activate_skill(session_id, skill_id.clone()).await?;
    chat.app.set_status(format!("activated skill {skill_id}"));
    Ok(())
}

async fn deactivate_skill(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    skill_id: SkillId,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    client
        .deactivate_skill(session_id, skill_id.clone())
        .await?;
    chat.app.set_status(format!("deactivated skill {skill_id}"));
    Ok(())
}

async fn invoke_skill(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    skill_id: SkillId,
    arguments: String,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    let display_text = if arguments.trim().is_empty() {
        format!("Invoke skill {skill_id}")
    } else {
        format!("Invoke skill {skill_id}: {arguments}")
    };
    let acceptance = client
        .invoke_skill(session_id, skill_id.clone(), arguments, display_text)
        .await?;
    chat.app.set_status(if acceptance.queued {
        format!("skill {skill_id} queued")
    } else {
        format!("skill {skill_id} invoked")
    });
    Ok(())
}

fn truncate_for_status(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}\n…")
    } else {
        truncated
    }
}
