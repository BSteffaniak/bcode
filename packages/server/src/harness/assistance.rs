//! Live host consent for parent assistance. Canonical open retains provenance, not bearer authority.
//!
//! This bounded registry is deliberately not a recovery journal. Restart loses live consent and
//! fails closed; retained grants cannot recreate it. Every use also verifies both canonical owners.

use super::{HarnessOwner, ManagedHarnessError};
use bcode_harness_models::HarnessAssistanceGrant;
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::{Mutex, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

/// Host policy boundary for live assistance consent, not a portable grant validator.
///
/// Implementations must reauthorize the retained authenticated caller for both full current
/// owners and prove the parent's current eligibility to receive/respond to assistance.
/// Parentage, workspace equality and grant shape are not eligibility evidence.
/// No implementation is installed by default; missing policy keeps assistance disabled.
pub trait ParentAssistanceAuthorization: Send + Sync {
    /// Recheck live caller authorization and parent execution/interaction eligibility.
    /// # Errors
    /// Rejects cancelled/ineligible execution, expired caller authority and stale ownership.
    fn authorize<'a>(
        &'a self,
        child: &'a HarnessOwner,
        parent: &'a HarnessOwner,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), ManagedHarnessError>> + Send + 'a>,
    >;
}

const MAX_LIVE_GRANTS: usize = 128;

pub(super) struct AssistanceConsent {
    pub grant: HarnessAssistanceGrant,
    pub child: HarnessOwner,
    pub parent: HarnessOwner,
    pub gate: Arc<RwLock<bool>>,
    pub authorization: Arc<dyn ParentAssistanceAuthorization>,
}

#[derive(Default)]
pub(super) struct AssistanceRegistry {
    grants: Mutex<BTreeMap<String, Arc<AssistanceConsent>>>,
}

impl AssistanceRegistry {
    pub async fn issue(
        &self,
        child: HarnessOwner,
        parent: HarnessOwner,
        grant: HarnessAssistanceGrant,
        authorization: Arc<dyn ParentAssistanceAuthorization>,
    ) -> Result<HarnessAssistanceGrant, ManagedHarnessError> {
        let mut grants = self.grants.lock().await;
        let key = child.session_id.to_string();
        if let Some(existing) = grants.get(&key) {
            if existing.child != child || existing.parent != parent || existing.grant != grant {
                return Err(ManagedHarnessError::Conflict);
            }
            if !*existing.gate.read().await {
                return Err(ManagedHarnessError::StaleOwner);
            }
            return Ok(existing.grant.clone());
        }
        if grants.len() >= MAX_LIVE_GRANTS {
            return Err(ManagedHarnessError::Unavailable);
        }
        grant.validate().map_err(|_| ManagedHarnessError::Invalid)?;
        if grant.child_session_id != key
            || grant.child_generation != child.generation
            || grant.parent_session_id != parent.session_id.to_string()
            || grant.parent_generation != parent.generation
        {
            return Err(ManagedHarnessError::Invalid);
        }
        grants.insert(
            key,
            Arc::new(AssistanceConsent {
                grant: grant.clone(),
                child,
                parent,
                gate: Arc::new(RwLock::new(true)),
                authorization,
            }),
        );
        drop(grants);
        Ok(grant)
    }

    pub async fn exact(
        &self,
        child: &HarnessOwner,
        grant: &HarnessAssistanceGrant,
    ) -> Result<Arc<AssistanceConsent>, ManagedHarnessError> {
        let consent = self
            .grants
            .lock()
            .await
            .get(&child.session_id.to_string())
            .cloned()
            .ok_or(ManagedHarnessError::StaleOwner)?;
        if consent.child != *child || consent.grant != *grant {
            return Err(ManagedHarnessError::StaleOwner);
        }
        Ok(consent)
    }

    pub async fn read(
        consent: &AssistanceConsent,
    ) -> Result<OwnedRwLockReadGuard<bool>, ManagedHarnessError> {
        let guard = consent.gate.clone().read_owned().await;
        if !*guard {
            return Err(ManagedHarnessError::StaleOwner);
        }
        Ok(guard)
    }

    /// Revocation tombstones remain bounded by the same admitted-live-session limit. They are
    /// never evicted into new authority; a later issuance for this child cannot silently reactivate.
    pub async fn revoke(
        &self,
        session: bcode_session_models::SessionId,
    ) -> Vec<OwnedRwLockWriteGuard<bool>> {
        let consents = self
            .grants
            .lock()
            .await
            .values()
            .filter(|v| v.child.session_id == session || v.parent.session_id == session)
            .cloned()
            .collect::<Vec<_>>();
        let mut guards = Vec::with_capacity(consents.len());
        for consent in consents {
            let mut guard = consent.gate.clone().write_owned().await;
            *guard = false;
            guards.push(guard);
        }
        guards
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::SessionId;
    fn owner() -> HarnessOwner {
        HarnessOwner {
            session_id: SessionId::new(),
            generation: 1,
            artifact_id: "artifact".into(),
            daemon_id: "daemon".into(),
        }
    }

    fn grant(child: &HarnessOwner, parent: &HarnessOwner) -> HarnessAssistanceGrant {
        HarnessAssistanceGrant {
            admission_id: "host-admission".into(),
            child_session_id: child.session_id.to_string(),
            child_generation: child.generation,
            parent_session_id: parent.session_id.to_string(),
            parent_generation: parent.generation,
            scope: "bounded help".into(),
        }
    }

    struct Allowed;
    impl ParentAssistanceAuthorization for Allowed {
        fn authorize<'a>(
            &'a self,
            _: &'a HarnessOwner,
            _: &'a HarnessOwner,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), ManagedHarnessError>> + Send + 'a>,
        > {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn caller_dto_cannot_replace_host_issuance_and_restart_does_not_restore_it() {
        let registry = AssistanceRegistry::default();
        let child = owner();
        let parent = owner();
        let grant = registry
            .issue(
                child.clone(),
                parent.clone(),
                grant(&child, &parent),
                Arc::new(Allowed),
            )
            .await
            .unwrap();
        let consent = registry.exact(&child, &grant).await.unwrap();
        assert!(AssistanceRegistry::read(&consent).await.is_ok());
        let mut forged = grant.clone();
        forged.parent_generation += 1;
        assert!(registry.exact(&child, &forged).await.is_err());
        forged = grant.clone();
        forged.scope = "approve all native tools".into();
        assert!(registry.exact(&child, &forged).await.is_err());
        assert!(
            AssistanceRegistry::default()
                .exact(&child, &grant)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn cancellation_of_either_owner_revokes_without_reissuance() {
        let registry = AssistanceRegistry::default();
        let child = owner();
        let parent = owner();
        let grant = registry
            .issue(
                child.clone(),
                parent.clone(),
                grant(&child, &parent),
                Arc::new(Allowed),
            )
            .await
            .unwrap();
        let consent = registry.exact(&child, &grant).await.unwrap();
        drop(registry.revoke(parent.session_id).await);
        assert!(AssistanceRegistry::read(&consent).await.is_err());
        assert!(
            registry
                .issue(child, parent, grant, Arc::new(Allowed))
                .await
                .is_err()
        );
    }
}
