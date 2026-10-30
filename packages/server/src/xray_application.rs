//! Authorized, transport-neutral admission to live session diagnostics.
//!
//! A diagnostic grant is not execution authority. This facade never submits, resumes,
//! cancels, repairs, reconstructs, or takes ownership of a session. Callers must obtain
//! a session-scoped grant from their authenticated application boundary before admission.

use std::collections::BTreeSet;
use std::sync::Arc;

use bcode_session::SessionHealth;
use bcode_session_models::SessionId;
use bcode_xray::{XrayHub, XrayObservation};
use bcode_xray_models::XrayObserveRequest;

use crate::ServerState;

/// Maximum number of explicitly selected sessions in one diagnostic grant.
pub const MAX_XRAY_GRANTED_SESSIONS: usize = 256;

/// Explicit authority to observe selected sessions, optionally including sensitive capture.
///
/// There is deliberately no wildcard, filesystem root, parent-session inheritance, or
/// cross-location discovery. Construct this only after the caller's authorization decision.
#[derive(Clone)]
pub struct XrayAccessGrant {
    sessions: BTreeSet<SessionId>,
    deep_capture: bool,
}

impl XrayAccessGrant {
    /// Record an already-authorized diagnostic scope. Deep capture requires a separate,
    /// explicit sensitive-data decision; possession of ordinary session access is insufficient.
    ///
    /// # Errors
    /// Returns [`XrayApplicationError::InvalidRequest`] for an empty or oversized scope.
    pub fn authorized(
        sessions: BTreeSet<SessionId>,
        deep_capture: bool,
    ) -> Result<Self, XrayApplicationError> {
        if sessions.is_empty() || sessions.len() > MAX_XRAY_GRANTED_SESSIONS {
            return Err(XrayApplicationError::InvalidRequest);
        }
        Ok(Self {
            sessions,
            deep_capture,
        })
    }

    pub(crate) fn authorize(
        &self,
        request: &XrayObserveRequest,
    ) -> Result<SessionId, XrayApplicationError> {
        if request.schema_version != 1 || request.session_id.len() > 36 {
            return Err(XrayApplicationError::InvalidRequest);
        }
        let session_id = request
            .session_id
            .parse::<SessionId>()
            .map_err(|_| XrayApplicationError::InvalidRequest)?;
        // One canonical spelling prevents producer and observer identity mismatches.
        if session_id.to_string() != request.session_id {
            return Err(XrayApplicationError::InvalidRequest);
        }
        if !self.sessions.contains(&session_id) || (request.deep_capture && !self.deep_capture) {
            return Err(XrayApplicationError::Forbidden);
        }
        Ok(session_id)
    }
}

/// Secret-safe errors: never expose storage paths, hub internals, or captured content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum XrayApplicationError {
    /// Unsupported schema, noncanonical identity, or invalid scope.
    #[error("invalid X-ray observation request")]
    InvalidRequest,
    /// The selected session or sensitive capture is outside the authenticated scope.
    #[error("X-ray diagnostic access is not authorized")]
    Forbidden,
    /// Canonical session health is missing, degraded, or unavailable.
    #[error("X-ray session access is unavailable; check session storage status")]
    Unavailable,
    /// The engine could not admit another bounded live observation.
    #[error("X-ray observation could not be admitted")]
    NotAdmitted,
}

impl XrayApplicationError {
    /// Stable error code for transport adapters.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidRequest => "xray_invalid_request",
            Self::Forbidden => "xray_forbidden",
            Self::Unavailable => "xray_session_unavailable",
            Self::NotAdmitted => "xray_not_admitted",
        }
    }
}

/// Session-owned application boundary shared by local HTTP and other authorized clients.
///
/// The coordinator must supply the same hub used by runtime instrumentation, not a new
/// per-transport hub. The `ServerState` fixes the canonical state location.
#[derive(Clone)]
pub struct XrayApplication {
    state: Arc<ServerState>,
    hub: Arc<XrayHub>,
}

impl XrayApplication {
    /// Bind live diagnostics to an existing server and its instrumentation hub.
    #[must_use]
    pub const fn new(state: Arc<ServerState>, hub: Arc<XrayHub>) -> Self {
        Self { state, hub }
    }

    /// Read a bounded retained payload from an already-authorized sensitive observation.
    /// This accesses only transient hub retention, never canonical storage or reconstruction.
    ///
    /// # Errors
    /// Rejects foreign/shallow scopes, invalid ranges, unavailable observations and payloads.
    pub fn read_payload(
        &self,
        grant: &XrayAccessGrant,
        request: &bcode_xray_models::XrayReadPayloadRequest,
    ) -> Result<bcode_xray_models::XrayPayloadRange, XrayApplicationError> {
        grant.authorize(&XrayObserveRequest {
            schema_version: request.schema_version,
            session_id: request.session_id.clone(),
            deep_capture: true,
        })?;
        if request.observation_id.len() > 128
            || request.payload_id.len() > 128
            || request.length == 0
            || request.length > 1024 * 1024
            || request
                .offset
                .checked_add(u64::from(request.length))
                .is_none()
        {
            return Err(XrayApplicationError::InvalidRequest);
        }
        self.hub
            .read_payload(request)
            .map_err(|_| XrayApplicationError::Unavailable)
    }

    /// Authorize scope, check bounded current session health, then attach while idle or active.
    ///
    /// The engine's first envelope is the readiness boundary. HTTP headers alone do not
    /// establish readiness. No canonical events or retained sensitive traces are read here.
    ///
    /// # Errors
    /// Returns a fixed public error for invalid scope, unavailable canonical session access,
    /// failed storage admission/retirement, or rejected engine admission.
    pub async fn observe(
        &self,
        grant: &XrayAccessGrant,
        request: XrayObserveRequest,
    ) -> Result<XrayObservation, XrayApplicationError> {
        let session_id = grant.authorize(&request)?;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let mut admission = crate::storage_read_admission::RegisteredStorageRead::for_session(
            &self.state,
            session_id,
        )
        .await
        .map_err(|_| XrayApplicationError::Unavailable)?;
        let available = matches!(
            self.state.sessions.session_health(session_id).await,
            SessionHealth::Ready
        );
        // This is a health probe, not a content export. Retire its registration before
        // enabling capture; no storage read or lease is kept alive by the observation.
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(admission) = admission.take() {
            admission
                .complete()
                .await
                .map_err(|_| XrayApplicationError::Unavailable)?;
        }
        if !available {
            return Err(XrayApplicationError::Unavailable);
        }
        self.hub
            .observe(request)
            .map_err(|_| XrayApplicationError::NotAdmitted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(session_id: SessionId, deep_capture: bool) -> XrayObserveRequest {
        XrayObserveRequest {
            schema_version: 1,
            session_id: session_id.to_string(),
            deep_capture,
        }
    }

    #[test]
    fn grant_is_session_scoped_and_sensitive_capture_is_separate() {
        let selected = SessionId::new();
        let other = SessionId::new();
        let grant = XrayAccessGrant::authorized(BTreeSet::from([selected]), false).unwrap();
        assert_eq!(grant.authorize(&request(selected, false)), Ok(selected));
        assert_eq!(
            grant.authorize(&request(other, false)),
            Err(XrayApplicationError::Forbidden)
        );
        assert_eq!(
            grant.authorize(&request(selected, true)),
            Err(XrayApplicationError::Forbidden)
        );
        let sensitive = XrayAccessGrant::authorized(BTreeSet::from([selected]), true).unwrap();
        assert_eq!(sensitive.authorize(&request(selected, true)), Ok(selected));
    }

    #[test]
    fn rejects_future_versions_and_alternate_identity_spellings() {
        let selected: SessionId = "abcdef01-abcd-4abc-8abc-abcdef012345".parse().unwrap();
        let grant = XrayAccessGrant::authorized(BTreeSet::from([selected]), false).unwrap();
        let mut input = request(selected, false);
        input.schema_version = 2;
        assert_eq!(
            grant.authorize(&input),
            Err(XrayApplicationError::InvalidRequest)
        );
        input.schema_version = 1;
        input.session_id.make_ascii_uppercase();
        assert_eq!(
            grant.authorize(&input),
            Err(XrayApplicationError::InvalidRequest)
        );
    }

    #[test]
    fn oversized_scope_is_rejected() {
        let sessions = (0..=MAX_XRAY_GRANTED_SESSIONS)
            .map(|_| SessionId::new())
            .collect();
        assert!(XrayAccessGrant::authorized(sessions, false).is_err());
    }

    #[test]
    fn empty_scope_never_means_all_sessions() {
        assert!(XrayAccessGrant::authorized(BTreeSet::new(), true).is_err());
    }
}
