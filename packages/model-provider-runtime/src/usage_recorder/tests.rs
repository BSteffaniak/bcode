//! Recorder invariants shared by every provider adapter.

use super::*;

struct Decoder;
impl UsageDecoder for Decoder {
    fn capture_spec(&self) -> UsageCaptureSpec {
        UsageCaptureSpec {
            api_shape: "fixture",
            containers: &[&["reply"]],
            default_source: "usage",
            confirmed_fields: &[],
            complete_sources: &["done"],
        }
    }
    fn normalize(&self, original: &OriginalUsage) -> Result<TokenUsage, String> {
        let value: serde_json::Value =
            serde_json::from_str(&original.reports.last().ok_or("missing")?.usage_json)
                .map_err(|_| "invalid")?;
        Ok(TokenUsage {
            input_tokens: value["input"]
                .as_u64()
                .and_then(|value| u32::try_from(value).ok()),
            output_tokens: Some(0),
            ..Default::default()
        })
    }
}

#[test]
fn exact_evidence_and_normalized_usage_publish_once_in_order() {
    let mut recorder = UsageRecorder::new("provider", &Decoder, BTreeMap::new(), true);
    assert_eq!(
        recorder.observe_json(r#"{"type":"text","text":"not billing"}"#),
        UsageObservation::Absent
    );
    assert_eq!(recorder.observe_json(r#"{"type":"done","reply":{"usage":{"input":10,"future":123456789012345678901234567890,"fraction":1.2300}}}"#),UsageObservation::Captured);
    let mut events = Vec::new();
    let live = recorder.finish(true, |event| events.push(event)).unwrap();
    assert!(recorder.finish(true, |event| events.push(event)).is_none());
    let [
        ProviderTurnEvent::OriginalUsage { original },
        ProviderTurnEvent::Usage { usage },
    ] = events.as_slice()
    else {
        panic!("paired publication")
    };
    assert!(original.reports[0].usage_json.contains("1.2300"));
    assert!(
        original.reports[0]
            .usage_json
            .contains("123456789012345678901234567890")
    );
    assert_eq!(*usage, live);
    assert_eq!(
        normalize_registered_usage("provider", &[&Decoder], original).unwrap(),
        live
    );
    assert!(normalize_registered_usage("foreign", &[&Decoder], original).is_err());
    let mut received = None;
    assert!(
        super::super::receive_original_usage("foreign", &mut received, (**original).clone())
            .is_err()
    );
    assert!(received.is_none());
    super::super::receive_original_usage("provider", &mut received, (**original).clone()).unwrap();
    assert_eq!(received.as_ref(), Some(original.as_ref()));
}

#[test]
fn terminal_without_observations_rejects_late_reports() {
    let mut recorder = UsageRecorder::new("provider", &Decoder, BTreeMap::new(), true);
    assert!(
        recorder
            .finish(false, |_| panic!("no observations"))
            .is_none()
    );
    assert_eq!(
        recorder.observe_json(r#"{"type":"done","reply":{"usage":{"input":5}}}"#),
        UsageObservation::Absent
    );
    assert!(
        recorder
            .finish(true, |_| panic!("terminal state reopened"))
            .is_none()
    );
}

#[test]
fn invalid_requested_settings_are_not_published() {
    let mut recorder = UsageRecorder::new(
        "provider",
        &Decoder,
        BTreeMap::from([("authorization".into(), "SECRET".into())]),
        true,
    );
    let mut events = Vec::new();
    recorder.finish(false, |event| events.push(event));
    let [ProviderTurnEvent::OriginalUsage { original }] = events.as_slice() else {
        panic!("request capture issue")
    };
    assert!(original.validate().is_ok());
    assert!(original.requested.is_empty());
    assert!(!serde_json::to_string(&events).unwrap().contains("SECRET"));
}

#[test]
fn scoped_drop_flushes_interrupted_evidence_once() {
    let events = std::cell::RefCell::new(Vec::new());
    {
        let mut recorder = UsageRecorder::new("provider", &Decoder, BTreeMap::new(), true)
            .scoped(|event| events.borrow_mut().push(event));
        recorder.observe_json(r#"{"reply":{"usage":{"input":5}}}"#);
    }
    let events = events.borrow();
    let [
        ProviderTurnEvent::OriginalUsage { original },
        ProviderTurnEvent::Usage { usage },
    ] = events.as_slice()
    else {
        panic!("drop publication")
    };
    assert!(!original.complete);
    assert_eq!(usage.output_tokens, None);
}

#[test]
fn capture_off_and_interruption_do_not_fabricate_final_output() {
    let mut recorder = UsageRecorder::new("provider", &Decoder, BTreeMap::new(), false);
    recorder.observe_json(r#"{"reply":{"usage":{"input":5}}}"#);
    let mut events = Vec::new();
    let usage = recorder.finish(false, |event| events.push(event)).unwrap();
    assert_eq!(usage.output_tokens, None);
    assert!(matches!(
        events.as_slice(),
        [ProviderTurnEvent::Usage { .. }]
    ));
}

#[test]
fn malformed_capture_differs_from_absence_and_preserves_accepted_evidence() {
    let mut recorder = UsageRecorder::new("provider", &Decoder, BTreeMap::new(), true);
    recorder.observe_json(r#"{"reply":{"usage":{"input":5}}}"#);
    assert_eq!(
        recorder.observe_json("invalid JSON"),
        UsageObservation::Rejected
    );
    let mut events = Vec::new();
    assert!(recorder.finish(false, |event| events.push(event)).is_none());
    let [ProviderTurnEvent::OriginalUsage { original }] = events.as_slice() else {
        panic!("failure evidence")
    };
    assert_eq!(original.reports.len(), 1);
    assert_eq!(
        original.capture_issue,
        Some(UsageCaptureIssue::UnsafeOrMalformed)
    );
}

#[test]
fn unsafe_billing_is_not_retained_but_valid_normalized_fields_can_survive() {
    let mut recorder = UsageRecorder::new("provider", &Decoder, BTreeMap::new(), true);
    recorder.observe_json(r#"{"type":"done","reply":{"usage":{"input":5,"access_token":"SECRET"},"output":"PRIVATE"}}"#);
    let mut events = Vec::new();
    recorder.finish(true, |event| events.push(event));
    let [
        ProviderTurnEvent::OriginalUsage { original },
        ProviderTurnEvent::Usage { usage },
    ] = events.as_slice()
    else {
        panic!("capture and normalization are independent")
    };
    assert!(original.reports.is_empty());
    assert_eq!(
        original.capture_issue,
        Some(UsageCaptureIssue::UnsafeOrMalformed)
    );
    assert_eq!(usage.input_tokens, Some(5));
    assert!(!format!("{events:?}").contains("SECRET"));
    assert!(!serde_json::to_string(&events).unwrap().contains("PRIVATE"));
}

#[test]
fn byte_budget_and_depth_failures_preserve_prior_evidence() {
    for rejected in [
        format!(
            r#"{{"reply":{{"usage":{{"large":{}}}}}}}"#,
            "1".repeat(bcode_session_models::MAX_ORIGINAL_USAGE_BYTES)
        ),
        format!(
            r#"{{"reply":{{"usage":{{"deep":{}0{}}}}}}}"#,
            "[".repeat(34),
            "]".repeat(34)
        ),
    ] {
        let mut recorder = UsageRecorder::new("provider", &Decoder, BTreeMap::new(), true);
        recorder.observe_json(r#"{"reply":{"usage":{"input":5}}}"#);
        assert_eq!(recorder.observe_json(&rejected), UsageObservation::Rejected);
        let mut events = Vec::new();
        recorder.finish(false, |event| events.push(event));
        let ProviderTurnEvent::OriginalUsage { original } = &events[0] else {
            panic!("capture issue")
        };
        assert_eq!(original.reports.len(), 1);
        assert!(original.validate().is_ok());
    }
}

#[test]
fn exhaustion_retains_all_previously_accepted_reports() {
    let mut recorder = UsageRecorder::new("provider", &Decoder, BTreeMap::new(), true);
    for _ in 0..65 {
        recorder.observe_json(r#"{"reply":{"usage":{"input":5}}}"#);
    }
    let mut events = Vec::new();
    recorder.finish(false, |event| events.push(event));
    let [
        ProviderTurnEvent::OriginalUsage { original },
        ProviderTurnEvent::Usage { .. },
    ] = events.as_slice()
    else {
        panic!("limit evidence")
    };
    assert_eq!(original.reports.len(), 64);
    assert!(original.validate().is_ok());
    assert_eq!(
        original.capture_issue,
        Some(UsageCaptureIssue::LimitExceeded)
    );
}
