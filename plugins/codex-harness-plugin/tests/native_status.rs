#![cfg(unix)]
//! Explicit, bounded native-custody status acceptance. Never starts a thread,
//! logs in, refreshes, reads credential files or exports native account fields.
use bcode_codex_harness_plugin::native_connection::{
    candidate, inspect_native_status, prepare_native,
};
use bcode_codex_harness_plugin::transport::Launch;
use bcode_harness_models::native_connections::{NativeConnectionError, NativePrepareRequest};
use std::path::Path;
use std::{path::PathBuf, time::Duration};

#[test]
#[ignore = "explicit opt-in: installed native status only, no generation or token export"]
fn installed_native_status_exact_artifact_remains_execution_blocked() {
    let installation = candidate(
        Path::new("/etc/profiles/per-user/bsteffaniak/bin/codex"),
        Path::new("/Users/bsteffaniak/.codex"),
    )
    .expect("native installation unavailable");
    assert_eq!(
        installation.executable,
        "/nix/store/6h8n09ic7cvcici5wq16h8rl5490y5hh-codex-dev-0.164.0/bin/codex"
    );
    assert_eq!(
        installation.installation_id,
        "sha256:028fe6b5f4c0ffb910d1cc8bcae41f6d22115d05c5dea6170a484ee2f0f4f946"
    );
    inspect_native_status(&Launch {
        executable: PathBuf::from(&installation.executable),
        codex_home: PathBuf::from(&installation.config_namespace),
        working_directory: PathBuf::from("/"),
        ephemeral_credentials: false,
        timeout: Duration::from_secs(5),
    })
    .expect("native non-refreshing account status unavailable");
    let request = NativePrepareRequest {
        schema_version: 1,
        request_id: "installed-native-status-only".to_owned(),
        candidate: installation,
    };
    assert_eq!(
        prepare_native(&request),
        Err(NativeConnectionError::CompatibilityUnavailable)
    );
}
