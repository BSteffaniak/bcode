//! Invocation-scoped reads of a delegated managed child session.
//!
//! These read contracts let an authenticated tool invocation of a child's immutable canonical
//! parent resolve the child's exact retained routing target and bounded canonical history.
//! Reading grants no mutation, replay, resume or transport authority: every later mutation is
//! still admitted by the host against canonical ownership, generation and the retained target.

use serde::{Deserialize, Serialize};

use crate::{
    HarnessContractError, HarnessInstance, HarnessObservationPage, HarnessSessionTarget,
    MAX_HARNESS_OBSERVATIONS, id,
};

/// Managed-session application operation returning [`ManagedDelegationBindingResponse`].
pub const MANAGED_DELEGATION_BINDING_OPERATION: &str = "binding";
/// Managed-session application operation returning a canonical [`HarnessObservationPage`].
pub const MANAGED_DELEGATION_HISTORY_OPERATION: &str = "history";

/// Request the exact retained routing target of one delegated child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedDelegationBindingRequest {
    /// Canonical Bcode child session identity.
    pub session_id: String,
}

impl ManagedDelegationBindingRequest {
    /// Validate bounded identity shape only.
    ///
    /// # Errors
    ///
    /// * Returns [`HarnessContractError::InvalidIdentity`] for an empty, oversized,
    ///   control-character or padded session identity.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        id(&self.session_id)
    }
}

/// Exact canonical routing evidence for one managed child session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedDelegationBinding {
    /// Canonical Bcode child session identity.
    pub session_id: String,
    /// Retained configured instance; never re-resolved from current configuration.
    pub instance: HarnessInstance,
    /// Retained prepared or confirmed target, including the current ownership generation.
    pub target: HarnessSessionTarget,
}

impl ManagedDelegationBinding {
    /// Validate exact target/session/instance agreement without promoting a prepared target.
    ///
    /// # Errors
    ///
    /// * Rejects malformed instances or targets.
    /// * Returns [`HarnessContractError::WrongOwner`] when the target names another session.
    /// * Returns [`HarnessContractError::InvalidShape`] when the target names another instance.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        self.instance.validate()?;
        self.target
            .validate_owner(&self.session_id, self.target.generation())?;
        if self.target.instance_id() != self.instance.instance_id {
            return Err(HarnessContractError::InvalidShape);
        }
        Ok(())
    }
}

/// Binding read result. `None` means no canonical session with that identity exists yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedDelegationBindingResponse {
    /// Exact retained routing evidence, when the session exists and is managed.
    pub binding: Option<ManagedDelegationBinding>,
}

impl ManagedDelegationBindingResponse {
    /// Validate the optional binding against the requested session.
    ///
    /// # Errors
    ///
    /// * Rejects malformed bindings.
    /// * Returns [`HarnessContractError::WrongOwner`] when the binding names another session.
    pub fn validate_for(
        &self,
        request: &ManagedDelegationBindingRequest,
    ) -> Result<(), HarnessContractError> {
        if let Some(binding) = &self.binding {
            binding.validate()?;
            if binding.session_id != request.session_id {
                return Err(HarnessContractError::WrongOwner);
            }
        }
        Ok(())
    }
}

/// Request one bounded page of a delegated child's canonical observation history.
///
/// The host may first commit at most one bounded native observation page using the child's
/// retained acknowledgement cursor. That drain is observation only and never a mutation replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedDelegationHistoryRequest {
    /// Canonical Bcode child session identity.
    pub session_id: String,
    /// Opaque canonical history cursor from a previous page.
    pub after: Option<String>,
    /// Maximum observations in the returned page.
    pub limit: u16,
}

impl ManagedDelegationHistoryRequest {
    /// Validate bounded identity, cursor and page size.
    ///
    /// # Errors
    ///
    /// * Returns [`HarnessContractError::InvalidIdentity`] for malformed identities or cursors.
    /// * Returns [`HarnessContractError::BoundsExceeded`] for a zero or excessive page size.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        id(&self.session_id)?;
        if let Some(cursor) = &self.after {
            id(cursor)?;
        }
        if self.limit == 0 || usize::from(self.limit) > MAX_HARNESS_OBSERVATIONS {
            return Err(HarnessContractError::BoundsExceeded);
        }
        Ok(())
    }

    /// Validate a returned page against this request.
    ///
    /// # Errors
    ///
    /// * Returns [`HarnessContractError::BoundsExceeded`] for oversized pages.
    /// * Returns [`HarnessContractError::WrongOwner`] for observations of another session.
    /// * Rejects malformed or duplicate observations and malformed cursors.
    pub fn validate_page(&self, page: &HarnessObservationPage) -> Result<(), HarnessContractError> {
        if page.observations.len() > usize::from(self.limit) {
            return Err(HarnessContractError::BoundsExceeded);
        }
        let mut seen = std::collections::BTreeSet::new();
        for observation in &page.observations {
            observation.validate()?;
            if observation.session_id != self.session_id {
                return Err(HarnessContractError::WrongOwner);
            }
            if !seen.insert(&observation.observation_id) {
                return Err(HarnessContractError::InvalidShape);
            }
        }
        if let Some(cursor) = &page.next_after {
            id(cursor)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HarnessAuthBinding, HarnessAuthCustody, HarnessPreparedSession};

    fn instance() -> HarnessInstance {
        HarnessInstance {
            instance_id: "instance".into(),
            plugin_id: "test.harness".into(),
            executable: "/explicit/harness".into(),
            config_namespace: "isolated".into(),
            auth: HarnessAuthBinding {
                provider_id: "test.auth".into(),
                profile: "native".into(),
                credential_namespace: "isolated".into(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        }
    }

    fn binding(session: &str, instance_id: &str) -> ManagedDelegationBinding {
        ManagedDelegationBinding {
            session_id: session.into(),
            instance: instance(),
            target: HarnessSessionTarget::Prepared(HarnessPreparedSession {
                open_id: "open".into(),
                session_id: session.into(),
                instance_id: instance_id.into(),
                generation: 1,
                native_version: "1.0.0".into(),
                requested_native_session_id: None,
            }),
        }
    }

    #[test]
    fn binding_must_name_the_requested_session_and_retained_instance() {
        let request = ManagedDelegationBindingRequest {
            session_id: "child".into(),
        };
        let mut response = ManagedDelegationBindingResponse {
            binding: Some(binding("child", "instance")),
        };
        assert_eq!(response.validate_for(&request), Ok(()));
        response.binding = Some(binding("other", "instance"));
        assert!(response.validate_for(&request).is_err());
        response.binding = Some(binding("child", "replacement"));
        assert!(response.validate_for(&request).is_err());
        response.binding = None;
        assert_eq!(response.validate_for(&request), Ok(()));
    }

    #[test]
    fn history_requests_and_pages_are_bounded_and_session_scoped() {
        let mut request = ManagedDelegationHistoryRequest {
            session_id: "child".into(),
            after: None,
            limit: 2,
        };
        assert_eq!(request.validate(), Ok(()));
        request.limit = 0;
        assert!(request.validate().is_err());
        request.limit = u16::try_from(MAX_HARNESS_OBSERVATIONS + 1).unwrap();
        assert!(request.validate().is_err());
        request.limit = 1;
        request.after = Some(" padded".into());
        assert!(request.validate().is_err());
        request.after = None;
        let observation = |session: &str, id: &str| crate::HarnessObservation {
            observation_id: id.into(),
            session_id: session.into(),
            generation: 1,
            native_correlations: Vec::new(),
            completeness: crate::HarnessCompleteness::Complete,
            kind: crate::HarnessObservationKind::Result {
                text: "done".into(),
            },
        };
        let status = crate::HarnessExecutionStatus {
            connectivity: crate::HarnessConnectivity::Unknown,
            activity: crate::HarnessActivity::Unknown,
            attention: crate::HarnessAttention::Unknown,
            continuation: crate::HarnessContinuation::Unknown,
            completeness: crate::HarnessCompleteness::Unknown,
        };
        let mut page = HarnessObservationPage {
            retention: None,
            hydration_requests: Vec::new(),
            inventory_page: None,
            observations: vec![observation("child", "one")],
            next_after: Some("one".into()),
            completeness: crate::HarnessCompleteness::Complete,
            status,
            live: Vec::new(),
        };
        assert_eq!(request.validate_page(&page), Ok(()));
        page.observations.push(observation("child", "two"));
        assert_eq!(
            request.validate_page(&page),
            Err(HarnessContractError::BoundsExceeded)
        );
        request.limit = 2;
        page.observations[1] = observation("other", "two");
        assert_eq!(
            request.validate_page(&page),
            Err(HarnessContractError::WrongOwner)
        );
    }
}
