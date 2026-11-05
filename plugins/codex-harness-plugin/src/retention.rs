//! Connection-local delivery receipts and canonical read-only hydration handoff.
use crate::transport::CodexError;
use bcode_harness_models::{
    HarnessCanonicalAck, HarnessHydrationReply, HarnessHydrationRequest, HarnessNativeReference,
    HarnessObservation, HarnessRetentionContext, HarnessRetentionPage, MAX_HARNESS_ITEMS,
    harness_observations_digest,
};
use std::collections::BTreeMap;

#[cfg(test)]
#[path = "retention_tests.rs"]
mod tests;

pub struct Retention {
    connection: String,
    delivered: Option<(HarnessRetentionPage, Vec<HarnessObservation>)>,
    acknowledged: Option<HarnessCanonicalAck>,
    requests: BTreeMap<HarnessNativeReference, HarnessHydrationRequest>,
    canonical_revision: u64,
    enabled: bool,
}
impl Default for Retention {
    fn default() -> Self {
        Self {
            connection: uuid::Uuid::new_v4().to_string(),
            delivered: None,
            acknowledged: None,
            requests: BTreeMap::new(),
            canonical_revision: 0,
            enabled: false,
        }
    }
}
impl Retention {
    pub(super) fn deliver(
        &mut self,
        observations: &[HarnessObservation],
    ) -> Result<HarnessRetentionPage, CodexError> {
        let page = HarnessRetentionPage {
            connection_id: self.connection.clone(),
            page_id: uuid::Uuid::new_v4().to_string(),
            observations_digest: harness_observations_digest(observations)
                .map_err(|_| CodexError::Bounds)?,
        };
        // Without retention-aware host, preserve legacy bounded behavior. With one,
        // the caller may not advance until its previous delivery has been committed.
        if self.enabled && self.delivered.is_some() {
            return Err(CodexError::Uncertain);
        }
        self.delivered = Some((page.clone(), observations.to_vec()));
        Ok(page)
    }
    pub(super) fn apply(
        &mut self,
        context: &HarnessRetentionContext,
        session: &str,
        generation: u64,
    ) -> Result<(Vec<HarnessObservation>, Vec<HarnessHydrationReply>), CodexError> {
        context
            .validate_for(session, generation)
            .map_err(|_| CodexError::Protocol)?;
        if context.connection_id != self.connection {
            return Err(CodexError::Stale);
        }
        // Validate the entire batch before changing receipt/cache state.
        if let Some(ack) = &context.acknowledgment
            && self.acknowledged.as_ref() != Some(ack)
        {
            let (page, _) = self.delivered.as_ref().ok_or(CodexError::Stale)?;
            if ack.page_id != page.page_id
                || ack.observations_digest != page.observations_digest
                || ack.canonical_revision < self.canonical_revision
            {
                return Err(CodexError::Stale);
            }
        }
        let minimum_revision = context
            .acknowledgment
            .as_ref()
            .map_or(self.canonical_revision, |ack| {
                self.canonical_revision.max(ack.canonical_revision)
            });
        for reply in &context.hydration {
            let request = self
                .requests
                .get(&reply.reference)
                .ok_or(CodexError::Stale)?;
            if reply.request_id != request.request_id || reply.canonical_revision < minimum_revision
            {
                return Err(CodexError::Stale);
            }
        }
        let mut observations = Vec::new();
        if let Some(ack) = &context.acknowledgment
            && self.acknowledged.as_ref() != Some(ack)
        {
            observations = self.delivered.take().ok_or(CodexError::Stale)?.1;
            self.canonical_revision = ack.canonical_revision;
            self.acknowledged = Some(ack.clone());
        }
        self.enabled = true;
        for reply in &context.hydration {
            self.requests.remove(&reply.reference);
        }
        Ok((observations, context.hydration.clone()))
    }
    #[must_use]
    pub(super) const fn ready(&self) -> bool {
        !self.enabled || self.delivered.is_none()
    }
    #[must_use]
    pub(super) const fn enabled(&self) -> bool {
        self.enabled
    }
    pub(super) fn request(&mut self, reference: HarnessNativeReference) -> Result<(), CodexError> {
        reference.validate().map_err(|_| CodexError::Protocol)?;
        if !self.requests.contains_key(&reference) {
            if self.requests.len() >= MAX_HARNESS_ITEMS {
                return Err(CodexError::Bounds);
            }
            self.requests.insert(
                reference.clone(),
                HarnessHydrationRequest {
                    request_id: uuid::Uuid::new_v4().to_string(),
                    reference,
                },
            );
        }
        Ok(())
    }
    #[must_use]
    pub(super) fn requests(&self) -> Vec<HarnessHydrationRequest> {
        self.requests.values().cloned().collect()
    }
}
