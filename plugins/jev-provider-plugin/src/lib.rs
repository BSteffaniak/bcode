#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! `TypeSafe` Jev adapter for Bcode's provider-neutral judgement interface.
//! Credentials and HTTP payloads are private to this plugin; errors never include upstream bodies.

use bcode_model::ProviderRequestContext;
use bcode_model::judgement::{
    self, Answer, Model, ModelList, ProviderRequest, Question, QuestionKind, Response, Usage,
};
use bcode_plugin_sdk::ServiceCancellation;
use bcode_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

const DEFAULT_BASE_URL: &str = "https://jevtypesafeai.com";
const OFFICIAL_BASE_URL: &str = "https://api.typesafe.ai";
const VERIFIED_MODEL_ID: &str = "jev-1.13.0";
const MODEL_ALIAS: &str = "jev-latest";

/// `TypeSafe` Jev judgement provider.
#[derive(Default)]
pub struct JevProviderPlugin;

impl ConcurrentRustPlugin for JevProviderPlugin {
    fn register_auth_providers_concurrent(
        &self,
        registrar: AuthRegistrar,
    ) -> Result<(), PluginError> {
        registrar
            .register(&jev_auth_contribution())
            .map_err(|_| PluginError::failed("failed to register Jev auth"))
    }

    fn invoke_service_concurrent(&self, context: NativeServiceContext) -> ServiceResponse {
        invoke(&context.request, &context.cancellation)
    }
}

impl RustPlugin for JevProviderPlugin {
    fn register_auth_providers(&mut self, registrar: AuthRegistrar) -> Result<(), PluginError> {
        registrar
            .register(&jev_auth_contribution())
            .map_err(|_| PluginError::failed("failed to register Jev auth"))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        invoke(&context.request, &context.cancellation)
    }
}

fn jev_auth_contribution() -> bcode_provider_auth_models::AuthProviderContribution {
    use bcode_provider_auth_models::{
        AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION, AuthCredentialSource, AuthMethodContribution,
        AuthSecretField, AuthSecretValidation,
    };
    let mut contribution = bcode_provider_auth_models::AuthProviderContribution {
        schema_version: AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
        provider_id: "jev".into(),
        display_name: "TypeSafe Jev".into(),
        methods: vec![AuthMethodContribution::SecretFields {
            method_id: "api_key".into(),
            display_name: "API key".into(),
            fields: vec![AuthSecretField {
                credential_id: "api_key".into(),
                storage_key: "BCODE_JEV_API_KEY".into(),
                prompt: "Jev API key".into(),
                optional: false,
                validation: AuthSecretValidation {
                    min_bytes: Some(1),
                    max_bytes: Some(512),
                    required_prefix: None,
                },
                discovery_sources: vec![AuthCredentialSource::Environment {
                    name: "JEV_API_KEY".into(),
                }],
                invocation_env: vec!["BCODE_JEV_API_KEY".into(), "JEV_API_KEY".into()],
            }],
            supports_verification: false,
            supports_revocation: false,
        }],
    };
    let AuthMethodContribution::SecretFields { fields, .. } = &contribution.methods[0] else {
        unreachable!("hosted enrollment uses secret fields");
    };
    let mut official_fields = fields.clone();
    let field = &mut official_fields[0];
    field.credential_id = "typesafe_api_key".into();
    field.storage_key = "BCODE_TYPESAFE_API_KEY".into();
    field.prompt = "Official TypeSafe API key (not a hosted Jev key)".into();
    field.discovery_sources = vec![AuthCredentialSource::Environment {
        name: "TYPESAFE_API_KEY".into(),
    }];
    field.invocation_env = vec!["BCODE_TYPESAFE_API_KEY".into(), "TYPESAFE_API_KEY".into()];
    contribution
        .methods
        .push(AuthMethodContribution::SecretFields {
            method_id: "typesafe_api_key".into(),
            display_name: "Official TypeSafe API key".into(),
            fields: official_fields,
            supports_verification: false,
            supports_revocation: false,
        });
    contribution
}

fn invoke(request: &ServiceRequest, cancellation: &ServiceCancellation) -> ServiceResponse {
    let verification = request.interface_id == judgement::VERIFICATION_INTERFACE_ID;
    if request.interface_id != judgement::INTERFACE_ID && !verification {
        return ServiceResponse::error("unsupported_interface", "unsupported judgement interface");
    }
    if verification && request.operation != judgement::OP_VERIFY {
        return ServiceResponse::error(
            "unsupported_operation",
            "unsupported verification operation",
        );
    }
    if !verification && request.operation == judgement::OP_VERIFY {
        return ServiceResponse::error("unsupported_operation", "unsupported judgement operation");
    }
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return ServiceResponse::error("runtime_unavailable", "provider runtime unavailable");
    };
    match request.operation.as_str() {
        judgement::OP_VERIFY => {
            let Ok(envelope) = request.payload_json::<judgement::VerificationRequest>() else {
                return ServiceResponse::error("invalid_request", "invalid verification request");
            };
            match runtime.block_on(cancel_on_signal(verify(&envelope), cancellation)) {
                Ok(result) => encode(&result),
                Err(error) => ServiceResponse::error("cancelled", error),
            }
        }
        judgement::OP_MODELS => {
            let Ok(context) = request.payload_json::<ProviderRequestContext>() else {
                return ServiceResponse::error(
                    "invalid_request",
                    "invalid model discovery request",
                );
            };
            match runtime.block_on(cancel_on_signal(discover_models(&context), cancellation)) {
                Ok(list) => encode(&list),
                Err(error) => ServiceResponse::error(error, error),
            }
        }
        judgement::OP_JUDGE => {
            let Ok(envelope) = request.payload_json::<ProviderRequest>() else {
                return ServiceResponse::error("invalid_request", "invalid judgement request");
            };
            match runtime.block_on(cancel_on_signal(judge(&envelope), cancellation)) {
                Ok(result) => encode(&result),
                Err(error) => ServiceResponse::error(error, error),
            }
        }
        _ => ServiceResponse::error("unsupported_operation", "unsupported judgement operation"),
    }
}

async fn cancel_on_signal<T>(
    work: impl std::future::Future<Output = Result<T, &'static str>>,
    cancellation: &ServiceCancellation,
) -> Result<T, &'static str> {
    tokio::pin!(work);
    loop {
        if cancellation.is_cancelled() {
            return Err("judgement invocation cancelled");
        }
        tokio::select! {
            result = &mut work => return result,
            () = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }
}

fn encode<T: Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value).unwrap_or_else(|_| {
        ServiceResponse::error("encode_failed", "provider response encoding failed")
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    Hosted,
    Official,
}

fn backend(context: &ProviderRequestContext) -> Result<Backend, &'static str> {
    let selected = context.settings.get("backend").or_else(|| {
        context
            .auth
            .as_ref()
            .and_then(|auth| auth.attributes.get("backend"))
    });
    match selected.map(String::as_str) {
        Some("hosted") => Ok(Backend::Hosted),
        Some("official") => Ok(Backend::Official),
        Some(_) => Err("unsupported provider backend"),
        None => {
            if let Some(scheme) = context
                .auth
                .as_ref()
                .and_then(|auth| auth.scheme.as_deref())
            {
                match scheme {
                    "typesafe_api_key" => return Ok(Backend::Official),
                    "api_key" => return Ok(Backend::Hosted),
                    _ => return Err("unsupported provider authentication method"),
                }
            }
            let has_official = context
                .auth
                .as_ref()
                .is_some_and(|auth| auth.credentials.contains_key("typesafe_api_key"));
            let has_hosted = context
                .auth
                .as_ref()
                .is_some_and(|auth| auth.credentials.contains_key("api_key"));
            if has_official && has_hosted {
                return Err("ambiguous provider backend");
            }
            Ok(if has_official {
                Backend::Official
            } else {
                Backend::Hosted
            })
        }
    }
}

fn credential(context: &ProviderRequestContext) -> Result<&str, &'static str> {
    let id = match backend(context)? {
        Backend::Hosted => "api_key",
        Backend::Official => "typesafe_api_key",
    };
    context
        .auth
        .as_ref()
        .and_then(|auth| auth.credentials.get(id))
        .map(|credential| credential.value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or("provider authentication is unavailable")
}

fn base_url(context: &ProviderRequestContext) -> Result<String, &'static str> {
    let url = context
        .settings
        .get("base_url")
        .or_else(|| {
            context
                .auth
                .as_ref()
                .and_then(|auth| auth.attributes.get("base_url"))
        })
        .map_or(
            match backend(context)? {
                Backend::Hosted => DEFAULT_BASE_URL,
                Backend::Official => OFFICIAL_BASE_URL,
            },
            String::as_str,
        );
    let parsed = reqwest::Url::parse(url).map_err(|_| "invalid provider endpoint")?;
    if parsed.scheme() != "https"
        && !(parsed.scheme() == "http" && parsed.host_str() == Some("127.0.0.1"))
    {
        return Err("invalid provider endpoint");
    }
    if parsed.username() != ""
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("invalid provider endpoint");
    }
    if (parsed.host_str() == Some("api.typesafe.ai") && backend(context)? != Backend::Official)
        || (parsed.host_str() == Some("jevtypesafeai.com") && backend(context)? != Backend::Hosted)
    {
        return Err("provider credential endpoint mismatch");
    }
    Ok(url.trim_end_matches('/').to_owned())
}

fn http_client() -> Result<reqwest::Client, &'static str> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "provider transport unavailable")
}

fn check_status(status: reqwest::StatusCode) -> Result<(), &'static str> {
    if status.is_success() {
        return Ok(());
    }
    Err(match status.as_u16() {
        401 | 403 => "credential_rejected",
        402 => "quota_exhausted",
        429 => "rate_limited",
        408 | 425 | 500..=599 => "temporarily_unavailable",
        _ => "provider_rejected",
    })
}

fn transport_error(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else {
        "temporarily_unavailable"
    }
}

async fn read_bounded(
    mut response: reqwest::Response,
    exceeded: &'static str,
) -> Result<Vec<u8>, &'static str> {
    if response
        .content_length()
        .is_some_and(|size| size > judgement::MAX_RESPONSE_BYTES as u64)
    {
        return Err(exceeded);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "provider response read failed")?
    {
        if chunk.len() > judgement::MAX_RESPONSE_BYTES - bytes.len() {
            return Err(exceeded);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn supported_kinds() -> BTreeSet<QuestionKind> {
    BTreeSet::from([
        QuestionKind::Choice,
        QuestionKind::Score,
        QuestionKind::YesNo,
    ])
}

async fn discover_models(context: &ProviderRequestContext) -> Result<ModelList, &'static str> {
    if base_url(context)? == DEFAULT_BASE_URL {
        // This API currently exposes no authenticated model-list route. Advertise only the
        // alias and concrete model observed on its decision endpoint, not another host's list.
        // Model metadata is public; credential availability is checked independently by
        // the application, and judgement still requires a selected owned credential.
        return Ok(ModelList {
            provider_id: "bcode.jev".into(),
            question_kinds: supported_kinds(),
            models: [MODEL_ALIAS, VERIFIED_MODEL_ID]
                .into_iter()
                .map(|id| Model {
                    model_id: id.into(),
                    question_kinds: supported_kinds(),
                })
                .collect(),
        });
    }
    let key = credential(context)?;
    let path = match backend(context)? {
        Backend::Hosted => "/api/v1/models",
        Backend::Official => "/v1/models",
    };
    let url = format!("{}{path}", base_url(context)?);
    let response = http_client()?
        .get(url)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|error| transport_error(&error))?;
    check_status(response.status())?;
    let bytes = read_bounded(response, "model discovery response exceeds size limit").await?;
    let wire: ModelsWire =
        serde_json::from_slice(&bytes).map_err(|_| "invalid model discovery response")?;
    if wire.models.len() > 4096 {
        return Err("model discovery response exceeds size limit");
    }
    let mut seen = BTreeSet::new();
    let models = wire
        .models
        .into_iter()
        .filter_map(|model| {
            if model.name.is_empty() || !seen.insert(model.name.clone()) {
                return None;
            }
            Some(Model {
                model_id: model.name,
                question_kinds: supported_kinds(),
            })
        })
        .collect();
    Ok(ModelList {
        provider_id: "bcode.jev".into(),
        question_kinds: supported_kinds(),
        models,
    })
}

#[derive(Deserialize)]
struct ModelsWire {
    models: Vec<ModelWire>,
}
#[derive(Deserialize)]
struct ModelWire {
    name: String,
}

async fn verify(
    envelope: &judgement::VerificationRequest,
) -> Result<judgement::VerificationResult, &'static str> {
    use judgement::{AuthenticationStatus as Auth, ModelReadiness as Ready};
    let ctx = &envelope.provider_context;
    let selected = match backend(ctx) {
        Ok(selected) => selected,
        Err(error) => {
            return Ok(verification_failure(
                error,
                judgement::VerificationMethod::AuthenticatedDiscovery,
            ));
        }
    };
    let method = match selected {
        Backend::Hosted => judgement::VerificationMethod::LightweightJudgement,
        Backend::Official => judgement::VerificationMethod::AuthenticatedDiscovery,
    };
    if let Err(error) = credential(ctx).and_then(|_| base_url(ctx)) {
        return Ok(verification_failure(error, method));
    }
    match selected {
        Backend::Official => match discover_models(ctx).await {
            Ok(list) => Ok(judgement::VerificationResult {
                method,
                authentication: Auth::Accepted,
                model: if list
                    .models
                    .iter()
                    .any(|model| model.model_id == envelope.model_id)
                {
                    Ready::NotChecked
                } else {
                    Ready::Unavailable
                },
            }),
            Err(error) => Ok(verification_failure(error, method)),
        },
        Backend::Hosted => {
            if ![MODEL_ALIAS, VERIFIED_MODEL_ID].contains(&envelope.model_id.as_str()) {
                return Ok(judgement::VerificationResult {
                    method,
                    authentication: Auth::Unknown,
                    model: Ready::Unavailable,
                });
            }
            // A single fixed inference probe, authorized by central access. No application
            // state is transmitted; do not retry a potentially billable inference here.
            match judge(&hosted_probe(ctx.clone())).await {
                Ok(_) => Ok(judgement::VerificationResult {
                    method,
                    authentication: Auth::Accepted,
                    model: Ready::Ready,
                }),
                Err(error) => Ok(verification_failure(error, method)),
            }
        }
    }
}

fn verification_failure(
    error: &str,
    method: judgement::VerificationMethod,
) -> judgement::VerificationResult {
    use judgement::{AuthenticationStatus as Auth, ModelReadiness as Ready};
    let (authentication, model) = match error {
        "credential_rejected" => (Auth::Rejected, Ready::Unavailable),
        "quota_exhausted" | "rate_limited" => (Auth::Unknown, Ready::QuotaLimited),
        "temporarily_unavailable" | "timeout" => (Auth::Unknown, Ready::TemporaryFailure),
        _ => (Auth::Unknown, Ready::Unavailable),
    };
    judgement::VerificationResult {
        method,
        authentication,
        model,
    }
}

fn hosted_probe(context: ProviderRequestContext) -> ProviderRequest {
    ProviderRequest {
        provider_context: context,
        judgement: judgement::Request {
            model_id: MODEL_ALIAS.into(),
            state: judgement::State::Text("ok".into()),
            questions: BTreeMap::from([(
                "verification".into(),
                Question::YesNo {
                    instructions: "Is the state ok?".into(),
                },
            )]),
        },
    }
}

async fn judge(envelope: &ProviderRequest) -> Result<Response, &'static str> {
    let key = credential(&envelope.provider_context)?;
    judgement::validate_request(&envelope.judgement)?;
    for question in envelope.judgement.questions.values() {
        if let Question::Score { levels, .. } = question
            && levels.len() > 10
        {
            return Err("score rubric exceeds provider limit");
        }
    }
    let path = match backend(&envelope.provider_context)? {
        Backend::Hosted => "/api/v1/decide",
        Backend::Official => "/v1/systemone",
    };
    let url = format!("{}{path}", base_url(&envelope.provider_context)?);
    let questions = envelope.judgement.questions.iter().map(|(id, question)| {
        let wire = match question {
            Question::Choice { instructions, options } => serde_json::json!({"type":"choice", "instructions":instructions, "criteria":options}),
            Question::Score { instructions, levels } => serde_json::json!({"type":"score", "instructions":instructions, "criteria":levels}),
            Question::YesNo { instructions } => serde_json::json!({"type":"noul", "instructions":instructions}),
        };
        (id.clone(), wire)
    }).collect::<BTreeMap<_, _>>();
    let state = match &envelope.judgement.state {
        judgement::State::Text(text) => serde_json::Value::String(text.clone()),
        judgement::State::Structured(value) => value.clone(),
    };
    let payload = serde_json::json!({"state":state, "model":envelope.judgement.model_id, "questions":questions});
    if serde_json::to_vec(&payload)
        .map_err(|_| "invalid provider request")?
        .len()
        > judgement::MAX_REQUEST_BYTES
    {
        return Err("provider request exceeds size limit");
    }
    let response = http_client()?
        .post(url)
        .bearer_auth(key)
        .json(&payload)
        .send()
        .await
        .map_err(|error| transport_error(&error))?;
    check_status(response.status())?;
    let bytes = read_bounded(response, "judgement provider response exceeds size limit").await?;
    let wire: JudgeWire =
        serde_json::from_slice(&bytes).map_err(|_| "invalid judgement provider response")?;
    if wire.model.is_empty()
        || (wire.model != envelope.judgement.model_id
            && !(envelope.judgement.model_id == MODEL_ALIAS && wire.model == VERIFIED_MODEL_ID))
    {
        return Err("invalid judgement provider model identity");
    }
    let answers = wire
        .answers
        .into_iter()
        .map(|(id, answer)| {
            let question = envelope
                .judgement
                .questions
                .get(&id)
                .ok_or("unexpected judgement answer")?;
            let normalized = normalize_answer(question, answer)?;
            Ok((id, normalized))
        })
        .collect::<Result<BTreeMap<_, _>, &'static str>>()?;
    let result = Response {
        answers,
        usage: wire.usage.map(|usage| Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        }),
    };
    let model = Model {
        model_id: envelope.judgement.model_id.clone(),
        question_kinds: supported_kinds(),
    };
    judgement::validate_response(&envelope.judgement, &model, &result)?;
    Ok(result)
}

#[derive(Deserialize)]
struct JudgeWire {
    model: String,
    answers: BTreeMap<String, AnswerWire>,
    usage: Option<UsageWire>,
}
#[derive(Deserialize)]
struct UsageWire {
    input_tokens: u64,
    output_tokens: u64,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnswerWire {
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Score {
        score: f64,
        legend: BTreeMap<String, String>,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Noul {
        noul: f64,
    },
}

fn normalize_answer(question: &Question, answer: AnswerWire) -> Result<Answer, &'static str> {
    match (question, answer) {
        (
            Question::Choice { .. },
            AnswerWire::Choice {
                choice,
                probabilities,
                confidence,
            },
        ) => Ok(Answer::Choice {
            selected: choice,
            probabilities: Some(probabilities),
            confidence: Some(confidence),
        }),
        (
            Question::Score { levels, .. },
            AnswerWire::Score {
                score,
                legend,
                probabilities,
                confidence,
            },
        ) => {
            let mut ordered = Vec::with_capacity(levels.len());
            for (index, level) in levels.iter().enumerate() {
                let key = index.to_string();
                if legend.get(&key) != Some(level) {
                    return Err("invalid judgement rubric legend");
                }
                ordered.push(
                    *probabilities
                        .get(&key)
                        .ok_or("incomplete judgement probabilities")?,
                );
            }
            if legend.len() != levels.len() || probabilities.len() != levels.len() {
                return Err("invalid judgement rubric length");
            }
            Ok(Answer::Score {
                value: score,
                probabilities: Some(ordered),
                confidence: Some(confidence),
            })
        }
        (Question::YesNo { .. }, AnswerWire::Noul { noul }) => {
            Ok(Answer::YesNo { probability: noul })
        }
        _ => Err("judgement answer type mismatch"),
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_concurrent_plugin_vtable!(
        JevProviderPlugin,
        include_str!("../bcode-plugin.toml")
    )
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_concurrent_plugin!(
    JevProviderPlugin,
    include_str!("../bcode-plugin.toml")
);

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_model::{ProviderAuthContext, ProviderAuthCredential};
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn context(base_url: String) -> ProviderRequestContext {
        ProviderRequestContext {
            settings: BTreeMap::from([("base_url".into(), base_url)]),
            auth: Some(ProviderAuthContext {
                credentials: BTreeMap::from([(
                    "api_key".into(),
                    ProviderAuthCredential {
                        value: "test-only-credential".into(),
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    // Handle only loopback connections; do not print requests (they carry credentials).
    fn fake_http_with_status(
        responses: Vec<(String, String, u16)>,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local fake provider");
        let url = format!("http://{}", listener.local_addr().expect("local address"));
        let handle = std::thread::spawn(move || {
            for (path, body, status) in responses {
                let (mut stream, _) = listener.accept().expect("accept provider call");
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .expect("set timeout");
                let mut input = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let len = stream.read(&mut buffer).expect("read request");
                    assert!(len > 0, "request closed before headers");
                    input.extend_from_slice(&buffer[..len]);
                    if input.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                    assert!(input.len() < judgement::MAX_REQUEST_BYTES);
                }
                let headers = String::from_utf8_lossy(&input);
                assert!(headers.contains(&path));
                assert!(
                    headers
                        .to_ascii_lowercase()
                        .contains("authorization: bearer test-only-credential")
                );
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let header_end = input.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                while input.len() - header_end < content_length {
                    let len = stream.read(&mut buffer).expect("read body");
                    assert!(len > 0);
                    input.extend_from_slice(&buffer[..len]);
                }
                if path.starts_with("POST") {
                    let payload: serde_json::Value =
                        serde_json::from_slice(&input[header_end..header_end + content_length])
                            .expect("request JSON");
                    if payload["questions"].get("verification").is_some() {
                        assert_eq!(
                            payload,
                            serde_json::json!({
                                "state":"ok", "model":MODEL_ALIAS,
                                "questions":{"verification":{"type":"noul","instructions":"Is the state ok?"}}
                            })
                        );
                    } else {
                        assert_eq!(payload["model"], "jev-1");
                        assert_eq!(payload["questions"]["q"]["type"], "noul");
                    }
                }
                let reply = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).expect("write reply");
            }
        });
        (url, handle)
    }

    fn fake_http(responses: Vec<(String, String)>) -> (String, std::thread::JoinHandle<()>) {
        fake_http_with_status(
            responses
                .into_iter()
                .map(|(path, body)| (path, body, 200))
                .collect(),
        )
    }

    #[test]
    fn verification_uses_one_fixed_hosted_probe_and_separates_official_readiness() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (url, handle) = fake_http(vec![(
            "POST /api/v1/decide".into(),
            r#"{"model":"jev-1.13.0","answers":{"verification":{"type":"noul","noul":0.9}}}"#
                .into(),
        )]);
        let result = runtime
            .block_on(verify(&judgement::VerificationRequest {
                model_id: MODEL_ALIAS.into(),
                provider_context: context(url),
            }))
            .unwrap();
        assert!(result.is_ready());
        handle.join().unwrap();
        let (url, handle) = fake_http(vec![(
            "GET /v1/models".into(),
            r#"{"models":[{"name":"jev-latest"}]}"#.into(),
        )]);
        let mut ctx = context(url);
        let auth = ctx.auth.as_mut().unwrap();
        let key = auth.credentials.remove("api_key").unwrap();
        auth.credentials.insert("typesafe_api_key".into(), key);
        let result = runtime
            .block_on(verify(&judgement::VerificationRequest {
                model_id: MODEL_ALIAS.into(),
                provider_context: ctx,
            }))
            .unwrap();
        assert_eq!(
            result.authentication,
            judgement::AuthenticationStatus::Accepted
        );
        assert_eq!(result.model, judgement::ModelReadiness::NotChecked);
        assert!(result.is_ready());
        assert_eq!(
            result.method,
            judgement::VerificationMethod::AuthenticatedDiscovery
        );
        handle.join().unwrap();
    }

    #[test]
    fn verification_failures_preserve_independent_auth_evidence() {
        use judgement::{AuthenticationStatus as Auth, ModelReadiness as Ready};
        for (status, auth, model) in [
            (401, Auth::Rejected, Ready::Unavailable),
            (402, Auth::Unknown, Ready::QuotaLimited),
            (429, Auth::Unknown, Ready::QuotaLimited),
            (503, Auth::Unknown, Ready::TemporaryFailure),
        ] {
            let (url, handle) = fake_http_with_status(vec![(
                "POST /api/v1/decide".into(),
                "test-only-credential private".into(),
                status,
            )]);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = runtime
                .block_on(verify(&judgement::VerificationRequest {
                    model_id: MODEL_ALIAS.into(),
                    provider_context: context(url),
                }))
                .unwrap();
            assert_eq!(
                result,
                judgement::VerificationResult {
                    method: judgement::VerificationMethod::LightweightJudgement,
                    authentication: auth,
                    model
                }
            );
            handle.join().unwrap();
        }
    }

    #[test]
    fn cancelled_verification_does_not_issue_a_probe() {
        let signal = ServiceCancellation::default();
        signal.cancel();
        let request = ServiceRequest {
            interface_id: judgement::VERIFICATION_INTERFACE_ID.into(),
            operation: judgement::OP_VERIFY.into(),
            payload: serde_json::to_vec(&judgement::VerificationRequest {
                model_id: MODEL_ALIAS.into(),
                provider_context: context("http://127.0.0.1:1".into()),
            })
            .unwrap(),
        };
        let response = invoke(&request, &signal);
        assert!(response.error.is_some());
    }

    #[test]
    fn status_failures_are_normalized_without_upstream_content() {
        for (status, expected) in [
            (401, "credential_rejected"),
            (403, "credential_rejected"),
            (402, "quota_exhausted"),
            (429, "rate_limited"),
            (503, "temporarily_unavailable"),
            (529, "temporarily_unavailable"),
            (422, "provider_rejected"),
            (302, "provider_rejected"),
        ] {
            assert_eq!(
                check_status(reqwest::StatusCode::from_u16(status).unwrap()),
                Err(expected)
            );
        }
    }

    #[test]
    fn official_backend_never_uses_hosted_credentials() {
        let mut ctx = context(OFFICIAL_BASE_URL.into());
        assert_eq!(
            base_url(&ctx).unwrap_err(),
            "provider credential endpoint mismatch"
        );
        ctx.settings.insert("backend".into(), "official".into());
        assert!(credential(&ctx).is_err());
        ctx.auth.as_mut().unwrap().credentials.insert(
            "typesafe_api_key".into(),
            ProviderAuthCredential {
                value: "official-test-key".into(),
                ..Default::default()
            },
        );
        assert_eq!(credential(&ctx).unwrap(), "official-test-key");
        ctx.settings
            .insert("base_url".into(), DEFAULT_BASE_URL.into());
        assert_eq!(
            base_url(&ctx).unwrap_err(),
            "provider credential endpoint mismatch"
        );
        ctx.settings.remove("backend");
        assert!(backend(&ctx).is_err());
        ctx.auth.as_mut().unwrap().scheme = Some("typesafe_api_key".into());
        assert_eq!(credential(&ctx).unwrap(), "official-test-key");
    }

    #[test]
    fn official_discovery_and_judgement_use_distinct_routes_and_key() {
        let (url, handle) = fake_http(vec![
            (
                "GET /v1/models".into(),
                r#"{"models":[{"name":"jev-1"}]}"#.into(),
            ),
            (
                "POST /v1/systemone".into(),
                r#"{"model":"jev-1","answers":{"q":{"type":"noul","noul":0.7}}}"#.into(),
            ),
        ]);
        let mut ctx = context(url);
        let auth = ctx.auth.as_mut().unwrap();
        let key = auth.credentials.remove("api_key").unwrap();
        auth.credentials.insert("typesafe_api_key".into(), key);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(
            runtime.block_on(discover_models(&ctx)).unwrap().models[0].model_id,
            "jev-1"
        );
        let envelope = ProviderRequest {
            provider_context: ctx,
            judgement: judgement::Request {
                model_id: "jev-1".into(),
                state: judgement::State::Text("sample".into()),
                questions: BTreeMap::from([(
                    "q".into(),
                    Question::YesNo {
                        instructions: "true?".into(),
                    },
                )]),
            },
        };
        assert!(runtime.block_on(judge(&envelope)).is_ok());
        handle.join().unwrap();
    }

    #[test]
    fn jev_registration_declares_ambient_keys_and_owned_auth_method() {
        let auth = jev_auth_contribution();
        auth.validate().unwrap();
        assert_eq!(auth.provider_id, "jev");
        let bcode_provider_auth_models::AuthMethodContribution::SecretFields { fields, .. } =
            &auth.methods[0]
        else {
            panic!("Jev auth must use a static key");
        };
        assert_eq!(fields[0].credential_id, "api_key");
        assert_eq!(
            fields[0].invocation_env,
            ["BCODE_JEV_API_KEY", "JEV_API_KEY"]
        );
        assert!(fields[0].discovery_sources.iter().any(|source| matches!(source, bcode_provider_auth_models::AuthCredentialSource::Environment { name } if name == "JEV_API_KEY")));
    }

    #[test]
    fn default_endpoint_advertises_only_verified_decision_models() {
        let provider_context = context(DEFAULT_BASE_URL.into());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let list = runtime
            .block_on(discover_models(&provider_context))
            .unwrap();
        assert_eq!(list.provider_id, "bcode.jev");
        assert_eq!(list.models.len(), 2);
        assert_eq!(list.models[0].model_id, MODEL_ALIAS);
        assert_eq!(list.models[1].model_id, VERIFIED_MODEL_ID);
        assert!(
            list.models
                .iter()
                .all(|model| model.question_kinds == supported_kinds())
        );
        let mut unauthenticated = provider_context;
        unauthenticated.auth = None;
        assert!(runtime.block_on(discover_models(&unauthenticated)).is_ok());
        assert!(credential(&unauthenticated).is_err());
    }

    #[test]
    fn authenticated_discovery_and_judgement_over_http() {
        let (url, handle) = fake_http(vec![
            ("GET /api/v1/models".into(), r#"{"models":[{"name":"jev-1"}]}"#.into()),
            ("POST /api/v1/decide".into(), r#"{"model":"jev-1","answers":{"q":{"type":"noul","noul":0.7}},"usage":{"input_tokens":11,"output_tokens":3}}"#.into()),
        ]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let provider_context = context(url);
        let listing = runtime
            .block_on(discover_models(&provider_context))
            .expect("discover");
        assert_eq!(listing.models[0].model_id, "jev-1");
        let envelope = ProviderRequest {
            judgement: judgement::Request {
                model_id: "jev-1".into(),
                state: judgement::State::Text("sample".into()),
                questions: BTreeMap::from([(
                    "q".into(),
                    Question::YesNo {
                        instructions: "true?".into(),
                    },
                )]),
            },
            provider_context,
        };
        let response = runtime.block_on(judge(&envelope)).expect("judge");
        assert!(matches!(
            response.answers["q"],
            Answer::YesNo { probability: 0.7 }
        ));
        assert_eq!(response.usage.unwrap().input_tokens, 11);
        handle.join().expect("fake server finished");
    }

    #[test]
    fn upstream_auth_failure_does_not_expose_body_or_secret() {
        let (url, handle) = fake_http_with_status(vec![(
            "GET /api/v1/models".into(),
            "test-only-credential private provider error".into(),
            401,
        )]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(
            runtime
                .block_on(discover_models(&context(url)))
                .unwrap_err(),
            "credential_rejected"
        );
        handle.join().expect("fake server finished");
    }

    #[test]
    fn oversized_http_discovery_is_rejected() {
        let (url, handle) = fake_http(vec![(
            "GET /api/v1/models".into(),
            "x".repeat(judgement::MAX_RESPONSE_BYTES + 1),
        )]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(discover_models(&context(url)));
        assert_eq!(
            result.unwrap_err(),
            "model discovery response exceeds size limit"
        );
        handle.join().expect("fake server finished");
    }

    #[test]
    fn malformed_and_failed_judgements_fail_closed_without_exposing_upstream_body() {
        for (body, status, expected) in [
            (
                "test-only-credential upstream secret",
                422,
                "provider_rejected",
            ),
            (
                r#"{"model":"jev-1","answers":{"unexpected":{"type":"noul","noul":0.7}}}"#,
                200,
                "unexpected judgement answer",
            ),
            (
                r#"{"model":"jev-1","answers":{"q":{"type":"noul","noul":1.5}}}"#,
                200,
                "invalid judgement answer",
            ),
        ] {
            let (url, handle) =
                fake_http_with_status(vec![("POST /api/v1/decide".into(), body.into(), status)]);
            let envelope = ProviderRequest {
                judgement: judgement::Request {
                    model_id: "jev-1".into(),
                    state: judgement::State::Text("sample".into()),
                    questions: BTreeMap::from([(
                        "q".into(),
                        Question::YesNo {
                            instructions: "true?".into(),
                        },
                    )]),
                },
                provider_context: context(url),
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            assert_eq!(runtime.block_on(judge(&envelope)).unwrap_err(), expected);
            handle.join().expect("fake server finished");
        }
    }

    #[test]
    fn endpoint_uses_owned_profile_setting_without_changing_chat_selection() {
        let mut provider_context = context("http://127.0.0.1:12345".into());
        provider_context.settings.clear();
        provider_context
            .auth
            .as_mut()
            .unwrap()
            .attributes
            .insert("base_url".into(), "http://127.0.0.1:12345".into());
        assert_eq!(
            base_url(&provider_context).unwrap(),
            "http://127.0.0.1:12345"
        );
        provider_context
            .settings
            .insert("base_url".into(), "http://127.0.0.1:23456".into());
        assert_eq!(
            base_url(&provider_context).unwrap(),
            "http://127.0.0.1:23456"
        );
    }

    #[test]
    fn cancellation_interrupts_in_flight_http_response() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local fake provider");
        let url = format!("http://{}", listener.local_addr().expect("local address"));
        let (request_seen, request_received) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept discovery");
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("read timeout");
            let mut buffer = [0_u8; 4096];
            let mut input = Vec::new();
            while !input.windows(4).any(|window| window == b"\r\n\r\n") {
                let len = stream.read(&mut buffer).expect("read headers");
                assert!(len > 0);
                input.extend_from_slice(&buffer[..len]);
                assert!(input.len() < judgement::MAX_REQUEST_BYTES);
            }
            assert!(String::from_utf8_lossy(&input).starts_with("GET /api/v1/models"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
                .expect("write partial reply");
            request_seen.send(()).expect("notify pending response");
            let len = stream
                .read(&mut buffer)
                .expect("client closes cancelled request");
            assert_eq!(len, 0, "cancelled HTTP request must close its socket");
        });
        let cancellation = ServiceCancellation::default();
        let cancel = cancellation.clone();
        let canceller = std::thread::spawn(move || {
            request_received
                .recv_timeout(Duration::from_secs(2))
                .expect("provider request arrived");
            cancel.cancel();
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(2),
                cancel_on_signal(discover_models(&context(url)), &cancellation),
            )
            .await
        });
        assert_eq!(
            result.expect("request cancelled before provider timeout"),
            Err("judgement invocation cancelled")
        );
        canceller.join().expect("cancellation thread");
        drop(runtime);
        server.join().expect("HTTP connection closed");
    }

    #[test]
    fn cancelled_invocation_does_not_wait_for_pending_provider_work() {
        let cancellation = ServiceCancellation::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let pending = async { std::future::pending::<Result<(), &'static str>>().await };
        let token = cancellation.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            token.cancel();
        });
        let result = runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(2),
                cancel_on_signal(pending, &cancellation),
            )
            .await
        });
        assert_eq!(
            result.expect("cancellation completed"),
            Err("judgement invocation cancelled")
        );
        canceller.join().unwrap();
    }

    #[test]
    fn rejects_mismatched_answer_type_or_rubric() {
        let question = Question::Score {
            instructions: "rate".into(),
            levels: vec!["low".into(), "high".into()],
        };
        let wrong_legend = AnswerWire::Score {
            score: 0.2,
            legend: BTreeMap::from([("0".into(), "low".into()), ("1".into(), "wrong".into())]),
            probabilities: BTreeMap::from([("0".into(), 0.8), ("1".into(), 0.2)]),
            confidence: 0.8,
        };
        assert!(normalize_answer(&question, wrong_legend).is_err());
        assert!(normalize_answer(&question, AnswerWire::Noul { noul: 0.2 }).is_err());
    }

    #[test]
    fn converts_all_vendor_answer_types_without_exposing_wire_shapes() {
        let choice = Question::Choice {
            instructions: "pick".into(),
            options: BTreeMap::from([("a".into(), "A".into()), ("b".into(), "B".into())]),
        };
        let answer = AnswerWire::Choice {
            choice: "a".into(),
            probabilities: BTreeMap::from([("a".into(), 0.75), ("b".into(), 0.25)]),
            confidence: 0.75,
        };
        assert!(
            matches!(normalize_answer(&choice, answer), Ok(Answer::Choice { selected, .. }) if selected == "a")
        );
        let score = Question::Score {
            instructions: "rate".into(),
            levels: vec!["low".into(), "high".into()],
        };
        let answer = AnswerWire::Score {
            score: 0.3,
            legend: BTreeMap::from([("0".into(), "low".into()), ("1".into(), "high".into())]),
            probabilities: BTreeMap::from([("0".into(), 0.7), ("1".into(), 0.3)]),
            confidence: 0.7,
        };
        assert!(matches!(
            normalize_answer(&score, answer),
            Ok(Answer::Score { value: 0.3, .. })
        ));
        assert!(matches!(
            normalize_answer(
                &Question::YesNo {
                    instructions: "true?".into()
                },
                AnswerWire::Noul { noul: 0.8 }
            ),
            Ok(Answer::YesNo { probability: 0.8 })
        ));
    }
}
