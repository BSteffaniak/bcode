//! Actor authority boundary for original-turn continuation mutations.
use super::{SessionActor, SessionCommand, SessionHandle};
use crate::{SessionError, SessionLoadStatusKind};
use bcode_session_models::{
    SessionEvent, SessionEventKind, TurnContinuationKey, TurnContinuationOperation,
    TurnContinuationOperationRecord, TurnContinuationReceipt, TurnContinuationRecord,
};
use std::sync::Arc;
use tokio::sync::broadcast;

impl SessionHandle {
    pub async fn complete_interaction_with_continuation(
        &self,
        interaction: bcode_session_models::DurableInteractionOperation,
        settlement: bcode_session_models::TurnContinuationSettlement,
        guard: Arc<dyn crate::DurableInteractionCommitGuard>,
        timestamp: u64,
        mutations: broadcast::Sender<crate::SessionMutationCommitted>,
    ) -> Result<bcode_session_models::InteractionContinuationReceipt, SessionError> {
        self.send(|reply| SessionCommand::CompleteInteractionContinuation {
            interaction: Box::new(interaction),
            settlement,
            guard,
            timestamp,
            mutations,
            reply,
        })
        .await?
    }
    pub async fn commit_turn_continuation(
        &self,
        operation: TurnContinuationOperation,
        guard: Arc<dyn crate::DurableInteractionCommitGuard>,
        timestamp: u64,
        mutations: broadcast::Sender<crate::SessionMutationCommitted>,
    ) -> Result<TurnContinuationReceipt, SessionError> {
        self.send(|reply| SessionCommand::CommitTurnContinuation {
            operation: Box::new(operation),
            guard,
            timestamp,
            mutations,
            reply,
        })
        .await?
    }
    pub async fn read_turn_continuation(
        &self,
        key: TurnContinuationKey,
        revision: u64,
    ) -> Result<TurnContinuationRecord, SessionError> {
        self.send(|reply| SessionCommand::ReadTurnContinuation {
            key,
            revision,
            reply,
        })
        .await?
    }
    pub async fn turn_continuation_operation(
        &self,
        key: TurnContinuationKey,
        operation_id: String,
    ) -> Result<Option<TurnContinuationOperationRecord>, SessionError> {
        self.send(|reply| SessionCommand::TurnContinuationOperation {
            key,
            operation_id,
            reply,
        })
        .await?
    }
}
impl SessionActor {
    pub(super) async fn handle_turn_continuation_command(
        &mut self,
        command: SessionCommand,
    ) -> Option<SessionCommand> {
        match command {
            SessionCommand::CompleteInteractionContinuation {
                interaction,
                settlement,
                guard,
                timestamp,
                mutations,
                reply,
            } => {
                let result = self
                    .complete_interaction_continuation(
                        *interaction,
                        settlement,
                        guard.as_ref(),
                        timestamp,
                        &mutations,
                    )
                    .await;
                let _ = reply.send(result);
                drop(guard);
            }
            SessionCommand::CommitTurnContinuation {
                operation,
                guard,
                timestamp,
                mutations,
                reply,
            } => {
                let result = self
                    .commit_turn_continuation(*operation, guard.as_ref(), timestamp, &mutations)
                    .await;
                let _ = reply.send(result);
                drop(guard);
            }
            SessionCommand::ReadTurnContinuation {
                key,
                revision,
                reply,
            } => {
                let result = match self.existing_session_db().await {
                    Ok(Some(db)) => db
                        .read_turn_continuation(&key, revision)
                        .await
                        .map_err(Into::into),
                    Ok(None) => Err(SessionError::DbUnavailable(self.state.summary.id)),
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            SessionCommand::TurnContinuationOperation {
                key,
                operation_id,
                reply,
            } => {
                let result = match self.existing_session_db().await {
                    Ok(Some(db)) => db
                        .turn_continuation_operation(&key, &operation_id)
                        .await
                        .map_err(Into::into),
                    Ok(None) => Err(SessionError::DbUnavailable(self.state.summary.id)),
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            command => return Some(command),
        }
        None
    }
    // Keep result publication, attachment, actor acknowledgement and ownership in one
    // cohesive transaction boundary; splitting it obscures the atomic commit ordering.
    #[allow(clippy::too_many_lines)]
    async fn complete_interaction_continuation(
        &mut self,
        interaction: bcode_session_models::DurableInteractionOperation,
        settlement: bcode_session_models::TurnContinuationSettlement,
        guard: &dyn crate::DurableInteractionCommitGuard,
        timestamp: u64,
        mutations: &broadcast::Sender<crate::SessionMutationCommitted>,
    ) -> Result<bcode_session_models::InteractionContinuationReceipt, SessionError> {
        use bcode_session_models::{
            DurableInteractionTransition, TURN_CONTINUATION_VERSION, TurnContinuationOperation,
            TurnContinuationTransition,
        };
        guard.validate()?;
        crate::durable_interaction::ensure_publication_enabled()?;
        interaction
            .validate()
            .map_err(|error| SessionError::EventSerialization(error.into()))?;
        self.ensure_ownership()?;
        let store = self
            .store
            .clone()
            .ok_or(SessionError::DbUnavailable(self.state.summary.id))?;
        let _write =
            crate::lease::acquire_session_write_lock(&store.root_path(), self.state.summary.id)?;
        self.ensure_session_db_for_write().await?;
        self.refresh_state_from_db_for_write().await?;
        let db = self
            .db
            .as_ref()
            .ok_or(SessionError::DbUnavailable(self.state.summary.id))?;
        let DurableInteractionTransition::Complete {
            result,
            exchange_resolution,
            ..
        } = &interaction.transition
        else {
            return Err(SessionError::EventSerialization(
                "combined operation requires Complete".into(),
            ));
        };
        let prior_interaction = db
            .durable_interaction_operation(&interaction.key, &interaction.operation_id)
            .await?;
        let sequence = prior_interaction
            .as_ref()
            .map_or(self.state.next_sequence, |prior| {
                prior.receipt.event_sequence
            });
        let mut first = self.state.build_next_event(
            SessionEventKind::DurableInteractionCommitted {
                operation: interaction.clone(),
            },
            timestamp,
        );
        first.sequence = sequence;
        let result_event = SessionEvent {
            sequence: sequence
                .checked_add(2)
                .ok_or_else(|| SessionError::EventSerialization("sequence overflow".into()))?,
            kind: SessionEventKind::ToolInvocationResultRecorded {
                record: result.as_ref().clone(),
            },
            ..first.clone()
        };
        let mut operation = TurnContinuationOperation {
            version: TURN_CONTINUATION_VERSION,
            key: settlement.key,
            operation_id: settlement.operation_id,
            request_digest: settlement.request_digest,
            expected_revision: Some(settlement.expected_revision),
            transition: TurnContinuationTransition::AttachSelectedResult {
                invocation_id: interaction.key.invocation_id.clone(),
                result_ref: crate::turn_continuation::turn_continuation_source_ref(&result_event)?,
            },
        };
        operation
            .validate()
            .map_err(|error| SessionError::EventSerialization(error.into()))?;
        if let Some(prior) = prior_interaction {
            let attachment = db
                .turn_continuation_operation(&operation.key, &operation.operation_id)
                .await?
                .ok_or_else(|| {
                    SessionError::EventSerialization("partial combined receipt".into())
                })?;
            // Retry timestamps are not canonical event timestamps. Reuse the exact original
            // result locator verified by the retained receipt, never hash a rebuilt event.
            operation.transition = TurnContinuationTransition::AttachSelectedResult {
                invocation_id: interaction.key.invocation_id.clone(),
                result_ref: attachment.receipt.result_ref.clone().ok_or_else(|| {
                    SessionError::EventSerialization("missing combined result locator".into())
                })?,
            };
            if prior.operation != interaction || attachment.operation != operation {
                return Err(SessionError::EventSerialization(
                    "conflicting combined retry".into(),
                ));
            }
            guard.validate()?;
            return Ok(bcode_session_models::InteractionContinuationReceipt {
                interaction: prior.receipt,
                continuation: attachment.receipt,
            });
        }
        if db
            .turn_continuation_operation(&operation.key, &operation.operation_id)
            .await?
            .is_some()
        {
            return Err(SessionError::EventSerialization(
                "conflicting attachment operation".into(),
            ));
        }
        let previous = db.durable_interaction(&interaction.key).await?;
        let interaction_record =
            crate::durable_interaction::advance(previous, &interaction, sequence)?;
        let continuation = db.turn_continuation_record(&operation.key).await?;
        let attach_sequence = sequence
            .checked_add(3)
            .ok_or_else(|| SessionError::EventSerialization("sequence overflow".into()))?;
        let reference = crate::turn_continuation::operation_result_ref(&operation, &result_event)?;
        let continuation_record = crate::turn_continuation::advance(
            continuation,
            &operation,
            attach_sequence,
            reference,
        )?;
        let resolution = SessionEvent {
            sequence: sequence + 1,
            kind: SessionEventKind::ToolExchangeResolved {
                event: exchange_resolution.clone(),
            },
            ..first.clone()
        };
        let attach = SessionEvent {
            sequence: attach_sequence,
            kind: SessionEventKind::TurnContinuationCommitted {
                operation: Box::new(operation),
            },
            ..first.clone()
        };
        let events = vec![
            (first, Some(timestamp)),
            (resolution, Some(timestamp)),
            (result_event, Some(timestamp)),
            (attach, Some(timestamp)),
        ];
        db.append_interaction_continuation_batch(&events, guard)
            .await?;
        for (event, _) in &events {
            self.state.apply_persisted_event(event.clone(), timestamp);
        }
        self.update_manifest_and_schedule_catalog().await;
        self.state.load_status = SessionLoadStatusKind::Current;
        self.refresh_snapshot();
        for (event, _) in events {
            let _ = mutations.send(crate::SessionMutationCommitted {
                session_id: event.session_id,
                event,
                summary: self.state.summary(),
            });
        }
        Ok(bcode_session_models::InteractionContinuationReceipt {
            interaction: interaction_record.receipt,
            continuation: continuation_record.receipt,
        })
    }
    async fn commit_turn_continuation(
        &mut self,
        operation: TurnContinuationOperation,
        guard: &dyn crate::DurableInteractionCommitGuard,
        timestamp: u64,
        mutations: &broadcast::Sender<crate::SessionMutationCommitted>,
    ) -> Result<TurnContinuationReceipt, SessionError> {
        guard.validate()?;
        crate::durable_interaction::ensure_publication_enabled()?;
        operation
            .validate()
            .map_err(|error| SessionError::EventSerialization(error.into()))?;
        self.ensure_ownership()?;
        let store = self
            .store
            .clone()
            .ok_or(SessionError::DbUnavailable(self.state.summary.id))?;
        let _write =
            crate::lease::acquire_session_write_lock(&store.root_path(), self.state.summary.id)?;
        self.ensure_session_db_for_write().await?;
        self.refresh_state_from_db_for_write().await?;
        let db = self
            .db
            .as_ref()
            .ok_or(SessionError::DbUnavailable(self.state.summary.id))?;
        if let Some(receipt) = db.turn_continuation_retry(&operation).await? {
            guard.validate()?;
            return Ok(receipt);
        }
        let event = self.state.build_next_event(
            SessionEventKind::TurnContinuationCommitted {
                operation: Box::new(operation.clone()),
            },
            timestamp,
        );
        let result_event = crate::turn_continuation::result_event(&operation, &event)?;
        let previous = db.turn_continuation_record(&operation.key).await?;
        let result_ref = crate::turn_continuation::operation_result_ref(&operation, &event)?;
        let next =
            crate::turn_continuation::advance(previous, &operation, event.sequence, result_ref)?;
        let mut events = vec![(event, Some(timestamp))];
        if let Some(result) = result_event {
            events.push((result, Some(timestamp)));
        }
        db.append_turn_continuation_batch(&events, guard).await?;
        for (event, _) in &events {
            self.state.apply_persisted_event(event.clone(), timestamp);
        }
        self.update_manifest_and_schedule_catalog().await;
        self.state.load_status = SessionLoadStatusKind::Current;
        self.refresh_snapshot();
        for (event, _) in events {
            let _ = mutations.send(crate::SessionMutationCommitted {
                session_id: event.session_id,
                event,
                summary: self.state.summary(),
            });
        }
        Ok(next.receipt)
    }
}
