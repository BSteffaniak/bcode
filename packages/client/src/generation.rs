//! Tool-free structured generation through ordinary session admission.
use super::{BcodeClient, ClientError};
use bcode_plugin_sdk::generation::{
    PluginStructuredGenerationControl, PluginStructuredGenerationRequest,
    PluginStructuredGenerationResult,
};
use bcode_session_models::SessionId;

#[derive(Default)]
struct GenerationAssistantOutput {
    turn_id: Option<String>,
    legacy: Option<String>,
    segment: Option<(u32, String)>,
}

impl GenerationAssistantOutput {
    fn observe(&mut self, event: &bcode_session_models::SessionEventKind) {
        use bcode_session_models::SessionEventKind;
        match event {
            SessionEventKind::ModelTurnStarted { turn_id, .. } => {
                self.turn_id = Some(turn_id.clone());
                self.legacy = None;
                self.segment = None;
            }
            SessionEventKind::AssistantMessage { text } => self.legacy = Some(text.clone()),
            SessionEventKind::AssistantResponseSegment {
                turn_id,
                segment_order,
                text,
                ..
            }
            | SessionEventKind::PositionedAssistantResponseSegment {
                turn_id,
                segment_order,
                text,
                ..
            } if self.turn_id.as_ref() == Some(turn_id)
                && self
                    .segment
                    .as_ref()
                    .is_none_or(|(order, _)| segment_order >= order) =>
            {
                // Complete responses, not deltas: use the latest finalization segment.
                self.segment = Some((*segment_order, text.clone()));
            }
            _ => {}
        }
    }

    fn for_turn(&self, turn_id: &str) -> Option<&str> {
        if self.turn_id.as_deref() != Some(turn_id) {
            return None;
        }
        self.segment
            .as_ref()
            .map(|(_, text)| text.as_str())
            .or(self.legacy.as_deref())
    }
}

async fn prepare_observable_generation(
    client: &BcodeClient,
    request: &PluginStructuredGenerationRequest,
    control: &bcode_plugin_sdk::generation::PluginStructuredGenerationControl,
) -> Result<
    (
        SessionId,
        Option<bcode_session_models::PreparedContextGeneration>,
    ),
    ClientError,
> {
    if request.timeout_ms == 0 {
        return Err(ClientError::Protocol(
            "structured generation timeout must be positive".into(),
        ));
    }
    let prepared = prepare_generation_session(client, request).await?;
    control.set_session_id(prepared.0);
    if control.is_cancelled() {
        return Err(ClientError::Protocol(
            "generation cancelled before submission".into(),
        ));
    }
    Ok(prepared)
}

async fn prepare_generation_session(
    client: &BcodeClient,
    request: &PluginStructuredGenerationRequest,
) -> Result<
    (
        SessionId,
        Option<bcode_session_models::PreparedContextGeneration>,
    ),
    ClientError,
> {
    if let Some(source_session_id) = request.source_session_id {
        let prepared = client
            .prepare_context_generation(bcode_session_models::PrepareContextGeneration {
                version: 1,
                source_session_id,
                name: request.session_name.clone(),
            })
            .await
            .map_err(|error| ClientError::Protocol(error.to_string()))?;
        Ok((prepared.session_id, Some(prepared)))
    } else {
        let session = client
            .create_session(Some(request.session_name.clone()))
            .await
            .map_err(|error| ClientError::Protocol(error.to_string()))?;
        Ok((session.id, None))
    }
}

impl BcodeClient {
    /// Generate schema-validated output using an isolated session and ordinary permissions.
    ///
    /// # Errors
    /// Returns admission, provider, cancellation, timeout, or invalid-output errors.
    pub async fn generate_structured_output(
        &self,
        request: PluginStructuredGenerationRequest,
        control: PluginStructuredGenerationControl,
    ) -> Result<PluginStructuredGenerationResult, ClientError> {
        let client = self.clone();
        async move {
            let (session_id, prepared) =
                prepare_observable_generation(&client, &request, &control).await?;
            let prompt = format!("{}\n\n{}", request.system_prompt, request.prompt);
            client
                .send_user_message_with_execution(
                    session_id,
                    prompt,
                    bcode_ipc::PromptPlacement::FollowUp,
                    bcode_session_models::TurnExecutionOptions {
                        request_context_id: prepared
                            .as_ref()
                            .map(|prepared| prepared.context_id.clone()),
                        tools: bcode_session_models::TurnToolPolicy::Disabled,
                        structured_output: Some(
                            bcode_session_models::TurnStructuredOutputRequest {
                                name: request.output_name,
                                schema: request.output_schema,
                                strict: true,
                                max_corrections: 0,
                            },
                        ),
                        ..bcode_session_models::TurnExecutionOptions::default()
                    },
                )
                .await
                .map_err(|error| ClientError::Protocol(error.to_string()))?;
            let started = std::time::Instant::now();
            let mut cursor = None;
            let mut assistant = GenerationAssistantOutput::default();
            let mut cancellation_sent = false;
            loop {
                if control.is_cancelled() && !cancellation_sent {
                    client
                        .cancel_session_turn(session_id)
                        .await
                        .map_err(|error| ClientError::Protocol(error.to_string()))?;
                    cancellation_sent = true;
                }
                let page = client
                    .session_history_page(
                        session_id,
                        bcode_session_models::SessionHistoryQuery {
                            cursor,
                            limit: 100,
                            direction: bcode_session_models::SessionHistoryDirection::Forward,
                        },
                    )
                    .await
                    .map_err(|error| ClientError::Protocol(error.to_string()))?;
                for event in page.events {
                    cursor = Some(bcode_session_models::SessionHistoryCursor {
                        sequence: event.sequence,
                    });
                    assistant.observe(&event.kind);
                    if let bcode_session_models::SessionEventKind::ModelTurnFinished {
                        turn_id,
                        outcome,
                        message,
                        ..
                    } = event.kind
                    {
                        if outcome != bcode_session_models::ModelTurnOutcome::Completed {
                            return Err(ClientError::Protocol(format!(
                                "structured generation ended with {outcome:?}: {}",
                                message.unwrap_or_default()
                            )));
                        }
                        let text = assistant.for_turn(&turn_id).ok_or_else(|| {
                            ClientError::Protocol(
                                "structured generation returned no assistant payload".to_string(),
                            )
                        })?;
                        let output = serde_json::from_str(text).map_err(|error| {
                            ClientError::Protocol(format!(
                                "structured generation returned invalid JSON: {error}"
                            ))
                        })?;
                        return Ok(
                            bcode_plugin_sdk::generation::PluginStructuredGenerationResult {
                                output,
                                source: prepared.map(|prepared| prepared.source),
                            },
                        );
                    }
                }
                if started.elapsed() >= std::time::Duration::from_millis(request.timeout_ms) {
                    let _ = client.cancel_session_turn(session_id).await;
                    return Err(ClientError::Protocol(
                        "structured generation timed out".to_string(),
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        .await
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn generation_reads_positioned_clarification_and_prefers_final_segment() {
        use bcode_session_models::SessionEventKind as Event;
        let mut output = super::GenerationAssistantOutput::default();
        output.observe(&Event::ModelTurnStarted {
            turn_id: "generation".into(),
        });
        output.observe(&Event::AssistantMessage {
            text: "preliminary prose".into(),
        });
        let json = r#"{"outcome":"clarification_required","clarification":"Which capability?","implementation_prompt":"Not ready","stop_condition":"Not ready"}"#;
        output.observe(&Event::PositionedAssistantResponseSegment {
            turn_id: "generation".into(),
            output_position: bcode_session_models::TurnOutputPosition::new(0),
            segment_id: "segment-0".into(),
            segment_order: 1,
            text: json.into(),
        });
        output.observe(&Event::AssistantResponseSegment {
            turn_id: "generation".into(),
            segment_id: "older".into(),
            segment_order: 0,
            text: "older".into(),
        });
        output.observe(&Event::AssistantResponseSegment {
            turn_id: "other".into(),
            segment_id: "foreign".into(),
            segment_order: 2,
            text: "foreign".into(),
        });
        assert_eq!(output.for_turn("generation"), Some(json));
        let parsed: serde_json::Value =
            serde_json::from_str(output.for_turn("generation").unwrap()).unwrap();
        assert_eq!(parsed["outcome"], "clarification_required");
        assert!(output.for_turn("other").is_none());
        output.observe(&Event::ModelTurnStarted {
            turn_id: "next".into(),
        });
        assert!(output.for_turn("next").is_none());
        output.observe(&Event::AssistantMessage {
            text: "legacy response".into(),
        });
        assert_eq!(output.for_turn("next"), Some("legacy response"));
        output.observe(&Event::AssistantResponseSegment {
            turn_id: "next".into(),
            segment_id: "final".into(),
            segment_order: 0,
            text: "canonical response".into(),
        });
        assert_eq!(output.for_turn("next"), Some("canonical response"));
    }
}
