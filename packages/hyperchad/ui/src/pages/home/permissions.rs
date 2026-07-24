//! Permission request and resolution presentation.

use super::theme::{accent, color, radius, space, typeface};
use crate::context::{PresentationAction, PresentationContext};
use bcode_session_view_models::PermissionView;
use hyperchad::template::{Containers, container};

use super::components::{StatusTone, permission_card};
use super::semantic_dom_id;

pub(super) fn permission_history(permission: &PermissionView) -> Containers {
    let outcome = match permission.approved {
        Some(true) => "approved",
        Some(false) => "denied",
        None if permission.resolved => "resolved",
        None => "requested",
    };
    let tone = match permission.approved {
        Some(true) => StatusTone::Success,
        Some(false) => StatusTone::Error,
        None => StatusTone::Neutral,
    };
    let content = container! {
        @if let Some(detail) = &permission.detail {
            div color=(color::MUTED) font-size=((typeface::LABEL)) { (detail) }
        }
    };
    permission_card(&permission.tool_name, outcome, tone, &content, false)
}

pub(super) fn permission_request(
    permission: &PermissionView,
    session_id: Option<bcode_session_models::SessionId>,
    context: &impl PresentationContext,
) -> Containers {
    let has_batch_actions = permission
        .batch
        .as_ref()
        .is_some_and(|batch| batch.call_count > 1);
    let item_id = semantic_dom_id("permission", &permission.permission_id);
    container! {
        div id=(item_id) border=((1, color::WARNING)) border-radius=((radius::CARD)) padding=((space::MD)) margin-bottom=((space::S10)) {
            div justify-content=space-between gap=((space::MD)) margin-bottom=((space::S6)) {
                div {
                    div color=(color::WARNING) { (permission.title.as_deref().unwrap_or("Permission requested")) }
                    div color=(color::STRONG) font-size=((typeface::LABEL)) margin-top=((space::S3)) { (permission.tool_name) }
                }
                @if let Some(batch) = &permission.batch {
                    div color=(color::MUTED) font-size=((typeface::LABEL)) {
                        "call " (batch.call_index.saturating_add(1).to_string()) " of " (batch.call_count.to_string())
                    }
                }
            }
            @if let Some(detail) = &permission.detail {
                div color=(color::TEXT) { (detail) }
            } @else {
                div color=(color::MUTED) { "No additional details provided." }
            }
            div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::S6)) {
                @if !permission.agent_id.is_empty() { "agent " (permission.agent_id) }
                @if let Some(policy_source) = &permission.policy_source { " · policy " (policy_source) }
                @if permission.can_remember { " · decision can be remembered" }
            }
            @if permission.resolved {
                (permission_history(permission))
            } @else if let Some(session_id) = session_id {
                div direction=row gap=((space::SM)) margin-top=((space::S10)) {
                    form hx-post=(context.action_target(PresentationAction::ResolvePermission)) hx-target="#bcode-web-shell" hx-swap=this {
                        input type=hidden name="session_id" value=(session_id.to_string());
                        input type=hidden name="permission_id" value=(permission.permission_id.clone());
                        input type=hidden name="approved" value="true";
                        @if permission.can_remember {
                            span color=(color::MUTED) font-size=((typeface::DETAIL)) margin-right=8 {
                                input type=checkbox name="remember" value="true";
                                " remember"
                            }
                        }
                        button type=submit background=(accent::POSITIVE) color=(color::ON_ACCENT) border-radius=((radius::CONTROL)) padding="10, 14" { "approve" }
                    }
                    form hx-post=(context.action_target(PresentationAction::ResolvePermission)) hx-target="#bcode-web-shell" hx-swap=this {
                        input type=hidden name="session_id" value=(session_id.to_string());
                        input type=hidden name="permission_id" value=(permission.permission_id.clone());
                        input type=hidden name="approved" value="false";
                        @if permission.can_remember {
                            span color=(color::MUTED) font-size=((typeface::DETAIL)) margin-right=8 {
                                input type=checkbox name="remember" value="true";
                                " remember"
                            }
                        }
                        button type=submit background=(accent::DESTRUCTIVE) color=(color::ON_ACCENT) border-radius=((radius::CONTROL)) padding="10, 14" { "deny" }
                    }
                }
                @if has_batch_actions {
                    @if let Some(batch) = &permission.batch {
                        div direction=row gap=((space::SM)) margin-top=((space::SM)) {
                            form hx-post=(context.action_target(PresentationAction::ResolvePermissionBatch)) hx-target="#bcode-web-shell" hx-swap=this {
                                input type=hidden name="session_id" value=(session_id.to_string());
                                input type=hidden name="batch_id" value=(batch.batch_id.clone());
                                input type=hidden name="approved" value="true";
                                button type=submit background=(accent::POSITIVE) color=(color::ON_ACCENT) border-radius=((radius::CONTROL)) padding="10, 14" { "approve all " (batch.call_count.to_string()) }
                            }
                            form hx-post=(context.action_target(PresentationAction::ResolvePermissionBatch)) hx-target="#bcode-web-shell" hx-swap=this {
                                input type=hidden name="session_id" value=(session_id.to_string());
                                input type=hidden name="batch_id" value=(batch.batch_id.clone());
                                input type=hidden name="approved" value="false";
                                button type=submit background=(accent::DESTRUCTIVE) color=(color::ON_ACCENT) border-radius=((radius::CONTROL)) padding="10, 14" { "deny all " (batch.call_count.to_string()) }
                            }
                        }
                    }
                }
            }
        }
    }
}
