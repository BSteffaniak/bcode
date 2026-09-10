//! Provider-neutral usage decoder contracts. Transport and capture lifecycle live in provider runtime.

use crate::TokenUsage;
use bcode_session_models::OriginalUsage;

/// Paths and event labels identifying billing reports in one protocol.
#[derive(Debug, Clone, Copy)]
pub struct UsageCaptureSpec {
    /// Stable shape used in stored evidence; not a format version.
    pub api_shape: &'static str,
    /// Candidate parent paths containing `usage`, in precedence order.
    pub containers: &'static [&'static [&'static str]],
    /// Event type used when the protocol omits a `type` field.
    pub default_source: &'static str,
    /// Billing labels outside `usage` that may be retained, declared by this protocol.
    pub confirmed_fields: &'static [&'static str],
    /// Source labels that declare complete usage.
    pub complete_sources: &'static [&'static str],
}

/// Pure protocol interpretation shared by live capture and offline repricing.
pub trait UsageDecoder: Send + Sync {
    /// Describe where billing data lives, without inspecting provider identity in the host.
    fn capture_spec(&self) -> UsageCaptureSpec;
    /// Normalize one live report using the prior normalized result when this protocol sends
    /// cumulative partial observations. The default is a self-contained final report.
    ///
    /// # Errors
    ///
    /// Returns an error if the report cannot be interpreted without guessing.
    fn observe(
        &self,
        _previous: Option<&TokenUsage>,
        original: &OriginalUsage,
    ) -> Result<TokenUsage, String> {
        self.normalize(original)
    }

    /// Interpret ordered, validated billing reports. Never performs I/O.
    ///
    /// # Errors
    ///
    /// Returns an error when the reports do not establish supported normalized usage.
    fn normalize(&self, original: &OriginalUsage) -> Result<TokenUsage, String>;
}
