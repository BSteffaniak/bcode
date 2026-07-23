//! Permission request and resolution presentation.

use bcode_session_view_models::PermissionView;
use hyperchad::template::{Containers, container};

pub(super) fn permission_history(permission: &PermissionView) -> Containers {
    let outcome = match permission.approved {
        Some(true) => "approved",
        Some(false) => "denied",
        None if permission.resolved => "resolved",
        None => "requested",
    };
    let color = match permission.approved {
        Some(true) => "#7ee787",
        Some(false) => "#f85149",
        None => "#8b949e",
    };
    container! {
        aside border="1, #30363d" border-radius=8 padding=10 {
            div justify-content=space-between gap=12 {
                div color="#f0f6fc" { (permission.tool_name) }
                div color=(color) font-size=12 { (outcome) }
            }
            @if let Some(detail) = &permission.detail {
                div color="#8b949e" font-size=12 margin-top=6 { (detail) }
            }
        }
    }
}

pub(super) fn permission_request(
    permission: &PermissionView,
    session_id: Option<bcode_session_models::SessionId>,
    access_token: &str,
) -> Containers {
    let has_batch_actions = permission
        .batch
        .as_ref()
        .is_some_and(|batch| batch.call_count > 1);
    container! {
        div border="1, #f2cc60" border-radius=8 padding=12 margin-bottom=10 {
            div justify-content=space-between gap=12 margin-bottom=6 {
                div {
                    div color="#f2cc60" { (permission.title.as_deref().unwrap_or("Permission requested")) }
                    div color="#f0f6fc" font-size=12 margin-top=3 { (permission.tool_name) }
                }
                @if let Some(batch) = &permission.batch {
                    div color="#8b949e" font-size=12 {
                        "call " (batch.call_index.saturating_add(1).to_string()) " of " (batch.call_count.to_string())
                    }
                }
            }
            @if let Some(detail) = &permission.detail {
                div color="#c9d1d9" { (detail) }
            } @else {
                div color="#8b949e" { "No additional details provided." }
            }
            div color="#8b949e" font-size=11 margin-top=6 {
                @if !permission.agent_id.is_empty() { "agent " (permission.agent_id) }
                @if let Some(policy_source) = &permission.policy_source { " · policy " (policy_source) }
                @if permission.can_remember { " · decision can be remembered" }
            }
            @if permission.resolved {
                (permission_history(permission))
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
                @if has_batch_actions {
                    @if let Some(batch) = &permission.batch {
                        div direction=row gap=8 margin-top=8 {
                            form hx-post=(format!("/actions/permission-batch?token={access_token}")) hx-target="#bcode-web-shell" hx-swap=this {
                                input type=hidden name="session_id" value=(session_id.to_string());
                                input type=hidden name="batch_id" value=(batch.batch_id.clone());
                                input type=hidden name="approved" value="true";
                                button type=submit background="#238636" color=white border-radius=6 padding="6, 12" { "approve all " (batch.call_count.to_string()) }
                            }
                            form hx-post=(format!("/actions/permission-batch?token={access_token}")) hx-target="#bcode-web-shell" hx-swap=this {
                                input type=hidden name="session_id" value=(session_id.to_string());
                                input type=hidden name="batch_id" value=(batch.batch_id.clone());
                                input type=hidden name="approved" value="false";
                                button type=submit background="#da3633" color=white border-radius=6 padding="6, 12" { "deny all " (batch.call_count.to_string()) }
                            }
                        }
                    }
                }
            }
        }
    }
}
