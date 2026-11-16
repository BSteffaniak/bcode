use super::{Arc, ProductHostSession, SessionCode, Uuid};
use bcode_plugin_sdk::definition::host_session::ProductHostSessionClient;
use bcode_plugin_sdk::definition::host_session_models::SessionOutcome;
use bcode_plugin_sdk::definition::host_session_models::SessionServiceKind;
use bcode_plugin_sdk::definition::host_session_models::{
    HostSessionRequest as Q, HostSessionResponse as R, JobReservation,
};
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn typed_close_join_retains_producer_and_exact_receipt() {
    let (instance, activation) = super::super::product_host_tests::active();
    let gate = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let services =
        bmux_plugin_sdk::definition::PluginHostServices::new(|_| panic!("no sync fallback"))
            .with_async({
                let gate = Arc::clone(&gate);
                let calls = Arc::clone(&calls);
                move |request| {
                    let gate = Arc::clone(&gate);
                    let calls = Arc::clone(&calls);
                    Box::pin(async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        gate.notified().await;
                        Ok(request.payload)
                    })
                }
            });
    let session = ProductHostSession::new(&activation, services, Arc::new(())).unwrap();
    let client = session.typed_client().unwrap();
    let admission = Q::BeginJob {
        request_id: Uuid::new_v4(),
        reservation: JobReservation {
            operations: 1,
            request_bytes: 256,
            response_bytes: 256,
        },
    };
    let begun = client.request(admission.clone()).await.unwrap();
    assert_eq!(client.request(admission).await.unwrap(), begun);
    let R::JobBegun { job } = begun else {
        panic!("job")
    };
    let start = Q::Start {
        job,
        request_id: Uuid::new_v4(),
        capability: "test/v1".into(),
        kind: SessionServiceKind::Command,
        interface: "test/v1".into(),
        operation: "run".into(),
        payload: vec![7],
    };
    let started = client.request(start.clone()).await.unwrap();
    assert_eq!(client.request(start).await.unwrap(), started);
    let R::Started { operation } = started else {
        panic!("operation")
    };
    assert_eq!(
        client.request(Q::JoinJob { job }).await.unwrap(),
        R::Pending
    );
    assert_eq!(
        client.request(Q::CloseSession).await.unwrap(),
        R::AdmissionsClosed
    );
    assert_eq!(client.request(Q::JoinSession).await.unwrap(), R::Pending);
    // Producer is still owned after all request futures returned.
    gate.notify_one();
    let outcome = loop {
        let response = client.observe(job, operation).await.unwrap();
        if response != R::Pending {
            break response;
        }
        tokio::task::yield_now().await;
    };
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let R::Terminal {
        receipt,
        outcome: SessionOutcome::Completed { payload },
    } = outcome
    else {
        panic!("terminal")
    };
    assert_eq!(payload, vec![7]);
    acknowledge_exact_outcome(&client, job, operation, receipt).await;
    assert_eq!(client.request(Q::JoinJob { job }).await.unwrap(), R::Joined);
    assert_eq!(
        client.request(Q::ReleaseJob { job }).await.unwrap(),
        R::Released
    );
    assert_eq!(client.request(Q::JoinSession).await.unwrap(), R::Joined);
    assert_eq!(
        client.request(Q::ReleaseSession).await.unwrap(),
        R::Released
    );
    super::super::product_host_tests::finish(&instance, &activation).await;
}

async fn acknowledge_exact_outcome(
    client: &ProductHostSessionClient,
    job: Uuid,
    operation: Uuid,
    receipt: Uuid,
) {
    assert_eq!(
        client
            .request(Q::Acknowledge {
                job,
                operation,
                receipt: Uuid::new_v4()
            })
            .await
            .unwrap(),
        R::Rejected {
            code: SessionCode::StaleReceipt
        }
    );
    assert_eq!(
        client
            .request(Q::Acknowledge {
                job,
                operation,
                receipt
            })
            .await
            .unwrap(),
        R::Acknowledged
    );
    assert_eq!(
        client
            .request(Q::Acknowledge {
                job,
                operation,
                receipt
            })
            .await
            .unwrap(),
        R::Acknowledged
    );
}
