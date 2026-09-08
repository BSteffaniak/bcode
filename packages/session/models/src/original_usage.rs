//! Original provider billing observations retained independently of normalization.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Maximum retained billing data per provider request.
pub const MAX_ORIGINAL_USAGE_BYTES: usize = 64 * 1024;
/// Maximum independently reported usage fragments per request.
pub const MAX_ORIGINAL_USAGE_REPORTS: usize = 64;

/// Billing-only provider evidence. Never include this in frontend snapshots or model context.
/// The enclosing provider/session contracts define compatibility; this data has no own version.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginalUsage {
    /// Provider plugin responsible for interpreting the reports.
    pub provider_id: String,
    /// Provider API shape, such as `responses`, `messages`, or `converse_sdk`.
    pub api_shape: String,
    /// Request billing settings, kept distinct from confirmed response fields.
    #[serde(default)]
    pub requested: BTreeMap<String, String>,
    /// Original reports in provider order, not a merged interpretation.
    #[serde(default)]
    pub reports: Vec<OriginalUsageReport>,
    /// Whether the provider completed its usage reporting.
    #[serde(default)]
    pub complete: bool,
    /// Capture was incomplete; missing bytes must not be guessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_issue: Option<UsageCaptureIssue>,
}

/// An upstream usage object captured before typed token deserialization.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginalUsageReport {
    /// Source event type, distinguishing initial and terminal observations.
    pub source: String,
    /// Original JSON usage subtree, retaining numeric spellings and unknown fields.
    pub usage_json: String,
    /// Confirmed billing labels outside the upstream usage object.
    #[serde(default)]
    pub confirmed: BTreeMap<String, String>,
}

/// Why complete original billing evidence is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageCaptureIssue {
    /// The report exceeded the per-request byte, depth, or fragment budget.
    LimitExceeded,
    /// Billing data could not be isolated or decoded safely.
    UnsafeOrMalformed,
    /// An upstream SDK exposes typed fields only; unknown wire fields may have been lost.
    SdkFieldsOnly,
}

impl std::fmt::Debug for OriginalUsage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalUsage")
            .field("reports", &self.reports.len())
            .field("complete", &self.complete)
            .field("capture_issue", &self.capture_issue)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for OriginalUsageReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalUsageReport")
            .field("bytes", &self.usage_json.len())
            .finish_non_exhaustive()
    }
}

impl OriginalUsage {
    /// Validate capture budgets and reject non-billing/secret-bearing structures.
    ///
    /// # Errors
    ///
    /// Rejects excessive reports, bytes or depth, unsafe labels, and malformed/non-numeric usage.
    pub fn validate(&self) -> Result<(), String> {
        if self.reports.is_empty() && self.capture_issue.is_none() {
            return Err("original usage has no observations".into());
        }
        if self.reports.len() > MAX_ORIGINAL_USAGE_REPORTS
            || serde_json::to_vec(self)
                .map_err(|_| "invalid original usage")?
                .len()
                > MAX_ORIGINAL_USAGE_BYTES
        {
            return Err("original usage exceeds capture budget".into());
        }
        if !safe_label(&self.provider_id)
            || !safe_label(&self.api_shape)
            || !safe_context(&self.requested)
        {
            return Err("invalid original usage identity or billing settings".into());
        }
        for report in &self.reports {
            if !safe_label(&report.source) || !safe_context(&report.confirmed) {
                return Err("unsafe original usage labels".into());
            }
            let value: Box<serde_json::value::RawValue> = serde_json::from_str(&report.usage_json)
                .map_err(|_| "invalid original usage JSON")?;
            if !value.get().trim_start().starts_with('{') || !safe_usage_json(value.get(), 0) {
                return Err("unsafe original usage subtree".into());
            }
        }
        Ok(())
    }
}
fn safe_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 256
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-:/".contains(&byte))
}
fn safe_context(context: &BTreeMap<String, String>) -> bool {
    context.len() <= 8
        && context.iter().all(|(key, value)| {
            matches!(
                key.as_str(),
                "model"
                    | "service_tier"
                    | "cache_ttl_seconds"
                    | "prompt_cache_retention"
                    | "billing_scope"
                    | "invocation_class"
            ) && safe_label(value)
        })
}
fn safe_usage_json(json: &str, depth: usize) -> bool {
    if depth > 32 {
        return false;
    }
    let json = json.trim();
    if json.starts_with('{') {
        let Ok(map) =
            serde_json::from_str::<BTreeMap<String, Box<serde_json::value::RawValue>>>(json)
        else {
            return false;
        };
        map.iter().all(|(key, value)| {
            safe_label(key)
                && !matches!(
                    key.to_ascii_lowercase().as_str(),
                    "authorization"
                        | "api_key"
                        | "access_token"
                        | "refresh_token"
                        | "password"
                        | "secret"
                        | "prompt"
                        | "content"
                )
                && if value.get().starts_with('"') {
                    matches!(
                        key.as_str(),
                        "model" | "service_tier" | "prompt_cache_retention"
                    ) && serde_json::from_str::<String>(value.get())
                        .is_ok_and(|label| safe_label(&label))
                } else {
                    safe_usage_json(value.get(), depth + 1)
                }
        })
    } else if json.starts_with('[') {
        serde_json::from_str::<Vec<Box<serde_json::value::RawValue>>>(json).is_ok_and(|items| {
            items
                .iter()
                .all(|item| safe_usage_json(item.get(), depth + 1))
        })
    } else {
        matches!(json, "null" | "true" | "false")
            || json
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'-')
    }
}
