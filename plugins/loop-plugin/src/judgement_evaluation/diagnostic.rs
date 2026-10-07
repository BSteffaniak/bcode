//! Loop-owned parsed decision diagnostics. No persistence is implied by parsing.

use bcode_model::judgement::{Answer, Response};
use bcode_plugin_sdk::{ServiceBridge, ServiceBridgeRequest, ServiceBridgeResponse};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Plugin-owned sensitive capture consent; unrelated loop configuration remains accepted.
#[derive(Debug, Default, Deserialize)]
pub struct DiagnosticConfig {
    #[serde(default)]
    pub persist_judgement_diagnostics: bool,
}

/// Explicit capture receipts. Capture failures never change completion semantics.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JudgementDiagnosticCapture {
    pub version: u32,
    pub receipts: Vec<JudgementDiagnosticReceipt>,
}

/// Opaque application artifact reference, or a normalized reason capture was unavailable.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum JudgementDiagnosticReceipt {
    Written {
        boundary: String,
        artifact_id: String,
        byte_len: u64,
        reference: serde_json::Value,
    },
    Unavailable {
        boundary: String,
        reason: String,
    },
}

impl JudgementDiagnosticCapture {
    pub(super) fn write(
        &mut self,
        bridge: &ServiceBridge,
        dispatch_identity: &str,
        boundary: &str,
        payload: &impl Serialize,
    ) {
        self.version = 1;
        // Each dispatch has at most input, request, response and outcome. Bound bytes before routing.
        let value = serde_json::json!({
            "version": 1,
            "dispatch_identity": dispatch_identity,
            "boundary": boundary,
            "payload": payload,
        });
        let bytes = serde_json::to_vec(&value);
        let receipt = match bytes {
            Ok(bytes) if bytes.len() <= 512 * 1024 => {
                let artifact_id = format!("loop-judgement-v1-{boundary}");
                let resolution = bridge.request(&ServiceBridgeRequest::WriteArtifact(
                    bcode_tool::ToolArtifactWriteRequest {
                        invocation_id: dispatch_identity.into(),
                        artifact_id,
                        content_type: "application/json".into(),
                        bytes,
                        metadata: serde_json::json!({
                            "schema": "bcode.loop.judgement-diagnostic",
                            "version": 1,
                            "sensitive": true,
                            "boundary": boundary,
                        }),
                    },
                ));
                match resolution {
                    Ok(ServiceBridgeResponse::Artifact(
                        bcode_tool::ToolArtifactWriteResolution::Written {
                            artifact_id,
                            byte_len,
                            reference,
                        },
                    )) => JudgementDiagnosticReceipt::Written {
                        boundary: boundary.into(),
                        artifact_id,
                        byte_len,
                        reference,
                    },
                    Ok(ServiceBridgeResponse::Artifact(
                        bcode_tool::ToolArtifactWriteResolution::Cancelled,
                    )) => Self::unavailable(boundary, "cancelled"),
                    Ok(ServiceBridgeResponse::Artifact(
                        bcode_tool::ToolArtifactWriteResolution::TooLarge { .. },
                    )) => Self::unavailable(boundary, "too_large"),
                    _ => Self::unavailable(boundary, "artifact_write_unavailable"),
                }
            }
            Ok(_) => Self::unavailable(boundary, "too_large"),
            Err(_) => Self::unavailable(boundary, "encoding_failed"),
        };
        self.receipts.push(receipt);
    }

    fn unavailable(boundary: &str, reason: &str) -> JudgementDiagnosticReceipt {
        JudgementDiagnosticReceipt::Unavailable {
            boundary: boundary.into(),
            reason: reason.into(),
        }
    }
}

/// Versioned sensitive decision artifact; persist only with explicit diagnostic consent.
#[derive(Debug, PartialEq, Serialize)]
pub struct CompletionDecision {
    pub version: u32,
    pub evaluator_condition_met: bool,
    pub threshold_percent: u8,
    pub probability: f64,
    pub condition_met: bool,
}

impl CompletionDecision {
    pub(super) fn parse(
        response: &Response,
        evaluator_condition_met: bool,
        threshold_percent: u8,
    ) -> Result<Self, &'static str> {
        if response.answers.len() != 1 {
            return Err("invalid judgement answer");
        }
        let Some(Answer::YesNo { probability }) = response.answers.get("completion") else {
            return Err("invalid judgement answer");
        };
        if !probability.is_finite() || !(0.0..=1.0).contains(probability) {
            return Err("invalid judgement answer");
        }
        Ok(Self {
            version: 1,
            evaluator_condition_met,
            threshold_percent,
            probability: *probability,
            // Confidence cannot promote unresolved agent evidence into completion.
            condition_met: evaluator_condition_met
                && *probability >= f64::from(threshold_percent) / 100.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn consent_is_explicit_and_typed() {
        assert!(
            !serde_json::from_value::<DiagnosticConfig>(serde_json::json!({}))
                .unwrap()
                .persist_judgement_diagnostics
        );
        assert!(
            serde_json::from_value::<DiagnosticConfig>(serde_json::json!({
                "persist_judgement_diagnostics": true,
            }))
            .unwrap()
            .persist_judgement_diagnostics
        );
        assert!(
            serde_json::from_value::<DiagnosticConfig>(serde_json::json!({
                "persist_judgement_diagnostics": "true",
            }))
            .is_err()
        );
    }

    #[test]
    fn missing_artifact_capability_is_reported_without_sensitive_error_text() {
        let mut capture = JudgementDiagnosticCapture::default();
        capture.write(
            &ServiceBridge::default(),
            "dispatch",
            "request",
            &"sensitive evidence",
        );
        assert_eq!(capture.version, 1);
        assert!(
            matches!(&capture.receipts[..], [JudgementDiagnosticReceipt::Unavailable {
            boundary, reason,
        }] if boundary == "request" && reason == "artifact_write_unavailable")
        );
    }

    #[test]
    fn artifact_capture_persists_exact_payload_and_retains_opaque_reference() {
        extern "C" fn callback(
            request_ptr: *const u8,
            request_len: usize,
            output_ptr: *mut u8,
            output_capacity: usize,
            output_len: *mut usize,
            _user_data: *mut std::ffi::c_void,
        ) -> i32 {
            let bytes = unsafe { std::slice::from_raw_parts(request_ptr, request_len) };
            let request: ServiceBridgeRequest = serde_json::from_slice(bytes).unwrap();
            let ServiceBridgeRequest::WriteArtifact(request) = request else {
                panic!("expected artifact capability");
            };
            assert_eq!(request.invocation_id, "dispatch");
            assert_eq!(request.artifact_id, "loop-judgement-v1-request");
            assert_eq!(request.metadata["sensitive"], true);
            let value: serde_json::Value = serde_json::from_slice(&request.bytes).unwrap();
            assert_eq!(
                value["payload"],
                serde_json::json!({"observed": "exact fixture bytes"})
            );
            let response =
                ServiceBridgeResponse::Artifact(bcode_tool::ToolArtifactWriteResolution::Written {
                    artifact_id: request.artifact_id,
                    byte_len: u64::try_from(request.bytes.len()).unwrap(),
                    reference: serde_json::json!({"uri": "opaque-fixture-reference"}),
                });
            let encoded = serde_json::to_vec(&response).unwrap();
            assert!(encoded.len() <= output_capacity);
            unsafe {
                std::ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
                *output_len = encoded.len();
            }
            0
        }
        let bridge = ServiceBridge::new(
            Some(callback),
            std::ptr::null_mut(),
            bcode_plugin_sdk::ServiceCancellation::default(),
        );
        let mut capture = JudgementDiagnosticCapture::default();
        capture.write(
            &bridge,
            "dispatch",
            "request",
            &serde_json::json!({"observed": "exact fixture bytes"}),
        );
        assert!(
            matches!(&capture.receipts[..], [JudgementDiagnosticReceipt::Written {
            reference, ..
        }] if reference["uri"] == "opaque-fixture-reference")
        );
    }

    fn response(probability: f64) -> Response {
        Response {
            answers: BTreeMap::from([("completion".into(), Answer::YesNo { probability })]),
            usage: None,
        }
    }

    #[test]
    fn capture_decision_matches_gate_without_promoting_unverified_evidence() {
        let decision = CompletionDecision::parse(&response(1.0), false, 90).unwrap();
        assert!(!decision.condition_met);
        let artifact = serde_json::to_value(decision).unwrap();
        assert_eq!(artifact["version"], 1);
        assert_eq!(artifact["evaluator_condition_met"], false);
        assert_eq!(artifact["probability"], 1.0);
        assert_eq!(artifact["threshold_percent"], 90);
        assert!(
            CompletionDecision::parse(&response(0.9), true, 90)
                .unwrap()
                .condition_met
        );
        assert!(
            !CompletionDecision::parse(&response(0.899), true, 90)
                .unwrap()
                .condition_met
        );
    }

    #[test]
    fn malformed_answers_never_produce_a_decision_artifact() {
        for probability in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
            assert!(CompletionDecision::parse(&response(probability), true, 90).is_err());
        }
        let mut answer = response(1.0);
        answer
            .answers
            .insert("extra".into(), Answer::YesNo { probability: 1.0 });
        assert!(CompletionDecision::parse(&answer, true, 90).is_err());
        answer.answers.remove("completion");
        assert!(CompletionDecision::parse(&answer, true, 90).is_err());
    }
}
