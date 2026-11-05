//! Exact post-commit page retention; cursors alone never transfer ownership.
use bcode_harness_models::{
    HarnessCanonicalAck, HarnessFailure, HarnessObservation, HarnessRetentionContext,
    HarnessRetentionPage, harness_observations_digest,
};

pub struct PageRetention {
    connection: String,
    serial: u64,
    revision: u64,
    pending: Option<(HarnessRetentionPage, Vec<HarnessObservation>)>,
    acknowledged: Option<HarnessCanonicalAck>,
}
impl PageRetention {
    pub const fn new(connection: String) -> Self {
        Self {
            connection,
            serial: 0,
            revision: 0,
            pending: None,
            acknowledged: None,
        }
    }
    pub const fn pending(&self) -> bool {
        self.pending.is_some()
    }
    pub fn apply(
        &mut self,
        context: Option<&HarnessRetentionContext>,
    ) -> Result<Vec<HarnessObservation>, HarnessFailure> {
        let Some(context) = context else {
            return Ok(Vec::new());
        };
        if context.connection_id != self.connection {
            return Err(HarnessFailure::StaleGeneration);
        }
        let Some(ack) = &context.acknowledgment else {
            return Ok(Vec::new());
        };
        if self.acknowledged.as_ref() == Some(ack) {
            return Ok(Vec::new());
        }
        let (page, _) = self.pending.as_ref().ok_or(HarnessFailure::Conflict)?;
        if ack.canonical_revision < self.revision
            || ack.canonical_revision == 0
            || page.page_id != ack.page_id
            || page.observations_digest != ack.observations_digest
        {
            return Err(HarnessFailure::Conflict);
        }
        self.revision = ack.canonical_revision;
        self.acknowledged = Some(ack.clone());
        Ok(self.pending.take().ok_or(HarnessFailure::Conflict)?.1)
    }
    pub fn offer(
        &mut self,
        observations: Vec<HarnessObservation>,
    ) -> Result<(HarnessRetentionPage, Vec<HarnessObservation>), HarnessFailure> {
        if let Some(page) = &self.pending {
            return Ok(page.clone());
        }
        self.serial = self
            .serial
            .checked_add(1)
            .ok_or(HarnessFailure::Unavailable)?;
        let page = HarnessRetentionPage {
            connection_id: self.connection.clone(),
            page_id: format!("{}:{}", self.connection, self.serial),
            observations_digest: harness_observations_digest(&observations)
                .map_err(|_| HarnessFailure::InvalidRequest)?,
        };
        self.pending = Some((page.clone(), observations.clone()));
        Ok((page, observations))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_exact_postcommit_ack_releases_page_and_duplicate_is_idempotent() {
        let mut state = PageRetention::new("connection".into());
        let (page, _) = state.offer(Vec::new()).unwrap();
        assert_eq!(state.apply(None).unwrap(), Vec::new());
        assert_eq!(state.offer(Vec::new()).unwrap().0, page);
        let ack = HarnessCanonicalAck {
            page_id: page.page_id.clone(),
            canonical_revision: 1,
            observations_digest: page.observations_digest.clone(),
        };
        let mut context = HarnessRetentionContext {
            connection_id: "wrong".into(),
            acknowledgment: Some(ack.clone()),
            hydration: Vec::new(),
        };
        assert!(state.apply(Some(&context)).is_err());
        context.connection_id = "connection".into();
        context.acknowledgment.as_mut().unwrap().observations_digest = "0".repeat(64);
        assert!(state.apply(Some(&context)).is_err());
        context.acknowledgment = Some(ack);
        state.apply(Some(&context)).unwrap();
        state.apply(Some(&context)).unwrap();
        assert_ne!(state.offer(Vec::new()).unwrap().0.page_id, page.page_id);
    }
}
