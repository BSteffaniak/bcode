//! Offline plugin boundary coverage. The ledger below models the managed host's
//! commit-before-ACK ordering; it is not a database or server integration test.
use super::*;
use bcode_claude_code::{AttributionStage, SubmissionAttribution};
use bcode_harness_models::{
    HarnessExecutionEvent, HarnessObservationPage, HarnessTrackingCoverage,
    harness_observations_digest,
};

#[derive(Default)]
struct CommittedPages {
    pages: Vec<HarnessObservationPage>,
}
impl CommittedPages {
    fn commit(
        &mut self,
        page: &HarnessObservationPage,
    ) -> Option<bcode_harness_models::HarnessRetentionContext> {
        page.validate_retention().unwrap();
        assert_eq!(page.hydration_requests, Vec::new());
        self.pages.push(page.clone());
        // A page with nothing to transfer is not retained; the host then has no
        // acknowledgment to return for it.
        let Some(metadata) = page.retention.clone() else {
            assert_eq!(page.observations, Vec::new());
            return None;
        };
        assert_eq!(
            metadata.observations_digest,
            harness_observations_digest(&page.observations).unwrap()
        );
        // The exact returned page is preserved above before issuing its acknowledgment.
        Some(retained_page_ack(
            metadata,
            u64::try_from(self.pages.len()).unwrap(),
        ))
    }
}

fn poll(
    stream: &mut StreamState,
    prepared: &HarnessPreparedSession,
    request: &HarnessObserveRequest,
    cycle: usize,
) -> HarnessObservationPage {
    let HarnessServiceResponseKind::Observations(page) = stream
        .observation_page(prepared, request, &Settlement::Unsupported)
        .unwrap_or_else(|error| panic!("observation cycle {cycle}: {error:?}"))
    else {
        panic!("observation page required");
    };
    assert_eq!(page.status.activity, HarnessActivity::Unknown);
    assert_eq!(page.completeness, HarnessCompleteness::Complete);
    page
}

fn staged_foreground(
    result_subtype: Option<&str>,
) -> (
    HarnessPreparedSession,
    HarnessSessionTarget,
    StreamState,
    HarnessSubmission,
) {
    const REPLY: &str = "11111111-1111-4111-8111-000000000001";
    const FRAME: &str = "11111111-1111-4111-8111-000000000002";
    let (prepared, target, mut stream) = eof_stream_fixture();
    stream.normalizer.configure_execution(false, true);
    stream.normalizer.enable_retention();
    let submission = HarnessSubmission {
        submission_id: "foreground".into(),
        attempt_id: "00000000-0000-4000-8000-000000000001".into(),
        session_id: prepared.session_id.clone(),
        generation: prepared.generation,
        text: "offline sentinel".into(),
        placement: HarnessPlacement::NextTurn,
    };
    let uuid = stream.normalizer.retain_submission(&submission).unwrap();
    let mut result = json!({
        "type":"result", "uuid":FRAME, "is_error":false, "result":"offline sentinel"
    });
    if let Some(subtype) = result_subtype {
        result["subtype"] = json!(subtype);
    }
    let events = [
        Event::Message(NativeMessage::System(
            json!({"type":"system","subtype":"init"}),
        )),
        Event::Message(NativeMessage::Notice(json!({"type":"rate_limit_event"}))),
        Event::Message(NativeMessage::Stream(
            json!({"type":"stream_event","event":{"type":"message_start"}}),
        )),
        Event::Attribution(SubmissionAttribution {
            native_frame_uuid: REPLY.into(),
            primary: uuid.clone(),
            consumed: vec![uuid.clone()],
            resumed: false,
            stage: AttributionStage::Reply,
        }),
        Event::Message(NativeMessage::Assistant(json!({
            "type":"assistant", "uuid":REPLY,
            "message":{"content":[{"type":"text","text":"offline sentinel"}]}
        }))),
        Event::Attribution(SubmissionAttribution {
            native_frame_uuid: FRAME.into(),
            primary: uuid.clone(),
            consumed: vec![uuid],
            resumed: false,
            stage: AttributionStage::Result {
                is_error: false,
                queued_turn_count: Some(0),
            },
        }),
        Event::Message(NativeMessage::Result(result)),
    ];
    for event in events {
        let observations = stream.normalizer.event(event);
        assert!(stream.push(observations));
    }
    (prepared, target, stream, submission)
}

fn is_foreground_outcome(item: &HarnessObservation) -> bool {
    matches!(
        item.kind,
        HarnessObservationKind::Result { .. }
            | HarnessObservationKind::Execution {
                event: HarnessExecutionEvent::SubmissionTerminal { .. }
            }
    )
}

#[test]
fn a_malformed_result_is_reported_as_a_gap_and_observation_continues() {
    let (prepared, target, mut stream, _) = staged_foreground(None);
    let mut request = eof_observe_request(target);
    let mut committed = CommittedPages::default();
    // Missing native disposition is malformed evidence: it never becomes a fabricated
    // outcome. It is reported once as a gap; after that page is committed, pages are complete
    // again and the session keeps being observed.
    let mut reported = false;
    for cycle in 1..=12 {
        let Ok(HarnessServiceResponseKind::Observations(page)) =
            stream.observation_page(&prepared, &request, &Settlement::Unsupported)
        else {
            panic!("observation cycle {cycle} must keep producing pages");
        };
        assert!(!page.observations.iter().any(is_foreground_outcome));
        if page.completeness == HarnessCompleteness::Gap {
            reported = true;
        } else if reported {
            assert!(!stream.gap, "a committed gap report clears the gap");
            return;
        }
        request.retention = committed.commit(&page);
        request.after = page.next_after;
    }
    panic!("the gap must be reported, then cleared after its page is committed");
}

#[test]
fn staged_pages_need_exact_committed_ack_and_batch_records_between_evidence() {
    let (prepared, target, mut stream, submission) = staged_foreground(Some("success"));
    let mut request = eof_observe_request(target);
    let mut committed = CommittedPages::default();
    let mut delivered: Vec<HarnessObservation> = Vec::new();
    let mut cycles = 0;
    while !delivered
        .iter()
        .any(|item| matches!(item.kind, HarnessObservationKind::Result { .. }))
    {
        cycles += 1;
        // Seven staged native records; execution evidence still bounds each batch.
        assert!(
            cycles <= 4,
            "staged records must not need one host round trip each"
        );
        let page = poll(&mut stream, &prepared, &request, cycles);
        // A cursor alone never releases even an empty retained page.
        let mut unacknowledged = request.clone();
        unacknowledged.retention = None;
        unacknowledged.after = page.next_after.clone();
        let repeated = poll(&mut stream, &prepared, &unacknowledged, cycles);
        assert_eq!(
            serde_json::to_value(&repeated).unwrap(),
            serde_json::to_value(&page).unwrap()
        );
        let acknowledgment = committed.commit(&page);
        if let Some(acknowledgment) = &acknowledgment {
            let mut wrong = acknowledgment.clone();
            wrong.acknowledgment.as_mut().unwrap().observations_digest = "0".repeat(64);
            let invalid = HarnessObserveRequest {
                retention: Some(wrong),
                ..request.clone()
            };
            assert!(matches!(
                stream.observation_page(&prepared, &invalid, &Settlement::Unsupported),
                Err(HarnessFailure::Conflict)
            ));
        }
        request.retention = acknowledgment;
        // Match the immediately preceding committed page, including null after an
        // empty page. Do not carry forward the last nonempty observation ID.
        request.after = page.next_after.clone();
        delivered.extend(page.observations);
    }
    let position = |predicate: &dyn Fn(&HarnessObservation) -> bool| {
        delivered
            .iter()
            .position(predicate)
            .expect("delivered observation")
    };
    let text = position(&|item| {
        matches!(&item.kind,
        HarnessObservationKind::Text { text, .. } if text == "offline sentinel")
    });
    let result = position(&|item| {
        matches!(&item.kind,
        HarnessObservationKind::Result { text } if text == "offline sentinel")
    });
    assert!(
        text < result,
        "native order is preserved across batched records"
    );
    assert!(delivered.iter().any(|item| matches!(&item.kind,
        HarnessObservationKind::Execution { event: HarnessExecutionEvent::SubmissionTerminal { attempt, .. } }
            if attempt.submission_id == submission.submission_id && attempt.attempt_id == submission.attempt_id)));
    assert!(delivered.iter().any(|item| matches!(
        &item.kind,
        HarnessObservationKind::Execution {
            event: HarnessExecutionEvent::Coverage { .. }
        }
    )));
    for item in &delivered {
        if let HarnessObservationKind::Execution {
            event:
                HarnessExecutionEvent::Coverage {
                    background,
                    continuations,
                    ..
                },
        } = &item.kind
        {
            assert_eq!(*background, HarnessTrackingCoverage::Unknown);
            assert_eq!(*continuations, HarnessTrackingCoverage::Unknown);
        }
    }
    // The next successful cycle acknowledges the foreground result's exact page.
    let final_page = poll(&mut stream, &prepared, &request, cycles + 1);
    assert_eq!(final_page.observations, Vec::new());
    // Nothing remains to transfer, so the idle page is not retained.
    assert_eq!(committed.commit(&final_page), None);
    assert_eq!(committed.pages.len(), cycles + 1);
}
