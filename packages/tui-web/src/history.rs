//! Engine history requests executed over the browser feed.
//!
//! The engine issues [`HostRequest`]s as data. Each one runs the shared bounded loader
//! ([`bcode_tui_transcript::window::load_history`]), the same implementation the native host
//! runs, as a suspended job: every read the loader makes becomes one claim the JavaScript host
//! transports to [`crate::feed::HISTORY_PAGE_ROUTE`] and answers with the response bytes. The
//! job resumes with the decoded page and either asks for its next read or completes with the
//! engine's page or window result. No I/O, clock or executor lives here.
//!
//! * One job per lane ([`HistoryLane`]): a newer request in a lane supersedes the older job,
//!   whose late response is then a stale claim.
//! * A failed read leaves the engine request unanswered; the host re-issues it with backoff
//!   while the engine still awaits it ([`HistoryRetries`]).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use bcode_tui_transcript::engine::HostRequest;
use bcode_tui_transcript::window::{
    HistoryLane, HistoryLoadError, HistoryLoadRequest, HistoryLoaded, SessionHistoryPage,
    SessionHistoryQuery, SessionId, load_history,
};
use web_time::Instant;

use crate::feed::{FeedError, HistoryPageRequest};

/// Delay before the first re-issue of a failed history request; doubled for every further
/// consecutive failure in the lane up to [`MAX_RETRY_DELAY`] (as the native host).
pub const FIRST_RETRY_DELAY: Duration = Duration::from_millis(250);
/// Longest delay between re-issues of a failing history request.
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(8);

type LoadResult = Result<HistoryLoaded, HistoryLoadError<FeedError>>;

/// The read a suspended job waits for, and the answer delivered to it.
#[derive(Default)]
struct ReadSlot {
    /// The query the loader asked for and the host has not answered yet.
    query: Option<SessionHistoryQuery>,
    /// The decoded answer, taken by the loader when it resumes.
    page: Option<Result<SessionHistoryPage, FeedError>>,
}

/// One read the loader awaits; resolves once the host delivered its answer.
struct PendingRead(Rc<RefCell<ReadSlot>>);

impl Future for PendingRead {
    type Output = Result<SessionHistoryPage, FeedError>;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.0
            .borrow_mut()
            .page
            .take()
            .map_or(Poll::Pending, Poll::Ready)
    }
}

/// One engine request being loaded.
struct Job {
    request: HostRequest,
    slot: Rc<RefCell<ReadSlot>>,
    /// Claim of the read handed to the host, while it is in flight.
    claim: Option<u64>,
    future: Pin<Box<dyn Future<Output = LoadResult>>>,
}

impl Job {
    fn new(session_id: SessionId, request: HostRequest, resident_last: Option<u64>) -> Self {
        let slot = Rc::new(RefCell::new(ReadSlot::default()));
        let reader = Rc::clone(&slot);
        let future = load_history(
            HistoryLoadRequest {
                session_id,
                request,
                resident_last,
            },
            move |query| {
                reader.borrow_mut().query = Some(query);
                PendingRead(Rc::clone(&reader))
            },
        );
        Self {
            request,
            slot,
            claim: None,
            future: Box::pin(future),
        }
    }

    /// Run the loader until it needs a read it has not been given, or completes.
    fn poll(&mut self) -> Option<LoadResult> {
        let mut context = Context::from_waker(Waker::noop());
        match self.future.as_mut().poll(&mut context) {
            Poll::Ready(result) => Some(result),
            Poll::Pending => None,
        }
    }
}

/// One read for the host to transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRead {
    /// Claim the answer is delivered with.
    pub claim: u64,
    /// JSON body for [`crate::feed::HISTORY_PAGE_ROUTE`].
    pub body: String,
}

/// A completed engine request.
pub struct Completed {
    /// The request.
    pub request: HostRequest,
    /// The loader result.
    pub result: LoadResult,
}

/// Suspended history loads of one attached session, one per lane.
pub struct HistoryPump {
    session_id: SessionId,
    jobs: BTreeMap<HistoryLane, Job>,
    next_claim: u64,
}

impl HistoryPump {
    /// No loads for `session_id`.
    #[must_use]
    pub const fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            jobs: BTreeMap::new(),
            next_claim: 0,
        }
    }

    /// Start loading `request`, superseding the job in its lane. `resident_last` is the newest
    /// resident sequence (`LiveState` reads after it, W11). A request that completes without
    /// any read is returned at once.
    pub fn start(&mut self, request: HostRequest, resident_last: Option<u64>) -> Option<Completed> {
        let lane = HistoryLane::of(&request);
        let mut job = Job::new(self.session_id, request, resident_last);
        if let Some(result) = job.poll() {
            self.jobs.remove(&lane);
            return Some(Completed { request, result });
        }
        self.jobs.insert(lane, job);
        None
    }

    /// Whether a job is loading `request`.
    #[must_use]
    pub fn is_loading(&self, request: &HostRequest) -> bool {
        self.jobs
            .get(&HistoryLane::of(request))
            .is_some_and(|job| job.request == *request)
    }

    /// Drop every job whose request `keep` rejects (superseded engine requests).
    pub fn retain(&mut self, mut keep: impl FnMut(&HostRequest) -> bool) {
        self.jobs.retain(|_, job| keep(&job.request));
    }

    /// The next read no claim covers yet, claimed for the host.
    ///
    /// # Errors
    ///
    /// Returns an error if the request body cannot be serialized.
    pub fn next_read(&mut self) -> Result<Option<HistoryRead>, &'static str> {
        let session_id = self.session_id;
        let Some(job) = self
            .jobs
            .values_mut()
            .find(|job| job.claim.is_none() && job.slot.borrow().query.is_some())
        else {
            return Ok(None);
        };
        let Some(query) = job.slot.borrow().query else {
            return Ok(None);
        };
        self.next_claim = self.next_claim.wrapping_add(1);
        job.claim = Some(self.next_claim);
        let body = serde_json::to_string(&HistoryPageRequest { session_id, query })
            .map_err(|_| "invalid history request")?;
        Ok(Some(HistoryRead {
            claim: self.next_claim,
            body,
        }))
    }

    /// Whether `claim` is a read in flight.
    #[must_use]
    pub fn owns(&self, claim: u64) -> bool {
        self.jobs.values().any(|job| job.claim == Some(claim))
    }

    /// Answer the read `claim` names. Returns the completed request when the loader finished;
    /// `None` when it asked for another read or the claim is stale.
    pub fn deliver(
        &mut self,
        claim: u64,
        page: Result<SessionHistoryPage, FeedError>,
    ) -> Option<Completed> {
        let lane = *self
            .jobs
            .iter()
            .find(|(_, job)| job.claim == Some(claim))?
            .0;
        let job = self.jobs.get_mut(&lane)?;
        job.claim = None;
        {
            let mut slot = job.slot.borrow_mut();
            slot.query = None;
            slot.page = Some(page);
        }
        let result = job.poll()?;
        let job = self.jobs.remove(&lane)?;
        Some(Completed {
            request: job.request,
            result,
        })
    }
}

/// Failed history requests waiting for a re-issue, at most one per lane.
#[derive(Debug, Default)]
pub struct HistoryRetries {
    due: BTreeMap<HistoryLane, (HostRequest, Instant)>,
    failures: BTreeMap<HistoryLane, u32>,
}

impl HistoryRetries {
    /// Schedule `request`, which failed at `at`, after its lane's backoff.
    pub fn schedule(&mut self, request: HostRequest, at: Instant) {
        let lane = HistoryLane::of(&request);
        let failures = self.failures.entry(lane).or_default();
        let delay = FIRST_RETRY_DELAY
            .saturating_mul(1_u32 << (*failures).min(5))
            .min(MAX_RETRY_DELAY);
        *failures = failures.saturating_add(1);
        self.due.insert(lane, (request, at + delay));
    }

    /// A result arrived for `request`: its lane's failure streak ends.
    pub fn clear(&mut self, request: &HostRequest) {
        let lane = HistoryLane::of(request);
        self.failures.remove(&lane);
        self.due.remove(&lane);
    }

    /// Earliest due retry.
    #[must_use]
    pub fn next_due(&self) -> Option<Instant> {
        self.due.values().map(|(_, due)| *due).min()
    }

    /// Remove and return the retries due at `now`.
    pub fn take_due(&mut self, now: Instant) -> Vec<HostRequest> {
        let (due, waiting): (BTreeMap<_, _>, BTreeMap<_, _>) = std::mem::take(&mut self.due)
            .into_iter()
            .partition(|(_, (_, at))| *at <= now);
        self.due = waiting;
        due.into_values().map(|(request, _)| request).collect()
    }
}

#[cfg(test)]
mod tests {
    use bcode_tui_transcript::window::{
        IssuedRequest, PageRequest, SessionEvent, SessionHistoryDirection, WindowRequest,
    };

    use super::*;

    fn event(session_id: SessionId, sequence: u64) -> SessionEvent {
        SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: format!("a{sequence}"),
            },
        }
    }

    fn page(
        session_id: SessionId,
        events: Vec<SessionEvent>,
        has_more: bool,
    ) -> SessionHistoryPage {
        SessionHistoryPage {
            session_id,
            events,
            compatibility_issues: Vec::new(),
            next_cursor: None,
            has_more,
        }
    }

    fn query(read: &HistoryRead) -> SessionHistoryQuery {
        let value: serde_json::Value = serde_json::from_str(&read.body).unwrap();
        serde_json::from_value(value["query"].clone()).unwrap()
    }

    #[test]
    fn a_window_completes_after_its_reads_and_bound_probes() {
        let session = SessionId::new();
        let mut pump = HistoryPump::new(session);
        let request = HostRequest::Window(IssuedRequest {
            generation: 1,
            request: WindowRequest::Tail,
        });
        assert!(pump.start(request, None).is_none());
        let read = pump.next_read().unwrap().unwrap();
        assert!(
            pump.next_read().unwrap().is_none(),
            "one read in flight per job"
        );
        assert_eq!(query(&read).direction, SessionHistoryDirection::Backward);
        // More history exists: the loader probes the session start next.
        assert!(
            pump.deliver(read.claim, Ok(page(session, vec![event(session, 9)], true)))
                .is_none()
        );
        let probe = pump.next_read().unwrap().unwrap();
        assert_eq!(query(&probe).limit, 1);
        assert!(
            pump.deliver(read.claim, Ok(page(session, Vec::new(), false)))
                .is_none()
        );
        let done = pump
            .deliver(
                probe.claim,
                Ok(page(session, vec![event(session, 1)], true)),
            )
            .expect("complete");
        let Ok(HistoryLoaded::Window(window)) = done.result else {
            panic!("window result");
        };
        assert_eq!(window.first_available_sequence, Some(1));
        assert!(!pump.owns(probe.claim));
    }

    #[test]
    fn a_newer_request_supersedes_its_lane_and_failures_back_off() {
        let session = SessionId::new();
        let mut pump = HistoryPump::new(session);
        let older = |generation| {
            HostRequest::Page(IssuedRequest {
                generation,
                request: PageRequest::Older {
                    before_sequence: 50,
                },
            })
        };
        assert!(pump.start(older(1), Some(60)).is_none());
        let stale = pump.next_read().unwrap().unwrap();
        assert!(pump.start(older(2), Some(60)).is_none());
        assert!(!pump.owns(stale.claim));
        assert!(
            pump.deliver(stale.claim, Ok(page(session, Vec::new(), false)))
                .is_none()
        );
        let read = pump.next_read().unwrap().unwrap();
        let failed = pump
            .deliver(read.claim, Err(FeedError::Server("browser_busy".into())))
            .expect("a failed read completes the job");
        assert!(matches!(failed.result, Err(HistoryLoadError::Read(_))));
        let mut retries = HistoryRetries::default();
        let at = Instant::now();
        retries.schedule(failed.request, at);
        assert_eq!(retries.next_due(), Some(at + FIRST_RETRY_DELAY));
        assert_eq!(retries.take_due(at), Vec::<HostRequest>::new());
        assert_eq!(retries.take_due(at + FIRST_RETRY_DELAY), vec![older(2)]);
        retries.schedule(failed.request, at);
        retries.schedule(failed.request, at);
        assert_eq!(retries.next_due(), Some(at + FIRST_RETRY_DELAY * 4));
        retries.clear(&failed.request);
        assert!(retries.next_due().is_none());
    }
}
