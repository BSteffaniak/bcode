//! Optional `bcode.harness.models/v1` service: native `model/list` discovery and exact
//! selected open. Advertised models are not entitlement or proof of successful inference.
use crate::transport::{AppServer, CodexError};
use crate::{PLUGIN_ID, PluginState, config::Config, failure, launch};
use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessModel, HarnessModelEvidence, HarnessModelsRequest,
    HarnessModelsRequestKind, HarnessModelsResponse, HarnessModelsResponseKind,
    HarnessServiceResponseKind, HarnessSessionTarget, MAX_HARNESS_ID_BYTES, MAX_HARNESS_MODELS,
    MAX_HARNESS_TEXT_BYTES, MAX_HARNESS_WIRE_BYTES,
};
use bcode_plugin_sdk::prelude::{NativeServiceContext, ServiceResponse};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// Bounded pages read from native `model/list`.
const MAX_MODEL_PAGES: usize = 4;

/// Map one native `model/list` page into portable metadata, skipping hidden or malformed
/// entries. The native `model` slug is the key accepted by `thread/start` and settings.
pub fn models_from_page(page: &Value, models: &mut Vec<HarnessModel>) -> Result<(), CodexError> {
    let data = page["data"].as_array().ok_or(CodexError::Protocol)?;
    let mut seen: BTreeSet<String> = models.iter().map(|model| model.model_id.clone()).collect();
    for entry in data {
        if entry["hidden"] == true {
            continue;
        }
        let Some(model_id) = entry["model"]
            .as_str()
            .or_else(|| entry["id"].as_str())
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= MAX_HARNESS_ID_BYTES
                    && id.trim() == *id
                    && !id.chars().any(char::is_control)
            })
        else {
            continue;
        };
        if models.len() >= MAX_HARNESS_MODELS || !seen.insert(model_id.to_owned()) {
            continue;
        }
        let display_name = entry["displayName"]
            .as_str()
            .filter(|name| !name.trim().is_empty() && name.len() <= MAX_HARNESS_TEXT_BYTES)
            .unwrap_or(model_id);
        let description = entry["description"]
            .as_str()
            .filter(|text| !text.is_empty() && text.len() <= MAX_HARNESS_TEXT_BYTES)
            .map(|text| {
                if entry["isDefault"] == true {
                    format!("{text} (Codex default)")
                } else {
                    text.to_owned()
                }
            });
        let model = HarnessModel {
            model_id: model_id.to_owned(),
            display_name: display_name.to_owned(),
            description,
        };
        if model.validate().is_ok() {
            models.push(model);
        }
    }
    Ok(())
}

impl PluginState {
    pub(crate) fn models_service(&mut self, context: &NativeServiceContext) -> ServiceResponse {
        if context.request.payload.len() > MAX_HARNESS_WIRE_BYTES {
            return ServiceResponse::error("invalid_request", "Model request exceeds bound");
        }
        let Ok(request) = serde_json::from_slice::<HarnessModelsRequest>(&context.request.payload)
        else {
            return ServiceResponse::error("invalid_request", "Invalid model request");
        };
        if request.validate().is_err()
            || request.instance.plugin_id != PLUGIN_ID
            || request.kind.operation() != context.request.operation
        {
            return ServiceResponse::error(
                "invalid_request",
                "Model contract or operation mismatch",
            );
        }
        let result = self.invoke_models(&request, context).map_err(failure);
        let response = HarnessModelsResponse {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            instance_id: request.instance.instance_id.clone(),
            kind: result.unwrap_or_else(HarnessModelsResponseKind::Failure),
        };
        if response.validate_for(&request).is_err() {
            return ServiceResponse::error("invalid_response", "Model response violates contract");
        }
        ServiceResponse::json(&response).unwrap_or_else(|_| {
            ServiceResponse::error("invalid_response", "Model response encoding failed")
        })
    }

    fn invoke_models(
        &mut self,
        request: &HarnessModelsRequest,
        context: &NativeServiceContext,
    ) -> Result<HarnessModelsResponseKind, CodexError> {
        let configuration = Config::from_context(context)?;
        configuration.validate_instance(&request.instance)?;
        match &request.kind {
            HarnessModelsRequestKind::DiscoverModels => {
                // A dedicated app-server in the configured namespace; no thread, prompt or
                // account mutation.
                let launch = launch(
                    &request.instance,
                    configuration.auth_working_directory(&request.instance),
                )?;
                let mut server = AppServer::spawn(&launch)?;
                let native_version = server.initialize(&launch)?;
                let mut models = Vec::new();
                let mut cursor: Option<String> = None;
                for _ in 0..MAX_MODEL_PAGES {
                    let page = server.call(
                        "model/list",
                        json!({"includeHidden": false, "limit": 100, "cursor": cursor}),
                    )?;
                    models_from_page(&page, &mut models)?;
                    cursor = page["nextCursor"].as_str().map(str::to_owned);
                    if cursor.is_none() || models.len() >= MAX_HARNESS_MODELS {
                        break;
                    }
                }
                Ok(HarnessModelsResponseKind::Models {
                    native_version,
                    models,
                })
            }
            HarnessModelsRequestKind::OpenSelected { open, selection } => {
                let opened = self.handle_open(&request.instance, open, context, Some(selection))?;
                let HarnessServiceResponseKind::Opened(binding) = opened else {
                    return Err(CodexError::Uncertain);
                };
                let effective_model_id = self
                    .owners
                    .get(&open.session_id)
                    .and_then(|owner| owner.effective_model.clone());
                Ok(HarnessModelsResponseKind::Selected {
                    target: HarnessSessionTarget::Confirmed(binding),
                    evidence: HarnessModelEvidence {
                        requested_model_id: selection.model_id.clone(),
                        // Native thread/start reports the configured model for the thread.
                        effective_model_id,
                    },
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_model_list_maps_visible_unique_models() {
        let mut models = Vec::new();
        models_from_page(
            &json!({"data":[
                {"id":"preset-a","model":"gpt-a","displayName":"GPT A","description":"Fast","hidden":false,"isDefault":true},
                {"id":"preset-b","model":"gpt-b","displayName":"","description":"","hidden":false,"isDefault":false},
                {"id":"hidden","model":"gpt-h","displayName":"Hidden","description":"x","hidden":true,"isDefault":false},
                {"id":"dup","model":"gpt-a","displayName":"Dup","description":"x","hidden":false,"isDefault":false},
                {"id":" bad","model":" bad","displayName":"Bad","description":"x","hidden":false,"isDefault":false}
            ],"nextCursor":null}),
            &mut models,
        )
        .unwrap();
        assert_eq!(
            models,
            vec![
                HarnessModel {
                    model_id: "gpt-a".into(),
                    display_name: "GPT A".into(),
                    description: Some("Fast (Codex default)".into()),
                },
                HarnessModel {
                    model_id: "gpt-b".into(),
                    display_name: "gpt-b".into(),
                    description: None,
                },
            ]
        );
        assert_eq!(
            models_from_page(&json!({"unexpected":true}), &mut models),
            Err(CodexError::Protocol)
        );
    }
}
