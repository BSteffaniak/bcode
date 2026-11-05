use super::*;
use crate::harness::HarnessCommand;
use clap::Parser;

#[derive(Parser)]
struct CommandLine {
    #[command(subcommand)]
    command: HarnessCommand,
}

fn options(args: &[&str]) -> OpenOptions {
    let HarnessCommand::Open { options } = CommandLine::try_parse_from(args).unwrap().command
    else {
        panic!("expected configured open");
    };
    options
}

fn instance() -> HarnessInstance {
    serde_json::from_value(serde_json::json!({
        "instance_id": "primary", "plugin_id": "example.native",
        "executable": "/opt/native", "config_namespace": "/config/native",
        "auth": {"provider_id": "example", "profile": "native", "credential_namespace": "/config/native", "custody": "native_managed"}
    })).unwrap()
}

#[test]
fn selection_is_explicit_and_exclusive() {
    for args in [
        vec!["test", "open"],
        vec![
            "test",
            "open",
            "--instance",
            "primary",
            "--default-instance",
        ],
        vec!["test", "open", "--api-key", "secret"],
        vec!["test", "open", "--name", "unsupported"],
    ] {
        assert!(CommandLine::try_parse_from(args).is_err());
    }
    assert_eq!(
        options(&["test", "open", "--default-instance"])
            .selection()
            .unwrap(),
        Selection::Default {}
    );
    assert_eq!(
        options(&["test", "open", "--instance", "primary"])
            .selection()
            .unwrap(),
        Selection::Instance {
            instance_id: "primary".into()
        }
    );
    assert!(
        options(&["test", "open", "--instance", " "])
            .selection()
            .is_err()
    );
}

#[test]
fn retry_arguments_require_both_retained_identities() {
    let session = SessionId::new().to_string();
    for args in [
        vec![
            "test",
            "open",
            "--default-instance",
            "--session-id",
            session.as_str(),
        ],
        vec![
            "test",
            "open",
            "--default-instance",
            "--open-id",
            "retained",
        ],
    ] {
        assert!(CommandLine::try_parse_from(args).is_err());
    }
    let options = options(&[
        "test",
        "open",
        "--default-instance",
        "--session-id",
        &session,
        "--open-id",
        "retained",
    ]);
    assert_eq!(options.session_id.unwrap().to_string(), session);
    assert_eq!(options.open_id.as_deref(), Some("retained"));
}

#[test]
fn existing_start_and_catalog_alias_parse_without_contract_changes() {
    assert!(matches!(
        CommandLine::try_parse_from(["test", "start", "--request", "retained.json"])
            .unwrap()
            .command,
        HarnessCommand::Start { .. }
    ));
    for verb in ["list", "catalog"] {
        assert!(matches!(
            CommandLine::try_parse_from(["test", verb]).unwrap().command,
            HarnessCommand::List
        ));
    }
}

#[test]
fn retained_ids_parent_and_working_directory_build_existing_start() {
    let session = SessionId::new();
    let parent = SessionId::new();
    let mut options = options(&["test", "open", "--instance", "primary"]);
    options.session_id = Some(session);
    options.parent = Some(parent);
    options.open_id = Some("retained-open".into());
    options.cwd = Some(std::env::current_dir().unwrap());
    let call = options.build_call(instance()).unwrap();
    assert_eq!(call.session_id, Some(session));
    assert_eq!(call.parent_session_id, Some(parent.to_string()));
    assert!(call.assistance_destination_session_id.is_none());
    assert_eq!(call.harness.request_id, "retained-open");
    let HarnessRequestKind::Open(open) = &call.harness.kind else {
        panic!("expected open");
    };
    assert_eq!(open.open_id, call.harness.request_id);
    assert_eq!(open.session_id, session.to_string());
    assert_eq!(open.generation, 1);
    assert!(PathBuf::from(&open.working_directory).is_absolute());
    assert!(open.resume.is_none());
    assert!(open.parent_assistance.is_none());
    let request = envelope(&call).unwrap();
    request.validate_envelope().unwrap();
    request.decode_call().unwrap();
    assert_eq!(request.operation, ManagedSessionOperation::Start);
    assert_eq!(
        serde_json::to_value(&call).unwrap(),
        serde_json::to_value(options.build_call(instance()).unwrap()).unwrap()
    );
}

#[test]
fn generated_ids_are_exact_and_never_resume_or_self_parent() {
    let mut options = options(&["test", "open", "--default-instance"]);
    let call = options.build_call(instance()).unwrap();
    let HarnessRequestKind::Open(open) = &call.harness.kind else {
        panic!("expected open");
    };
    assert_eq!(open.open_id, call.harness.request_id);
    assert_eq!(open.session_id, call.session_id.unwrap().to_string());
    assert!(uuid::Uuid::parse_str(&open.open_id).is_ok());
    assert!(call.parent_session_id.is_none());
    options.session_id = call.session_id;
    options.parent = call.session_id;
    assert!(options.build_call(instance()).is_err());
}

#[test]
fn preparation_must_echo_selection_and_exact_reference() {
    let selection = Selection::Instance {
        instance_id: "primary".into(),
    };
    let mut prepared = PreparedInstance {
        selection: selection.clone(),
        instance: instance(),
    };
    verify_prepared(&selection, &prepared).unwrap();
    prepared.instance.instance_id = "other".into();
    assert!(verify_prepared(&selection, &prepared).is_err());
    prepared.instance = instance();
    prepared.selection = Selection::Default {};
    assert!(verify_prepared(&selection, &prepared).is_err());
}
