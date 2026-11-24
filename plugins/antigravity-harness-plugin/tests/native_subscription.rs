//! Opt-in live test against an installed `agy` CLI with existing native Google OAuth login.
//!
//! Run with:
//! `cargo test -p bcode_antigravity_harness_plugin --test native_subscription -- --ignored --nocapture`
#![cfg(unix)]

use bcode_antigravity_harness_plugin::session::AcpSession;
use bcode_antigravity_harness_plugin::transport::{Launch, probe_version};
use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessExecutionEvent, HarnessObservationKind, HarnessPlacement,
    HarnessSessionBinding, HarnessSubmission, HarnessTerminalDisposition,
};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[test]
#[ignore = "Requires installed `agy` CLI and live Google OAuth login"]
fn live_first_turn_against_installed_agy() {
    let exe = std::env::var_os("BCODE_NATIVE_AGY_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/profiles/per-user/bsteffaniak/bin/agy"));
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let gemini_dir = std::env::var_os("BCODE_NATIVE_AGY_GEMINI_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".gemini"));
    let tmp = tempfile::tempdir().expect("tempdir");

    let launch = Launch {
        executable: exe,
        gemini_dir,
        working_directory: tmp.path().to_path_buf(),
        timeout: Duration::from_secs(30),
    };
    let version = probe_version(&launch).expect("probe agy --version");
    assert!(!version.is_empty());

    let binding = HarnessSessionBinding {
        schema_version: HARNESS_SCHEMA_VERSION,
        session_id: "live-agy-test".into(),
        instance_id: "antigravity".into(),
        native_session_id: "open-1".into(),
        native_version: version,
        generation: 1,
    };
    let mut session =
        AcpSession::open_with_model(&launch, binding, false, Some("gemini-3.8-flash-low"))
            .expect("open live agy session");
    session
        .submit(&HarnessSubmission {
            submission_id: "sub-1".into(),
            attempt_id: "att-1".into(),
            session_id: "live-agy-test".into(),
            generation: 1,
            text: "Reply with exactly PONG and nothing else.".into(),
            placement: HarnessPlacement::NextTurn,
        })
        .expect("submit");

    let deadline = Instant::now() + Duration::from_secs(45);
    let mut completed = false;
    while Instant::now() < deadline && !completed {
        let (batch, _) = session.observe(16).expect("observe");
        for obs in batch {
            if matches!(
                obs.kind,
                HarnessObservationKind::Execution {
                    event: HarnessExecutionEvent::SubmissionTerminal {
                        disposition: HarnessTerminalDisposition::Completed,
                        ..
                    }
                }
            ) {
                completed = true;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(completed, "expected live ACP turn to complete");
    session.send_close();
}
