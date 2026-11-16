use super::{
    Arc, HostSessionRequest, NativeRetainedReservationV1, PRODUCT_HOST_SESSION_CONTRACT,
    PRODUCT_HOST_SESSION_VERSION, ProductHostSession, RETAINED_HOST_CONTROL_JOIN,
    RetainedHostDispatch, RetainedHostObservation, RetainedHostOutcome, encode,
};
use bmux_plugin_sdk::abi::retained::RETAINED_HOST_VERSION;

fn reservation() -> NativeRetainedReservationV1 {
    NativeRetainedReservationV1 {
        size: u32::try_from(std::mem::size_of::<NativeRetainedReservationV1>()).unwrap(),
        version: RETAINED_HOST_VERSION,
        max_outcome_bytes: 4096,
        operation_slots: 1,
        reserved: 0,
    }
}
#[tokio::test]
async fn negotiation_retry_ack_is_transport_only_and_postclose_query_survives() {
    let (instance, activation) = super::super::product_host_tests::active();
    let session = ProductHostSession::new(
        &activation,
        bmux_plugin_sdk::definition::PluginHostServices::new(|_| panic!("no fallback")),
        Arc::new(()),
    )
    .unwrap();
    let request = bmux_plugin_sdk::HostServiceOwnedRequest {
        capability: PRODUCT_HOST_SESSION_CONTRACT.into(),
        interface: PRODUCT_HOST_SESSION_CONTRACT.into(),
        kind: bmux_plugin_sdk::ServiceKind::Query,
        operation: "negotiate".into(),
        payload: Vec::new(),
    };
    let operation =
        RetainedHostDispatch::begin(&session, 1, &request.as_request(), reservation()).unwrap();
    assert_eq!(
        RetainedHostDispatch::begin(&session, 1, &request.as_request(), reservation()).unwrap(),
        operation
    );
    session.close().unwrap();
    let (receipt, bytes) = loop {
        match RetainedHostDispatch::observe(&session, operation, 4096).unwrap() {
            RetainedHostObservation::Pending => tokio::task::yield_now().await,
            RetainedHostObservation::Terminal {
                receipt,
                outcome: RetainedHostOutcome::Completed(bytes),
            } => break (receipt, bytes),
            other @ RetainedHostObservation::Terminal { .. } => panic!("unexpected {other:?}"),
        }
    };
    let negotiation: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(negotiation["contract"], PRODUCT_HOST_SESSION_CONTRACT);
    assert_eq!(negotiation["version"], PRODUCT_HOST_SESSION_VERSION);
    assert!(RetainedHostDispatch::acknowledge(&session, operation, receipt + 1).is_err());
    RetainedHostDispatch::acknowledge(&session, operation, receipt).unwrap();
    RetainedHostDispatch::acknowledge(&session, operation, receipt).unwrap();
    assert!(!RetainedHostDispatch::release(&session).unwrap());
    let request2 = bmux_plugin_sdk::HostServiceOwnedRequest {
        operation: "request".into(),
        payload: serde_json::to_vec(&HostSessionRequest::ListJobs {
            after: None,
            limit: 1,
        })
        .unwrap(),
        ..request
    };
    assert!(
        RetainedHostDispatch::begin(&session, 1, &request2.as_request(), reservation()).is_err()
    );
    let query =
        RetainedHostDispatch::begin(&session, 2, &request2.as_request(), reservation()).unwrap();
    loop {
        if let RetainedHostObservation::Terminal { receipt, .. } =
            RetainedHostDispatch::observe(&session, query, 4096).unwrap()
        {
            RetainedHostDispatch::acknowledge(&session, query, receipt).unwrap();
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(RetainedHostDispatch::control(&session, 0, RETAINED_HOST_CONTROL_JOIN).unwrap());
    session.join().await.unwrap();
    session.release().unwrap();
    assert!(RetainedHostDispatch::release(&session).unwrap());
    super::super::product_host_tests::finish(&instance, &activation).await;
}
#[test]
fn encoding_counts_before_allocation_including_header() {
    assert!(encode(&"abc", 20).is_err());
    assert_eq!(encode(&"abc", 21).unwrap(), b"\"abc\"");
}

#[tokio::test]
async fn lookup_after_close_preserves_mapping_budget_and_ack_tombstone() {
    let (instance, activation) = super::super::product_host_tests::active();
    let session = ProductHostSession::new(
        &activation,
        bmux_plugin_sdk::definition::PluginHostServices::new(|_| {
            panic!("lookup must not dispatch")
        }),
        Arc::new(()),
    )
    .unwrap();
    let request = bmux_plugin_sdk::HostServiceOwnedRequest {
        capability: PRODUCT_HOST_SESSION_CONTRACT.into(),
        interface: PRODUCT_HOST_SESSION_CONTRACT.into(),
        kind: bmux_plugin_sdk::ServiceKind::Query,
        operation: "negotiate".into(),
        payload: vec![],
    };
    let operation =
        RetainedHostDispatch::begin(&session, 9, &request.as_request(), reservation()).unwrap();
    session.close().unwrap();
    let before = {
        let ledger = session.inner.transport.lock().unwrap();
        let facts = (ledger.bytes, ledger.next, ledger.records.len());
        drop(ledger);
        facts
    };
    assert_eq!(
        RetainedHostDispatch::lookup(&session, 9, &request.as_request(), reservation()).unwrap(),
        Some(operation)
    );
    assert_eq!(
        RetainedHostDispatch::lookup(&session, 10, &request.as_request(), reservation()).unwrap(),
        None
    );
    let mut wrong = request.clone();
    wrong.kind = bmux_plugin_sdk::ServiceKind::Command;
    assert!(RetainedHostDispatch::lookup(&session, 9, &wrong.as_request(), reservation()).is_err());
    let mut wrong = reservation();
    wrong.max_outcome_bytes += 1;
    assert!(RetainedHostDispatch::lookup(&session, 9, &request.as_request(), wrong).is_err());
    for wrong in [
        NativeRetainedReservationV1 {
            size: reservation().size + 1,
            ..reservation()
        },
        NativeRetainedReservationV1 {
            version: reservation().version + 1,
            ..reservation()
        },
        NativeRetainedReservationV1 {
            operation_slots: 2,
            ..reservation()
        },
        NativeRetainedReservationV1 {
            reserved: 1,
            ..reservation()
        },
    ] {
        assert!(RetainedHostDispatch::lookup(&session, 9, &request.as_request(), wrong).is_err());
    }
    let receipt = loop {
        if let RetainedHostObservation::Terminal { receipt, .. } =
            RetainedHostDispatch::observe(&session, operation, 4096).unwrap()
        {
            break receipt;
        }
        tokio::task::yield_now().await;
    };
    RetainedHostDispatch::acknowledge(&session, operation, receipt).unwrap();
    assert_eq!(
        RetainedHostDispatch::lookup(&session, 9, &request.as_request(), reservation()).unwrap(),
        Some(operation)
    );
    assert!(RetainedHostDispatch::observe(&session, operation, 4096).is_err());
    {
        let ledger = session.inner.transport.lock().unwrap();
        assert_eq!((ledger.bytes, ledger.next, ledger.records.len()), before);
    }
    assert!(RetainedHostDispatch::control(&session, 0, RETAINED_HOST_CONTROL_JOIN).unwrap());
    session.join().await.unwrap();
    session.release().unwrap();
    super::super::product_host_tests::finish(&instance, &activation).await;
}
