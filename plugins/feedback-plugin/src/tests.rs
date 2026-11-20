use bcode_feedback_models::{
    Actor, FEEDBACK_SERVICE_INTERFACE_ID, Facet, FeedbackItem, FeedbackPage, FeedbackStatus,
    OP_GET, OP_LIST, OP_RECORD, OP_SET_STATUS, RecordFeedbackRequest, SetFeedbackStatusRequest,
};
use bcode_plugin_sdk::prelude::*;
use bcode_tool::{ToolInvocationRequest, ToolInvocationResponse};
use serde_json::{Value, json};

use crate::FeedbackPlugin;

fn context(
    interface_id: &str,
    operation: &str,
    payload: &impl serde::Serialize,
) -> NativeServiceContext {
    context_with_config(interface_id, operation, payload, Value::Null)
}

fn context_with_config(
    interface_id: &str,
    operation: &str,
    payload: &impl serde::Serialize,
    config: Value,
) -> NativeServiceContext {
    NativeServiceContext {
        plugin_id: crate::PLUGIN_ID.into(),
        request: ServiceRequest {
            interface_id: interface_id.into(),
            operation: operation.into(),
            payload: serde_json::to_vec(payload).unwrap(),
        },
        config: bcode_plugin_sdk::PluginConfigContext {
            config,
            ..Default::default()
        },
        events: bcode_plugin_sdk::ServiceEventEmitter::default(),
        cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
        bridge: bcode_plugin_sdk::ServiceBridge::default(),
        transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
    }
}

fn service<T: serde::de::DeserializeOwned>(
    plugin: &mut FeedbackPlugin,
    operation: &str,
    payload: &impl serde::Serialize,
) -> Result<T, String> {
    let response =
        plugin.invoke_service(context(FEEDBACK_SERVICE_INTERFACE_ID, operation, payload));
    match response.error {
        Some(error) => Err(error.code),
        None => Ok(serde_json::from_slice(&response.payload).unwrap()),
    }
}

fn tool(plugin: &mut FeedbackPlugin, name: &str, arguments: Value) -> ToolInvocationResponse {
    tool_with_config(plugin, name, arguments, Value::Null)
}

fn tool_with_config(
    plugin: &mut FeedbackPlugin,
    name: &str,
    arguments: Value,
    config: Value,
) -> ToolInvocationResponse {
    let response = plugin.invoke_service(context_with_config(
        bcode_tool::TOOL_SERVICE_INTERFACE_ID,
        bcode_tool::OP_INVOKE_TOOL,
        &ToolInvocationRequest {
            tool_call_id: "call".into(),
            name: name.into(),
            arguments,
            preparation_descriptor: Value::Null,
        },
        config,
    ));
    assert!(response.error.is_none(), "tool transport failed");
    serde_json::from_slice(&response.payload).unwrap()
}

fn plugin() -> (tempfile::TempDir, FeedbackPlugin) {
    let dir = tempfile::tempdir().unwrap();
    let plugin = FeedbackPlugin::with_state_dir(dir.path().to_owned());
    (dir, plugin)
}

fn record_sensitive(plugin: &mut FeedbackPlugin) -> FeedbackItem {
    service(
        plugin,
        OP_RECORD,
        &RecordFeedbackRequest {
            app: "bdata".into(),
            text: "Copy failed with an error".into(),
            facets: vec![
                Facet::new("bdata.dialect", "postgres"),
                Facet {
                    key: "bdata.connection".into(),
                    value: "postgres://user:hunter2@db".into(),
                    sensitive: true,
                },
            ],
        },
    )
    .unwrap()
}

#[test]
fn clients_see_sensitive_values_but_tools_never_do() {
    let (_dir, mut plugin) = plugin();
    let item = record_sensitive(&mut plugin);

    let read: FeedbackItem = service(&mut plugin, OP_GET, &json!({"id": item.id})).unwrap();
    assert!(
        read.facets
            .iter()
            .any(|facet| facet.value.contains("hunter2"))
    );

    for (name, arguments) in [
        ("feedback.get", json!({"id": item.id.to_string()})),
        ("feedback.search", json!({"app": "bdata"})),
        ("feedback.search", json!({"text": "copy failed"})),
    ] {
        let result = tool(&mut plugin, name, arguments);
        assert!(!result.is_error, "{name}: {}", result.output);
        assert!(
            !result.output.contains("hunter2"),
            "{name} leaked a sensitive value"
        );
        assert!(
            result.output.contains("postgres"),
            "{name} dropped a public facet"
        );
    }
}

#[test]
fn agent_tools_triage_and_resolve_but_cannot_verify() {
    let (_dir, mut plugin) = plugin();
    let item = record_sensitive(&mut plugin);
    let id = item.id.to_string();

    let triaged = tool(
        &mut plugin,
        "feedback.triage",
        json!({"id": id, "title": "Copy fails for postgres results", "category": "bug",
               "severity": "high", "tags": ["clipboard"]}),
    );
    assert!(!triaged.is_error, "{}", triaged.output);

    let resolved = tool(
        &mut plugin,
        "feedback.set_status",
        json!({"id": id, "status": "agent_resolved", "note": "Handled the error", "commits": ["abc123"]}),
    );
    assert!(!resolved.is_error, "{}", resolved.output);

    let verify = tool(
        &mut plugin,
        "feedback.set_status",
        json!({"id": id, "status": "verified"}),
    );
    assert!(verify.is_error, "agents must not verify");

    let page: FeedbackPage = service(&mut plugin, OP_LIST, &json!({"limit": 10})).unwrap();
    let stored = &page.items[0];
    assert_eq!(stored.status, FeedbackStatus::AgentResolved);
    assert_eq!(stored.status_change.as_ref().unwrap().by, Actor::Agent);
    assert_eq!(
        stored.triage.as_ref().unwrap().title,
        "Copy fails for postgres results"
    );
}

#[test]
fn service_status_changes_act_for_the_person_whatever_the_payload_says() {
    let (_dir, mut plugin) = plugin();
    let item = record_sensitive(&mut plugin);
    let verified: FeedbackItem = service(
        &mut plugin,
        OP_SET_STATUS,
        &SetFeedbackStatusRequest {
            id: item.id,
            status: FeedbackStatus::Verified,
            actor: Actor::Agent,
            note: None,
            session_id: None,
            commits: Vec::new(),
        },
    )
    .unwrap();
    assert_eq!(verified.status, FeedbackStatus::Verified);
    assert_eq!(verified.status_change.unwrap().by, Actor::User);
}

#[test]
fn configuration_can_withhold_agent_resolution() {
    let (_dir, mut plugin) = plugin();
    let item = record_sensitive(&mut plugin);
    let result = tool_with_config(
        &mut plugin,
        "feedback.set_status",
        json!({"id": item.id.to_string(), "status": "agent_resolved"}),
        json!({"agent_resolution": "disabled"}),
    );
    assert!(result.is_error);
}

#[test]
fn invalid_tool_arguments_are_reported_to_the_model() {
    let (_dir, mut plugin) = plugin();
    let result = tool(
        &mut plugin,
        "feedback.get",
        json!({"id": "fb-1", "extra": true}),
    );
    assert!(result.is_error);
    let result = tool(
        &mut plugin,
        "feedback.record",
        json!({"app": "Not Valid", "text": "x"}),
    );
    assert!(result.is_error);
}

#[test]
fn every_tool_schema_is_closed() {
    for definition in crate::tools::definitions() {
        assert_eq!(
            definition.input_schema["additionalProperties"], false,
            "{}",
            definition.name
        );
    }
}
