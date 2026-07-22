//! Home page for the Bcode web renderer.

use bcode_plugin_sdk::path::display_from_current_dir;
use std::collections::BTreeMap;
use std::sync::LazyLock;

use bcode_session_models::SessionSummary;
use bcode_session_view_models::{
    InteractionViewSummary, PermissionView, PluginVisualView, SessionViewSnapshot,
    ToolInvocationViewStatus, TranscriptViewItemKind,
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

static VISUAL_ADAPTERS: LazyLock<BTreeMap<(&'static str, u32), VisualAdapter>> =
    LazyLock::new(|| {
        let structured = render_structured_visual as VisualAdapter;
        BTreeMap::from([
            (
                ("bcode.tool.request.shell.run", 1),
                render_shell_request as VisualAdapter,
            ),
            (("bcode.filesystem.request", 1), structured),
            (("bcode.filesystem.change", 1), structured),
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
            (("bcode.web-search.search_request", 1), structured),
            (("bcode.web-search.fetch_request", 1), structured),
            (("bcode.web-search.status_request", 1), structured),
            (("bcode.web-search.inspect_request", 1), structured),
            (("bcode.git.clone_request", 1), structured),
            (("bcode.git.clone_result", 1), structured),
            (("bcode.worktree.request", 1), structured),
            (("bcode.worktree.list", 1), structured),
            (("bcode.worktree.create_result", 1), structured),
            (("bcode.worktree.remove_result", 1), structured),
            (("bcode.vim-edit.request.preview", 1), structured),
            (("bcode.vim-edit.request.apply", 1), structured),
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
            render_plugin_visual("tool contribution", &visual)
        }
    }
}

fn render_tool_result(result: &bcode_session_view_models::ToolResultView) -> Containers {
    if let bcode_session_view_models::ToolResultView::Artifact { artifact } = result {
        let visual = PluginVisualView::from(bcode_session_models::PluginVisualDescriptor {
            visual_id: Some(artifact.artifact.artifact_id.clone()),
            producer_plugin_id: Some(artifact.artifact.producer_plugin_id.clone()),
            schema: artifact.artifact.schema.clone(),
            schema_version: artifact.artifact.schema_version,
            title: artifact.artifact.title.clone(),
            subtitle: None,
            payload: artifact.artifact.metadata.clone(),
        });
        render_plugin_visual("semantic result", &visual)
    } else {
        json_panel(
            "semantic result",
            &serde_json::to_value(result).unwrap_or(serde_json::Value::Null),
        )
    }
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
        TranscriptViewItemKind::Permission { .. } => "permission",
        TranscriptViewItemKind::RuntimeWork { .. } => "runtime work",
        TranscriptViewItemKind::Usage { .. } => "usage",
        TranscriptViewItemKind::Interaction { .. } => "interaction",
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
        ChatMessageView, PermissionBatchView, PermissionView, RuntimeWorkView, ToolArtifactView,
        ToolInvocationView, ToolResultView, ToolTimingView, TranscriptViewItem,
        TranscriptViewItemId,
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
            "bcode.git.clone_result",
            "bcode.worktree.request",
            "bcode.worktree.list",
            "bcode.worktree.create_result",
            "bcode.worktree.remove_result",
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
    fn unknown_contribution_keeps_complete_opaque_envelope_in_render_tree() {
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
        assert!(rendered.contains("future.unknown/schema"));
        assert!(rendered.contains("opaque-web"));
        assert!(rendered.contains("append"));
    }

    #[test]
    fn git_contribution_renders_through_schema_adapter_and_keeps_fallback() {
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
        assert!(rendered.contains("bcode.git.clone_request"));
    }

    #[test]
    fn shell_contribution_keeps_shared_payload_in_web_render_tree() {
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
        assert!(rendered.contains("bcode.shell.run.summary"));
        assert!(rendered.contains("shell-render-sentinel"));
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
