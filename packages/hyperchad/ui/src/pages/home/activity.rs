//! Active invocation and runtime-work presentation.

use std::collections::{BTreeMap, BTreeSet};

use super::theme::{color, radius, space, surface, typeface};
use bcode_session_view_models::SessionViewSnapshot;
use hyperchad::actions::logic::if_responsive;
use hyperchad::template::{Containers, container};
use hyperchad::transformer::models::LayoutDirection;

use super::components::progress_status;
use super::semantic_dom_id;
use super::usage::runtime_usage;

pub(super) fn runtime_state_section(snapshot: &SessionViewSnapshot) -> Containers {
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
    container! {
        section background=(surface::PANEL) border=((1, surface::BORDER)) border-radius=((radius::PANEL)) padding=((space::MD)) margin-bottom=((space::S18)) {
            div #runtime-summary direction=(if_responsive("narrow").then(LayoutDirection::Column).or_else(LayoutDirection::Row)) gap=((space::S18)) font-size=((typeface::LABEL)) {
                div { span color=(color::MUTED) { "provider " } span color=(color::TEXT) { (provider) } }
                div { span color=(color::MUTED) { "model " } span color=(color::TEXT) { (model) } }
                div { span color=(color::MUTED) { "agent " } span color=(color::TEXT) { (agent) } }
                div { span color=(color::MUTED) { "turn " } span color=(color::TEXT) { (turn) } }
            }
            (runtime_usage(runtime))
            @if let Some(progress) = &runtime.provider_progress {
                (progress_status(&progress.detail, None, None))
            }
        }
    }
}

pub(super) fn unrepresented_active_invocations(
    snapshot: &SessionViewSnapshot,
) -> BTreeMap<String, bcode_session_models::ToolInvocationLifecycleEvent> {
    let represented = snapshot
        .transcript
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { tool }
            | bcode_session_view_models::TranscriptViewItemKind::ToolRequest { tool } => {
                Some(tool.tool_call_id.as_str())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    snapshot
        .active_invocations
        .iter()
        .filter(|(invocation_id, _)| !represented.contains(invocation_id.as_str()))
        .map(|(invocation_id, lifecycle)| (invocation_id.clone(), lifecycle.clone()))
        .collect()
}

pub(super) fn unrepresented_runtime_work(
    snapshot: &SessionViewSnapshot,
) -> Vec<bcode_session_view_models::RuntimeWorkView> {
    let represented = snapshot
        .transcript
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            bcode_session_view_models::TranscriptViewItemKind::RuntimeWork { work } => {
                Some(&work.work_id)
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    snapshot
        .runtime_work
        .iter()
        .filter(|work| !represented.contains(&work.work_id))
        .cloned()
        .collect()
}

pub(super) fn active_invocations_section(
    active: &BTreeMap<String, bcode_session_models::ToolInvocationLifecycleEvent>,
) -> Containers {
    let heading = if active.len() == 1 {
        "active tool"
    } else {
        "active invocations"
    };
    container! {
        section background=(surface::PANEL) border=((1, surface::BORDER)) border-radius=((radius::PANEL)) padding=((space::LG)) margin-bottom=((space::S18)) {
            h2 color=(color::STRONG) font-size=((typeface::SECTION)) margin-bottom=((space::S14)) { (heading) }
            @for (invocation_id, lifecycle) in active {
                @let item_id = semantic_dom_id("active-tool", invocation_id);
                div id=(item_id) background=(surface::APP) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) padding=((space::S10)) margin-bottom=((space::SM)) {
                    div color=(color::STRONG) {
                        (lifecycle.message.as_deref().unwrap_or("Tool operation in progress"))
                    }
                    div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::S3)) { (format!("{:?}", lifecycle.stage)) }
                    details margin-top=((space::XS)) {
                        summary color=(color::MUTED) font-size=((typeface::DETAIL)) { "developer details" }
                        div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::S3)) { (invocation_id) }
                    }
                }
            }
        }
    }
}

pub(super) fn runtime_work_section(
    runtime_work: &[bcode_session_view_models::RuntimeWorkView],
) -> Containers {
    container! {
        section background=(surface::PANEL) border=((1, surface::BORDER)) border-radius=((radius::PANEL)) padding=((space::LG)) margin-bottom=((space::S18)) {
            h2 color=(color::STRONG) font-size=((typeface::SECTION)) margin-bottom=((space::S14)) { "runtime work" }
            @for work in runtime_work {
                @let item_id = semantic_dom_id("runtime-work", &work.work_id.to_string());
                div id=(item_id) background=(surface::APP) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) padding=((space::S10)) margin-bottom=((space::SM)) {
                    div color=(color::STRONG) {
                        (work.label) " · " (format!("{:?}", work.status))
                    }
                    div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::S3)) { (format!("{:?}", work.kind)) }
                    details margin-top=((space::XS)) {
                        summary color=(color::MUTED) font-size=((typeface::DETAIL)) { "developer details" }
                        div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::S3)) { (work.work_id.to_string()) }
                    }
                    @if let Some(message) = &work.message {
                        div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { (message) }
                    }
                    @if work.completed_units.is_some() || work.total_units.is_some() {
                        (progress_status("Progress", work.completed_units, work.total_units))
                    }
                }
            }
        }
    }
}
