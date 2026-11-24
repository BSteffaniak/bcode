//! Optional `bcode.harness.models/v1` service: `agy models` discovery and exact selected open.
//! Advertised models are not entitlement or proof of successful inference.
use crate::transport::{AcpError, probe_version, run_bounded_cli};
use crate::{PLUGIN_ID, PluginState, config::Config, failure, launch};
use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessModel, HarnessModelEvidence, HarnessModelsRequest,
    HarnessModelsRequestKind, HarnessModelsResponse, HarnessModelsResponseKind,
    HarnessServiceResponseKind, HarnessSessionTarget, MAX_HARNESS_ID_BYTES, MAX_HARNESS_MODELS,
    MAX_HARNESS_TEXT_BYTES, MAX_HARNESS_WIRE_BYTES,
};
use bcode_plugin_sdk::prelude::{NativeServiceContext, ServiceResponse};
use std::collections::BTreeSet;

/// Parse tab-separated `<model_id>\t<display_name>` lines from `agy models` output.
/// Skips header/status lines (e.g. `Fetching available models...`) and malformed entries.
#[must_use]
pub fn parse_agy_models_output(output: &str) -> Vec<HarnessModel> {
    let mut models = Vec::new();
    let mut seen = BTreeSet::new();
    for raw_line in output.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((id_part, name_part)) = line.split_once('\t') else {
            continue;
        };
        let model_id = id_part.trim();
        let display_name = name_part.trim();
        if model_id.is_empty()
            || model_id.len() > MAX_HARNESS_ID_BYTES
            || model_id.chars().any(char::is_control)
            || display_name.is_empty()
            || display_name.len() > MAX_HARNESS_TEXT_BYTES
            || display_name.chars().any(char::is_control)
        {
            continue;
        }
        if models.len() >= MAX_HARNESS_MODELS || !seen.insert(model_id.to_owned()) {
            continue;
        }
        let model = HarnessModel {
            model_id: model_id.to_owned(),
            display_name: display_name.to_owned(),
            description: None,
        };
        if model.validate().is_ok() {
            models.push(model);
        }
    }
    models
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
    ) -> Result<HarnessModelsResponseKind, AcpError> {
        let configuration = Config::from_context(context)?;
        configuration.validate_instance(&request.instance)?;
        match &request.kind {
            HarnessModelsRequestKind::DiscoverModels => {
                let launch = launch(
                    &request.instance,
                    configuration.auth_working_directory(&request.instance),
                )?;
                let native_version =
                    match configuration.instances.get(&request.instance.instance_id) {
                        Some(selected) => selected.exact_version.clone(),
                        None => probe_version(&launch)?,
                    };
                let output = run_bounded_cli(&launch, &["models"])?;
                let models = parse_agy_models_output(&output);
                Ok(HarnessModelsResponseKind::Models {
                    native_version,
                    models,
                })
            }
            HarnessModelsRequestKind::OpenSelected { open, selection } => {
                let opened = self.handle_open(&request.instance, open, Some(selection))?;
                let HarnessServiceResponseKind::Opened(binding) = opened else {
                    return Err(AcpError::Protocol);
                };
                let effective_model_id = self
                    .owners
                    .get(&open.session_id)
                    .and_then(|owner| owner.effective_model.clone());
                Ok(HarnessModelsResponseKind::Selected {
                    target: HarnessSessionTarget::Confirmed(binding),
                    evidence: HarnessModelEvidence {
                        requested_model_id: selection.model_id.clone(),
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
    fn parses_agy_models_table_and_skips_status_lines() {
        let sample = "\
Fetching available models...
gemini-4-neon-high\tGemini 4 Neon (High)
gemini-4-neon-medium\tGemini 4 Neon (Medium)
gemini-4-neon-high\tDuplicate Should Skip
not-a-model-line
";
        let models = parse_agy_models_output(sample);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].model_id, "gemini-4-neon-high");
        assert_eq!(models[0].display_name, "Gemini 4 Neon (High)");
        assert_eq!(models[1].model_id, "gemini-4-neon-medium");
        assert_eq!(models[1].display_name, "Gemini 4 Neon (Medium)");
    }
}
