//! Turso connection initialization and bounded lock retry policy.

use std::{path::Path, time::Duration};
use switchy::database::Database;

const DATABASE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DATABASE_OPEN_RETRY_ATTEMPTS: u32 = 7;
const DATABASE_OPEN_INITIAL_RETRY_DELAY: Duration = Duration::from_millis(25);
const DATABASE_OPEN_MAX_RETRY_DELAY: Duration = Duration::from_secs(2);

pub async fn init_turso_local_with_retry(
    path: &Path,
) -> Result<Box<dyn Database>, switchy::database_connection::InitTursoError> {
    let mut attempt = 0_u32;
    let mut delay = DATABASE_OPEN_INITIAL_RETRY_DELAY;
    loop {
        match switchy::database_connection::builder()
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
        {
            Ok(db) => return Ok(db),
            Err(error)
                if is_database_lock_error(&error) && attempt < DATABASE_OPEN_RETRY_ATTEMPTS =>
            {
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(delay).await;
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

    #[test]
    fn lock_error_messages_are_classified_narrowly() {
        assert!(is_database_lock_error_message("database is locked"));
        assert!(is_database_lock_error_message("database busy"));
        assert!(is_database_lock_error_message("failed locking file"));
        assert!(!is_database_lock_error_message("permission denied"));
    }
}
