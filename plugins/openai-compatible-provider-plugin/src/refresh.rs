//! Provider-owned refresh exchange bracketed by durable host admission and commit.

use std::collections::BTreeMap;
use std::future::Future;

use bcode_model::{ProviderError, ProviderErrorCategory, ProviderRequestContext};
use bcode_plugin_sdk::{ServiceBridgeRequest, ServiceBridgeResponse, ServiceCancellation};
use bcode_provider_auth_models::{
    AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION, AuthRefreshBeginRequest, AuthRefreshBeginResponse,
    AuthRefreshCommitRequest, OP_BEGIN_REFRESH, OP_COMMIT_REFRESH,
};

use super::{
    AuthSettings, RefreshedChatGptAuth, Settings, cancellable_auth_refresh,
    credential_update_result, openai_oauth_credential_storage, provider_error, unix_timestamp,
};

type CredentialRecord = BTreeMap<String, Option<String>>;

fn reconnect() -> ProviderError {
    provider_error(
        "token_refresh_requires_reconnect",
        ProviderErrorCategory::Auth,
        "ChatGPT credential refresh could not be safely completed; reconnect this auth profile",
    )
}

fn host_request(
    invocation_id: &str,
    operation: &str,
    payload: serde_json::Value,
) -> ServiceBridgeRequest {
    ServiceBridgeRequest::InvokeService(bcode_tool::ToolInvocationServiceRequest {
        invocation_id: invocation_id.to_owned(),
        request_id: format!("{invocation_id}-{operation}"),
        route_id: None,
        interface_id: bcode_provider_auth_models::AUTH_HOST_INTERFACE_ID.to_owned(),
        operation: operation.to_owned(),
        payload,
    })
}

/// Exchange once after admission. Dropping or failing the exchange deliberately leaves the fence.
/// No newly rotated credential is returned to the caller until commit is acknowledged.
fn admit_refresh<H>(
    begin: AuthRefreshBeginRequest,
    invocation_id: &str,
    host: &mut H,
) -> Result<AuthRefreshCommitRequest, ProviderError>
where
    H: FnMut(ServiceBridgeRequest) -> Result<ServiceBridgeResponse, ProviderError>,
{
    begin.validate().map_err(|_| reconnect())?;
    let response = match host(host_request(
        invocation_id,
        OP_BEGIN_REFRESH,
        serde_json::to_value(&begin).map_err(|_| reconnect())?,
    ))? {
        ServiceBridgeResponse::Service(bcode_tool::ToolInvocationServiceResolution::Failed {
            code,
            message,
        }) => {
            return Err(provider_error(code, ProviderErrorCategory::Auth, message));
        }
        response => response,
    };
    let ServiceBridgeResponse::Service(bcode_tool::ToolInvocationServiceResolution::Responded {
        payload,
    }) = response
    else {
        return Err(reconnect());
    };
    let admitted: AuthRefreshBeginResponse =
        serde_json::from_value(payload).map_err(|_| reconnect())?;
    if admitted.schema_version != AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION {
        return Err(reconnect());
    }
    let commit = AuthRefreshCommitRequest {
        schema_version: begin.schema_version,
        provider_id: begin.provider_id,
        profile: begin.profile,
        attempt: admitted.attempt,
        credentials: begin.credentials,
    };
    commit.validate().map_err(|_| reconnect())?;
    Ok(commit)
}

pub async fn refresh<H, E, F>(
    settings: Settings,
    context: &ProviderRequestContext,
    invocation_id: &str,
    cancellation: &ServiceCancellation,
    mut host: H,
    exchange: E,
) -> Result<(Settings, Option<CredentialRecord>), ProviderError>
where
    H: FnMut(ServiceBridgeRequest) -> Result<ServiceBridgeResponse, ProviderError>,
    E: FnOnce(Settings) -> F,
    F: Future<Output = Result<(Settings, Option<RefreshedChatGptAuth>), ProviderError>>,
{
    let AuthSettings::ChatGpt {
        expires_at: Some(expiry),
        refresh_token,
        profile,
        ..
    } = &settings.auth
    else {
        return Ok((settings, None));
    };
    if *expiry > unix_timestamp() + 60 {
        return Ok((settings, None));
    }
    if cancellation.is_cancelled() {
        return Err(provider_error(
            "token_refresh_cancelled",
            ProviderErrorCategory::Cancelled,
            "credential refresh was cancelled",
        ));
    }
    if refresh_token
        .as_deref()
        .is_none_or(|value| value.trim().is_empty())
    {
        return Err(reconnect());
    }
    let auth = context.auth.as_ref().ok_or_else(reconnect)?;
    if auth
        .credentials
        .get("refresh_token")
        .map(|value| value.value.as_str())
        != refresh_token.as_deref()
    {
        return Err(reconnect());
    }
    let profile = profile
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(reconnect)?;
    if auth.profile.as_deref() != Some(profile)
        || auth
            .attributes
            .get("provider")
            .is_some_and(|value| value != "openai")
    {
        return Err(reconnect());
    }
    // Snapshot the six registered subscription-method fields exactly from the invocation,
    // including absent optional values, rather than reconstructing parsed settings.
    let credentials: CredentialRecord = openai_oauth_credential_storage()
        .into_iter()
        .map(|field| {
            let value = auth
                .credentials
                .get(&field.credential_id)
                .map(|value| value.value.clone());
            (field.credential_id, value)
        })
        .collect();
    let begin = AuthRefreshBeginRequest {
        schema_version: AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION,
        provider_id: "openai".to_owned(),
        profile: profile.to_owned(),
        credentials,
    };
    let mut commit = admit_refresh(begin, invocation_id, &mut host)?;
    let (settings, rotated) = cancellable_auth_refresh(cancellation, exchange(settings))
        .await
        .map_err(|error| {
            if error.category == ProviderErrorCategory::Cancelled {
                error
            } else {
                reconnect()
            }
        })?;
    let rotated = rotated.ok_or_else(reconnect)?;
    commit.credentials.extend([
        ("access_token".to_owned(), Some(rotated.access_token)),
        ("refresh_token".to_owned(), Some(rotated.refresh_token)),
        (
            "expires_at".to_owned(),
            Some(rotated.expires_at.to_string()),
        ),
        ("id_token".to_owned(), rotated.id_token),
        ("account_id".to_owned(), rotated.account_id),
    ]);
    commit.validate().map_err(|_| reconnect())?;
    // Do not check cancellation here: a completed rotation must be durably committed first.
    let response = host(host_request(
        invocation_id,
        OP_COMMIT_REFRESH,
        serde_json::to_value(&commit).map_err(|_| reconnect())?,
    ))?;
    credential_update_result(response, &commit.credentials.keys().cloned().collect())?;
    Ok((settings, Some(commit.credentials)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_model::{ProviderAuthContext, ProviderAuthCredential};
    use bcode_model_provider_runtime::ProviderRuntime;
    use std::sync::{Arc, Mutex};

    fn fixture() -> (Settings, ProviderRequestContext) {
        let context = ProviderRequestContext {
            auth: Some(ProviderAuthContext {
                profile: Some("subscription".to_owned()),
                attributes: BTreeMap::from([("provider".to_owned(), "openai".to_owned())]),
                credentials: BTreeMap::from([
                    (
                        "access_token".to_owned(),
                        ProviderAuthCredential {
                            value: "old-access".to_owned(),
                            source: None,
                        },
                    ),
                    (
                        "refresh_token".to_owned(),
                        ProviderAuthCredential {
                            value: "old-refresh".to_owned(),
                            source: None,
                        },
                    ),
                    (
                        "expires_at".to_owned(),
                        ProviderAuthCredential {
                            value: "1".to_owned(),
                            source: None,
                        },
                    ),
                    (
                        "auth_mode".to_owned(),
                        ProviderAuthCredential {
                            value: "chatgpt".to_owned(),
                            source: None,
                        },
                    ),
                ]),
                ..ProviderAuthContext::default()
            }),
            ..ProviderRequestContext::default()
        };
        let mut settings = crate::settings_for_context(&context);
        settings.auth = AuthSettings::ChatGpt {
            access_token: "old-access".to_owned(),
            refresh_token: Some("old-refresh".to_owned()),
            expires_at: Some(1),
            account_id: None,
            profile: Some("subscription".to_owned()),
        };
        (settings, context)
    }

    fn responded(payload: serde_json::Value) -> ServiceBridgeResponse {
        ServiceBridgeResponse::Service(bcode_tool::ToolInvocationServiceResolution::Responded {
            payload,
        })
    }

    fn rotation() -> RefreshedChatGptAuth {
        RefreshedChatGptAuth {
            access_token: "new-access".to_owned(),
            refresh_token: "new-refresh".to_owned(),
            expires_at: 999_999,
            id_token: Some("new-id".to_owned()),
            account_id: Some("new-account".to_owned()),
        }
    }

    #[test]
    fn admission_exchange_commit_order_and_complete_records() {
        let (settings, context) = fixture();
        let events = Arc::new(Mutex::new(Vec::new()));
        let host_events = events.clone();
        let exchange_events = events.clone();
        let runtime = ProviderRuntime::new().expect("runtime");
        let result = runtime.block_on(async move {
            refresh(settings, &context, "invocation", &ServiceCancellation::default(),
                move |request| {
                    let ServiceBridgeRequest::InvokeService(request) = request else { panic!("host service") };
                    host_events.lock().expect("events").push(request.operation.clone());
                    if request.operation == OP_BEGIN_REFRESH {
                        let begin: AuthRefreshBeginRequest = serde_json::from_value(request.payload).expect("begin");
                        assert_eq!(begin.credentials.len(), 6);
                        assert_eq!(begin.credentials["access_token"].as_deref(), Some("old-access"));
                        assert_eq!(begin.credentials["id_token"], None);
                        assert_eq!(begin.credentials["account_id"], None);
                        assert_eq!(begin.credentials["auth_mode"].as_deref(), Some("chatgpt"));
                        Ok(responded(serde_json::json!({"schema_version": AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION, "attempt": "host-attempt"})))
                    } else {
                        assert_eq!(request.operation, OP_COMMIT_REFRESH);
                        let commit: AuthRefreshCommitRequest = serde_json::from_value(request.payload).expect("commit");
                        assert_eq!(commit.attempt, "host-attempt");
                        assert_eq!(commit.credentials.len(), 6);
                        assert_eq!(commit.credentials["access_token"].as_deref(), Some("new-access"));
                        assert_eq!(commit.credentials["auth_mode"].as_deref(), Some("chatgpt"));
                        Ok(responded(serde_json::json!({"schema_version": AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION,
                            "updated_credentials": commit.credentials.keys().collect::<Vec<_>>() })))
                    }
                },
                move |settings| async move {
                    exchange_events.lock().expect("events").push("exchange".to_owned());
                    Ok((settings, Some(rotation())))
                }).await
        }).expect("runtime").expect("committed");
        assert_eq!(
            *events.lock().expect("events"),
            vec![OP_BEGIN_REFRESH, "exchange", OP_COMMIT_REFRESH]
        );
        assert_eq!(
            result.1.expect("record")["id_token"].as_deref(),
            Some("new-id")
        );
    }

    #[test]
    fn refused_or_incompatible_admission_never_exchanges() {
        for payload in [
            serde_json::json!({}),
            serde_json::json!({"schema_version": 0, "attempt": "host-attempt"}),
            serde_json::json!({"schema_version": AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION, "attempt": ""}),
        ] {
            let (settings, context) = fixture();
            let runtime = ProviderRuntime::new().expect("runtime");
            let error = runtime
                .block_on(async move {
                    refresh(
                        settings,
                        &context,
                        "invocation",
                        &ServiceCancellation::default(),
                        |_| Ok(responded(payload.clone())),
                        |_| async { panic!("exchange must not be polled") },
                    )
                    .await
                })
                .expect("runtime")
                .expect_err("admission rejected");
            assert_eq!(error.code, "token_refresh_requires_reconnect");
            assert!(!error.message.contains("old-refresh"));
        }
    }

    #[test]
    fn uncertain_exchange_leaves_fence_without_commit_or_retry() {
        let (settings, context) = fixture();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let host_calls = calls.clone();
        let exchange_calls = calls.clone();
        let runtime = ProviderRuntime::new().expect("runtime");
        let error = runtime.block_on(async move {
            refresh(settings, &context, "invocation", &ServiceCancellation::default(),
                move |request| {
                    let ServiceBridgeRequest::InvokeService(request) = request else { panic!("host service") };
                    host_calls.lock().expect("events").push(request.operation);
                    Ok(responded(serde_json::json!({"schema_version": AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION, "attempt": "host-attempt"})))
                },
                move |_| async move {
                    exchange_calls.lock().expect("events").push("exchange".to_owned());
                    Err(provider_error("remote_uncertain", ProviderErrorCategory::ProviderInternal, "old-refresh"))
                }).await
        }).expect("runtime").expect_err("uncertain");
        assert_eq!(
            *calls.lock().expect("events"),
            vec![OP_BEGIN_REFRESH, "exchange"]
        );
        assert_eq!(error.code, "token_refresh_requires_reconnect");
        assert!(!error.message.contains("old-refresh"));
    }

    #[test]
    fn unacknowledged_commit_never_delivers_rotated_credentials() {
        let (settings, context) = fixture();
        let runtime = ProviderRuntime::new().expect("runtime");
        let error = runtime.block_on(async move {
            refresh(settings, &context, "invocation", &ServiceCancellation::default(),
                |request| {
                    let ServiceBridgeRequest::InvokeService(request) = request else { panic!("host service") };
                    if request.operation == OP_BEGIN_REFRESH {
                        Ok(responded(serde_json::json!({"schema_version": AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION, "attempt": "host-attempt"})))
                    } else {
                        Ok(responded(serde_json::json!({"schema_version": AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION, "updated_credentials": ["access_token"]})))
                    }
                },
                |settings| async { Ok((settings, Some(rotation()))) }).await
        }).expect("runtime").expect_err("commit not acknowledged");
        assert!(!error.message.contains("new-access"));
    }

    #[test]
    fn cancellation_during_exchange_keeps_fence_without_commit() {
        let (settings, context) = fixture();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let host_calls = calls.clone();
        let cancellation = ServiceCancellation::default();
        let exchange_cancellation = cancellation.clone();
        let runtime = ProviderRuntime::new().expect("runtime");
        let error = runtime.block_on(async move {
            refresh(settings, &context, "invocation", &cancellation,
                move |request| {
                    let ServiceBridgeRequest::InvokeService(request) = request else { panic!("host service") };
                    host_calls.lock().expect("events").push(request.operation);
                    Ok(responded(serde_json::json!({"schema_version": AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION, "attempt": "host-attempt"})))
                },
                move |_| async move {
                    exchange_cancellation.cancel();
                    std::future::pending().await
                }).await
        }).expect("runtime").expect_err("cancelled");
        assert_eq!(error.category, ProviderErrorCategory::Cancelled);
        assert_eq!(*calls.lock().expect("events"), vec![OP_BEGIN_REFRESH]);
    }

    #[test]
    fn completed_rotation_commits_even_when_cancellation_arrives() {
        let (settings, context) = fixture();
        let cancellation = ServiceCancellation::default();
        let exchange_cancellation = cancellation.clone();
        let runtime = ProviderRuntime::new().expect("runtime");
        let result = runtime.block_on(async move {
            refresh(settings, &context, "invocation", &cancellation,
                |request| {
                    let ServiceBridgeRequest::InvokeService(request) = request else { panic!("host service") };
                    if request.operation == OP_BEGIN_REFRESH {
                        Ok(responded(serde_json::json!({"schema_version": AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION, "attempt": "host-attempt"})))
                    } else {
                        let commit: AuthRefreshCommitRequest = serde_json::from_value(request.payload).expect("commit");
                        Ok(responded(serde_json::json!({"schema_version": AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION,
                            "updated_credentials": commit.credentials.keys().collect::<Vec<_>>() })))
                    }
                },
                move |settings| async move {
                    exchange_cancellation.cancel();
                    Ok((settings, Some(rotation())))
                }).await
        }).expect("runtime").expect("committed before cancellation is surfaced");
        assert!(result.1.is_some());
    }
}
