use super::*;

#[test]
fn empty_snapshot_shows_reasoning_by_default() {
    assert!(SessionViewSnapshot::empty().thinking.visible);
}

#[test]
fn runtime_work_status_label_preserves_semantic_activity() {
    let running = |id: &str, kind, label: &str, message: Option<&str>| RuntimeWorkView {
        work_id: WorkId::new(id),
        kind,
        label: label.to_owned(),
        status: RuntimeWorkStatus::Running,
        cancellable: true,
        message: message.map(ToOwned::to_owned),
        completed_units: None,
        total_units: None,
        updated_at_ms: None,
    };
    let one = running("work-1", RuntimeWorkKind::Tool, "shell", Some("halfway"));
    assert_eq!(
        runtime_work_status_label(std::slice::from_ref(&one)).as_deref(),
        Some("running tool: shell — halfway")
    );
    let two = running("work-2", RuntimeWorkKind::Tool, "web", None);
    assert_eq!(
        runtime_work_status_label(&[one, two]).as_deref(),
        Some("running 2 tools")
    );
    let queued = RuntimeWorkView {
        work_id: WorkId::new("a-work"),
        kind: RuntimeWorkKind::ModelTurn,
        label: "queued turn".to_owned(),
        status: RuntimeWorkStatus::Queued,
        cancellable: false,
        message: None,
        completed_units: None,
        total_units: None,
        updated_at_ms: None,
    };
    let plugin = running("z-work", RuntimeWorkKind::PluginInvocation, "plugin", None);
    assert_eq!(
        runtime_work_status_label(&[plugin, queued]).as_deref(),
        Some("queued: queued turn")
    );

    let cancelling = RuntimeWorkView {
        status: RuntimeWorkStatus::Cancelling,
        ..running("work-3", RuntimeWorkKind::PluginInvocation, "plugin", None)
    };
    assert_eq!(
        runtime_work_status_label(&[cancelling]).as_deref(),
        Some("cancelling: plugin")
    );
}

#[test]
fn runtime_work_view_deserializes_legacy_shape() {
    let work: RuntimeWorkView = serde_json::from_value(serde_json::json!({
        "work_id": "legacy-work",
        "status": "running",
        "message": "legacy",
        "completed_units": null,
        "total_units": null,
        "updated_at_ms": null
    }))
    .expect("legacy runtime work view");

    assert_eq!(work.kind, RuntimeWorkKind::Tool);
    assert_eq!(work.label, "");
    assert!(!work.cancellable);
}

#[test]
fn permission_view_deserializes_legacy_shape() {
    let permission: PermissionView = serde_json::from_value(serde_json::json!({
        "permission_id": "permission-1",
        "tool_call_id": "call-1",
        "title": "Permission requested",
        "detail": null,
        "resolved": false,
        "approved": null,
        "can_remember": false
    }))
    .expect("legacy permission view");

    assert_eq!(permission.session_id, None);
    assert_eq!(permission.tool_name, "");
    assert_eq!(permission.batch, None);
    assert_eq!(permission.policy_source, None);
}
