//! Turn context blocks driven through a real IPC submission and a fake-provider turn.

// Each test's fixture owns the daemon state and socket for the whole test on purpose.
#![allow(clippy::significant_drop_tightening)]

use std::time::Duration;

use bcode_session::SessionManager;
use bcode_session_models::{
    SessionEvent, SessionEventKind, SessionId, TurnAdmission, TurnAdmissionMetadata,
    TurnAdmissionMetadataError, TurnContext, TurnContextBlock, TurnExecutionOptions,
};

use crate::client_tools_tests::{Fixture, fixture};

fn admission(context: TurnContext) -> TurnAdmissionMetadata {
    TurnAdmissionMetadata {
        execution: TurnExecutionOptions {
            provider_plugin_id: Some("bcode.fake-provider".into()),
            model_id: Some("fake-echo".into()),
            ..Default::default()
        },
        context,
        ..Default::default()
    }
}

async fn session(fixture: &Fixture) -> SessionId {
    fixture
        .client
        .create_session_in_working_directory(
            Some("context".to_owned()),
            fixture.workspace.path().to_path_buf(),
        )
        .await
        .expect("create session")
        .id
}

async fn history(fixture: &Fixture, session_id: SessionId) -> Vec<SessionEvent> {
    fixture
        .state
        .sessions
        .session_events_range(session_id, 0, u64::MAX, 4096)
        .await
        .expect("history")
}

/// Submit `text` and return the answer the fake model gave to it.
async fn answer(
    fixture: &Fixture,
    session_id: SessionId,
    text: &str,
    context: TurnContext,
) -> String {
    let answers_before = assistant_answers(&history(fixture, session_id).await).len();
    let admitted = fixture
        .client
        .submit_turn(session_id, text.to_owned(), admission(context))
        .await
        .expect("submit");
    assert!(
        matches!(admitted, TurnAdmission::Accepted(_)),
        "{admitted:?}"
    );
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let answers = assistant_answers(&history(fixture, session_id).await);
            if answers.len() > answers_before {
                break answers[answers_before].clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("the turn did not answer");
    })
}

fn assistant_answers(events: &[SessionEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::AssistantMessage { text }
            | SessionEventKind::AssistantResponseSegment { text, .. }
            | SessionEventKind::PositionedAssistantResponseSegment { text, .. } => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect()
}

fn user_messages(events: &[SessionEvent]) -> Vec<(String, TurnAdmissionMetadata)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::UserMessage {
                text, admission, ..
            }
            | SessionEventKind::SubmissionActivated {
                text, admission, ..
            } => Some((text.clone(), admission.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn durable_context_is_stored_beside_the_message_and_read_by_every_later_turn() {
    let fixture = fixture();
    let session_id = session(&fixture).await;
    let screen = TurnContextBlock::new("screen", "orders table, rows 1-20");
    let first = answer(
        &fixture,
        session_id,
        "echo-all first",
        TurnContext::durable(vec![screen.clone()]),
    )
    .await;
    assert!(first.contains("echo-all first"), "{first}");
    assert!(
        first.contains("<context label=\"screen\">\norders table, rows 1-20\n</context>"),
        "{first}"
    );

    let events = history(&fixture, session_id).await;
    let messages = user_messages(&events);
    let (text, stored) = messages.first().expect("user message");
    assert_eq!(
        text, "echo-all first",
        "the message text excludes its context"
    );
    assert_eq!(stored.context.blocks, vec![screen]);

    // Later turns read earlier messages with their durable context.
    let context = fixture
        .state
        .sessions
        .model_context_events(session_id)
        .await
        .expect("model context");
    let projected = crate::session_events_to_model_messages(&context);
    let first_user = projected
        .iter()
        .find(|message| message.role == bcode_model::MessageRole::User)
        .expect("projected user message");
    assert_eq!(first_user.content.len(), 2, "{first_user:?}");
}

#[tokio::test]
async fn request_only_context_reaches_its_turn_and_never_history() {
    let fixture = fixture();
    let session_id = session(&fixture).await;
    let secret = "selection: customer 4711";
    let answered = answer(
        &fixture,
        session_id,
        "echo-all now",
        TurnContext::request_only(vec![TurnContextBlock::new("selection", secret)]),
    )
    .await;
    assert!(answered.contains(secret), "{answered}");

    let events = history(&fixture, session_id).await;
    let durable = serde_json::to_string(&events).expect("encode history");
    // The answer quotes it; the submitted message and its metadata never hold it.
    let (_, stored) = user_messages(&events).pop().expect("user message");
    assert_eq!(stored.context.request_only.len(), 0);
    assert!(stored.context.request_only_handle.is_some());
    assert!(
        !serde_json::to_string(&stored)
            .expect("encode")
            .contains(secret)
    );
    assert_eq!(
        durable.matches(secret).count(),
        assistant_answers(&events)
            .iter()
            .map(|answer| answer.matches(secret).count())
            .sum::<usize>()
    );

    // The next turn does not see it.
    let next = answer(
        &fixture,
        session_id,
        "echo-all later",
        TurnContext::default(),
    )
    .await;
    assert!(!next.contains(secret), "{next}");
}

#[tokio::test]
async fn a_turn_whose_request_only_context_was_lost_fails_closed() {
    let fixture = fixture();
    let session_id = session(&fixture).await;
    answer(
        &fixture,
        session_id,
        "echo-all now",
        TurnContext::request_only(vec![TurnContextBlock::new("selection", "rows")]),
    )
    .await;
    let trigger = history(&fixture, session_id)
        .await
        .into_iter()
        .find(|event| {
            matches!(
                event.kind,
                SessionEventKind::UserMessage { .. } | SessionEventKind::SubmissionActivated { .. }
            )
        })
        .expect("trigger");
    assert!(
        crate::request_only_turn_context(&fixture.state, session_id, &trigger)
            .expect("held by this daemon")
            .is_some()
    );

    // A replacement daemon starts with no request-only context in memory.
    let restarted_dir = tempfile::tempdir().expect("dir");
    let restarted = crate::tests::test_server_state(
        SessionManager::persistent(restarted_dir.path().join("sessions")).expect("sessions"),
    );
    let error = crate::request_only_turn_context(&restarted, session_id, &trigger)
        .expect_err("fails closed");
    assert!(
        matches!(
            error,
            bcode_session::SessionError::TurnAdmission(
                TurnAdmissionMetadataError::RequestOnlyContextUnavailable
            )
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn out_of_bounds_or_forged_context_is_rejected_before_admission() {
    let fixture = fixture();
    let session_id = session(&fixture).await;
    let oversized = TurnContext::durable(vec![TurnContextBlock::new(
        "screen",
        "x".repeat(bcode_session_models::MAX_TURN_CONTEXT_BYTES + 1),
    )]);
    let forged = TurnContext {
        request_only_handle: Some("guessed".into()),
        ..TurnContext::default()
    };
    for context in [oversized, forged] {
        let result = fixture
            .client
            .submit_turn(session_id, "hello".to_owned(), admission(context))
            .await;
        assert!(
            matches!(
                &result,
                Err(bcode_client::ClientError::Server { code, .. }) if code == "invalid_turn_context"
            ),
            "{result:?}"
        );
    }
    assert_eq!(user_messages(&history(&fixture, session_id).await).len(), 0);
}
