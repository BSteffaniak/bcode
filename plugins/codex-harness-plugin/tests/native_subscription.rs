#![cfg(unix)]
//! Opt-in exact-version subscription acceptance through the production session API.
//! No native transcript or account identity is printed. Background settlement and
//! subsequent conditional start remain unproven, even after a successful turn.
use bcode_codex_harness_plugin::{
    session::CodexSession,
    transport::{CodexError, Launch},
};
use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessActivity, HarnessCompleteness, HarnessContinuation,
    HarnessDeliveryState, HarnessObservationKind, HarnessPlacement, HarnessSessionBinding,
    HarnessSubmission, HarnessTerminalDisposition,
};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const EXECUTABLE: &str = "/nix/store/a4pdj3h0lfdhmahz8gz1a8nhg0cpnnxc-codex-dev-0.164.0/bin/codex";
const SHA256: &str = "65fc3e92f25ec7f781a19f293ce2f0dd472631551eccdba5b0cda273022aaf34";
const TEST: &str = "installed_native_subscription_exact_first_turn";
const CHILD: &str = "BCODE_NATIVE_CODEX_ISOLATED_TEST";

fn verify_artifact() {
    assert!(
        std::fs::canonicalize("/etc/profiles/per-user/braden/bin/codex")
            .is_ok_and(|path| path == Path::new(EXECUTABLE)),
        "installed Codex selection changed"
    );
    let hash = Command::new("sha256sum")
        .arg(EXECUTABLE)
        .stderr(Stdio::null())
        .output()
        .expect("sha256sum required for native artifact verification");
    assert!(hash.status.success(), "native artifact hashing failed");
    assert!(
        std::str::from_utf8(&hash.stdout)
            .is_ok_and(|text| text.split_whitespace().next() == Some(SHA256)),
        "native artifact hash mismatch"
    );
    let version = Command::new(EXECUTABLE)
        .arg("--version")
        .stderr(Stdio::null())
        .output()
        .expect("native version probe failed");
    assert!(version.status.success(), "native version probe rejected");
    assert!(
        version.stdout == b"codex-cli 0.164.0\n",
        "native version mismatch"
    );
}

// A separate invocation of this one test gives Launch a cleared environment without
// unsafe process-global environment mutation in a potentially multithreaded test runner.
fn isolated_invocation() {
    let state = tempfile::tempdir().expect("isolated native state");
    let workspace = state.path().join("workspace");
    std::fs::create_dir(&workspace).expect("isolated native workspace");
    let namespace = std::env::var_os("BCODE_NATIVE_CODEX_HOME")
        .map(PathBuf::from)
        .expect("set BCODE_NATIVE_CODEX_HOME to the existing ChatGPT credential namespace");
    assert!(
        namespace.is_absolute() && namespace.is_dir(),
        "invalid credential namespace"
    );
    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    command
        .env_clear()
        .env(CHILD, "1")
        .env("CODEX_HOME", namespace)
        .env("CODEX_SQLITE_HOME", state.path())
        .env("TMPDIR", state.path())
        .current_dir(&workspace)
        .args([
            "--ignored",
            "--exact",
            TEST,
            "--nocapture",
            "--test-threads=1",
        ])
        .stdin(Stdio::null());
    // Preserve native credential/keyring and TLS lookup, not API keys, proxy routes,
    // provider overrides, account overrides, or unrelated Codex configuration.
    for key in [
        "HOME",
        "USER",
        "PATH",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "NIX_SSL_CERT_FILE",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let mut child = command.spawn().expect("isolated test invocation failed");
    let deadline = Instant::now() + Duration::from_secs(150);
    loop {
        if let Some(status) = child.try_wait().expect("isolated test wait failed") {
            assert!(status.success(), "isolated native acceptance failed");
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("isolated native acceptance exceeded total deadline");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
#[ignore = "explicit opt-in: spends one existing ChatGPT subscription turn on exact Codex 0.164.0"]
fn installed_native_subscription_exact_first_turn() {
    if std::env::var_os(CHILD).is_none() {
        isolated_invocation();
        return;
    }
    for key in [
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "OPENAI_BASE_URL",
        "OPENAI_ORG_ID",
    ] {
        assert!(
            std::env::var_os(key).is_none(),
            "ambient API route was not cleared"
        );
    }
    verify_artifact();
    let launch = Launch {
        executable: PathBuf::from(EXECUTABLE),
        codex_home: PathBuf::from(std::env::var_os("CODEX_HOME").expect("credential namespace")),
        working_directory: std::env::current_dir().expect("isolated workspace"),
        ephemeral_credentials: false,
        timeout: Duration::from_secs(15),
    };
    let retained_binding = HarnessSessionBinding {
        schema_version: HARNESS_SCHEMA_VERSION,
        session_id: format!("native-test-{}", uuid::Uuid::new_v4()),
        instance_id: "native-codex-subscription-test".into(),
        native_session_id: "pending-native-confirmation".into(),
        native_version: "0.164.0".into(),
        generation: 1,
    };
    // This production path verifies account/read.type == chatgpt, built-in OpenAI,
    // no configured MCP servers, remote-control fencing, and native ephemeral policy.
    // It never invokes login, copies credentials, supplies keys or falls back to APIs.
    let mut session = CodexSession::open_ephemeral(&launch, retained_binding)
        .expect("native subscription admission failed");
    let retained_binding = session.binding().clone();
    assert!(
        retained_binding.native_version == "0.164.0",
        "binding version mismatch"
    );
    assert!(retained_binding.native_session_id != "pending-native-confirmation");
    let answer = format!("BCODE_NATIVE_OK_{}", uuid::Uuid::new_v4());
    let retained_submission = HarnessSubmission {
        submission_id: uuid::Uuid::new_v4().to_string(),
        attempt_id: uuid::Uuid::new_v4().to_string(),
        session_id: retained_binding.session_id.clone(),
        generation: retained_binding.generation,
        placement: HarnessPlacement::NextTurn,
        text: format!(
            "Compatibility check. Do not use tools, change files, request permissions, or delegate. \
             Reply with exactly this single line and nothing else: {answer}"
        ),
    };
    retained_submission
        .validate()
        .expect("retained submission invalid");
    let receipt = session
        .submit(&retained_submission)
        .expect("native first submission failed");
    assert!(
        receipt.state == HarnessDeliveryState::NativeAccepted,
        "native turn/start not accepted"
    );
    assert!(receipt.submission_id == retained_submission.submission_id);
    assert!(receipt.attempt_id == retained_submission.attempt_id);
    assert!(receipt.session_id == retained_binding.session_id);
    assert!(receipt.generation == retained_binding.generation);
    assert!(receipt.native_request_id.is_some());
    // Same exact retained attempt returns its cached native receipt; it cannot spend again.
    assert!(
        session
            .submit(&retained_submission)
            .is_ok_and(|again| again == receipt)
    );
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut accepted_turn = None;
    let mut final_result = false;
    let mut terminal = false;
    let mut count = 0;
    while Instant::now() < deadline && !terminal {
        let (page, completeness) = session.observe(32).expect("native observation failed");
        assert!(
            completeness != HarnessCompleteness::Gap,
            "native observation stream gap"
        );
        for event in page {
            count += 1;
            assert!(count <= 4096, "native observation budget exceeded");
            event
                .validate()
                .expect("invalid normalized native evidence");
            assert!(event.session_id == retained_binding.session_id);
            assert!(event.generation == retained_binding.generation);
            assert!(
                event.completeness != HarnessCompleteness::Gap,
                "native evidence gap"
            );
            match event.kind {
                HarnessObservationKind::SubmissionAccepted {
                    submission_id,
                    attempt_id,
                    native_turn_id,
                } => {
                    assert!(accepted_turn.is_none(), "duplicate native acceptance");
                    assert!(submission_id == retained_submission.submission_id);
                    assert!(attempt_id == retained_submission.attempt_id);
                    assert!(event.native_correlations.contains(&native_turn_id));
                    accepted_turn = Some(native_turn_id);
                }
                HarnessObservationKind::Result { text } => {
                    let turn = accepted_turn
                        .as_ref()
                        .expect("result without native acceptance");
                    assert!(event.native_correlations.contains(turn));
                    assert!(event.completeness == HarnessCompleteness::Complete);
                    assert!(text.trim() == answer, "native final item answer mismatch");
                    final_result = true;
                }
                HarnessObservationKind::NativeTurnTerminal {
                    native_turn_id,
                    disposition,
                } => {
                    assert!(accepted_turn.as_ref() == Some(&native_turn_id));
                    assert!(disposition == HarnessTerminalDisposition::Completed);
                    assert!(final_result, "turn completed without correlated final item");
                    terminal = true;
                }
                HarnessObservationKind::Settlement { .. } => {
                    panic!("native completion must not synthesize background settlement");
                }
                HarnessObservationKind::Tool { .. }
                | HarnessObservationKind::Subagent { .. }
                | HarnessObservationKind::Interaction { .. } => {
                    panic!("unexpected native tool, delegation or approval in restricted probe");
                }
                HarnessObservationKind::Diagnostic { .. } => {
                    panic!("native diagnostic during acceptance");
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        terminal && final_result,
        "native first turn deadline exceeded"
    );
    assert!(session.status().activity != HarnessActivity::Settled);
    assert!(session.status().continuation == HarnessContinuation::Unknown);
    let mut subsequent = retained_submission.clone();
    subsequent.submission_id = uuid::Uuid::new_v4().to_string();
    subsequent.attempt_id = uuid::Uuid::new_v4().to_string();
    assert!(matches!(
        session.submit(&subsequent),
        Err(CodexError::Unsupported)
    ));
    eprintln!(
        "native_codex version=0.164.0 sha256_verified=true chatgpt=true ephemeral=true \
         exact_acceptance=true final_item=true terminal=completed settlement_proven=false \
         subsequent_start=unsupported tool_observed=false"
    );
}
