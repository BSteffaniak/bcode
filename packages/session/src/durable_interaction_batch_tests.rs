//! Atomic batch-shape regression tests.
use super::*;

#[test]
fn terminal_batch_requires_exact_exchange_result_and_contiguous_identity() {
    let session = SessionId::new();
    let first = advance(None, &operation(), 1).unwrap();
    let completion = complete(&first);
    let DurableInteractionTransition::Complete {
        result,
        exchange_resolution,
        ..
    } = &completion.transition
    else {
        unreachable!()
    };
    let interaction = event(session, 2, completion.clone());
    let resolution = SessionEvent {
        kind: SessionEventKind::ToolExchangeResolved {
            event: exchange_resolution.clone(),
        },
        sequence: 3,
        ..interaction.clone()
    };
    let terminal = SessionEvent {
        kind: SessionEventKind::ToolInvocationResultRecorded {
            record: result.as_ref().clone(),
        },
        sequence: 4,
        ..interaction.clone()
    };
    let batch = vec![(interaction, None), (resolution, None), (terminal, None)];
    assert!(crate::db::validate_durable_interaction_batch(&batch).is_ok());
    assert!(crate::db::validate_durable_interaction_batch(&batch[..1]).is_err());
    assert!(crate::db::validate_durable_interaction_batch(&batch[..2]).is_err());
    let mut wrong = batch.clone();
    wrong[1].0.sequence = 4;
    assert!(crate::db::validate_durable_interaction_batch(&wrong).is_err());
    let mut wrong = batch.clone();
    if let SessionEventKind::ToolExchangeResolved { event } = &mut wrong[1].0.kind {
        event.exchange_id = "other".into();
    }
    assert!(crate::db::validate_durable_interaction_batch(&wrong).is_err());
    let mut wrong = batch;
    if let SessionEventKind::ToolInvocationResultRecorded { record } = &mut wrong[2].0.kind {
        record.model_output = "other".into();
    }
    assert!(crate::db::validate_durable_interaction_batch(&wrong).is_err());
}

#[test]
fn caller_request_digest_is_validated_without_interpreting_checkpoint() {
    let mut request = operation();
    assert!(request.validate().is_ok());
    for invalid in [
        "A".repeat(64),
        "g".repeat(64),
        "a".repeat(63),
        String::new(),
    ] {
        request.request_digest = invalid;
        assert!(request.validate().is_err());
    }
}
