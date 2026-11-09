//! Optional `bcode.harness.terminal-auth/v1` preparation: a namespace-bound plan for the
//! client to run `codex login` in its own terminal. Nothing is launched in the daemon, and a
//! prepared plan is neither readiness nor permission; the host reinspects status afterwards.
use crate::config::Config;
use crate::transport::CodexError;
use crate::{AUTH_PROVIDER_ID, PLUGIN_ID};
use bcode_harness_models::{HarnessAuthCustody, HarnessInstance, MAX_HARNESS_ID_BYTES};
use bcode_plugin_sdk::prelude::{NativeServiceContext, ServiceResponse};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

/// Auth method id for the client-terminal native login.
pub const NATIVE_LOGIN_METHOD: &str = "native-login";
/// Service operation name shared by every terminal-auth producer.
const PREPARE_LOGIN_OPERATION: &str = "prepare_login";
/// Environment the client terminal may pass to native login; never credentials.
const ALLOWED_ENVIRONMENT: [&str; 7] = ["HOME", "USER", "PATH", "TMPDIR", "LANG", "LC_ALL", "TERM"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareLoginRequest {
    schema_version: u16,
    request_id: String,
    instance: HarnessInstance,
}

/// Build the client-terminal login plan for an exact native-custody instance.
///
/// # Errors
/// Rejects other plugins, providers or custody, relative paths, mismatched namespaces and an
/// unusable version.
pub fn login_plan(
    instance: &HarnessInstance,
    expected_version: &str,
    environment: impl IntoIterator<Item = (String, String)>,
) -> Result<Value, CodexError> {
    instance.validate().map_err(|_| CodexError::Configuration)?;
    if instance.plugin_id != PLUGIN_ID
        || instance.auth.provider_id != AUTH_PROVIDER_ID
        || instance.auth.custody != HarnessAuthCustody::NativeManaged
        || instance.auth.credential_namespace != instance.config_namespace
        || !Path::new(&instance.executable).is_absolute()
        || !Path::new(&instance.config_namespace).is_absolute()
        || expected_version.is_empty()
        || expected_version.len() > MAX_HARNESS_ID_BYTES
        || expected_version.chars().any(char::is_control)
    {
        return Err(CodexError::Configuration);
    }
    let mut env: BTreeMap<String, String> = environment
        .into_iter()
        .filter(|(key, value)| {
            ALLOWED_ENVIRONMENT.contains(&key.as_str())
                && value.len() <= 16 * 1024
                && !value.contains('\0')
        })
        .collect();
    env.insert("CODEX_HOME".into(), instance.config_namespace.clone());
    Ok(json!({
        "schema_version": 1,
        "namespace": {"plugin_id": PLUGIN_ID, "credential_namespace": instance.auth.credential_namespace},
        "method": {"method_id": NATIVE_LOGIN_METHOD, "support": "supported", "custody": "native_managed",
            "execution": "client_terminal", "native_inactivity": "unknown"},
        "executable": instance.executable,
        "expected_version": expected_version,
        "working_directory": instance.config_namespace,
        "arguments": ["login"],
        "environment": env,
    }))
}

/// Serve `prepare_login` for the generic terminal-auth interface.
pub fn prepare_login_service(context: &NativeServiceContext) -> ServiceResponse {
    if context.request.operation != PREPARE_LOGIN_OPERATION
        || context.request.payload.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES
    {
        return ServiceResponse::error("invalid_request", "Unsupported or oversized login request");
    }
    let Ok(request) = context.request.payload_json::<PrepareLoginRequest>() else {
        return ServiceResponse::error("invalid_request", "Invalid login preparation request");
    };
    if request.schema_version != 1
        || request.request_id.is_empty()
        || request.request_id.len() > MAX_HARNESS_ID_BYTES
        || request.request_id.trim() != request.request_id
        || request.request_id.chars().any(char::is_control)
    {
        return ServiceResponse::error("invalid_request", "Invalid login request identity");
    }
    let plan = Config::from_context(context)
        .and_then(|config| {
            config.validate_instance(&request.instance)?;
            // The configured exact version, or a fresh top-level probe of the same executable.
            match config.instances.get(&request.instance.instance_id) {
                Some(selected) => Ok(selected.exact_version.clone()),
                None => crate::transport::probe_version(&crate::launch(
                    &request.instance,
                    &request.instance.config_namespace,
                )?),
            }
        })
        .and_then(|version| {
            login_plan(
                &request.instance,
                &version,
                ALLOWED_ENVIRONMENT.iter().filter_map(|key| {
                    std::env::var(key)
                        .ok()
                        .map(|value| ((*key).to_owned(), value))
                }),
            )
        });
    let Ok(handoff) = plan else {
        return ServiceResponse::error(
            "policy",
            "Native login selection unsupported or mismatched",
        );
    };
    ServiceResponse::json(&json!({
        "schema_version": 1, "request_id": request.request_id, "handoff": handoff,
    }))
    .unwrap_or_else(|_| ServiceResponse::error("invalid_response", "Login plan encoding failed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_harness_models::HarnessAuthBinding;

    fn instance(custody: HarnessAuthCustody) -> HarnessInstance {
        HarnessInstance {
            instance_id: "codex".into(),
            plugin_id: PLUGIN_ID.into(),
            executable: "/bin/codex".into(),
            config_namespace: "/home/user/.codex".into(),
            auth: HarnessAuthBinding {
                provider_id: AUTH_PROVIDER_ID.into(),
                profile: "native".into(),
                credential_namespace: "/home/user/.codex".into(),
                custody,
            },
        }
    }

    #[test]
    fn plan_runs_native_login_in_the_exact_namespace_without_credentials() {
        let plan = login_plan(
            &instance(HarnessAuthCustody::NativeManaged),
            "0.164.0",
            [
                ("HOME".to_owned(), "/home/user".to_owned()),
                ("OPENAI_API_KEY".to_owned(), "secret".to_owned()),
            ],
        )
        .unwrap();
        assert_eq!(plan["arguments"], json!(["login"]));
        assert_eq!(plan["environment"]["CODEX_HOME"], "/home/user/.codex");
        assert_eq!(plan["environment"]["HOME"], "/home/user");
        assert!(plan["environment"].get("OPENAI_API_KEY").is_none());
        assert_eq!(plan["method"]["execution"], "client_terminal");
        assert_eq!(
            plan["namespace"]["credential_namespace"],
            "/home/user/.codex"
        );
        assert_eq!(
            login_plan(&instance(HarnessAuthCustody::VaultManaged), "0.164.0", []),
            Err(CodexError::Configuration)
        );
        assert_eq!(
            login_plan(&instance(HarnessAuthCustody::NativeManaged), "", []),
            Err(CodexError::Configuration)
        );
    }
}
