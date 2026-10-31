//! Model execution through the admitted, caller-bound public host service only.
use base64::Engine as _;
use bcode_model::{
    ContentBlock, MessageRole, ModelMessage, ProviderTurnEvent, host_operation as wire,
};
use bcode_plugin_sdk::{
    ServiceBridge, ServiceBridgeRequest, ServiceBridgeResponse, ServiceCancellation,
};
use bcode_recap::{
    analysis_contracts::analysis_response_schema,
    model::{
        AnalysisFuture, AnalysisRequest, AnalysisResponse, ModelError, ModelExecutor, ModelFuture,
        ModelRequest, ModelResponse,
    },
    process::Cancellation,
};
use bcode_tool::{ToolInvocationServiceRequest, ToolInvocationServiceResolution};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct HostExecutor {
    pub bridge: ServiceBridge,
    pub invocation_id: String,
    pub cancellation: ServiceCancellation,
}
impl HostExecutor {
    pub fn check_admission(&self) -> Result<(), ModelError> {
        self.call::<bcode_model::AckResponse>(wire::OP_ADMISSION, serde_json::json!({}))?;
        Ok(())
    }
    fn call<T: serde::de::DeserializeOwned>(
        &self,
        operation: &str,
        payload: impl serde::Serialize,
    ) -> Result<T, ModelError> {
        let response = self
            .bridge
            .request(&ServiceBridgeRequest::InvokeService(
                ToolInvocationServiceRequest {
                    invocation_id: self.invocation_id.clone(),
                    request_id: format!("{}-{operation}", self.invocation_id),
                    route_id: None,
                    interface_id: wire::INTERFACE_ID.into(),
                    operation: operation.into(),
                    payload: serde_json::to_value(payload)
                        .map_err(|_| ModelError::ExecutionFailed)?,
                },
            ))
            .map_err(|_| ModelError::ExecutionFailed)?;
        match response {
            ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded {
                payload,
            }) => serde_json::from_value(payload).map_err(|_| ModelError::InvalidResponse),
            ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Cancelled) => {
                Err(ModelError::Cancelled)
            }
            ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Failed {
                code,
                ..
            }) => {
                let category = match code.as_str() {
                    "model_host_not_admitted" => "admission_denied",
                    "invalid_request" => "request_invalid",
                    "structured_decode_rejected" => "structured_decode_rejected",
                    "structured_validation_rejected" => "structured_validation_rejected",
                    "start_envelope_rejected" => "start_envelope_rejected",
                    "request_conflict" => "request_conflict",
                    "cursor_evicted" => "event_cursor_evicted",
                    "cleanup_pending" => "cleanup_pending",
                    "model_unavailable" => "model_unavailable",
                    _ => "host_operation_failed",
                };
                eprintln!("recap_model_host: {category}");
                Err(ModelError::Unavailable)
            }
            _ => Err(ModelError::Unavailable),
        }
    }
}
impl ModelExecutor for HostExecutor {
    fn recovery_identity(&self) -> Option<String> {
        let identity: wire::RecoveryIdentity = self
            .call(wire::OP_RECOVERY_IDENTITY, serde_json::json!({}))
            .ok()?;
        (identity.version == 1 && !identity.identity.is_empty()).then_some(identity.identity)
    }

    fn execute<'a>(&'a self, request: ModelRequest, cancel: &'a Cancellation) -> ModelFuture<'a> {
        Box::pin(async move {
            let max_bytes = request.max_response_bytes;
            let input = structured_request(request)?;
            let text = self.execute_structured(input, max_bytes, cancel).await?;
            ModelResponse::from_json(text.as_bytes(), max_bytes)
        })
    }

    fn execute_analysis<'a>(
        &'a self,
        request: AnalysisRequest,
        cancel: &'a Cancellation,
    ) -> AnalysisFuture<'a> {
        Box::pin(async move {
            let stage = request.stage;
            let max_bytes = request.max_response_bytes;
            let input = structured_analysis_request(request)?;
            let text = self.execute_structured(input, max_bytes, cancel).await?;
            AnalysisResponse::from_json_slice(stage, text.as_bytes(), max_bytes)
        })
    }
}

impl HostExecutor {
    async fn execute_structured(
        &self,
        input: wire::StructuredStartRequest,
        max_bytes: usize,
        cancel: &Cancellation,
    ) -> Result<String, ModelError> {
        if cancel.is_cancelled() || self.cancellation.is_cancelled() {
            return Err(ModelError::Cancelled);
        }
        let request_bytes = serde_json::to_vec(&input)
            .map_err(|_| ModelError::ExecutionFailed)?
            .len();
        eprintln!(
            "recap_model_request: serialized_bytes={request_bytes} image_count={}",
            input
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter(|block| matches!(block, ContentBlock::Image { .. }))
                .count()
        );
        let scope_id = input.scope_id.clone();
        input.validate_public().map_err(|reason| {
            eprintln!("recap_model_request_rejected: {reason}");
            ModelError::ExecutionFailed
        })?;
        let started: wire::StartResponse = self.call(wire::OP_START_STRUCTURED, &input)?;
        let turn = wire::TurnRequest {
            scope_id: scope_id.clone(),
            turn_id: started.turn_id.clone(),
        };
        let mut cursor = 0;
        let mut text = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        let result = loop {
            if cancel.is_cancelled()
                || self.cancellation.is_cancelled()
                || tokio::time::Instant::now() >= deadline
            {
                let _: Result<bcode_model::AckResponse, _> = self.call(wire::OP_CANCEL, &turn);
                break Err(ModelError::Cancelled);
            }
            let page: wire::PollResponse = match self.call(
                wire::OP_POLL,
                wire::PollRequest {
                    scope_id: scope_id.clone(),
                    turn_id: started.turn_id.clone(),
                    after_sequence: cursor,
                },
            ) {
                Ok(page) => page,
                Err(error) => break Err(error),
            };
            let mut failed = false;
            for event in page.events {
                match event.event {
                    ProviderTurnEvent::TextDelta { text: delta }
                    | ProviderTurnEvent::Output {
                        event: bcode_model::ProviderOutputEvent::TextDelta { text: delta },
                        ..
                    } => text.push_str(&delta),
                    ProviderTurnEvent::Error { .. } | ProviderTurnEvent::Cancelled => {
                        failed = true;
                    }
                    _ => {}
                }
            }
            if failed || text.len() > max_bytes {
                break Err(ModelError::ExecutionFailed);
            }
            cursor = page.next_sequence;
            if page.terminal {
                break Ok(text);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        // Finish is explicit and never triggers retry of an uncertain model operation.
        let _: Result<bcode_model::AckResponse, _> = self.call(wire::OP_CANCEL, &turn);
        let _: Result<bcode_model::AckResponse, _> = self.call(wire::OP_FINISH, &turn);
        result
    }
}

fn structured_analysis_request(
    request: AnalysisRequest,
) -> Result<wire::StructuredStartRequest, ModelError> {
    let schema = analysis_response_schema(request.stage);
    structured_request_with_schema(
        ModelRequest {
            instructions: request.instructions,
            input_text: serde_json::to_string(&request.input)
                .map_err(|_| ModelError::InvalidResponse)?,
            images: request.images,
            max_response_bytes: request.max_response_bytes,
        },
        schema,
    )
}

fn structured_request(request: ModelRequest) -> Result<wire::StructuredStartRequest, ModelError> {
    structured_request_with_schema(request, bcode_recap::model::response_schema())
}

fn structured_request_with_schema(
    request: ModelRequest,
    schema: serde_json::Value,
) -> Result<wire::StructuredStartRequest, ModelError> {
    let mut content = vec![ContentBlock::Text {
        text: request.input_text,
    }];
    for image in request.images {
        content.push(ContentBlock::Text {
            text: format!("Source asset: {}", image.asset_id),
        });
        content.push(ContentBlock::Image {
            image: bcode_model::ImageContent {
                mime_type: image.mime_type,
                data_base64: base64::engine::general_purpose::STANDARD.encode(image.bytes),
                metadata: bcode_model::ImageMetadata::default(),
            },
        });
    }
    let scope_id = format!(
        "recap-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ModelError::ExecutionFailed)?
            .as_nanos()
    );
    let input = wire::StructuredStartRequest {
        scope_id: scope_id.clone(),
        request_id: scope_id,
        expires_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ModelError::ExecutionFailed)?
            .as_secs()
            + 120,
        messages: vec![
            ModelMessage {
                role: MessageRole::System,
                content: vec![ContentBlock::Text {
                    text: request.instructions,
                }],
            },
            ModelMessage {
                role: MessageRole::User,
                content,
            },
        ],
        output: bcode_model::StructuredOutputRequest {
            name: "recap".into(),
            schema,
            strict: true,
        },
        max_output_tokens: Some(8192),
    };

    Ok(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_recap::model::AnalysisStage;

    #[test]
    fn analysis_requests_preserve_stage_schema_source_channels_and_token_compatibility() {
        for stage in [
            AnalysisStage::ExtractionInventory,
            AnalysisStage::Outline,
            AnalysisStage::ChapterComposition,
            AnalysisStage::SourceAudit,
            AnalysisStage::SourceRepair,
            AnalysisStage::VisualInspection,
        ] {
            let input = structured_analysis_request(AnalysisRequest {
                stage,
                instructions: "trusted analysis policy".into(),
                input: serde_json::json!({"source": "untrusted caption"}),
                images: vec![bcode_recap::model::ImageAsset {
                    asset_id: "frame-1".into(),
                    mime_type: "image/jpeg".into(),
                    bytes: vec![1, 2, 3],
                }],
                max_response_bytes: 1024,
            })
            .unwrap();
            input.validate_public().unwrap();
            assert_eq!(input.output.schema, analysis_response_schema(stage));
            assert_eq!(input.max_output_tokens, Some(8192));
            assert_eq!(input.messages[0].role, MessageRole::System);
            assert_eq!(
                input.messages[0].content,
                vec![ContentBlock::Text {
                    text: "trusted analysis policy".into()
                }]
            );
            assert_eq!(input.messages[1].role, MessageRole::User);
            assert!(
                matches!(&input.messages[1].content[0], ContentBlock::Text { text } if text.contains("untrusted caption"))
            );
            assert!(
                matches!(&input.messages[1].content[2], ContentBlock::Image { image } if image.data_base64 == "AQID")
            );
            let standard = br#"{"title":"not analysis","sections":[],"image_choices":[]}"#;
            assert_eq!(
                AnalysisResponse::from_json_slice(stage, standard, 1024),
                Err(ModelError::InvalidResponse)
            );
        }
    }

    #[tokio::test]
    async fn cancelled_analysis_never_requires_a_host_bridge() {
        let executor = HostExecutor {
            bridge: ServiceBridge::default(),
            invocation_id: "cancelled-test".into(),
            cancellation: ServiceCancellation::default(),
        };
        let cancel = Cancellation::default();
        cancel.cancel();
        let result = executor
            .execute_analysis(
                AnalysisRequest {
                    stage: AnalysisStage::Outline,
                    instructions: "policy".into(),
                    input: serde_json::json!({}),
                    images: vec![],
                    max_response_bytes: 1024,
                },
                &cancel,
            )
            .await;
        assert_eq!(result, Err(ModelError::Cancelled));
    }

    #[test]
    fn generated_multimodal_request_round_trips_host_contract_without_losing_frames() {
        let request = bcode_recap::model::ModelRequest {
            instructions: "Compose only grounded editorial content".into(),
            input_text: serde_json::to_string(&serde_json::json!({"transcript": (0..919).map(|i| serde_json::json!({"id":format!("cue-{i}"),"text":"actual source evidence".repeat(8)})).collect::<Vec<_>>()})).unwrap(),
            images: (0..12).map(|i| bcode_recap::model::ImageAsset { asset_id: format!("frame-{i}"), mime_type: "image/jpeg".into(), bytes: vec![0; 4096] }).collect(),
            max_response_bytes: 1024 * 1024,
        };
        let input = super::structured_request(request).unwrap();
        input.validate_public().unwrap();
        let encoded = serde_json::to_vec(&input).unwrap();
        assert!(encoded.len() > 196_608 && encoded.len() < 900_000);
        let host: bcode_model::host_operation::StructuredStartRequest =
            serde_json::from_slice(&encoded).unwrap();
        host.validate_public().unwrap();
        assert_eq!(host, input);
        assert_eq!(
            host.messages
                .iter()
                .flat_map(|message| &message.content)
                .filter(|block| matches!(block, bcode_model::ContentBlock::Image { .. }))
                .count(),
            12
        );
    }
}
