//! Home page for the Bcode web renderer.

use bcode_plugin_sdk::path::display_from_current_dir;
use std::collections::BTreeMap;
use std::sync::LazyLock;

use bcode_session_models::SessionSummary;
use bcode_session_view_models::{
    InteractionViewSummary, PermissionView, PluginVisualView, SessionViewSnapshot,
    ToolArtifactView, ToolInvocationView, ToolInvocationViewStatus, ToolResultView,
    TranscriptViewItemKind,
};
use hyperchad::template::{Containers, container};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct QuestionSnapshot {
    request: QuestionRequest,
    answers: Vec<QuestionAnswer>,
}

#[derive(Debug, Deserialize)]
struct QuestionRequest {
    questions: Vec<Question>,
}

#[derive(Debug, Deserialize)]
struct Question {
    header: Option<String>,
    #[serde(rename = "question")]
    text: String,
    options: Vec<QuestionOption>,
    custom: bool,
    required: bool,
}

#[derive(Debug, Deserialize)]
struct QuestionOption {
    label: String,
    value: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct QuestionAnswer {
    question_index: usize,
    selected: Vec<String>,
    custom: Option<String>,
}

type VisualAdapter = fn(&PluginVisualView) -> Option<Containers>;
type ArtifactAdapter = fn(&ToolArtifactView) -> Option<Containers>;

static ARTIFACT_ADAPTERS: LazyLock<BTreeMap<(&'static str, u32), ArtifactAdapter>> =
    LazyLock::new(|| {
        BTreeMap::from([
            (
                ("bcode.document.extract_result", 1),
                render_document_extract_result as ArtifactAdapter,
            ),
            (
                ("bcode.document.status", 1),
                render_document_status as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.read", 1),
                render_filesystem_read_result as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.image", 1),
                render_filesystem_image_result as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.change", 1),
                render_filesystem_change_result as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.exists", 1),
                render_filesystem_exists_result as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.list", 1),
                render_filesystem_list_result as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.find", 1),
                render_filesystem_find_result as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.grep", 1),
                render_filesystem_grep_result as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.stat", 1),
                render_filesystem_stat_result as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.artifact.metadata", 1),
                render_filesystem_artifact_metadata as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.artifact.read", 1),
                render_filesystem_artifact_read as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.artifact.grep", 1),
                render_filesystem_artifact_grep as ArtifactAdapter,
            ),
            (
                ("bcode.git.clone_result", 1),
                render_git_clone_result as ArtifactAdapter,
            ),
            (
                ("bcode.ocr.extract_result", 1),
                render_ocr_extract_result as ArtifactAdapter,
            ),
            (
                ("bcode.ocr.status", 1),
                render_ocr_status as ArtifactAdapter,
            ),
            (
                ("bcode.web-search.search_results", 1),
                render_web_search_results as ArtifactAdapter,
            ),
            (
                ("bcode.web-search.fetch_result", 1),
                render_web_fetch_result as ArtifactAdapter,
            ),
            (
                ("bcode.worktree.list", 1),
                render_worktree_list_result as ArtifactAdapter,
            ),
            (
                ("bcode.worktree.create_result", 1),
                render_worktree_create_result as ArtifactAdapter,
            ),
            (
                ("bcode.worktree.remove_result", 1),
                render_worktree_remove_result as ArtifactAdapter,
            ),
        ])
    });

static VISUAL_ADAPTERS: LazyLock<BTreeMap<(&'static str, u32), VisualAdapter>> =
    LazyLock::new(|| {
        let structured = render_structured_visual as VisualAdapter;
        BTreeMap::from([
            (
                ("bcode.tool.request.shell.run", 1),
                render_shell_request as VisualAdapter,
            ),
            (
                ("bcode.filesystem.request", 1),
                render_filesystem_request as VisualAdapter,
            ),
            (
                ("bcode.filesystem.change", 1),
                render_filesystem_change as VisualAdapter,
            ),
            (("bcode.filesystem.read", 1), structured),
            (("bcode.filesystem.image", 1), structured),
            (("bcode.filesystem.exists", 1), structured),
            (("bcode.filesystem.list", 1), structured),
            (("bcode.filesystem.find", 1), structured),
            (("bcode.filesystem.grep", 1), structured),
            (("bcode.filesystem.stat", 1), structured),
            (("bcode.filesystem.artifact.metadata", 1), structured),
            (("bcode.filesystem.artifact.read", 1), structured),
            (("bcode.filesystem.artifact.grep", 1), structured),
            (("bcode.document.request", 1), structured),
            (("bcode.ocr.request", 1), structured),
            (
                ("bcode.web-search.search_request", 1),
                render_web_search_request as VisualAdapter,
            ),
            (
                ("bcode.web-search.fetch_request", 1),
                render_web_fetch_request as VisualAdapter,
            ),
            (("bcode.web-search.status_request", 1), structured),
            (("bcode.web-search.inspect_request", 1), structured),
            (
                ("bcode.git.clone_request", 1),
                render_git_clone_request as VisualAdapter,
            ),
            (
                ("bcode.worktree.request", 1),
                render_worktree_request as VisualAdapter,
            ),
            (
                ("bcode.vim-edit.request.preview", 1),
                render_vim_edit_request as VisualAdapter,
            ),
            (
                ("bcode.vim-edit.request.apply", 1),
                render_vim_edit_request as VisualAdapter,
            ),
            (("bcode.vim-edit.live", 1), structured),
            (("bcode.vim-edit.playback", 1), structured),
        ])
    });

/// Render the Bcode web renderer shell.
#[must_use]
pub fn home(
    snapshot: &SessionViewSnapshot,
    sessions: &[SessionSummary],
    access_token: &str,
) -> Containers {
    let title = snapshot
        .title
        .as_deref()
        .or_else(|| {
            snapshot
                .session_summary
                .as_ref()
                .and_then(SessionSummary::title)
        })
        .unwrap_or("No session attached");
    let status = if snapshot.session_id.is_some() {
        "daemon connected · session attached"
    } else {
        "daemon connected · no active session"
    };

    container! {
        div #bcode-web-shell padding=24 background="#0d1117" color="#c9d1d9" font-family="monospace" {
            header justify-content=space-between align-items=center margin-bottom=24 {
                div {
                    h1 color="#7ee787" font-size=28 margin-bottom=4 { "bcode web" }
                    div color="#8b949e" font-size=13 { "renderer-neutral session view powered by HyperChad" }
                }
                div background="#161b22" border="1, #30363d" border-radius=999 padding="6, 12" color="#7ee787" font-size=12 {
                    (status)
                }
            }

            div gap=18 align-items=start {
                aside width=280 background="#161b22" border="1, #30363d" border-radius=10 padding=14 {
                    h2 font-size=14 color="#f0f6fc" margin-bottom=12 { "sessions" }
                    @if sessions.is_empty() {
                        div color="#8b949e" font-size=12 { "No sessions loaded yet." }
                    } @else {
                        @for session in sessions {
                            anchor href=(format!("/session/{}?token={access_token}&hyperchad-event-scope={access_token}:{}", session.id, session.id)) text-decoration="none" {
                                div color="#f0f6fc" font-size=13 { (session.title().unwrap_or("Untitled session")) }
                                div color="#8b949e" font-size=11 { (display_from_current_dir(&session.working_directory).to_string()) }
                            }
                        }
                    }
                }

                main flex=1 {
                    section background="#161b22" border="1, #30363d" border-radius=10 padding=16 margin-bottom=18 {
                        div justify-content=space-between gap=12 align-items=start {
                            div {
                                h2 color="#f0f6fc" font-size=20 margin-bottom=4 { (title) }
                                div color="#8b949e" font-size=12 {
                                    "revision " (snapshot.revision.to_string())
                                    @if let Some(sequence) = snapshot.latest_sequence {
                                        " · latest event " (sequence.to_string())
                                    }
                                }
                            }
                            div color="#8b949e" font-size=12 {
                                (snapshot.working_directory.as_ref().map_or_else(|| "—".to_string(), |path| display_from_current_dir(path).to_string()))
                            }
                        }
                    }

                    @if !snapshot.active_invocations.is_empty() {
                        (active_invocations_section(&snapshot.active_invocations))
                    }
                    @if !snapshot.runtime_work.is_empty() {
                        (runtime_work_section(&snapshot.runtime_work))
                    }

                    (runtime_state_section(snapshot))

                    (transcript_section(snapshot, access_token))

                    @if !snapshot.interactions.is_empty() {
                        section background="#161b22" border="1, #30363d" border-radius=10 padding=16 margin-bottom=18 {
                            h2 color="#f0f6fc" font-size=16 margin-bottom=14 { "interactions" }
                            @for interaction in &snapshot.interactions {
                                (interaction_request(interaction, snapshot.session_id, access_token))
                            }
                        }
                    }

                    @if !snapshot.permissions.is_empty() {
                        section background="#161b22" border="1, #30363d" border-radius=10 padding=16 margin-bottom=18 {
                            h2 color="#f0f6fc" font-size=16 margin-bottom=14 { "permissions" }
                            @for permission in &snapshot.permissions {
                                (permission_request(permission, snapshot.session_id, access_token))
                            }
                        }
                    }

                    section background="#161b22" border="1, #30363d" border-radius=10 padding=16 {
                        h2 color="#f0f6fc" font-size=16 margin-bottom=12 { "composer" }
                        @if let Some(message) = &snapshot.composer.disabled_reason {
                            div background="#0d1117" border="1, #30363d" border-radius=8 padding=10 color="#8b949e" font-size=12 margin-bottom=12 {
                                (message)
                            }
                        }
                        (composer(snapshot, access_token))
                    }
                }
            }
        }
    }
}

fn transcript_section(snapshot: &SessionViewSnapshot, access_token: &str) -> Containers {
    container! {
        section background="#161b22" border="1, #30363d" border-radius=10 padding=16 margin-bottom=18 {
            h2 color="#f0f6fc" font-size=16 margin-bottom=14 { "transcript" }
            @if snapshot.transcript.has_older_history {
                @if let (Some(session_id), Some(anchor_sequence)) = (snapshot.session_id, snapshot.transcript.source_start_sequence) {
                    form hx-post=(format!("/actions/history-window?token={access_token}")) hx-target="#bcode-web-shell" hx-swap=this margin-bottom=12 {
                        input type=hidden name="session_id" value=(session_id.to_string());
                        input type=hidden name="direction" value="older";
                        input type=hidden name="anchor_sequence" value=(anchor_sequence.to_string());
                        button type=submit background="#21262d" color="#58a6ff" border="1, #30363d" border-radius=6 padding="6, 12" { "load older history" }
                    }
                }
            }
            @if snapshot.transcript.items.is_empty() {
                div color="#8b949e" font-size=13 { "Attach or create a session to begin." }
            } @else {
                @for item in &snapshot.transcript.items {
                    @if should_render_transcript_item(item, snapshot.thinking.visible) {
                        (transcript_item(item))
                    }
                }
            }
            @if snapshot.transcript.has_newer_history {
                @if let (Some(session_id), Some(anchor_sequence)) = (snapshot.session_id, snapshot.transcript.source_end_sequence) {
                    form hx-post=(format!("/actions/history-window?token={access_token}")) hx-target="#bcode-web-shell" hx-swap=this margin-top=12 {
                        input type=hidden name="session_id" value=(session_id.to_string());
                        input type=hidden name="direction" value="newer";
                        input type=hidden name="anchor_sequence" value=(anchor_sequence.to_string());
                        button type=submit background="#21262d" color="#58a6ff" border="1, #30363d" border-radius=6 padding="6, 12" { "load newer history" }
                    }
                }
            }
        }
    }
}

fn runtime_state_section(snapshot: &SessionViewSnapshot) -> Containers {
    let runtime = &snapshot.runtime;
    let model = runtime
        .requested_model_id
        .as_deref()
        .or(runtime.effective_model_id.as_deref())
        .unwrap_or("—");
    let provider = runtime.provider_plugin_id.as_deref().unwrap_or("—");
    let agent = runtime.agent_id.as_deref().unwrap_or("—");
    let turn = runtime.active_turn_id.as_deref().map_or_else(
        || {
            runtime
                .last_turn_outcome
                .map_or_else(|| "idle".to_owned(), |outcome| format!("{outcome:?}"))
        },
        |turn_id| {
            if runtime.cancelling {
                format!("{turn_id} (cancelling)")
            } else {
                turn_id.to_owned()
            }
        },
    );
    let context = runtime.context_occupancy.as_ref().map_or_else(
        || "—".to_owned(),
        |occupancy| {
            let count = occupancy.observation.context_tokens;
            if count.is_estimated() {
                format!("~{} tokens", count.tokens())
            } else {
                format!("{} tokens", count.tokens())
            }
        },
    );
    container! {
        section background="#161b22" border="1, #30363d" border-radius=10 padding=12 margin-bottom=18 {
            div direction=row gap=18 font-size=12 {
                div { span color="#8b949e" { "provider " } span color="#c9d1d9" { (provider) } }
                div { span color="#8b949e" { "model " } span color="#c9d1d9" { (model) } }
                div { span color="#8b949e" { "agent " } span color="#c9d1d9" { (agent) } }
                div { span color="#8b949e" { "turn " } span color="#c9d1d9" { (turn) } }
                div { span color="#8b949e" { "context " } span color="#c9d1d9" { (context) } }
            }
        }
    }
}

fn active_invocations_section(
    active: &BTreeMap<String, bcode_session_models::ToolInvocationLifecycleEvent>,
) -> Containers {
    let heading = if active.len() == 1 {
        "active tool"
    } else {
        "active invocations"
    };
    container! {
        section background="#161b22" border="1, #30363d" border-radius=10 padding=16 margin-bottom=18 {
            h2 color="#f0f6fc" font-size=16 margin-bottom=14 { (heading) }
            @for (invocation_id, lifecycle) in active {
                div background="#0d1117" border="1, #30363d" border-radius=6 padding=10 margin-bottom=8 {
                    div color="#f0f6fc" {
                        (lifecycle.message.as_deref().unwrap_or(invocation_id))
                    }
                    div color="#8b949e" font-size=11 margin-top=3 {
                        (invocation_id) " · " (format!("{:?}", lifecycle.stage))
                    }
                }
            }
        }
    }
}

fn runtime_work_section(runtime_work: &[bcode_session_view_models::RuntimeWorkView]) -> Containers {
    container! {
        section background="#161b22" border="1, #30363d" border-radius=10 padding=16 margin-bottom=18 {
            h2 color="#f0f6fc" font-size=16 margin-bottom=14 { "runtime work" }
            @for work in runtime_work {
                div background="#0d1117" border="1, #30363d" border-radius=6 padding=10 margin-bottom=8 {
                    div color="#f0f6fc" {
                        (work.label) " · " (format!("{:?}", work.status))
                    }
                    div color="#8b949e" font-size=11 margin-top=3 {
                        (work.work_id.to_string()) " · " (format!("{:?}", work.kind))
                    }
                    @if let Some(message) = &work.message {
                        div color="#8b949e" font-size=12 margin-top=4 { (message) }
                    }
                    @if let (Some(completed), Some(total)) = (work.completed_units, work.total_units) {
                        div color="#58a6ff" font-size=11 margin-top=4 {
                            (completed.to_string()) "/" (total.to_string())
                        }
                    }
                }
            }
        }
    }
}

fn composer(snapshot: &SessionViewSnapshot, access_token: &str) -> Containers {
    let action = format!("/actions/submit-message?token={access_token}");
    container! {
        div {
            form hx-post=(action) hx-target="#bcode-web-shell" hx-swap=this background="#0d1117" border="1, #30363d" border-radius=8 padding=12 {
                @if let Some(session_id) = snapshot.session_id {
                    input type=hidden name="session_id" value=(session_id.to_string());
                }
                input type=hidden name="placement" value="steering";
                @if let Some(session_id) = snapshot.session_id {
                    textarea name="text" rows="5" placeholder="Send a message to this session" hx-post=(format!("/actions/update-draft/{session_id}?token={access_token}")) hx-trigger="change" hx-target="#bcode-web-shell" hx-swap=this width=100% padding=10 border="1, #30363d" border-radius=6 background="#010409" color="#c9d1d9" {
                        (snapshot.composer.draft)
                    }
                } @else {
                    textarea name="text" rows="5" placeholder="Send a message to start a session" width=100% padding=10 border="1, #30363d" border-radius=6 background="#010409" color="#c9d1d9" {
                        (snapshot.composer.draft)
                    }
                }
                button type=submit background="#238636" color=white border-radius=6 padding="8, 14" margin-top=10 {
                    "send"
                }
            }
            @if let Some(session_id) = snapshot.session_id {
                form hx-post=(format!("/actions/cancel-turn?token={access_token}")) hx-target="#bcode-web-shell" hx-swap=this margin-top=10 {
                    input type=hidden name="session_id" value=(session_id.to_string());
                    input type=hidden name="clear_queue" value="true";
                    button type=submit background="#da3633" color=white border-radius=6 padding="8, 14" {
                        "cancel turn"
                    }
                }
            }
        }
    }
}

fn interaction_request(
    interaction: &InteractionViewSummary,
    session_id: Option<bcode_session_models::SessionId>,
    access_token: &str,
) -> Containers {
    container! {
        div border="1, #58a6ff" border-radius=8 padding=10 margin-bottom=10 {
            div color="#58a6ff" margin-bottom=6 {
                (interaction.title.as_deref().unwrap_or("Interactive request"))
            }
            div color="#8b949e" font-size=12 margin-bottom=8 {
                (interaction.kind)
                @if interaction.required { " · required" }
            }
            @if interaction.resolved {
                div color="#8b949e" font-size=12 margin-top=8 { "resolved" }
                @if let Some(resolution) = &interaction.resolution {
                    (json_panel("resolution", resolution))
                }
            } @else {
                @if interaction.kind == "bcode.question" {
                    @if let Some(snapshot) = interaction.snapshot.as_ref().and_then(|value| serde_json::from_value::<QuestionSnapshot>(value.clone()).ok()) {
                        (question_interaction(&snapshot, interaction, session_id, access_token))
                    } @else if let Some(snapshot) = &interaction.snapshot {
                        (json_panel("controller snapshot", snapshot))
                    }
                } @else if let Some(snapshot) = &interaction.snapshot {
                    (json_panel("controller snapshot", snapshot))
                }
                @if let Some(session_id) = session_id {
                    (generic_interaction_controls(interaction, session_id, access_token))
                }
            }
        }
    }
}

fn generic_interaction_controls(
    interaction: &InteractionViewSummary,
    session_id: bcode_session_models::SessionId,
    access_token: &str,
) -> Containers {
    container! {
        details margin-top=10 {
            summary color="#8b949e" { "generic semantic controls" }
            form hx-post=(format!("/actions/interaction?token={access_token}")) hx-target="#bcode-web-shell" hx-swap=this margin-top=8 {
                input type=hidden name="session_id" value=(session_id.to_string());
                input type=hidden name="interaction_id" value=(interaction.interaction_id.clone());
                div gap=8 {
                    select name="kind" selected="submit" padding=8 border="1, #30363d" border-radius=6 background="#010409" color="#c9d1d9" {
                        option value="activate" { "activate control" }
                        option value="change" { "change control value" }
                        option value="focus" { "focus control" }
                        option value="blur" { "blur control" }
                        option value="navigate" { "navigate focus" }
                        option value="submit" { "submit interaction" }
                        option value="cancel" { "cancel interaction" }
                    }
                    input name="control_id" type=text placeholder="control id (activate/change/focus/blur)" padding=8 border="1, #30363d" border-radius=6 background="#010409" color="#c9d1d9";
                    input name="value" type=text placeholder="JSON value (change only)" padding=8 border="1, #30363d" border-radius=6 background="#010409" color="#c9d1d9";
                    input type=hidden name="value_is_json" value="true";
                    select name="direction" selected="next" padding=8 border="1, #30363d" border-radius=6 background="#010409" color="#c9d1d9" {
                        option value="next" { "next" }
                        option value="previous" { "previous" }
                    }
                }
                button type=submit background="#1f6feb" color=white border-radius=6 padding="6, 12" margin-top=8 {
                    "send interaction input"
                }
            }
        }
    }
}

fn question_interaction(
    snapshot: &QuestionSnapshot,
    interaction: &InteractionViewSummary,
    session_id: Option<bcode_session_models::SessionId>,
    access_token: &str,
) -> Containers {
    container! {
        div gap=12 {
            @for (question_index, question) in snapshot.request.questions.iter().enumerate() {
                div background="#0d1117" border="1, #30363d" border-radius=6 padding=10 {
                    @if let Some(header) = &question.header {
                        div color="#58a6ff" font-size=12 margin-bottom=4 { (header) }
                    }
                    div color="#f0f6fc" margin-bottom=8 {
                        (question.text.clone())
                        @if question.required { span color="#f85149" { " *" } }
                    }
                    @if let Some(session_id) = session_id {
                        div gap=6 {
                            @for (option_index, option) in question.options.iter().enumerate() {
                                (question_option(
                                    snapshot,
                                    interaction,
                                    session_id,
                                    access_token,
                                    question_index,
                                    option_index,
                                    option,
                                ))
                            }
                            @if question.custom || question.options.is_empty() {
                                (question_custom_answer(
                                    snapshot,
                                    interaction,
                                    session_id,
                                    access_token,
                                    question_index,
                                ))
                            }
                        }
                    }
                }
            }
            @if let Some(session_id) = session_id {
                div direction=row gap=8 {
                    (question_terminal_action(interaction, session_id, access_token, "submit", "submit answers", "#238636"))
                    (question_terminal_action(interaction, session_id, access_token, "cancel", "cancel", "#da3633"))
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn question_option(
    snapshot: &QuestionSnapshot,
    interaction: &InteractionViewSummary,
    session_id: bcode_session_models::SessionId,
    access_token: &str,
    question_index: usize,
    option_index: usize,
    option: &QuestionOption,
) -> Containers {
    let selected_value = option
        .value
        .as_deref()
        .map_or_else(|| option_index.to_string(), ToOwned::to_owned);
    let selected = snapshot
        .answers
        .iter()
        .find(|answer| answer.question_index == question_index)
        .is_some_and(|answer| answer.selected.contains(&selected_value));
    container! {
        form hx-post=(format!("/actions/interaction?token={access_token}")) hx-target="#bcode-web-shell" hx-swap=this {
            input type=hidden name="session_id" value=(session_id.to_string());
            input type=hidden name="interaction_id" value=(interaction.interaction_id.clone());
            input type=hidden name="kind" value="activate";
            input type=hidden name="control_id" value=(format!("question-{question_index}.option-{option_index}"));
            button type=submit width=100% background=(if selected { "#1f6feb" } else { "#161b22" }) color="#f0f6fc" border="1, #30363d" border-radius=6 padding=8 {
                (if selected { "✓ " } else { "" }) (option.label.clone())
                @if let Some(description) = &option.description {
                    div color="#8b949e" font-size=11 margin-top=3 { (description) }
                }
            }
        }
    }
}

fn question_custom_answer(
    snapshot: &QuestionSnapshot,
    interaction: &InteractionViewSummary,
    session_id: bcode_session_models::SessionId,
    access_token: &str,
    question_index: usize,
) -> Containers {
    let value = snapshot
        .answers
        .iter()
        .find(|answer| answer.question_index == question_index)
        .and_then(|answer| answer.custom.as_deref())
        .unwrap_or_default();
    container! {
        form hx-post=(format!("/actions/interaction?token={access_token}")) hx-target="#bcode-web-shell" hx-swap=this direction=row gap=6 {
            input type=hidden name="session_id" value=(session_id.to_string());
            input type=hidden name="interaction_id" value=(interaction.interaction_id.clone());
            input type=hidden name="kind" value="change";
            input type=hidden name="control_id" value=(format!("question-{question_index}.custom"));
            input name="value" type=text value=(value) placeholder="custom answer" flex=1 padding=8 border="1, #30363d" border-radius=6 background="#010409" color="#c9d1d9";
            button type=submit background="#1f6feb" color=white border-radius=6 padding="6, 12" { "set" }
        }
    }
}

fn question_terminal_action(
    interaction: &InteractionViewSummary,
    session_id: bcode_session_models::SessionId,
    access_token: &str,
    kind: &str,
    label: &str,
    color: &str,
) -> Containers {
    container! {
        form hx-post=(format!("/actions/interaction?token={access_token}")) hx-target="#bcode-web-shell" hx-swap=this {
            input type=hidden name="session_id" value=(session_id.to_string());
            input type=hidden name="interaction_id" value=(interaction.interaction_id.clone());
            input type=hidden name="kind" value=(kind);
            button type=submit background=(color) color=white border-radius=6 padding="6, 12" { (label) }
        }
    }
}

fn permission_request(
    permission: &PermissionView,
    session_id: Option<bcode_session_models::SessionId>,
    access_token: &str,
) -> Containers {
    container! {
        div border="1, #f2cc60" border-radius=8 padding=10 margin-bottom=10 {
            div color="#f2cc60" margin-bottom=6 { (permission.title.as_deref().unwrap_or("Permission requested")) }
            div color="#c9d1d9" { (permission.detail.as_deref().unwrap_or("No details provided.")) }
            @if let Some(batch) = &permission.batch {
                div color="#8b949e" font-size=12 margin-top=6 {
                    "batch " (batch.call_index.saturating_add(1).to_string()) " of " (batch.call_count.to_string())
                }
            }
            @if permission.resolved {
                div color="#8b949e" font-size=12 margin-top=8 {
                    "resolved: " (if permission.approved.unwrap_or(false) { "approved" } else { "denied" })
                }
            } @else if let Some(session_id) = session_id {
                div direction=row gap=8 margin-top=10 {
                    form hx-post=(format!("/actions/permission?token={access_token}")) hx-target="#bcode-web-shell" hx-swap=this {
                        input type=hidden name="session_id" value=(session_id.to_string());
                        input type=hidden name="permission_id" value=(permission.permission_id.clone());
                        input type=hidden name="approved" value="true";
                        @if permission.can_remember {
                            span color="#8b949e" font-size=11 margin-right=8 {
                                input type=checkbox name="remember" value="true";
                                " remember"
                            }
                        }
                        button type=submit background="#238636" color=white border-radius=6 padding="6, 12" { "approve" }
                    }
                    form hx-post=(format!("/actions/permission?token={access_token}")) hx-target="#bcode-web-shell" hx-swap=this {
                        input type=hidden name="session_id" value=(session_id.to_string());
                        input type=hidden name="permission_id" value=(permission.permission_id.clone());
                        input type=hidden name="approved" value="false";
                        @if permission.can_remember {
                            span color="#8b949e" font-size=11 margin-right=8 {
                                input type=checkbox name="remember" value="true";
                                " remember"
                            }
                        }
                        button type=submit background="#da3633" color=white border-radius=6 padding="6, 12" { "deny" }
                    }
                }
                @if let Some(batch) = &permission.batch {
                    div direction=row gap=8 margin-top=8 {
                        form hx-post=(format!("/actions/permission-batch?token={access_token}")) hx-target="#bcode-web-shell" hx-swap=this {
                            input type=hidden name="session_id" value=(session_id.to_string());
                            input type=hidden name="batch_id" value=(batch.batch_id.clone());
                            input type=hidden name="approved" value="true";
                            button type=submit background="#238636" color=white border-radius=6 padding="6, 12" { "approve all" }
                        }
                        form hx-post=(format!("/actions/permission-batch?token={access_token}")) hx-target="#bcode-web-shell" hx-swap=this {
                            input type=hidden name="session_id" value=(session_id.to_string());
                            input type=hidden name="batch_id" value=(batch.batch_id.clone());
                            input type=hidden name="approved" value="false";
                            button type=submit background="#da3633" color=white border-radius=6 padding="6, 12" { "deny all" }
                        }
                    }
                }
            }
        }
    }
}

const fn should_render_transcript_item(
    item: &bcode_session_view_models::TranscriptViewItem,
    reasoning_visible: bool,
) -> bool {
    reasoning_visible || !matches!(&item.kind, TranscriptViewItemKind::ReasoningMessage { .. })
}

fn transcript_item(item: &bcode_session_view_models::TranscriptViewItem) -> Containers {
    container! {
        div background="#0d1117" border-left="2, #30363d" border-radius=8 padding=12 margin-bottom=10 {
            div justify-content=space-between margin-bottom=8 color="#8b949e" font-size=11 {
                span { (item_label(&item.kind)) }
                span { "#" (item.id.get().to_string()) " r" (item.revision.to_string()) }
            }
            (transcript_item_body(&item.kind))
        }
    }
}

fn transcript_item_body(kind: &TranscriptViewItemKind) -> Containers {
    match kind {
        TranscriptViewItemKind::UserMessage { message }
        | TranscriptViewItemKind::AssistantMessage { message }
        | TranscriptViewItemKind::ReasoningMessage { message }
        | TranscriptViewItemKind::SystemMessage { message } => container! {
            div white-space="preserve-wrap" margin=0 color="#c9d1d9" { (message.text) }
        },
        TranscriptViewItemKind::Compaction { compaction } => container! {
            div white-space="preserve-wrap" margin=0 color="#c9d1d9" { (compaction.text) }
        },
        TranscriptViewItemKind::Skill { skill } => container! {
            div white-space="preserve-wrap" margin=0 color="#c9d1d9" { (skill.text) }
        },
        TranscriptViewItemKind::ToolRequest { tool } => render_tool_request(tool),
        TranscriptViewItemKind::ToolInvocation { tool } => container! {
            div {
                div color="#f0f6fc" margin-bottom=6 { (tool.tool_name.as_deref().unwrap_or("unknown tool")) }
                div color=(tool_status_color(tool.status)) font-size=12 margin-bottom=8 { (format!("{:?}", tool.status)) }
                @if let Some(arguments_json) = &tool.arguments_json {
                    details margin-bottom=8 {
                        summary color="#8b949e" { "arguments" }
                        div white-space="preserve-wrap" color="#c9d1d9" { (arguments_json) }
                    }
                }
                @if let Some(output) = &tool.output {
                    div white-space="preserve-wrap" background="#010409" border="1, #30363d" border-radius=6 padding=8 color="#c9d1d9" { (output.text) }
                }
                @if let Some(result_text) = &tool.result_text {
                    details open=true {
                        summary color="#8b949e" { "result" }
                        div white-space="preserve-wrap" color="#c9d1d9" { (result_text) }
                    }
                }
                @if let Some(visual) = &tool.request_visual {
                    (render_plugin_visual("request visual", visual))
                }
                @if let Some(result) = &tool.result {
                    (render_tool_result(result))
                }
            }
        },
        TranscriptViewItemKind::Permission { permission } => container! {
            div border="1, #f2cc60" border-radius=8 padding=10 {
                div color="#f2cc60" margin-bottom=6 { (permission.title.as_deref().unwrap_or("Permission requested")) }
                div color="#c9d1d9" { (permission.detail.as_deref().unwrap_or("No details provided.")) }
            }
        },
        TranscriptViewItemKind::Usage { usage } => container! {
            div color="#8b949e" font-size=12 {
                "input " (usage.usage.input_tokens.map_or_else(|| "unknown".to_owned(), |value| value.to_string()))
                " · output " (usage.usage.output_tokens.map_or_else(|| "unknown".to_owned(), |value| value.to_string()))
                " · total " (usage.usage.metered_total_tokens().map_or_else(|| "unknown".to_owned(), |value| value.to_string()))
            }
        },
        TranscriptViewItemKind::RuntimeWork { work } => container! {
            div {
                div color="#f0f6fc" { (work.message.as_deref().unwrap_or("Runtime work")) }
                div color="#8b949e" font-size=12 { (format!("{:?}", work.status)) }
            }
        },
        TranscriptViewItemKind::Interaction { interaction } => container! {
            div {
                div color="#f0f6fc" { (interaction.title.as_deref().unwrap_or("Interactive request")) }
                div color="#8b949e" font-size=12 { (interaction.kind) }
                @if let Some(snapshot) = &interaction.snapshot {
                    (json_panel("snapshot", snapshot))
                }
            }
        },
        TranscriptViewItemKind::PluginVisual { visual } => {
            render_plugin_visual("plugin visual", visual)
        }
        TranscriptViewItemKind::ToolContribution { contribution } => {
            let visual = PluginVisualView::from(bcode_session_models::PluginVisualDescriptor {
                visual_id: Some(format!(
                    "{}-{}",
                    contribution.invocation_id, contribution.contribution_id
                )),
                producer_plugin_id: Some(contribution.producer_id.clone()),
                schema: contribution.schema.clone(),
                schema_version: contribution.schema_version,
                title: Some("Tool contribution".to_owned()),
                subtitle: None,
                payload: contribution.payload.clone(),
            });
            VISUAL_ADAPTERS
                .get(&(contribution.schema.as_str(), contribution.schema_version))
                .and_then(|adapter| adapter(&visual))
                .unwrap_or_default()
        }
    }
}

fn render_tool_request(tool: &ToolInvocationView) -> Containers {
    container! {
        div {
            div color="#f0f6fc" margin-bottom=6 { (tool.tool_name.as_deref().unwrap_or("unknown tool")) }
            @if let Some(visual) = &tool.request_visual {
                (render_plugin_visual("request visual", visual))
            }
        }
    }
}

fn render_tool_result(result: &ToolResultView) -> Containers {
    let rich = match result {
        ToolResultView::Artifact { artifact } => ARTIFACT_ADAPTERS
            .get(&(
                artifact.artifact.schema.as_str(),
                artifact.artifact.schema_version,
            ))
            .and_then(|adapter| adapter(artifact)),
        ToolResultView::Text { .. } | ToolResultView::Json { .. } => None,
    };
    let fallback = serde_json::to_value(result).unwrap_or(serde_json::Value::Null);
    container! {
        @if let Some(rich) = rich {
            (rich)
        }
        (json_panel("semantic result", &fallback))
    }
}

fn render_document_extract_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let source = metadata.get("source").and_then(serde_json::Value::as_str)?;
    let content_type = metadata
        .get("content_type")
        .and_then(serde_json::Value::as_str);
    let extractor = metadata
        .get("extractor")
        .and_then(serde_json::Value::as_str);
    let truncated = metadata
        .get("truncated")
        .and_then(serde_json::Value::as_bool);
    let document_path = metadata
        .get("document_path")
        .and_then(serde_json::Value::as_str);
    let text_path = metadata
        .get("text_path")
        .and_then(serde_json::Value::as_str);
    let text = metadata.get("text").and_then(serde_json::Value::as_str);
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { (artifact.artifact.title.as_deref().unwrap_or("Document extraction")) }
            div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (source) }
            @if let Some(content_type) = content_type { div color="#8b949e" font-size=12 margin-top=4 { "type: " (content_type) } }
            @if let Some(extractor) = extractor { div color="#8b949e" font-size=12 margin-top=4 { "extractor: " (extractor) } }
            @if let Some(document_path) = document_path { div color="#8b949e" font-size=12 margin-top=4 font-family="monospace" white-space="preserve-wrap" { "document: " (document_path) } }
            @if let Some(text_path) = text_path { div color="#8b949e" font-size=12 margin-top=4 font-family="monospace" white-space="preserve-wrap" { "text: " (text_path) } }
            @if let Some(truncated) = truncated { div color="#8b949e" font-size=12 margin-top=4 { "truncated: " (truncated.to_string()) } }
            @if let Some(text) = text { div color="#c9d1d9" font-size=12 white-space="preserve-wrap" border-top="1, #30363d" margin-top=8 padding-top=8 { (text) } }
        }
    })
}

fn render_document_status(artifact: &ToolArtifactView) -> Option<Containers> {
    render_extract_capabilities(artifact, "Document extractors", "extractors")
}

fn render_filesystem_read_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let contents = artifact
        .artifact
        .metadata
        .get("contents")
        .and_then(serde_json::Value::as_str)?;
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { (artifact.artifact.title.as_deref().unwrap_or("File contents")) }
            div color="#c9d1d9" font-size=12 font-family="monospace" white-space="preserve-wrap" { (contents) }
        }
    })
}

fn render_filesystem_image_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let path = metadata.get("path").and_then(serde_json::Value::as_str)?;
    let mime_type = metadata
        .get("mime_type")
        .and_then(serde_json::Value::as_str);
    let width = metadata.get("width").and_then(serde_json::Value::as_u64);
    let height = metadata.get("height").and_then(serde_json::Value::as_u64);
    let byte_len = metadata.get("byte_len").and_then(serde_json::Value::as_u64);
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { (artifact.artifact.title.as_deref().unwrap_or("Image file")) }
            div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (path) }
            @if let Some(mime_type) = mime_type { div color="#8b949e" font-size=12 margin-top=4 { "type: " (mime_type) } }
            @if let (Some(width), Some(height)) = (width, height) { div color="#8b949e" font-size=12 margin-top=4 { "dimensions: " (width.to_string()) "x" (height.to_string()) } }
            @if let Some(byte_len) = byte_len { div color="#8b949e" font-size=12 margin-top=4 { "bytes: " (byte_len.to_string()) } }
        }
    })
}

fn render_filesystem_change_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let path = metadata.get("path").and_then(serde_json::Value::as_str)?;
    let summary = metadata.get("summary").and_then(serde_json::Value::as_str);
    let old_text = metadata.get("old_text").and_then(serde_json::Value::as_str);
    let new_text = metadata.get("new_text").and_then(serde_json::Value::as_str);
    let start_line = metadata
        .get("start_line")
        .and_then(serde_json::Value::as_u64);
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { (artifact.artifact.title.as_deref().unwrap_or("File change")) }
            div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (path) }
            @if let Some(summary) = summary { div color="#8b949e" font-size=12 margin-top=4 white-space="preserve-wrap" { (summary) } }
            @if let Some(start_line) = start_line { div color="#8b949e" font-size=12 margin-top=4 { "start line: " (start_line.to_string()) } }
            @if let Some(old_text) = old_text { div color="#f85149" font-family="monospace" white-space="preserve-wrap" border-top="1, #30363d" margin-top=8 padding-top=8 { "- " (old_text) } }
            @if let Some(new_text) = new_text { div color="#7ee787" font-family="monospace" white-space="preserve-wrap" border-top="1, #30363d" margin-top=8 padding-top=8 { "+ " (new_text) } }
        }
    })
}

fn render_filesystem_exists_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let exists = artifact
        .artifact
        .metadata
        .get("exists")
        .and_then(serde_json::Value::as_bool)?;
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { (artifact.artifact.title.as_deref().unwrap_or("Path exists")) }
            div color="#f0f6fc" { "exists: " (exists.to_string()) }
        }
    })
}

fn render_filesystem_list_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let entries = artifact
        .artifact
        .metadata
        .get("entries")
        .and_then(serde_json::Value::as_array)?;
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { (format!("{} ({})", artifact.artifact.title.as_deref().unwrap_or("Directory entries"), entries.len())) }
            @for entry in entries.iter().take(25) {
                @if let Some(entry) = entry.as_object() {
                    div border-top="1, #30363d" padding-top=6 margin-top=6 {
                        @if let Some(path) = entry.get("path").and_then(serde_json::Value::as_str) {
                            span color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (path) }
                        }
                        @if let Some(kind) = entry.get("kind").and_then(serde_json::Value::as_str) {
                            span color="#8b949e" { " · " (kind) }
                        }
                    }
                }
            }
            @if entries.len() > 25 {
                div color="#8b949e" font-size=12 margin-top=8 { "… " ((entries.len() - 25).to_string()) " more entries" }
            }
            (filesystem_result_metadata(&artifact.artifact.metadata))
        }
    })
}

fn render_filesystem_find_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let paths = artifact
        .artifact
        .metadata
        .get("paths")
        .and_then(serde_json::Value::as_array)?;
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { (format!("{} ({})", artifact.artifact.title.as_deref().unwrap_or("Path matches"), paths.len())) }
            @for path in paths.iter().filter_map(serde_json::Value::as_str).take(30) {
                div color="#f0f6fc" font-size=12 font-family="monospace" white-space="preserve-wrap" border-top="1, #30363d" padding-top=4 margin-top=4 { (path) }
            }
            @if paths.len() > 30 {
                div color="#8b949e" font-size=12 margin-top=8 { "… " ((paths.len() - 30).to_string()) " more paths" }
            }
            (filesystem_result_metadata(&artifact.artifact.metadata))
        }
    })
}

fn render_filesystem_grep_result(artifact: &ToolArtifactView) -> Option<Containers> {
    render_grep_matches(artifact, "Search matches")
}

fn render_filesystem_stat_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let exists = metadata
        .get("exists")
        .and_then(serde_json::Value::as_bool)?;
    let kind = metadata.get("kind").and_then(serde_json::Value::as_str);
    let len = metadata.get("len").and_then(serde_json::Value::as_u64);
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { (artifact.artifact.title.as_deref().unwrap_or("Path metadata")) }
            div color="#f0f6fc" { "exists: " (exists.to_string()) }
            @if let Some(kind) = kind { div color="#8b949e" font-size=12 margin-top=4 { "kind: " (kind) } }
            @if let Some(len) = len { div color="#8b949e" font-size=12 margin-top=4 { "len: " (len.to_string()) } }
        }
    })
}

fn render_filesystem_artifact_metadata(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let path = metadata.get("path").and_then(serde_json::Value::as_str)?;
    let exists = metadata.get("exists").and_then(serde_json::Value::as_bool);
    let kind = metadata.get("kind").and_then(serde_json::Value::as_str);
    let byte_len = metadata.get("byte_len").and_then(serde_json::Value::as_u64);
    let content_type = metadata
        .get("content_type")
        .and_then(serde_json::Value::as_str);
    let complete = metadata
        .get("complete")
        .and_then(serde_json::Value::as_bool);
    let message = metadata.get("message").and_then(serde_json::Value::as_str);
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { (artifact.artifact.title.as_deref().unwrap_or("Artifact metadata")) }
            div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (path) }
            @if let Some(exists) = exists { div color="#8b949e" font-size=12 margin-top=4 { "exists: " (exists.to_string()) } }
            @if let Some(kind) = kind { div color="#8b949e" font-size=12 margin-top=4 { "kind: " (kind) } }
            @if let Some(byte_len) = byte_len { div color="#8b949e" font-size=12 margin-top=4 { "bytes: " (byte_len.to_string()) } }
            @if let Some(content_type) = content_type { div color="#8b949e" font-size=12 margin-top=4 { "type: " (content_type) } }
            @if let Some(complete) = complete { div color="#8b949e" font-size=12 margin-top=4 { "complete: " (complete.to_string()) } }
            @if let Some(message) = message { div color="#8b949e" font-size=12 margin-top=4 white-space="preserve-wrap" { (message) } }
        }
    })
}

fn render_filesystem_artifact_read(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let path = metadata.get("path").and_then(serde_json::Value::as_str)?;
    let contents = metadata.get("contents").and_then(serde_json::Value::as_str);
    let returned_bytes = metadata
        .get("returned_bytes")
        .and_then(serde_json::Value::as_u64);
    let total_bytes = metadata
        .get("total_bytes")
        .and_then(serde_json::Value::as_u64);
    let truncated = metadata
        .get("truncated")
        .and_then(serde_json::Value::as_bool);
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { (artifact.artifact.title.as_deref().unwrap_or("Artifact contents")) }
            div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (path) }
            @if let Some(returned_bytes) = returned_bytes { div color="#8b949e" font-size=12 margin-top=4 { "returned bytes: " (returned_bytes.to_string()) } }
            @if let Some(total_bytes) = total_bytes { div color="#8b949e" font-size=12 margin-top=4 { "total bytes: " (total_bytes.to_string()) } }
            @if let Some(truncated) = truncated { div color="#8b949e" font-size=12 margin-top=4 { "truncated: " (truncated.to_string()) } }
            @if let Some(contents) = contents { div color="#c9d1d9" font-size=12 font-family="monospace" white-space="preserve-wrap" border-top="1, #30363d" margin-top=8 padding-top=8 { (contents) } }
        }
    })
}

fn render_filesystem_artifact_grep(artifact: &ToolArtifactView) -> Option<Containers> {
    render_grep_matches(artifact, "Artifact matches")
}

fn render_grep_matches(
    artifact: &ToolArtifactView,
    fallback_title: &'static str,
) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let matches = metadata
        .get("matches")
        .and_then(serde_json::Value::as_array)?;
    let path = metadata.get("path").and_then(serde_json::Value::as_str);
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { (format!("{} ({})", artifact.artifact.title.as_deref().unwrap_or(fallback_title), matches.len())) }
            @if let Some(path) = path { div color="#8b949e" font-size=12 font-family="monospace" white-space="preserve-wrap" margin-bottom=6 { (path) } }
            @for hit in matches.iter().take(30) {
                @if let Some(hit) = hit.as_object() {
                    div border-top="1, #30363d" padding-top=6 margin-top=6 {
                        @if let Some(path) = hit.get("path").and_then(serde_json::Value::as_str) { div color="#f0f6fc" font-size=12 font-family="monospace" white-space="preserve-wrap" { (path) } }
                        @if let Some(line_number) = hit.get("line_number").and_then(serde_json::Value::as_u64) { span color="#8b949e" font-size=12 { (line_number.to_string()) ": " } }
                        @if let Some(line) = hit.get("line").and_then(serde_json::Value::as_str) { span color="#c9d1d9" font-size=12 white-space="preserve-wrap" { (line) } }
                    }
                }
            }
            @if matches.len() > 30 {
                div color="#8b949e" font-size=12 margin-top=8 { "… " ((matches.len() - 30).to_string()) " more matches" }
            }
            (filesystem_result_metadata(metadata))
        }
    })
}

fn filesystem_result_metadata(metadata: &serde_json::Value) -> Containers {
    let backend = metadata.get("backend").and_then(serde_json::Value::as_str);
    let visited_entries = metadata
        .get("visited_entries")
        .and_then(serde_json::Value::as_u64);
    let partial = metadata.get("partial").and_then(serde_json::Value::as_bool);
    let timed_out = metadata
        .get("timed_out")
        .and_then(serde_json::Value::as_bool);
    let message = metadata.get("message").and_then(serde_json::Value::as_str);
    container! {
        @if backend.is_some() || visited_entries.is_some() || partial.is_some() || timed_out.is_some() || message.is_some() {
            div color="#8b949e" font-size=12 margin-top=8 {
                @if let Some(backend) = backend { div { "backend: " (backend) } }
                @if let Some(visited_entries) = visited_entries { div { "visited entries: " (visited_entries.to_string()) } }
                @if let Some(partial) = partial { div { "partial: " (partial.to_string()) } }
                @if let Some(timed_out) = timed_out { div { "timed out: " (timed_out.to_string()) } }
                @if let Some(message) = message { div white-space="preserve-wrap" { (message) } }
            }
        }
    }
}

fn render_git_clone_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let repo = metadata.get("repo").and_then(serde_json::Value::as_str)?;
    let owner = metadata.get("owner").and_then(serde_json::Value::as_str);
    let host = metadata.get("host").and_then(serde_json::Value::as_str);
    let clone_url = metadata
        .get("clone_url")
        .and_then(serde_json::Value::as_str);
    let path = metadata.get("path").and_then(serde_json::Value::as_str);
    let already_exists = metadata
        .get("already_exists")
        .and_then(serde_json::Value::as_bool);
    let repo_label = owner.map_or_else(|| repo.to_owned(), |owner| format!("{owner}/{repo}"));
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div direction=row gap=8 align-items=center margin-bottom=6 {
                span color="#58a6ff" { (artifact.artifact.title.as_deref().unwrap_or("Repository clone")) }
                @if let Some(host) = host { span color="#8b949e" { (host) } }
            }
            div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (repo_label) }
            @if let Some(path) = path { div color="#8b949e" font-size=12 margin-top=4 font-family="monospace" white-space="preserve-wrap" { "path: " (path) } }
            @if let Some(clone_url) = clone_url { div color="#8b949e" font-size=12 margin-top=4 font-family="monospace" white-space="preserve-wrap" { "remote: " (clone_url) } }
            @if let Some(already_exists) = already_exists { div color="#8b949e" font-size=12 margin-top=4 { "already exists: " (already_exists.to_string()) } }
        }
    })
}

fn render_ocr_extract_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let text = metadata.get("text").and_then(serde_json::Value::as_str)?;
    let source = metadata
        .get("source")
        .and_then(serde_json::Value::as_object);
    let path = source
        .and_then(|source| source.get("path"))
        .and_then(serde_json::Value::as_str);
    let url = source
        .and_then(|source| source.get("url"))
        .and_then(serde_json::Value::as_str);
    let engine = metadata.get("engine").and_then(serde_json::Value::as_str);
    let language = metadata.get("language").and_then(serde_json::Value::as_str);
    let truncated = metadata
        .get("truncated")
        .and_then(serde_json::Value::as_bool);
    let text_bytes = metadata
        .get("text_bytes")
        .and_then(serde_json::Value::as_u64);
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { (artifact.artifact.title.as_deref().unwrap_or("OCR extraction")) }
            @if let Some(path) = path { div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (path) } }
            @if let Some(url) = url { div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (url) } }
            @if let Some(engine) = engine { div color="#8b949e" font-size=12 margin-top=4 { "engine: " (engine) } }
            @if let Some(language) = language { div color="#8b949e" font-size=12 margin-top=4 { "language: " (language) } }
            @if let Some(text_bytes) = text_bytes { div color="#8b949e" font-size=12 margin-top=4 { "text bytes: " (text_bytes.to_string()) } }
            @if let Some(truncated) = truncated { div color="#8b949e" font-size=12 margin-top=4 { "truncated: " (truncated.to_string()) } }
            div color="#c9d1d9" font-size=12 white-space="preserve-wrap" border-top="1, #30363d" margin-top=8 padding-top=8 { (text) }
        }
    })
}

fn render_ocr_status(artifact: &ToolArtifactView) -> Option<Containers> {
    render_extract_capabilities(artifact, "OCR engines", "engines")
}

fn render_extract_capabilities(
    artifact: &ToolArtifactView,
    title: &'static str,
    entries_key: &'static str,
) -> Option<Containers> {
    let extract = artifact
        .artifact
        .metadata
        .get("extract")
        .and_then(serde_json::Value::as_object)?;
    let available = extract
        .get("available")
        .and_then(serde_json::Value::as_bool);
    let entries = extract
        .get(entries_key)
        .and_then(serde_json::Value::as_array);
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { (artifact.artifact.title.as_deref().unwrap_or(title)) }
            @if let Some(available) = available { div color="#8b949e" font-size=12 margin-bottom=4 { "available: " (available.to_string()) } }
            @if let Some(entries) = entries {
                @for entry in entries {
                    @if let Some(entry) = entry.as_object() {
                        div border-top="1, #30363d" padding-top=6 margin-top=6 {
                            @if let Some(name) = entry.get("name").and_then(serde_json::Value::as_str) { span color="#f0f6fc" { (name) } }
                            @if let Some(quality) = entry.get("quality").and_then(serde_json::Value::as_str) { span color="#8b949e" { " · " (quality) } }
                            @if let Some(available) = entry.get("available").and_then(serde_json::Value::as_bool) { span color="#8b949e" { " · available: " (available.to_string()) } }
                        }
                    }
                }
            }
        }
    })
}

fn render_web_search_results(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let query = metadata.get("query").and_then(serde_json::Value::as_str);
    let provider = metadata.get("provider").and_then(serde_json::Value::as_str);
    let partial = metadata.get("partial").and_then(serde_json::Value::as_bool);
    let message = metadata.get("message").and_then(serde_json::Value::as_str);
    let results = metadata.get("results")?.as_array()?;
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div direction=row gap=8 align-items=center margin-bottom=6 {
                span color="#58a6ff" { (artifact.artifact.title.as_deref().unwrap_or("Search results")) }
                @if let Some(provider) = provider {
                    span color="#8b949e" { (provider) }
                }
            }
            @if let Some(query) = query {
                div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" margin-bottom=8 { (query) }
            }
            @for (index, result) in results.iter().take(10).enumerate() {
                @if let Some(result) = result.as_object() {
                    div border-top="1, #30363d" padding-top=8 margin-top=8 {
                        div color="#58a6ff" font-size=12 margin-bottom=2 { (format!("{}.", index + 1)) }
                        @if let Some(title) = result.get("title").and_then(serde_json::Value::as_str) {
                            div color="#f0f6fc" white-space="preserve-wrap" { (title) }
                        }
                        @if let Some(url) = result.get("url").and_then(serde_json::Value::as_str) {
                            div color="#8b949e" font-size=12 font-family="monospace" white-space="preserve-wrap" margin-top=2 { (url) }
                        }
                        @if let Some(snippet) = result.get("snippet").and_then(serde_json::Value::as_str) {
                            div color="#c9d1d9" font-size=12 white-space="preserve-wrap" margin-top=4 { (snippet) }
                        }
                    }
                }
            }
            @if results.len() > 10 {
                div color="#8b949e" font-size=12 margin-top=8 { "… " ((results.len() - 10).to_string()) " more results" }
            }
            @if partial == Some(true) {
                div color="#f2cc60" font-size=12 margin-top=8 { "partial results" }
            }
            @if let Some(message) = message {
                div color="#8b949e" font-size=12 margin-top=4 white-space="preserve-wrap" { (message) }
            }
        }
    })
}

fn render_web_fetch_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let url = metadata
        .get("final_url")
        .or_else(|| metadata.get("url"))
        .and_then(serde_json::Value::as_str)?;
    let title = metadata.get("title").and_then(serde_json::Value::as_str);
    let status = metadata.get("status").and_then(serde_json::Value::as_u64);
    let content_type = metadata
        .get("content_type")
        .and_then(serde_json::Value::as_str);
    let content_format = metadata
        .get("content_format")
        .and_then(serde_json::Value::as_str);
    let rendered = metadata
        .get("rendered")
        .and_then(serde_json::Value::as_bool);
    let truncated = metadata
        .get("truncated")
        .and_then(serde_json::Value::as_bool);
    let preview = metadata
        .get("markdown")
        .or_else(|| metadata.get("text"))
        .and_then(serde_json::Value::as_str);
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { (artifact.artifact.title.as_deref().unwrap_or("Fetched page")) }
            @if let Some(title) = title {
                div color="#f0f6fc" white-space="preserve-wrap" margin-bottom=4 { (title) }
            }
            div color="#8b949e" font-size=12 font-family="monospace" white-space="preserve-wrap" margin-bottom=8 { (url) }
            @if let Some(status) = status {
                div color="#8b949e" font-size=12 margin-top=4 { "status: " (status.to_string()) }
            }
            @if let Some(content_type) = content_type {
                div color="#8b949e" font-size=12 margin-top=4 { "type: " (content_type) }
            }
            @if let Some(content_format) = content_format {
                div color="#8b949e" font-size=12 margin-top=4 { "format: " (content_format) }
            }
            @if let Some(rendered) = rendered {
                div color="#8b949e" font-size=12 margin-top=4 { "rendered: " (rendered.to_string()) }
            }
            @if let Some(truncated) = truncated {
                div color="#8b949e" font-size=12 margin-top=4 { "truncated: " (truncated.to_string()) }
            }
            @if let Some(preview) = preview {
                div color="#c9d1d9" font-size=12 white-space="preserve-wrap" border-top="1, #30363d" margin-top=8 padding-top=8 { (preview) }
            }
        }
    })
}

fn render_worktree_list_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let worktrees = artifact
        .artifact
        .metadata
        .get("worktrees")
        .or_else(|| artifact.artifact.metadata.get("entries"))
        .and_then(serde_json::Value::as_array)?;
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { (format!("{} ({})", artifact.artifact.title.as_deref().unwrap_or("Worktrees"), worktrees.len())) }
            @for worktree in worktrees.iter().take(20) {
                @if let Some(worktree) = worktree.as_object() {
                    div border-top="1, #30363d" padding-top=6 margin-top=6 {
                        @if let Some(path) = worktree.get("path").and_then(serde_json::Value::as_str) {
                            div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (path) }
                        }
                        @if let Some(branch) = worktree.get("branch").and_then(serde_json::Value::as_str) {
                            div color="#8b949e" font-size=12 margin-top=2 { "branch: " (branch) }
                        }
                        @if let Some(commit) = worktree.get("commit").and_then(serde_json::Value::as_str) {
                            div color="#8b949e" font-size=12 margin-top=2 { "commit: " (commit) }
                        }
                        @if let Some(is_main) = worktree.get("is_main").and_then(serde_json::Value::as_bool) {
                            div color="#8b949e" font-size=12 margin-top=2 { "main: " (is_main.to_string()) }
                        }
                    }
                }
            }
            @if worktrees.len() > 20 {
                div color="#8b949e" font-size=12 margin-top=8 { "… " ((worktrees.len() - 20).to_string()) " more worktrees" }
            }
        }
    })
}

fn render_worktree_create_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let path = metadata.get("path").and_then(serde_json::Value::as_str)?;
    let repo_root = metadata
        .get("repo_root")
        .and_then(serde_json::Value::as_str);
    let branch = metadata.get("branch").and_then(serde_json::Value::as_str);
    let created_branch = metadata
        .get("created_branch")
        .and_then(serde_json::Value::as_bool);
    let setup_applied = metadata
        .get("setup_applied")
        .and_then(serde_json::Value::as_bool);
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { (artifact.artifact.title.as_deref().unwrap_or("Worktree created")) }
            div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (path) }
            @if let Some(repo_root) = repo_root { div color="#8b949e" font-size=12 margin-top=4 font-family="monospace" white-space="preserve-wrap" { "repo: " (repo_root) } }
            @if let Some(branch) = branch { div color="#8b949e" font-size=12 margin-top=4 { "branch: " (branch) } }
            @if let Some(created_branch) = created_branch { div color="#8b949e" font-size=12 margin-top=4 { "created branch: " (created_branch.to_string()) } }
            @if let Some(setup_applied) = setup_applied { div color="#8b949e" font-size=12 margin-top=4 { "setup applied: " (setup_applied.to_string()) } }
        }
    })
}

fn render_worktree_remove_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let path = artifact
        .artifact
        .metadata
        .get("path")
        .and_then(serde_json::Value::as_str)?;
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { (artifact.artifact.title.as_deref().unwrap_or("Worktree removed")) }
            div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (path) }
        }
    })
}

fn render_plugin_visual(title: &str, visual: &PluginVisualView) -> Containers {
    let rich = VISUAL_ADAPTERS
        .get(&(
            visual.descriptor.schema.as_str(),
            visual.descriptor.schema_version,
        ))
        .and_then(|adapter| adapter(visual));
    container! {
        @if let Some(rich) = rich {
            (rich)
        }
        (json_panel(title, &visual.generic_payload))
    }
}

fn render_structured_visual(visual: &PluginVisualView) -> Option<Containers> {
    let payload = &visual.descriptor.payload;
    let arguments = payload.get("arguments").unwrap_or(payload);
    let object = arguments.as_object()?;
    let fields = object
        .iter()
        .filter(|(key, value)| {
            !key.starts_with('_') && !value.is_null() && !value.is_object() && !value.is_array()
        })
        .map(|(key, value)| {
            let value = value
                .as_str()
                .map_or_else(|| value.to_string(), ToOwned::to_owned);
            (key.replace('_', " "), value)
        })
        .collect::<Vec<_>>();
    if fields.is_empty() {
        return None;
    }
    let title = visual
        .descriptor
        .title
        .as_deref()
        .unwrap_or(visual.descriptor.schema.as_str());
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=8 { (title) }
            @for (key, value) in fields {
                div direction=row gap=8 margin-bottom=4 {
                    span color="#8b949e" min-width=120 { (key) }
                    span color="#f0f6fc" white-space="preserve-wrap" { (value) }
                }
            }
        }
    })
}

fn render_filesystem_request(visual: &PluginVisualView) -> Option<Containers> {
    let payload = visual
        .descriptor
        .payload
        .get("arguments")
        .unwrap_or(&visual.descriptor.payload);
    let operation = payload
        .get("operation")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("filesystem");
    let path = payload.get("path").and_then(serde_json::Value::as_str)?;
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div direction=row gap=8 align-items=center margin-bottom=6 {
                span color="#58a6ff" { (operation) }
                span color="#8b949e" { "filesystem" }
            }
            div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (path) }
            @for key in ["pattern", "query", "url", "region"] {
                @if let Some(value) = payload.get(key).and_then(serde_json::Value::as_str) {
                    div color="#8b949e" font-size=12 margin-top=4 { (key) ": " (value) }
                }
            }
        }
    })
}

fn render_filesystem_change(visual: &PluginVisualView) -> Option<Containers> {
    let payload = visual
        .descriptor
        .payload
        .get("arguments")
        .unwrap_or(&visual.descriptor.payload);
    let path = payload.get("path").and_then(serde_json::Value::as_str)?;
    let old_text = payload.get("old_text").and_then(serde_json::Value::as_str);
    let new_text = payload.get("new_text").and_then(serde_json::Value::as_str);
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { (visual.descriptor.title.as_deref().unwrap_or("Filesystem change")) }
            div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (path) }
            @if let Some(old_text) = old_text {
                div color="#f85149" font-family="monospace" white-space="preserve-wrap" border-top="1, #30363d" margin-top=8 padding-top=8 { "- " (old_text) }
            }
            @if let Some(new_text) = new_text {
                div color="#7ee787" font-family="monospace" white-space="preserve-wrap" border-top="1, #30363d" margin-top=8 padding-top=8 { "+ " (new_text) }
            }
        }
    })
}

fn render_vim_edit_request(visual: &PluginVisualView) -> Option<Containers> {
    let arguments = visual
        .descriptor
        .payload
        .get("arguments")
        .unwrap_or(&visual.descriptor.payload);
    let single_path = arguments.get("path").and_then(serde_json::Value::as_str);
    let files = arguments.get("files").and_then(serde_json::Value::as_array);
    let steps = arguments.get("steps").and_then(serde_json::Value::as_array);
    let sandbox = arguments.get("sandbox").and_then(serde_json::Value::as_str);
    let timeout_ms = arguments
        .get("timeout_ms")
        .and_then(serde_json::Value::as_u64);
    if single_path.is_none() && files.is_none() {
        return None;
    }
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { (visual.descriptor.title.as_deref().unwrap_or("Vim edit")) }
            @if let Some(path) = single_path {
                div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (path) }
                @if let Some(steps) = steps {
                    div color="#8b949e" font-size=12 margin-top=4 { "steps: " (steps.len().to_string()) }
                }
            }
            @if let Some(files) = files {
                @for file in files.iter().take(10) {
                    @if let Some(file) = file.as_object() {
                        div border-top="1, #30363d" padding-top=6 margin-top=6 {
                            @if let Some(path) = file.get("path").and_then(serde_json::Value::as_str) {
                                span color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (path) }
                            }
                            @if let Some(steps) = file.get("steps").and_then(serde_json::Value::as_array) {
                                span color="#8b949e" { " · steps: " (steps.len().to_string()) }
                            }
                        }
                    }
                }
                @if files.len() > 10 {
                    div color="#8b949e" font-size=12 margin-top=8 { "… " ((files.len() - 10).to_string()) " more files" }
                }
            }
            @if let Some(sandbox) = sandbox { div color="#8b949e" font-size=12 margin-top=4 { "sandbox: " (sandbox) } }
            @if let Some(timeout_ms) = timeout_ms { div color="#8b949e" font-size=12 margin-top=4 { "timeout: " (timeout_ms.to_string()) " ms" } }
        }
    })
}

fn render_git_clone_request(visual: &PluginVisualView) -> Option<Containers> {
    let arguments = visual
        .descriptor
        .payload
        .get("arguments")
        .unwrap_or(&visual.descriptor.payload);
    let url = arguments.get("url")?.as_str()?;
    let reference = arguments
        .get("ref")
        .or_else(|| arguments.get("branch"))
        .and_then(serde_json::Value::as_str);
    let destination = arguments
        .get("destination")
        .and_then(serde_json::Value::as_str);
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div color="#58a6ff" margin-bottom=6 { "Clone repository" }
            div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (url) }
            @if let Some(reference) = reference { div color="#8b949e" font-size=12 margin-top=4 { "ref: " (reference) } }
            @if let Some(destination) = destination { div color="#8b949e" font-size=12 margin-top=4 font-family="monospace" white-space="preserve-wrap" { "destination: " (destination) } }
        }
    })
}

fn render_worktree_request(visual: &PluginVisualView) -> Option<Containers> {
    let arguments = visual
        .descriptor
        .payload
        .get("arguments")
        .unwrap_or(&visual.descriptor.payload);
    let operation = arguments
        .get("operation")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("worktree");
    let primary_path = arguments
        .get("path")
        .or_else(|| arguments.get("name"))
        .and_then(serde_json::Value::as_str)?;
    let cwd = arguments.get("cwd").and_then(serde_json::Value::as_str);
    let branch = arguments
        .get("branch")
        .or_else(|| arguments.get("new_branch"))
        .and_then(serde_json::Value::as_str);
    let base_ref = arguments
        .get("base_ref")
        .and_then(serde_json::Value::as_str);
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div direction=row gap=8 align-items=center margin-bottom=6 {
                span color="#58a6ff" { (operation) }
                span color="#8b949e" { "worktree" }
            }
            div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (primary_path) }
            @if let Some(cwd) = cwd { div color="#8b949e" font-size=12 margin-top=4 font-family="monospace" white-space="preserve-wrap" { "cwd: " (cwd) } }
            @if let Some(branch) = branch { div color="#8b949e" font-size=12 margin-top=4 { "branch: " (branch) } }
            @if let Some(base_ref) = base_ref { div color="#8b949e" font-size=12 margin-top=4 { "base ref: " (base_ref) } }
            @for key in ["detach", "force", "no_setup"] {
                @if let Some(value) = arguments.get(key).and_then(serde_json::Value::as_bool) {
                    div color="#8b949e" font-size=12 margin-top=4 { (key.replace('_', " ")) ": " (value.to_string()) }
                }
            }
        }
    })
}

fn render_web_search_request(visual: &PluginVisualView) -> Option<Containers> {
    let arguments = visual
        .descriptor
        .payload
        .get("arguments")
        .unwrap_or(&visual.descriptor.payload);
    let query = arguments.get("query")?.as_str()?;
    let provider = arguments
        .get("provider")
        .and_then(serde_json::Value::as_str);
    let site = arguments.get("site").and_then(serde_json::Value::as_str);
    let freshness = arguments
        .get("freshness")
        .and_then(serde_json::Value::as_str);
    let region = arguments.get("region").and_then(serde_json::Value::as_str);
    let safe_search = arguments
        .get("safe_search")
        .and_then(serde_json::Value::as_str);
    let max_results = arguments
        .get("max_results")
        .and_then(serde_json::Value::as_u64);
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div direction=row gap=8 align-items=center margin-bottom=6 {
                span color="#58a6ff" { "Web search" }
                @if let Some(provider) = provider {
                    span color="#8b949e" { (provider) }
                }
            }
            div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (query) }
            @if let Some(site) = site {
                div color="#8b949e" font-size=12 margin-top=4 { "site: " (site) }
            }
            @if let Some(freshness) = freshness {
                div color="#8b949e" font-size=12 margin-top=4 { "freshness: " (freshness) }
            }
            @if let Some(region) = region {
                div color="#8b949e" font-size=12 margin-top=4 { "region: " (region) }
            }
            @if let Some(safe_search) = safe_search {
                div color="#8b949e" font-size=12 margin-top=4 { "safe search: " (safe_search) }
            }
            @if let Some(max_results) = max_results {
                div color="#8b949e" font-size=12 margin-top=4 { "max results: " (max_results.to_string()) }
            }
        }
    })
}

fn render_web_fetch_request(visual: &PluginVisualView) -> Option<Containers> {
    let arguments = visual
        .descriptor
        .payload
        .get("arguments")
        .unwrap_or(&visual.descriptor.payload);
    let url = arguments.get("url")?.as_str()?;
    let provider = arguments
        .get("provider")
        .and_then(serde_json::Value::as_str);
    let render = arguments.get("render").and_then(serde_json::Value::as_bool);
    let max_bytes = arguments
        .get("max_bytes")
        .and_then(serde_json::Value::as_u64);
    let timeout_ms = arguments
        .get("timeout_ms")
        .and_then(serde_json::Value::as_u64);
    let prompt = arguments.get("prompt").and_then(serde_json::Value::as_str);
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            div direction=row gap=8 align-items=center margin-bottom=6 {
                span color="#58a6ff" { "Fetch page" }
                @if let Some(provider) = provider {
                    span color="#8b949e" { (provider) }
                }
            }
            div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (url) }
            @if let Some(render) = render {
                div color="#8b949e" font-size=12 margin-top=4 { "rendered browser fetch: " (render.to_string()) }
            }
            @if let Some(max_bytes) = max_bytes {
                div color="#8b949e" font-size=12 margin-top=4 { "max bytes: " (max_bytes.to_string()) }
            }
            @if let Some(timeout_ms) = timeout_ms {
                div color="#8b949e" font-size=12 margin-top=4 { "timeout: " (timeout_ms.to_string()) " ms" }
            }
            @if let Some(prompt) = prompt {
                div color="#8b949e" font-size=12 margin-top=4 white-space="preserve-wrap" { "prompt: " (prompt) }
            }
        }
    })
}

fn render_shell_request(visual: &PluginVisualView) -> Option<Containers> {
    let payload = &visual.descriptor.payload;
    let arguments = payload.get("arguments").unwrap_or(payload);
    let command = arguments.get("command")?.as_str()?;
    let cwd = arguments.get("cwd").and_then(serde_json::Value::as_str);
    let output = payload
        .pointer("/_bcode_runtime/output")
        .and_then(serde_json::Value::as_str);
    Some(container! {
        div border="1, #30363d" border-radius=6 background="#010409" padding=10 margin-top=8 {
            @if let Some(cwd) = cwd {
                div color="#8b949e" font-size=11 margin-bottom=4 { (cwd) }
            }
            div color="#f0f6fc" font-family="monospace" white-space="preserve-wrap" { (command) }
            @if let Some(output) = output {
                div color="#c9d1d9" font-family="monospace" white-space="preserve-wrap" border-top="1, #30363d" margin-top=8 padding-top=8 { (output) }
            }
        }
    })
}

fn json_panel(title: &str, value: &serde_json::Value) -> Containers {
    let json = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    container! {
        details margin-top=8 {
            summary color="#8b949e" { (title) }
            div white-space="preserve-wrap" background="#010409" border="1, #30363d" border-radius=6 padding=8 color="#c9d1d9" { (json) }
        }
    }
}

const fn item_label(kind: &TranscriptViewItemKind) -> &'static str {
    match kind {
        TranscriptViewItemKind::UserMessage { .. } => "user",
        TranscriptViewItemKind::AssistantMessage { .. } => "assistant",
        TranscriptViewItemKind::ReasoningMessage { .. } => "reasoning",
        TranscriptViewItemKind::ToolInvocation { .. } => "tool",
        TranscriptViewItemKind::ToolRequest { .. } => "tool request",
        TranscriptViewItemKind::Permission { .. } => "permission",
        TranscriptViewItemKind::RuntimeWork { .. } => "runtime work",
        TranscriptViewItemKind::Usage { .. } => "usage",
        TranscriptViewItemKind::Compaction { .. } => "compaction",
        TranscriptViewItemKind::Interaction { .. } => "interaction",
        TranscriptViewItemKind::Skill { skill } => match skill.status {
            bcode_session_view_models::SkillViewStatus::ContextLoaded => "skill context",
            bcode_session_view_models::SkillViewStatus::Failed => "skill error",
            bcode_session_view_models::SkillViewStatus::Invoked
            | bcode_session_view_models::SkillViewStatus::Suggested => "skill",
        },
        TranscriptViewItemKind::SystemMessage { .. } => "system",
        TranscriptViewItemKind::PluginVisual { .. } => "plugin visual",
        TranscriptViewItemKind::ToolContribution { .. } => "tool contribution",
    }
}

const fn tool_status_color(status: ToolInvocationViewStatus) -> &'static str {
    match status {
        ToolInvocationViewStatus::Requested => "#8b949e",
        ToolInvocationViewStatus::Running => "#7ee787",
        ToolInvocationViewStatus::Finished => "#58a6ff",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{PluginVisualDescriptor, RuntimeWorkStatus, ToolArtifact, WorkId};
    use bcode_session_view_models::{
        ChatMessageView, CompactionView, CompactionViewStatus, PermissionBatchView, PermissionView,
        RuntimeWorkView, SkillView, SkillViewStatus, ToolArtifactView, ToolInvocationView,
        ToolResultView, ToolTimingView, TranscriptViewItem, TranscriptViewItemId,
    };

    #[test]
    fn reasoning_items_follow_shared_visibility() {
        let item = TranscriptViewItem {
            id: TranscriptViewItemId::new("reasoning:test"),
            sequence: Some(1),
            timestamp_ms: None,
            revision: 1,
            streaming: false,
            kind: TranscriptViewItemKind::ReasoningMessage {
                message: ChatMessageView::markdown("hidden"),
            },
        };

        assert!(!should_render_transcript_item(&item, false));
        assert!(should_render_transcript_item(&item, true));
    }

    #[test]
    fn skill_transcript_item_renders_semantic_label_and_text() {
        let item = TranscriptViewItem {
            id: TranscriptViewItemId::new("skill:test"),
            sequence: Some(1),
            timestamp_ms: None,
            revision: 1,
            streaming: false,
            kind: TranscriptViewItemKind::Skill {
                skill: SkillView {
                    skill_id: "review".to_owned(),
                    status: SkillViewStatus::Failed,
                    text: "review: boom".to_owned(),
                },
            },
        };

        assert_eq!(item_label(&item.kind), "skill error");
        let rendered = format!("{:?}", transcript_item(&item));
        assert!(rendered.contains("review: boom"));

        let context_item = TranscriptViewItem {
            id: TranscriptViewItemId::new("skill:context"),
            sequence: Some(2),
            timestamp_ms: None,
            revision: 1,
            streaming: false,
            kind: TranscriptViewItemKind::Skill {
                skill: SkillView {
                    skill_id: "review".to_owned(),
                    status: SkillViewStatus::ContextLoaded,
                    text: "loaded review".to_owned(),
                },
            },
        };
        assert_eq!(item_label(&context_item.kind), "skill context");
        let rendered = format!("{:?}", transcript_item(&context_item));
        assert!(rendered.contains("loaded review"));

        let compaction_item = TranscriptViewItem {
            id: TranscriptViewItemId::new("compaction:test"),
            sequence: Some(3),
            timestamp_ms: None,
            revision: 1,
            streaming: false,
            kind: TranscriptViewItemKind::Compaction {
                compaction: CompactionView {
                    status: CompactionViewStatus::Local,
                    text: "local context compaction: summary".to_owned(),
                    provider_plugin_id: None,
                    model_id: None,
                },
            },
        };
        assert_eq!(item_label(&compaction_item.kind), "compaction");
        let rendered = format!("{:?}", transcript_item(&compaction_item));
        assert!(rendered.contains("local context compaction: summary"));
    }

    #[test]
    fn web_shell_renders_all_primary_regions_including_runtime_state() {
        let mut snapshot = SessionViewSnapshot::empty();
        snapshot.session_id = Some(bcode_session_models::SessionId::new());
        snapshot.runtime_work.push(RuntimeWorkView {
            work_id: WorkId::new("work-1"),
            kind: bcode_session_models::RuntimeWorkKind::Tool,
            label: "index workspace".to_owned(),
            status: RuntimeWorkStatus::Running,
            cancellable: true,
            message: Some("indexing".to_owned()),
            completed_units: Some(2),
            total_units: Some(4),
            updated_at_ms: Some(1),
        });

        let rendered = format!("{:?}", home(&snapshot, &[], "secret-token"));
        assert!(rendered.contains("sessions"));
        assert!(rendered.contains("transcript"));
        assert!(rendered.contains("composer"));
        assert!(rendered.contains("daemon connected · session attached"));
        assert!(rendered.contains("runtime work"));
        assert!(rendered.contains("index workspace"));
        assert!(rendered.contains("work-1"));
    }

    #[test]
    fn grouped_permission_renders_per_call_and_apply_to_all_actions() {
        let mut snapshot = SessionViewSnapshot::empty();
        snapshot.session_id = Some(bcode_session_models::SessionId::new());
        snapshot.permissions.push(PermissionView {
            permission_id: "permission-1".to_owned(),
            session_id: snapshot.session_id,
            tool_call_id: "call-1".to_owned(),
            tool_name: "shell.run".to_owned(),
            arguments_json: r#"{"command":"pwd"}"#.to_owned(),
            batch: Some(PermissionBatchView {
                batch_id: "batch-1".to_owned(),
                call_index: 0,
                call_count: 3,
            }),
            agent_id: "agent-1".to_owned(),
            title: Some("Permission requested".to_owned()),
            policy_source: Some("test".to_owned()),
            detail: Some("review".to_owned()),
            resolved: false,
            approved: None,
            can_remember: true,
        });

        let rendered = format!("{:?}", home(&snapshot, &[], "secret-token"));
        assert!(rendered.contains("batch"));
        assert!(rendered.contains('1'));
        assert!(rendered.contains('3'));
        assert!(rendered.contains("approve all"));
        assert!(rendered.contains("deny all"));
        assert!(rendered.contains("/actions/permission-batch"));
        assert!(rendered.contains("/actions/permission?"));
        assert!(rendered.contains("batch-1"));
    }

    #[test]
    fn transcript_history_controls_render_source_anchored_actions() {
        let mut snapshot = SessionViewSnapshot::empty();
        snapshot.session_id = Some(bcode_session_models::SessionId::new());
        snapshot.transcript.source_start_sequence = Some(10);
        snapshot.transcript.source_end_sequence = Some(20);
        snapshot.transcript.has_older_history = true;
        snapshot.transcript.has_newer_history = true;

        let rendered = format!("{:?}", home(&snapshot, &[], "secret-token"));
        assert!(rendered.contains("/actions/history-window?token=secret-token"));
        assert!(rendered.contains("load older history"));
        assert!(rendered.contains("load newer history"));
        assert!(rendered.contains("10"));
        assert!(rendered.contains("20"));
    }

    #[test]
    fn access_token_is_propagated_to_browser_actions() {
        let rendered = format!(
            "{:?}",
            home(&SessionViewSnapshot::empty(), &[], "secret-token")
        );

        assert!(rendered.contains("/actions/submit-message?token=secret-token"));
    }

    #[test]
    fn session_links_propagate_access_token_and_live_scope() {
        let session = bcode_session_models::SessionSummary {
            id: bcode_session_models::SessionId::new(),
            name: Some("session".to_owned()),
            explicit_name: None,
            derived_title: None,
            title_source: bcode_session_models::SessionTitleSource::Explicit,
            client_count: 0,
            created_at_ms: 1,
            updated_at_ms: 1,
            working_directory: "/tmp/project".into(),
            import: None,
            fork: None,
        };
        let rendered = format!(
            "{:?}",
            home(
                &SessionViewSnapshot::empty(),
                std::slice::from_ref(&session),
                "secret-token"
            )
        );
        assert!(
            rendered.contains(&format!(
                "token=secret-token&amp;hyperchad-event-scope=secret-token:{}",
                session.id
            )) || rendered.contains(&format!(
                "token=secret-token&hyperchad-event-scope=secret-token:{}",
                session.id
            ))
        );
    }

    #[test]
    fn question_snapshot_renders_polished_controls_and_generic_fallback() {
        let interaction = InteractionViewSummary {
            interaction_id: "interaction-1".to_owned(),
            kind: "bcode.question".to_owned(),
            surface_kind: "bcode.question.inline".to_owned(),
            tool_call_id: Some("call-1".to_owned()),
            title: Some("Choose".to_owned()),
            required: true,
            snapshot: Some(serde_json::json!({
                "request": {
                    "questions": [{
                        "header": "Decision",
                        "question": "Proceed?",
                        "options": [{"label": "Yes", "value": "yes", "description": "Continue"}],
                        "control": "radio",
                        "selection_mode": "single",
                        "custom": true,
                        "custom_mode": "additional",
                        "required": true
                    }]
                },
                "answers": [{"question_index": 0, "selected": ["yes"], "custom": null}],
                "focus": {"type": "question", "question_index": 0},
                "focused_control_id": "question-0"
            })),
            resolved: false,
            resolution: None,
        };

        let rendered = format!(
            "{:?}",
            interaction_request(
                &interaction,
                Some(bcode_session_models::SessionId::new()),
                "secret-token"
            )
        );
        assert!(rendered.contains("Proceed?"));
        assert!(rendered.contains("Continue"));
        assert!(rendered.contains("question-0.option-0"));
        assert!(rendered.contains("submit answers"));
        assert!(rendered.contains("generic semantic controls"));
    }

    #[test]
    fn bundled_visual_registry_covers_actual_high_value_request_schemas() {
        for schema in [
            "bcode.filesystem.request",
            "bcode.filesystem.change",
            "bcode.filesystem.read",
            "bcode.filesystem.image",
            "bcode.filesystem.exists",
            "bcode.filesystem.list",
            "bcode.filesystem.find",
            "bcode.filesystem.grep",
            "bcode.filesystem.stat",
            "bcode.filesystem.artifact.metadata",
            "bcode.filesystem.artifact.read",
            "bcode.filesystem.artifact.grep",
            "bcode.document.request",
            "bcode.ocr.request",
            "bcode.web-search.search_request",
            "bcode.web-search.fetch_request",
            "bcode.web-search.status_request",
            "bcode.web-search.inspect_request",
            "bcode.git.clone_request",
            "bcode.worktree.request",
            "bcode.vim-edit.request.preview",
            "bcode.vim-edit.request.apply",
            "bcode.vim-edit.live",
            "bcode.vim-edit.playback",
        ] {
            assert!(
                VISUAL_ADAPTERS.contains_key(&(schema, 1)),
                "missing {schema}"
            );
        }
    }

    #[test]
    fn structured_request_adapter_renders_meaningful_fields_and_fallback() {
        let visual = PluginVisualView::from(PluginVisualDescriptor {
            visual_id: Some("filesystem-1".to_owned()),
            producer_plugin_id: Some("bcode.filesystem".to_owned()),
            schema: "bcode.filesystem.request".to_owned(),
            schema_version: 1,
            title: Some("Read file".to_owned()),
            subtitle: None,
            payload: serde_json::json!({"operation": "read", "path": "/tmp/sentinel.txt"}),
        });

        let rendered = format!("{:?}", render_plugin_visual("request visual", &visual));
        assert!(rendered.contains("Read file"));
        assert!(rendered.contains("/tmp/sentinel.txt"));
        assert!(rendered.contains("request visual"));
    }

    #[test]
    fn shell_visual_adapter_is_versioned_and_keeps_generic_fallback() {
        let visual = PluginVisualView::from(PluginVisualDescriptor {
            visual_id: Some("shell-1".to_owned()),
            producer_plugin_id: Some("bcode.shell".to_owned()),
            schema: "bcode.tool.request.shell.run".to_owned(),
            schema_version: 1,
            title: Some("Shell command".to_owned()),
            subtitle: None,
            payload: serde_json::json!({
                "arguments": {"command": "printf sentinel", "cwd": "/tmp"},
                "_bcode_runtime": {"output": "sentinel output"}
            }),
        });

        let rendered = format!("{:?}", render_plugin_visual("plugin visual", &visual));
        assert!(rendered.contains("printf sentinel"));
        assert!(rendered.contains("sentinel output"));
        assert!(rendered.contains("plugin visual"));
    }

    #[test]
    fn unknown_visual_schema_uses_generic_fallback() {
        let visual = PluginVisualView::from(PluginVisualDescriptor {
            visual_id: None,
            producer_plugin_id: Some("fixture".to_owned()),
            schema: "fixture.unknown".to_owned(),
            schema_version: 99,
            title: None,
            subtitle: None,
            payload: serde_json::json!({"sentinel": "generic-only"}),
        });

        let rendered = format!("{:?}", render_plugin_visual("plugin visual", &visual));
        assert!(rendered.contains("generic-only"));
    }

    #[test]
    fn generic_plugin_visual_keeps_schema_payload_in_render_tree() {
        let kind = TranscriptViewItemKind::PluginVisual {
            visual: PluginVisualView::from(PluginVisualDescriptor {
                visual_id: Some("visual-1".to_owned()),
                producer_plugin_id: Some("fixture-plugin".to_owned()),
                schema: "fixture.visual".to_owned(),
                schema_version: 1,
                title: Some("Fixture visual".to_owned()),
                subtitle: None,
                payload: serde_json::json!({"sentinel": "visual-payload"}),
            }),
        };

        let rendered = format!("{:?}", transcript_item_body(&kind));
        assert!(rendered.contains("fixture.visual"));
        assert!(rendered.contains("visual-payload"));
    }

    #[test]
    fn unknown_contribution_has_no_raw_web_fallback() {
        let kind = TranscriptViewItemKind::ToolContribution {
            contribution: bcode_session_models::ToolContributionEvent {
                invocation_id: "call".to_owned(),
                contribution_id: "surface".to_owned(),
                sequence: 9,
                producer_id: "future.producer".to_owned(),
                schema: "future.unknown/schema".to_owned(),
                schema_version: 77,
                operation: bcode_session_models::ToolContributionOperation::Append,
                persistence: bcode_session_models::ToolContributionPersistence::Durable,
                artifact: None,
                payload: serde_json::json!({"sentinel": "opaque-web"}),
            },
        };
        let rendered = format!("{:?}", transcript_item_body(&kind));
        assert!(!rendered.contains("future.unknown/schema"));
        assert!(!rendered.contains("opaque-web"));
        assert!(!rendered.contains("append"));
    }

    #[test]
    fn git_contribution_renders_through_schema_adapter_without_fallback() {
        let kind = TranscriptViewItemKind::ToolContribution {
            contribution: bcode_session_models::ToolContributionEvent {
                invocation_id: "git-call".to_owned(),
                contribution_id: "clone-request".to_owned(),
                sequence: 1,
                producer_id: "bcode.git".to_owned(),
                schema: "bcode.git.clone_request".to_owned(),
                schema_version: 1,
                operation: bcode_session_models::ToolContributionOperation::Upsert,
                persistence: bcode_session_models::ToolContributionPersistence::Durable,
                artifact: None,
                payload: serde_json::json!({
                    "url": "https://github.com/bmorphism/bcode",
                    "ref": "main"
                }),
            },
        };

        let rendered = format!("{:?}", transcript_item_body(&kind));
        assert!(rendered.contains("github.com/bmorphism/bcode"));
        assert!(rendered.contains("main"));
        assert!(!rendered.contains("bcode.git.clone_request"));
    }

    #[test]
    fn unsupported_shell_contribution_has_no_raw_web_fallback() {
        let kind = TranscriptViewItemKind::ToolContribution {
            contribution: bcode_session_models::ToolContributionEvent {
                invocation_id: "shell-call".to_owned(),
                contribution_id: "shell-run-summary".to_owned(),
                sequence: 1,
                producer_id: "bcode.shell".to_owned(),
                schema: "bcode.shell.run.summary".to_owned(),
                schema_version: 1,
                operation: bcode_session_models::ToolContributionOperation::Upsert,
                persistence: bcode_session_models::ToolContributionPersistence::Durable,
                artifact: None,
                payload: serde_json::json!({"output": "shell-render-sentinel"}),
            },
        };
        let rendered = format!("{:?}", transcript_item_body(&kind));
        assert!(!rendered.contains("bcode.shell.run.summary"));
        assert!(!rendered.contains("shell-render-sentinel"));
    }

    #[test]
    fn visual_adapters_are_schema_version_specific_and_keep_fallbacks() {
        let supported = PluginVisualView::from(PluginVisualDescriptor {
            visual_id: Some("filesystem-version-1".to_owned()),
            producer_plugin_id: Some("bcode.filesystem".to_owned()),
            schema: "bcode.filesystem.request".to_owned(),
            schema_version: 1,
            title: Some("Filesystem read".to_owned()),
            subtitle: None,
            payload: serde_json::json!({"operation": "read", "path": "/tmp/versioned"}),
        });
        let supported_rendered = format!("{:?}", render_plugin_visual("plugin visual", &supported));
        assert!(supported_rendered.contains("Filesystem read"));
        assert!(supported_rendered.contains("/tmp/versioned"));
        assert!(supported_rendered.contains("bcode.filesystem.request"));

        let unsupported_version = PluginVisualView::from(PluginVisualDescriptor {
            schema_version: 2,
            ..supported.descriptor
        });
        let unsupported_rendered = format!(
            "{:?}",
            render_plugin_visual("plugin visual", &unsupported_version)
        );
        assert!(
            !VISUAL_ADAPTERS.contains_key(&("bcode.filesystem.request", 2)),
            "unexpected rich adapter for unsupported schema version"
        );
        assert!(unsupported_rendered.contains("bcode.filesystem.request"));
        assert!(unsupported_rendered.contains("/tmp/versioned"));
    }

    #[test]
    fn every_registered_visual_adapter_has_a_fixture() {
        for ((schema, schema_version), adapter) in VISUAL_ADAPTERS.iter() {
            let visual = PluginVisualView::from(PluginVisualDescriptor {
                visual_id: Some(format!("fixture:{schema}:{schema_version}")),
                producer_plugin_id: Some("fixture-plugin".to_owned()),
                schema: (*schema).to_owned(),
                schema_version: *schema_version,
                title: Some(format!("Fixture {schema}")),
                subtitle: None,
                payload: visual_adapter_fixture_payload(schema),
            });
            assert!(
                adapter(&visual).is_some(),
                "adapter fixture did not render {schema}@{schema_version}"
            );
            let rendered = format!("{:?}", render_plugin_visual("plugin visual", &visual));
            assert!(rendered.contains(schema));
            assert!(rendered.contains("fixture"));
        }
    }

    #[test]
    fn every_registered_artifact_adapter_has_a_fixture() {
        for ((schema, schema_version), adapter) in ARTIFACT_ADAPTERS.iter() {
            let artifact = ToolArtifactView::from(ToolArtifact {
                artifact_id: format!("fixture:{schema}:{schema_version}"),
                producer_plugin_id: "fixture-plugin".to_owned(),
                schema: (*schema).to_owned(),
                schema_version: *schema_version,
                tool_call_id: Some("fixture-call".to_owned()),
                title: Some(format!("Fixture {schema}")),
                metadata: artifact_adapter_fixture_metadata(schema),
                refs: Vec::new(),
            });
            assert!(
                adapter(&artifact).is_some(),
                "artifact adapter fixture did not render {schema}@{schema_version}"
            );
            let rendered = format!(
                "{:?}",
                render_tool_result(&ToolResultView::Artifact { artifact })
            );
            assert!(rendered.contains(schema));
            assert!(rendered.contains("fixture"));
        }
    }

    fn artifact_adapter_fixture_metadata(schema: &str) -> serde_json::Value {
        document_artifact_fixture(schema)
            .or_else(|| filesystem_artifact_fixture(schema))
            .or_else(|| ocr_artifact_fixture(schema))
            .or_else(|| web_and_worktree_artifact_fixture(schema))
            .unwrap_or_else(|| serde_json::json!({"fixture": true}))
    }

    fn document_artifact_fixture(schema: &str) -> Option<serde_json::Value> {
        match schema {
            "bcode.document.extract_result" => Some(serde_json::json!({
                "source": "file:///tmp/fixture.pdf",
                "content_type": "application/pdf",
                "artifact_kind": "document",
                "artifact_scope": "session",
                "document_path": "/tmp/fixture.pdf",
                "text_path": "/tmp/fixture.txt",
                "text": "fixture document text",
                "truncated": false,
                "extractor": "native"
            })),
            "bcode.document.status" => Some(serde_json::json!({
                "extract": {
                    "available": true,
                    "extractors": [{
                        "name": "fixture-extractor",
                        "available": true,
                        "quality": "fixture-quality"
                    }],
                    "configured_order": ["fixture-extractor"]
                }
            })),
            _ => None,
        }
    }

    fn filesystem_artifact_fixture(schema: &str) -> Option<serde_json::Value> {
        match schema {
            "bcode.filesystem.read" => Some(serde_json::json!({
                "contents": "fixture file contents"
            })),
            "bcode.filesystem.image" => Some(serde_json::json!({
                "path": "/tmp/fixture.png",
                "mime_type": "image/png",
                "width": 640,
                "height": 480,
                "byte_len": 1024
            })),
            "bcode.filesystem.change" => Some(serde_json::json!({
                "tool_name": "filesystem.edit",
                "summary": "fixture change",
                "path": "/tmp/fixture.txt",
                "old_text": "old fixture",
                "new_text": "new fixture",
                "start_line": 1
            })),
            "bcode.filesystem.exists" => Some(serde_json::json!({
                "exists": true
            })),
            "bcode.filesystem.list" => Some(serde_json::json!({
                "entries": [{"path": "/tmp/fixture.txt", "kind": "file"}],
                "backend": "fixture-backend",
                "timed_out": false,
                "partial": false,
                "visited_entries": 1,
                "message": "fixture message"
            })),
            "bcode.filesystem.find" => Some(serde_json::json!({
                "paths": ["/tmp/fixture.txt"],
                "backend": "fixture-backend",
                "timed_out": false,
                "partial": false,
                "visited_entries": 1,
                "message": "fixture message"
            })),
            "bcode.filesystem.grep" => Some(serde_json::json!({
                "matches": [{"path": "/tmp/fixture.txt", "line_number": 1, "line": "fixture match"}],
                "backend": "fixture-backend",
                "timed_out": false,
                "partial": false,
                "visited_entries": 1,
                "message": "fixture message"
            })),
            "bcode.filesystem.stat" => Some(serde_json::json!({
                "exists": true,
                "kind": "file",
                "len": 128
            })),
            _ => filesystem_artifact_file_fixture(schema),
        }
    }

    fn filesystem_artifact_file_fixture(schema: &str) -> Option<serde_json::Value> {
        match schema {
            "bcode.filesystem.artifact.metadata" => Some(serde_json::json!({
                "path": "/tmp/fixture-artifact.json",
                "exists": true,
                "kind": "file",
                "byte_len": 128,
                "content_type": "application/json",
                "complete": true,
                "message": "fixture message"
            })),
            "bcode.filesystem.artifact.read" => Some(serde_json::json!({
                "path": "/tmp/fixture-artifact.json",
                "offset_bytes": 0,
                "returned_bytes": 16,
                "total_bytes": 16,
                "from_end": false,
                "truncated": false,
                "contents": "fixture artifact"
            })),
            "bcode.filesystem.artifact.grep" => Some(serde_json::json!({
                "path": "/tmp/fixture-artifact.json",
                "matches": [{"path": "/tmp/fixture-artifact.json", "line_number": 1, "line": "fixture artifact match"}],
                "total_bytes": 128,
                "partial": false,
                "message": "fixture message"
            })),
            _ => None,
        }
    }

    fn ocr_artifact_fixture(schema: &str) -> Option<serde_json::Value> {
        match schema {
            "bcode.ocr.extract_result" => Some(serde_json::json!({
                "text": "fixture OCR text",
                "source": {
                    "path": "/tmp/fixture.png",
                    "url": null
                },
                "engine": "tesseract",
                "language": "eng",
                "truncated": false,
                "text_bytes": 16,
                "full_text_bytes": 16
            })),
            "bcode.ocr.status" => Some(serde_json::json!({
                "extract": {
                    "available": true,
                    "default_engine": "tesseract",
                    "engines": [{
                        "name": "tesseract",
                        "available": true,
                        "version": "fixture-version",
                        "quality": "fixture-quality"
                    }]
                }
            })),
            _ => None,
        }
    }

    fn web_and_worktree_artifact_fixture(schema: &str) -> Option<serde_json::Value> {
        match schema {
            "bcode.git.clone_result" => Some(serde_json::json!({
                "host": "github.com",
                "owner": "fixture-owner",
                "repo": "fixture-repo",
                "clone_url": "https://github.com/fixture-owner/fixture-repo.git",
                "path": "/tmp/fixture-repo",
                "already_exists": false
            })),
            "bcode.web-search.search_results" => Some(serde_json::json!({
                "query": "fixture search",
                "provider": "fixture-provider",
                "results": [{
                    "title": "fixture result",
                    "url": "https://example.com/fixture",
                    "snippet": "fixture snippet"
                }],
                "partial": false,
                "message": "fixture message"
            })),
            "bcode.web-search.fetch_result" => Some(serde_json::json!({
                "url": "https://example.com/fixture",
                "final_url": "https://example.com/fixture-final",
                "status": 200,
                "title": "fixture page",
                "content_type": "text/html",
                "content_format": "markdown",
                "rendered": true,
                "truncated": false,
                "markdown": "fixture body"
            })),
            _ => worktree_artifact_fixture(schema),
        }
    }

    fn worktree_artifact_fixture(schema: &str) -> Option<serde_json::Value> {
        match schema {
            "bcode.worktree.list" => Some(serde_json::json!({
                "main_root": "/tmp/fixture-repo",
                "worktrees": [{
                    "path": "/tmp/fixture-worktree",
                    "is_main": false,
                    "branch": "fixture-branch",
                    "commit": "abc1234"
                }]
            })),
            "bcode.worktree.create_result" => Some(serde_json::json!({
                "repo_root": "/tmp/fixture-repo",
                "path": "/tmp/fixture-worktree",
                "branch": "fixture-branch",
                "created_branch": true,
                "setup_applied": false
            })),
            "bcode.worktree.remove_result" => Some(serde_json::json!({
                "path": "/tmp/fixture-worktree"
            })),
            _ => None,
        }
    }

    fn visual_adapter_fixture_payload(schema: &str) -> serde_json::Value {
        match schema {
            "bcode.tool.request.shell.run" => {
                serde_json::json!({"command": "echo fixture", "cwd": "/tmp"})
            }
            "bcode.web-search.search_request" => serde_json::json!({
                "arguments": {
                    "query": "fixture query",
                    "provider": "fixture-provider",
                    "site": "example.com"
                }
            }),
            "bcode.web-search.fetch_request" => serde_json::json!({
                "arguments": {
                    "url": "https://example.com/fixture",
                    "provider": "fixture-provider",
                    "render": true
                }
            }),
            "bcode.git.clone_request" => serde_json::json!({
                "arguments": {
                    "url": "https://github.com/fixture-owner/fixture-repo.git",
                    "ref": "main",
                    "destination": "/tmp/fixture-repo"
                }
            }),
            "bcode.worktree.request" => serde_json::json!({
                "arguments": {
                    "operation": "create",
                    "path": "/tmp/fixture-worktree",
                    "branch": "fixture-branch",
                    "base_ref": "head"
                }
            }),
            "bcode.vim-edit.request.preview" | "bcode.vim-edit.request.apply" => {
                serde_json::json!({
                    "arguments": {
                        "path": "/tmp/fixture.txt",
                        "steps": [{"keys": "ifixture<Esc>"}],
                        "sandbox": "default",
                        "timeout_ms": 1000
                    }
                })
            }
            _ => serde_json::json!({"operation": "fixture", "path": "/tmp/fixture"}),
        }
    }

    #[test]
    fn web_search_request_adapter_renders_query_options_and_fallback() {
        let visual = PluginVisualView::from(PluginVisualDescriptor {
            visual_id: Some("web-search-1".to_owned()),
            producer_plugin_id: Some("bcode.web-search".to_owned()),
            schema: "bcode.web-search.search_request".to_owned(),
            schema_version: 1,
            title: Some("Web search".to_owned()),
            subtitle: None,
            payload: serde_json::json!({
                "arguments": {
                    "query": "renderer neutral app",
                    "provider": "brave",
                    "site": "example.com",
                    "freshness": "week",
                    "region": "us",
                    "safe_search": "moderate",
                    "max_results": 5
                }
            }),
        });

        let rendered = format!("{:?}", render_plugin_visual("plugin visual", &visual));
        assert!(rendered.contains("renderer neutral app"));
        assert!(rendered.contains("brave"));
        assert!(rendered.contains("example.com"));
        assert!(rendered.contains("max results"));
        assert!(rendered.contains("bcode.web-search.search_request"));
    }

    #[test]
    fn web_fetch_request_adapter_renders_url_options_and_fallback() {
        let visual = PluginVisualView::from(PluginVisualDescriptor {
            visual_id: Some("web-fetch-1".to_owned()),
            producer_plugin_id: Some("bcode.web-search".to_owned()),
            schema: "bcode.web-search.fetch_request".to_owned(),
            schema_version: 1,
            title: Some("Fetch page".to_owned()),
            subtitle: None,
            payload: serde_json::json!({
                "arguments": {
                    "url": "https://example.com/page",
                    "provider": "rendered",
                    "render": true,
                    "max_bytes": 4096,
                    "timeout_ms": 1000,
                    "prompt": "extract summary"
                }
            }),
        });

        let rendered = format!("{:?}", render_plugin_visual("plugin visual", &visual));
        assert!(rendered.contains("https://example.com/page"));
        assert!(rendered.contains("rendered"));
        assert!(rendered.contains("max bytes"));
        assert!(rendered.contains("extract summary"));
        assert!(rendered.contains("bcode.web-search.fetch_request"));
    }

    #[test]
    fn web_search_result_adapter_renders_results_and_semantic_fallback() {
        let artifact = ToolArtifactView::from(ToolArtifact {
            artifact_id: "web-search-result".to_owned(),
            producer_plugin_id: "bcode.web-search".to_owned(),
            schema: "bcode.web-search.search_results".to_owned(),
            schema_version: 1,
            tool_call_id: Some("call-web-search".to_owned()),
            title: Some("Search results".to_owned()),
            metadata: serde_json::json!({
                "query": "rust tui web renderer",
                "provider": "brave",
                "results": [{
                    "title": "Renderer Neutral",
                    "url": "https://example.com/renderer",
                    "snippet": "A renderer-neutral search result"
                }],
                "partial": false,
                "message": "ok"
            }),
            refs: Vec::new(),
        });
        let result = ToolResultView::Artifact { artifact };

        let rendered = format!("{:?}", render_tool_result(&result));
        assert!(rendered.contains("rust tui web renderer"));
        assert!(rendered.contains("Renderer Neutral"));
        assert!(rendered.contains("https://example.com/renderer"));
        assert!(rendered.contains("semantic result"));
        assert!(rendered.contains("bcode.web-search.search_results"));
    }

    #[test]
    fn web_fetch_result_adapter_renders_metadata_preview_and_semantic_fallback() {
        let artifact = ToolArtifactView::from(ToolArtifact {
            artifact_id: "web-fetch-result".to_owned(),
            producer_plugin_id: "bcode.web-search".to_owned(),
            schema: "bcode.web-search.fetch_result".to_owned(),
            schema_version: 1,
            tool_call_id: Some("call-web-fetch".to_owned()),
            title: Some("Fetched page".to_owned()),
            metadata: serde_json::json!({
                "url": "https://example.com/original",
                "final_url": "https://example.com/final",
                "status": 200,
                "title": "Example page",
                "content_type": "text/html",
                "content_format": "markdown",
                "rendered": true,
                "truncated": false,
                "markdown": "# Sentinel preview"
            }),
            refs: Vec::new(),
        });
        let result = ToolResultView::Artifact { artifact };

        let rendered = format!("{:?}", render_tool_result(&result));
        assert!(rendered.contains("Example page"));
        assert!(rendered.contains("https://example.com/final"));
        assert!(rendered.contains("Sentinel preview"));
        assert!(rendered.contains("semantic result"));
        assert!(rendered.contains("bcode.web-search.fetch_result"));
    }

    #[test]
    fn filesystem_change_adapter_renders_path_and_diff_fields_with_fallback() {
        let visual = PluginVisualView::from(PluginVisualDescriptor {
            visual_id: Some("change-1".to_owned()),
            producer_plugin_id: Some("bcode.filesystem".to_owned()),
            schema: "bcode.filesystem.change".to_owned(),
            schema_version: 1,
            title: Some("Edit file".to_owned()),
            subtitle: None,
            payload: serde_json::json!({
                "path": "/tmp/example.rs",
                "old_text": "old();",
                "new_text": "new();"
            }),
        });

        let rendered = format!("{:?}", render_plugin_visual("plugin visual", &visual));
        assert!(rendered.contains("/tmp/example.rs"));
        assert!(rendered.contains("old();"));
        assert!(rendered.contains("new();"));
        assert!(rendered.contains("bcode.filesystem.change"));
    }

    #[test]
    fn generic_tool_artifact_keeps_schema_metadata_in_render_tree() {
        let artifact = ToolArtifactView::from(ToolArtifact {
            artifact_id: "artifact-1".to_owned(),
            producer_plugin_id: "fixture-plugin".to_owned(),
            schema: "fixture.artifact".to_owned(),
            schema_version: 1,
            tool_call_id: Some("call-1".to_owned()),
            title: Some("Fixture artifact".to_owned()),
            metadata: serde_json::json!({"sentinel": "artifact-metadata"}),
            refs: Vec::new(),
        });
        let kind = TranscriptViewItemKind::ToolInvocation {
            tool: Box::new(ToolInvocationView {
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: Some("fixture-plugin".to_owned()),
                tool_name: Some("fixture".to_owned()),
                arguments_json: None,
                working_directory: None,
                request_visual: None,
                status: ToolInvocationViewStatus::Finished,
                result_text: None,
                is_error: Some(false),
                result: Some(ToolResultView::Artifact { artifact }),
                output: None,
                timing: ToolTimingView::default(),
            }),
        };

        let rendered = format!("{:?}", transcript_item_body(&kind));
        assert!(rendered.contains("fixture.artifact"));
        assert!(rendered.contains("artifact-metadata"));
    }
}
