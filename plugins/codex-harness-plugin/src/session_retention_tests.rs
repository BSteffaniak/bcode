use super::*;
use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessCanonicalAck, HarnessExecutionEvent, HarnessHydrationReply,
    HarnessHydrationResolution, HarnessRetentionContext, HarnessWorkState,
};
use std::os::unix::fs::PermissionsExt as _;

#[test]
fn canonical_ack_pages_forty_native_processes_without_lifetime_gap() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("fake-codex");
    std::fs::write(&executable, include_str!("../tests/fixtures/app_server.py")).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let launch = Launch {
        executable,
        codex_home: directory.path().join("codex-home"),
        working_directory: directory.path().to_owned(),
        ephemeral_credentials: false,
        timeout: std::time::Duration::from_secs(2),
    };
    let binding = HarnessSessionBinding {
        schema_version: HARNESS_SCHEMA_VERSION,
        session_id: "session".into(),
        instance_id: "instance".into(),
        native_session_id: "pending".into(),
        native_version: "pending".into(),
        generation: 1,
    };
    let mut session = CodexSession::open(&launch, binding, false).unwrap();
    session
        .submit(&HarnessSubmission {
            submission_id: "submission".into(),
            attempt_id: "attempt".into(),
            session_id: "session".into(),
            generation: 1,
            text: "lifecycle-inventory".into(),
            placement: HarnessPlacement::NextTurn,
        })
        .unwrap();
    let mut context = None;
    let mut canonical = Vec::new();
    let mut processes = BTreeSet::new();
    let mut scans = Vec::new();
    for revision in 1..600 {
        if let Some(context) = context.take() {
            session.apply_retention(&context).unwrap();
        }
        let (page, completeness) = session.observe(4).unwrap();
        assert_ne!(completeness, HarnessCompleteness::Gap);
        for observation in &page {
            if let HarnessObservationKind::Execution {
                event:
                    HarnessExecutionEvent::BackgroundWork {
                        native_task_id: Some(id),
                        state: HarnessWorkState::Running,
                        ..
                    },
            } = &observation.kind
                && id.parse::<u32>().is_ok_and(|n| n >= 1000)
            {
                processes.insert(id.clone());
            }
        }
        canonical.extend(page.clone());
        let receipt = session.retention_page(&page).unwrap();
        if let Some(scan) = session.take_inventory_page() {
            scans.push(scan);
        }
        let replies = session
            .hydration_requests()
            .into_iter()
            .map(|request| {
                // Inventory fixture identities are new. Host real implementation uses
                // bounded native-reference lookup, not replay or guessed association.
                HarnessHydrationReply {
                    request_id: request.request_id,
                    reference: request.reference,
                    canonical_revision: revision,
                    resolution: HarnessHydrationResolution::Absent,
                }
            })
            .collect();
        context = Some(HarnessRetentionContext {
            connection_id: receipt.connection_id,
            acknowledgment: Some(HarnessCanonicalAck {
                page_id: receipt.page_id,
                canonical_revision: revision,
                observations_digest: receipt.observations_digest,
            }),
            hydration: replies,
        });
        if processes.len() == 40 && scans.last().is_some_and(|scan| scan.next_cursor.is_none()) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(processes.len(), 40);
    assert_eq!(scans.len(), 10);
    assert!(
        scans
            .windows(2)
            .all(|pair| pair[0].scan_id == pair[1].scan_id
                && pair[1].page_index == pair[0].page_index + 1)
    );
    assert!(canonical.iter().any(|observation| matches!(&observation.kind,HarnessObservationKind::Execution { event:HarnessExecutionEvent::SubmissionTerminal { output:Some(output),.. } } if output=="foreground complete")));
}
