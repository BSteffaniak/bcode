//! Shared model/backend picker behavior.

use super::*;

#[test]
fn open_intent_preserves_workspace_identity_and_never_inherits_history() {
    let instance = instance();
    let session_id = SessionId::new();
    let cwd = tempfile::tempdir().unwrap();
    let call =
        managed_start_call(instance.clone(), session_id, "open-1".into(), cwd.path()).unwrap();
    assert_eq!(call.session_id, Some(session_id));
    assert_eq!(call.parent_session_id, None);
    assert_eq!(call.assistance_destination_session_id, None);
    assert_eq!(call.harness.instance, instance);
    let bcode_harness_models::HarnessRequestKind::Open(open) = call.harness.kind else {
        panic!("expected open")
    };
    assert_eq!(open.session_id, session_id.to_string());
    assert_eq!(open.open_id, "open-1");
    assert_eq!(
        open.working_directory,
        cwd.path().canonicalize().unwrap().to_str().unwrap()
    );
    assert!(open.resume.is_none());
}

fn instance() -> HarnessInstance {
    serde_json::from_value(serde_json::json!({
        "instance_id":"claude", "plugin_id":"bcode.claude-code", "executable":"/opt/claude",
        "config_namespace":"/native/config", "auth":{"provider_id":"claude-code","profile":"native",
        "credential_namespace":"/native/config","custody":"native_managed"}
    }))
    .unwrap()
}

#[test]
fn open_refuses_unavailable_workspace_instead_of_substituting_current_directory() {
    let cwd = tempfile::tempdir().unwrap();
    assert!(
        managed_start_call(
            instance(),
            SessionId::new(),
            "open-1".into(),
            &cwd.path().join("missing")
        )
        .is_err()
    );
}

#[tokio::test]
async fn picker_exit_survives_teardown_and_retains_return_context() {
    let request = ExecutionPickerRequest {
        session_id: Some(SessionId::new()),
        draft: "unsent input".into(),
        working_directory: "/workspace".into(),
        current: crate::model_switcher::inventory::CurrentChoice::default(),
        current_effort: None,
        pending: None,
    };
    let target = crate::runtime::ManagedJourneyTarget {
        session_id: request.session_id.unwrap(),
        instance_id: "claude",
        generation: 1,
    };
    let mut budget = crate::runtime::ManagedReentryBudget::default();
    let step = crate::runtime::next_managed_step(
        crate::runtime::ManagedChatExit::ExecutionPicker(request.clone()),
        &target,
        &mut budget,
    )
    .await;
    let crate::runtime::ManagedJourneyStep::End(
        crate::runtime::ManagedJourneyEnd::ExecutionPicker(returned),
    ) = step
    else {
        panic!("expected picker handoff")
    };
    assert_eq!(returned, request);
}
