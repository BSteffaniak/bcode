//! Shared absolute allowances for diagnostic I/O, never execution deadlines.
use std::future::Future;
use tokio::time::Instant;

/// Do not begin another operation after the allowance has expired. A timeout
/// cancels only this observer future; it never asserts remote effects were undone.
pub(super) async fn until<F: Future>(
    deadline: Instant,
    operation: F,
) -> Result<F::Output, &'static str> {
    if Instant::now() >= deadline {
        return Err("watch_operation_deadline");
    }
    tokio::time::timeout_at(deadline, operation)
        .await
        .map_err(|_| "watch_operation_deadline")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, time::Duration};

    #[tokio::test]
    async fn expired_allowance_does_not_begin_an_operation() {
        let dispatched = Cell::new(false);
        let result = until(Instant::now(), async {
            dispatched.set(true);
        })
        .await;
        assert_eq!(result, Err("watch_operation_deadline"));
        assert!(!dispatched.get());
    }

    #[tokio::test]
    async fn stalled_operation_drops_and_cannot_renew_the_shared_allowance() {
        struct Dropped<'a>(&'a Cell<bool>);
        impl Drop for Dropped<'_> {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let dropped = Cell::new(false);
        let deadline = Instant::now() + Duration::from_millis(10);
        let result = until(deadline, async {
            let _guard = Dropped(&dropped);
            std::future::pending::<()>().await;
        })
        .await;
        assert_eq!(result, Err("watch_operation_deadline"));
        assert!(dropped.get());
        let dispatched = Cell::new(false);
        assert!(
            until(deadline, async { dispatched.set(true) })
                .await
                .is_err()
        );
        assert!(!dispatched.get());
    }
}
