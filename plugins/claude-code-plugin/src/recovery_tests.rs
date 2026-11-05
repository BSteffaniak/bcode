//! Resume validation is pure: nonexistent selected executables cannot be launched here.
use super::{
    HARNESS_SCHEMA_VERSION, HarnessControlRequest, HarnessFailure, HarnessInstance,
    HarnessOpenRequest, InstanceConfig, PLUGIN_ID, validate_release, validate_resume,
    validate_resume_selection,
};
use bcode_harness_models::HarnessSessionTarget;

fn fixture() -> (HarnessInstance, HarnessOpenRequest, InstanceConfig) {
    let instance = serde_json::from_value(serde_json::json!({
        "instance_id":"primary", "plugin_id":PLUGIN_ID,
        "executable":"/nonexistent/recovery-claude", "config_namespace":"/nonexistent/recovery-config",
        "auth":{"provider_id":"claude-code","profile":"native",
            "credential_namespace":"/nonexistent/recovery-config","custody":"native_managed"}
    })).unwrap();
    let open = serde_json::from_value(serde_json::json!({
        "open_id":"recovery-open", "session_id":"00000000-0000-4000-8000-000000000001",
        "generation":2,"working_directory":"/nonexistent/recovery-workspace",
        "resume":{"schema_version":HARNESS_SCHEMA_VERSION,
            "session_id":"00000000-0000-4000-8000-000000000001",
            "instance_id":"primary","native_session_id":"00000000-0000-4000-8000-000000000002",
            "native_version":"verified-exact","generation":2}
    }))
    .unwrap();
    let selected = serde_json::from_value(serde_json::json!({
        "executable":"/nonexistent/recovery-claude", "config_directory":"/nonexistent/recovery-config",
        "exact_version":"verified-exact"
    })).unwrap();
    (instance, open, selected)
}

#[test]
fn release_requires_exact_retained_close_after_reap() {
    let (_, open, _) = fixture();
    let control = HarnessControlRequest {
        target: HarnessSessionTarget::Confirmed(open.resume.unwrap()),
        mutation_id: "close-exact".into(),
    };
    assert!(validate_release(false, Some(&control), &control).is_err());
    assert!(validate_release(true, None, &control).is_err());
    assert!(validate_release(true, Some(&control), &control).is_ok());
    // Duplicate evidence lookup is read-only and stable, not a second Close.
    assert!(validate_release(true, Some(&control), &control).is_ok());
    let mut changed = control.clone();
    changed.mutation_id = "other-close".into();
    assert!(validate_release(true, Some(&control), &changed).is_err());
    changed = control.clone();
    let HarnessSessionTarget::Confirmed(binding) = &mut changed.target else {
        panic!()
    };
    binding.generation += 1;
    assert!(validate_release(true, Some(&control), &changed).is_err());
}

#[test]
fn exact_next_generation_selection_does_not_certify_inert_resume() {
    let (instance, open, selected) = fixture();
    assert!(validate_resume_selection(&instance, &open, &selected).is_ok());
    assert_eq!(
        validate_resume(&instance, &open, &selected),
        Err(HarnessFailure::Unsupported)
    );
    assert_eq!(open.generation, 2);
    assert_eq!(open.resume.as_ref().unwrap().generation, 2);
}

#[test]
fn resume_preflight_rejects_old_generation_and_changed_selection() {
    let (mut instance, mut open, selected) = fixture();
    open.resume.as_mut().unwrap().generation = 1;
    assert!(validate_resume_selection(&instance, &open, &selected).is_err());
    open.resume.as_mut().unwrap().generation = 2;
    open.resume.as_mut().unwrap().native_version = "other".into();
    assert!(validate_resume_selection(&instance, &open, &selected).is_err());
    open.resume.as_mut().unwrap().native_version = "verified-exact".into();
    instance.executable = "/other".into();
    assert!(validate_resume_selection(&instance, &open, &selected).is_err());
    instance.executable = "/nonexistent/recovery-claude".into();
    instance.config_namespace = "/other-config".into();
    assert!(validate_resume_selection(&instance, &open, &selected).is_err());
}
