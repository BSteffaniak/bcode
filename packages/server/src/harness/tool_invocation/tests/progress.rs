//! Bounded, payload-free diagnostics for the aggregate production-path fixture.
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Instant,
};
use tracing_subscriber::{Layer, layer::Context};

type Stages = Arc<Mutex<VecDeque<(&'static str, u128)>>>;

#[derive(Clone)]
pub(super) struct Progress {
    started: Instant,
    stages: Stages,
}

impl Progress {
    pub(super) fn new() -> Self {
        Self {
            started: Instant::now(),
            stages: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub(super) async fn branch<T>(
        &self,
        branch: &'static str,
        index: usize,
        future: impl std::future::Future<Output = T>,
    ) -> T {
        let mut observation = Branch {
            name: branch,
            index,
            started: Instant::now(),
            epoch: self.started,
            complete: false,
        };
        eprintln!(
            "fixture branch={branch} index={index} state=start elapsed_ms={}",
            self.started.elapsed().as_millis()
        );
        let value = future.await;
        observation.complete = true;
        value
    }

    pub(super) async fn request<T>(
        &self,
        branch: &'static str,
        index: usize,
        future: impl std::future::Future<Output = T>,
    ) -> T {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.branch(branch, index, future),
        )
        .await;
        result.unwrap_or_else(|_| {
            self.dump();
            panic!("fixture branch={branch} index={index} exceeded operation liveness deadline");
        })
    }

    pub(super) fn dump(&self) {
        for (stage, elapsed) in self.stages.lock().unwrap().iter() {
            eprintln!("fixture stage={stage} elapsed_ms={elapsed}");
        }
    }
}

struct Branch {
    name: &'static str,
    index: usize,
    started: Instant,
    epoch: Instant,
    complete: bool,
}

impl Drop for Branch {
    fn drop(&mut self) {
        let state = if self.complete { "complete" } else { "pending" };
        eprintln!(
            "fixture branch={} index={} state={state} elapsed_ms={} duration_ms={}",
            self.name,
            self.index,
            self.epoch.elapsed().as_millis(),
            self.started.elapsed().as_millis(),
        );
    }
}

#[derive(Default)]
struct Stage(Option<&'static str>);
impl tracing::field::Visit for Stage {
    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() != "stage" {
            return;
        }
        self.0 = match value {
            "invoke_start" => Some("invoke_start"),
            "invoke_authority_done" => Some("invoke_authority_done"),
            "invoke_retained_start" => Some("invoke_retained_start"),
            "invoke_retained_done" => Some("invoke_retained_done"),
            "invoke_context_start" => Some("invoke_context_start"),
            "invoke_context_done" => Some("invoke_context_done"),
            "invoke_runtime_start" => Some("invoke_runtime_start"),
            "invoke_runtime_done" => Some("invoke_runtime_done"),
            "pinned_authority_start" => Some("pinned_authority_start"),
            "pinned_authority_done" => Some("pinned_authority_done"),
            "pinned_claim_start" => Some("pinned_claim_start"),
            "pinned_claim_done" => Some("pinned_claim_done"),
            "pinned_final_authority_start" => Some("pinned_final_authority_start"),
            "pinned_start_start" => Some("pinned_start_start"),
            "pinned_start_done" => Some("pinned_start_done"),
            "pinned_dispatch_start" => Some("pinned_dispatch_start"),
            "pinned_dispatch_done" => Some("pinned_dispatch_done"),
            "pinned_result_start" => Some("pinned_result_start"),
            "pinned_result_done" => Some("pinned_result_done"),
            "handle_start" => Some("handle_start"),
            "handle_done" => Some("handle_done"),
            "gate_lock_start" => Some("gate_lock_start"),
            "gate_lock_done" => Some("gate_lock_done"),
            _ => None,
        };
    }
}

impl<S: tracing::Subscriber> Layer<S> for Progress {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        if !matches!(
            event.metadata().target(),
            "bcode_server::harness" | "bcode_session::harness"
        ) {
            return;
        }
        let mut stage = Stage::default();
        event.record(&mut stage);
        if let Some(stage) = stage.0 {
            let mut stages = self.stages.lock().unwrap();
            if stages.len() == 256 {
                let _ = stages.pop_front();
            }
            stages.push_back((stage, self.started.elapsed().as_millis()));
        }
    }
}
