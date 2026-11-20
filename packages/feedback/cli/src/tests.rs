use bcode_feedback_models::{Facet, FeedbackId, FeedbackItem, FeedbackStatus, TriageState};
use clap::Parser as _;

use crate::{FeedbackCli, FeedbackCommand};

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
    }
}

#[test]
fn intake_context_names_the_item_and_withholds_sensitive_facets() {
    let context = crate::intake::context(&item(vec![
        Facet::new("bdata.dialect", "postgres"),
        Facet {
            key: "bdata.connection".into(),
            value: "postgres://u:hunter2@db".into(),
            sensitive: true,
        },
    ]));
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
    let context = crate::intake::context(&item(facets));
    assert!(
        context.validate().is_ok(),
        "maximum-size facets must not make the intake turn invalid"
    );
}

#[test]
fn submit_parses_facets_text_and_modes() {
    let cli = FeedbackCli::try_parse_from([
        "feedback",
        "submit",
        "--app",
        "bmux",
        "--facet",
        "bmux.pane=3",
        "--sensitive-facet",
        "token=abc=def",
        "--wait",
        "split",
        "flickers",
    ])
    .unwrap();
    let FeedbackCommand::Submit(args) = cli.command else {
        panic!("submit");
    };
    assert_eq!(args.app.as_deref(), Some("bmux"));
    assert_eq!(args.facets, vec![("bmux.pane".into(), "3".into())]);
    assert_eq!(
        args.sensitive_facets,
        vec![("token".into(), "abc=def".into())]
    );
    assert_eq!(args.wait, Some(600));
    assert_eq!(args.text.join(" "), "split flickers");

    assert!(
        FeedbackCli::try_parse_from(["feedback", "submit", "--no-triage", "--wait", "--app", "x"])
            .is_err(),
        "recording without triage cannot wait for a reply"
    );
    assert!(
        FeedbackCli::try_parse_from(["feedback", "submit", "--request-json", "--app", "x"])
            .is_err(),
        "a JSON request carries its own app"
    );
    assert!(FeedbackCli::try_parse_from(["feedback", "submit", "--request-json"]).is_ok());
}

#[test]
fn ids_parse_in_either_form() {
    let cli = FeedbackCli::try_parse_from(["feedback", "verify", "12", "--note", "works"]).unwrap();
    let FeedbackCommand::Verify(args) = cli.command else {
        panic!("verify");
    };
    assert_eq!(args.id, FeedbackId(12));
    assert!(FeedbackCli::try_parse_from(["feedback", "show", "fb-x"]).is_err());
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
        crate::intake::final_reply(&events, &TurnId("t1".into())).as_deref(),
        Some("Recorded fb-2, related to fb-1.")
    );
    assert_eq!(
        crate::intake::final_reply(&events, &TurnId("t3".into())),
        None
    );
}
