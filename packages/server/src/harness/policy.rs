//! Host validation of native evidence. Parentage and native activity confer no authority.

use bcode_harness_models::{
    HarnessCapabilities, HarnessDeliveryState, HarnessInteraction, HarnessObservation,
    HarnessObservationKind, HarnessPlacement, HarnessReceipt, HarnessResponse,
    HarnessSessionTarget, HarnessSubmission, HarnessSupportLevel,
};

use super::ManagedHarnessError;

pub(super) fn submission(
    binding: &HarnessSessionTarget,
    capabilities: &HarnessCapabilities,
    proposed: &HarnessSubmission,
) -> Result<(), ManagedHarnessError> {
    binding
        .validate()
        .map_err(|_| ManagedHarnessError::Invalid)?;
    capabilities
        .validate()
        .map_err(|_| ManagedHarnessError::Invalid)?;
    proposed
        .validate()
        .map_err(|_| ManagedHarnessError::Invalid)?;
    binding
        .validate_owner(&proposed.session_id, proposed.generation)
        .map_err(|_| ManagedHarnessError::StaleOwner)?;
    if matches!(proposed.placement, HarnessPlacement::Steer { .. })
        && capabilities.steering.support != HarnessSupportLevel::Supported
    {
        // Constrained support requires a separate host-admitted constraint evaluator.
        // Do not silently reinterpret steering as next-turn input.
        return Err(ManagedHarnessError::Unsupported);
    }
    Ok(())
}

pub(super) fn response(
    binding: &HarnessSessionTarget,
    outstanding: &HarnessInteraction,
    proposed: &HarnessResponse,
) -> Result<(), ManagedHarnessError> {
    binding
        .validate_owner(&proposed.session_id, proposed.generation)
        .map_err(|_| ManagedHarnessError::StaleOwner)?;
    proposed
        .validate_for(outstanding)
        .map_err(|_| ManagedHarnessError::Invalid)
}

pub(super) fn receipt(
    proposed: &HarnessSubmission,
    received: &HarnessReceipt,
) -> Result<(), ManagedHarnessError> {
    if received.session_id != proposed.session_id
        || received.generation != proposed.generation
        || received.submission_id != proposed.submission_id
        || received.attempt_id != proposed.attempt_id
        || received.native_request_id.as_ref().is_some_and(|id| {
            id.is_empty()
                || id.len() > bcode_harness_models::MAX_HARNESS_ID_BYTES
                || id.chars().any(char::is_control)
        })
    {
        return Err(ManagedHarnessError::Invalid);
    }
    transition(HarnessDeliveryState::DeliveryAttempted, received.state)
}

pub(super) fn observation(
    binding: &HarnessSessionTarget,
    capabilities: &HarnessCapabilities,
    observation: &HarnessObservation,
) -> Result<(), ManagedHarnessError> {
    observation
        .validate()
        .map_err(|_| ManagedHarnessError::Invalid)?;
    binding
        .validate_owner(&observation.session_id, observation.generation)
        .map_err(|_| ManagedHarnessError::StaleOwner)?;
    if let HarnessObservationKind::BindingConfirmed {
        open_id,
        binding: confirmed,
    } = &observation.kind
    {
        binding
            .validate_confirmation(open_id, confirmed)
            .map_err(|_| ManagedHarnessError::Conflict)?;
    }
    if matches!(observation.kind, HarnessObservationKind::Settlement { .. })
        && capabilities.authoritative_settlement.support != HarnessSupportLevel::Supported
    {
        tracing::warn!(target: "bcode_server::harness", stage = "observe_policy", category = "settlement_capability", observation_kind = "settlement", support = ?capabilities.authoritative_settlement.support, "managed observation rejected");
        return Err(ManagedHarnessError::Unsupported);
    }
    Ok(())
}

/// Native acknowledgments and idle/results cannot reopen a settled mutation.
pub(super) fn transition(
    previous: HarnessDeliveryState,
    next: HarnessDeliveryState,
) -> Result<(), ManagedHarnessError> {
    use HarnessDeliveryState as S;
    let permitted = previous == next
        || match previous {
            S::Accepted => matches!(next, S::Prepared | S::Rejected),
            S::Prepared => matches!(next, S::DeliveryAttempted | S::Rejected),
            S::DeliveryAttempted => matches!(
                next,
                S::NativeAccepted | S::Executing | S::Settled | S::Uncertain | S::Rejected
            ),
            S::NativeAccepted => matches!(next, S::Executing | S::Settled | S::Uncertain),
            S::Executing => matches!(next, S::Settled | S::Uncertain),
            // Only explicit reconciliation evidence may resolve uncertainty. Never Prepared.
            S::Uncertain => matches!(
                next,
                S::NativeAccepted | S::Executing | S::Settled | S::Rejected
            ),
            S::Settled | S::Rejected => false,
        };
    if permitted {
        Ok(())
    } else {
        Err(ManagedHarnessError::Conflict)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_harness_models::{HARNESS_SCHEMA_VERSION, HarnessSessionBinding};

    fn binding() -> HarnessSessionTarget {
        HarnessSessionTarget::Confirmed(HarnessSessionBinding {
            schema_version: HARNESS_SCHEMA_VERSION,
            session_id: "session".into(),
            instance_id: "instance".into(),
            native_session_id: "native".into(),
            native_version: "1".into(),
            generation: 7,
        })
    }

    #[test]
    fn stale_generation_is_not_parent_or_identity_authority() {
        let stale = binding().validate_owner("session", 6);
        assert!(stale.is_err());
        assert!(binding().validate_owner("different", 7).is_err());
        assert!(binding().validate_owner("session", 7).is_ok());
    }

    #[test]
    fn ambiguous_delivery_never_becomes_replayable_preparation() {
        use HarnessDeliveryState as S;
        for state in [
            S::DeliveryAttempted,
            S::NativeAccepted,
            S::Executing,
            S::Uncertain,
        ] {
            assert_eq!(
                transition(state, S::Prepared),
                Err(ManagedHarnessError::Conflict)
            );
            assert_eq!(
                transition(state, S::Accepted),
                Err(ManagedHarnessError::Conflict)
            );
        }
        assert!(transition(S::Uncertain, S::Settled).is_ok());
    }

    fn capabilities() -> HarnessCapabilities {
        use bcode_harness_models::{HarnessCapability, HarnessSupportLevel};
        let supported = HarnessCapability {
            support: HarnessSupportLevel::Supported,
            constraints: vec![],
        };
        HarnessCapabilities {
            resume: supported.clone(),
            steering: supported.clone(),
            approval: supported.clone(),
            questions: supported.clone(),
            assistance: supported.clone(),
            interrupt: supported.clone(),
            authoritative_settlement: supported.clone(),
            background_continuation: supported.clone(),
            authentication: supported,
        }
    }

    fn input() -> HarnessSubmission {
        HarnessSubmission {
            submission_id: "submission".into(),
            attempt_id: "attempt".into(),
            session_id: "session".into(),
            generation: 7,
            text: "hello".into(),
            placement: HarnessPlacement::NextTurn,
        }
    }

    #[test]
    fn steering_is_not_silently_queued_or_admitted_under_unknown_constraints() {
        let mut capabilities = capabilities();
        let mut input = input();
        input.placement = HarnessPlacement::Steer {
            expected_native_turn_id: "turn".into(),
        };
        assert!(submission(&binding(), &capabilities, &input).is_ok());
        for support in [
            HarnessSupportLevel::Unsupported,
            HarnessSupportLevel::Unknown,
            HarnessSupportLevel::Constrained,
        ] {
            capabilities.steering.support = support;
            capabilities.steering.constraints = vec!["native version gate".into()];
            assert_eq!(
                submission(&binding(), &capabilities, &input),
                Err(ManagedHarnessError::Unsupported)
            );
        }
    }

    #[test]
    fn lost_ack_receipt_identity_and_delivery_phase_are_checked() {
        let input = input();
        let mut accepted = HarnessReceipt {
            submission_id: input.submission_id.clone(),
            attempt_id: input.attempt_id.clone(),
            session_id: input.session_id.clone(),
            generation: input.generation,
            state: HarnessDeliveryState::NativeAccepted,
            native_request_id: None,
        };
        assert!(receipt(&input, &accepted).is_ok());
        accepted.attempt_id = "other-attempt".into();
        assert_eq!(
            receipt(&input, &accepted),
            Err(ManagedHarnessError::Invalid)
        );
        accepted.attempt_id = input.attempt_id.clone();
        accepted.state = HarnessDeliveryState::Prepared;
        assert_eq!(
            receipt(&input, &accepted),
            Err(ManagedHarnessError::Conflict)
        );
    }

    #[test]
    fn constrained_question_observation_is_inert_and_does_not_require_settlement_support() {
        use bcode_harness_models::{HarnessCompleteness, HarnessInteractionKind};
        let mut capabilities = capabilities();
        capabilities.questions.support = HarnessSupportLevel::Constrained;
        capabilities.questions.constraints = vec!["single-select normalized questions only".into()];
        capabilities.authoritative_settlement.support = HarnessSupportLevel::Unsupported;
        let event = HarnessObservation {
            observation_id: "question-event".into(),
            session_id: "session".into(),
            generation: 7,
            native_correlations: vec![],
            completeness: HarnessCompleteness::Complete,
            kind: HarnessObservationKind::Interaction {
                interaction: HarnessInteraction {
                    request_id: "question".into(),
                    session_id: "session".into(),
                    generation: 7,
                    kind: HarnessInteractionKind::Question,
                    prompt: "Choose".into(),
                    scope: "native question".into(),
                    choices: vec![],
                },
            },
        };
        assert!(observation(&binding(), &capabilities, &event).is_ok());
        // Reading a normalized question grants neither response delivery nor settlement.
        let mut settlement = event;
        settlement.kind = HarnessObservationKind::Settlement {
            native_turn_id: "turn".into(),
        };
        for support in [
            HarnessSupportLevel::Unsupported,
            HarnessSupportLevel::Unknown,
            HarnessSupportLevel::Constrained,
        ] {
            capabilities.authoritative_settlement.support = support;
            capabilities.authoritative_settlement.constraints = vec!["unverified condition".into()];
            assert_eq!(
                observation(&binding(), &capabilities, &settlement),
                Err(ManagedHarnessError::Unsupported)
            );
        }
    }

    #[test]
    fn native_result_and_tool_observation_do_not_require_settlement_capability() {
        use bcode_harness_models::HarnessCompleteness;
        let mut capabilities = capabilities();
        capabilities.authoritative_settlement.support = HarnessSupportLevel::Unsupported;
        let mut event = HarnessObservation {
            observation_id: "event".into(),
            session_id: "session".into(),
            generation: 7,
            native_correlations: vec![],
            completeness: HarnessCompleteness::Partial,
            kind: HarnessObservationKind::Result {
                text: "answer".into(),
            },
        };
        assert!(observation(&binding(), &capabilities, &event).is_ok());
        event.kind = HarnessObservationKind::Tool {
            native_tool_id: "tool".into(),
            name: "shell".into(),
            status: "observed".into(),
        };
        assert!(observation(&binding(), &capabilities, &event).is_ok());
        event.kind = HarnessObservationKind::Settlement {
            native_turn_id: "turn".into(),
        };
        assert_eq!(
            observation(&binding(), &capabilities, &event),
            Err(ManagedHarnessError::Unsupported)
        );
        event.generation = 6;
        assert_eq!(
            observation(&binding(), &capabilities, &event),
            Err(ManagedHarnessError::StaleOwner)
        );
    }

    #[test]
    fn approval_question_assistance_responses_stay_typed_and_generation_fenced() {
        use bcode_harness_models::{HarnessInteractionKind, HarnessResponseValue};
        for (kind, value) in [
            (
                HarnessInteractionKind::Approval,
                HarnessResponseValue::Approval { approved: true },
            ),
            (
                HarnessInteractionKind::Question,
                HarnessResponseValue::Answer {
                    choice_id: None,
                    text: "answer".into(),
                },
            ),
            (
                HarnessInteractionKind::Assistance,
                HarnessResponseValue::Assistance {
                    text: "help".into(),
                },
            ),
        ] {
            let interaction = HarnessInteraction {
                request_id: "request".into(),
                session_id: "session".into(),
                generation: 7,
                kind,
                prompt: "prompt".into(),
                scope: "native scope".into(),
                choices: vec![],
            };
            let mut proposed = HarnessResponse {
                response_id: "response".into(),
                request_id: "request".into(),
                session_id: "session".into(),
                generation: 7,
                value,
            };
            assert!(response(&binding(), &interaction, &proposed).is_ok());
            proposed.generation = 6;
            assert_eq!(
                response(&binding(), &interaction, &proposed),
                Err(ManagedHarnessError::StaleOwner)
            );
        }
    }

    #[test]
    fn terminal_receipts_cannot_reopen() {
        use HarnessDeliveryState as S;
        for terminal in [S::Settled, S::Rejected] {
            assert!(transition(terminal, terminal).is_ok());
            for next in [
                S::Accepted,
                S::Prepared,
                S::DeliveryAttempted,
                S::NativeAccepted,
                S::Executing,
                S::Uncertain,
            ] {
                assert_eq!(
                    transition(terminal, next),
                    Err(ManagedHarnessError::Conflict)
                );
            }
        }
    }
}
