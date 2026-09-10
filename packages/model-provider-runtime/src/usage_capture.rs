//! Bounded accumulation used by request capture and host receipt.
use bcode_session_models::{OriginalUsage, UsageCaptureIssue};

/// Accumulate billing-only reports in order with an explicit capture failure on overflow.
pub fn append_usage_capture(target: &mut Option<OriginalUsage>, incoming: OriginalUsage) {
    let current = target.get_or_insert_with(|| OriginalUsage {
        provider_id: incoming.provider_id.clone(),
        api_shape: incoming.api_shape.clone(),
        ..Default::default()
    });
    if current.provider_id != incoming.provider_id || current.api_shape != incoming.api_shape {
        current.capture_issue = Some(UsageCaptureIssue::UnsafeOrMalformed);
        return;
    }
    current.complete |= incoming.complete;
    if current
        .capture_issue
        .is_some_and(|issue| issue != UsageCaptureIssue::SdkFieldsOnly)
    {
        return;
    }
    let mut candidate = current.clone();
    for (key, value) in incoming.requested {
        if candidate
            .requested
            .get(&key)
            .is_some_and(|old| old != &value)
        {
            current.capture_issue = Some(UsageCaptureIssue::UnsafeOrMalformed);
            return;
        }
        candidate.requested.insert(key, value);
    }
    candidate.reports.extend(incoming.reports);
    candidate.capture_issue = match (candidate.capture_issue, incoming.capture_issue) {
        (_, Some(issue)) if issue != UsageCaptureIssue::SdkFieldsOnly => Some(issue),
        (current, incoming) => current.or(incoming),
    };
    // Reserve enough space for an explicit capture-failure marker. Never delete accepted facts.
    let bytes = serde_json::to_vec(&candidate).map_or(usize::MAX, |bytes| bytes.len());
    if candidate.reports.len() > bcode_session_models::MAX_ORIGINAL_USAGE_REPORTS
        || bytes > bcode_session_models::MAX_ORIGINAL_USAGE_BYTES.saturating_sub(64)
    {
        current.capture_issue = Some(UsageCaptureIssue::LimitExceeded);
    } else if candidate.validate().is_err() {
        current.capture_issue = Some(UsageCaptureIssue::UnsafeOrMalformed);
    } else {
        *current = candidate;
    }
}
