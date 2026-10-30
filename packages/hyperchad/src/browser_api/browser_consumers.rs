//! Application-owned browser consumers. No renderer sender or canonical history cache.
use std::{collections::BTreeMap, sync::Mutex, time::Duration};

use bcode_client::{BcodeClient, ClientError};
use bcode_session_models::SessionId;
use tokio::{sync::watch, task::JoinHandle, time::Instant};

const LEASE: Duration = Duration::from_secs(60);

struct Consumer {
    deadline: watch::Sender<Instant>,
    ready: watch::Receiver<bool>,
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
#[derive(Default)]
pub struct BrowserConsumers {
    entries: Mutex<BTreeMap<SessionId, Consumer>>,
}

impl BrowserConsumers {
    /// Renew one session lease and wait for its compatible attachment.
    ///
    /// # Errors
    /// Returns an error on capacity exhaustion or if the owner ends before attachment.
    /// Callers apply their existing bounded backend request deadline.
    pub async fn renew(&self, client: &BcodeClient, session: SessionId) -> Result<(), ClientError> {
        let mut ready = self.admit(client, session, Instant::now(), LEASE)?;
        loop {
            if *ready.borrow_and_update() {
                return Ok(());
            }
            ready.changed().await.map_err(|_| {
                ClientError::Protocol("browser consumer attachment ended".to_owned())
            })?;
        }
    }

    fn admit(
        &self,
        client: &BcodeClient,
        session: SessionId,
        now: Instant,
        lease: Duration,
    ) -> Result<watch::Receiver<bool>, ClientError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|_, consumer| {
            !consumer.task.is_finished() && *consumer.deadline.borrow() > now
        });
        if let Some(consumer) = entries.get_mut(&session) {
            consumer.deadline.send_replace(now + lease);
            return Ok(consumer.ready.clone());
        }
        if entries.len() >= super::MAX_CACHED_SESSIONS {
            return Err(ClientError::Protocol(
                "browser consumer capacity reached".to_owned(),
            ));
        }
        let (deadline, expiry) = watch::channel(now + lease);
        let (status, ready) = watch::channel(false);
        let client = client.clone();
        let task = tokio::spawn(async move {
            tokio::select! {
                biased;
                () = lease_ended(expiry) => {},
                () = drain(client, session, status) => {},
            }
        });
        entries.insert(
            session,
            Consumer {
                deadline,
                ready: ready.clone(),
                task,
            },
        );
        drop(entries);
        Ok(ready)
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

async fn drain(client: BcodeClient, session: SessionId, status: watch::Sender<bool>) {
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
            // discarded; polling remains the sole observation publisher.
            drop(initial);
            status.send_replace(true);
            while connection.recv_event_without_reconnect().await.is_ok() {}
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

    #[tokio::test]
    async fn bounded_admission_reuses_active_leases_without_eviction() {
        let pool = BrowserConsumers::default();
        let client = client();
        let now = Instant::now();
        let session = SessionId::new();
        let first = pool.admit(&client, session, now, LEASE).unwrap();
        let second = pool.admit(&client, session, now, LEASE).unwrap();
        assert!(first.same_channel(&second));
        for _ in 1..super::super::MAX_CACHED_SESSIONS {
            pool.admit(&client, SessionId::new(), now, LEASE).unwrap();
        }
        assert!(pool.admit(&client, SessionId::new(), now, LEASE).is_err());
        assert!(first.same_channel(&pool.admit(&client, session, now, LEASE).unwrap()));
    }

    #[tokio::test]
    async fn expired_admission_reclaims_capacity_and_app_drop_closes_waiters() {
        let pool = BrowserConsumers::default();
        let client = client();
        let now = Instant::now();
        let session = SessionId::new();
        let mut first = pool.admit(&client, session, now, LEASE).unwrap();
        let replacement = pool.admit(&client, session, now + LEASE, LEASE).unwrap();
        assert!(!first.same_channel(&replacement));
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
        let directory = tempfile::tempdir().unwrap();
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
        let exercise = exercise_browser_consumer(&client, &directory, &shutdown);
        tokio::join!(server, exercise);
    }

    #[cfg(unix)]
    async fn exercise_browser_consumer(
        client: &BcodeClient,
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
        // The isolated server retains exactly one consumer socket after the
        // transient observation sockets drain; no HTML renderer was installed.
        wait_attached_count(client, directory.path(), session, 1).await;
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
        tokio::time::timeout(Duration::from_secs(5), async {
            while closed.changed().await.is_ok() {}
        })
        .await
        .unwrap();
        wait_attached_count(client, directory.path(), session, 0).await;
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
                let sessions = client
                    .list_sessions_in_working_directory(working_directory.to_path_buf())
                    .await
                    .unwrap();
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
        let mut ready = pool
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
