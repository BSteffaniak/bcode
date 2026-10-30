//! Provider diagnostics behavioral coverage. Not executed during interface convergence.

use super::*;
use bcode_model::diagnostics::{ProviderDiagnosticPayload, ProviderDiagnosticStage};
use bcode_model::{ProviderTurnEvent, StopReason, TokenUsage};
use bcode_xray::{XRAY_SCHEMA_VERSION, XrayCorrelation, XrayHub, XrayObserveRequest};
use serde::Serialize;
use serde_json::json;

fn context(hub: &XrayHub) -> CaptureContext {
    hub.context(XrayCorrelation {
        session_id: "session".into(),
        turn_id: Some("turn".into()),
        request_id: Some("request".into()),
        attempt_id: Some("attempt".into()),
        ..XrayCorrelation::default()
    })
}

fn observe(hub: &XrayHub, deep_capture: bool) -> bcode_xray::XrayObservation {
    hub.observe(XrayObserveRequest {
        schema_version: XRAY_SCHEMA_VERSION,
        session_id: "session".into(),
        deep_capture,
    })
    .unwrap()
}

fn run(future: impl Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future);
}

#[derive(Serialize)]
struct Body {
    text: String,
}
impl ProviderDiagnosticPayload for Body {
    const FORMAT: &'static str = "test.request_body";
    const SCHEMA_VERSION: u16 = 1;
    const STAGE: ProviderDiagnosticStage = ProviderDiagnosticStage::RequestAdapted;
}

#[test]
fn shallow_and_detached_observers_do_not_evaluate_sensitive_projection() {
    let hub = XrayHub::default();
    let shallow = observe(&hub, false);
    let diagnostics = ProviderDiagnostics::new(Some(context(&hub)));
    diagnostics.payload::<Body>(|| panic!("shallow must not build"));
    drop(shallow);
    let deep = observe(&hub, true);
    drop(deep);
    diagnostics.payload::<Body>(|| panic!("detached must not build"));
}

#[test]
fn provider_projection_is_sensitive_versioned_and_omission_is_explicit() {
    run(async {
        let hub = XrayHub::default();
        let mut observer = observe(&hub, true);
        let diagnostics = ProviderDiagnostics::new(Some(context(&hub)));
        diagnostics.payload(|| Body {
            text: "body".into(),
        });
        diagnostics.payload(|| Body {
            text: "x".repeat(MAX_PROVIDER_DIAGNOSTIC_BYTES),
        });
        observer.finalize().unwrap();
        assert_eq!(observer.recv().await.unwrap().event.name, "ready");
        let payload = observer.recv().await.unwrap().event;
        assert!(payload.sensitive);
        assert_eq!(payload.name, "request_adapted");
        assert_eq!(payload.data["format"], "test.request_body");
        assert_eq!(payload.data["schema_version"], 1);
        assert_eq!(payload.data["payload"]["text"], "body");
        assert_eq!(payload.correlation.attempt_id.as_deref(), Some("attempt"));
        let omission = observer.recv().await.unwrap().event;
        assert_eq!(omission.name, "payload_omitted");
        assert_eq!(omission.data["complete"], false);
        assert_eq!(observer.recv().await.unwrap().event.name, "finalized");
    });
}

#[test]
fn accepted_usage_and_retry_keep_original_semantics_without_message_leaks() {
    run(async {
        let hub = XrayHub::default();
        let mut observer = observe(&hub, true);
        let state = crate::TurnState::default()
            .with_diagnostics(ProviderDiagnostics::new(Some(context(&hub))));
        let usage = ProviderTurnEvent::Usage {
            usage: TokenUsage {
                input_tokens: Some(12),
                output_tokens: Some(3),
                ..TokenUsage::default()
            },
        };
        let retry = ProviderTurnEvent::RetryScheduled {
            message: "private provider error/credential".into(),
            retry_at_unix: 7,
        };
        state.push(usage.clone());
        state.push(retry.clone());
        state.push(ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::EndTurn,
        });
        state.push(usage.clone()); // stale post-terminal usage is not observed or accepted
        let events = state.drain();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0], usage);
        assert_eq!(events[1], retry);
        observer.finalize().unwrap();
        assert_eq!(observer.recv().await.unwrap().event.name, "ready");
        let observed_usage = observer.recv().await.unwrap().event;
        assert_eq!(observed_usage.data["input_tokens"], 12);
        assert_eq!(
            observed_usage.data["semantics"],
            "final_cumulative_per_request"
        );
        let observed_retry = observer.recv().await.unwrap().event;
        assert_eq!(observed_retry.data, json!({ "retry_at_unix": 7 }));
        assert_eq!(observer.recv().await.unwrap().event.name, "turn_finished");
        assert_eq!(observer.recv().await.unwrap().event.name, "finalized");
    });
}

#[test]
fn origin_context_crosses_provider_runtime_spawn_and_blocking_dispatch() {
    run(async {
        let hub = XrayHub::default();
        let origin = context(&hub);
        let runtime = crate::ProviderRuntime::new().unwrap();
        origin
            .scope(async {
                let captured = runtime
                    .spawn(async {
                        ProviderDiagnostics::current()
                            .capture_context()
                            .unwrap()
                            .correlation()
                            .clone()
                    })
                    .await
                    .unwrap();
                assert_eq!(&captured, origin.correlation());
                let blocking = runtime
                    .block_on(async {
                        ProviderDiagnostics::current()
                            .capture_context()
                            .unwrap()
                            .correlation()
                            .clone()
                    })
                    .unwrap();
                assert_eq!(&blocking, origin.correlation());
            })
            .await;
        runtime.shutdown(std::time::Duration::from_secs(2)).unwrap();
        assert!(CaptureContext::current().is_none());
    });
}
