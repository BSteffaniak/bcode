//! Optional `bcode.harness.terminal-auth/v1` preparation: a namespace-bound plan for the
//! client to run `agy acp --login` in its own terminal. Nothing is launched in the daemon, and a
//! prepared plan is neither readiness nor permission; the host reinspects status afterwards.
use crate::config::Config;
use crate::transport::AcpError;
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
/// * Rejects other plugins, providers or custody, relative paths, mismatched namespaces and an
///   unusable version.
pub fn login_plan(
    instance: &HarnessInstance,
    expected_version: &str,
    environment: impl IntoIterator<Item = (String, String)>,
) -> Result<Value, AcpError> {
    instance.validate().map_err(|_| AcpError::Configuration)?;
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
        return Err(AcpError::Configuration);
    }
    let env: BTreeMap<String, String> = environment
        .into_iter()
        .filter(|(key, value)| {
            ALLOWED_ENVIRONMENT.contains(&key.as_str())
                && value.len() <= 16 * 1024
                && !value.contains('\0')
        })
        .collect();
    Ok(json!({
        "schema_version": 1,
        "namespace": {
            "plugin_id": PLUGIN_ID,
            "credential_namespace": instance.auth.credential_namespace
        },
        "method": {
            "method_id": NATIVE_LOGIN_METHOD,
            "support": "supported",
            "custody": "native_managed",
            "execution": "client_terminal",
            "native_inactivity": "unknown"
        },
        "executable": instance.executable,
        "expected_version": expected_version,
        "working_directory": instance.config_namespace,
        "arguments": [
            "acp",
            "--login",
            "--disable-telemetry",
            "--gemini-dir",
            instance.config_namespace,
        ],
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
        "schema_version": 1,
        "request_id": request.request_id,
        "handoff": handoff,
    }))
    .unwrap_or_else(|_| ServiceResponse::error("encode_failed", "Login plan encoding failed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_harness_models::HarnessAuthBinding;

    #[test]
    fn login_plan_uses_acp_login_and_strips_ambient_secrets() {
        let instance = HarnessInstance {
            instance_id: "antigravity".into(),
            plugin_id: PLUGIN_ID.into(),
            executable: "/usr/local/bin/agy".into(),
            config_namespace: "/home/user/.gemini".into(),
            auth: HarnessAuthBinding {
                provider_id: AUTH_PROVIDER_ID.into(),
                profile: "native".into(),
                credential_namespace: "/home/user/.gemini".into(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        };
        let plan = login_plan(
            &instance,
            "1.5.15",
            [
                ("HOME".into(), "/home/user".into()),
                ("GEMINI_API_KEY".into(), "secret".into()),
                ("GOOGLE_API_KEY".into(), "secret2".into()),
            ],
        )
        .unwrap();
        assert_eq!(
            plan["arguments"],
            json!([
                "acp",
                "--login",
                "--disable-telemetry",
                "--gemini-dir",
                "/home/user/.gemini"
            ])
        );
        assert_eq!(plan["environment"]["HOME"], "/home/user");
        assert!(plan["environment"].get("GEMINI_API_KEY").is_none());
        assert!(plan["environment"].get("GOOGLE_API_KEY").is_none());
    }
}
