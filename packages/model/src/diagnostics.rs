//! Provider-owned sensitive diagnostic payload contracts.
//!
//! These are implementation contracts, not frontend events or persisted session history.
//! Adapters own their diagnostic projections and compatibility versions. Never implement this
//! trait for authentication, transport headers, configuration, or an unrestricted HTTP exchange.

use serde::{Deserialize, Serialize};

/// The provider boundary at which a diagnostic projection was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderDiagnosticStage {
    /// Final adapted request body, before sending; not proof of remote acceptance.
    RequestAdapted,
    /// One protocol frame observed before normalized stream conversion.
    StreamFrame,
    /// A decoded response, independent of host publication or durable settlement.
    ResponseDecoded,
    /// Provider billing evidence before normalization; not an additional usage charge.
    UsageReport,
}

impl ProviderDiagnosticStage {
    /// Return a fixed diagnostic event name.
    #[must_use]
    pub const fn event_name(self) -> &'static str {
        match self {
            Self::RequestAdapted => "request_adapted",
            Self::StreamFrame => "stream_frame",
            Self::ResponseDecoded => "response_decoded",
            Self::UsageReport => "usage_report",
        }
    }
}

/// An explicitly selected, credential-free provider diagnostic projection.
///
/// Implement this on a provider-owned DTO containing only the body fields needed for diagnosis.
/// `FORMAT` identifies the provider and wire shape; `SCHEMA_VERSION` versions this independently
/// evolving diagnostic representation. Consumers must reject unknown formats/versions rather
/// than interpret them as a normalized public contract. Payloads remain sensitive even after
/// credentials are excluded. Capture must be lazy, bounded and explicitly enabled by an observer.
/// Serialization must not perform network, storage, or other execution side effects.
///
/// This interface deliberately does not require or expose an X-ray engine dependency.
pub trait ProviderDiagnosticPayload: Serialize {
    /// Stable provider-owned format name, not a URL, model, account, or request identity.
    const FORMAT: &'static str;
    /// Positive compatibility version of this projection.
    const SCHEMA_VERSION: u16;
    /// Boundary represented by this payload.
    const STAGE: ProviderDiagnosticStage;
}
