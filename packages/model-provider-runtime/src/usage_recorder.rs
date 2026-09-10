//! Request-scoped capture and publication, independent of provider transport.

#[cfg(test)]
mod tests;

use bcode_model::{
    ModelTurnRequest, ProviderTurnEvent, TokenUsage, UsageCaptureSpec, UsageDecoder,
};
use bcode_session_models::{OriginalUsage, OriginalUsageReport, UsageCaptureIssue};
use std::collections::BTreeMap;

/// The result of inspecting a protocol frame before typed decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageObservation {
    /// No billing report was present.
    Absent,
    /// A billing report was retained.
    Captured,
    /// Evidence could not be retained safely; capture status records the failure.
    Rejected,
}

/// Captures original facts and emits one normalized usage outcome per request attempt.
/// Each provider owns a decoder; this type owns budgets, ordering and lifecycle.
pub struct UsageRecorder<'a> {
    decoder: &'a dyn UsageDecoder,
    original: OriginalUsage,
    normalized: Option<TokenUsage>,
    normalization_failed: bool,
    capture_enabled: bool,
    published: bool,
}

impl std::fmt::Debug for UsageRecorder<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsageRecorder")
            .field("original", &self.original)
            .field("published", &self.published)
            .finish_non_exhaustive()
    }
}

impl<'a> UsageRecorder<'a> {
    /// Create a recorder with explicit capture opt-in and already resolved billing settings.
    #[must_use]
    pub fn new(
        provider: &str,
        decoder: &'a dyn UsageDecoder,
        requested: BTreeMap<String, String>,
        capture_enabled: bool,
    ) -> Self {
        let mut original = OriginalUsage {
            provider_id: provider.into(),
            api_shape: decoder.capture_spec().api_shape.into(),
            requested,
            capture_issue: Some(UsageCaptureIssue::UnsafeOrMalformed),
            ..Default::default()
        };
        if original.validate().is_err() {
            original.requested.clear();
        } else {
            original.capture_issue = None;
        }
        Self {
            decoder,
            original,
            normalized: None,
            normalization_failed: false,
            capture_enabled,
            published: false,
        }
    }

    /// Create a recorder using the host's existing capture opt-in.
    #[must_use]
    pub fn for_request(
        provider: &str,
        decoder: &'a dyn UsageDecoder,
        request: &ModelTurnRequest,
        requested: BTreeMap<String, String>,
    ) -> Self {
        Self::new(
            provider,
            decoder,
            requested,
            request
                .metadata
                .get(bcode_model::CAPTURE_ORIGINAL_USAGE_METADATA_KEY)
                .is_some_and(|value| value == "true"),
        )
    }

    /// Inspect billing data before typed response decoding. Unknown fields remain untouched.
    pub fn observe_json(&mut self, json: &str) -> UsageObservation {
        if self.published {
            return UsageObservation::Absent;
        }
        match extract_usage_report(json, self.decoder.capture_spec()) {
            Ok(None) => UsageObservation::Absent,
            Ok(Some(report)) => self.observe_report(report),
            Err(issue) => {
                self.normalization_failed = true;
                self.normalized = None;
                self.original.capture_issue = self.original.capture_issue.or(Some(issue));
                UsageObservation::Rejected
            }
        }
    }

    /// Accept a report from a protocol-specific SDK adapter without pretending it was raw wire JSON.
    pub fn observe_sdk(&mut self, report: OriginalUsageReport) -> UsageObservation {
        if self.published {
            return UsageObservation::Absent;
        }
        let result = self.observe_report(report);
        self.original.capture_issue = self
            .original
            .capture_issue
            .or(Some(UsageCaptureIssue::SdkFieldsOnly));
        result
    }

    fn observe_report(&mut self, report: OriginalUsageReport) -> UsageObservation {
        if self.published {
            return UsageObservation::Absent;
        }
        let complete = self
            .decoder
            .capture_spec()
            .complete_sources
            .contains(&report.source.as_str());
        let incoming = OriginalUsage {
            provider_id: self.original.provider_id.clone(),
            api_shape: self.original.api_shape.clone(),
            requested: self.original.requested.clone(),
            reports: vec![report],
            complete,
            ..Default::default()
        };
        if let Ok(usage) = self.decoder.observe(self.normalized.as_ref(), &incoming) {
            self.normalized = Some(usage);
            self.normalization_failed = false;
        } else {
            self.normalization_failed = true;
            self.normalized = None;
        }
        let mut current = Some(std::mem::take(&mut self.original));
        super::append_usage_capture(&mut current, incoming);
        self.original = current.unwrap_or_default();
        if self
            .original
            .capture_issue
            .is_some_and(|issue| issue != UsageCaptureIssue::SdkFieldsOnly)
        {
            UsageObservation::Rejected
        } else {
            UsageObservation::Captured
        }
    }

    /// Read normalized usage without publishing or changing the original evidence.
    ///
    /// # Errors
    ///
    /// Returns an error for missing or unsupported protocol data. Capture completeness is separate:
    /// valid normalized facts may remain available when retaining the raw report exceeded policy.
    pub fn normalized(&self) -> Result<TokenUsage, String> {
        if self.normalization_failed {
            return Err("usage normalization failed".into());
        }
        let mut usage = self.normalized.clone().ok_or("no normalized usage")?;
        apply_finality(&mut usage, self.original.complete);
        Ok(usage)
    }

    /// Publish original evidence before normalized usage, exactly once. An interrupted outcome
    /// cannot infer final output from an initial zero. Returns normalization even with capture off.
    pub fn finish(
        &mut self,
        complete: bool,
        mut emit: impl FnMut(ProviderTurnEvent),
    ) -> Option<TokenUsage> {
        if self.published {
            return None;
        }
        self.published = true;
        if self.original.reports.is_empty() && self.original.capture_issue.is_none() {
            return None;
        }
        let complete = self.original.complete || complete;
        self.original.complete = complete;
        let normalized = self.normalized().ok();
        if self.capture_enabled {
            emit(ProviderTurnEvent::OriginalUsage {
                original: Box::new(self.original.clone()),
            });
        }
        if let Some(usage) = &normalized {
            emit(ProviderTurnEvent::Usage {
                usage: usage.clone(),
            });
        }
        normalized
    }

    /// Finish an interrupted recorder when a transport future exits or is dropped.
    /// Only publication is centralized; this guard never waits for provider work.
    #[must_use]
    pub const fn scoped<F: Fn(ProviderTurnEvent)>(self, emit: F) -> ScopedUsageRecorder<'a, F> {
        ScopedUsageRecorder {
            recorder: self,
            emit,
        }
    }

    /// Apply transport opt-in without duplicating the capture policy in provider code.
    pub const fn set_capture_enabled(&mut self, enabled: bool) {
        self.capture_enabled = enabled;
    }

    /// Report whether a complete upstream usage source was observed.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.original.complete
    }
}

/// RAII finalization for transport scopes, retaining evidence on cancellation or decode errors.
pub struct ScopedUsageRecorder<'a, F: Fn(ProviderTurnEvent)> {
    recorder: UsageRecorder<'a>,
    emit: F,
}
impl<F: Fn(ProviderTurnEvent)> Drop for ScopedUsageRecorder<'_, F> {
    fn drop(&mut self) {
        self.recorder.finish(false, &self.emit);
    }
}
impl<'a, F: Fn(ProviderTurnEvent)> std::ops::Deref for ScopedUsageRecorder<'a, F> {
    type Target = UsageRecorder<'a>;
    fn deref(&self) -> &Self::Target {
        &self.recorder
    }
}
impl<F: Fn(ProviderTurnEvent)> std::ops::DerefMut for ScopedUsageRecorder<'_, F> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.recorder
    }
}

/// Shared offline dispatch: identity validation and decoder selection are not plugin boilerplate.
///
/// # Errors
///
/// Returns an error for unsupported API shape, foreign evidence or failed normalization.
pub fn normalize_registered_usage(
    provider: &str,
    decoders: &[&dyn UsageDecoder],
    original: &OriginalUsage,
) -> Result<TokenUsage, String> {
    let mut matching = decoders
        .iter()
        .filter(|decoder| decoder.capture_spec().api_shape == original.api_shape);
    let decoder = matching.next().ok_or("unsupported usage API shape")?;
    if matching.next().is_some() {
        return Err("ambiguous usage decoder registration".into());
    }
    normalize_original(provider, *decoder, original)
}

fn normalize_original(
    provider: &str,
    decoder: &dyn UsageDecoder,
    original: &OriginalUsage,
) -> Result<TokenUsage, String> {
    original.validate()?;
    if original.provider_id != provider || original.api_shape != decoder.capture_spec().api_shape {
        return Err("usage attribution mismatch".into());
    }
    if original
        .capture_issue
        .is_some_and(|issue| issue != UsageCaptureIssue::SdkFieldsOnly)
    {
        return Err("original usage capture incomplete".into());
    }
    let mut usage = decoder.normalize(original)?;
    apply_finality(&mut usage, original.complete);
    Ok(usage)
}

fn apply_finality(usage: &mut TokenUsage, complete: bool) {
    if !complete {
        usage.output_tokens = None;
        usage.total_tokens = None;
        usage.details = std::mem::take(&mut usage.details)
            .into_vec()
            .into_iter()
            .filter(|detail| detail.bucket != bcode_model::ModelPricingBucket::Output)
            .collect::<Vec<_>>()
            .into_boxed_slice();
    }
}

/// Returns a protocol extraction result without interpreting provider identity.
fn extract_usage_report(
    json: &str,
    spec: UsageCaptureSpec,
) -> Result<Option<OriginalUsageReport>, UsageCaptureIssue> {
    let event: BTreeMap<&str, &serde_json::value::RawValue> =
        serde_json::from_str(json).map_err(|_| UsageCaptureIssue::UnsafeOrMalformed)?;
    let source = event
        .get("type")
        .map(|raw| serde_json::from_str::<String>(raw.get()))
        .transpose()
        .map_err(|_| UsageCaptureIssue::UnsafeOrMalformed)?
        .unwrap_or_else(|| spec.default_source.into());
    for path in spec.containers {
        let mut container = event.clone();
        let mut found = true;
        for key in *path {
            let Some(raw) = container.get(key) else {
                found = false;
                break;
            };
            container = serde_json::from_str(raw.get())
                .map_err(|_| UsageCaptureIssue::UnsafeOrMalformed)?;
        }
        if !found {
            continue;
        }
        let Some(raw) = container.get("usage") else {
            continue;
        };
        if raw.get() == "null" {
            return Ok(None);
        }
        // Bound the owned copy and typed normalization before allocating provider-controlled data.
        if raw.get().len() > bcode_session_models::MAX_ORIGINAL_USAGE_BYTES {
            return Err(UsageCaptureIssue::LimitExceeded);
        }
        let confirmed = spec
            .confirmed_fields
            .iter()
            .copied()
            .filter_map(|key| {
                container
                    .get(key)
                    .filter(|raw| raw.get() != "null")
                    .map(|raw| (key, raw))
            })
            .map(|(key, raw)| {
                serde_json::from_str::<String>(raw.get())
                    .map(|value| (key.into(), value))
                    .map_err(|_| UsageCaptureIssue::UnsafeOrMalformed)
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        return Ok(Some(OriginalUsageReport {
            source,
            usage_json: raw.get().into(),
            confirmed,
        }));
    }
    Ok(None)
}
