use super::*;
use bcode_xray_models::{XrayCorrelation, XrayEnvelope, XrayEvent};

fn event() -> ExecutionWatchEvent {
    ExecutionWatchEvent {
        run_id: "run".into(),
        session_id: "session".into(),
        envelope: XrayEnvelope {
            schema_version: XRAY_SCHEMA_VERSION,
            observation_id: "observation".into(),
            sequence: 1,
            timestamp_unix_ms: 0,
            event: XrayEvent {
                domain: "xray".into(),
                name: "ready".into(),
                sensitive: false,
                capture: None,
                correlation: XrayCorrelation {
                    session_id: "session".into(),
                    ..Default::default()
                },
                data: serde_json::json!({}),
            },
        },
    }
}

#[test]
fn relative_recording_parent_resolves_to_absolute_sink_destination() {
    let directory = recording_directory(std::path::Path::new(".")).unwrap();
    assert!(directory.is_absolute());
    assert_eq!(
        directory,
        std::env::current_dir()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join("xray-execution")
    );
    let absent = tempfile::tempdir().unwrap();
    assert_eq!(
        recording_directory(&absent.path().join("missing")),
        Err("watch_recording_parent_unavailable")
    );
}

#[test]
fn page_bound_is_total_envelope_bytes_not_a_per_item_allowance() {
    let event = event();
    let size = u64::try_from(serde_json::to_vec(&event.envelope).unwrap().len()).unwrap();
    assert!(envelopes_fit_page(std::slice::from_ref(&event), size));
    assert!(!envelopes_fit_page(std::slice::from_ref(&event), size - 1));
    let page = [event.clone(), event];
    assert!(!envelopes_fit_page(&page, size));
    assert!(envelopes_fit_page(&page, size * 2));
    assert!(envelopes_fit_page(&[], 0));
}
