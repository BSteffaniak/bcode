//! Home page for the Bcode web renderer.

use bcode_session_models::SessionSummary;
use bcode_session_view_models::{
    PermissionView, SessionViewSnapshot, ToolInvocationViewStatus, TranscriptViewItemKind,
};
use hyperchad::template::{Containers, container};

/// Render the Bcode web renderer shell.
#[must_use]
pub fn home(snapshot: &SessionViewSnapshot, sessions: &[SessionSummary]) -> Containers {
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
        "connected"
    } else {
        "ready"
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
                            anchor href=(format!("/session/{}", session.id)) text-decoration="none" {
                                div color="#f0f6fc" font-size=13 { (session.title().unwrap_or("Untitled session")) }
                                div color="#8b949e" font-size=11 { (session.working_directory.display().to_string()) }
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
                                (snapshot.working_directory.as_ref().map_or_else(|| "—".to_string(), |path| path.display().to_string()))
                            }
                        }
                    }

                    section background="#161b22" border="1, #30363d" border-radius=10 padding=16 margin-bottom=18 {
                        h2 color="#f0f6fc" font-size=16 margin-bottom=14 { "transcript" }
                        @if snapshot.transcript.items.is_empty() {
                            div color="#8b949e" font-size=13 { "Attach or create a session to begin." }
                        } @else {
                            @for item in &snapshot.transcript.items {
                                (transcript_item(item))
                            }
                        }
                    }

                    @if !snapshot.permissions.is_empty() {
                        section background="#161b22" border="1, #30363d" border-radius=10 padding=16 margin-bottom=18 {
                            h2 color="#f0f6fc" font-size=16 margin-bottom=14 { "permissions" }
                            @for permission in &snapshot.permissions {
                                (permission_request(permission, snapshot.session_id))
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
                        (composer(snapshot))
                    }
                }
            }
        }
    }
}

fn composer(snapshot: &SessionViewSnapshot) -> Containers {
    let action = "/actions/submit-message";
    container! {
        div {
            form hx-post=(action) hx-target="#bcode-web-shell" hx-swap=this background="#0d1117" border="1, #30363d" border-radius=8 padding=12 {
                @if let Some(session_id) = snapshot.session_id {
                    input type=hidden name="session_id" value=(session_id.to_string());
                }
                input type=hidden name="placement" value="steering";
                textarea name="text" rows="5" placeholder="Send a message to this session" width=100% padding=10 border="1, #30363d" border-radius=6 background="#010409" color="#c9d1d9" {
                    (snapshot.composer.draft)
                }
                button type=submit background="#238636" color=white border-radius=6 padding="8, 14" margin-top=10 {
                    "send"
                }
            }
            @if let Some(session_id) = snapshot.session_id {
                form hx-post="/actions/cancel-turn" hx-target="#bcode-web-shell" hx-swap=this margin-top=10 {
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

fn permission_request(
    permission: &PermissionView,
    session_id: Option<bcode_session_models::SessionId>,
) -> Containers {
    container! {
        div border="1, #f2cc60" border-radius=8 padding=10 margin-bottom=10 {
            div color="#f2cc60" margin-bottom=6 { (permission.title.as_deref().unwrap_or("Permission requested")) }
            div color="#c9d1d9" { (permission.detail.as_deref().unwrap_or("No details provided.")) }
            @if permission.resolved {
                div color="#8b949e" font-size=12 margin-top=8 {
                    "resolved: " (if permission.approved.unwrap_or(false) { "approved" } else { "denied" })
                }
            } @else if let Some(session_id) = session_id {
                div direction=row gap=8 margin-top=10 {
                    form hx-post="/actions/permission" hx-target="#bcode-web-shell" hx-swap=this {
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
                    form hx-post="/actions/permission" hx-target="#bcode-web-shell" hx-swap=this {
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
            }
        }
    }
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
                    (json_panel("request visual", &visual.generic_payload))
                }
                @if let Some(result) = &tool.result {
                    (json_panel("semantic result", &serde_json::to_value(result).unwrap_or(serde_json::Value::Null)))
                }
            }
        },
        TranscriptViewItemKind::Permission { permission } => container! {
            div border="1, #f2cc60" border-radius=8 padding=10 {
                div color="#f2cc60" margin-bottom=6 { (permission.title.as_deref().unwrap_or("Permission requested")) }
                div color="#c9d1d9" { (permission.detail.as_deref().unwrap_or("No details provided.")) }
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
            json_panel("plugin visual", &visual.generic_payload)
        }
    }
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
        TranscriptViewItemKind::Interaction { .. } => "interaction",
        TranscriptViewItemKind::SystemMessage { .. } => "system",
        TranscriptViewItemKind::PluginVisual { .. } => "plugin visual",
    }
}

const fn tool_status_color(status: ToolInvocationViewStatus) -> &'static str {
    match status {
        ToolInvocationViewStatus::Requested => "#8b949e",
        ToolInvocationViewStatus::Running => "#7ee787",
        ToolInvocationViewStatus::Finished => "#58a6ff",
    }
}
