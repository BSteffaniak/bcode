//! Ralph loop TUI flow.

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use bcode_ipc::{
    RalphApproveRequest, RalphCancelRequest, RalphLifecycleRequest, RalphListIterationsRequest,
    RalphListRunsRequest, RalphResumeRequest, RalphRunRequest, RalphRunStatusRequest,
    RalphRunSummary, RalphStatusSummary,
};
use bcode_ralph as ralph_state;
use bcode_session_models::{SessionHistoryDirection, SessionHistoryQuery};
use bcode_worktree_models::WorktreeCreateRequest;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::{SelectionMode, TextEditBuffer};
use bmux_tui::event::{Event, FocusEvent, MouseEvent};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Style, Widget};
use bmux_tui::style::Color;
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing, ModalTheme};
use bmux_tui_components::text_input::{TextInputControl, TextInputState};

use super::helpers;
use super::keymap::BmuxKeyMap;
use super::runtime_context::{TuiIo, TuiServices};
use super::session_flow::ActiveChat;
use super::{TuiError, ralph_start_dialog, ralph_start_dialog_render};

/// Open the plugin-owned Ralph home UI.
pub async fn open_home<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let mut flash_message: Option<String> = None;
    loop {
        let repo_root = current_repo_root(chat)?;
        match super::ralph_launcher::run_home_with_input(
            io.terminal,
            io.input,
            repo_root,
            flash_message.as_deref(),
        )
        .await?
        {
            super::ralph_launcher::RalphHomeOutcome::Action(action) => {
                match dispatch_home_action(action, io, services, chat).await {
                    Ok(()) => {
                        if action == super::ralph_launcher::RalphHomeAction::RebuildLoopContext {
                            return Ok(());
                        }
                        flash_message = Some(flash_message_for_action(action));
                    }
                    Err(TuiError::Canceled) => {
                        chat.app.set_status("Ralph action canceled".to_owned());
                        flash_message = Some(
                            "Action canceled. Choose the next Ralph action when ready.".to_owned(),
                        );
                    }
                    Err(error) => return Err(error),
                }
            }
            super::ralph_launcher::RalphHomeOutcome::Exit => {
                chat.app.set_status("Ralph UI closed".to_owned());
                return Ok(());
            }
        }
    }
}

fn flash_message_for_action(action: super::ralph_launcher::RalphHomeAction) -> String {
    match action {
        super::ralph_launcher::RalphHomeAction::Plan => {
            "Guided setup started. Next: answer the assistant's clarifying questions in chat."
                .to_owned()
        }
        super::ralph_launcher::RalphHomeAction::SaveDraft => {
            "Setup draft saved. Next: review it, then approve setup draft.".to_owned()
        }
        super::ralph_launcher::RalphHomeAction::ViewDraft => {
            "Setup draft shown. Next: approve it or ask Ralph to revise it.".to_owned()
        }
        super::ralph_launcher::RalphHomeAction::ReviseDraft => {
            "Revision prompt prepared. Submit it, then save setup draft again.".to_owned()
        }
        super::ralph_launcher::RalphHomeAction::RebuildLoopContext => {
            "Rebuild setup started. Answer questions, then save/approve/apply the draft.".to_owned()
        }
        super::ralph_launcher::RalphHomeAction::ApproveDraft => {
            "Setup draft approved. Next: create the loop from the approved draft.".to_owned()
        }
        super::ralph_launcher::RalphHomeAction::CreateFromDraft => {
            "Loop created from setup draft. Next: prepare a run.".to_owned()
        }
        super::ralph_launcher::RalphHomeAction::ApplyDraftToLoop => {
            "Existing loop context rebuilt from draft. Next: review status or prepare run."
                .to_owned()
        }
        super::ralph_launcher::RalphHomeAction::Start => {
            "Setup complete. Next: review the docs if desired, then prepare a run.".to_owned()
        }
        super::ralph_launcher::RalphHomeAction::Run
        | super::ralph_launcher::RalphHomeAction::Goal => {
            "Run prepared. Next: approve/start the prepared run.".to_owned()
        }
        super::ralph_launcher::RalphHomeAction::Approve => {
            "Run approved. Next: watch status/iterations, or stop if needed.".to_owned()
        }
        super::ralph_launcher::RalphHomeAction::Stop => {
            "Stop requested. Next: refresh status, then resume, audit, or replan.".to_owned()
        }
        super::ralph_launcher::RalphHomeAction::Resume => {
            "Resume requested. Next: watch status/iterations.".to_owned()
        }
        super::ralph_launcher::RalphHomeAction::Status => {
            "Status written to chat. Next: choose the recommended Ralph action below.".to_owned()
        }
        super::ralph_launcher::RalphHomeAction::Runs => {
            "Runs written to chat. Next: choose an available action for the latest run.".to_owned()
        }
        super::ralph_launcher::RalphHomeAction::Iterations => {
            "Iterations written to chat. Next: continue, audit, replan, or resume as appropriate."
                .to_owned()
        }
        super::ralph_launcher::RalphHomeAction::Open => {
            "Progress doc path written to chat. Next: prepare/run or recalibrate as needed."
                .to_owned()
        }
        super::ralph_launcher::RalphHomeAction::Audit => {
            "Audit prompt written to chat. Next: run the audit or replan from findings.".to_owned()
        }
        super::ralph_launcher::RalphHomeAction::Replan => {
            "Replan prompt written to chat. Next: apply the replan, then prepare a run.".to_owned()
        }
    }
}

fn markdown_preview(text: Option<&str>) -> String {
    text.map_or_else(
        || "<missing>".to_owned(),
        |value| {
            let preview = value.lines().take(12).collect::<Vec<_>>().join("\n");
            if value.lines().count() > 12 {
                format!("{preview}\n...")
            } else {
                preview
            }
        },
    )
}

fn view_setup_draft(chat: &mut ActiveChat) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let Some(draft) = ralph_state::latest_setup_draft(&repo_root)? else {
        chat.app.set_status("no Ralph setup draft found".to_owned());
        return Ok(());
    };
    let readiness = draft.readiness();
    chat.app.push_system_note(format!(
        "Ralph setup draft review\n* Draft: {}\n* Status: {}\n* Mode: {}\n* Target state: {}\n* Loop: {}\n* Branch: {}\n* Worktree: {}\n* Validation: {}\n* Ready: charter={} progress={} approved={}\n* Draft JSON: {}\n* Setup transcript: {}\n\nCharter preview:\n{}\n\nProgress preview:\n{}",
        draft.draft_id,
        draft.status,
        draft.mode,
        draft
            .target_state_dir
            .as_ref()
            .map_or_else(|| "<none>".to_owned(), |path| path.display().to_string()),
        draft.loop_name,
        draft.branch.as_deref().unwrap_or("<default>"),
        draft
            .work_area_path
            .as_ref()
            .map_or_else(|| "<default>".to_owned(), |path| path.display().to_string()),
        if draft.validation_commands.is_empty() {
            "<none>".to_owned()
        } else {
            draft.validation_commands.join("; ")
        },
        readiness.has_charter,
        readiness.has_progress,
        readiness.approved,
        draft.draft_path.display(),
        draft
            .setup_transcript_path
            .as_ref()
            .map_or_else(|| "<none>".to_owned(), |path| path.display().to_string()),
        markdown_preview(draft.charter_draft.as_deref()),
        markdown_preview(draft.progress_draft.as_deref())
    ));
    chat.app.set_status("Ralph setup draft shown".to_owned());
    Ok(())
}

fn revision_prompt(draft: &ralph_state::RalphSetupDraft) -> String {
    format!(
        "Revise Ralph setup draft `{draft_id}`.\n\n\
         Goal: improve the saved setup draft, not create files yet. Preserve correct constraints and decisions, fix weak/missing sections, and ask focused questions only if essential.\n\n\
         Required output shape:\n\n\
         RALPH_SETUP_DRAFT_START\n\
         loop_name: <name>\n\
         branch: <optional branch name or <none>>\n\
         worktree_path: <optional absolute path or <none>>\n\
         validation:\n\
         - <command>\n\n\
         --- charter.md ---\n\
         <complete revised charter markdown>\n\n\
         --- progress.md ---\n\
         <complete revised progress markdown with actionable checklist items>\n\
         RALPH_SETUP_DRAFT_END\n\n\
         Current draft metadata:\n\
         * Status: {status}\n\
         * Loop: {loop_name}\n\
         * Branch: {branch}\n\
         * Worktree: {worktree}\n\
         * Validation: {validation}\n\n\
         Current charter draft:\n\n{charter}\n\n\
         Current progress draft:\n\n{progress}",
        draft_id = draft.draft_id,
        status = draft.status,
        loop_name = draft.loop_name,
        branch = draft.branch.as_deref().unwrap_or("<default>"),
        worktree = draft
            .work_area_path
            .as_ref()
            .map_or_else(|| "<default>".to_owned(), |path| path.display().to_string()),
        validation = if draft.validation_commands.is_empty() {
            "<none>".to_owned()
        } else {
            draft.validation_commands.join("; ")
        },
        charter = draft.charter_draft.as_deref().unwrap_or("<missing>"),
        progress = draft.progress_draft.as_deref().unwrap_or("<missing>")
    )
}

fn revise_setup_draft(chat: &mut ActiveChat) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let Some(draft) = ralph_state::latest_setup_draft(&repo_root)? else {
        chat.app.set_status("no Ralph setup draft found".to_owned());
        return Ok(());
    };
    let updated = ralph_state::update_setup_draft(ralph_state::RalphSetupDraftUpdateRequest {
        draft_id: draft.draft_id,
        repo_root,
        status: ralph_state::RalphSetupDraftStatus::Drafting,
        loop_name: None,
        charter_draft: draft.charter_draft,
        progress_draft: draft.progress_draft,
        validation_commands: draft.validation_commands,
        branch: draft.branch,
        work_area_path: draft.work_area_path,
    })?;
    let prompt = revision_prompt(&updated);
    append_setup_transcript(
        &updated,
        &format!("## Requested setup draft revision\n\n{prompt}"),
    )?;
    chat.app.composer_mut().clear();
    chat.app.composer_mut().insert_str(&prompt);
    chat.app.push_system_note(format!(
        "Ralph setup draft revision prompt prepared\n* Draft: {}\n* Status: {}\n* Next: submit the prompt, then use Save setup draft on the assistant's revised artifact",
        updated.draft_id, updated.status
    ));
    chat.app
        .set_status("Ralph setup draft revision prompt prepared".to_owned());
    Ok(())
}

fn approve_setup_draft(chat: &mut ActiveChat) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let Some(draft) = ralph_state::latest_setup_draft(&repo_root)? else {
        chat.app.set_status("no Ralph setup draft found".to_owned());
        return Ok(());
    };
    let readiness = draft.readiness();
    if !readiness.has_charter || !readiness.has_progress {
        chat.app.push_system_note(format!(
            "Ralph setup draft is not ready for approval\n* Draft: {}\n* Has charter: {}\n* Has progress: {}\n* Next: ask the assistant to produce explicit charter.md and progress.md drafts, then save setup draft again",
            draft.draft_id, readiness.has_charter, readiness.has_progress
        ));
        chat.app
            .set_status("Ralph setup draft missing charter/progress".to_owned());
        return Ok(());
    }
    let updated = ralph_state::update_setup_draft(ralph_state::RalphSetupDraftUpdateRequest {
        draft_id: draft.draft_id,
        repo_root,
        status: ralph_state::RalphSetupDraftStatus::Approved,
        loop_name: None,
        charter_draft: draft.charter_draft,
        progress_draft: draft.progress_draft,
        validation_commands: draft.validation_commands,
        branch: draft.branch,
        work_area_path: draft.work_area_path,
    })?;
    append_setup_transcript(
        &updated,
        &format!(
            "## Approved setup draft\n\nDraft `{}` approved.",
            updated.draft_id
        ),
    )?;
    chat.app.push_system_note(format!(
        "Ralph setup draft approved\n* Draft: {}\n* Path: {}\n* Next: {}",
        updated.draft_id,
        updated.draft_path.display(),
        if updated.mode == ralph_state::RalphSetupDraftMode::RebuildExistingLoop {
            "apply draft to loop"
        } else {
            "create loop from draft"
        }
    ));
    chat.app.set_status("Ralph setup draft approved".to_owned());
    Ok(())
}

async fn create_loop_from_draft(
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let Some(draft) = ralph_state::latest_setup_draft(&repo_root)? else {
        chat.app.set_status("no Ralph setup draft found".to_owned());
        return Ok(());
    };
    if draft.mode != ralph_state::RalphSetupDraftMode::NewLoop {
        chat.app.push_system_note(format!(
            "Ralph setup draft is not a new-loop draft\n* Draft: {}\n* Mode: {}\n* Next: use Apply draft to loop for rebuild drafts",
            draft.draft_id, draft.mode
        ));
        chat.app
            .set_status("Ralph setup draft is not a new-loop draft".to_owned());
        return Ok(());
    }
    let readiness = draft.readiness();
    if !readiness.ready() {
        chat.app.push_system_note(format!(
            "Ralph setup draft is not approved for loop creation\n* Draft: {}\n* Has charter: {}\n* Has progress: {}\n* Approved: {}\n* Next: save and approve the setup draft before creating the loop",
            draft.draft_id, readiness.has_charter, readiness.has_progress, readiness.approved
        ));
        chat.app
            .set_status("Ralph setup draft is not ready".to_owned());
        return Ok(());
    }
    let state = ralph_state::create_loop_from_setup_draft(
        &draft.draft_id,
        &repo_root,
        chat.app.session_title(),
    )?;
    let work_area = services
        .client
        .create_worktree(WorktreeCreateRequest {
            name: format!("ralph-{}", draft.loop_name),
            cwd: Some(repo_root),
            path: draft.work_area_path.clone(),
            branch: None,
            new_branch: draft.branch.clone(),
            base_ref: Some(bcode_worktree_models::WorktreeBaseRef::Head),
            detach: false,
            force: false,
            attach_session_id: None,
            new_session: true,
            no_setup: false,
        })
        .await?;
    let work_area_session_id = work_area
        .session
        .as_ref()
        .map(|session| session.id.to_string());
    ralph_state::record_work_area(
        &state,
        &work_area.path,
        work_area.branch.as_deref(),
        work_area_session_id.as_deref(),
    )?;
    chat.app.push_system_note(format!(
        "Ralph loop created from setup draft\n* Draft: {}\n* Loop: {}\n* Charter: {}\n* Progress doc: {}\n* State: {}\n* Isolated work area: {}\n* Session: {}\n* Next: prepare a run, then approve/start it",
        draft.draft_id,
        draft.loop_name,
        state.charter_doc_path.display(),
        state.progress_doc_path.display(),
        state.state_dir.display(),
        work_area.path.display(),
        work_area_session_id.as_deref().unwrap_or("<none>")
    ));
    chat.app
        .set_status("Ralph loop created from setup draft".to_owned());
    Ok(())
}

fn latest_assistant_message(chat: &ActiveChat) -> Option<String> {
    chat.app
        .transcript()
        .iter()
        .rev()
        .find(|item| item.role == "assistant" && !item.text.trim().is_empty())
        .map(|item| item.text.clone())
}

fn append_setup_transcript(
    draft: &ralph_state::RalphSetupDraft,
    entry: &str,
) -> Result<(), TuiError> {
    let Some(path) = &draft.setup_transcript_path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut existing = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    if !existing.is_empty() {
        existing.push_str("\n\n");
    }
    existing.push_str(entry);
    existing.push('\n');
    std::fs::write(path, existing)?;
    Ok(())
}

fn extract_scalar_field(text: &str, field: &str) -> Option<String> {
    let prefix = format!("{field}:");
    text.lines()
        .find_map(|line| line.trim().strip_prefix(&prefix).map(str::trim))
        .filter(|value| !value.is_empty() && *value != "<none>")
        .map(ToOwned::to_owned)
}

fn extract_validation_commands(text: &str) -> Vec<String> {
    let Some(start) = text.find("validation:") else {
        return Vec::new();
    };
    let after_start = &text[start + "validation:".len()..];
    after_start
        .lines()
        .map(str::trim)
        .take_while(|line| !line.starts_with("--- ") && !line.ends_with(':'))
        .filter_map(|line| line.strip_prefix("- ").map(str::trim))
        .filter(|command| !command.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn extract_between(text: &str, start_marker: &str, end_marker: &str) -> Option<String> {
    let start = text.find(start_marker)? + start_marker.len();
    let after_start = &text[start..];
    let end = after_start.find(end_marker).unwrap_or(after_start.len());
    let content = after_start[..end]
        .trim()
        .trim_matches('`')
        .trim()
        .to_owned();
    (!content.is_empty()).then_some(content)
}

fn extract_markdown_section(text: &str, marker: &str) -> Option<String> {
    let dashed_marker = format!("--- {marker} ---");
    if marker == "charter.md" {
        return extract_between(text, &dashed_marker, "--- progress.md ---")
            .or_else(|| extract_between(text, marker, "progress.md"));
    }
    if marker == "progress.md" {
        return extract_between(text, &dashed_marker, "RALPH_SETUP_DRAFT_END")
            .or_else(|| extract_between(text, marker, "RALPH_SETUP_DRAFT_END"));
    }
    let start = text.find(marker)?;
    let after_marker = &text[start + marker.len()..];
    let content = after_marker
        .split("```")
        .nth(1)
        .map_or(after_marker, |fenced| fenced)
        .trim();
    (!content.is_empty()).then(|| content.to_owned())
}

fn save_setup_draft(chat: &mut ActiveChat) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let Some(draft) = ralph_state::latest_setup_draft(&repo_root)? else {
        chat.app.set_status("no Ralph setup draft found".to_owned());
        return Ok(());
    };
    let Some(message) = latest_assistant_message(chat) else {
        chat.app
            .set_status("no assistant draft found to save".to_owned());
        return Ok(());
    };
    let charter =
        extract_markdown_section(&message, "charter.md").or_else(|| draft.charter_draft.clone());
    let progress =
        extract_markdown_section(&message, "progress.md").or_else(|| draft.progress_draft.clone());
    let parsed_validation_commands = extract_validation_commands(&message);
    let validation_commands = if parsed_validation_commands.is_empty() {
        draft.validation_commands
    } else {
        parsed_validation_commands
    };
    let updated = ralph_state::update_setup_draft(ralph_state::RalphSetupDraftUpdateRequest {
        draft_id: draft.draft_id,
        repo_root,
        status: ralph_state::RalphSetupDraftStatus::DraftReady,
        loop_name: extract_scalar_field(&message, "loop_name"),
        charter_draft: charter,
        progress_draft: progress,
        validation_commands,
        branch: extract_scalar_field(&message, "branch").or(draft.branch),
        work_area_path: extract_scalar_field(&message, "worktree_path")
            .or_else(|| extract_scalar_field(&message, "work_area_path"))
            .map(PathBuf::from)
            .or(draft.work_area_path),
    })?;
    append_setup_transcript(
        &updated,
        &format!(
            "## Saved setup draft\n\nStatus: {}\n\n{}",
            updated.status, message
        ),
    )?;
    let readiness = updated.readiness();
    chat.app.push_system_note(format!(
        "Ralph setup draft saved\n* Draft: {}\n* Status: {}\n* Loop: {}\n* Branch: {}\n* Worktree: {}\n* Has charter: {}\n* Has progress: {}\n* Path: {}\n* Next: {}",
        updated.draft_id,
        updated.status,
        updated.loop_name,
        updated.branch.as_deref().unwrap_or("<default>"),
        updated
            .work_area_path
            .as_ref()
            .map_or_else(|| "<default>".to_owned(), |path| path.display().to_string()),
        readiness.has_charter,
        readiness.has_progress,
        updated.draft_path.display(),
        if readiness.has_charter && readiness.has_progress {
            "review the saved draft, then approve setup draft"
        } else {
            "ask the assistant for the exact RALPH_SETUP_DRAFT_START artifact, then save again"
        }
    ));
    chat.app.set_status("Ralph setup draft saved".to_owned());
    Ok(())
}

fn rebuild_setup_prompt(
    draft: &ralph_state::RalphSetupDraft,
    loop_summary: &ralph_state::RalphLoopSummary,
) -> String {
    format!(
        "We are rebuilding context for an existing Ralph loop because its current charter/progress may be nonsense or stubbed.\n\n\
         Existing loop:\n\
         * Name: {loop_name}\n\
         * State dir: {state_dir}\n\
         * Charter path: {charter_path}\n\
         * Progress path: {progress_path}\n\n\
         Treat this as a fresh setup pass for the existing loop. Do not preserve meaningless existing charter/progress text. Preserve only real constraints from the repo/session and ask clarifying questions if essential.\n\n\
         Produce the replacement artifact in this exact shape when ready:\n\n\
         RALPH_SETUP_DRAFT_START\n\
         loop_name: {draft_loop_name}\n\
         branch: <optional branch name or <none>>\n\
         worktree_path: <optional absolute path or <none>>\n\
         validation:\n\
         - <command>\n\n\
         --- charter.md ---\n\
         <complete replacement charter markdown>\n\n\
         --- progress.md ---\n\
         <complete replacement progress markdown with actionable checklist items>\n\
         RALPH_SETUP_DRAFT_END\n\n\
         Initial context pack:\n\n{source_context}",
        loop_name = loop_summary.loop_name,
        state_dir = loop_summary.state_dir.display(),
        charter_path = loop_summary.charter_doc_path.display(),
        progress_path = loop_summary.progress_doc_path.display(),
        draft_loop_name = draft.loop_name,
        source_context = draft.source_context
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RalphRebuildField {
    Intent,
    Problem,
    Constraints,
    Validation,
}

struct RalphRebuildDialog {
    intent: TextInputState,
    problem: TextInputState,
    constraints: TextInputState,
    validation: TextInputState,
    focused_field: RalphRebuildField,
    status: String,
}

impl RalphRebuildDialog {
    fn new(loop_summary: &ralph_state::RalphLoopSummary, validation_commands: &[String]) -> Self {
        Self {
            intent: TextInputState::new(TextEditBuffer::from_text(format!(
                "Rebuild this Ralph loop so it has accurate charter/progress context for {}.",
                loop_summary.loop_name
            ))),
            problem: TextInputState::new(TextEditBuffer::from_text(
                "The current charter/progress may be stubbed, nonsense, stale, or not grounded in the actual repository/session state.",
            )),
            constraints: TextInputState::new(TextEditBuffer::from_text(
                "Preserve run history. Do not overwrite files until I approve/apply the draft. Ignore meaningless existing context.",
            )),
            validation: TextInputState::new(TextEditBuffer::from_text(
                validation_commands.join("; "),
            )),
            focused_field: RalphRebuildField::Intent,
            status:
                "Fill in context, Tab between fields, Enter starts the guided Ralph conversation."
                    .to_owned(),
        }
    }

    const fn focused_input_mut(&mut self) -> &mut TextInputState {
        match self.focused_field {
            RalphRebuildField::Intent => &mut self.intent,
            RalphRebuildField::Problem => &mut self.problem,
            RalphRebuildField::Constraints => &mut self.constraints,
            RalphRebuildField::Validation => &mut self.validation,
        }
    }

    const fn focus_next(&mut self) {
        self.focused_field = match self.focused_field {
            RalphRebuildField::Intent => RalphRebuildField::Problem,
            RalphRebuildField::Problem => RalphRebuildField::Constraints,
            RalphRebuildField::Constraints => RalphRebuildField::Validation,
            RalphRebuildField::Validation => RalphRebuildField::Intent,
        };
    }

    fn validation_commands(&self) -> Vec<String> {
        self.validation
            .buffer()
            .text()
            .split(';')
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    }

    fn user_context(&self) -> String {
        format!(
            "User-provided rebuild context:\n\nIntent:\n{}\n\nCurrent problem:\n{}\n\nConstraints / non-goals:\n{}\n",
            self.intent.buffer().text().trim(),
            self.problem.buffer().text().trim(),
            self.constraints.buffer().text().trim()
        )
    }
}

fn render_rebuild_dialog(
    dialog: &mut RalphRebuildDialog,
    loop_summary: &ralph_state::RalphLoopSummary,
    frame: &mut Frame<'_>,
    theme: super::render::TuiTheme,
) {
    let modal = ModalFrame::new(
        ModalSizing::new(Size::new(96, 30), Size::new(118, 34), Insets::all(4)),
        ModalTheme::dark(theme.accent),
    )
    .title(" Ralph rebuild setup ")
    .padding(Insets::new(1, 2, 1, 2))
    .placement(ModalPlacement::UpperThird);
    modal.render(frame.area(), frame);
    let content = modal.content_area(frame.area());
    let mut row = content.y;
    render_rebuild_line(
        &modal,
        content,
        &mut row,
        frame,
        "Guided rebuild conversation for existing loop context. Nothing is overwritten here.",
    );
    render_rebuild_line(
        &modal,
        content,
        &mut row,
        frame,
        &format!("Loop: {}", loop_summary.loop_name),
    );
    render_rebuild_line(
        &modal,
        content,
        &mut row,
        frame,
        &format!("State: {}", loop_summary.state_dir.display()),
    );
    row = row.saturating_add(1);
    render_rebuild_input(
        dialog,
        &modal,
        content,
        &mut row,
        frame,
        RalphRebuildField::Intent,
        "Intent / goal",
    );
    render_rebuild_input(
        dialog,
        &modal,
        content,
        &mut row,
        frame,
        RalphRebuildField::Problem,
        "What is wrong now?",
    );
    render_rebuild_input(
        dialog,
        &modal,
        content,
        &mut row,
        frame,
        RalphRebuildField::Constraints,
        "Constraints / non-goals",
    );
    render_rebuild_input(
        dialog,
        &modal,
        content,
        &mut row,
        frame,
        RalphRebuildField::Validation,
        "Validation commands (; separated)",
    );
    row = row.saturating_add(1);
    render_rebuild_line(
        &modal,
        content,
        &mut row,
        frame,
        "After Enter, Bcode sends this scoped Ralph rebuild turn automatically; no composer handoff.",
    );
    render_rebuild_line(
        &modal,
        content,
        &mut row,
        frame,
        "Next response stays in the chat transcript, then use Save/View/Approve/Apply in Ralph.",
    );
    row = content.bottom().saturating_sub(2);
    render_rebuild_line(
        &modal,
        content,
        &mut row,
        frame,
        "Enter start conversation · Tab field · Esc cancel",
    );
    render_rebuild_line(&modal, content, &mut row, frame, &dialog.status);
}

fn render_rebuild_line(
    modal: &ModalFrame,
    content: Rect,
    row: &mut u16,
    frame: &mut Frame<'_>,
    text: &str,
) {
    if *row >= content.bottom() {
        return;
    }
    modal.render_line(
        Rect::new(content.x, *row, content.width, 1),
        &Line::from(text.to_owned()),
        frame,
    );
    *row = row.saturating_add(1);
}

fn render_rebuild_input(
    dialog: &mut RalphRebuildDialog,
    modal: &ModalFrame,
    content: Rect,
    row: &mut u16,
    frame: &mut Frame<'_>,
    field: RalphRebuildField,
    label: &str,
) {
    render_rebuild_line(modal, content, row, frame, label);
    if *row >= content.bottom() {
        return;
    }
    let input_area = Rect::new(content.x, *row, content.width, 1);
    let focused = dialog.focused_field == field;
    let input = match field {
        RalphRebuildField::Intent => &mut dialog.intent,
        RalphRebuildField::Problem => &mut dialog.problem,
        RalphRebuildField::Constraints => &mut dialog.constraints,
        RalphRebuildField::Validation => &mut dialog.validation,
    };
    input.set_content_area(input_area, &ralph_start_dialog::input_policy());
    TextInput::new(input.buffer())
        .style(Style::new().fg(Color::Yellow).bg(Color::Black))
        .selection_style(Style::new().fg(Color::Black).bg(Color::Yellow))
        .vertical_scroll(input.vertical_scroll())
        .cursor_visible(focused)
        .render(input_area, frame);
    *row = row.saturating_add(2);
}

async fn collect_rebuild_context<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    loop_summary: &ralph_state::RalphLoopSummary,
    default_validation_commands: &[String],
) -> Result<Option<RalphRebuildDialog>, TuiError> {
    let mut dialog = RalphRebuildDialog::new(loop_summary, default_validation_commands);
    loop {
        io.terminal.resize(helpers::terminal_area()?);
        io.terminal.draw(|frame| {
            render_rebuild_dialog(&mut dialog, loop_summary, frame, services.theme);
        })?;
        let Some(event) = io.input.recv().await? else {
            continue;
        };
        match event {
            Event::Resize(size) => io.terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => {
                dialog.focused_input_mut().buffer_mut().insert_str(&text);
                dialog
                    .focused_input_mut()
                    .sync_scroll_to_cursor(&ralph_start_dialog::input_policy());
            }
            Event::Key(key) => match key.key {
                KeyCode::Escape => return Ok(None),
                KeyCode::Tab => dialog.focus_next(),
                KeyCode::Enter => return Ok(Some(dialog)),
                _ => handle_rebuild_dialog_key(&mut dialog, services.keymap, key),
            },
            Event::Mouse(_) | Event::Focus(_) | Event::Tick | Event::User(_) => {}
        }
    }
}

fn handle_rebuild_dialog_key(
    dialog: &mut RalphRebuildDialog,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) {
    if let Some(motion) = keymap.editor_selection_motion_for_key(stroke) {
        dialog
            .focused_input_mut()
            .buffer_mut()
            .move_cursor_with_selection(motion, SelectionMode::Extend);
        dialog
            .focused_input_mut()
            .sync_scroll_to_cursor(&ralph_start_dialog::input_policy());
        return;
    }
    if let Some(command) = keymap.editor_command_for_key(stroke) {
        dialog
            .focused_input_mut()
            .buffer_mut()
            .apply_command(command);
        dialog
            .focused_input_mut()
            .sync_scroll_to_cursor(&ralph_start_dialog::input_policy());
        return;
    }
    let _ = TextInputControl::new(&ralph_start_dialog::input_policy())
        .handle_key(dialog.focused_input_mut(), stroke);
}

/// Start an LLM-guided rebuild setup draft for the latest existing loop.
pub async fn rebuild_loop_context<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let Some(loop_summary) = ralph_state::latest_loop(&repo_root)? else {
        chat.app
            .set_status("no Ralph loop found to rebuild".to_owned());
        return Ok(());
    };
    let default_validation_commands = ralph_state::default_validation_commands(&repo_root);
    let Some(dialog) =
        collect_rebuild_context(io, services, &loop_summary, &default_validation_commands).await?
    else {
        chat.app.set_status("Ralph rebuild canceled".to_owned());
        return Ok(());
    };
    let mut source_context = setup_source_context(services, chat).await?;
    source_context.push_str("\n\n");
    source_context.push_str(&dialog.user_context());
    source_context.push_str("\nExisting Ralph loop files:\n* charter: ");
    source_context.push_str(&loop_summary.charter_doc_path.display().to_string());
    source_context.push_str("\n* progress: ");
    source_context.push_str(&loop_summary.progress_doc_path.display().to_string());
    source_context.push_str("\n* state dir: ");
    source_context.push_str(&loop_summary.state_dir.display().to_string());
    source_context.push('\n');
    let draft = ralph_state::create_setup_draft(ralph_state::RalphSetupDraftCreateRequest {
        repo_root: repo_root.clone(),
        loop_name: loop_summary.loop_name.clone(),
        session_title: chat.app.session_title().map(ToOwned::to_owned),
        source_context,
        validation_commands: dialog.validation_commands(),
        mode: ralph_state::RalphSetupDraftMode::RebuildExistingLoop,
        target_state_dir: Some(loop_summary.state_dir.clone()),
    })?;
    let prompt = rebuild_setup_prompt(&draft, &loop_summary);
    append_setup_transcript(
        &draft,
        &format!(
            "## Created rebuild setup draft\n\nTarget state dir: {}\n\n{}",
            loop_summary.state_dir.display(),
            prompt
        ),
    )?;
    let session_id =
        super::session_flow::persist_draft_session(io.terminal, services.client, chat).await?;
    services
        .client
        .send_user_message(
            session_id,
            prompt.clone(),
            bcode_ipc::PromptPlacement::FollowUp,
        )
        .await?;
    chat.app.push_system_note(format!(
        "Ralph rebuild conversation started\n* Draft: {}\n* Target loop: {}\n* Target state: {}\n* Transcript: {}\n* Assistant turn: started automatically from the guided rebuild context\n* Next: answer Ralph's questions here, then Save/View/Approve/Apply the setup draft from Ralph UI",
        draft.draft_id,
        loop_summary.loop_name,
        loop_summary.state_dir.display(),
        draft
            .setup_transcript_path
            .as_ref()
            .map_or_else(|| "<none>".to_owned(), |path| path.display().to_string())
    ));
    chat.app
        .set_status("Ralph rebuild conversation started".to_owned());
    Ok(())
}

fn apply_draft_to_existing_loop(chat: &mut ActiveChat) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let Some(draft) = ralph_state::latest_setup_draft(&repo_root)? else {
        chat.app.set_status("no Ralph setup draft found".to_owned());
        return Ok(());
    };
    if draft.mode != ralph_state::RalphSetupDraftMode::RebuildExistingLoop {
        chat.app.push_system_note(format!(
            "Ralph setup draft is not a rebuild draft\n* Draft: {}\n* Mode: {}\n* Next: use Create loop from draft for new-loop drafts, or start Rebuild loop context",
            draft.draft_id, draft.mode
        ));
        chat.app
            .set_status("Ralph setup draft is not a rebuild draft".to_owned());
        return Ok(());
    }
    let readiness = draft.readiness();
    if !readiness.ready() {
        chat.app.push_system_note(format!(
            "Ralph rebuild draft is not ready to apply\n* Draft: {}\n* Has charter: {}\n* Has progress: {}\n* Approved: {}\n* Next: save and approve the rebuild draft before applying it",
            draft.draft_id, readiness.has_charter, readiness.has_progress, readiness.approved
        ));
        chat.app
            .set_status("Ralph rebuild draft is not ready".to_owned());
        return Ok(());
    }
    let result = ralph_state::apply_setup_draft_to_existing_loop(&draft.draft_id, &repo_root)?;
    chat.app.push_system_note(format!(
        "Ralph loop context rebuilt\n* State dir: {}\n* Backups: {}\n* Charter: {}\n* Progress: {}\n* Run history was preserved",
        result.state_dir.display(),
        result.backup_dir.display(),
        result.charter_doc_path.display(),
        result.progress_doc_path.display()
    ));
    chat.app
        .set_status("Ralph loop context rebuilt from draft".to_owned());
    Ok(())
}

/// Start an LLM-guided Ralph setup draft instead of immediately creating loop files.
pub async fn plan_loop(services: &TuiServices<'_>, chat: &mut ActiveChat) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let default_name = chat
        .app
        .session_title()
        .map_or_else(|| "new-ralph-loop".to_owned(), ToString::to_string);
    let validation_commands = ralph_state::default_validation_commands(&repo_root);
    let source_context = setup_source_context(services, chat).await?;
    let draft = ralph_state::create_setup_draft(ralph_state::RalphSetupDraftCreateRequest {
        repo_root: repo_root.clone(),
        loop_name: default_name,
        session_title: chat.app.session_title().map(ToOwned::to_owned),
        source_context,
        validation_commands,
        mode: ralph_state::RalphSetupDraftMode::NewLoop,
        target_state_dir: None,
    })?;
    let prompt = guided_setup_prompt(&draft);
    append_setup_transcript(
        &draft,
        &format!(
            "## Created setup draft\n\nRepo: {}\n\nInitial context:\n\n{}",
            repo_root.display(),
            draft.source_context
        ),
    )?;
    chat.app.push_system_note(format!(
        "Ralph guided setup draft created\n* Draft: {}\n* Status: {}\n* Repo: {}\n* Next: answer the assistant's clarifying questions; approve only after charter/progress are meaningful",
        draft.draft_id,
        draft.status,
        repo_root.display()
    ));
    chat.app.composer_mut().clear();
    chat.app.composer_mut().insert_str(&prompt);
    chat.app.set_status(
        "Ralph setup draft created; submit the guided setup prompt to start planning".to_owned(),
    );
    Ok(())
}

async fn setup_source_context(
    services: &TuiServices<'_>,
    chat: &ActiveChat,
) -> Result<String, TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        return Ok("No active session history was available. Ask the user for the loop goal, constraints, non-goals, validation expectations, and definition of done.".to_owned());
    };
    let history = services
        .client
        .session_history_page(
            session_id,
            SessionHistoryQuery {
                cursor: None,
                limit: 64,
                direction: SessionHistoryDirection::Backward,
            },
        )
        .await?;
    Ok(history
        .events
        .iter()
        .filter_map(|event| match &event.kind {
            bcode_session_models::SessionEventKind::UserMessage { text, .. } => {
                Some(format!("User: {text}"))
            }
            bcode_session_models::SessionEventKind::AssistantMessage { text, .. } => {
                Some(format!("Assistant: {text}"))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n"))
}

fn guided_setup_prompt(draft: &ralph_state::RalphSetupDraft) -> String {
    format!(
        "Start Ralph guided setup for draft `{draft_id}`.\n\n\
         Goal: do not create loop files yet. First help me clarify the goal, constraints, non-goals, validation, and definition of done. Ask focused clarifying questions until the loop is well specified. Then draft a meaningful `charter.md` and `progress.md` for review.\n\n\
         Required process:\n\
         1. Summarize what you understand from the context.\n\
         2. Ask the minimum necessary clarifying questions.\n\
         3. After answers, produce a final setup artifact in this exact shape so Ralph can save it reliably:\n\n\
         RALPH_SETUP_DRAFT_START\n\
         loop_name: <name>\n\
         branch: <optional branch name or <none>>\n\
         worktree_path: <optional absolute path or <none>>\n\
         validation:\n\
         - <command>\n\n\
         --- charter.md ---\n\
         <complete charter markdown>\n\n\
         --- progress.md ---\n\
         <complete progress markdown with actionable checklist items>\n\
         RALPH_SETUP_DRAFT_END\n\n\
         4. Do not claim setup is complete until I explicitly approve creating the loop from the draft.\n\n\
         Current draft status: {status}\n\
         Proposed loop name: {loop_name}\n\
         Validation commands: {validation}\n\n\
         Captured context:\n{context}",
        draft_id = draft.draft_id,
        status = draft.status,
        loop_name = draft.loop_name,
        validation = if draft.validation_commands.is_empty() {
            "<none>".to_owned()
        } else {
            draft.validation_commands.join("; ")
        },
        context = draft.source_context
    )
}

async fn dispatch_home_action<W: Write>(
    action: super::ralph_launcher::RalphHomeAction,
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    match action {
        super::ralph_launcher::RalphHomeAction::Plan => plan_loop(services, chat).await,
        super::ralph_launcher::RalphHomeAction::SaveDraft => save_setup_draft(chat),
        super::ralph_launcher::RalphHomeAction::ViewDraft => view_setup_draft(chat),
        super::ralph_launcher::RalphHomeAction::ReviseDraft => revise_setup_draft(chat),
        super::ralph_launcher::RalphHomeAction::RebuildLoopContext => {
            rebuild_loop_context(io, services, chat).await
        }
        super::ralph_launcher::RalphHomeAction::ApproveDraft => approve_setup_draft(chat),
        super::ralph_launcher::RalphHomeAction::CreateFromDraft => {
            create_loop_from_draft(services, chat).await
        }
        super::ralph_launcher::RalphHomeAction::ApplyDraftToLoop => {
            apply_draft_to_existing_loop(chat)
        }
        super::ralph_launcher::RalphHomeAction::Start => start_loop(io, services, chat).await,
        super::ralph_launcher::RalphHomeAction::Run
        | super::ralph_launcher::RalphHomeAction::Goal => run_loop(services, chat).await,
        super::ralph_launcher::RalphHomeAction::Approve => approve_run(services, chat).await,
        super::ralph_launcher::RalphHomeAction::Stop => stop_loop(services, chat).await,
        super::ralph_launcher::RalphHomeAction::Resume => resume_run(services, chat).await,
        super::ralph_launcher::RalphHomeAction::Status => show_status(services, chat).await,
        super::ralph_launcher::RalphHomeAction::Runs => list_runs(services, chat).await,
        super::ralph_launcher::RalphHomeAction::Iterations => list_iterations(services, chat).await,
        super::ralph_launcher::RalphHomeAction::Open => open_progress(chat),
        super::ralph_launcher::RalphHomeAction::Audit => {
            show_prompt(chat, ralph_state::RalphPromptKind::Audit)
        }
        super::ralph_launcher::RalphHomeAction::Replan => {
            show_prompt(chat, ralph_state::RalphPromptKind::Replan)
        }
    }
}

/// Show latest Ralph loop status for the current repository.
pub async fn show_status(
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let response = services
        .client
        .ralph_run_status(RalphRunStatusRequest {
            repo_root,
            loop_state_dir: None,
        })
        .await?;
    let Some(summary) = response.loop_summary else {
        chat.app
            .set_status("no Ralph loops for current repository".to_owned());
        return Ok(());
    };
    chat.app.push_system_note(format_status_note(
        &summary,
        response.active_run.as_ref(),
        response.interrupted_runs.len(),
    ));
    chat.app.set_status("Ralph status shown".to_owned());
    Ok(())
}

/// Prepare the latest Ralph loop through the server-side runner API.
pub async fn run_loop(services: &TuiServices<'_>, chat: &mut ActiveChat) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let response = services
        .client
        .run_ralph_loop(RalphRunRequest {
            repo_root,
            loop_state_dir: None,
            max_iterations: None,
            no_progress_limit: None,
            require_approval: true,
        })
        .await?;
    chat.app.push_system_note(format!(
        "Ralph run prepared\n* Run: {}\n* Status: {}\n* State: {}\n* Session: {}\n* Next: /ralph approve",
        response.run.run_id,
        response.run.status,
        response.run.state_dir.display(),
        response.run.session_id.as_deref().unwrap_or("<none>")
    ));
    chat.app
        .set_status("Ralph run prepared; approve to start".to_owned());
    Ok(())
}

/// Approve and start the latest prepared Ralph run through the server-side runner API.
pub async fn approve_run(
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let response = services
        .client
        .approve_ralph_run(RalphApproveRequest {
            repo_root,
            loop_state_dir: None,
            run_id: None,
        })
        .await?;
    chat.app.push_system_note(format!(
        "Ralph run approved\n* Run: {}\n* Status: {}\n* State: {}\n* Session: {}",
        response.run.run_id,
        response.run.status,
        response.run.state_dir.display(),
        response.run.session_id.as_deref().unwrap_or("<none>")
    ));
    chat.app.set_status("Ralph run approved".to_owned());
    Ok(())
}

/// List recent Ralph runs for the current repository.
pub async fn list_runs(services: &TuiServices<'_>, chat: &mut ActiveChat) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let response = services
        .client
        .list_ralph_runs(RalphListRunsRequest {
            repo_root,
            loop_state_dir: None,
        })
        .await?;
    let Some(summary) = response.loop_summary else {
        chat.app
            .set_status("no Ralph loops for current repository".to_owned());
        return Ok(());
    };
    let runs = if response.runs.is_empty() {
        "* <none>".to_owned()
    } else {
        response
            .runs
            .iter()
            .map(|run| {
                format!(
                    "* {} — {}{}",
                    run.run_id,
                    run.status,
                    run.stop_reason
                        .as_deref()
                        .map_or_else(String::new, |reason| format!(" ({reason})"))
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    chat.app.push_system_note(format!(
        "Ralph runs\n* Loop: {}\n{}",
        summary.loop_name, runs
    ));
    chat.app.set_status("Ralph runs shown".to_owned());
    Ok(())
}

/// List iterations for the latest Ralph run in the current repository.
pub async fn list_iterations(
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let response = services
        .client
        .list_ralph_iterations(RalphListIterationsRequest {
            repo_root,
            loop_state_dir: None,
            run_id: None,
        })
        .await?;
    let Some(summary) = response.loop_summary else {
        chat.app
            .set_status("no Ralph loops for current repository".to_owned());
        return Ok(());
    };
    let run_label = response
        .run
        .as_ref()
        .map_or_else(|| "<none>".to_owned(), |run| run.run_id.clone());
    let iterations = if response.iterations.is_empty() {
        "* <none>".to_owned()
    } else {
        response
            .iterations
            .iter()
            .map(|iteration| {
                format!(
                    "* #{} — {}{}",
                    iteration.iteration_number,
                    iteration.status,
                    iteration
                        .stop_reason
                        .as_deref()
                        .map_or_else(String::new, |reason| format!(" ({reason})"))
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let validations = if response.validations.is_empty() {
        "* <none>".to_owned()
    } else {
        response
            .validations
            .iter()
            .map(|validation| {
                format!(
                    "* {} — {}{}",
                    validation.command,
                    validation.status,
                    validation
                        .exit_code
                        .map_or_else(String::new, |code| format!(" (exit {code})"))
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    chat.app.push_system_note(format!(
        "Ralph iterations\n* Loop: {}\n* Run: {}\nIterations:\n{}\nValidations:\n{}",
        summary.loop_name, run_label, iterations, validations
    ));
    chat.app.set_status("Ralph iterations shown".to_owned());
    Ok(())
}

/// Prepare an approval-gated resume run for the latest interrupted Ralph run.
pub async fn resume_run(services: &TuiServices<'_>, chat: &mut ActiveChat) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let response = services
        .client
        .resume_ralph_run(RalphResumeRequest {
            repo_root,
            loop_state_dir: None,
            interrupted_run_id: None,
        })
        .await?;
    chat.app.push_system_note(format!(
        "Ralph resume prepared\n* Interrupted run: {}\n* New run: {}\n* Status: {}\n* Next: approve before autonomous execution continues",
        response.interrupted_run.run_id,
        response.resumed_run.run_id,
        response.resumed_run.status
    ));
    chat.app
        .set_status("Ralph resume prepared; approval required".to_owned());
    Ok(())
}

/// Request cancellation for the active Ralph loop run.
pub async fn stop_loop(services: &TuiServices<'_>, chat: &mut ActiveChat) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let response = services
        .client
        .cancel_ralph_loop(RalphCancelRequest {
            repo_root,
            run_id: None,
            loop_state_dir: None,
        })
        .await?;
    chat.app.push_system_note(format!(
        "Ralph stop requested\n* Run: {}\n* Status: {}\n* Cancel requested: {}",
        response.run.run_id, response.run.status, response.cancel_requested
    ));
    chat.app.set_status("Ralph stop requested".to_owned());
    Ok(())
}

fn format_status_note(
    summary: &RalphStatusSummary,
    active_run: Option<&RalphRunSummary>,
    interrupted_run_count: usize,
) -> String {
    let run_status = active_run.map_or_else(
        || "none".to_owned(),
        |run| {
            format!(
                "{} ({}){}{}{}",
                run.run_id,
                run.status,
                run.runtime_work_id
                    .as_deref()
                    .map_or_else(String::new, |work_id| format!(", work: {work_id}")),
                run.stop_reason
                    .as_deref()
                    .map_or_else(String::new, |reason| format!(", stop: {reason}")),
                if run.cancel_requested {
                    ", cancel requested"
                } else {
                    ""
                }
            )
        },
    );
    let validation_commands = if summary.validation_commands.is_empty() {
        "<none>".to_owned()
    } else {
        summary.validation_commands.join("; ")
    };
    format!(
        "Ralph loop status\n* Loop: {}\n* Status: {}\n* Active run: {}\n* Interrupted runs: {}\n* Iterations: {}\n* Checklist: {} checked, {} unchecked\n* Validation: {}\n* Next: {}\n* Progress doc: {}\n* State: {}\n* Isolated work area: {}\n* Session: {}",
        summary.loop_name,
        summary.status,
        run_status,
        interrupted_run_count,
        summary.iteration_count,
        summary.checked_count,
        summary.unchecked_count,
        validation_commands,
        summary.next_action,
        summary.progress_doc_path.display(),
        summary.state_dir.display(),
        summary
            .work_area_path
            .as_ref()
            .map_or_else(|| "<none>".to_owned(), |path| path.display().to_string()),
        summary.session_id.as_deref().unwrap_or("<none>")
    )
}

/// Build and show a Ralph orchestration prompt for the current repository.
pub fn show_prompt(
    chat: &mut ActiveChat,
    kind: ralph_state::RalphPromptKind,
) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let Some(summary) = ralph_state::latest_loop(&repo_root)? else {
        chat.app
            .set_status("no Ralph loops for current repository".to_owned());
        return Ok(());
    };
    let prompt = ralph_state::build_prompt(&summary, kind)?;
    ralph_state::append_lifecycle_event_for_summary(
        &summary,
        ralph_state::RalphLifecycleEventKind::PromptPrepared,
        "Prepared Ralph orchestration prompt",
    )?;
    chat.app.push_system_note(format!(
        "Ralph prompt prepared\n* Loop: {}\n* Progress doc: {}\n\n{}",
        summary.loop_name,
        summary.progress_doc_path.display(),
        prompt
    ));
    chat.app
        .set_status("Ralph prompt prepared; submit manually when ready".to_owned());
    Ok(())
}

/// Show latest Ralph progress doc path for the current repository.
pub fn open_progress(chat: &mut ActiveChat) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let Some(summary) = ralph_state::latest_loop(&repo_root)? else {
        chat.app
            .set_status("no Ralph loops for current repository".to_owned());
        return Ok(());
    };
    ralph_state::append_lifecycle_event_for_summary(
        &summary,
        ralph_state::RalphLifecycleEventKind::ProgressOpened,
        "Viewed Ralph progress doc path",
    )?;
    chat.app.push_system_note(format!(
        "Ralph progress doc\n* Loop: {}\n* Path: {}",
        summary.loop_name,
        summary.progress_doc_path.display()
    ));
    chat.app
        .set_status("Ralph progress doc path shown".to_owned());
    Ok(())
}

fn current_repo_root(chat: &ActiveChat) -> Result<std::path::PathBuf, TuiError> {
    chat.app
        .working_directory()
        .map_or_else(std::env::current_dir, |path| Ok(path.to_path_buf()))
        .map_err(TuiError::Io)
}

/// Start the Ralph loop setup flow.
pub async fn start_loop<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let default_name = chat
        .app
        .session_title()
        .map_or_else(|| "new-ralph-loop".to_owned(), ToString::to_string);
    let repo_root = chat
        .app
        .working_directory()
        .map_or_else(std::env::current_dir, |path| Ok(path.to_path_buf()))?;
    let default_validation_commands = ralph_state::default_validation_commands(&repo_root);
    let mut dialog =
        ralph_start_dialog::RalphStartDialog::new(&default_name, &default_validation_commands);
    loop {
        io.terminal.resize(helpers::terminal_area()?);
        io.terminal.draw(|frame| {
            ralph_start_dialog_render::render_dialog(&mut dialog, frame, services.theme);
        })?;
        let Some(event) = io.input.recv().await? else {
            continue;
        };
        match event {
            Event::Resize(size) => io.terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => {
                let _ = TextInputControl::new(&ralph_start_dialog::input_policy())
                    .handle_paste(dialog.focused_input_mut(), &text);
            }
            Event::Key(stroke) => match stroke.key {
                KeyCode::Escape => return Err(TuiError::Canceled),
                KeyCode::Tab => dialog.focus_next(),
                KeyCode::Enter => {
                    if confirm_start_loop(&mut dialog, services, chat).await? {
                        return Ok(());
                    }
                }
                _ => handle_loop_name_key(&mut dialog, services.keymap, stroke),
            },
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
            Event::Mouse(mouse) => handle_loop_name_mouse(&mut dialog, mouse),
        }
    }
}

async fn confirm_start_loop(
    dialog: &mut ralph_start_dialog::RalphStartDialog,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<bool, TuiError> {
    let loop_name = dialog.loop_name_text();
    if loop_name.is_empty() {
        dialog.set_status("Ralph loop name is required");
        return Ok(false);
    }
    let repo_root = chat
        .app
        .working_directory()
        .map_or_else(std::env::current_dir, |path| Ok(path.to_path_buf()))?;
    let state =
        ralph_state::create_initial_loop_state(&loop_name, &repo_root, chat.app.session_title())?;
    let validation_commands = dialog.validation_command_texts();
    ralph_state::set_validation_commands(&state.state_dir, &validation_commands, "setup")?;
    if let Some(session_id) = chat.app.session_id() {
        let history = services
            .client
            .session_history_page(
                session_id,
                SessionHistoryQuery {
                    cursor: None,
                    limit: 64,
                    direction: SessionHistoryDirection::Backward,
                },
            )
            .await?;
        ralph_state::write_context_pack(&state, chat.app.session_title(), &history.events)?;
        ralph_state::generate_progress_doc_from_context(&state, &loop_name, &repo_root)?;
    }
    let work_area = services
        .client
        .create_worktree(WorktreeCreateRequest {
            name: format!("ralph-{loop_name}"),
            cwd: Some(repo_root),
            path: dialog.work_area_path_text().map(PathBuf::from),
            branch: None,
            new_branch: dialog.branch_text(),
            base_ref: Some(bcode_worktree_models::WorktreeBaseRef::Head),
            detach: false,
            force: false,
            attach_session_id: None,
            new_session: true,
            no_setup: false,
        })
        .await?;
    let work_area_session_id = work_area
        .session
        .as_ref()
        .map(|session| session.id.to_string());
    ralph_state::record_work_area(
        &state,
        &work_area.path,
        work_area.branch.as_deref(),
        work_area_session_id.as_deref(),
    )?;
    if let Some(session) = &work_area.session {
        let _event = services
            .client
            .record_ralph_lifecycle(RalphLifecycleRequest {
                session_id: session.id,
                loop_name: loop_name.clone(),
                state_dir: state.state_dir.clone(),
                kind: "work_area_created".to_owned(),
                message: "Created Ralph isolated work area".to_owned(),
                occurred_at_ms: now_ms(),
            })
            .await?;
    }
    let validation_summary = if validation_commands.is_empty() {
        "<none>".to_owned()
    } else {
        validation_commands.join("; ")
    };
    chat.app.push_system_note(format!(
        "Ralph loop created\n* Loop: {loop_name}\n* Charter: {}\n* Progress doc: {}\n* State: {}\n* Isolated work area: {}\n* Session: {}\n* Validation: {}\n* Next: review docs if desired, then prepare a run and approve/start it",
        state.charter_doc_path.display(),
        state.progress_doc_path.display(),
        state.state_dir.display(),
        work_area.path.display(),
        work_area_session_id.as_deref().unwrap_or("<none>"),
        validation_summary
    ));
    chat.app.set_status("Ralph loop created".to_owned());
    Ok(true)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn handle_loop_name_key(
    dialog: &mut ralph_start_dialog::RalphStartDialog,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) {
    if let Some(motion) = keymap.editor_selection_motion_for_key(stroke) {
        dialog
            .focused_input_mut()
            .buffer_mut()
            .move_cursor_with_selection(motion, SelectionMode::Extend);
        dialog
            .focused_input_mut()
            .sync_scroll_to_cursor(&ralph_start_dialog::input_policy());
        return;
    }
    if let Some(command) = keymap.editor_command_for_key(stroke) {
        dialog
            .focused_input_mut()
            .buffer_mut()
            .apply_command(command);
        dialog
            .focused_input_mut()
            .sync_scroll_to_cursor(&ralph_start_dialog::input_policy());
        return;
    }
    let _ = TextInputControl::new(&ralph_start_dialog::input_policy())
        .handle_key(dialog.focused_input_mut(), stroke);
}

fn handle_loop_name_mouse(dialog: &mut ralph_start_dialog::RalphStartDialog, mouse: MouseEvent) {
    let _ = TextInputControl::new(&ralph_start_dialog::input_policy())
        .handle_mouse(dialog.focused_input_mut(), mouse);
}
