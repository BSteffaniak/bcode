//! Executor responsiveness and mutation-result semantics of the blocking store boundary.
use super::*;

#[tokio::test(flavor = "current_thread")]
async fn blocking_store_work_does_not_stall_executor() {
    let root = tempfile::tempdir().unwrap();
    let store = NativeConnectionStore::new(root.path());
    let (started, observed) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let operation = tokio::spawn(async move {
        store_operation(&store, move |_| {
            started.send(()).unwrap();
            released
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            Ok(())
        })
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), observed)
        .await
        .unwrap()
        .unwrap();
    // Only an executor that remained responsive can release this synchronous operation.
    release.send(()).unwrap();
    assert_eq!(operation.await.unwrap(), Ok(()));
}

#[tokio::test]
async fn blocking_boundary_preserves_domain_errors_and_maps_worker_failure() {
    let root = tempfile::tempdir().unwrap();
    let store = NativeConnectionStore::new(root.path());
    assert_eq!(
        store_operation(&store, |_| Err::<(), _>(NativeConnectionError::Conflict)).await,
        Err(NativeConnectionError::Conflict)
    );
    assert_eq!(
        store_operation(&store, |_| -> Result<(), NativeConnectionError> {
            panic!("synthetic worker failure")
        })
        .await,
        Err(NativeConnectionError::StateUnavailable)
    );
}
