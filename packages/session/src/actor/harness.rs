//! Serialized managed harness operations. Owner comparison occurs inside the writer mailbox.
use super::{SessionActor, SessionError, SessionEvent, SessionEventKind};
use bcode_harness_models::{
    HarnessInteraction, HarnessObservationPage, HarnessReceipt, HarnessReconciliation,
};
use bcode_session_models::external_harness::{
    ExternalHarnessClaim, ExternalHarnessOperation, ExternalHarnessOperationRecord,
    ExternalHarnessOwner, ExternalHarnessResponseKind, ExternalHarnessState,
};

pub enum HarnessAction {
    State,
    Write(Box<ExternalHarnessOperationRecord>),
    Page {
        after: Option<String>,
        page: HarnessObservationPage,
    },
    Receipt {
        submission_id: String,
        attempt_id: String,
    },
    Interaction {
        request_id: String,
    },
}
pub enum HarnessResult {
    State(Option<Box<ExternalHarnessState>>),
    Claim(ExternalHarnessClaim),
    Committed,
    Receipt(Option<HarnessReceipt>),
    Interaction(Option<HarnessInteraction>),
}
fn failure() -> SessionError {
    SessionError::EventSerialization("managed harness owner or operation conflict".into())
}
fn op(event: &SessionEvent) -> Result<&ExternalHarnessOperation, SessionError> {
    if let SessionEventKind::ExternalHarnessOperation { operation } = &event.kind {
        Ok(operation)
    } else {
        Err(failure())
    }
}
impl SessionActor {
    pub(super) async fn harness_operation(
        &mut self,
        owner: ExternalHarnessOwner,
        action: HarnessAction,
        timestamp: u64,
    ) -> Result<HarnessResult, SessionError> {
        owner.validate().map_err(SessionError::EventSerialization)?;
        if owner.session_id != self.state.summary.id {
            return Err(failure());
        }
        self.ensure_ownership()?;
        self.store.as_ref().ok_or_else(failure)?;
        // Authenticate against the live actor's lease, not caller-supplied daemon strings.
        let lease = self.lease.as_ref().ok_or_else(failure)?.owner();
        if lease.build_fingerprint.as_deref() != Some(owner.artifact_id.as_str())
            || lease.daemon_instance_id.as_deref() != Some(owner.daemon_id.as_str())
        {
            return Err(failure());
        }
        self.ensure_session_db_for_write().await?;
        let db = self.db.as_ref().ok_or_else(failure)?;
        let state = db.harness_state(&owner).await?;
        let action = if let HarnessAction::Page { after, page } = action {
            let instance = state.as_ref().ok_or_else(failure)?.instance.clone();
            HarnessAction::Write(Box::new(ExternalHarnessOperationRecord::ObservationPage {
                instance,
                after,
                page,
            }))
        } else {
            action
        };
        match action {
            HarnessAction::Page { .. } => Err(failure()),
            HarnessAction::State => Ok(HarnessResult::State(state.map(Box::new))),
            HarnessAction::Receipt {
                submission_id,
                attempt_id,
            } => {
                state.ok_or_else(failure)?;
                let id = format!("{}:{}:{}", submission_id.len(), submission_id, attempt_id);
                let event = db.harness_latest(owner.session_id, "receipt", &id).await?;
                let receipt = event
                    .as_ref()
                    .map(op)
                    .transpose()?
                    .and_then(|op| match &op.record {
                        ExternalHarnessOperationRecord::DeliveryCompleted { response, .. } => {
                            match &response.kind {
                                ExternalHarnessResponseKind::Receipt(receipt) => {
                                    Some(receipt.clone())
                                }
                                ExternalHarnessResponseKind::Reconciled(
                                    HarnessReconciliation::Known { receipt, .. }
                                    | HarnessReconciliation::KnownPrepared { receipt, .. },
                                ) => receipt.clone(),
                                _ => None,
                            }
                        }
                        _ => None,
                    });
                Ok(HarnessResult::Receipt(receipt))
            }
            HarnessAction::Interaction { request_id } => {
                state.ok_or_else(failure)?;
                let interaction = if db
                    .harness_latest(owner.session_id, "answered", &request_id)
                    .await?
                    .is_some()
                {
                    None
                } else {
                    db.harness_interaction(owner.session_id, &request_id)
                        .await?
                };
                Ok(HarnessResult::Interaction(interaction))
            }
            HarnessAction::Write(record) => {
                self.write_harness_operation(owner, *record, state, timestamp)
                    .await
            }
        }
    }
    async fn write_harness_operation(
        &mut self,
        owner: ExternalHarnessOwner,
        record: ExternalHarnessOperationRecord,
        state: Option<ExternalHarnessState>,
        timestamp: u64,
    ) -> Result<HarnessResult, SessionError> {
        let db = self.db.as_ref().ok_or_else(failure)?;
        let operation = ExternalHarnessOperation {
            version: 1,
            owner: owner.clone(),
            record,
        };
        operation
            .validate()
            .map_err(SessionError::EventSerialization)?;
        if let Some(result) = retained_retry(db, &owner, state.as_ref(), &operation).await? {
            return Ok(result);
        }
        let claim = matches!(
            operation.record,
            ExternalHarnessOperationRecord::DeliveryClaimed { .. }
        );
        // Reuses canonical actor append: write lock, refreshed generation, transactional
        // preconditions + canonical event + locators + post-commit fanout.
        self.append_event(
            SessionEventKind::ExternalHarnessOperation {
                operation: Box::new(operation),
            },
            None,
            timestamp,
        )
        .await?;
        Ok(if claim {
            HarnessResult::Claim(ExternalHarnessClaim::New)
        } else {
            HarnessResult::Committed
        })
    }
}

async fn retained_retry(
    db: &super::SessionDb,
    owner: &ExternalHarnessOwner,
    state: Option<&ExternalHarnessState>,
    operation: &ExternalHarnessOperation,
) -> Result<Option<HarnessResult>, SessionError> {
    // All retries are compared inside this same serialized writer operation.
    match &operation.record {
        ExternalHarnessOperationRecord::Configured {
            instance,
            capabilities,
            open_request,
        } => {
            if let Some(state) = state {
                if state.instance != *instance
                    || state.capabilities != *capabilities
                    || state.prepared_open != *open_request
                {
                    return Err(failure());
                }
                return Ok(Some(HarnessResult::Committed));
            }
        }
        ExternalHarnessOperationRecord::DeliveryClaimed { request } => {
            state.ok_or_else(failure)?;
            if let Some(event) = db
                .harness_latest(owner.session_id, "claim", &request.request_id)
                .await?
            {
                let original = op(&event)?;
                if original.owner != *owner
                    || !matches!(&original.record,ExternalHarnessOperationRecord::DeliveryClaimed { request: previous } if previous==request)
                {
                    return Err(failure());
                }
                if let Some(event) = db
                    .harness_latest(owner.session_id, "completion", &request.request_id)
                    .await?
                {
                    if let ExternalHarnessOperationRecord::DeliveryCompleted { response, .. } =
                        &op(&event)?.record
                    {
                        return Ok(Some(HarnessResult::Claim(ExternalHarnessClaim::Recorded(
                            Box::new(response.clone()),
                        ))));
                    }
                    return Err(failure());
                }
                return Ok(Some(HarnessResult::Claim(ExternalHarnessClaim::Uncertain)));
            }
        }
        ExternalHarnessOperationRecord::DeliveryCompleted { request, .. } => {
            state.ok_or_else(failure)?;
            if let Some(event) = db
                .harness_latest(owner.session_id, "completion", &request.request_id)
                .await?
            {
                if op(&event)? != operation {
                    return Err(failure());
                }
                return Ok(Some(HarnessResult::Committed));
            }
        }
        ExternalHarnessOperationRecord::ObservationPage { after, .. } => {
            state.ok_or_else(failure)?;
            let id = format!(
                "{}:{}",
                owner.generation,
                serde_json::to_string(after).map_err(|_| failure())?
            );
            if let Some(event) = db
                .harness_latest(owner.session_id, "page_input", &id)
                .await?
            {
                if op(&event)? == operation {
                    return Ok(Some(HarnessResult::Committed));
                }
                // Empty unchanged-cursor long polls are replaceable observations only.
                if !matches!(&op(&event)?.record,ExternalHarnessOperationRecord::ObservationPage { page,.. } if page.observations.is_empty() && &page.next_after==after)
                {
                    return Err(failure());
                }
            }
        }
    }
    Ok(None)
}
