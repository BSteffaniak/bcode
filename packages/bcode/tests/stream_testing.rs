#![cfg(feature = "testing")]

use bcode::{
    AgentEvent, AgentRuntime, CancellationToken, ProviderTurnEvent, RuntimeError, StopReason,
    TextStreamItem, stream_text_builder,
    testing::{
        ScriptedProvider, ScriptedProviderTurn, TextStreamAssertionError, TextStreamEventKind,
        TextStreamRecorder, TextStreamTranscript, record_text_stream,
    },
};
use std::num::NonZeroUsize;
use std::time::Duration;

#[tokio::test]
async fn recorder_asserts_order_and_terminal_last_completion() {
    let stream = stream_text_builder()
        .prompt("hello")
        .run(ScriptedProvider::new([ScriptedProviderTurn::new().events(
            [
                ProviderTurnEvent::TurnStarted,
                ProviderTurnEvent::TextDelta {
                    text: "a".to_string(),
                },
                ProviderTurnEvent::TextDelta {
                    text: "b".to_string(),
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        )]));

    let transcript = record_text_stream(stream).await;
    transcript
        .assert_event_kind_order(&[
            TextStreamEventKind::TurnStarted,
            TextStreamEventKind::TurnStarted,
            TextStreamEventKind::TextDelta,
            TextStreamEventKind::TextDelta,
            TextStreamEventKind::Finished,
        ])
        .expect("runtime event order");
    let events = transcript.events();
    transcript
        .assert_event_order(&events)
        .expect("exact event payload order");
    let response = transcript.assert_finished().expect("successful terminal");
    assert_eq!(response.text, "ab");
    assert!(transcript.is_exhausted());
    assert!(matches!(
        transcript.items().last(),
        Some(TextStreamItem::Finished(_))
    ));
}

#[tokio::test]
async fn recorder_supports_explicit_partial_consumption() {
    let stream = stream_text_builder()
        .prompt("hello")
        .run(ScriptedProvider::new([
            ScriptedProviderTurn::complete_text("answer"),
        ]));
    let mut recorder = TextStreamRecorder::new(stream);

    assert_eq!(recorder.consume_up_to(1).await, 1);
    assert_eq!(recorder.items().len(), 1);
    assert!(!recorder.is_exhausted());

    let transcript = recorder.finish().await;
    transcript
        .assert_terminal_coherence()
        .expect("continuing after partial consumption reaches one terminal");
}

#[tokio::test]
async fn recorder_cancels_and_asserts_typed_terminal_state() {
    let cancellation = CancellationToken::new();
    let stream = stream_text_builder()
        .prompt("hello")
        .cancellation(cancellation.clone())
        .run(ScriptedProvider::new([ScriptedProviderTurn::new()
            .events([ProviderTurnEvent::TurnStarted])
            .pending()]));
    let mut recorder = TextStreamRecorder::new(stream);
    assert_eq!(recorder.consume_up_to(1).await, 1);

    let transcript = recorder.cancel_and_finish(&cancellation).await;
    transcript.assert_cancelled().expect("typed cancellation");
    transcript
        .assert_terminal_coherence()
        .expect("cancel terminal is coherent");
}

#[tokio::test]
async fn recorder_asserts_bounded_backpressure_overflow() {
    let capacity = NonZeroUsize::new(2).expect("positive capacity");
    let runtime = AgentRuntime::new().with_stream_buffer_capacity(capacity);
    let events = std::iter::once(ProviderTurnEvent::TurnStarted)
        .chain((0..8).map(|index| ProviderTurnEvent::TextDelta {
            text: index.to_string(),
        }))
        .chain(std::iter::once(ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::EndTurn,
        }));
    let stream = stream_text_builder()
        .prompt("hello")
        .configure_agent(|builder| builder.runtime(runtime))
        .run(ScriptedProvider::new([
            ScriptedProviderTurn::new().events(events)
        ]));

    // Let the producer fill its bounded queue before attaching the recorder as a consumer.
    tokio::task::yield_now().await;
    let transcript = record_text_stream(stream).await;
    transcript
        .assert_backpressure_overflow(capacity.get())
        .expect("typed bounded-buffer overflow");
    assert!(matches!(
        transcript.assert_runtime_error(),
        Ok(RuntimeError::StreamBufferFull { capacity: 2 })
    ));
}

#[test]
fn transcript_assertions_reject_partial_and_incoherent_recordings() {
    let partial = TextStreamTranscript::from_items(
        vec![TextStreamItem::Event(AgentEvent::TurnStarted)],
        false,
    );
    assert_eq!(
        partial.assert_terminal_coherence(),
        Err(TextStreamAssertionError::NotExhausted)
    );

    let missing_terminal = TextStreamTranscript::from_items(
        vec![TextStreamItem::Event(AgentEvent::TurnStarted)],
        true,
    );
    assert_eq!(
        missing_terminal.assert_terminal_coherence(),
        Err(TextStreamAssertionError::MissingTerminal)
    );

    let terminal_not_last = TextStreamTranscript::from_items(
        vec![
            TextStreamItem::Error(bcode::BcodeError::Runtime(RuntimeError::Cancelled)),
            TextStreamItem::Event(AgentEvent::Cancelled),
        ],
        true,
    );
    assert_eq!(
        terminal_not_last.assert_terminal_coherence(),
        Err(TextStreamAssertionError::TerminalNotLast {
            terminal_index: 0,
            item_count: 2,
        })
    );

    let multiple_terminals = TextStreamTranscript::from_items(
        vec![
            TextStreamItem::Error(bcode::BcodeError::Runtime(RuntimeError::Cancelled)),
            TextStreamItem::Error(bcode::BcodeError::Runtime(RuntimeError::Cancelled)),
        ],
        true,
    );
    assert_eq!(
        multiple_terminals.assert_terminal_coherence(),
        Err(TextStreamAssertionError::MultipleTerminals { count: 2 })
    );
}

#[tokio::test]
async fn dropping_partially_consumed_recorder_cancels_provider_cleanup() {
    let provider = ScriptedProvider::new([ScriptedProviderTurn::new()
        .events([ProviderTurnEvent::TurnStarted])
        .pending()]);
    let probe = provider.probe();
    let stream = stream_text_builder().prompt("hello").run(provider.clone());
    let mut recorder = TextStreamRecorder::new(stream);
    assert_eq!(recorder.consume_up_to(1).await, 1);

    drop(recorder);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if probe.cancellations().len() == 1 && probe.finishes().len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("drop cancellation cleanup completes");
    probe
        .assert_cancellation_count(1)
        .expect("drop cancels provider turn");
    probe
        .assert_finish_count(1)
        .expect("drop finishes provider turn");
}
