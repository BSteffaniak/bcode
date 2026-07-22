use bcode::{
    AgentEvent, ProviderTurnEvent, StopReason, stream_text_builder,
    testing::{
        ScriptedProvider, ScriptedProviderTurn, TextStreamEventKind, record_text_stream,
    },
};

fn assert_send<T: Send>() {}
fn assert_sync<T: Sync>() {}

async fn exercise_testing_surface() {
    let provider = ScriptedProvider::new([ScriptedProviderTurn::new().events([
        ProviderTurnEvent::TurnStarted,
        ProviderTurnEvent::TextDelta {
            text: "ok".to_string(),
        },
        ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::EndTurn,
        },
    ])]);
    let probe = provider.probe();
    let stream = stream_text_builder().prompt("hello").run(provider);
    let transcript = record_text_stream(stream).await;
    transcript
        .assert_event_kind_order(&[
            TextStreamEventKind::TurnStarted,
            TextStreamEventKind::TurnStarted,
            TextStreamEventKind::TextDelta,
            TextStreamEventKind::Finished,
        ])
        .unwrap();
    transcript
        .assert_event_order(&transcript.events())
        .unwrap();
    let _response = transcript.assert_finished().unwrap();
    let _requests = probe.requests();
}

fn main() {
    assert_send::<ScriptedProvider>();
    assert_sync::<ScriptedProvider>();
    assert_send::<bcode::TextStream>();
    assert_send::<bcode::GenerateTextResponse>();
    assert_sync::<bcode::GenerateTextResponse>();
    let _event = AgentEvent::TurnStarted;
    let _exercise = exercise_testing_surface;
}
