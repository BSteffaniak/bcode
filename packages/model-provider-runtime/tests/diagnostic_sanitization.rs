use bcode_model_provider_runtime::sanitize_provider_diagnostic;

#[test]
fn sanitizes_common_provider_credential_shapes_and_bounds_output() {
    let aws_key = "AKIA1234567890ABCDEF";
    let message = format!(
        "Authorization: Bearer sk-secret api_key=key-secret \
         {{\"refresh_token\":\"refresh-secret\",\"client_secret\":\"client-secret\"}} \
         https://user:password@example.test/path?access_token=query-secret&safe=yes \
         aws={aws_key}"
    );

    let sanitized = sanitize_provider_diagnostic(&message);

    for secret in [
        "sk-secret",
        "key-secret",
        "refresh-secret",
        "client-secret",
        "password",
        "query-secret",
        aws_key,
    ] {
        assert!(!sanitized.contains(secret), "leaked {secret}: {sanitized}");
    }
    assert!(sanitized.contains("[REDACTED]"));
    assert!(sanitized.contains("safe=yes"));

    let oversized = sanitize_provider_diagnostic(&"x".repeat(5_000));
    assert!(oversized.chars().count() <= 4_110);
    assert!(oversized.ends_with("…[TRUNCATED]"));
}

#[test]
fn does_not_redact_secret_words_without_assignment_or_credentials() {
    let message = "the provider reported an invalid secret configuration and password policy";
    assert_eq!(sanitize_provider_diagnostic(message), message);
}
