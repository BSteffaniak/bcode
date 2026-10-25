//! Behavioral coverage only: no microphone, network, shell, clipboard or inference.
use bcode_plugin_sdk::{ServiceRequest, ServiceResponse};
use bcode_vox::contracts::{Action, Error, Status};
use bcode_vox_plugin::{
    frontend::LocalFrontend,
    service::{self, LocalControl, StatusResponse},
};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

#[derive(Default)]
struct Fake {
    actions: Rc<RefCell<Vec<Action>>>,
    fail: bool,
}
impl LocalControl for Fake {
    fn action(&mut self, action: Action) -> bcode_vox::Result<()> {
        self.actions.borrow_mut().push(action);
        if self.fail {
            Err(Error::Backend("private transcript credential".into()))
        } else {
            Ok(())
        }
    }
    fn status(&self) -> bcode_vox::Result<Status> {
        Ok(Status::Failed {
            message: "private transcript credential".into(),
        })
    }
}
fn request(operation: &str, payload: &str) -> ServiceRequest {
    ServiceRequest {
        interface_id: service::INTERFACE_ID.into(),
        operation: operation.into(),
        payload: payload.as_bytes().to_vec(),
    }
}
fn code(response: &ServiceResponse) -> Option<&str> {
    response.error.as_ref().map(|error| error.code.as_str())
}

#[test]
fn absent_frontend_never_claims_idle_or_accepts_actions() {
    for action in ["start", "stop", "toggle", "cancel"] {
        let response = service::dispatch(
            &request("action", &format!(r#"{{"version":1,"action":"{action}"}}"#)),
            None,
        );
        assert_eq!(code(&response), Some("local_frontend_required"));
    }
    let response = service::dispatch(&request("status", r#"{"version":1}"#), None);
    let value: StatusResponse = response.payload_json().unwrap();
    assert!(!value.local_frontend);
    assert!(value.status.is_none());
}

#[test]
fn bound_frontend_consumes_each_user_action_once() {
    let fake = Fake::default();
    let observed = fake.actions.clone();
    let mut frontend = LocalFrontend::new(fake);
    for action in ["start", "stop", "toggle", "cancel"] {
        let response = frontend.invoke(&request(
            "action",
            &format!(r#"{{"version":1,"action":"{action}"}}"#),
        ));
        assert!(response.error.is_none());
        let value: service::ActionResponse = response.payload_json().unwrap();
        assert!(value.accepted);
    }
    assert_eq!(
        *observed.borrow(),
        [Action::Start, Action::Stop, Action::Toggle, Action::Cancel]
    );
}

#[test]
fn unsafe_diagnostics_are_normalized_and_failures_not_retried() {
    let fake = Fake {
        fail: true,
        ..Fake::default()
    };
    let observed = fake.actions.clone();
    let mut frontend = LocalFrontend::new(fake);
    let response = frontend.invoke(&request("action", r#"{"version":1,"action":"stop"}"#));
    assert_eq!(code(&response), Some("local_runtime_failed"));
    assert!(!response.error.unwrap().message.contains("private"));
    assert_eq!(observed.borrow().len(), 1);
    let response = frontend.invoke(&request("status", r#"{"version":1}"#));
    let value: StatusResponse = response.payload_json().unwrap();
    assert!(value.local_frontend);
    assert_eq!(
        value.status,
        Some(Status::Failed {
            message: "Vox local operation failed".into()
        })
    );
    assert_eq!(observed.borrow().len(), 1);
}

#[test]
fn invalid_payloads_cannot_select_or_spoof_locality() {
    let fake = Fake::default();
    let observed = fake.actions.clone();
    let mut frontend = LocalFrontend::new(fake);
    for payload in [
        r#"{"version":2,"action":"start"}"#,
        r#"{"version":1,"action":"start","local_frontend":true}"#,
        r#"{"version":1,"action":"capture"}"#,
        r#"{"version":1}"#,
    ] {
        assert!(frontend.invoke(&request("action", payload)).error.is_some());
    }
    assert_eq!(
        code(&frontend.invoke(&request(
            "action",
            &"x".repeat(service::MAX_REQUEST_BYTES + 1)
        ))),
        Some("request_too_large")
    );
    assert_eq!(
        code(&frontend.invoke(&request("microphone", "{}"))),
        Some("unsupported_operation")
    );
    assert!(observed.borrow().is_empty());
}

#[test]
fn command_discovery_and_local_dispatch_agree() {
    let fake = Fake::default();
    let observed = fake.actions.clone();
    let mut frontend = LocalFrontend::new(fake);
    let commands = bcode_vox_plugin::commands();
    assert_eq!(commands.len(), 5);
    for command in commands {
        let payload = serde_json::to_vec(&bcode_command::InvokeCommandRequest {
            command_id: command.id,
            args: BTreeMap::new(),
            context: None,
        })
        .unwrap();
        let request = ServiceRequest {
            interface_id: bcode_command::COMMAND_INTERFACE_ID.into(),
            operation: bcode_command::OP_INVOKE_COMMAND.into(),
            payload,
        };
        let response = frontend.invoke(&request);
        assert!(response.error.is_none());
        let value: bcode_command::InvokeCommandResponse = response.payload_json().unwrap();
        assert!(value.success);
        assert!(value.effects.is_empty());
        assert!(!value.message.unwrap().contains("private"));
    }
    assert_eq!(observed.borrow().len(), 4);
    frontend.shutdown().unwrap();
    assert_eq!(observed.borrow().last(), Some(&Action::Cancel));
}

#[test]
fn daemon_commands_fail_closed_and_reject_arguments() {
    let request = ServiceRequest {
        interface_id: bcode_command::COMMAND_INTERFACE_ID.into(),
        operation: bcode_command::OP_INVOKE_COMMAND.into(),
        payload: br#"{"command_id":"vox.start","args":{},"context":null}"#.to_vec(),
    };
    assert_eq!(
        code(&service::dispatch(&request, None)),
        Some("local_frontend_required")
    );
    let mut request = request;
    request.payload = br#"{"command_id":"vox.start","args":{"arguments":"clipboard"}}"#.to_vec();
    assert_eq!(
        code(&service::dispatch(&request, None)),
        Some("invalid_arguments")
    );
}
