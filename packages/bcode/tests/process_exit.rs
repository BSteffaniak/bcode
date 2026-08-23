#![cfg(feature = "app")]

use std::process::Command;

fn run_bcode(arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bcode"))
        .args(arguments)
        .output()
        .expect("run bcode binary")
}

#[test]
fn invalid_cli_arguments_exit_with_usage_category_and_stderr_diagnostic() {
    let output = run_bcode(&["send", "not-a-session-id", "hello"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(
        stderr.contains("invalid value"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn malformed_interaction_json_exits_as_validation_without_stdout_noise() {
    let payload = tempfile::NamedTempFile::new().expect("temporary payload");
    std::fs::write(payload.path(), b"not-json").expect("write malformed JSON");
    let output = Command::new(env!("CARGO_BIN_EXE_bcode"))
        .args([
            "interaction",
            "respond",
            "exchange-1",
            "--payload",
            payload.path().to_str().expect("UTF-8 path"),
            "--json",
        ])
        .output()
        .expect("run bcode binary");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.starts_with("error:"), "unexpected stderr: {stderr}");
}
