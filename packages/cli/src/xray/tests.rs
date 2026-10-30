use super::*;
use clap::Parser;

#[derive(Parser)]
struct Command {
    #[command(flatten)]
    args: XrayArgs,
}

#[test]
fn bootstrap_is_default_and_manual_overrides_must_be_paired() {
    let parsed = Command::try_parse_from(["xray", "00000000-0000-0000-0000-000000000001"]).unwrap();
    assert!(parsed.args.endpoint.is_none());
    assert!(parsed.args.token_file.is_none());
    for trailing in [
        ["--endpoint", "http://127.0.0.1:8080"],
        ["--token-file", "/private/token"],
    ] {
        assert!(
            Command::try_parse_from([
                "xray",
                "00000000-0000-0000-0000-000000000001",
                trailing[0],
                trailing[1],
            ])
            .is_err()
        );
    }
}

#[test]
fn deep_capture_is_explicit_and_duration_is_positive() {
    let parsed = Command::try_parse_from([
        "xray",
        "00000000-0000-0000-0000-000000000001",
        "--endpoint",
        "http://127.0.0.1:8080",
        "--token-file",
        "/private/token",
    ])
    .unwrap();
    assert!(!parsed.args.deep);
    assert!(parsed.args.record.is_none());
    assert!(
        Command::try_parse_from([
            "xray",
            "00000000-0000-0000-0000-000000000001",
            "--endpoint",
            "http://127.0.0.1:8080",
            "--token-file",
            "/private/token",
            "--duration-seconds",
            "0",
        ])
        .is_err()
    );
}

#[test]
fn jsonl_preserves_unknown_domain_data_and_writes_one_line() {
    let envelope = bcode_xray_models::XrayEnvelope {
        schema_version: bcode_xray_models::XRAY_SCHEMA_VERSION,
        observation_id: "observer".into(),
        sequence: 2,
        timestamp_unix_ms: 0,
        event: bcode_xray_models::XrayEvent {
            domain: "future".into(),
            name: "future_event".into(),
            correlation: bcode_xray_models::XrayCorrelation {
                session_id: "session".into(),
                ..Default::default()
            },
            data: serde_json::json!({"text": "line one\nline two 雪"}),
            sensitive: false,
        },
    };
    let mut output = Vec::new();
    write_envelope(&mut output, None, &envelope).unwrap();
    let mut lines = output.split(|byte| *byte == b'\n');
    let line = lines.next().unwrap();
    assert_eq!(
        serde_json::from_slice::<bcode_xray_models::XrayEnvelope>(line).unwrap(),
        envelope
    );
    assert_eq!(lines.next(), Some(b"".as_slice()));
    assert!(lines.next().is_none());
    assert!(finish_recording(None, None).is_err());
}

#[cfg(unix)]
#[test]
fn credential_files_reject_symlinks_and_public_permissions() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("token");
    std::fs::write(&path, "test-token\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(read_token(&path).unwrap(), "test-token\n");
    let link = directory.path().join("link");
    symlink(&path, &link).unwrap();
    assert!(read_token(&link).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read_token(&path).is_err());
}

#[cfg(unix)]
#[test]
fn oversized_credentials_fail_without_echoing_contents() {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("token");
    std::fs::write(&path, "private-test-value".repeat(4096)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let error = read_token(&path).unwrap_err().to_string();
    assert!(!error.contains("private-test-value"));
}
