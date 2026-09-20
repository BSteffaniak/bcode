#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Explicit, read-only probe of Codex credential authorization for `ChatGPT` history.
//! Does not refresh tokens, persist responses, or follow redirects.
//! `PROFILE --preview` explicitly displays bounded conversation text.

use bcode_provider_auth::{ProviderRequestContextResolution, try_resolve_provider_request_context};
use serde_json::{Value, json};
use std::time::Duration;

async fn probe(
    client: &reqwest::Client,
    token: &str,
    account: Option<&str>,
    path: &str,
    selected_id: &mut Option<String>,
    expected_id: &str,
) -> Value {
    let mut request = client
        .get(format!("https://chatgpt.com/backend-api/{path}"))
        .bearer_auth(token)
        .header("Accept", "application/json")
        .header("Origin", "https://chatgpt.com")
        .header("User-Agent", "bcode-history-feasibility/1");
    if let Some(account) = account {
        request = request.header("ChatGPT-Account-Id", account);
    }
    let Ok(mut response) = request.send().await else {
        return json!({"outcome": "transport_error"});
    };
    let status = response.status().as_u16();
    let json_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    let challenge = response
        .headers()
        .get("cf-mitigated")
        .is_some_and(|value| value == "challenge");
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if body.len() + chunk.len() <= 4 * 1024 * 1024 => {
                body.extend_from_slice(&chunk);
            }
            Ok(Some(_)) => return json!({"status": status, "outcome": "body_limit"}),
            Ok(None) => break,
            Err(_) => return json!({"status": status, "outcome": "body_read_error"}),
        }
    }
    let parsed = serde_json::from_slice::<Value>(&body).ok();
    if expected_id == "--preview" {
        return if status == 200 {
            parsed.unwrap_or_else(|| json!({"outcome": "invalid_json"}))
        } else {
            json!({"status": status})
        };
    }
    if let Some(id) = parsed
        .as_ref()
        .and_then(|v| v.get("items"))
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        *selected_id = Some(id.to_owned());
    }
    let requested_id = path.strip_prefix("conversation/");
    json!({
        "conversation_id_matches": requested_id.map(|id| parsed.as_ref().is_some_and(|v| {
            ["conversation_id", "id"].iter().any(|key| v.get(key).and_then(Value::as_str) == Some(id))
        })),
        "listed_target_match": parsed.as_ref().and_then(|v| v.get("items")).and_then(Value::as_array).map(|items| items.iter().any(|item| item.get("id").and_then(Value::as_str).is_some_and(|id| id.strip_prefix("WEB:").unwrap_or(id) == expected_id.strip_prefix("WEB:").unwrap_or(expected_id)))),
        "message_mapping_present": parsed.as_ref().is_some_and(|v| v.get("mapping").is_some_and(Value::is_object)),
        "status": status,
        "json_content_type": json_type,
        "valid_json": parsed.is_some(),
        "browser_challenge_header": challenge,
        "history_items": parsed.as_ref().and_then(|v| v.get("items")).and_then(Value::as_array).map(Vec::len),
        "codex_rate_limit_present": parsed.as_ref().is_some_and(|v| v.get("rate_limit").is_some()),
    })
}

async fn run() -> Result<Value, &'static str> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 2
        || args[0].is_empty()
        || (args[1] != "--preview" && !valid_conversation_id(&args[1]))
    {
        return Err("expected_profile_and_conversation_id");
    }
    let profile = &args[0];
    let conversation_id = &args[1];
    let config = bcode_config::load_config().map_err(|_| "configuration_failed")?;
    let mut selection = config.resolved_model_selection();
    selection.provider_plugin_id = Some("bcode.openai-compatible".to_owned());
    selection.auth_profile = Some(profile.clone());
    selection.auth_pool = None;
    let context = try_resolve_provider_request_context(ProviderRequestContextResolution {
        config: &config,
        selection,
    })
    .map_err(|_| "auth_resolution_failed")?;
    let auth = context.auth.as_ref().ok_or("missing_auth")?;
    if auth.profile.as_deref() != Some(profile.as_str()) {
        return Err("resolved_profile_mismatch");
    }
    if auth.scheme.as_deref() != Some("chatgpt") || auth.backend.as_deref() != Some("sshenv") {
        return Err("expected_sshenv_chatgpt_auth");
    }
    let token = &auth
        .credentials
        .get("access_token")
        .ok_or("missing_access_token")?
        .value;
    let account = auth.credentials.get("account_id").map(|v| v.value.as_str());
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| "client_failed")?;
    let mut selected_id = None;
    let control = probe(&client, token, account, "wham/usage", &mut selected_id, "").await;
    if control.get("status").and_then(Value::as_u64) != Some(200) {
        return Ok(json!({"control": control, "history": "not_attempted_control_failed"}));
    }
    let history = probe(
        &client,
        token,
        account,
        "conversations?offset=0&limit=20&order=updated",
        &mut selected_id,
        conversation_id,
    )
    .await;
    if conversation_id == "--preview" {
        let mut previews = Vec::new();
        if let Some(items) = history.get("items").and_then(Value::as_array) {
            for item in items.iter().take(20) {
                let Some(id) = item
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| valid_conversation_id(id))
                else {
                    continue;
                };
                let detail = probe(
                    &client,
                    token,
                    account,
                    &format!("conversation/{id}"),
                    &mut selected_id,
                    "--preview",
                )
                .await;
                previews.push(conversation_preview(id, &detail));
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
        return Ok(json!({"profile": profile, "previews": previews}));
    }
    let detail = probe(
        &client,
        token,
        account,
        &format!("conversation/{conversation_id}"),
        &mut selected_id,
        conversation_id,
    )
    .await;
    Ok(
        json!({"profile": profile, "control": control, "history": history, "detail": detail, "refresh_attempted": false}),
    )
}

fn short_text(text: &str) -> String {
    text.chars().take(240).collect()
}

fn conversation_preview(id: &str, detail: &Value) -> Value {
    let Some(mapping) = detail.get("mapping").and_then(Value::as_object) else {
        return json!({"id": id, "outcome": "no_mapping", "status": detail.get("status")});
    };
    let mut messages = mapping
        .values()
        .filter_map(|node| node.get("message"))
        .filter(|message| {
            matches!(
                message.pointer("/author/role").and_then(Value::as_str),
                Some("user" | "assistant")
            )
        })
        .collect::<Vec<_>>();
    messages.sort_by(|a, b| {
        a.get("create_time")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            .total_cmp(&b.get("create_time").and_then(Value::as_f64).unwrap_or(0.0))
    });
    let excerpts = messages
        .iter()
        .filter_map(|message| {
            let parts = message.pointer("/content/parts")?.as_array()?;
            let text = parts.iter().find_map(Value::as_str)?;
            Some(json!({"role": message.pointer("/author/role"), "text": short_text(text)}))
        })
        .take(2)
        .collect::<Vec<_>>();
    json!({"id": id, "title": detail.get("title").and_then(Value::as_str).map(short_text), "message_count": messages.len(), "earliest_excerpts": excerpts})
}

fn valid_conversation_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b':'))
}

#[cfg(test)]
mod tests {
    use super::{conversation_preview, valid_conversation_id};

    #[test]
    fn previews_only_bounded_message_text() {
        let detail = serde_json::json!({"title": "Test", "mapping": {
            "a": {"message": {"author": {"role": "system"}, "content": {"parts": ["hidden"]}}},
            "b": {"message": {"author": {"role": "user"}, "create_time": 1, "content": {"parts": ["x".repeat(500)]}}}
        }, "private_metadata": "not displayed"});
        let preview = conversation_preview("id", &detail);
        let excerpts = preview["earliest_excerpts"].as_array().unwrap();
        assert_eq!(excerpts.len(), 1);
        assert_eq!(excerpts[0]["text"].as_str().unwrap().len(), 240);
        assert!(preview.get("private_metadata").is_none());
        assert_eq!(
            conversation_preview("id", &serde_json::json!({"status":404}))["outcome"],
            "no_mapping"
        );
    }

    #[test]
    fn accepts_web_ids_but_rejects_url_injection() {
        assert!(valid_conversation_id(
            "WEB:a14ac0a8-15e5-4730-bebf-9d168d860c76"
        ));
        assert!(valid_conversation_id(
            "a14ac0a8-15e5-4730-bebf-9d168d860c76"
        ));
        for invalid in [
            "",
            "../usage",
            "id?other=1",
            "id#fragment",
            "id/child",
            "id%2fchild",
        ] {
            assert!(!valid_conversation_id(invalid));
        }
        assert!(!valid_conversation_id(&"a".repeat(129)));
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    match run().await {
        Ok(report) => println!("{report}"),
        Err(code) => {
            println!("{}", json!({"error": code}));
            std::process::exit(1);
        }
    }
}
