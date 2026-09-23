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
    bcode_provider_auth_models::AuthProviderContribution {
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
    }
}

fn invoke(request: &ServiceRequest, cancellation: &ServiceCancellation) -> ServiceResponse {
    if request.interface_id != judgement::INTERFACE_ID {
        return ServiceResponse::error("unsupported_interface", "unsupported judgement interface");
    }
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return ServiceResponse::error("runtime_unavailable", "provider runtime unavailable");
    };
    match request.operation.as_str() {
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

fn credential(context: &ProviderRequestContext) -> Result<&str, &'static str> {
    context
        .auth
        .as_ref()
        .and_then(|auth| auth.credentials.get("api_key"))
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
        .map_or(DEFAULT_BASE_URL, String::as_str);
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
    Ok(url.trim_end_matches('/').to_owned())
}

fn http_client() -> Result<reqwest::Client, &'static str> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| "provider transport unavailable")
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
        credential(context)?;
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
    let url = format!("{}/api/v1/models", base_url(context)?);
    let response = http_client()?
        .get(url)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|_| "model discovery failed")?;
    if !response.status().is_success() {
        return Err("model discovery failed");
    }
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
    let url = format!("{}/api/v1/decide", base_url(&envelope.provider_context)?);
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
        .map_err(|_| "judgement provider request failed")?;
    if !response.status().is_success() {
        return Err("judgement provider request failed");
    }
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
                    assert_eq!(payload["model"], "jev-1");
                    assert_eq!(payload["questions"]["q"]["type"], "noul");
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
        assert!(runtime.block_on(discover_models(&unauthenticated)).is_err());
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
            "model discovery failed"
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
                "judgement provider request failed",
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
