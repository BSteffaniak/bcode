//! Explicit opt-in, status-only compatibility check. Never invokes generation or login.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
use bcode_claude_code::{
    AuthMethod, BillingRoute, LaunchOptions, Limits, NativeAuthSupport, NativeEnvironment,
    SettingsInheritance, SettlementSupport, auth_status,
};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt::Write, path::PathBuf};

/// All selectors are mandatory to avoid probing an ambient installation or credential namespace.
/// The directory must be an existing empty temporary directory explicitly supplied by the caller.
#[tokio::test]
#[ignore = "requires explicitly authorized installed artifact, digest, version and empty config directory"]
async fn installed_isolated_native_status_only() {
    let executable = PathBuf::from(
        std::env::var_os("BCODE_CLAUDE_TEST_EXECUTABLE").expect("explicit executable"),
    );
    let expected_digest = std::env::var("BCODE_CLAUDE_TEST_SHA256").expect("explicit sha256");
    let expected_version = std::env::var("BCODE_CLAUDE_TEST_VERSION").expect("explicit version");
    let directory = PathBuf::from(
        std::env::var_os("BCODE_CLAUDE_TEST_CONFIG_DIRECTORY").expect("explicit empty namespace"),
    );
    assert!(executable.is_absolute());
    assert_eq!(
        executable.canonicalize().expect("canonical executable"),
        executable
    );
    assert!(directory.is_absolute());
    assert_eq!(
        directory.canonicalize().expect("canonical directory"),
        directory
    );
    assert!(
        std::fs::read_dir(&directory)
            .expect("namespace")
            .next()
            .is_none(),
        "never inspect live credentials"
    );
    let bytes = std::fs::read(&executable).expect("executable bytes only");
    let digest = Sha256::digest(bytes);
    let mut actual = String::with_capacity(64);
    for byte in digest {
        write!(actual, "{byte:02x}").expect("String write");
    }
    assert_eq!(actual, expected_digest, "exact authorized artifact");
    let options = LaunchOptions {
        executable,
        expected_version,
        cwd: directory.clone(),
        config_directory: directory.clone(),
        environment: NativeEnvironment(BTreeMap::from([
            ("HOME".into(), directory.to_str().expect("UTF-8").into()),
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("LANG".into(), "C".into()),
            ("DISABLE_AUTOUPDATER".into(), "1".into()),
        ])),
        billing_route: BillingRoute::NativeAccount,
        settings: SettingsInheritance::Native,
        setting_sources: Vec::new(),
        resume_session_id: None,
        settlement_support: SettlementSupport::Unknown,
        // This test checks precisely this claim. It does not publish an execution profile.
        auth_support: NativeAuthSupport::VerifiedStatus,
        limits: Limits::default(),
    };
    let status = auth_status(&options)
        .await
        .expect("sanitized exact-version native status");
    assert!(!status.logged_in);
    assert_eq!(status.method, AuthMethod::None);
    assert_eq!(status.config_directory, directory);
}
