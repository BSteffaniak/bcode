//! Content-free workflow diagnostics. Capture is observational, never execution authority.
//!
//! Callers must supply the originating context before crossing a task boundary. This
//! module deliberately owns neither a run registry nor a daemon-global capture hub.
use std::future::Future;
use std::time::Instant;

use bcode_xray::CaptureContext;

/// Resolve the actual current canonical association, never a guessed ambient session.
/// Failure disables diagnostics only; it does not change dispatch/ownership outcomes.
/// Call this at background-driver entry before spawning descendants.
pub async fn originating_run_context(
    state: &crate::ServerState,
    run_id: &str,
) -> Option<CaptureContext> {
    let run = crate::workflow_representation::status(state, run_id)
        .await
        .ok()??;
    let session_id = run.parent_session_id?;
    // Session spelling is canonical and must not be inferred from historical provenance.
    let session = session_id.parse::<bcode_session_models::SessionId>().ok()?;
    Some(owner_run_context(&state.xray, &session.to_string(), run_id))
}

fn owner_run_context(hub: &bcode_xray::XrayHub, session_id: &str, run_id: &str) -> CaptureContext {
    let origin = CaptureContext::current()
        .filter(|context| {
            context.correlation().session_id == session_id
                && context
                    .correlation()
                    .run_id
                    .as_deref()
                    .is_none_or(|current| current == run_id)
        })
        .unwrap_or_else(|| {
            hub.context(bcode_xray_models::XrayCorrelation {
                session_id: session_id.to_owned(),
                ..Default::default()
            })
        });
    let mut correlation = origin.correlation().clone();
    correlation.run_id = Some(run_id.to_owned());
    correlation.activation_id = None;
    correlation.attempt_id = None;
    origin.with_correlation(correlation)
}

/// Bind a run without borrowing an unrelated activation or attempt from the caller.
pub fn run_context(origin: Option<CaptureContext>, run_id: &str) -> Option<CaptureContext> {
    let context = origin?;
    let mut correlation = context.correlation().clone();
    correlation.run_id = Some(run_id.to_owned());
    correlation.activation_id = None;
    correlation.attempt_id = None;
    Some(context.with_correlation(correlation))
}

/// Bind the exact activation; a dispatch identity, not an attempt number, identifies attempts.
pub fn activation_context(
    origin: Option<CaptureContext>,
    run_id: &str,
    activation_id: &str,
    dispatch_identity: Option<&str>,
) -> Option<CaptureContext> {
    let context = run_context(origin, run_id)?;
    let mut correlation = context.correlation().clone();
    correlation.activation_id = Some(activation_id.to_owned());
    correlation.attempt_id = dispatch_identity.map(str::to_owned);
    Some(context.with_correlation(correlation))
}

/// Explicitly carry the originating context into a future, including a spawned future.
pub fn scope<F: Future>(
    context: Option<CaptureContext>,
    future: F,
) -> impl Future<Output = F::Output> {
    // Allocate before constructing the scoped future so every caller stores only a
    // pointer, not a second inline copy of the entire workflow execution future.
    let future = Box::pin(future);
    async move {
        match context {
            Some(context) => context.scope(future).await,
            None => future.await,
        }
    }
}

/// A bounded producer-side boundary. Dropping it never claims cancellation or success.
///
/// The engine owns nonblocking queue admission, observer filtering and gap accounting.
/// Only fixed operation/outcome names and opaque identities enter this producer.
pub struct Boundary {
    context: Option<CaptureContext>,
    operation: &'static str,
    identity: uuid::Uuid,
    started: Instant,
    finished: bool,
}

impl Boundary {
    #[must_use]
    pub fn new(context: Option<CaptureContext>, operation: &'static str) -> Self {
        let boundary = Self {
            context,
            operation,
            identity: uuid::Uuid::new_v4(),
            started: Instant::now(),
            finished: false,
        };
        boundary.emit("started", "pending");
        boundary
    }

    pub fn finish(mut self, outcome: &'static str) {
        self.finished = true;
        self.emit("finished", outcome);
    }

    pub fn finish_result<T, E>(self, result: &Result<T, E>) {
        self.finish(if result.is_ok() { "ok" } else { "error" });
    }

    fn emit(&self, phase: &'static str, outcome: &'static str) {
        if let Some(context) = self
            .context
            .as_ref()
            .filter(|context| context.capture_active())
        {
            context.emit(
                "workflow",
                self.operation,
                serde_json::json!({
                    "boundary_id": self.identity.to_string(),
                    "phase": phase,
                    "outcome": outcome,
                    "elapsed_ms": u64::try_from(self.started.elapsed().as_millis())
                        .unwrap_or(u64::MAX),
                }),
                false,
            );
        }
    }
}

impl Drop for Boundary {
    fn drop(&mut self) {
        if !self.finished {
            self.emit("finished", "unfinished");
        }
    }
}

/// Emit only a normalized domain fact, never receipt content or arbitrary errors.
pub fn fact(context: Option<&CaptureContext>, name: &str, outcome: &'static str) {
    if let Some(context) = context.filter(|context| context.capture_active()) {
        context.emit(
            "workflow",
            name,
            serde_json::json!({ "outcome": outcome }),
            false,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_xray::XrayHub;
    use bcode_xray_models::{XrayCorrelation, XrayObserveRequest};

    fn origin(hub: &XrayHub, session: &str) -> CaptureContext {
        hub.context(XrayCorrelation {
            session_id: session.into(),
            turn_id: Some("origin-turn".into()),
            ..XrayCorrelation::default()
        })
    }

    #[test]
    // The intentionally inline state verifies that scoping boxes the future itself.
    #[allow(clippy::large_stack_arrays)]
    fn scoped_future_keeps_large_execution_state_out_of_its_callers() {
        let operation = async {
            let state = [7_u8; 64 * 1024];
            std::future::pending::<()>().await;
            state
        };
        assert!(std::mem::size_of_val(&operation) >= 64 * 1024);
        let scoped = scope(None, operation);
        assert!(std::mem::size_of_val(&scoped) < 1024);
    }

    #[tokio::test]
    async fn cancelling_boxed_scope_drops_the_same_in_flight_operation() {
        struct OnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = OnDrop(dropped.clone());
        let (entered, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(scope(None, async move {
            let _guard = guard;
            let _ = entered.send(());
            std::future::pending::<()>().await;
        }));
        ready.await.expect("operation entered");
        task.abort();
        assert!(task.await.expect_err("aborted").is_cancelled());
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn canonical_owner_replaces_foreign_or_absent_background_task_context() {
        let hub = XrayHub::default();
        let absent = owner_run_context(&hub, "owner", "run");
        assert_eq!(absent.correlation().session_id, "owner");
        assert_eq!(absent.correlation().run_id.as_deref(), Some("run"));
        let foreign = activation_context(
            Some(origin(&hub, "foreign")),
            "other-run",
            "other-activation",
            Some("other-attempt"),
        )
        .expect("foreign");
        foreign
            .scope(async {
                let actual = owner_run_context(&hub, "owner", "run");
                assert_eq!(actual.correlation().session_id, "owner");
                assert!(actual.correlation().turn_id.is_none());
                assert!(actual.correlation().activation_id.is_none());
                assert!(actual.correlation().attempt_id.is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn explicit_context_survives_spawn_and_does_not_take_ambient_identity() {
        let hub = XrayHub::default();
        let expected = activation_context(
            Some(origin(&hub, "origin")),
            "run",
            "activation",
            Some("dispatch"),
        )
        .expect("origin");
        let foreign = origin(&hub, "foreign");
        let task = foreign
            .scope(async move {
                tokio::spawn(scope(Some(expected), async {
                    CaptureContext::current()
                        .expect("explicit scope")
                        .correlation()
                        .clone()
                }))
                .await
            })
            .await;
        let actual = task.expect("task");
        assert_eq!(actual.session_id, "origin");
        assert_eq!(actual.turn_id.as_deref(), Some("origin-turn"));
        assert_eq!(actual.run_id.as_deref(), Some("run"));
        assert_eq!(actual.activation_id.as_deref(), Some("activation"));
        assert_eq!(actual.attempt_id.as_deref(), Some("dispatch"));
    }

    #[test]
    fn run_binding_discards_caller_attempt_and_does_not_invent_an_origin() {
        let hub = XrayHub::default();
        let attempt = activation_context(
            Some(origin(&hub, "session")),
            "old",
            "activation",
            Some("attempt"),
        );
        let run = run_context(attempt, "new").expect("run");
        assert_eq!(run.correlation().run_id.as_deref(), Some("new"));
        assert!(run.correlation().activation_id.is_none());
        assert!(run.correlation().attempt_id.is_none());
        assert!(run_context(None, "new").is_none());
    }

    #[tokio::test]
    async fn boundary_lifetime_does_not_keep_sensitive_capture_enabled() {
        let hub = XrayHub::default();
        let observation = hub
            .observe(XrayObserveRequest {
                schema_version: 1,
                session_id: "session".into(),
                deep_capture: true,
            })
            .expect("observe");
        let context = origin(&hub, "session");
        let boundary = Boundary::new(Some(context.clone()), "test.lifetime");
        assert!(context.deep_capture_active());
        drop(observation);
        assert!(!context.capture_active());
        assert!(!context.deep_capture_active());
        boundary.finish("ok");
        assert!(!hub.capture_active("session"));
    }

    #[tokio::test]
    async fn dropped_boundary_is_unfinished_and_explicit_completion_is_not_reopened() {
        let hub = XrayHub::default();
        let mut observation = hub
            .observe(XrayObserveRequest {
                schema_version: 1,
                session_id: "session".into(),
                deep_capture: false,
            })
            .expect("observe");
        let _ready = observation.recv().await.expect("ready");
        let context = Some(origin(&hub, "session"));
        drop(Boundary::new(context.clone(), "test.aborted"));
        Boundary::new(context, "test.completed").finish("ok");
        let mut events = Vec::new();
        for _ in 0..4 {
            events.push(observation.recv().await.expect("boundary event").event);
        }
        assert_eq!(events[0].data["phase"], "started");
        assert_eq!(events[1].data["outcome"], "unfinished");
        assert_eq!(events[2].data["phase"], "started");
        assert_eq!(events[3].data["outcome"], "ok");
        assert_eq!(events[0].data["boundary_id"], events[1].data["boundary_id"]);
        assert_ne!(events[0].data["boundary_id"], events[2].data["boundary_id"]);
        assert!(events.iter().all(|event| !event.sensitive));
    }
}
