use super::*;
use bcode_provider_auth_models::{
    AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION, AuthMethodContribution, AuthSecretField,
    AuthSecretValidation,
};

const PLUGIN: &str = "fixture.plugin";
const FUTURE: u64 = 4_102_444_800;

fn source() -> AuthExternalSource {
    AuthExternalSource {
        source_id: "fixture-tokens".into(),
        application: "Fixture Agent".into(),
        method_id: "tokens".into(),
        location: AuthExternalLocation {
            relative_path: ".fixture/auth.json".into(),
            root_override: Some(AuthExternalRootOverride {
                env: "FIXTURE_HOME".into(),
                relative_path: "auth.json".into(),
            }),
        },
        discriminator: Some(("/mode".into(), "tokens".into())),
        required_absent_pointers: vec!["/api_key".into()],
        credentials: vec![
            AuthExternalCredential {
                credential_id: "access_token".into(),
                pointer: "/tokens/access".into(),
                optional: false,
            },
            AuthExternalCredential {
                credential_id: "account_id".into(),
                pointer: "/tokens/account".into(),
                optional: true,
            },
        ],
        account_pointer: Some("/tokens/account".into()),
        literal_policy: AuthLiteralTokenPolicy {
            reject_command_prefix: false,
            reject_dollar_expansions: false,
            reject_whitespace_or_control: true,
            reject_environment_identifier: false,
        },
        expiry: Some(AuthExternalExpiry {
            pointer: "/tokens/expires".into(),
            format: AuthExternalExpiryFormat::UnixSeconds,
        }),
    }
}

fn provider(display_name: &str) -> AuthProviderContribution {
    AuthProviderContribution {
        schema_version: AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
        provider_id: "fixture".into(),
        display_name: display_name.into(),
        methods: vec![AuthMethodContribution::SecretFields {
            method_id: "tokens".into(),
            display_name: "Tokens".into(),
            fields: vec![AuthSecretField {
                credential_id: "access_token".into(),
                prompt: "Token".into(),
                storage_key: "FIXTURE_TOKEN".into(),
                optional: false,
                invocation_env: vec![],
                validation: AuthSecretValidation::default(),
                discovery_sources: vec![],
            }],
            supports_verification: false,
            supports_revocation: false,
        }],
        external_sources: vec![source()],
    }
}

struct Fixture {
    home: tempfile::TempDir,
    _state: tempfile::TempDir,
    store: ExternalAuthStore,
    provider: AuthProviderContribution,
}

impl Fixture {
    fn new() -> Self {
        let state = tempfile::tempdir().unwrap();
        let fixture = Self {
            home: tempfile::tempdir().unwrap(),
            store: ExternalAuthStore::new(state.path()),
            _state: state,
            provider: provider("Fixture"),
        };
        fixture.write(&serde_json::json!({
            "mode": "tokens",
            "tokens": {"access": "first", "account": "acct-1", "refresh": "r", "expires": FUTURE}
        }));
        fixture
    }

    fn roots(&self) -> SourceRoots {
        SourceRoots {
            home: self.home.path().to_owned(),
            overrides: BTreeMap::new(),
        }
    }

    fn write(&self, document: &serde_json::Value) {
        let path = self.home.path().join(".fixture/auth.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(document).unwrap()).unwrap();
    }

    fn candidate(&self) -> ExternalAuthCandidate {
        discover(&self.roots(), &[(PLUGIN, &self.provider)])
            .unwrap()
            .remove(0)
    }

    fn approve(&self) -> Result<ExternalAuthGrant, ExternalAuthError> {
        self.store.approve(
            &self.roots(),
            &[(PLUGIN, &self.provider)],
            &self.candidate(),
            1,
        )
    }

    fn recipe(binding: &str) -> bcode_model::ProviderRequestContext {
        bcode_model::ProviderRequestContext {
            settings: BTreeMap::from([("endpoint".into(), "approved".into())]),
            external_auth_binding: Some(binding.into()),
            ..Default::default()
        }
    }

    fn dispatch(
        &self,
        recipe: &bcode_model::ProviderRequestContext,
    ) -> Result<bcode_model::ProviderRequestContext, ExternalAuthError> {
        let choice = bcode_model::StartupRecommendation {
            provider_plugin_id: PLUGIN.into(),
            model_id: "model".into(),
            settings: BTreeMap::from([("endpoint".into(), "approved".into())]),
        };
        self.store.resolve_dispatch(DispatchResolution {
            recipe,
            provider_plugin_id: PLUGIN,
            model_id: "model",
            roots: &self.roots(),
            enabled: &[(PLUGIN, &self.provider)],
            provider_choices: &[choice],
            routing_settings: &[],
        })
    }

    fn access_token(&self, binding: &str) -> Result<String, ExternalAuthError> {
        let context = self.dispatch(&Self::recipe(binding))?;
        assert_eq!(context.external_auth_binding.as_deref(), Some(binding));
        let credentials = context.auth.unwrap().credentials;
        assert!(!credentials.contains_key("refresh"));
        Ok(credentials["access_token"].value.clone())
    }
}

#[test]
fn discovery_reports_presence_without_parsing_and_binding_survives_cosmetic_edits() {
    let fixture = Fixture::new();
    std::fs::write(fixture.home.path().join(".fixture/auth.json"), "not json").unwrap();
    let candidate = fixture.candidate();
    assert!(candidate.available);
    assert_eq!(candidate.application, "Fixture Agent");
    let renamed = provider("Renamed provider");
    let renamed_candidate = discover(&fixture.roots(), &[(PLUGIN, &renamed)])
        .unwrap()
        .remove(0);
    assert_eq!(renamed_candidate.binding_id, candidate.binding_id);
    std::fs::remove_file(fixture.home.path().join(".fixture/auth.json")).unwrap();
    assert!(!fixture.candidate().available);
}

#[test]
fn approval_reads_once_to_reject_unsupported_documents_and_records_the_account() {
    let fixture = Fixture::new();
    fixture.write(&serde_json::json!({"mode": "tokens", "api_key": "k", "tokens": {"access": "a", "account": "x", "expires": FUTURE}}));
    assert_eq!(fixture.approve(), Err(ExternalAuthError::SourceUnavailable));
    assert_eq!(fixture.store.list().unwrap(), []);
    fixture.write(&serde_json::json!({"mode": "tokens", "api_key": null, "tokens": {"access": "a", "account": "x", "expires": FUTURE}}));
    let grant = fixture.approve().unwrap();
    assert_eq!(
        grant.account_fingerprint.as_ref().map(String::len),
        Some(16)
    );
    assert_eq!(fixture.store.list().unwrap(), [grant]);
}

#[test]
fn dispatch_follows_rotation_and_rejects_account_changes_and_revocation() {
    let fixture = Fixture::new();
    let binding = fixture.candidate().binding_id;
    assert_eq!(
        fixture.access_token(&binding),
        Err(ExternalAuthError::NotApproved)
    );
    fixture.approve().unwrap();
    assert_eq!(fixture.access_token(&binding).unwrap(), "first");
    fixture.write(&serde_json::json!({"mode": "tokens", "tokens": {"access": "rotated", "account": "acct-1", "expires": FUTURE}}));
    assert_eq!(fixture.access_token(&binding).unwrap(), "rotated");
    fixture.write(
        &serde_json::json!({"mode": "tokens", "tokens": {"access": "other", "account": "acct-2", "expires": FUTURE}}),
    );
    assert_eq!(
        fixture.access_token(&binding),
        Err(ExternalAuthError::AccountChanged)
    );
    fixture.approve().unwrap();
    assert_eq!(fixture.access_token(&binding).unwrap(), "other");
    assert!(fixture.store.revoke(&binding).unwrap());
    assert!(!fixture.store.revoke(&binding).unwrap());
    assert_eq!(
        fixture.access_token(&binding),
        Err(ExternalAuthError::NotApproved)
    );
}

#[test]
fn dispatch_never_redirects_or_merges_other_authentication() {
    let fixture = Fixture::new();
    let binding = fixture.approve().unwrap().binding_id;
    let mut redirected = Fixture::recipe(&binding);
    redirected
        .settings
        .insert("endpoint".into(), "https://untrusted.invalid".into());
    assert_eq!(
        fixture.dispatch(&redirected),
        Err(ExternalAuthError::DestinationRejected)
    );
    for mutate in [
        |recipe: &mut bcode_model::ProviderRequestContext| {
            recipe.auth_profile = Some("fallback".into());
        },
        |recipe: &mut bcode_model::ProviderRequestContext| {
            recipe.auth_pool = Some("pool".into());
        },
        |recipe: &mut bcode_model::ProviderRequestContext| {
            recipe.env.insert("FIXTURE_TOKEN".into(), "client".into());
        },
        |recipe: &mut bcode_model::ProviderRequestContext| {
            recipe.auth = Some(bcode_model::ProviderAuthContext::default());
        },
    ] {
        let mut recipe = Fixture::recipe(&binding);
        mutate(&mut recipe);
        assert_eq!(
            fixture.dispatch(&recipe),
            Err(ExternalAuthError::InvalidRequest)
        );
    }
}

#[test]
fn source_identity_changes_and_root_overrides_are_distinct_bindings() {
    let fixture = Fixture::new();
    let binding = fixture.approve().unwrap().binding_id;
    let mut changed = fixture.provider.clone();
    changed.external_sources[0].source_id = "fixture-tokens-v2".into();
    let candidate = discover(&fixture.roots(), &[(PLUGIN, &changed)])
        .unwrap()
        .remove(0);
    assert_ne!(candidate.binding_id, binding);
    let alternate = tempfile::tempdir().unwrap();
    std::fs::write(
        alternate.path().join("auth.json"),
        r#"{"mode":"tokens","tokens":{"access":"alt","account":"acct-1","expires":4102444800}}"#,
    )
    .unwrap();
    let mut roots = fixture.roots();
    roots
        .overrides
        .insert("FIXTURE_HOME".into(), alternate.path().to_owned());
    let overridden = discover(&roots, &[(PLUGIN, &fixture.provider)])
        .unwrap()
        .remove(0);
    assert_ne!(overridden.binding_id, binding);
    // The approved home-relative binding no longer resolves under a different root.
    let choice = bcode_model::StartupRecommendation {
        provider_plugin_id: PLUGIN.into(),
        model_id: "model".into(),
        settings: BTreeMap::from([("endpoint".into(), "approved".into())]),
    };
    assert_eq!(
        fixture.store.resolve_dispatch(DispatchResolution {
            recipe: &Fixture::recipe(&binding),
            provider_plugin_id: PLUGIN,
            model_id: "model",
            roots: &roots,
            enabled: &[(PLUGIN, &fixture.provider)],
            provider_choices: &[choice],
            routing_settings: &[],
        }),
        Err(ExternalAuthError::DeclarationChanged)
    );
}

#[test]
fn concurrent_readers_share_the_consent_lock() {
    let fixture = Fixture::new();
    let binding = fixture.approve().unwrap().binding_id;
    let held = fixture.store.lock(false).unwrap();
    assert_eq!(fixture.access_token(&binding).unwrap(), "first");
    drop(held);
}

#[test]
fn damaged_state_fails_closed() {
    let fixture = Fixture::new();
    fixture.approve().unwrap();
    std::fs::write(
        &fixture.store.path,
        b"{\"schema_version\":99,\"grants\":[]}",
    )
    .unwrap();
    assert_eq!(
        fixture.store.list(),
        Err(ExternalAuthError::UnsupportedState)
    );
    assert_eq!(
        fixture.access_token(&fixture.candidate().binding_id),
        Err(ExternalAuthError::UnsupportedState)
    );
}

#[test]
fn expiry_is_checked_locally_before_dispatch() {
    let fixture = Fixture::new();
    let binding = fixture.approve().unwrap().binding_id;
    fixture.write(&serde_json::json!({
        "mode": "tokens", "tokens": {"access": "stale", "account": "acct-1", "expires": 1}
    }));
    assert_eq!(
        fixture.access_token(&binding),
        Err(ExternalAuthError::Expired)
    );
    fixture.write(&serde_json::json!({
        "mode": "tokens", "tokens": {"access": "stale", "account": "acct-1"}
    }));
    assert_eq!(
        fixture.access_token(&binding),
        Err(ExternalAuthError::SourceUnavailable),
        "a declared expiry that cannot be read is an unsupported document"
    );
}

#[test]
fn jwt_expiry_reads_the_payload_exp_claim() {
    use base64::Engine as _;
    let encode = |value: serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
    };
    let token = format!(
        "{}.{}.sig",
        encode(serde_json::json!({"alg": "none"})),
        encode(serde_json::json!({"exp": 42}))
    );
    let declared = AuthExternalExpiry {
        pointer: "/token".into(),
        format: AuthExternalExpiryFormat::JwtExp,
    };
    assert_eq!(
        expiry(&serde_json::json!({"token": token}), &declared),
        Ok(42)
    );
    assert_eq!(
        expiry(&serde_json::json!({"token": "not-a-jwt"}), &declared),
        Err(ExternalAuthError::SourceUnavailable)
    );
}

#[test]
fn status_reports_each_connection_without_failing_the_list() {
    let fixture = Fixture::new();
    fixture.approve().unwrap();
    let status = |fixture: &Fixture| {
        fixture
            .store
            .status(&fixture.roots(), &[(PLUGIN, &fixture.provider)])
            .unwrap()
            .remove(0)
            .status
    };
    assert_eq!(
        status(&fixture),
        ExternalConnectionStatus::Ready {
            expires_at_unix: Some(FUTURE)
        }
    );
    fixture.write(&serde_json::json!({
        "mode": "tokens", "tokens": {"access": "a", "account": "acct-1", "expires": 1}
    }));
    assert_eq!(status(&fixture), ExternalConnectionStatus::Expired);
    fixture.write(&serde_json::json!({
        "mode": "tokens", "tokens": {"access": "a", "account": "acct-2", "expires": FUTURE}
    }));
    assert_eq!(status(&fixture), ExternalConnectionStatus::AccountChanged);
    std::fs::remove_file(fixture.home.path().join(".fixture/auth.json")).unwrap();
    assert_eq!(
        status(&fixture),
        ExternalConnectionStatus::SourceUnavailable
    );
    let without_source = AuthProviderContribution {
        external_sources: Vec::new(),
        ..fixture.provider.clone()
    };
    assert_eq!(
        fixture
            .store
            .status(&fixture.roots(), &[(PLUGIN, &without_source)])
            .unwrap()
            .remove(0)
            .status,
        ExternalConnectionStatus::DeclarationChanged
    );
}

#[test]
fn routing_settings_decide_the_destination_and_other_settings_are_free() {
    let choice = BTreeMap::from([("base_url".to_owned(), "https://api".to_owned())]);
    let routing = ["base_url".to_owned(), "openai.base_url".to_owned()];
    let with = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    };
    assert!(destination_matches(
        &choice,
        &with(&[("base_url", "https://api")]),
        &routing
    ));
    assert!(destination_matches(
        &choice,
        &with(&[("base_url", "https://api"), ("reasoning", "high")]),
        &routing
    ));
    // A namespaced alias that would override the plain key is rejected.
    assert!(!destination_matches(
        &choice,
        &with(&[
            ("base_url", "https://api"),
            ("openai.base_url", "https://evil")
        ]),
        &routing
    ));
    assert!(!destination_matches(&choice, &with(&[]), &routing));
    // Without a declaration, only an exact match is accepted.
    assert!(!destination_matches(
        &choice,
        &with(&[("base_url", "https://api"), ("reasoning", "high")]),
        &[]
    ));
}
