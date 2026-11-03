//! Ordinary-event ingress ownership. Consumer locks never participate in admission.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::task::{Wake, Waker};

use tokio::sync::Notify;

use super::XrayEvent;

struct Progress(Arc<Notify>);

impl Wake for Progress {
    fn wake(self: Arc<Self>) {
        self.0.notify_waiters();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.notify_waiters();
    }
}

pub struct Budget {
    operations: AtomicUsize,
    reporters: AtomicUsize,
    bytes: AtomicUsize,
    pub notify: Arc<Notify>,
    pub waker: Waker,
}

impl Budget {
    pub fn new() -> Arc<Self> {
        Self::with_notify(Arc::new(Notify::new()))
    }

    pub fn with_notify(notify: Arc<Notify>) -> Arc<Self> {
        Arc::new(Self {
            operations: AtomicUsize::new(0),
            reporters: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
            waker: Waker::from(Arc::new(Progress(Arc::clone(&notify)))),
            notify,
        })
    }

    /// Register before inspecting facts that can produce global uncertainty.
    /// A guard is stack-owned by each synchronous reporter; its count cannot
    /// exceed the number of simultaneously live stack guards in address space.
    pub fn reporter(self: &Arc<Self>) -> Reporter {
        self.reporters.fetch_add(1, Ordering::SeqCst);
        Reporter(Arc::clone(self))
    }

    #[cfg(test)]
    pub fn used_bytes(&self) -> usize {
        self.bytes.load(Ordering::Acquire)
    }

    pub fn reporters_settled(&self) -> bool {
        self.reporters.load(Ordering::SeqCst) == 0
    }

    pub fn operation(self: &Arc<Self>, limit: usize) -> Option<Operation> {
        reserve(&self.operations, 1, limit).then(|| Operation(Arc::clone(self)))
    }

    pub fn bytes(self: &Arc<Self>, count: usize, limit: usize) -> Option<Bytes> {
        reserve(&self.bytes, count, limit).then(|| Bytes {
            budget: Arc::clone(self),
            count,
        })
    }
}

fn reserve(counter: &AtomicUsize, count: usize, limit: usize) -> bool {
    counter
        .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(count).filter(|next| *next <= limit)
        })
        .is_ok()
}

pub struct Reporter(Arc<Budget>);

impl Drop for Reporter {
    fn drop(&mut self) {
        self.0.reporters.fetch_sub(1, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }
}

pub struct Operation(Arc<Budget>);

impl Drop for Operation {
    fn drop(&mut self) {
        self.0.operations.fetch_sub(1, Ordering::Release);
        self.0.notify.notify_waiters();
    }
}

pub struct Bytes {
    budget: Arc<Budget>,
    count: usize,
}

impl Bytes {
    pub fn notify(&self) {
        self.budget.notify.notify_waiters();
    }
}

impl Drop for Bytes {
    fn drop(&mut self) {
        self.budget.bytes.fetch_sub(self.count, Ordering::Release);
    }
}

#[derive(Default)]
pub struct Admission {
    // Registration occurs under the lifecycle registry's shared read lock; close
    // holds its write lock. Thus no entrant can increment after close.
    pub closed: AtomicBool,
    pub active: AtomicUsize,
    pub pending: AtomicUsize,
    pub loss: AtomicU64,
}

impl Admission {
    pub fn settled(&self) -> bool {
        self.active.load(Ordering::Acquire) == 0 && self.pending.load(Ordering::Acquire) == 0
    }

    pub fn lose(&self) {
        let _ = self
            .loss
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |loss| {
                Some(loss.saturating_add(1))
            });
    }
}

pub struct Target {
    pub id: String,
    pub admission: Arc<Admission>,
    pub deliver: bool,
    /// Monotonic admission time, not the later drain/processing time.
    pub admitted_at: std::time::Instant,
}

/// Owns all registrations until publication or abandonment has been accounted.
pub struct Publication {
    pub targets: Vec<Target>,
    budget: Arc<Budget>,
    _operation: Operation,
    sender: tokio::sync::mpsc::Sender<Command>,
    published: bool,
}

impl Publication {
    pub fn new(
        targets: Vec<Target>,
        budget: &Arc<Budget>,
        operation: Operation,
        sender: tokio::sync::mpsc::Sender<Command>,
    ) -> Self {
        // At most max_inflight_operations registrations can exist per target.
        for target in &targets {
            target.admission.active.fetch_add(1, Ordering::Relaxed);
        }
        Self {
            targets,
            budget: Arc::clone(budget),
            _operation: operation,
            sender,
            published: false,
        }
    }

    pub fn sender(&self) -> tokio::sync::mpsc::Sender<Command> {
        self.sender.clone()
    }

    pub fn command(&self, event: XrayEvent, bytes: Bytes) -> Command {
        let targets = self
            .targets
            .iter()
            .map(|target| Target {
                id: target.id.clone(),
                admission: Arc::clone(&target.admission),
                deliver: target.deliver,
                admitted_at: target.admitted_at,
            })
            .collect::<Vec<_>>();
        // All fallible allocation is complete before pending ownership changes.
        for target in &targets {
            target.admission.pending.fetch_add(1, Ordering::Release);
        }
        Command {
            event,
            targets,
            bytes,
        }
    }

    pub const fn published(&mut self) {
        self.published = true;
    }
}

impl Drop for Publication {
    fn drop(&mut self) {
        for target in &self.targets {
            if !self.published {
                target.admission.lose();
            }
            target.admission.active.fetch_sub(1, Ordering::Release);
        }
        self.budget.notify.notify_waiters();
    }
}

pub struct Command {
    pub event: XrayEvent,
    pub targets: Vec<Target>,
    bytes: Bytes,
}

impl Drop for Command {
    fn drop(&mut self) {
        for target in &self.targets {
            target.admission.pending.fetch_sub(1, Ordering::Release);
        }
        // A different observation may have drained this command after a finalizer
        // went to sleep. Settlement itself must wake that finalizer; publication
        // and publisher retirement notifications may already have been consumed.
        self.bytes.budget.notify.notify_waiters();
    }
}
