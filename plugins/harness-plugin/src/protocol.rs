//! Plugin-owned application envelope, reusing the authoritative harness protocol.
use bcode_harness_models::{HarnessRequest, HarnessRequestKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Host-owned application route, deliberately distinct from the native harness service.
pub const MANAGED_SESSION_INTERFACE_ID: &str = "bcode.managed-session/v1";
/// Supported application operations; none implicitly grants execution authority.
pub const OPERATIONS: [&str; 5] = ["start", "inspect", "followup", "respond", "interrupt"];
/// Maximum encoded application request/result bytes.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// Managed-session admission request. Parent and assistance destination are hints checked by
/// the host against authenticated context, never authority or routing inferred from parentage.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSessionRequest {
    /// Exact Bcode target for instance inspection before a native binding exists.
    #[serde(default)]
    pub session_id: Option<bcode_session_models::SessionId>,
    /// Authoritative typed harness operation and retained mutation identity.
    pub harness: HarnessRequest,
    /// Immutable ordinary child association, admitted only at creation.
    pub parent_session_id: Option<String>,
    /// Explicit request for attention routing; does not grant response/approval authority.
    pub assistance_destination_session_id: Option<String>,
}

impl ManagedSessionRequest {
    /// Validate the semantic operation and bounds without performing admission or effects.
    ///
    /// # Errors
    /// Rejects malformed, oversized, incompatible and mismatched operations.
    pub fn validate(&self, operation: &str) -> Result<(), &'static str> {
        self.harness
            .validate()
            .map_err(|_| "invalid harness request")?;
        let matches = match operation {
            "start" => matches!(self.harness.kind, HarnessRequestKind::Open(_)),
            "inspect" => matches!(
                self.harness.kind,
                HarnessRequestKind::Inspect
                    | HarnessRequestKind::Observe(_)
                    | HarnessRequestKind::Reconcile(_)
            ),
            "followup" => matches!(self.harness.kind, HarnessRequestKind::Submit { .. }),
            "respond" => matches!(self.harness.kind, HarnessRequestKind::Respond { .. }),
            "interrupt" => matches!(self.harness.kind, HarnessRequestKind::Interrupt(_)),
            _ => false,
        };
        if !matches || (operation != "start" && self.parent_session_id.is_some()) {
            return Err("managed operation mismatch");
        }
        let mutation_id = match &self.harness.kind {
            HarnessRequestKind::Open(open) => Some(open.open_id.as_str()),
            HarnessRequestKind::Submit { submission, .. } => {
                Some(submission.submission_id.as_str())
            }
            HarnessRequestKind::Respond { response, .. } => Some(response.response_id.as_str()),
            HarnessRequestKind::Interrupt(control) => Some(control.mutation_id.as_str()),
            _ => None,
        };
        if mutation_id.is_some_and(|id| id != self.harness.request_id) {
            return Err("correlation must equal retained mutation identity");
        }
        for identity in [
            &self.parent_session_id,
            &self.assistance_destination_session_id,
        ]
        .into_iter()
        .flatten()
        {
            if identity.is_empty()
                || identity.len() > 256
                || identity.trim() != identity
                || identity.chars().any(char::is_control)
            {
                return Err("invalid session identity");
            }
        }
        bounded(self)?;
        Ok(())
    }
}

fn bounded(value: &impl Serialize) -> Result<(), &'static str> {
    struct Limit(usize);
    impl std::io::Write for Limit {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("response limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Limit(MAX_REQUEST_BYTES), value)
        .map_err(|_| "managed session payload exceeds bound")
}

/// Decode once and retain the exact validated payload for preparation binding.
///
/// # Errors
/// Rejects invalid shapes, operation mismatches and excessive payloads.
pub fn validated_payload(operation: &str, value: Value) -> Result<Value, &'static str> {
    bounded(&value)?;
    let request: ManagedSessionRequest =
        serde_json::from_value(value.clone()).map_err(|_| "invalid managed session request")?;
    request.validate(operation)?;
    // The original exact payload is retained; unknown nested fields cannot disappear into
    // a different mutation after policy has inspected the preparation descriptor.
    Ok(value)
}

pub fn bounded_response(value: &Value) -> Result<(), &'static str> {
    bounded(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(operation: &str, body: Value) -> Value {
        let mut request = json!({"harness": {"schema_version":1,"request_id":"retained", "instance": {
            "instance_id":"instance", "plugin_id":"test.harness", "executable":"/explicit/harness", "config_namespace":"isolated",
            "auth":{"provider_id":"test.auth", "profile":"native", "credential_namespace":"isolated", "custody":"native_managed"}
        }, "kind":{"operation":operation}}, "parent_session_id":null, "assistance_destination_session_id":null});
        request["harness"]["kind"]["request"] = body;
        request
    }

    fn binding() -> Value {
        json!({"state":"confirmed","session":{"schema_version":1,"session_id":"child","instance_id":"instance", "native_session_id":"native","native_version":"1.0.0","generation":2}})
    }

    #[test]
    fn top_level_and_child_creation_share_one_typed_operation() {
        let top = request(
            "open",
            json!({"open_id":"retained","session_id":"child", "generation":2, "working_directory":"/workspace", "resume":null}),
        );
        assert!(validated_payload("start", top.clone()).is_ok());
        let mut child = top;
        child["parent_session_id"] = json!("parent");
        child["assistance_destination_session_id"] = json!("parent");
        assert!(validated_payload("start", child.clone()).is_ok());
        assert!(validated_payload("followup", child).is_err());
    }

    #[test]
    fn assistance_remains_available_independently_of_parent_execution_state() {
        // There is intentionally no parent-running prerequisite. Admission and authority
        // belong to the host; these fields express requested destination, not a grant.
        let mut response = request(
            "respond",
            json!({"target":binding(), "response": {
                "response_id":"retained", "request_id":"question", "session_id":"child", "generation":2,
                "value":{"kind":"assistance", "text":"Use the documented API"}
            }}),
        );
        response["assistance_destination_session_id"] = json!("parent");
        assert!(validated_payload("respond", response.clone()).is_ok());
        response["harness"]["kind"]["request"]["response"]["generation"] = json!(1);
        assert!(validated_payload("respond", response).is_err());
    }

    #[test]
    fn retained_steering_and_control_ids_cannot_be_silently_replaced() {
        let mut followup = request(
            "submit",
            json!({"target":binding(), "submission": {
                "submission_id":"retained", "attempt_id":"attempt", "session_id":"child", "generation":2,
                "text":"Follow up", "placement":{"kind":"steer", "expected_native_turn_id":"native-turn"}
            }}),
        );
        assert!(validated_payload("followup", followup.clone()).is_ok());
        followup["harness"]["kind"]["request"]["submission"]["submission_id"] =
            json!("replacement");
        assert!(validated_payload("followup", followup).is_err());
        let control = request(
            "interrupt",
            json!({"target":binding(),"mutation_id":"retained"}),
        );
        assert!(validated_payload("interrupt", control).is_ok());
    }

    #[test]
    fn prepared_first_followup_is_typed_and_cannot_steer_an_unconfirmed_turn() {
        let mut call = request(
            "submit",
            json!({"target":{"state":"prepared","session":{
            "open_id":"open","session_id":"child","instance_id":"instance","generation":2,
            "native_version":"1.0.0","requested_native_session_id":null
        }},"submission":{"submission_id":"retained","attempt_id":"attempt","session_id":"child","generation":2,
            "text":"First user input","placement":{"kind":"next_turn"}}}),
        );
        assert!(validated_payload("followup", call.clone()).is_ok());
        call["harness"]["kind"]["request"]["submission"]["placement"] =
            json!({"kind":"steer","expected_native_turn_id":"invented"});
        assert!(validated_payload("followup", call).is_err());
    }

    #[test]
    fn malformed_requests_and_authority_smuggling_fail_closed() {
        assert!(validated_payload("start", json!({"authority": "parent"})).is_err());
        assert!(
            validated_payload("followup", json!({"harness": {"operation": "auth_begin"}})).is_err()
        );
        assert!(validated_payload("unknown", json!({})).is_err());
    }

    #[test]
    fn output_is_bounded_before_model_or_command_presentation() {
        assert!(bounded_response(&json!({"text": "x".repeat(MAX_REQUEST_BYTES)})).is_err());
        assert!(bounded_response(&json!({"receipt": "retained"})).is_ok());
    }
}
