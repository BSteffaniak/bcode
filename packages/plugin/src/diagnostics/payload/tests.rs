//! Behavioral protocol tests; no builds or test execution inside the fixture.
use super::*;
use bcode_xray::{XRAY_SCHEMA_VERSION, XrayCorrelation, XrayHub, XrayObserveRequest};

fn setup() -> (XrayHub, bcode_xray::XrayObservation, Entry) {
    let hub = XrayHub::default();
    let observation = hub
        .observe(XrayObserveRequest {
            schema_version: XRAY_SCHEMA_VERSION,
            session_id: "host-origin".into(),
            deep_capture: true,
        })
        .unwrap();
    let entry = Entry {
        key: 0,
        capture: hub.context(XrayCorrelation {
            session_id: "host-origin".into(),
            ..Default::default()
        }),
        owners: AtomicUsize::new(1),
        payloads: std::sync::Mutex::new(Transfers::default()),
    };
    (hub, observation, entry)
}
fn negotiate(entry: &Entry) {
    assert!(matches!(
        control(
            entry,
            PayloadControl::Negotiate {
                version: PAYLOAD_VERSION
            }
        ),
        PayloadReply::Supported {
            version: PAYLOAD_VERSION,
            ..
        }
    ));
}
fn begin(entry: &Entry, bytes: &[u8]) -> u64 {
    let PayloadReply::Accepted { transfer } = control(
        entry,
        PayloadControl::Begin {
            byte_len: bytes.len(),
            sha256: hex::encode(Sha256::digest(bytes)),
            media_type: "application/octet-stream".into(),
        },
    ) else {
        panic!("begin rejected")
    };
    transfer
}
fn chunk(entry: &Entry, id: u64, offset: usize, bytes: &[u8]) -> PayloadReply {
    control(
        entry,
        PayloadControl::Chunk {
            transfer: id,
            offset,
            bytes: bytes.to_vec(),
        },
    )
}

#[test]
fn complete_ordered_chunks_return_host_origin_digest_and_no_private_path() {
    let (_hub, _observation, entry) = setup();
    negotiate(&entry);
    let bytes = vec![47; 96 * 1024];
    let transfer = begin(&entry, &bytes);
    for (index, part) in bytes.chunks(MAX_CHUNK_BYTES).enumerate() {
        assert!(matches!(
            chunk(&entry, transfer, index * MAX_CHUNK_BYTES, part),
            PayloadReply::Progress
        ));
    }
    let PayloadReply::Retained { reference } = control(&entry, PayloadControl::Finish { transfer })
    else {
        panic!("retention failed")
    };
    assert_eq!(reference.session_id, "host-origin");
    assert_eq!(reference.byte_len, bytes.len() as u64);
    assert_eq!(reference.sha256, hex::encode(Sha256::digest(&bytes)));
    assert!(!reference.payload_id.contains('/'));
    assert!(entry.payloads.lock().unwrap().pending.is_empty());
    assert!(matches!(
        control(&entry, PayloadControl::Finish { transfer }),
        PayloadReply::Unavailable
    ));
}

#[test]
fn versions_order_repetition_incomplete_and_digest_fail_closed() {
    let (_hub, _observation, entry) = setup();
    assert!(matches!(
        control(&entry, PayloadControl::Finish { transfer: 1 }),
        PayloadReply::Unavailable
    ));
    assert!(matches!(
        control(&entry, PayloadControl::Negotiate { version: 99 }),
        PayloadReply::Unavailable
    ));
    negotiate(&entry);
    let id = begin(&entry, b"abcd");
    assert!(matches!(
        chunk(&entry, id, 1, b"abcd"),
        PayloadReply::Unavailable
    ));
    let id = begin(&entry, b"abcd");
    assert!(matches!(
        chunk(&entry, id, 0, b"ab"),
        PayloadReply::Progress
    ));
    assert!(matches!(
        chunk(&entry, id, 0, b"ab"),
        PayloadReply::Unavailable
    ));
    let id = begin(&entry, b"abcd");
    assert!(matches!(
        control(&entry, PayloadControl::Finish { transfer: id }),
        PayloadReply::Unavailable
    ));
    let id = begin(&entry, b"abcd");
    assert!(matches!(
        chunk(&entry, id, 0, b"dcba"),
        PayloadReply::Progress
    ));
    assert!(matches!(
        control(&entry, PayloadControl::Finish { transfer: id }),
        PayloadReply::Unavailable
    ));
    assert!(entry.payloads.lock().unwrap().pending.is_empty());
}

#[test]
fn lease_quota_oversized_chunk_expiry_abort_and_revocation_cleanup() {
    let (_hub, observation, entry) = setup();
    negotiate(&entry);
    for _ in 0..MAX_TRANSFERS {
        begin(&entry, b"abcd");
    }
    assert!(matches!(
        control(
            &entry,
            PayloadControl::Begin {
                byte_len: 4,
                sha256: hex::encode(Sha256::digest(b"abcd")),
                media_type: "text/plain".into(),
            }
        ),
        PayloadReply::Unavailable
    ));
    entry.payloads.lock().unwrap().pending.clear();
    let id = begin(&entry, &vec![1; MAX_CHUNK_BYTES + 1]);
    assert!(matches!(
        chunk(&entry, id, 0, &vec![1; MAX_CHUNK_BYTES + 1]),
        PayloadReply::Unavailable
    ));
    let id = begin(&entry, b"abcd");
    entry
        .payloads
        .lock()
        .unwrap()
        .pending
        .get_mut(&id)
        .unwrap()
        .started = Instant::now()
        .checked_sub(TTL)
        .expect("test clock supports transfer expiry");
    assert!(matches!(
        control(&entry, PayloadControl::Finish { transfer: id }),
        PayloadReply::Unavailable
    ));
    let id = begin(&entry, b"abcd");
    assert!(matches!(
        control(&entry, PayloadControl::Abort { transfer: id }),
        PayloadReply::Progress
    ));
    assert!(entry.payloads.lock().unwrap().pending.is_empty());
    let id = begin(&entry, b"abcd");
    drop(observation);
    assert!(matches!(
        chunk(&entry, id, 0, b"abcd"),
        PayloadReply::Unavailable
    ));
    assert!(entry.payloads.lock().unwrap().pending.is_empty());
}

#[tokio::test]
async fn renegotiation_abort_and_lease_drop_report_loss_and_release_owned_reservations() {
    let (_hub, mut observation, entry) = setup();
    negotiate(&entry);
    for _ in 0..MAX_TRANSFERS {
        begin(&entry, b"unfinished");
    }
    assert!(matches!(
        control(&entry, PayloadControl::Negotiate { version: 99 }),
        PayloadReply::Unavailable
    ));
    assert!(entry.payloads.lock().unwrap().pending.is_empty());
    negotiate(&entry);
    let id = begin(&entry, b"cancelled");
    assert!(matches!(
        control(&entry, PayloadControl::Abort { transfer: id }),
        PayloadReply::Progress
    ));
    // A duplicate abort is cleanup, not another lost payload.
    assert!(matches!(
        control(&entry, PayloadControl::Abort { transfer: id }),
        PayloadReply::Progress
    ));
    for _ in 0..MAX_TRANSFERS {
        begin(&entry, b"owner released");
    }
    drop(entry);
    observation.finalize().unwrap();
    let mut terminal_loss = None;
    while let Some(envelope) = observation.recv().await {
        if envelope.event.name == "finalized" {
            terminal_loss = Some(
                envelope.event.data["hub_contention_drops"]
                    .as_u64()
                    .unwrap(),
            );
        }
    }
    // Four invalidated transfers, one rejected negotiation, one aborted transfer,
    // and four unfinished transfers on lease release. No global counter assumptions.
    assert_eq!(terminal_loss, Some(2 * MAX_TRANSFERS as u64 + 2));
}

#[test]
fn lock_contention_never_waits_and_empty_payload_is_explicit_retention() {
    let (_hub, _observation, entry) = setup();
    let guard = entry.payloads.lock().unwrap();
    assert!(matches!(
        control(
            &entry,
            PayloadControl::Negotiate {
                version: PAYLOAD_VERSION
            }
        ),
        PayloadReply::Unavailable
    ));
    drop(guard);
    negotiate(&entry);
    let transfer = begin(&entry, b"");
    assert!(
        matches!(control(&entry, PayloadControl::Finish { transfer }), PayloadReply::Retained { reference } if reference.byte_len == 0)
    );
}
