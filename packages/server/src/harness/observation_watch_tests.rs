use super::{MAX_IDLE_BACKOFF, MIN_IDLE_BACKOFF, ManagedHarnessError, next_delay, terminal};
use std::time::Duration;

#[test]
fn progress_or_a_held_page_continues_immediately() {
    assert_eq!(next_delay(3, false, MAX_IDLE_BACKOFF), Duration::ZERO);
    assert_eq!(next_delay(0, true, MAX_IDLE_BACKOFF), Duration::ZERO);
}

#[test]
fn unheld_empty_pages_back_off_within_bounds() {
    let mut delay = Duration::ZERO;
    delay = next_delay(0, false, delay);
    assert_eq!(delay, MIN_IDLE_BACKOFF);
    for _ in 0..16 {
        delay = next_delay(0, false, delay);
    }
    assert_eq!(delay, MAX_IDLE_BACKOFF);
    // Progress resets the backoff for the next idle period.
    assert_eq!(
        next_delay(0, false, next_delay(1, false, delay)),
        MIN_IDLE_BACKOFF
    );
}

#[test]
fn ownership_and_shape_failures_end_the_watch_but_transient_failures_retry() {
    for error in [
        ManagedHarnessError::StaleOwner,
        ManagedHarnessError::Invalid,
        ManagedHarnessError::Unsupported,
        ManagedHarnessError::Conflict,
    ] {
        assert!(terminal(error));
    }
    for error in [
        ManagedHarnessError::Storage,
        ManagedHarnessError::Deadline,
        ManagedHarnessError::Unavailable,
        ManagedHarnessError::Uncertain,
    ] {
        assert!(!terminal(error));
    }
}

mod drive {
    use super::super::{ObservationWatches, WATCH_WAIT_MILLIS};
    use crate::harness::ManagedHarnessError;
    use crate::harness_dispatch::{
        ManagedAuthAdmission, ManagedCaller, ManagedSessionBackend, ManagedSessionTask,
    };
    use bcode_harness_models::HarnessServiceResponse;
    use bcode_ipc::harness::{ManagedSessionCall, ManagedSessionPollRequest};
    use bcode_session_models::{ClientId, SessionId};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Scripted page results; an exhausted script parks the page like a held native wait.
    #[derive(Default)]
    struct ScriptedBackend {
        script: Mutex<VecDeque<Result<usize, ManagedHarnessError>>>,
        waits: Mutex<Vec<u32>>,
    }

    impl ManagedSessionBackend for ScriptedBackend {
        fn shutdown(&self) -> ManagedSessionTask<'_, ()> {
            Box::pin(async { Ok(()) })
        }
        fn execute<'a>(
            &'a self,
            _caller: &'a ManagedCaller,
            _call: ManagedSessionCall,
        ) -> ManagedSessionTask<'a, HarnessServiceResponse> {
            Box::pin(async { Err(ManagedHarnessError::Unsupported) })
        }
        fn auth_scope<'a>(
            &'a self,
            _caller: &'a ManagedCaller,
            _session_id: SessionId,
            _instance_id: &'a str,
        ) -> ManagedSessionTask<'a, ManagedAuthAdmission> {
            Box::pin(async { Err(ManagedHarnessError::Unsupported) })
        }
        fn poll_observations<'a>(
            &'a self,
            _caller: &'a ManagedCaller,
            _query: &'a ManagedSessionPollRequest,
            wait_millis: u32,
        ) -> ManagedSessionTask<'a, usize> {
            self.waits.lock().unwrap().push(wait_millis);
            let next = self.script.lock().unwrap().pop_front();
            Box::pin(async move {
                match next {
                    Some(result) => result,
                    None => std::future::pending().await,
                }
            })
        }
    }

    fn fixture(
        script: Vec<Result<usize, ManagedHarnessError>>,
    ) -> (
        Arc<crate::ServerState>,
        Arc<ScriptedBackend>,
        ManagedSessionPollRequest,
    ) {
        let state = Arc::new(crate::tests::test_server_state(
            crate::SessionManager::default(),
        ));
        let backend = Arc::new(ScriptedBackend {
            script: Mutex::new(script.into()),
            waits: Mutex::default(),
        });
        let installed: Arc<dyn ManagedSessionBackend> = backend.clone();
        assert!(state.managed_sessions.set(installed).is_ok());
        let query = ManagedSessionPollRequest {
            session_id: SessionId::new(),
            instance_id: "instance".into(),
            generation: 1,
            limit: 16,
        };
        (state, backend, query)
    }

    async fn until(condition: impl Fn() -> bool + Send + Sync) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition within bound");
    }

    fn live(watches: &ObservationWatches) -> usize {
        watches.live.lock().unwrap().len()
    }

    #[tokio::test]
    async fn pages_are_held_and_a_changed_owner_ends_the_watch() {
        let (state, backend, query) = fixture(vec![
            Ok(2),
            Ok(1),
            Err(ManagedHarnessError::Storage),
            Ok(0),
            Err(ManagedHarnessError::StaleOwner),
        ]);
        let watches = ObservationWatches::default();
        watches.start(Arc::downgrade(&state), ClientId::new(), query);
        until(|| backend.waits.lock().unwrap().len() == 5 && live(&watches) == 0).await;
        // Transient failures retry; every page asks the adapter to hold for native output.
        assert!(
            backend
                .waits
                .lock()
                .unwrap()
                .iter()
                .all(|wait| *wait == WATCH_WAIT_MILLIS)
        );
    }

    #[tokio::test]
    async fn repeated_start_is_idempotent_and_shutdown_stops_watches() {
        let (state, backend, query) = fixture(Vec::new());
        let watches = ObservationWatches::default();
        let client = ClientId::new();
        watches.start(Arc::downgrade(&state), client, query.clone());
        watches.start(Arc::downgrade(&state), client, query.clone());
        until(|| backend.waits.lock().unwrap().len() == 1).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(backend.waits.lock().unwrap().len(), 1);
        assert_eq!(live(&watches), 1);
        // A new generation replaces the attachment's watch.
        watches.start(
            Arc::downgrade(&state),
            client,
            ManagedSessionPollRequest {
                generation: 2,
                ..query
            },
        );
        until(|| backend.waits.lock().unwrap().len() == 2).await;
        assert_eq!(live(&watches), 1);
        watches.shutdown();
        assert_eq!(live(&watches), 0);
    }
}
