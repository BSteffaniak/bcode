//! Bounded latest-only background Markdown projection coordination.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bcode_markdown_render::{
    MarkdownRenderOptions, MarkdownRenderResult, MarkdownStreamingRenderState,
};

/// Complete identity for one transcript Markdown render generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownProjectionGeneration {
    pub item_id: u64,
    pub item_revision: u64,
    pub options: MarkdownRenderOptions,
}

/// Immutable CPU-work request published to the Markdown worker.
#[derive(Debug, Clone)]
pub struct MarkdownProjectionRequest {
    pub generation: MarkdownProjectionGeneration,
    pub source: String,
}

/// Secret-safe failure returned when projection work cannot complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownProjectionFailure {
    /// The renderer terminated unexpectedly while processing untrusted Markdown.
    RendererPanicked,
}

/// Result of one background Markdown projection attempt.
#[derive(Debug, Clone)]
pub enum MarkdownProjectionOutcome {
    /// Successfully rendered rows and semantic sidecars.
    Rendered(Arc<MarkdownRenderResult>),
    /// Normalized failure that contains no source or renderer-private details.
    Failed(MarkdownProjectionFailure),
}

/// Completed Markdown projection returned by the worker.
#[derive(Debug, Clone)]
pub struct MarkdownProjectionCompletion {
    pub generation: MarkdownProjectionGeneration,
    pub outcome: MarkdownProjectionOutcome,
    pub render_duration: Duration,
}

struct WorkerMailbox {
    request: Option<MarkdownProjectionRequest>,
    shutdown: bool,
}

type MarkdownRenderer = dyn Fn(&mut MarkdownStreamingRenderState, &str, &MarkdownRenderOptions) -> MarkdownRenderResult
    + Send
    + Sync;

/// One active and one replaceable latest pending Markdown request.
pub struct MarkdownProjectionCoordinator {
    mailbox: Arc<(Mutex<WorkerMailbox>, Condvar)>,
    completion_rx: tokio::sync::watch::Receiver<Option<MarkdownProjectionCompletion>>,
    latest_requested: Option<MarkdownProjectionGeneration>,
    worker: JoinHandle<()>,
}

impl MarkdownProjectionCoordinator {
    /// Start a dedicated blocking worker that consumes only the latest request.
    #[must_use]
    pub fn new() -> Self {
        Self::new_with_renderer(MarkdownStreamingRenderState::render)
    }

    fn new_with_renderer(
        renderer: impl Fn(
            &mut MarkdownStreamingRenderState,
            &str,
            &MarkdownRenderOptions,
        ) -> MarkdownRenderResult
        + Send
        + Sync
        + 'static,
    ) -> Self {
        let mailbox = Arc::new((
            Mutex::new(WorkerMailbox {
                request: None,
                shutdown: false,
            }),
            Condvar::new(),
        ));
        let worker_mailbox = Arc::clone(&mailbox);
        let renderer: Arc<MarkdownRenderer> = Arc::new(renderer);
        let (completion_tx, completion_rx) = tokio::sync::watch::channel(None);
        let worker = std::thread::spawn(move || {
            let mut state = MarkdownStreamingRenderState::default();
            loop {
                let request = {
                    let (lock, changed) = &*worker_mailbox;
                    let mut mailbox = lock
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    while mailbox.request.is_none() && !mailbox.shutdown {
                        mailbox = changed
                            .wait(mailbox)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                    if mailbox.shutdown {
                        return;
                    }
                    mailbox.request.take()
                };
                let Some(request) = request else {
                    continue;
                };
                let started = Instant::now();
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    renderer(&mut state, &request.source, &request.generation.options)
                }))
                .map_or_else(
                    |_| {
                        state = MarkdownStreamingRenderState::default();
                        MarkdownProjectionOutcome::Failed(
                            MarkdownProjectionFailure::RendererPanicked,
                        )
                    },
                    |result| MarkdownProjectionOutcome::Rendered(Arc::new(result)),
                );
                if completion_tx
                    .send(Some(MarkdownProjectionCompletion {
                        generation: request.generation,
                        outcome,
                        render_duration: started.elapsed(),
                    }))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            mailbox,
            completion_rx,
            latest_requested: None,
            worker,
        }
    }

    /// Replace pending work with the newest request.
    pub fn request(&mut self, request: MarkdownProjectionRequest) {
        self.latest_requested = Some(request.generation.clone());
        let (lock, changed) = &*self.mailbox;
        lock.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .request = Some(request);
        changed.notify_one();
    }

    /// Return the exact generation currently requested by the TUI.
    #[must_use]
    pub const fn latest_requested(&self) -> Option<&MarkdownProjectionGeneration> {
        self.latest_requested.as_ref()
    }

    /// Mark a successful generation complete.
    pub fn complete(&mut self, generation: &MarkdownProjectionGeneration) {
        if self.latest_requested.as_ref() == Some(generation) {
            self.latest_requested = None;
        }
    }

    /// Return a receiver observing the latest completed projection slot.
    #[must_use]
    pub fn completion_receiver(
        &self,
    ) -> tokio::sync::watch::Receiver<Option<MarkdownProjectionCompletion>> {
        self.completion_rx.clone()
    }

    /// Return the newest completed generation, discarding stale completions.
    #[cfg(test)]
    pub fn try_latest_completion(&mut self) -> Option<MarkdownProjectionCompletion> {
        if !self.completion_rx.has_changed().unwrap_or(false) {
            return None;
        }
        let completion = self.completion_rx.borrow_and_update().clone();
        completion
            .filter(|completion| self.latest_requested.as_ref() == Some(&completion.generation))
    }

    /// Forget the accepted generation when its item/session leaves residency.
    pub fn invalidate(&mut self) {
        self.latest_requested = None;
        self.mailbox
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .request = None;
        let _ = self.completion_rx.borrow_and_update();
    }
}

impl Default for MarkdownProjectionCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MarkdownProjectionCoordinator {
    fn drop(&mut self) {
        let (lock, changed) = &*self.mailbox;
        lock.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shutdown = true;
        changed.notify_one();
        let worker = std::mem::replace(&mut self.worker, std::thread::spawn(|| {}));
        let _ = worker.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::sleep;

    fn request(revision: u64, source: &str) -> MarkdownProjectionRequest {
        MarkdownProjectionRequest {
            generation: MarkdownProjectionGeneration {
                item_id: 7,
                item_revision: revision,
                options: MarkdownRenderOptions::new(40).with_streaming(true),
            },
            source: source.to_owned(),
        }
    }

    async fn wait_latest(
        coordinator: &mut MarkdownProjectionCoordinator,
    ) -> MarkdownProjectionCompletion {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(completion) = coordinator.try_latest_completion() {
                return completion;
            }
            assert!(
                Instant::now() < deadline,
                "Markdown projection worker did not complete"
            );
            sleep(Duration::from_millis(2)).await;
        }
    }

    fn rendered(completion: &MarkdownProjectionCompletion) -> &MarkdownRenderResult {
        let MarkdownProjectionOutcome::Rendered(result) = &completion.outcome else {
            panic!("expected rendered Markdown projection");
        };
        result
    }

    #[tokio::test]
    async fn completion_slot_replaces_an_unobserved_older_generation() {
        let mut coordinator = MarkdownProjectionCoordinator::new();
        coordinator.request(request(1, "first"));
        for _ in 0..100 {
            if coordinator
                .completion_rx
                .borrow()
                .as_ref()
                .is_some_and(|completion| completion.generation.item_revision == 1)
            {
                break;
            }
            sleep(Duration::from_millis(2)).await;
        }
        coordinator.request(request(2, "latest"));

        let completion = wait_latest(&mut coordinator).await;
        assert_eq!(completion.generation.item_revision, 2);
    }

    #[tokio::test]
    async fn burst_requests_accept_only_latest_generation() {
        let mut coordinator = MarkdownProjectionCoordinator::new();
        for revision in 1..=32 {
            coordinator.request(request(revision, &format!("revision {revision}")));
        }
        let completion = wait_latest(&mut coordinator).await;
        assert_eq!(completion.generation.item_revision, 32);
        assert!(!rendered(&completion).lines.is_empty());
    }

    #[tokio::test]
    async fn stale_completion_is_rejected_after_new_generation() {
        let mut coordinator = MarkdownProjectionCoordinator::new();
        coordinator.request(request(1, &"old ".repeat(10_000)));
        coordinator.request(request(2, "latest"));
        let completion = wait_latest(&mut coordinator).await;
        assert_eq!(completion.generation.item_revision, 2);
    }

    #[tokio::test]
    async fn resize_options_race_accepts_only_latest_width() {
        let mut coordinator = MarkdownProjectionCoordinator::new();
        coordinator.request(request(1, &"wide content ".repeat(2_000)));
        let mut narrow = request(1, "narrow latest");
        narrow.generation.options.width = 18;
        coordinator.request(narrow);

        let completion = wait_latest(&mut coordinator).await;
        assert_eq!(completion.generation.options.width, 18);
        assert!(!rendered(&completion).lines.is_empty());
    }

    #[tokio::test]
    async fn worker_recovers_on_next_generation_after_normalized_failure() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let worker_attempts = Arc::clone(&attempts);
        let mut coordinator =
            MarkdownProjectionCoordinator::new_with_renderer(move |state, source, options| {
                assert!(
                    worker_attempts.fetch_add(1, Ordering::Relaxed) != 0,
                    "test renderer failure"
                );
                state.render(source, options)
            });
        coordinator.request(request(1, "fails"));
        let failed = wait_latest(&mut coordinator).await;
        assert!(matches!(
            failed.outcome,
            MarkdownProjectionOutcome::Failed(MarkdownProjectionFailure::RendererPanicked)
        ));

        coordinator.request(request(2, "recovers"));
        let recovered = wait_latest(&mut coordinator).await;
        assert_eq!(recovered.generation.item_revision, 2);
        assert!(!rendered(&recovered).lines.is_empty());
    }

    #[tokio::test]
    async fn invalidation_discards_pending_and_completed_work() {
        let mut coordinator = MarkdownProjectionCoordinator::new();
        coordinator.request(request(1, "pending"));
        coordinator.invalidate();
        sleep(Duration::from_millis(10)).await;
        assert!(coordinator.try_latest_completion().is_none());
    }
}
