//! Skill picker and skill action flow for the TUI.

use std::io::Write;

use super::effects::{SkillActionKind, SkillActionRequest, TuiEffect};
use super::runtime_context::{TuiIo, TuiServices};
use bcode_client::BcodeClient;
use bcode_skill_models::SkillId;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;

use super::helpers;
use super::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};
use super::picker_mouse::picker_row_from_mouse;
use super::{
    TuiError, session_flow::ActiveChat, skill_picker, skill_picker_render, text_input_flow,
};

#[allow(clippy::too_many_lines)]
/// Pick and perform a skill action for the active session.
pub async fn pick_skill_for_session<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let skills = match services.passive_client.list_skills().await {
        Ok(skills) => skills,
        Err(error) => {
            helpers::report_client_issue(&mut chat.app, "skills unavailable", &error);
            return Ok(());
        }
    };
    if skills.skills.is_empty() {
        chat.app.set_status("no skills available".to_owned());
        chat.push_presentation_note(
            "bcode.host",
            "No skills are available.".to_owned(),
            bcode_command::CommandTextFormat::PlainText,
        );
        return Ok(());
    }
    let mut picker = skill_picker::SkillPickerApp::new(skills.skills);
    loop {
        io.terminal.resize(helpers::terminal_area()?);
        io.terminal.draw(|frame| {
            skill_picker_render::render_skill_picker(&mut picker, frame, services.theme);
        })?;
        let Some(event) = io.input.recv().await? else {
            continue;
        };
        match event {
            Event::Resize(size) => io.terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => match picker.mode() {
                skill_picker::SkillPickerMode::Filter => {
                    let _ = text_input_flow::handle_paste(picker.filter_mut(), &text);
                    picker.refresh_filter();
                }
                skill_picker::SkillPickerMode::Argument => {
                    let _ = text_input_flow::handle_paste(picker.argument_mut(), &text);
                }
            },
            Event::Key(stroke) => {
                match handle_skill_picker_key(&mut picker, services.keymap, stroke) {
                    skill_picker::SkillPickerAction::Continue => {}
                    skill_picker::SkillPickerAction::Cancel => return Ok(()),
                    skill_picker::SkillPickerAction::Help(skill_id) => {
                        if let Err(error) =
                            describe_skill(services.passive_client, chat, skill_id).await
                        {
                            helpers::report_client_error(
                                &mut chat.app,
                                "skill help failed",
                                &error,
                            );
                        }
                        return Ok(());
                    }
                    skill_picker::SkillPickerAction::Activate(skill_id) => {
                        start_skill_action(
                            chat,
                            SkillActionKind::Activate,
                            skill_id,
                            String::new(),
                        )?;
                        return Ok(());
                    }
                    skill_picker::SkillPickerAction::Deactivate(skill_id) => {
                        start_skill_action(
                            chat,
                            SkillActionKind::Deactivate,
                            skill_id,
                            String::new(),
                        )?;
                        return Ok(());
                    }
                    skill_picker::SkillPickerAction::Invoke {
                        skill_id,
                        arguments,
                    } => {
                        start_skill_action(chat, SkillActionKind::Invoke, skill_id, arguments)?;
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
            if text_input_flow::handle_key(picker.filter_mut(), keymap, stroke)
                != bmux_tui_components::text_input::TextInputOutcome::Ignored
            {
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
        BmuxAction::InputSubmitSteering
        | BmuxAction::InputSubmitFollowUp
        | BmuxAction::InputHistoryPrevious
        | BmuxAction::InputHistoryNext
        | BmuxAction::AppExit
        | BmuxAction::AppInterrupt
        | BmuxAction::ClipboardPasteImage
        | BmuxAction::CommandPaletteOpen
        | BmuxAction::AgentCycle
        | BmuxAction::DiffViewerLayoutCycle
        | BmuxAction::MarkdownFocusNext
        | BmuxAction::MarkdownFocusPrevious
        | BmuxAction::MarkdownActivate
        | BmuxAction::MarkdownCopyDestination
        | BmuxAction::ThinkingEffortCycle
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
                arguments: picker.argument().buffer().text().to_owned(),
            },
        ),
        _ => {
            let _outcome = text_input_flow::handle_key(picker.argument_mut(), keymap, stroke);
            skill_picker::SkillPickerAction::Continue
        }
    }
}

async fn describe_skill(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    skill_id: SkillId,
) -> Result<(), TuiError> {
    let manifest = match client.describe_skill(skill_id.clone()).await {
        Ok(manifest) => manifest,
        Err(error) => {
            helpers::report_client_issue(&mut chat.app, "skill details unavailable", &error);
            return Ok(());
        }
    };
    let description = manifest
        .summary
        .description
        .as_deref()
        .unwrap_or("no description");
    chat.push_presentation_note(
        "bcode.host",
        super::slash_commands::format_skill_details_markdown(
            &manifest.summary.name,
            manifest.summary.id.as_str(),
            &manifest.summary.source.label,
            Some(description),
            &truncate_markdown_for_display(&manifest.instructions, 2_000),
        ),
        bcode_command::CommandTextFormat::Markdown,
    );
    chat.app.set_status(format!("shown skill {skill_id}"));
    Ok(())
}

pub fn start_invoke_skill_for_session(
    chat: &mut ActiveChat,
    skill_id: SkillId,
    arguments: String,
) -> Result<(), TuiError> {
    start_skill_action(chat, SkillActionKind::Invoke, skill_id, arguments)
}

fn start_skill_action(
    chat: &mut ActiveChat,
    action: SkillActionKind,
    skill_id: SkillId,
    arguments: String,
) -> Result<(), TuiError> {
    let session_id = chat.app.session_id();
    let agent_id = if action == SkillActionKind::Invoke {
        if session_id.is_some() {
            chat.app.pending_agent_id().map(ToOwned::to_owned)
        } else {
            let current = chat.app.current_agent_id().to_owned();
            (current != "build").then_some(current)
        }
    } else {
        None
    };
    let provider_plugin_id = (action == SkillActionKind::Invoke && session_id.is_none())
        .then(|| {
            chat.app
                .selected_provider_plugin_id()
                .map(ToOwned::to_owned)
        })
        .flatten();
    let model_id = (action == SkillActionKind::Invoke && session_id.is_none())
        .then(|| chat.app.selected_model_id().map(ToOwned::to_owned))
        .flatten();
    chat.start_effect(TuiEffect::SkillAction {
        request: Box::new(SkillActionRequest {
            session_id,
            launch_working_directory: std::env::current_dir()?,
            skill_id,
            action,
            arguments,
            provider_plugin_id,
            model_id,
            agent_id,
            reasoning_effort: (action == SkillActionKind::Invoke)
                .then(|| chat.app.reasoning_effort().map(ToOwned::to_owned))
                .flatten(),
            reasoning_summary: (action == SkillActionKind::Invoke)
                .then(|| chat.app.reasoning_summary().map(ToOwned::to_owned))
                .flatten(),
            reasoning_effort_generation: (action == SkillActionKind::Invoke)
                .then(|| chat.app.pending_reasoning_effort_generation())
                .flatten(),
            event_sender: chat.event_sender.clone(),
        }),
    });
    let label = match action {
        SkillActionKind::Activate => "activating skill…",
        SkillActionKind::Deactivate => "deactivating skill…",
        SkillActionKind::Invoke => "invoking skill…",
    };
    chat.app.set_status(label.to_owned());
    Ok(())
}

fn truncate_markdown_for_display(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }

    let mut output = String::new();
    let mut used = 0_usize;
    let mut open_fence: Option<String> = None;
    for line in value.split_inclusive('\n') {
        let line_chars = line.chars().count();
        if used.saturating_add(line_chars) > max_chars {
            break;
        }
        output.push_str(line);
        used = used.saturating_add(line_chars);
        let trimmed = line.trim_start();
        if let Some(marker) = fence_marker(trimmed) {
            if open_fence.as_deref() == Some(marker) {
                open_fence = None;
            } else if open_fence.is_none() {
                open_fence = Some(marker.to_owned());
            }
        }
    }
    if output.is_empty() {
        output = value.chars().take(max_chars).collect();
    }
    if !output.ends_with('\n') {
        output.push('\n');
    }
    if let Some(marker) = open_fence {
        output.push_str(&marker);
        output.push('\n');
    }
    output.push_str("\n_… instructions truncated for display …_");
    output
}

fn fence_marker(line: &str) -> Option<&'static str> {
    if line.starts_with("```") {
        Some("```")
    } else if line.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_markdown_for_display;

    #[test]
    fn markdown_truncation_closes_open_fence_and_adds_notice() {
        let input =
            "# Instructions\n\n```rust\nfn main() {\n    println!(\"long\");\n}\n```\n\nAfter.";
        let output = truncate_markdown_for_display(input, 40);

        assert!(output.contains("```rust"));
        assert_eq!(output.matches("```").count(), 2);
        assert!(output.ends_with("_… instructions truncated for display …_"));
    }

    #[test]
    fn short_markdown_is_unchanged() {
        let input = "## Steps\n\n1. Read\n2. Test";
        assert_eq!(truncate_markdown_for_display(input, 100), input);
    }
}
