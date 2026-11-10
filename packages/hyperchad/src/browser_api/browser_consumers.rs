//! Application-owned browser consumers. No renderer sender or canonical history cache.
//!
//! A consumer's attachment also feeds its session's bounded live input ring
//! ([`super::live::LiveFeed`]); the ring is never a canonical history cache.
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use bcode_client::{BcodeClient, ClientError};
use bcode_session_models::SessionId;
use tokio::{sync::watch, task::JoinHandle, time::Instant};

use super::live::LiveFeed;

const LEASE: Duration = Duration::from_secs(60);
/// Concurrent live long-polls of one session (one per mounted tab of it).
const MAX_SESSION_LIVE_WAITERS: usize = 4;
/// Concurrent live long-polls across all sessions.
const MAX_LIVE_WAITERS: usize = MAX_SESSION_LIVE_WAITERS * super::MAX_CACHED_SESSIONS;
/// Concurrent bounded history reads of one session (the browser's per-mount read limit).
const MAX_SESSION_HISTORY_READS: usize = 2;
/// Concurrent bounded history reads across all sessions.
const MAX_HISTORY_READS: usize = super::MAX_CACHED_SESSIONS;

/// Work admitted concurrently per session and across sessions, without a queue: one session's
/// tabs cannot take every slot from the others.
struct Slots {
    per_session: usize,
    total: usize,
    used: Mutex<BTreeMap<SessionId, usize>>,
}

impl Slots {
    const fn new(per_session: usize, total: usize) -> Self {
        Self {
            per_session,
            total,
            used: Mutex::new(BTreeMap::new()),
        }
    }

    fn try_acquire(&self, session: SessionId) -> Option<Slot<'_>> {
        let mut used = self
            .used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let total = used.values().sum::<usize>();
        let mine = used.get(&session).copied().unwrap_or_default();
        if total >= self.total || mine >= self.per_session {
            return None;
        }
        used.insert(session, mine + 1);
        drop(used);
        Some(Slot {
            slots: self,
            session,
        })
    }
}

/// One admitted unit of work; released when dropped.
pub(super) struct Slot<'a> {
    slots: &'a Slots,
    session: SessionId,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut used = self
            .slots
            .used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = used.get_mut(&self.session) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                used.remove(&self.session);
            }
        }
    }
}

struct Consumer {
    deadline: watch::Sender<Instant>,
    ready: watch::Receiver<bool>,
    feed: Arc<LiveFeed>,
    task: JoinHandle<()>,
}

impl Drop for Consumer {
    fn drop(&mut self) {
        // Abort also interrupts attachment/reconnect and drops the socket. The
        // worker owns no pool/app Arc, so app drop cannot leave a retention cycle.
        self.task.abort();
    }
}

/// Bounded application attachment owners, independent of renderer publication.
pub struct BrowserConsumers {
    entries: Mutex<BTreeMap<SessionId, Consumer>>,
    live_waiters: Slots,
    history_reads: Slots,
}

impl Default for BrowserConsumers {
    fn default() -> Self {
        Self {
            entries: Mutex::default(),
            live_waiters: Slots::new(MAX_SESSION_LIVE_WAITERS, MAX_LIVE_WAITERS),
            history_reads: Slots::new(MAX_SESSION_HISTORY_READS, MAX_HISTORY_READS),
        }
    }
}

impl BrowserConsumers {
    /// Renew one session lease and wait for its compatible attachment.
    ///
    /// Returns the live input feed of the attachment.
    ///
    /// # Errors
    /// Returns an error on capacity exhaustion or if the owner ends before attachment.
    /// Callers apply their existing bounded backend request deadline.
    pub async fn renew(
        &self,
        client: &BcodeClient,
        session: SessionId,
    ) -> Result<Arc<LiveFeed>, ClientError> {
        let (mut ready, feed) = self.admit(client, session, Instant::now(), LEASE)?;
        loop {
            if *ready.borrow_and_update() {
                return Ok(feed);
            }
            ready.changed().await.map_err(|_| {
                ClientError::Protocol("browser consumer attachment ended".to_owned())
            })?;
        }
    }

    /// One live long-poll slot for `session`, or `None` while the session's or all slots are
    /// in use.
    pub(super) fn admit_live_waiter(&self, session: SessionId) -> Option<Slot<'_>> {
        self.live_waiters.try_acquire(session)
    }

    /// One bounded history read slot for `session`, or `None` while the session's or all
    /// slots are in use.
    pub(super) fn admit_history_read(&self, session: SessionId) -> Option<Slot<'_>> {
        self.history_reads.try_acquire(session)
    }

    fn admit(
        &self,
        client: &BcodeClient,
        session: SessionId,
        now: Instant,
        lease: Duration,
    ) -> Result<(watch::Receiver<bool>, Arc<LiveFeed>), ClientError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|_, consumer| {
            !consumer.task.is_finished() && *consumer.deadline.borrow() > now
        });
        if let Some(consumer) = entries.get_mut(&session) {
            consumer.deadline.send_replace(now + lease);
            return Ok((consumer.ready.clone(), Arc::clone(&consumer.feed)));
        }
        if entries.len() >= super::MAX_CACHED_SESSIONS {
            return Err(ClientError::Protocol(
                "browser consumer capacity reached".to_owned(),
            ));
        }
        let (deadline, expiry) = watch::channel(now + lease);
        let (status, ready) = watch::channel(false);
        let feed = Arc::new(LiveFeed::default());
        let client = client.clone();
        let task = tokio::spawn({
            let feed = Arc::clone(&feed);
            async move {
                tokio::select! {
                    biased;
                    () = lease_ended(expiry) => {},
                    () = drain(client, session, status, feed) => {},
                }
            }
        });
        entries.insert(
            session,
            Consumer {
                deadline,
                ready: ready.clone(),
                feed: Arc::clone(&feed),
                task,
            },
        );
        drop(entries);
        Ok((ready, feed))
    }
}

async fn lease_ended(mut expiry: watch::Receiver<Instant>) {
    loop {
        let deadline = *expiry.borrow_and_update();
        tokio::select! {
            biased;
            changed = expiry.changed() => if changed.is_err() { return; },
            () = tokio::time::sleep_until(deadline) => return,
        }
    }
}

async fn drain(
    client: BcodeClient,
    session: SessionId,
    status: watch::Sender<bool>,
    feed: Arc<LiveFeed>,
) {
    loop {
        status.send_replace(false);
        if let Ok(mut connection) = client.connect("bcode-browser-consumer").await
            && let Ok(initial) = crate::attach_hyperchad_projection_window_with_request(
                &mut connection,
                session,
                crate::hyperchad_projection_window_request(),
            )
            .await
        {
            // Attachment is complete before readiness. History is immediately
            // discarded; polling remains the sole snapshot publisher. Each attachment
            // starts a new live feed epoch: inputs missed while detached are a gap.
            drop(initial);
            feed.reset();
            status.send_replace(true);
            while let Ok(event) = connection.recv_event_without_reconnect().await {
                feed.observe(session, &event);
            }
        }
        status.send_replace(false);
        tokio::time::sleep(crate::WATCH_RECONNECT_DELAY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> BcodeClient {
        BcodeClient::new(bcode_ipc::IpcEndpoint::unix_socket(
            std::path::PathBuf::from("/nonexistent/bcode-browser-consumer-test.sock"),
        ))
    }

    #[test]
    fn feed_reads_and_waits_are_admitted_within_fixed_limits() {
        let pool = BrowserConsumers::default();
        let sessions = (0..super::super::MAX_CACHED_SESSIONS)
            .map(|_| SessionId::new())
            .collect::<Vec<_>>();
        let waiters: Vec<_> = sessions
            .iter()
            .flat_map(|session| {
                (0..MAX_SESSION_LIVE_WAITERS).map(|_| pool.admit_live_waiter(*session).unwrap())
            })
            .collect();
        assert_eq!(waiters.len(), MAX_LIVE_WAITERS);
        assert!(pool.admit_live_waiter(SessionId::new()).is_none());
        let reads: Vec<_> = sessions
            .iter()
            .map(|session| pool.admit_history_read(*session).unwrap())
            .collect();
        assert_eq!(reads.len(), MAX_HISTORY_READS);
        assert!(pool.admit_history_read(sessions[0]).is_none());
        drop((waiters, reads));
        assert!(
            pool.admit_live_waiter(sessions[0]).is_some()
                && pool.admit_history_read(sessions[0]).is_some()
        );
    }

    #[test]
    fn one_session_cannot_take_every_feed_slot_from_another() {
        let pool = BrowserConsumers::default();
        let (busy, other) = (SessionId::new(), SessionId::new());
        // One session's tabs attach at once: reads and polls beyond its share are refused.
        let reads: Vec<_> = (0..MAX_SESSION_HISTORY_READS)
            .map(|_| pool.admit_history_read(busy).unwrap())
            .collect();
        assert!(pool.admit_history_read(busy).is_none());
        let waiters: Vec<_> = (0..MAX_SESSION_LIVE_WAITERS)
            .map(|_| pool.admit_live_waiter(busy).unwrap())
            .collect();
        assert!(pool.admit_live_waiter(busy).is_none());
        // Another session still loads and polls.
        let other_reads: Vec<_> = (0..MAX_SESSION_HISTORY_READS)
            .map(|_| pool.admit_history_read(other).unwrap())
            .collect();
        assert!(pool.admit_live_waiter(other).is_some());
        drop((reads, waiters, other_reads));
        assert!(pool.admit_history_read(busy).is_some());
    }

    #[tokio::test]
    async fn bounded_admission_reuses_active_leases_without_eviction() {
        let pool = BrowserConsumers::default();
        let client = client();
        let now = Instant::now();
        let session = SessionId::new();
        let (first, first_feed) = pool.admit(&client, session, now, LEASE).unwrap();
        let (second, second_feed) = pool.admit(&client, session, now, LEASE).unwrap();
        assert!(first.same_channel(&second));
        assert!(Arc::ptr_eq(&first_feed, &second_feed), "one feed per lease");
        for _ in 1..super::super::MAX_CACHED_SESSIONS {
            pool.admit(&client, SessionId::new(), now, LEASE).unwrap();
        }
        assert!(pool.admit(&client, SessionId::new(), now, LEASE).is_err());
        assert!(first.same_channel(&pool.admit(&client, session, now, LEASE).unwrap().0));
    }

    #[tokio::test]
    async fn expired_admission_reclaims_capacity_and_app_drop_closes_waiters() {
        let pool = BrowserConsumers::default();
        let client = client();
        let now = Instant::now();
        let session = SessionId::new();
        let (mut first, first_feed) = pool.admit(&client, session, now, LEASE).unwrap();
        let (replacement, replacement_feed) =
            pool.admit(&client, session, now + LEASE, LEASE).unwrap();
        assert!(!first.same_channel(&replacement));
        assert!(
            !Arc::ptr_eq(&first_feed, &replacement_feed),
            "a new lease starts a new feed"
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while first.changed().await.is_ok() {}
        })
        .await
        .unwrap();
        drop(pool);
        let mut replacement = replacement;
        tokio::time::timeout(Duration::from_secs(1), async {
            while replacement.changed().await.is_ok() {}
        })
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_server_poll_gap_without_html_retains_consumer_until_expiry_and_drop() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let locations = bcode_config::resolve_state_location_set_with_environment(
            &bcode_config::StateConfig::default(),
            &bcode_config::StateLocationSelection {
                root: Some(directory.path().join("state")),
                profile: None,
            },
            &bcode_config::ConfigEnvironmentSnapshot::new(
                BTreeMap::new(),
                directory.path().to_path_buf(),
            ),
        )
        .unwrap();
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.path().join("consumer.sock"));
        let client = BcodeClient::for_state_location(endpoint.clone(), locations.primary());
        let observer = BcodeClient::for_state_location(endpoint.clone(), locations.primary());
        let shutdown = bcode_agent_runtime::CancellationToken::new();
        let server = async {
            bcode_server::run_embedded_with_locations_and_shutdown(
                endpoint,
                bcode_config::BcodeConfig::default(),
                bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
                    &bcode_plugin::PluginSelection {
                        mode: bcode_plugin::PluginSelectionMode::Explicit,
                        enabled: std::collections::BTreeSet::new(),
                        disabled: std::collections::BTreeSet::new(),
                    },
                    &[],
                )
                .unwrap(),
                bcode_model_catalog::ModelCatalogResolver::embedded(),
                Vec::new(),
                shutdown.clone(),
                Some(locations),
            )
            .await
            .expect("embedded consumer fixture server must start and shut down cleanly");
        };
        let exercise = exercise_browser_consumer(client, &observer, &directory, &shutdown);
        tokio::join!(server, exercise);
    }

    #[cfg(unix)]
    async fn exercise_browser_consumer(
        client: BcodeClient,
        observer: &BcodeClient,
        directory: &tempfile::TempDir,
        shutdown: &bcode_agent_runtime::CancellationToken,
    ) {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match client.verified_server_status().await {
                    Ok(_) => break,
                    Err(error) => {
                        eprintln!("consumer fixture readiness: {error}");
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                }
            }
        })
        .await
        .unwrap();
        let session = client
            .create_session_in_working_directory(None, directory.path().to_path_buf())
            .await
            .unwrap()
            .id;
        let app = crate::HyperChadAppState::new_in_working_directory(
            client.clone(),
            "consumer-test",
            directory.path().to_path_buf(),
        );
        assert!(app.renderer_tx.lock().unwrap().is_none());
        // Check the dedicated consumer before snapshot requests populate reusable lanes.
        app.browser_consumers
            .renew(&app.client, session)
            .await
            .unwrap();
        wait_attached_count(observer, directory.path(), session, 1).await;
        let request = crate::hyperchad_projection_window_request();
        app.browser_window_snapshot(session, request.clone())
            .await
            .unwrap();
        let first = app
            .browser_consumers
            .entries
            .lock()
            .unwrap()
            .get(&session)
            .unwrap()
            .ready
            .clone();
        tokio::time::sleep(Duration::from_millis(50)).await;
        app.browser_window_snapshot(session, request).await.unwrap();
        {
            let entries = app.browser_consumers.entries.lock().unwrap();
            assert!(first.same_channel(&entries.get(&session).unwrap().ready));
            drop(entries);
        }
        app.browser_consumers
            .entries
            .lock()
            .unwrap()
            .get(&session)
            .unwrap()
            .deadline
            .send_replace(Instant::now());
        let mut closed = first;
        tokio::time::timeout(Duration::from_secs(5), async {
            while closed.changed().await.is_ok() {}
        })
        .await
        .unwrap();
        app.browser_consumers
            .renew(&app.client, session)
            .await
            .unwrap();
        let mut closed = app
            .browser_consumers
            .entries
            .lock()
            .unwrap()
            .get(&session)
            .unwrap()
            .ready
            .clone();
        drop(app);
        // Release snapshot request lanes as well as the leased consumer.
        drop(client);
        tokio::time::timeout(Duration::from_secs(5), async {
            while closed.changed().await.is_ok() {}
        })
        .await
        .unwrap();
        wait_attached_count(observer, directory.path(), session, 0).await;
        shutdown.cancel();
    }

    #[cfg(unix)]
    async fn wait_attached_count(
        client: &BcodeClient,
        working_directory: &std::path::Path,
        session: SessionId,
        count: usize,
    ) {
        let mut last_observed = None;
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // A dedicated catalog probe must not add a retained request-lane socket.
                let mut catalog = client.connect("browser-consumer-catalog-probe").await.unwrap();
                let sessions = catalog
                    .list_sessions_in_working_directory(working_directory.to_path_buf())
                    .await
                    .unwrap();
                drop(catalog);
                assert!(sessions.sessions.iter().any(|value| value.id == session), "exact fixture session must remain discoverable");
                // Catalog summaries cache durable metadata, not runtime client
                // membership. This isolated server has exactly this one leased
                // consumer plus the temporary status-query connection itself.
                let observed = client.verified_server_status().await.unwrap().connected_client_count;
                if last_observed.is_none() {
                    eprintln!("browser consumer count: expected connections={}, first observed={observed}", count + 1);
                }
                last_observed = Some(observed);
                if observed == count + 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            result.is_ok(),
            "expected exactly {} connections (consumer count {count} + query), last observed {last_observed:?}",
            count + 1
        );
    }

    #[tokio::test]
    async fn failed_attachment_waiter_closes_at_lease_expiry() {
        let pool = BrowserConsumers::default();
        let (mut ready, _feed) = pool
            .admit(
                &client(),
                SessionId::new(),
                Instant::now(),
                Duration::from_millis(20),
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                assert!(!*ready.borrow_and_update());
                if ready.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn lease_renewal_and_sender_drop_interrupt_idle_wait() {
        let (sender, receiver) = watch::channel(Instant::now() + LEASE);
        let worker = tokio::spawn(lease_ended(receiver));
        sender.send_replace(Instant::now() + LEASE + LEASE);
        tokio::task::yield_now().await;
        assert!(!worker.is_finished());
        drop(sender);
        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();
        let (_sender, receiver) = watch::channel(Instant::now());
        tokio::time::timeout(Duration::from_secs(1), lease_ended(receiver))
            .await
            .unwrap();
    }
}
