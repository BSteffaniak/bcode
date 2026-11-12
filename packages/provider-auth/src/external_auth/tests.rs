use super::*;
use bcode_provider_auth_models::{AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION, AuthSecretValidation};

fn synthetic_home(source: &str, contents: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(source);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
    home
}

fn scoped_approval(
    store: &ExternalAuthStore,
    home: &std::path::Path,
    enabled: &[(&str, &AuthProviderContribution)],
    candidate: &ExternalAuthCandidate,
    application_id: &str,
    now: u64,
) -> ExternalAuthGrant {
    store
        .approve(
            home,
            enabled,
            candidate,
            Some(ExternalAuthScope::Application {
                application_id: application_id.into(),
            }),
            now,
        )
        .unwrap()
}

fn provider(path: &str) -> AuthProviderContribution {
    AuthProviderContribution {
        schema_version: AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
        provider_id: "fixture".into(),
        display_name: "Synthetic".into(),
        methods: vec![AuthMethodContribution::SecretFields {
            method_id: "api_key".into(),
            display_name: "Synthetic static key".into(),
            supports_verification: false,
            supports_revocation: false,
            fields: vec![AuthSecretField {
                credential_id: "api_key".into(),
                prompt: "Key".into(),
                storage_key: "KEY".into(),
                optional: false,
                invocation_env: vec![],
                validation: AuthSecretValidation::default(),
                discovery_sources: vec![AuthCredentialSource::JsonFile {
                    application: "Codex".into(),
                    relative_path: path.into(),
                    pointer: "/key".into(),
                    discriminator: Some(("/type".into(), "api".into())),
                }],
            }],
        }],
    }
}

#[test]
fn consent_restart_revoke_and_static_source_preservation() {
    let home = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join(".codex")).unwrap();
    let file = home.path().join(".codex/auth.json");
    // Discovery and approval must not parse even malformed credentials.
    std::fs::write(&file, "not json").unwrap();
    let provider = provider(".codex/auth.json");
    let enabled = [("fixture.plugin", &provider)];
    let candidate = discover(home.path(), &enabled).unwrap().remove(0);
    assert!(candidate.available);
    let store = ExternalAuthStore::new(state.path());
    assert_eq!(
        store.resolve(home.path(), &enabled, &candidate, Some("cli")),
        Err(ExternalAuthError::NotApproved)
    );
    let grant = store
        .approve(home.path(), &enabled, &candidate, None, 1)
        .unwrap();
    assert_eq!(grant.scope, ExternalAuthScope::AllLocalApplications);
    let bytes = r#"{"type":"api","key":"synthetic-only"}"#;
    std::fs::write(&file, bytes).unwrap();
    let store = ExternalAuthStore::new(state.path());
    let auth = store
        .resolve(home.path(), &enabled, &candidate, Some("desktop"))
        .unwrap()
        .auth
        .unwrap();
    assert_eq!(auth.credentials["api_key"].value, "synthetic-only");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), bytes);
    assert!(
        !std::fs::read_to_string(&store.path)
            .unwrap()
            .contains("synthetic-only")
    );
    assert!(store.revoke(&grant).unwrap());
    assert_eq!(
        store.resolve(home.path(), &enabled, &candidate, Some("cli")),
        Err(ExternalAuthError::NotApproved)
    );
}

#[test]
fn narrow_scope_disabled_declaration_and_drift_fail_closed() {
    let home = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let mut provider = provider(".codex/auth.json");
    let candidate = discover(home.path(), &[("fixture.plugin", &provider)])
        .unwrap()
        .remove(0);
    let store = ExternalAuthStore::new(state.path());
    store
        .approve(
            home.path(),
            &[("fixture.plugin", &provider)],
            &candidate,
            Some(ExternalAuthScope::Application {
                application_id: "cli".into(),
            }),
            1,
        )
        .unwrap();
    assert_eq!(
        store.resolve(
            home.path(),
            &[("fixture.plugin", &provider)],
            &candidate,
            Some("desktop")
        ),
        Err(ExternalAuthError::ScopeDenied)
    );
    assert_eq!(
        store.resolve(home.path(), &[], &candidate, Some("cli")),
        Err(ExternalAuthError::DeclarationChanged)
    );
    provider.display_name = "changed declaration".into();
    assert_eq!(
        store.resolve(
            home.path(),
            &[("fixture.plugin", &provider)],
            &candidate,
            Some("cli")
        ),
        Err(ExternalAuthError::DeclarationChanged)
    );
}

#[test]
fn unknown_state_is_preserved() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("auth")).unwrap();
    let store = ExternalAuthStore::new(root.path());
    let bytes = r#"{"schema_version":200,"grants":[]}"#;
    std::fs::write(&store.path, bytes).unwrap();
    assert_eq!(store.list(), Err(ExternalAuthError::UnsupportedState));
    let provider = provider(".codex/auth.json");
    let candidate = discover(root.path(), &[("fixture.plugin", &provider)])
        .unwrap()
        .remove(0);
    assert_eq!(
        store.approve(
            root.path(),
            &[("fixture.plugin", &provider)],
            &candidate,
            None,
            1
        ),
        Err(ExternalAuthError::UnsupportedState)
    );
    assert_eq!(std::fs::read_to_string(&store.path).unwrap(), bytes);
}

#[test]
fn concurrent_updates_require_lock_and_do_not_overwrite() {
    let root = tempfile::tempdir().unwrap();
    let store = ExternalAuthStore::new(root.path());
    let lock = store.lock().unwrap();
    assert!(store.lock().is_err());
    drop(lock);
    assert!(store.lock().is_ok());
}

/// Explicit external selection rejects conflicting auth/destination configuration before discovery.
#[test]
fn explicit_runtime_binding_rejects_overrides_before_acquisition() {
    for key in [
        "base_url",
        "auth_mode",
        "dialect",
        "catalog_provider_id",
        "billing",
    ] {
        let mut selection = bcode_config::ResolvedModelSelection::default();
        selection
            .settings
            .insert("external_auth_binding".into(), "0".repeat(64));
        selection.settings.insert(key.into(), "untrusted".into());
        assert!(crate::startup::resolve_selected_external(&selection).is_err());
    }
    for use_pool in [true, false] {
        let mut selection = bcode_config::ResolvedModelSelection::default();
        selection
            .settings
            .insert("external_auth_binding".into(), "0".repeat(64));
        if use_pool {
            selection.auth_pool = Some("forbidden".into());
        } else {
            selection.auth_profile = Some("forbidden".into());
        }
        assert!(crate::startup::resolve_selected_external(&selection).is_err());
    }
}

#[test]
fn execution_registry_destination_and_scope_integration() {
    use crate::startup::{ExternalExecutionContext, resolve_external_execution};
    let home = synthetic_home(
        ".codex/auth.json",
        r#"{"type":"api","key":"synthetic-execution-key"}"#,
    );
    let root = tempfile::tempdir().unwrap();
    let provider = provider(".codex/auth.json");
    let enabled = [("fixture.plugin", &provider)];
    let candidate = discover(home.path(), &enabled).unwrap().remove(0);
    let store = ExternalAuthStore::new(root.path());
    // An unrelated narrow approval must not hide a later authorizing approval.
    scoped_approval(&store, home.path(), &enabled, &candidate, "other-app", 1);
    let broad = store
        .approve(home.path(), &enabled, &candidate, None, 2)
        .unwrap();
    let settings = std::collections::BTreeMap::from([
        ("base_url".into(), "https://synthetic.invalid/v1".into()),
        ("dialect".into(), "synthetic".into()),
        ("auth_mode".into(), "api_key".into()),
        ("catalog_provider_id".into(), "fixture".into()),
    ]);
    let choices = [bcode_model::StartupRecommendation {
        provider_plugin_id: "fixture.plugin".into(),
        model_id: "synthetic-model".into(),
        settings: settings.clone(),
    }];
    let mut selection = bcode_config::ResolvedModelSelection {
        provider_plugin_id: Some("fixture.plugin".into()),
        model_id: Some("synthetic-model".into()),
        settings,
        ..Default::default()
    };
    selection
        .settings
        .insert("external_auth_binding".into(), candidate.binding_id.clone());
    let resolve =
        |selection: &bcode_config::ResolvedModelSelection, declarations, application_id| {
            resolve_external_execution(
                selection,
                ExternalExecutionContext {
                    home: home.path(),
                    store: &store,
                    enabled: declarations,
                    application_id,
                    provider_choices: &choices,
                },
            )
        };
    // No verified caller identity: the earlier narrow grant must not hide broad consent.
    let context = resolve(&selection, &enabled, None).unwrap().unwrap();
    assert_eq!(
        context.auth.unwrap().credentials["api_key"].value,
        "synthetic-execution-key"
    );
    assert_eq!(context.settings, choices[0].settings);
    assert!(resolve(&selection, &[], None).is_err());
    let mut malicious = selection.clone();
    malicious
        .settings
        .insert("base_url".into(), "https://attacker.invalid".into());
    assert!(resolve(&malicious, &enabled, None).is_err());
    let mut wrong_model = selection.clone();
    wrong_model.model_id = Some("unapproved-model".into());
    assert!(resolve(&wrong_model, &enabled, None).is_err());
    let mut request_override = selection.clone();
    request_override
        .request
        .insert("billing".into(), serde_json::json!("unapproved").into());
    assert!(resolve(&request_override, &enabled, None).is_err());
    assert!(resolve(&selection, &enabled, Some("")).is_err());
    for use_pool in [false, true] {
        let mut conflicting = selection.clone();
        if use_pool {
            conflicting.auth_pool = Some("unselected-pool".into());
        } else {
            conflicting.auth_profile = Some("unselected-profile".into());
        }
        assert!(resolve(&conflicting, &enabled, None).is_err());
    }
    assert!(store.revoke(&broad).unwrap());
    assert!(resolve(&selection, &enabled, None).is_err());
    let narrow = store
        .approve(
            home.path(),
            &enabled,
            &candidate,
            Some(ExternalAuthScope::Application {
                application_id: "bcode".into(),
            }),
            3,
        )
        .unwrap();
    assert!(resolve(&selection, &enabled, None).is_err());
    assert!(resolve(&selection, &enabled, Some("other-caller")).is_err());
    assert!(resolve(&selection, &enabled, Some("bcode")).is_ok());
    store.revoke(&narrow).unwrap();
    assert!(resolve(&selection, &enabled, Some("bcode")).is_err());
}

#[test]
fn absent_identity_rotation_and_source_rejection_never_select_another_source() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let selected = home.path().join("selected.json");
    let alternate = home.path().join("alternate.json");
    std::fs::write(&selected, r#"{"type":"api","key":"first-synthetic-key"}"#).unwrap();
    std::fs::write(
        &alternate,
        r#"{"type":"api","key":"alternate-synthetic-key"}"#,
    )
    .unwrap();
    let mut provider = provider("selected.json");
    let AuthMethodContribution::SecretFields { fields, .. } = &mut provider.methods[0] else {
        panic!()
    };
    let mut other = fields[0].discovery_sources[0].clone();
    let AuthCredentialSource::JsonFile { relative_path, .. } = &mut other else {
        panic!()
    };
    *relative_path = "alternate.json".into();
    fields[0].discovery_sources.push(other);
    let enabled = [("fixture.plugin", &provider)];
    let candidates = discover(home.path(), &enabled).unwrap();
    let candidate = &candidates[0];
    let store = ExternalAuthStore::new(root.path());
    store
        .approve(home.path(), &enabled, &candidates[1], None, 1)
        .unwrap();
    let narrow = scoped_approval(&store, home.path(), &enabled, candidate, "verified-app", 2);
    assert_eq!(
        store.resolve(home.path(), &enabled, candidate, None),
        Err(ExternalAuthError::ScopeDenied)
    );
    let broad = store
        .approve(home.path(), &enabled, candidate, None, 3)
        .unwrap();
    for (identity, expected) in [
        (None, "first-synthetic-key"),
        (Some("verified-app"), "first-synthetic-key"),
    ] {
        let context = store
            .resolve(home.path(), &enabled, candidate, identity)
            .unwrap();
        assert_eq!(context.auth.unwrap().credentials["api_key"].value, expected);
    }
    assert_eq!(
        store.resolve(home.path(), &enabled, candidate, Some("")),
        Err(ExternalAuthError::InvalidRequest)
    );
    std::fs::write(&selected, r#"{"type":"api","key":"rotated-synthetic-key"}"#).unwrap();
    assert_eq!(
        store
            .resolve(home.path(), &enabled, candidate, None)
            .unwrap()
            .auth
            .unwrap()
            .credentials["api_key"]
            .value,
        "rotated-synthetic-key"
    );
    for rejected in [
        "not json",
        r#"{"type":"oauth","key":"not-an-api-key"}"#,
        r#"{"type":"api","key":""}"#,
    ] {
        std::fs::write(&selected, rejected).unwrap();
        assert_eq!(
            store.resolve(home.path(), &enabled, candidate, None),
            Err(ExternalAuthError::SourceUnavailable)
        );
    }
    std::fs::remove_file(&selected).unwrap();
    assert_eq!(
        store.resolve(home.path(), &enabled, candidate, None),
        Err(ExternalAuthError::SourceUnavailable)
    );
    assert!(store.revoke(&broad).unwrap());
    assert_eq!(
        store.resolve(home.path(), &enabled, candidate, None),
        Err(ExternalAuthError::ScopeDenied)
    );
    assert!(store.revoke(&narrow).unwrap());
    assert_eq!(
        store.resolve(home.path(), &enabled, candidate, None),
        Err(ExternalAuthError::NotApproved)
    );
    // The independently approved alternate still works, but is never an implicit fallback.
    assert_eq!(
        store
            .resolve(home.path(), &enabled, &candidates[1], None)
            .unwrap()
            .auth
            .unwrap()
            .credentials["api_key"]
            .value,
        "alternate-synthetic-key"
    );
}

#[test]
fn generic_resolution_requires_execution_evidence_even_for_bare_external_selection() {
    let config = bcode_config::BcodeConfig::default();
    let selection = bcode_config::ResolvedModelSelection {
        provider_plugin_id: Some("fixture.plugin".into()),
        settings: std::collections::BTreeMap::from([(
            "external_auth_binding".into(),
            "0".repeat(64),
        )]),
        ..Default::default()
    };
    assert!(
        crate::try_resolve_provider_request_context(crate::ProviderRequestContextResolution {
            config: &config,
            selection: selection.clone(),
        })
        .is_err()
    );
    let context =
        crate::resolve_provider_request_context(crate::ProviderRequestContextResolution {
            config: &config,
            selection,
        });
    let auth = context.auth.unwrap();
    assert!(auth.credentials.is_empty());
    assert!(context.env.is_empty());
    assert!(
        auth.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "external_auth_unavailable")
    );
}

fn validated_source_provider() -> AuthProviderContribution {
    use bcode_provider_auth_models::AuthLiteralTokenPolicy;
    let mut provider = provider(".codex/auth.json");
    let AuthMethodContribution::SecretFields { fields, .. } = &mut provider.methods[0] else {
        panic!()
    };
    fields[0].discovery_sources = vec![AuthCredentialSource::JsonFileValidated {
        application: "Synthetic dynamic store".into(),
        relative_path: ".codex/auth.json".into(),
        pointer: "/key".into(),
        discriminator: Some(("/auth_mode".into(), "apikey".into())),
        required_absent_pointers: vec!["/env".into()],
        literal_policy: AuthLiteralTokenPolicy {
            reject_command_prefix: true,
            reject_dollar_expansions: true,
            reject_whitespace_or_control: true,
            reject_environment_identifier: true,
        },
    }];
    provider
}

#[test]
fn source_local_literal_policy_and_discriminator_are_enforced() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join(".codex")).unwrap();
    let path = home.path().join(".codex/auth.json");
    let provider = validated_source_provider();
    let enabled = [("fixture.plugin", &provider)];
    let candidate = discover(home.path(), &enabled).unwrap().remove(0);
    let store = ExternalAuthStore::new(root.path());
    store
        .approve(home.path(), &enabled, &candidate, None, 1)
        .unwrap();
    for value in [
        "!command",
        "$TOKEN",
        "${TOKEN}",
        "has space",
        "line\nfeed",
        "\u{0001}control",
        "BARE_ENV_NAME",
    ] {
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"auth_mode":"apikey", "key":value})).unwrap(),
        )
        .unwrap();
        assert_eq!(
            store.resolve(home.path(), &enabled, &candidate, Some("bcode")),
            Err(ExternalAuthError::SourceUnavailable)
        );
    }
    for mode in ["chatgpt", "future"] {
        std::fs::write(
            &path,
            serde_json::to_vec(
                &serde_json::json!({"auth_mode":mode, "key":"sk-synthetic-literal"}),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            store.resolve(home.path(), &enabled, &candidate, Some("bcode")),
            Err(ExternalAuthError::SourceUnavailable)
        );
    }
    for env in [serde_json::Value::Null, serde_json::json!("TOKEN")] {
        std::fs::write(
            &path,
            serde_json::to_vec(
                &serde_json::json!({"auth_mode":"apikey", "key":"sk-synthetic-literal", "env":env}),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            store.resolve(home.path(), &enabled, &candidate, Some("bcode")),
            Err(ExternalAuthError::SourceUnavailable)
        );
    }
    std::fs::write(
        &path,
        r#"{"auth_mode":"apikey","key":"sk-synthetic-literal"}"#,
    )
    .unwrap();
    assert!(
        store
            .resolve(home.path(), &enabled, &candidate, Some("bcode"))
            .is_ok()
    );
    let encoded = serde_json::to_value(&provider).unwrap();
    assert!(serde_json::from_value::<AuthProviderContribution>(encoded).is_ok());
    let mut changed = provider.clone();
    let AuthMethodContribution::SecretFields { fields, .. } = &mut changed.methods[0] else {
        panic!()
    };
    let AuthCredentialSource::JsonFileValidated {
        literal_policy,
        required_absent_pointers,
        ..
    } = &mut fields[0].discovery_sources[0]
    else {
        panic!()
    };
    literal_policy.reject_dollar_expansions = false;
    required_absent_pointers.push("/alternate".into());
    let changed_enabled = [("fixture.plugin", &changed)];
    let changed_candidate = discover(home.path(), &changed_enabled).unwrap().remove(0);
    assert_ne!(changed_candidate.binding_id, candidate.binding_id);
    assert_eq!(
        store.resolve(home.path(), &changed_enabled, &candidate, Some("bcode")),
        Err(ExternalAuthError::DeclarationChanged)
    );
}

#[test]
fn four_source_kinds_are_declaration_driven_and_discovery_does_not_parse() {
    let home = tempfile::tempdir().unwrap();
    let paths = [
        ".claude/settings.json",
        ".codex/auth.json",
        ".pi/agent/auth.json",
        ".local/share/opencode/auth.json",
    ];
    for path in paths {
        let provider = provider(path);
        let candidates = discover(home.path(), &[("fixture.plugin", &provider)]).unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(!candidates[0].available);
    }
}
