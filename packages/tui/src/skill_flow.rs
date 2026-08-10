//! Root-owned skill picker and action semantics.

use bmux_keyboard::{KeyCode, KeyStroke};

use bcode_skill_models::SkillId;

use super::effects::{SkillActionKind, SkillActionRequest, TuiEffect};
use super::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};
use super::session_flow::ActiveChat;
use super::{skill_picker, text_input_flow};

pub fn handle_skill_picker_key(
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
        | BmuxAction::SessionSearchOpen
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
        | BmuxAction::InteractionFocusActive
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

pub fn format_skill_manifest_markdown(manifest: &bcode_skill_models::SkillManifest) -> String {
    let description = manifest
        .summary
        .description
        .as_deref()
        .unwrap_or("no description");
    super::slash_commands::format_skill_details_markdown(
        &manifest.summary.name,
        manifest.summary.id.as_str(),
        &manifest.summary.source.label,
        Some(description),
        &truncate_markdown_for_display(&manifest.instructions, 2_000),
    )
}

pub fn start_skill_action(
    launch_working_directory: &std::path::Path,
    chat: &mut ActiveChat,
    action: SkillActionKind,
    skill_id: SkillId,
    arguments: String,
) {
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
            launch_working_directory: launch_working_directory.to_path_buf(),
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
