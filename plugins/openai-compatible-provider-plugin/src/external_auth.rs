//! Static external credential declarations; no native settings, commands, or refresh.
//!
//! Format references: Codex `codex-rs/login/src/auth/storage.rs` (`AuthDotJson`),
//! OpenCode v1.2.27 `packages/opencode/src/auth/index.ts`, and Pi
//! `packages/coding-agent/src/core/auth-storage.ts` / `resolve-config-value.ts`.
//! Roots are fixed home-relative declarations. Custom `CODEX_HOME`, `XDG_DATA_HOME`,
//! and `PI_CODING_AGENT_DIR` require separately authorized roots, not ambient expansion.

use bcode_provider_auth_models::{AuthCredentialSource, AuthLiteralTokenPolicy};

fn provider(storage_key: &str) -> Option<(&'static str, &'static str)> {
    match storage_key {
        "BCODE_OPENAI_API_KEY" => Some(("OPENAI_API_KEY", "openai")),
        "BCODE_OPENROUTER_API_KEY" => Some(("OPENROUTER_API_KEY", "openrouter")),
        "BCODE_XAI_API_KEY" => Some(("XAI_API_KEY", "xai")),
        _ => None,
    }
}

/// Existing import declarations, also usable by consent-gated in-place resolution.
///
/// This function only describes locations; it neither reads nor authorizes them.
/// Dynamic Pi expressions are rejected by the source-local host-enforced policy.
#[must_use]
pub fn api_key_discovery_sources(storage_key: &str) -> Vec<AuthCredentialSource> {
    let Some((name, provider)) = provider(storage_key) else {
        return Vec::new();
    };
    let mut sources = vec![
        AuthCredentialSource::Environment {
            name: name.to_owned(),
        },
        AuthCredentialSource::JsonFileValidated {
            application: "OpenCode".to_owned(),
            relative_path: ".local/share/opencode/auth.json".to_owned(),
            pointer: format!("/{provider}/key"),
            discriminator: Some((format!("/{provider}/type"), "api".to_owned())),
            required_absent_pointers: Vec::new(),
            literal_policy: AuthLiteralTokenPolicy {
                reject_command_prefix: false,
                reject_dollar_expansions: false,
                reject_whitespace_or_control: true,
                reject_environment_identifier: false,
            },
        },
    ];
    if provider == "openai" {
        // Never reinterpret a legacy, subscription, or mixed store as API-key billing.
        // Even a null tokens field is rejected conservatively as an ambiguous store.
        sources.push(AuthCredentialSource::JsonFileValidated {
            application: "Codex (API key only)".to_owned(),
            relative_path: ".codex/auth.json".to_owned(),
            pointer: "/OPENAI_API_KEY".to_owned(),
            discriminator: Some(("/auth_mode".to_owned(), "apikey".to_owned())),
            required_absent_pointers: vec!["/tokens".to_owned()],
            literal_policy: AuthLiteralTokenPolicy {
                reject_command_prefix: false,
                reject_dollar_expansions: false,
                reject_whitespace_or_control: true,
                reject_environment_identifier: false,
            },
        });
    }
    sources.push(AuthCredentialSource::JsonFileValidated {
        required_absent_pointers: vec![format!("/{provider}/env")],
        literal_policy: AuthLiteralTokenPolicy {
            reject_command_prefix: true,
            reject_dollar_expansions: true,
            reject_whitespace_or_control: true,
            reject_environment_identifier: true,
        },
        application: "Pi (API key only)".to_owned(),
        relative_path: ".pi/agent/auth.json".to_owned(),
        pointer: format!("/{provider}/key"),
        discriminator: Some((format!("/{provider}/type"), "api_key".to_owned())),
    });
    sources
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_provider_auth::discovery::read_selected;
    use bcode_provider_auth_models::{AuthMethodContribution, AuthSecretField};
    use serde_json::{Value, json};

    const PROVIDERS: [(&str, &str, &str); 3] = [
        ("BCODE_OPENAI_API_KEY", "OPENAI_API_KEY", "openai"),
        (
            "BCODE_OPENROUTER_API_KEY",
            "OPENROUTER_API_KEY",
            "openrouter",
        ),
        ("BCODE_XAI_API_KEY", "XAI_API_KEY", "xai"),
    ];

    fn field(storage: &str) -> AuthSecretField {
        let AuthMethodContribution::SecretFields { mut fields, .. } =
            crate::api_key_auth_method(storage, "Synthetic key")
        else {
            panic!("expected API-key secret fields");
        };
        assert_eq!(fields.len(), 1);
        let field = fields.remove(0);
        field.validate().unwrap();
        field
    }

    fn selected_source(field: &AuthSecretField, app: &str) -> (usize, String) {
        field
            .discovery_sources
            .iter()
            .enumerate()
            .find_map(|(index, source)| match source {
                AuthCredentialSource::JsonFile {
                    application,
                    relative_path,
                    ..
                }
                | AuthCredentialSource::JsonFileValidated {
                    application,
                    relative_path,
                    ..
                } if application == app => Some((index, relative_path.clone())),
                _ => None,
            })
            .unwrap()
    }

    // Exercise the production field and host acquisition path, never ambient sources.
    fn assert_acquisition(storage: &str, app: &str, fixture: &Value, accepted: bool) {
        let field = field(storage);
        let (index, relative_path) = selected_source(&field, app);
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(relative_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(fixture).unwrap()).unwrap();
        let result = read_selected(home.path(), &field, index);
        if accepted {
            assert_eq!(
                result.unwrap().as_deref().map(String::as_str),
                Some("synthetic-key")
            );
        } else {
            assert!(
                !matches!(result, Ok(Some(_))),
                "unsupported fixture was acquired"
            );
        }
    }

    #[test]
    fn production_declarations_map_providers_and_fixed_roots() {
        assert_eq!(api_key_discovery_sources("unknown"), Vec::new());
        for (storage, env, provider) in PROVIDERS {
            let field = field(storage);
            assert_eq!(field.discovery_sources, api_key_discovery_sources(storage));
            assert_eq!(
                field.discovery_sources[0],
                AuthCredentialSource::Environment {
                    name: env.to_owned()
                }
            );
            assert_eq!(
                field.discovery_sources.len(),
                if provider == "openai" { 4 } else { 3 }
            );
            for (app, path, kind) in [
                ("OpenCode", ".local/share/opencode/auth.json", "api"),
                ("Pi (API key only)", ".pi/agent/auth.json", "api_key"),
            ] {
                let (index, relative_path) = selected_source(&field, app);
                assert_eq!(relative_path, path);
                match &field.discovery_sources[index] {
                    AuthCredentialSource::JsonFile {
                        pointer,
                        discriminator,
                        ..
                    }
                    | AuthCredentialSource::JsonFileValidated {
                        pointer,
                        discriminator,
                        ..
                    } => {
                        assert_eq!(pointer, &format!("/{provider}/key"));
                        assert_eq!(
                            discriminator,
                            &Some((format!("/{provider}/type"), kind.to_owned()))
                        );
                    }
                    AuthCredentialSource::Environment { .. } => unreachable!(),
                }
                assert_acquisition(
                    storage,
                    app,
                    &json!({provider: {"type": kind, "key": "synthetic-key"}}),
                    true,
                );
            }
        }
    }

    #[test]
    fn codex_acquisition_requires_apikey_and_rejects_mixed_subscription_tokens() {
        let storage = "BCODE_OPENAI_API_KEY";
        let app = "Codex (API key only)";
        assert_eq!(selected_source(&field(storage), app).1, ".codex/auth.json");
        assert_acquisition(
            storage,
            app,
            &json!({"auth_mode":"apikey", "OPENAI_API_KEY":"synthetic-key"}),
            true,
        );
        for fixture in [
            json!({"OPENAI_API_KEY":"synthetic-key"}),
            json!({"auth_mode":null,"OPENAI_API_KEY":"synthetic-key"}),
            json!({"auth_mode":"chatgpt","OPENAI_API_KEY":"synthetic-key","tokens":{"access_token":"synthetic-token"}}),
            json!({"auth_mode":"apikey","OPENAI_API_KEY":"synthetic-key","tokens":{"access_token":"synthetic-token"}}),
            json!({"auth_mode":"apikey","OPENAI_API_KEY":"synthetic-key","tokens":{"refresh_token":"synthetic-refresh"}}),
            json!({"auth_mode":"apikey","OPENAI_API_KEY":"synthetic-key","tokens":{}}),
            json!({"auth_mode":"apikey","OPENAI_API_KEY":"synthetic-key","tokens":null}),
            json!({"auth_mode":"apikey","tokens":{"access_token":"synthetic-token"}}),
            json!({"auth_mode":"apikey","OPENAI_API_KEY":"has whitespace"}),
        ] {
            assert_acquisition(storage, app, &fixture, false);
        }
    }

    #[test]
    fn pi_acquisition_accepts_only_unambiguous_literal_keys() {
        for (storage, _, provider) in PROVIDERS {
            let app = "Pi (API key only)";
            for key in [
                "!echo-synthetic",
                "${SYNTHETIC_KEY}",
                "$SYNTHETIC_KEY",
                "SYNTHETIC_KEY",
                "synthetic_key",
                "_synthetic",
                "",
                "has whitespace",
                "synthetic\nkey",
                "synthetic\u{0000}key",
            ] {
                assert_acquisition(
                    storage,
                    app,
                    &json!({provider: {"type":"api_key", "key":key}}),
                    false,
                );
            }
            for env in [
                json!(null),
                json!({}),
                json!("SYNTHETIC_KEY"),
                json!({"NAME":"synthetic-other"}),
            ] {
                assert_acquisition(
                    storage,
                    app,
                    &json!({provider: {"type":"api_key", "key":"synthetic-key", "env":env}}),
                    false,
                );
            }
            for kind in [json!("oauth"), json!("api"), json!(null)] {
                assert_acquisition(
                    storage,
                    app,
                    &json!({provider: {"type":kind, "key":"synthetic-key", "access":"synthetic-token"}}),
                    false,
                );
            }
            assert_acquisition(
                storage,
                app,
                &json!({provider: {"key":"synthetic-key"}}),
                false,
            );
        }
    }

    #[test]
    fn opencode_acquisition_rejects_oauth_even_with_a_key() {
        for (storage, _, provider) in PROVIDERS {
            for fixture in [
                json!({provider: {"type":"oauth", "key":"synthetic-key", "access":"synthetic-token", "refresh":"synthetic-refresh"}}),
                json!({provider: {"type":"oauth", "access":"synthetic-token"}}),
                json!({provider: {"key":"synthetic-key"}}),
                json!({provider: {"type":"api_key", "key":"synthetic-key"}}),
                json!({provider: {"type":null, "key":"synthetic-key"}}),
            ] {
                assert_acquisition(storage, "OpenCode", &fixture, false);
            }
        }
    }

    #[test]
    fn acquisition_rejects_invalid_documents_and_field_values() {
        for (storage, _, provider) in PROVIDERS {
            for (app, kind) in [("OpenCode", "api"), ("Pi (API key only)", "api_key")] {
                for key in [
                    json!(null),
                    json!(42),
                    json!(""),
                    json!("s".repeat(513)),
                    json!("has whitespace"),
                    json!("synthetic\u{0000}key"),
                ] {
                    assert_acquisition(
                        storage,
                        app,
                        &json!({provider: {"type":kind, "key":key}}),
                        false,
                    );
                }
            }
        }
        let field = field("BCODE_OPENAI_API_KEY");
        for app in ["OpenCode", "Pi (API key only)", "Codex (API key only)"] {
            let (index, relative_path) = selected_source(&field, app);
            let home = tempfile::tempdir().unwrap();
            let path = home.path().join(relative_path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            assert!(matches!(
                read_selected(home.path(), &field, index),
                Ok(None)
            ));
            for bytes in [b"not-json".to_vec(), vec![b' '; 1024 * 1024 + 1]] {
                std::fs::write(&path, bytes).unwrap();
                assert!(read_selected(home.path(), &field, index).is_err());
            }
        }
    }
}
