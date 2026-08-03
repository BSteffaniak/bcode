//! Ralph loop TUI flow.

use std::path::PathBuf;

use bcode_ipc::{
    RalphApproveRequest, RalphCancelRequest, RalphLifecycleRequest, RalphListIterationsRequest,
    RalphListRunsRequest, RalphResumeRequest, RalphRunRequest, RalphRunStatusRequest,
    RalphRunSummary, RalphStatusSummary,
};
use bcode_plugin_sdk::path::display_from_current_dir;
use bcode_ralph as ralph_state;
use bcode_session_models::{SessionHistoryDirection, SessionHistoryQuery};
use bcode_worktree_models::WorktreeCreateRequest;

use super::TuiError;
use super::session_flow::ActiveChat;

/// Ralph action that does not require a nested terminal screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RalphRootAction {
    ShowStatus,
    Run,
    Approve,
    Stop,
    ListRuns,
    ListIterations,
    Resume,
    Plan,
    SaveDraft,
    ViewDraft,
    ReviseDraft,
    ApproveDraft,
    ApplyDraftToLoop,
    CreateFromDraft,
    OpenProgress,
    Audit,
    Replan,
}

impl RalphRootAction {
    pub const fn requires_client(self) -> bool {
        matches!(
            self,
            Self::ShowStatus
                | Self::Run
                | Self::Approve
                | Self::Stop
                | Self::ListRuns
                | Self::ListIterations
                | Self::Resume
        )
    }
}

/// Presentation returned by one root-runtime Ralph action.
pub struct RalphRootOutput {
    pub status: String,
    pub markdown: Option<String>,
}

pub fn execute_root_local_action(
    chat: &mut ActiveChat,
    action: RalphRootAction,
) -> Result<RalphRootOutput, TuiError> {
    match action {
        RalphRootAction::SaveDraft => save_setup_draft(chat)?,
        RalphRootAction::ViewDraft => view_setup_draft(chat)?,
        RalphRootAction::ReviseDraft => revise_setup_draft(chat)?,
        RalphRootAction::ApproveDraft => approve_setup_draft(chat)?,
        RalphRootAction::ApplyDraftToLoop => apply_draft_to_existing_loop(chat)?,
        RalphRootAction::OpenProgress => open_progress(chat)?,
        RalphRootAction::Audit => show_prompt(chat, ralph_state::RalphPromptKind::Audit)?,
        RalphRootAction::Replan => show_prompt(chat, ralph_state::RalphPromptKind::Replan)?,
        RalphRootAction::Plan | RalphRootAction::CreateFromDraft => {
            return Err(TuiError::PluginService {
                code: "ralph_async_local_action_requires_effect".to_owned(),
                message: "Ralph action requires root effect scheduling".to_owned(),
            });
        }
        RalphRootAction::ShowStatus
        | RalphRootAction::Run
        | RalphRootAction::Approve
        | RalphRootAction::Stop
        | RalphRootAction::ListRuns
        | RalphRootAction::ListIterations
        | RalphRootAction::Resume => {
            return Err(TuiError::PluginService {
                code: "ralph_remote_action_requires_effect".to_owned(),
                message: "Ralph daemon action requires root effect scheduling".to_owned(),
            });
        }
    }
    Ok(RalphRootOutput {
        status: flash_message_for_root_action(action),
        markdown: None,
    })
}

fn flash_message_for_root_action(action: RalphRootAction) -> String {
    match action {
        RalphRootAction::SaveDraft => "Ralph setup draft saved".to_owned(),
        RalphRootAction::ViewDraft => "Ralph setup draft shown".to_owned(),
        RalphRootAction::ReviseDraft => "Ralph setup revision prompt prepared".to_owned(),
        RalphRootAction::ApproveDraft => "Ralph setup draft approval applied".to_owned(),
        RalphRootAction::ApplyDraftToLoop => "Ralph rebuild draft applied".to_owned(),
        RalphRootAction::OpenProgress => "Ralph progress path shown".to_owned(),
        RalphRootAction::Audit => "Ralph audit prompt prepared".to_owned(),
        RalphRootAction::Replan => "Ralph replan prompt prepared".to_owned(),
        RalphRootAction::Plan
        | RalphRootAction::CreateFromDraft
        | RalphRootAction::ShowStatus
        | RalphRootAction::Run
        | RalphRootAction::Approve
        | RalphRootAction::Stop
        | RalphRootAction::ListRuns
        | RalphRootAction::ListIterations
        | RalphRootAction::Resume => "Ralph action completed".to_owned(),
    }
}

pub struct RalphStartRequest {
    pub loop_name: String,
    pub repo_root: PathBuf,
    pub session_id: Option<bcode_session_models::SessionId>,
    pub session_title: Option<String>,
    pub work_area_path: Option<String>,
    pub branch: Option<String>,
    pub validation_commands: Vec<String>,
}

pub struct RalphStartOutput {
    pub status: String,
    pub markdown: String,
}

pub async fn execute_root_start(
    client: &bcode_client::BcodeClient,
    request: RalphStartRequest,
) -> Result<RalphStartOutput, TuiError> {
    let RalphStartRequest {
        loop_name,
        repo_root,
        session_id,
        session_title,
        work_area_path,
        branch,
        validation_commands,
    } = request;
    let state =
        ralph_state::create_initial_loop_state(&loop_name, &repo_root, session_title.as_deref())?;
    ralph_state::set_validation_commands(&state.state_dir, &validation_commands, "setup")?;
    if let Some(session_id) = session_id {
        let history = client
            .session_history_page(
                session_id,
                SessionHistoryQuery {
                    cursor: None,
                    limit: 64,
                    direction: SessionHistoryDirection::Backward,
                },
            )
            .await?;
        ralph_state::write_context_pack(&state, session_title.as_deref(), &history.events)?;
        ralph_state::generate_progress_doc_from_context(&state, &loop_name, &repo_root)?;
    }
    let work_area = client
        .create_worktree(WorktreeCreateRequest {
            name: format!("ralph-{loop_name}"),
            cwd: Some(repo_root),
            path: work_area_path.map(PathBuf::from),
            branch: None,
            new_branch: branch,
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
        let _event = client
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
    Ok(RalphStartOutput {
        status: "Ralph loop created".to_owned(),
        markdown: format!(
            "Ralph loop created\n* Loop: {loop_name}\n* Charter: {}\n* Progress doc: {}\n* State: {}\n* Isolated work area: {}\n* Session: {}\n* Validation: {}\n* Next: review docs if desired, then prepare a run and approve/start it",
            display_from_current_dir(&state.charter_doc_path),
            display_from_current_dir(&state.progress_doc_path),
            display_from_current_dir(&state.state_dir),
            display_from_current_dir(&work_area.path),
            work_area_session_id.as_deref().unwrap_or("<none>"),
            validation_summary
        ),
    })
}

/// Execute a non-interactive Ralph action through the application client boundary.
#[allow(clippy::too_many_lines)]
pub async fn execute_root_action(
    client: &bcode_client::BcodeClient,
    repo_root: PathBuf,
    action: RalphRootAction,
) -> Result<RalphRootOutput, TuiError> {
    match action {
        RalphRootAction::ShowStatus => {
            let response = client
                .ralph_run_status(RalphRunStatusRequest {
                    repo_root,
                    loop_state_dir: None,
                })
                .await?;
            let Some(summary) = response.loop_summary else {
                return Ok(RalphRootOutput {
                    status: "no Ralph loops for current repository".to_owned(),
                    markdown: None,
                });
            };
            Ok(RalphRootOutput {
                status: "Ralph status shown".to_owned(),
                markdown: Some(format_status_note(
                    &summary,
                    response.active_run.as_ref(),
                    response.interrupted_runs.len(),
                )),
            })
        }
        RalphRootAction::Run => {
            if let Some(draft) = active_unapplied_rebuild_draft(&repo_root)? {
                return Ok(RalphRootOutput {
                    status: "active rebuild draft must be applied or canceled before running"
                        .to_owned(),
                    markdown: Some(format!(
                        "Ralph rebuild draft is active\n* Draft: {}\n* Status: {}\n* Target loop: {}\n* Next: View/Revise/Approve/Apply the rebuild draft before preparing another autonomous run. This prevents running against stale loop context.",
                        draft.draft_id, draft.status, draft.loop_name
                    )),
                });
            }
            let response = client
                .run_ralph_loop(RalphRunRequest {
                    repo_root,
                    loop_state_dir: None,
                    max_iterations: None,
                    no_progress_limit: None,
                    require_approval: true,
                })
                .await?;
            Ok(RalphRootOutput {
                status: "Ralph run prepared; approve to start".to_owned(),
                markdown: Some(format!(
                    "Ralph run prepared\n* Run: {}\n* Status: {}\n* State: {}\n* Session: {}\n* Next: /ralph approve",
                    response.run.run_id,
                    response.run.status,
                    display_from_current_dir(&response.run.state_dir),
                    response.run.session_id.as_deref().unwrap_or("<none>")
                )),
            })
        }
        RalphRootAction::Approve => {
            let response = client
                .approve_ralph_run(RalphApproveRequest {
                    repo_root,
                    loop_state_dir: None,
                    run_id: None,
                })
                .await?;
            Ok(RalphRootOutput {
                status: "Ralph run approved".to_owned(),
                markdown: Some(format!(
                    "Ralph run approved\n* Run: {}\n* Status: {}\n* State: {}\n* Session: {}",
                    response.run.run_id,
                    response.run.status,
                    display_from_current_dir(&response.run.state_dir),
                    response.run.session_id.as_deref().unwrap_or("<none>")
                )),
            })
        }
        RalphRootAction::Stop => {
            let response = client
                .cancel_ralph_loop(RalphCancelRequest {
                    repo_root,
                    run_id: None,
                    loop_state_dir: None,
                })
                .await?;
            Ok(RalphRootOutput {
                status: "Ralph stop requested".to_owned(),
                markdown: Some(format!(
                    "Ralph stop requested\n* Run: {}\n* Status: {}\n* Cancel requested: {}",
                    response.run.run_id, response.run.status, response.cancel_requested
                )),
            })
        }
        RalphRootAction::ListRuns => execute_root_list_runs(client, repo_root).await,
        RalphRootAction::ListIterations => execute_root_list_iterations(client, repo_root).await,
        RalphRootAction::Resume => {
            let response = client
                .resume_ralph_run(RalphResumeRequest {
                    repo_root,
                    loop_state_dir: None,
                    interrupted_run_id: None,
                })
                .await?;
            Ok(RalphRootOutput {
                status: "Ralph resume prepared; approval required".to_owned(),
                markdown: Some(format!(
                    "Ralph resume prepared\n* Interrupted run: {}\n* New run: {}\n* Status: {}\n* Next: approve before autonomous execution continues",
                    response.interrupted_run.run_id,
                    response.resumed_run.run_id,
                    response.resumed_run.status
                )),
            })
        }
        RalphRootAction::Plan
        | RalphRootAction::SaveDraft
        | RalphRootAction::ViewDraft
        | RalphRootAction::ReviseDraft
        | RalphRootAction::ApproveDraft
        | RalphRootAction::ApplyDraftToLoop
        | RalphRootAction::CreateFromDraft
        | RalphRootAction::OpenProgress
        | RalphRootAction::Audit
        | RalphRootAction::Replan => Err(TuiError::PluginService {
            code: "ralph_local_action_requires_app_state".to_owned(),
            message: "Ralph local setup actions must execute on the serialized root model"
                .to_owned(),
        }),
    }
}

async fn execute_root_list_runs(
    client: &bcode_client::BcodeClient,
    repo_root: PathBuf,
) -> Result<RalphRootOutput, TuiError> {
    let response = client
        .list_ralph_runs(RalphListRunsRequest {
            repo_root,
            loop_state_dir: None,
        })
        .await?;
    let Some(summary) = response.loop_summary else {
        return Ok(RalphRootOutput {
            status: "no Ralph loops for current repository".to_owned(),
            markdown: None,
        });
    };
    let runs = if response.runs.is_empty() {
        "* <none>".to_owned()
    } else {
        response
            .runs
            .iter()
            .map(format_run_detail)
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(RalphRootOutput {
        status: "Ralph runs shown".to_owned(),
        markdown: Some(format!("Ralph runs\n* Loop: {}\n{runs}", summary.loop_name)),
    })
}

async fn execute_root_list_iterations(
    client: &bcode_client::BcodeClient,
    repo_root: PathBuf,
) -> Result<RalphRootOutput, TuiError> {
    let response = client
        .list_ralph_iterations(RalphListIterationsRequest {
            repo_root,
            loop_state_dir: None,
            run_id: None,
        })
        .await?;
    let Some(summary) = response.loop_summary else {
        return Ok(RalphRootOutput {
            status: "no Ralph loops for current repository".to_owned(),
            markdown: None,
        });
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
                let stop_reason = iteration
                    .stop_reason
                    .as_deref()
                    .map_or_else(String::new, |reason| format!(" ({reason})"));
                format!(
                    "* #{} — {}{}",
                    iteration.iteration_number, iteration.status, stop_reason
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
            .map(|validation| format!("* {} — {}", validation.command, validation.status))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(RalphRootOutput {
        status: "Ralph iterations shown".to_owned(),
        markdown: Some(format!(
            "Ralph iterations\n* Loop: {}\n* Run: {run_label}\nIterations:\n{iterations}\nValidations:\n{validations}",
            summary.loop_name
        )),
    })
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
    chat.push_presentation_markdown("bcode.ralph", format!(
        "Ralph setup draft review\n* Draft: {}\n* Status: {}\n* Mode: {}\n* Target state: {}\n* Loop: {}\n* Branch: {}\n* Worktree: {}\n* Validation: {}\n* Ready: charter={} progress={} approved={}\n* Draft JSON: {}\n* Setup transcript: {}\n\nCharter preview:\n{}\n\nProgress preview:\n{}",
        draft.draft_id,
        draft.status,
        draft.mode,
        draft
            .target_state_dir
            .as_ref()
            .map_or_else(|| "<none>".to_owned(), |path| display_from_current_dir(path).to_string()),
        draft.loop_name,
        draft.branch.as_deref().unwrap_or("<default>"),
        draft
            .work_area_path
            .as_ref()
            .map_or_else(|| "<default>".to_owned(), |path| display_from_current_dir(path).to_string()),
        if draft.validation_commands.is_empty() {
            "<none>".to_owned()
        } else {
            draft.validation_commands.join("; ")
        },
        readiness.has_charter,
        readiness.has_progress,
        readiness.approved,
        display_from_current_dir(&draft.draft_path),
        draft
            .setup_transcript_path
            .as_ref()
            .map_or_else(|| "<none>".to_owned(), |path| display_from_current_dir(path).to_string()),
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
        worktree = draft.work_area_path.as_ref().map_or_else(
            || "<default>".to_owned(),
            |path| display_from_current_dir(path).to_string()
        ),
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
    chat.push_presentation_markdown("bcode.ralph", format!(
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
        chat.push_presentation_markdown("bcode.ralph", format!(
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
    chat.push_presentation_markdown(
        "bcode.ralph",
        format!(
            "Ralph setup draft approved\n* Draft: {}\n* Path: {}\n* Next: {}",
            updated.draft_id,
            display_from_current_dir(&updated.draft_path),
            if updated.mode == ralph_state::RalphSetupDraftMode::RebuildExistingLoop {
                "apply draft to loop"
            } else {
                "create loop from draft"
            }
        ),
    );
    chat.app.set_status("Ralph setup draft approved".to_owned());
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
    chat.push_presentation_markdown("bcode.ralph", format!(
        "Ralph setup draft saved\n* Draft: {}\n* Status: {}\n* Loop: {}\n* Branch: {}\n* Worktree: {}\n* Has charter: {}\n* Has progress: {}\n* Path: {}\n* Next: {}",
        updated.draft_id,
        updated.status,
        updated.loop_name,
        updated.branch.as_deref().unwrap_or("<default>"),
        updated
            .work_area_path
            .as_ref()
            .map_or_else(|| "<default>".to_owned(), |path| display_from_current_dir(path).to_string()),
        readiness.has_charter,
        readiness.has_progress,
        display_from_current_dir(&updated.draft_path),
        if readiness.has_charter && readiness.has_progress {
            "review the saved draft, then approve setup draft"
        } else {
            "ask the assistant for the exact RALPH_SETUP_DRAFT_START artifact, then save again"
        }
    ));
    chat.app.set_status("Ralph setup draft saved".to_owned());
    Ok(())
}

fn apply_draft_to_existing_loop(chat: &mut ActiveChat) -> Result<(), TuiError> {
    let repo_root = current_repo_root(chat)?;
    let Some(draft) = ralph_state::latest_setup_draft(&repo_root)? else {
        chat.app.set_status("no Ralph setup draft found".to_owned());
        return Ok(());
    };
    if draft.mode != ralph_state::RalphSetupDraftMode::RebuildExistingLoop {
        chat.push_presentation_markdown("bcode.ralph", format!(
            "Ralph setup draft is not a rebuild draft\n* Draft: {}\n* Mode: {}\n* Next: use Create loop from draft for new-loop drafts, or start Rebuild loop context",
            draft.draft_id, draft.mode
        ));
        chat.app
            .set_status("Ralph setup draft is not a rebuild draft".to_owned());
        return Ok(());
    }
    let readiness = draft.readiness();
    if !readiness.ready() {
        chat.push_presentation_markdown("bcode.ralph", format!(
            "Ralph rebuild draft is not ready to apply\n* Draft: {}\n* Has charter: {}\n* Has progress: {}\n* Approved: {}\n* Next: save and approve the rebuild draft before applying it",
            draft.draft_id, readiness.has_charter, readiness.has_progress, readiness.approved
        ));
        chat.app
            .set_status("Ralph rebuild draft is not ready".to_owned());
        return Ok(());
    }
    let result = ralph_state::apply_setup_draft_to_existing_loop(&draft.draft_id, &repo_root)?;
    chat.push_presentation_markdown("bcode.ralph", format!(
        "Ralph loop context rebuilt\n* State dir: {}\n* Backups: {}\n* Charter: {}\n* Progress: {}\n* Run history was preserved",
        display_from_current_dir(&result.state_dir),
        display_from_current_dir(&result.backup_dir),
        display_from_current_dir(&result.charter_doc_path),
        display_from_current_dir(&result.progress_doc_path)
    ));
    chat.app
        .set_status("Ralph loop context rebuilt from draft".to_owned());
    Ok(())
}

/// Start an LLM-guided Ralph setup draft instead of immediately creating loop files.
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
    chat.push_presentation_markdown(
        "bcode.ralph",
        format!(
            "Ralph prompt prepared\n* Loop: {}\n* Progress doc: {}\n\n{}",
            summary.loop_name,
            display_from_current_dir(&summary.progress_doc_path),
            prompt
        ),
    );
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
    chat.push_presentation_markdown(
        "bcode.ralph",
        format!(
            "Ralph progress doc\n* Loop: {}\n* Path: {}",
            summary.loop_name,
            display_from_current_dir(&summary.progress_doc_path)
        ),
    );
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn format_run_detail(run: &RalphRunSummary) -> String {
    let stop_reason = run
        .stop_reason
        .as_deref()
        .map_or_else(String::new, |reason| format!(" ({reason})"));
    let error = run
        .error_message
        .as_deref()
        .map_or_else(String::new, |message| {
            format!(
                "\n  Error: {}\n  Recovery: {}",
                compact_failure_message(message),
                recovery_hint(run.stop_reason.as_deref(), Some(message))
            )
        });
    format!(
        "* {} — {}{}{}\n  Session: {}",
        run.run_id,
        run.status,
        stop_reason,
        error,
        run.session_id.as_deref().unwrap_or("<none>")
    )
}

fn compact_failure_message(message: &str) -> String {
    const MAX_LEN: usize = 240;
    let single_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.len() <= MAX_LEN {
        single_line
    } else {
        format!("{}…", &single_line[..MAX_LEN])
    }
}

fn recovery_hint(stop_reason: Option<&str>, error_message: Option<&str>) -> &'static str {
    let reason = stop_reason.unwrap_or_default().to_ascii_lowercase();
    let message = error_message.unwrap_or_default().to_ascii_lowercase();
    if reason.contains("daemon restart") || message.contains("daemon restart") {
        "use Resume safely; if resume is unavailable, audit/replan before retrying"
    } else if message.contains("rate") || message.contains("429") {
        "wait and retry, or switch provider/model if the limit persists"
    } else if message.contains("context")
        && (message.contains("large") || message.contains("length"))
    {
        "compact/replan context, then retry the run"
    } else if message.contains("permission") || message.contains("denied") {
        "grant/approve the required permission, then resume or retry"
    } else if reason.contains("model_turn_failed") {
        "open Iterations for turn details, then Retry/Resume or Audit/Replan"
    } else {
        "open Iterations for details; retry only after the cause is understood"
    }
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
                "{} ({}){}{}{}{}",
                run.run_id,
                run.status,
                run.runtime_work_id
                    .as_deref()
                    .map_or_else(String::new, |work_id| format!(", work: {work_id}")),
                run.stop_reason
                    .as_deref()
                    .map_or_else(String::new, |reason| format!(", stop: {reason}")),
                run.error_message
                    .as_deref()
                    .map_or_else(String::new, |message| {
                        format!(
                            ", error: {}, recovery: {}",
                            compact_failure_message(message),
                            recovery_hint(run.stop_reason.as_deref(), Some(message))
                        )
                    }),
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
        display_from_current_dir(&summary.progress_doc_path),
        display_from_current_dir(&summary.state_dir),
        summary.work_area_path.as_ref().map_or_else(
            || "<none>".to_owned(),
            |path| display_from_current_dir(path).to_string()
        ),
        summary.session_id.as_deref().unwrap_or("<none>")
    )
}

fn active_unapplied_rebuild_draft(
    repo_root: &std::path::Path,
) -> Result<Option<ralph_state::RalphSetupDraft>, TuiError> {
    Ok(ralph_state::latest_setup_draft(repo_root)?
        .filter(|draft| draft.mode == ralph_state::RalphSetupDraftMode::RebuildExistingLoop)
        .filter(|draft| {
            !matches!(
                draft.status,
                ralph_state::RalphSetupDraftStatus::Canceled
                    | ralph_state::RalphSetupDraftStatus::ConvertedToLoop
            )
        }))
}
