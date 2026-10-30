//! Plugin-owned serialized question transitions for dynamic and bundled loading.
//!
//! State is returned to the host, not retained in a process-global controller map.

use bcode_plugin_models::interaction_controller::{
    INTERACTION_CONTROLLER_VERSION, InteractionControllerRequest, InteractionControllerRoute,
    InteractionControllerTransition, InteractionOutput, MAX_CONTROLLER_BYTES,
};
#[cfg(feature = "native")]
use bcode_plugin_models::interaction_controller::{
    InteractionControllerDiscovery, InteractionControllerDiscoveryRequest,
};
#[cfg(feature = "native")]
use bcode_plugin_sdk::{NativeServiceContext, ServiceResponse};

use super::{
    QUESTION_EXCHANGE_SCHEMA, QUESTION_INTERACTION_KIND,
    question_interaction::QuestionInteractionController,
};

/// Return the exact producer route supported by this controller.
#[must_use]
pub fn route() -> InteractionControllerRoute {
    InteractionControllerRoute {
        producer_id: "bcode.question".to_owned(),
        exchange_schema: QUESTION_EXCHANGE_SCHEMA.to_owned(),
        min_schema_version: 1,
        max_schema_version: 1,
        interaction_kind: QUESTION_INTERACTION_KIND.to_owned(),
    }
}

/// Route the versioned controller service. All diagnostics deliberately omit payloads.
#[must_use]
#[cfg(feature = "native")]
pub fn invoke(context: &NativeServiceContext) -> ServiceResponse {
    if context.request.payload.len() > MAX_CONTROLLER_BYTES {
        return ServiceResponse::error(
            "controller_limit_exceeded",
            "question controller request exceeds size limit",
        );
    }
    match context.request.operation.as_str() {
        "discover" => {
            let Ok(request) = context
                .request
                .payload_json::<InteractionControllerDiscoveryRequest>()
            else {
                return invalid();
            };
            if request.version != INTERACTION_CONTROLLER_VERSION {
                return invalid();
            }
            super::question_native::json_response(&InteractionControllerDiscovery {
                version: INTERACTION_CONTROLLER_VERSION,
                routes: vec![route()],
            })
        }
        "transition" => {
            let Ok(request) = context
                .request
                .payload_json::<InteractionControllerRequest>()
            else {
                return invalid();
            };
            match transition(request) {
                Ok(response) => super::question_native::json_response(&response),
                Err(_) => invalid(),
            }
        }
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported question controller operation",
        ),
    }
}

#[cfg(feature = "native")]
fn invalid() -> ServiceResponse {
    ServiceResponse::error(
        "invalid_controller_contract",
        "invalid or unsupported question controller contract",
    )
}

/// Open or advance a bounded serialized producer controller.
///
/// # Errors
/// Rejects unsupported routes/versions, malformed state, and oversized transitions.
pub fn transition(
    request: InteractionControllerRequest,
) -> Result<InteractionControllerTransition, QuestionControllerError> {
    transition_inner(request).map_err(|()| QuestionControllerError)
}

/// Invalid, unsupported, or oversized question controller contract. Payloads are never included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuestionControllerError;
impl std::fmt::Display for QuestionControllerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid or unsupported question controller contract")
    }
}
impl std::error::Error for QuestionControllerError {}

fn transition_inner(
    request: InteractionControllerRequest,
) -> Result<InteractionControllerTransition, ()> {
    let (controller, output) = match request {
        InteractionControllerRequest::Open {
            version,
            route: requested_route,
            schema_version,
            payload,
        } => {
            if version != INTERACTION_CONTROLLER_VERSION
                || requested_route != route()
                || schema_version != 1
            {
                return Err(());
            }
            let request = super::parse_question_request(payload).map_err(|_| ())?;
            (
                QuestionInteractionController::new(request),
                InteractionOutput::None,
            )
        }
        InteractionControllerRequest::Input {
            version,
            route: requested_route,
            state,
            input,
        } => {
            if version != INTERACTION_CONTROLLER_VERSION || requested_route != route() {
                return Err(());
            }
            let mut controller: QuestionInteractionController =
                serde_json::from_value(state).map_err(|_| ())?;
            controller.validate_ephemeral_state()?;
            let output = controller.handle_input(input);
            (controller, output)
        }
    };
    let snapshot = serde_json::to_value(controller.snapshot()).map_err(|_| ())?;
    let state = serde_json::to_value(controller).map_err(|_| ())?;
    let response = InteractionControllerTransition {
        version: INTERACTION_CONTROLLER_VERSION,
        interaction_kind: QUESTION_INTERACTION_KIND.to_owned(),
        state,
        snapshot,
        output,
    };
    // The host enforces the same bound before admission. A plugin never returns an oversized state.
    if serde_json::to_vec(&response).map_err(|_| ())?.len() > MAX_CONTROLLER_BYTES {
        return Err(());
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_plugin_models::interaction_controller::{InteractionControlId, InteractionInput};
    use serde_json::{Value, json};

    fn open() -> InteractionControllerTransition {
        transition(InteractionControllerRequest::Open {
            version: 1,
            route: route(),
            schema_version: 1,
            payload: json!({"questions": [{"question":"Choose", "required":true, "custom":false,
                "options":[{"label":"Yes","value":"yes"}]}]}),
        })
        .unwrap()
    }
    fn apply(state: Value, input: InteractionInput) -> InteractionControllerTransition {
        transition(InteractionControllerRequest::Input {
            version: 1,
            route: route(),
            state,
            input,
        })
        .unwrap()
    }
    #[test]
    fn canonical_request_and_ephemeral_state_roundtrip_before_input() {
        let opened = open();
        let controller: QuestionInteractionController =
            serde_json::from_value(opened.state.clone()).unwrap();
        assert!(controller.validate_ephemeral_state().is_ok());
        assert_eq!(serde_json::to_value(&controller).unwrap(), opened.state);
        assert_eq!(
            serde_json::to_value(controller.snapshot()).unwrap(),
            opened.snapshot
        );
    }
    #[test]
    fn serialized_controller_retains_plugin_validation_and_submit_semantics() {
        let opened = open();
        let invalid = apply(opened.state, InteractionInput::Submit);
        assert!(!matches!(
            invalid.output,
            InteractionOutput::Submitted { .. }
        ));
        let selected = apply(
            invalid.state,
            InteractionInput::Activate {
                control_id: InteractionControlId::new("question-0.option-0"),
            },
        );
        let submitted = apply(selected.state, InteractionInput::Submit);
        assert!(matches!(
            submitted.output,
            InteractionOutput::Submitted { .. }
        ));
    }
    #[test]
    fn future_and_spoofed_routes_fail_without_interpreting_payload() {
        assert!(
            transition(InteractionControllerRequest::Open {
                version: 2,
                route: route(),
                schema_version: 1,
                payload: Value::Null
            })
            .is_err()
        );
        let mut spoofed = route();
        spoofed.producer_id = "other".into();
        assert!(
            transition(InteractionControllerRequest::Open {
                version: 1,
                route: spoofed,
                schema_version: 1,
                payload: Value::Null
            })
            .is_err()
        );
    }
    #[test]
    fn semantic_cancel_is_plugin_owned_and_corrupt_state_is_rejected() {
        let cancelled = apply(open().state, InteractionInput::Cancel);
        assert!(matches!(
            cancelled.output,
            InteractionOutput::Submitted { .. } | InteractionOutput::Cancelled
        ));
        assert!(
            transition(InteractionControllerRequest::Input {
                version: 1,
                route: route(),
                state: json!({}),
                input: InteractionInput::Submit
            })
            .is_err()
        );
    }
}
