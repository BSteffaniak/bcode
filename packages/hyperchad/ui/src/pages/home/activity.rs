//! Active invocation and runtime-work presentation.

use std::collections::{BTreeMap, BTreeSet};

use bcode_session_view_models::SessionViewSnapshot;
use hyperchad::actions::logic::if_responsive;
use hyperchad::template::{Containers, container};
use hyperchad::transformer::models::LayoutDirection;

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
        section background="#161b22" border="1, #30363d" border-radius=10 padding=12 margin-bottom=18 {
            div #runtime-summary direction=(if_responsive("narrow").then(LayoutDirection::Column).or_else(LayoutDirection::Row)) gap=18 font-size=12 {
                div { span color="#8b949e" { "provider " } span color="#c9d1d9" { (provider) } }
                div { span color="#8b949e" { "model " } span color="#c9d1d9" { (model) } }
                div { span color="#8b949e" { "agent " } span color="#c9d1d9" { (agent) } }
                div { span color="#8b949e" { "turn " } span color="#c9d1d9" { (turn) } }
            }
            (runtime_usage(runtime))
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
        section background="#161b22" border="1, #30363d" border-radius=10 padding=16 margin-bottom=18 {
            h2 color="#f0f6fc" font-size=16 margin-bottom=14 { (heading) }
            @for (invocation_id, lifecycle) in active {
                @let item_id = semantic_dom_id("active-tool", invocation_id);
                div id=(item_id) background="#0d1117" border="1, #30363d" border-radius=6 padding=10 margin-bottom=8 {
                    div color="#f0f6fc" {
                        (lifecycle.message.as_deref().unwrap_or("Tool operation in progress"))
                    }
                    div color="#8b949e" font-size=11 margin-top=3 { (format!("{:?}", lifecycle.stage)) }
                    details margin-top=4 {
                        summary color="#8b949e" font-size=11 { "developer details" }
                        div color="#8b949e" font-size=11 margin-top=3 { (invocation_id) }
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
        section background="#161b22" border="1, #30363d" border-radius=10 padding=16 margin-bottom=18 {
            h2 color="#f0f6fc" font-size=16 margin-bottom=14 { "runtime work" }
            @for work in runtime_work {
                @let item_id = semantic_dom_id("runtime-work", &work.work_id.to_string());
                div id=(item_id) background="#0d1117" border="1, #30363d" border-radius=6 padding=10 margin-bottom=8 {
                    div color="#f0f6fc" {
                        (work.label) " · " (format!("{:?}", work.status))
                    }
                    div color="#8b949e" font-size=11 margin-top=3 { (format!("{:?}", work.kind)) }
                    details margin-top=4 {
                        summary color="#8b949e" font-size=11 { "developer details" }
                        div color="#8b949e" font-size=11 margin-top=3 { (work.work_id.to_string()) }
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
