//! Turso connection initialization and bounded lock retry policy.

use std::{path::Path, time::Duration};
use switchy::database::Database;

const DATABASE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DATABASE_OPEN_RETRY_ATTEMPTS: u32 = 7;
const DATABASE_OPEN_INITIAL_RETRY_DELAY: Duration = Duration::from_millis(25);
const DATABASE_OPEN_MAX_RETRY_DELAY: Duration = Duration::from_secs(2);

tokio::task_local! {
    /// Explicit best-effort read scope, never inherited by unrelated actor commands.
    static FAIL_FAST_OPEN: bool;
}

pub async fn without_open_retries<T>(future: impl std::future::Future<Output = T>) -> T {
    FAIL_FAST_OPEN.scope(true, future).await
}

pub async fn init_turso_local_with_retry(
    path: &Path,
) -> Result<Box<dyn Database>, switchy::database_connection::InitTursoError> {
    let fail_fast = FAIL_FAST_OPEN.try_with(|value| *value).unwrap_or(false);
    initialize_with_retry(
        fail_fast,
        || async {
            switchy::database_connection::builder()
                .turso()
                .with_path(path)
                .with_busy_timeout(DATABASE_BUSY_TIMEOUT)
                // Turso's multi-process WAL mode is still experimental and has produced stale
                // WAL-index sidecars after daemon lifecycle churn. Bcode serializes writes with
                // database transactions and its session access guard instead of relying on that
                // experimental sidecar format for correctness.
                .with_multiprocess_wal(false)
                .build()
                .await
        },
        is_database_lock_error,
    )
    .await
}

async fn initialize_with_retry<T, E, F, Fut>(
    fail_fast: bool,
    mut open: F,
    is_locked: impl Fn(&E) -> bool,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let mut attempt = 0_u32;
    let mut delay = DATABASE_OPEN_INITIAL_RETRY_DELAY;
    loop {
        let phase = bcode_metrics::startup::phase("session.db.connection_attempt");
        let result = open().await;
        phase.finish_result(&result);
        if result.as_ref().is_err_and(&is_locked) {
            bcode_metrics::startup::count("session.db.open_lock_errors", 1);
        }
        match result {
            Ok(db) => return Ok(db),
            Err(error)
                if !fail_fast && is_locked(&error) && attempt < DATABASE_OPEN_RETRY_ATTEMPTS =>
            {
                attempt = attempt.saturating_add(1);
                let phase = bcode_metrics::startup::phase("session.db.open_retry_delay");
                tokio::time::sleep(delay).await;
                phase.finish();
                delay = delay.saturating_mul(2).min(DATABASE_OPEN_MAX_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
}

pub fn is_database_lock_error(error: &switchy::database_connection::InitTursoError) -> bool {
    is_database_lock_error_message(&error.to_string())
}

pub fn is_database_lock_error_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("locking error")
        || message.contains("failed locking file")
        || message.contains("database is locked")
        || message.contains("busy")
}

#[cfg(test)]
mod tests {
    use super::is_database_lock_error_message;

    #[tokio::test]
    async fn probe_attempts_lock_once_and_normal_open_retries() {
        let mut attempts = 0;
        let result = super::initialize_with_retry(
            true,
            || {
                attempts += 1;
                std::future::ready(Err::<(), _>("locked"))
            },
            |_| true,
        )
        .await;
        assert_eq!(result, Err("locked"));
        assert_eq!(attempts, 1);
        attempts = 0;
        let result = super::initialize_with_retry(
            false,
            || {
                attempts += 1;
                std::future::ready(if attempts == 1 { Err("locked") } else { Ok(()) })
            },
            |_| true,
        )
        .await;
        assert_eq!(result, Ok(()));
        assert_eq!(attempts, 2);
    }

    #[tokio::test]
    async fn fail_fast_scope_is_local_and_restores_normal_policy() {
        assert!(super::FAIL_FAST_OPEN.try_with(|value| *value).is_err());
        super::without_open_retries(async {
            assert!(super::FAIL_FAST_OPEN.with(|value| *value));
            let other = tokio::spawn(async {
                super::FAIL_FAST_OPEN
                    .try_with(|value| *value)
                    .unwrap_or(false)
            });
            assert!(!other.await.unwrap());
        })
        .await;
        assert!(super::FAIL_FAST_OPEN.try_with(|value| *value).is_err());
    }

    #[tokio::test]
    async fn probe_open_returns_errors_without_inventing_empty_database() {
        let root = tempfile::tempdir().unwrap();
        let missing_parent = root.path().join("missing").join("session.db");
        let result =
            super::without_open_retries(super::init_turso_local_with_retry(&missing_parent)).await;
        assert!(result.is_err());
        assert!(!missing_parent.exists());
    }

    #[test]
    fn lock_error_messages_are_classified_narrowly() {
        assert!(is_database_lock_error_message("database is locked"));
        assert!(is_database_lock_error_message("database busy"));
        assert!(is_database_lock_error_message("failed locking file"));
        assert!(!is_database_lock_error_message("permission denied"));
    }
}
