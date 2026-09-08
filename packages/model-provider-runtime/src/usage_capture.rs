//! Billing-only capture before provider usage normalization.

use bcode_session_models::{OriginalUsage, OriginalUsageReport, UsageCaptureIssue};
use std::collections::BTreeMap;

/// Capture untouched usage JSON and selected outer billing labels.
///
/// Request context is supplied separately by the provider adapter. Returns an explicit incomplete
/// capture instead of retaining secret-bearing or oversized data.
#[must_use]
pub fn capture_usage_json(
    provider: &str,
    shape: &str,
    json: &str,
    source: &str,
) -> Option<OriginalUsage> {
    let event: BTreeMap<&str, &serde_json::value::RawValue> = serde_json::from_str(json).ok()?;
    let source = event
        .get("type")
        .and_then(|raw| serde_json::from_str::<String>(raw.get()).ok())
        .unwrap_or_else(|| source.to_owned());
    let container = if let Some(response) = event.get("response").or_else(|| event.get("message")) {
        serde_json::from_str::<BTreeMap<&str, &serde_json::value::RawValue>>(response.get()).ok()?
    } else {
        event
    };
    let raw = container.get("usage")?;
    if raw.get() == "null" {
        return None;
    }
    if raw.get().len() > bcode_session_models::MAX_ORIGINAL_USAGE_BYTES {
        return Some(OriginalUsage {
            provider_id: provider.into(),
            api_shape: shape.into(),
            capture_issue: Some(UsageCaptureIssue::LimitExceeded),
            ..Default::default()
        });
    }
    let mut confirmed = BTreeMap::new();
    for field in ["model", "service_tier", "prompt_cache_retention"] {
        if let Some(value) = container
            .get(field)
            .and_then(|raw| serde_json::from_str::<String>(raw.get()).ok())
        {
            confirmed.insert(field.to_owned(), value);
        }
    }
    let mut original = OriginalUsage {
        provider_id: provider.into(),
        api_shape: shape.into(),
        reports: vec![OriginalUsageReport {
            source,
            usage_json: raw.get().into(),
            confirmed,
        }],
        ..Default::default()
    };
    if original.validate().is_err() {
        original.reports.clear();
        original.requested.clear();
        original.capture_issue = Some(UsageCaptureIssue::UnsafeOrMalformed);
    }
    Some(original)
}

/// Validate a provider billing capture before transport; preserve a failure marker, never unsafe bytes.
pub fn finalize_usage_capture(original: &mut OriginalUsage) {
    if original.validate().is_err() {
        original.reports.clear();
        original.requested.clear();
        original.capture_issue = Some(UsageCaptureIssue::UnsafeOrMalformed);
    }
}

/// Accumulate billing-only reports in order with an explicit capture failure on overflow.
pub fn append_usage_capture(target: &mut Option<OriginalUsage>, incoming: OriginalUsage) {
    if let Some(current) = target {
        current.complete |= incoming.complete;
        if current.capture_issue == Some(UsageCaptureIssue::LimitExceeded) {
            return;
        }
        if current.provider_id != incoming.provider_id || current.api_shape != incoming.api_shape {
            current.capture_issue = Some(UsageCaptureIssue::UnsafeOrMalformed);
            return;
        }
        current.reports.extend(incoming.reports);
        for (key, value) in incoming.requested {
            if current.requested.get(&key).is_some_and(|old| old != &value) {
                current.capture_issue = Some(UsageCaptureIssue::UnsafeOrMalformed);
            }
            current.requested.insert(key, value);
        }
        current.complete |= incoming.complete;
        current.capture_issue = current.capture_issue.or(incoming.capture_issue);
        if current.validate().is_err() {
            current.reports.clear();
            current.capture_issue = Some(UsageCaptureIssue::LimitExceeded);
        }
    } else {
        *target = Some(incoming);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_numbers_survive_capture_without_response_content() {
        let json = r#"{"response":{"model":"model","output":[{"text":"SECRET"}],"usage":{"future":123456789012345678901234567890,"details":{"novel":1.2300},"zero":0,"null":null}}}"#;
        let capture =
            capture_usage_json("provider", "responses", json, "response.completed").unwrap();
        assert!(
            capture.reports[0]
                .usage_json
                .contains("123456789012345678901234567890")
        );
        assert!(capture.reports[0].usage_json.contains("1.2300"));
        assert!(!format!("{capture:?}").contains("future"));
        assert!(!serde_json::to_string(&capture).unwrap().contains("SECRET"));
    }
    /// Capture budget is cumulative across fragments; once exceeded it remains incomplete.
    #[test]
    fn capture_limit_is_sticky_and_does_not_retain_unsafe_bytes() {
        let mut target = None;
        for _ in 0..70 {
            let incoming = capture_usage_json(
                "provider",
                "responses",
                r#"{"usage":{"tokens":1}}"#,
                "usage",
            )
            .unwrap();
            append_usage_capture(&mut target, incoming);
        }
        let capture = target.unwrap();
        assert_eq!(
            capture.capture_issue,
            Some(UsageCaptureIssue::LimitExceeded)
        );
        assert!(capture.reports.is_empty());
    }

    #[test]
    fn unsafe_usage_is_marked_not_silently_redacted() {
        let capture = capture_usage_json(
            "provider",
            "responses",
            r#"{"usage":{"access_token":"secret"}}"#,
            "response.completed",
        )
        .unwrap();
        assert!(capture.reports.is_empty());
        assert_eq!(
            capture.capture_issue,
            Some(UsageCaptureIssue::UnsafeOrMalformed)
        );
    }
}
