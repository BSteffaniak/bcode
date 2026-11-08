//! Optional model discovery/selection service; no model-provider or native agent loop.
use super::{
    ClaudeSession, Config, HARNESS_SCHEMA_VERSION, HarnessFailure, HarnessServiceResponseKind,
    HarnessSessionTarget, InstanceConfig, MAX_HARNESS_WIRE_BYTES, NativeServiceContext, PLUGIN_ID,
    PluginState, ServiceResponse,
};
use bcode_harness_models::{
    HarnessModel, HarnessModelEvidence, HarnessModelSelection, HarnessModelsRequest,
    HarnessModelsRequestKind, HarnessModelsResponse, HarnessModelsResponseKind,
};

pub struct RetainedSelection {
    pub selection: HarnessModelSelection,
    pub evidence: Option<HarnessModelEvidence>,
}

pub fn same_selection(
    retained: Option<&RetainedSelection>,
    proposed: Option<&HarnessModelSelection>,
) -> bool {
    match (retained, proposed) {
        (None, None) => true,
        (Some(retained), Some(proposed)) => retained.selection == *proposed,
        _ => false,
    }
}

impl PluginState {
    pub(super) fn invoke_models_service(
        &mut self,
        context: &NativeServiceContext,
    ) -> ServiceResponse {
        if context.request.payload.len() > MAX_HARNESS_WIRE_BYTES {
            return ServiceResponse::error("invalid_request", "Model request exceeds bound");
        }
        let request: HarnessModelsRequest = match serde_json::from_slice(&context.request.payload) {
            Ok(request) => request,
            Err(_) => return ServiceResponse::error("invalid_request", "Invalid model request"),
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
        let configuration: Config = match context.config() {
            Ok(configuration) => configuration,
            Err(_) => {
                return ServiceResponse::error("invalid_config", "Invalid Claude configuration");
            }
        };
        let result =
            if let Some(selected) = configuration.instances.get(&request.instance.instance_id) {
                if self.runtime.is_none() {
                    self.runtime = tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(2)
                        .enable_all()
                        .build()
                        .ok();
                }
                if let Some(runtime) = self.runtime.take() {
                    let result = runtime.block_on(self.invoke_models(&request, selected, context));
                    self.runtime = Some(runtime);
                    result
                } else {
                    Err(HarnessFailure::Unavailable)
                }
            } else {
                Err(HarnessFailure::Unavailable)
            };
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

    async fn invoke_models(
        &mut self,
        request: &HarnessModelsRequest,
        selected: &InstanceConfig,
        context: &NativeServiceContext,
    ) -> Result<HarnessModelsResponseKind, HarnessFailure> {
        match &request.kind {
            HarnessModelsRequestKind::DiscoverModels => {
                // The configured instance owns executable, namespace and auth custody.
                // No prompt, resume, native conversation adoption or default provider is used.
                let options = selected.options_with_context(
                    &request.instance,
                    selected.config_directory.clone(),
                    None,
                    context,
                )?;
                let session = ClaudeSession::spawn(options)
                    .await
                    .map_err(|_| HarnessFailure::Unavailable)?;
                let client = session.client();
                let result = async {
                    let initialize = client
                        .initialize()
                        .await
                        .map_err(|_| HarnessFailure::Unavailable)?;
                    let models = bcode_claude_code::available_models(&initialize)
                        .map_err(|_| HarnessFailure::Unsupported)?;
                    Ok(HarnessModelsResponseKind::Models {
                        // The probed executable version that advertised this snapshot.
                        native_version: client.native_version().to_owned(),
                        models: models
                            .into_iter()
                            .map(|model| HarnessModel {
                                model_id: model.value,
                                display_name: model.display_name,
                                description: model.description,
                            })
                            .collect(),
                    })
                }
                .await;
                client.terminate();
                client
                    .wait_reaped()
                    .await
                    .map_err(|_| HarnessFailure::Uncertain)?;
                result
            }
            HarnessModelsRequestKind::OpenSelected { open, selection } => {
                // Resume model mutation is not a supported recovery path in this extension.
                if open.resume.is_some() {
                    return Err(HarnessFailure::Unsupported);
                }
                let opened = self
                    .open_with_model(&request.instance, open, selected, context, Some(selection))
                    .await?;
                let target = match opened {
                    HarnessServiceResponseKind::Prepared(value) => {
                        HarnessSessionTarget::Prepared(value)
                    }
                    HarnessServiceResponseKind::Opened(value) => {
                        HarnessSessionTarget::Confirmed(value)
                    }
                    _ => return Err(HarnessFailure::Uncertain),
                };
                let evidence = self
                    .model_selections
                    .get(&open.session_id)
                    .and_then(|retained| retained.evidence.clone())
                    .ok_or(HarnessFailure::Uncertain)?;
                Ok(HarnessModelsResponseKind::Selected { target, evidence })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_selection_cannot_be_replaced_by_legacy_or_another_model() {
        let selection = HarnessModelSelection {
            model_id: "exact-native-key".into(),
        };
        let retained = RetainedSelection {
            selection: selection.clone(),
            evidence: None,
        };
        assert!(same_selection(None, None));
        assert!(same_selection(Some(&retained), Some(&selection)));
        assert!(!same_selection(Some(&retained), None));
        assert!(!same_selection(None, Some(&selection)));
        assert!(!same_selection(
            Some(&retained),
            Some(&HarnessModelSelection {
                model_id: "different".into()
            })
        ));
    }
}
