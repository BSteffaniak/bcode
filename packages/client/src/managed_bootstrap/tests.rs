use super::*;

#[test]
fn managed_recovery_route_validates_distinct_intent_before_transport() {
    let intent = serde_json::json!({
        "session_id": bcode_session_models::SessionId::new(),
        "instance_id": "selected", "expected_generation": 1, "recovery_id": "exact-recovery"
    });
    let envelope = |intent: &serde_json::Value, operation| {
        serde_json::to_vec(&ManagedSessionRequest {
            schema_version: 1,
            operation,
            request_json: serde_json::to_string(intent).unwrap(),
        })
        .unwrap()
    };
    let payload = envelope(&intent, ManagedSessionOperation::Recover);
    assert!(validate_route(MANAGED_SESSION_INTERFACE_ID, "recover", &payload).is_ok());
    assert!(validate_route(MANAGED_SESSION_INTERFACE_ID, "start", &payload).is_err());
    let mut invalid = intent.clone();
    invalid["expected_generation"] = serde_json::json!(0);
    assert!(
        validate_route(
            MANAGED_SESSION_INTERFACE_ID,
            "recover",
            &envelope(&invalid, ManagedSessionOperation::Recover)
        )
        .is_err()
    );
    invalid = intent.clone();
    invalid["replay_submission"] = serde_json::json!("forbidden");
    assert!(
        validate_route(
            MANAGED_SESSION_INTERFACE_ID,
            "recover",
            &envelope(&invalid, ManagedSessionOperation::Recover)
        )
        .is_err()
    );
    assert!(
        validate_route(
            MANAGED_SESSION_INTERFACE_ID,
            "start",
            &envelope(&intent, ManagedSessionOperation::Start)
        )
        .is_err()
    );
}

fn config() -> bcode_config::BcodeConfig {
    // Decode the same effective-configuration representation carried by the handshake.
    bcode_config::decode_effective_config(
        r#"
[model]
provider_plugin_id = "example.native-model"
model_id = "example-model"
auth_profile = "unrelated"
[auth.profiles.unrelated]
backend = "env"
provider_id = "example"
owner_plugin_id = "example.native-model"
[auth.profiles.unrelated.map.api_key]
env = "BCODE_MANAGED_BOOTSTRAP_TEST_UNSET_CREDENTIAL_714B"
[plugins.config."bcode.harness"]
enabled = true
"#,
    )
    .unwrap()
}

#[test]
fn missing_unrelated_credentials_preserve_complete_declarative_configuration() {
    let config = config();
    let directory = PathBuf::from("/chosen/workspace");
    let context = configured_context(&config, directory.clone()).unwrap();
    assert!(
        config.auth.profiles["unrelated"]
            .map
            .contains_key("api_key")
    );
    assert_eq!(context.working_directory, Some(directory));
    let decoded =
        bcode_config::decode_effective_config(context.effective_config_toml.as_deref().unwrap())
            .unwrap();
    assert_eq!(
        bcode_config::encode_effective_config(&decoded).unwrap(),
        bcode_config::encode_effective_config(&config).unwrap()
    );
    assert_eq!(
        context.selected_provider_plugin_id.as_deref(),
        Some("example.native-model")
    );
    assert!(context.provider_context.auth.is_none());
    assert_eq!(context.provider_context.auth_candidates, Vec::new());
    assert!(context.provider_context.env.is_empty());
    assert!(context.process_env.is_empty());
}

#[test]
fn model_materialization_failure_is_not_converted_into_managed_or_empty_auth() {
    let config = config();
    assert!(configured_context(&config, PathBuf::from("/workspace")).is_ok());
    let result = crate::configured_runtime_context_with_auth(
        &config,
        PathBuf::from("/workspace"),
        |_, selection| {
            assert_eq!(selection.auth_profile.as_deref(), Some("unrelated"));
            Err(ClientError::Protocol(
                "selected credentials unavailable".into(),
            ))
        },
    );
    assert!(
        matches!(result, Err(ClientError::Protocol(message)) if message == "selected credentials unavailable")
    );
}

#[test]
fn malformed_auth_and_unknown_profile_are_not_deferred() {
    let mut config = config();
    config
        .auth
        .profiles
        .get_mut("unrelated")
        .unwrap()
        .owner_plugin_id = Some("wrong.owner".into());
    assert!(configured_context(&config, PathBuf::from("/workspace")).is_err());
    // The existing model authentication path must still reject invalid ownership too.
    assert!(crate::configured_runtime_context(&config, PathBuf::from("/workspace")).is_err());
    config.model.profile = Some("missing-profile".into());
    assert!(configured_context(&config, PathBuf::from("/workspace")).is_err());
    assert!(bcode_config::decode_effective_config("[broken").is_err());
}

#[test]
fn managed_route_surface_rejects_models_and_arbitrary_services() {
    for (interface, operation, payload) in [
        ("bcode.model-provider/v1", "start_turn", b"{}".as_slice()),
        ("other.service/v1", "anything", b"{}".as_slice()),
        (CONFIGURATION_INTERFACE, "other", b"{}".as_slice()),
        (CONFIGURATION_INTERFACE, "prepare", b"{}".as_slice()),
        (
            CONFIGURATION_INTERFACE,
            "catalog",
            br#"{"unexpected":true}"#.as_slice(),
        ),
        (MANAGED_SESSION_INTERFACE_ID, "start", b"{}".as_slice()),
    ] {
        assert!(validate_route(interface, operation, payload).is_err());
    }
    assert!(validate_route(CONFIGURATION_INTERFACE, "catalog", b"{}").is_ok());
    assert!(
        validate_route(
            CONFIGURATION_INTERFACE,
            "prepare",
            br#"{"selection":{"kind":"default"}}"#
        )
        .is_ok()
    );
    assert!(
        validate_route(
            CONFIGURATION_INTERFACE,
            "prepare",
            br#"{"selection":{"kind":"instance","instance_id":"primary"}}"#
        )
        .is_ok()
    );
}

#[tokio::test]
async fn malformed_configuration_stays_connection_blocking_and_routes_fail_before_transport() {
    let client = ManagedSessionClient {
        inner: BcodeClient::default_endpoint_with_context(Err(ClientError::Protocol(
            "invalid configuration".into(),
        ))),
    };
    let error = client
        .call_plugin_service(
            CONFIGURATION_INTERFACE.into(),
            "catalog".into(),
            b"{}".to_vec(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("invalid configuration"));
    let error = client
        .call_plugin_service(
            "unrelated.provider/v1".into(),
            "run_turn".into(),
            b"{}".to_vec(),
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("only permits validated managed operations")
    );
}

#[test]
fn explicit_model_context_is_not_used_as_a_managed_fallback() {
    let context = configured_context(&config(), PathBuf::from("/workspace")).unwrap();
    let client = ManagedSessionClient {
        inner: BcodeClient::default_endpoint_with_context(Ok(context.clone())),
    };
    assert_eq!(client.inner.runtime_context, Some(context));
    assert!(client.inner.runtime_context_error.is_none());
    assert_eq!(client.inner.endpoint, crate::default_endpoint());
}
