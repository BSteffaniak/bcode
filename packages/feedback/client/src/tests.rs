use bcode_feedback_models::{Facet, FeedbackId, FeedbackItem, FeedbackStatus, TriageState};

fn item(facets: Vec<Facet>) -> FeedbackItem {
    FeedbackItem {
        id: FeedbackId(7),
        app: "bdata".into(),
        text: "copy failed".into(),
        facets,
        created_at_ms: 0,
        updated_at_ms: 0,
        triage_state: TriageState::Pending,
        triage: None,
        status: FeedbackStatus::Open,
        status_change: None,
        links: Vec::new(),
        attachments: Vec::new(),
        triage_dispatch: None,
    }
}

#[test]
fn intake_context_names_the_item_and_withholds_sensitive_facets() {
    let context = crate::intake_context(
        &item(vec![
            Facet::new("bdata.dialect", "postgres"),
            Facet {
                key: "bdata.connection".into(),
                value: "postgres://u:hunter2@db".into(),
                sensitive: true,
            },
        ]),
        None,
    )
    .unwrap();
    let text = context
        .blocks
        .iter()
        .map(|block| format!("{}:{}", block.label, block.text))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("fb-7"));
    assert!(text.contains("app:bdata"));
    assert!(text.contains("bdata.dialect=postgres"));
    assert!(!text.contains("hunter2"));
    assert!(
        context.request_only.is_empty(),
        "item context must stay durable for later turns"
    );
}

#[test]
fn intake_context_fits_turn_context_bounds_with_maximum_facets() {
    let facets = (0..bcode_feedback_models::MAX_FACETS)
        .map(|n| {
            Facet::new(
                format!("{n:0>64}"),
                "v".repeat(bcode_feedback_models::MAX_FACET_VALUE_BYTES),
            )
        })
        .collect();
    let instructions = "i".repeat(crate::MAX_APP_INSTRUCTIONS_BYTES);
    let context = crate::intake_context(&item(facets), Some(&instructions)).unwrap();
    assert!(
        context.validate().is_ok(),
        "maximum-size facets and instructions must not make the intake turn invalid"
    );
    assert!(
        context
            .blocks
            .iter()
            .any(|block| block.label == "app-instructions")
    );
    for invalid in [
        String::new(),
        "i".repeat(crate::MAX_APP_INSTRUCTIONS_BYTES + 1),
    ] {
        assert!(matches!(
            crate::intake_context(&item(Vec::new()), Some(&invalid)),
            Err(crate::FeedbackClientError::Invalid(_))
        ));
    }
}

#[test]
fn the_reply_is_the_last_output_position_of_this_turn_only() {
    use bcode_session_models::{
        SessionEvent, SessionEventKind, SessionId, TurnId, TurnOutputPosition,
    };
    let session_id = SessionId::new();
    let event = |sequence, turn: &str, position, order, text: &str| SessionEvent {
        schema_version: 1,
        sequence,
        timestamp_ms: 0,
        session_id,
        provenance: None,
        kind: SessionEventKind::PositionedAssistantResponseSegment {
            turn_id: turn.into(),
            output_position: TurnOutputPosition::new(position),
            segment_id: format!("s{sequence}"),
            segment_order: order,
            text: text.into(),
        },
    };
    let events = vec![
        event(1, "t1", 1, 0, "Let me search first."),
        event(5, "t1", 3, 1, "related to fb-1."),
        event(4, "t1", 3, 0, "Recorded fb-2, "),
        event(6, "t2", 9, 0, "another turn"),
    ];
    assert_eq!(
        crate::final_reply(&events, &TurnId("t1".into())).as_deref(),
        Some("Recorded fb-2, related to fb-1.")
    );
    assert_eq!(crate::final_reply(&events, &TurnId("t3".into())), None);
}
