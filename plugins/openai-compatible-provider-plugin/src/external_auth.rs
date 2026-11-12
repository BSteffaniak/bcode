//! Credential documents owned by other coding agents that this provider can use in place.
//!
//! These declarations only describe locations and formats. The host reads a document only
//! after the user approves the connection, and never refreshes or rewrites it. ChatGPT tokens
//! are delivered without their refresh token, so an expired Codex sign-in asks the user to
//! renew it in Codex instead of rotating Codex's credentials behind its back.

use bcode_provider_auth_models::external::{
    AuthExternalCredential, AuthExternalLocation, AuthExternalRootOverride, AuthExternalSource,
    AuthLiteralTokenPolicy,
};

const TOKEN_POLICY: AuthLiteralTokenPolicy = AuthLiteralTokenPolicy {
    reject_command_prefix: false,
    reject_dollar_expansions: false,
    reject_whitespace_or_control: true,
    reject_environment_identifier: false,
};

fn location(relative_path: &str, env: &str, root_relative_path: &str) -> AuthExternalLocation {
    AuthExternalLocation {
        relative_path: relative_path.to_owned(),
        root_override: Some(AuthExternalRootOverride {
            env: env.to_owned(),
            relative_path: root_relative_path.to_owned(),
        }),
    }
}

fn credential(credential_id: &str, pointer: &str, optional: bool) -> AuthExternalCredential {
    AuthExternalCredential {
        credential_id: credential_id.to_owned(),
        pointer: pointer.to_owned(),
        optional,
    }
}

/// External documents usable by the given registered provider ID.
#[must_use]
pub fn external_sources(provider_id: &str) -> Vec<AuthExternalSource> {
    if !matches!(provider_id, "openai" | "openrouter" | "xai") {
        return Vec::new();
    }
    let mut sources = vec![
        AuthExternalSource {
            source_id: "opencode-api-key".to_owned(),
            application: "OpenCode".to_owned(),
            method_id: "api_key".to_owned(),
            location: location(
                ".local/share/opencode/auth.json",
                "XDG_DATA_HOME",
                "opencode/auth.json",
            ),
            discriminator: Some((format!("/{provider_id}/type"), "api".to_owned())),
            required_absent_pointers: Vec::new(),
            credentials: vec![credential("api_key", &format!("/{provider_id}/key"), false)],
            account_pointer: None,
            literal_policy: TOKEN_POLICY,
        },
        AuthExternalSource {
            source_id: "pi-api-key".to_owned(),
            application: "Pi".to_owned(),
            method_id: "api_key".to_owned(),
            location: location(".pi/agent/auth.json", "PI_CODING_AGENT_DIR", "auth.json"),
            discriminator: Some((format!("/{provider_id}/type"), "api_key".to_owned())),
            // Pi can resolve keys from commands or environment references; only literals apply.
            required_absent_pointers: vec![format!("/{provider_id}/env")],
            credentials: vec![credential("api_key", &format!("/{provider_id}/key"), false)],
            account_pointer: None,
            literal_policy: AuthLiteralTokenPolicy {
                reject_command_prefix: true,
                reject_dollar_expansions: true,
                reject_whitespace_or_control: true,
                reject_environment_identifier: true,
            },
        },
    ];
    if provider_id == "openai" {
        sources.push(AuthExternalSource {
            source_id: "codex-api-key".to_owned(),
            application: "Codex (API key)".to_owned(),
            method_id: "api_key".to_owned(),
            location: location(".codex/auth.json", "CODEX_HOME", "auth.json"),
            discriminator: Some(("/auth_mode".to_owned(), "apikey".to_owned())),
            // Never reinterpret a subscription or mixed document as API-key billing.
            required_absent_pointers: vec!["/tokens".to_owned()],
            credentials: vec![credential("api_key", "/OPENAI_API_KEY", false)],
            account_pointer: None,
            literal_policy: TOKEN_POLICY,
        });
        sources.push(AuthExternalSource {
            source_id: "codex-chatgpt".to_owned(),
            application: "Codex (ChatGPT sign-in)".to_owned(),
            method_id: "chatgpt".to_owned(),
            location: location(".codex/auth.json", "CODEX_HOME", "auth.json"),
            discriminator: None,
            // Read-only: the refresh token stays with Codex.
            required_absent_pointers: Vec::new(),
            credentials: vec![
                credential("access_token", "/tokens/access_token", false),
                credential("account_id", "/tokens/account_id", true),
                credential("id_token", "/tokens/id_token", true),
            ],
            account_pointer: Some("/tokens/account_id".to_owned()),
            literal_policy: TOKEN_POLICY,
        });
    }
    sources
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_provider_auth::external_auth::{
        DispatchResolution, ExternalAuthError, ExternalAuthStore, SourceRoots, discover,
    };
    use bcode_provider_auth_models::AuthProviderContribution;
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::path::Path;

    fn contribution(provider: &str) -> AuthProviderContribution {
        let contribution = AuthProviderContribution {
            schema_version: bcode_provider_auth_models::AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
            provider_id: provider.to_owned(),
            display_name: provider.to_owned(),
            methods: vec![
                crate::api_key_auth_method("BCODE_OPENAI_API_KEY", "Key"),
                crate::openai_browser_auth_method(),
            ],
            external_sources: external_sources(provider),
        };
        contribution.validate().unwrap();
        contribution
    }

    fn write(root: &Path, relative: &str, document: &Value) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(document).unwrap()).unwrap();
    }

    fn roots(home: &Path) -> SourceRoots {
        SourceRoots {
            home: home.to_owned(),
            overrides: BTreeMap::new(),
        }
    }

    /// Approve and dispatch through the production host path; return delivered credentials.
    fn connect(
        roots: &SourceRoots,
        provider: &str,
        source_id: &str,
    ) -> Result<BTreeMap<String, String>, ExternalAuthError> {
        let contribution = contribution(provider);
        let enabled = [("openai-compatible", &contribution)];
        let candidate = discover(roots, &enabled)?
            .into_iter()
            .find(|candidate| candidate.target.source_id == source_id)
            .unwrap();
        assert!(candidate.available);
        let state = tempfile::tempdir().unwrap();
        let store = ExternalAuthStore::new(state.path());
        let grant = store.approve(roots, &enabled, &candidate, 1)?;
        let recipe = bcode_model::ProviderRequestContext {
            external_auth_binding: Some(grant.binding_id),
            ..Default::default()
        };
        let choice = bcode_model::StartupRecommendation {
            provider_plugin_id: "openai-compatible".into(),
            model_id: "model".into(),
            settings: BTreeMap::new(),
        };
        let context = store.resolve_dispatch(DispatchResolution {
            recipe: &recipe,
            provider_plugin_id: "openai-compatible",
            model_id: "model",
            roots,
            enabled: &enabled,
            provider_choices: &[choice],
        })?;
        Ok(context
            .auth
            .unwrap()
            .credentials
            .into_iter()
            .map(|(id, credential)| (id, credential.value))
            .collect())
    }

    #[test]
    fn codex_chatgpt_delivers_tokens_without_the_refresh_token() {
        let home = tempfile::tempdir().unwrap();
        write(
            home.path(),
            ".codex/auth.json",
            &json!({"OPENAI_API_KEY": null, "tokens": {
                "access_token": "access", "refresh_token": "refresh",
                "id_token": "id", "account_id": "account"}}),
        );
        let credentials = connect(&roots(home.path()), "openai", "codex-chatgpt").unwrap();
        assert_eq!(
            credentials,
            BTreeMap::from([
                ("access_token".into(), "access".into()),
                ("account_id".into(), "account".into()),
                ("id_token".into(), "id".into()),
            ])
        );
        // A subscription document is never API-key billing.
        assert_eq!(
            connect(&roots(home.path()), "openai", "codex-api-key"),
            Err(ExternalAuthError::SourceUnavailable)
        );
    }

    #[test]
    fn codex_api_key_mode_accepts_null_tokens_and_honors_codex_home() {
        let home = tempfile::tempdir().unwrap();
        let codex_home = tempfile::tempdir().unwrap();
        write(
            codex_home.path(),
            "auth.json",
            &json!({"auth_mode": "apikey", "OPENAI_API_KEY": "sk-key", "tokens": null}),
        );
        let mut roots = roots(home.path());
        roots
            .overrides
            .insert("CODEX_HOME".into(), codex_home.path().to_owned());
        assert_eq!(
            connect(&roots, "openai", "codex-api-key").unwrap()["api_key"],
            "sk-key"
        );
    }

    #[test]
    fn pi_accepts_only_literal_keys_and_opencode_rejects_oauth_entries() {
        let home = tempfile::tempdir().unwrap();
        write(
            home.path(),
            ".pi/agent/auth.json",
            &json!({"xai": {"type": "api_key", "key": "xai-literal"}}),
        );
        assert_eq!(
            connect(&roots(home.path()), "xai", "pi-api-key").unwrap()["api_key"],
            "xai-literal"
        );
        for key in ["!op read key", "$XAI_KEY", "XAI_KEY"] {
            write(
                home.path(),
                ".pi/agent/auth.json",
                &json!({"xai": {"type": "api_key", "key": key}}),
            );
            assert_eq!(
                connect(&roots(home.path()), "xai", "pi-api-key"),
                Err(ExternalAuthError::SourceUnavailable)
            );
        }
        write(
            home.path(),
            ".local/share/opencode/auth.json",
            &json!({"openrouter": {"type": "oauth", "key": "k", "access": "a"}}),
        );
        assert_eq!(
            connect(&roots(home.path()), "openrouter", "opencode-api-key"),
            Err(ExternalAuthError::SourceUnavailable)
        );
    }
}
