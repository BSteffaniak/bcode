//! Finite CLI-native manifestations of renderer-neutral command effects.
//!
//! Emitted text is presentation output, not implicitly persisted session content.
//! Session navigation resolves canonical metadata but never attaches a terminal.

use bcode_command::{CommandTextFormat, InvokeCommandResponse, SessionOpenFocus};
use bcode_session_models::{SessionId, SessionSummary};

/// Adapt a typed command result without turning uncertainty into terminal failure.
pub async fn consume_invocation(
    client: &bcode_client::BcodeClient,
    plugin_id: &str,
    result: bcode_command::application::CommandInvocationResult,
) -> (InvocationOutcome, bool) {
    match result.outcome {
        bcode_command::application::CommandInvocationOutcome::Completed(response) => {
            let (effect_outcomes, failed) = consume_effects(client, plugin_id, &response).await;
            let failed = failed || !response.success;
            (
                InvocationOutcome {
                    invocation_id: result.invocation_id,
                    outcome: bcode_command::application::CommandInvocationOutcome::Completed(
                        response,
                    ),
                    effect_outcomes,
                },
                failed,
            )
        }
        outcome @ (bcode_command::application::CommandInvocationOutcome::Failed(_)
        | bcode_command::application::CommandInvocationOutcome::Unknown(_)) => (
            InvocationOutcome {
                invocation_id: result.invocation_id,
                outcome,
                effect_outcomes: Vec::new(),
            },
            true,
        ),
    }
}

/// Structured CLI manifestation retains identity, known/unknown outcome and effect order.
#[derive(Debug, serde::Serialize)]
pub struct InvocationOutcome {
    pub invocation_id: String,
    pub outcome: bcode_command::application::CommandInvocationOutcome,
    pub effect_outcomes: Vec<EffectOutcome>,
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EffectOutcome {
    Emitted {
        #[serde(flatten)]
        content: EmittedContent,
    },
    Opened {
        session: Box<SessionSummary>,
        focus: SessionOpenFocus,
        attached: bool,
    },
    Failed {
        code: &'static str,
        session_id: SessionId,
    },
    Unsupported {
        code: &'static str,
        #[serde(flatten)]
        fallback: SurfaceFallback,
    },
    NotApplied {
        reason: &'static str,
    },
}

/// The plugin-owned options are preserved verbatim, not interpreted as host workflow
/// actions or pending-exchange authority. Native presentation remains unsupported.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SurfaceFallback {
    PluginSurface {
        owner_plugin_id: String,
        surface_kind: String,
        instance_id: String,
        options: serde_json::Value,
        next_action: &'static str,
    },
    HostSurface {
        surface_id: String,
        next_action: &'static str,
    },
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EmittedContent {
    Status {
        message: String,
    },
    TranscriptText {
        text: String,
        format: CommandTextFormat,
    },
    CopyText {
        text: String,
        clipboard_written: bool,
    },
}

/// Consume portable effects into CLI output without claiming native presentation.
pub async fn consume_effects(
    client: &bcode_client::BcodeClient,
    plugin_id: &str,
    response: &InvokeCommandResponse,
) -> (Vec<EffectOutcome>, bool) {
    let mut outcomes = Vec::new();
    let mut failed = !response.success;
    for effect in &response.effects {
        // Failed commands do not drive effects as if they succeeded.
        if !response.success {
            outcomes.push(EffectOutcome::NotApplied {
                reason: "command_failed",
            });
            continue;
        }
        let outcome = match effect {
            bcode_command::CommandEffect::Status { message } => EffectOutcome::Emitted {
                content: EmittedContent::Status {
                    message: message.clone(),
                },
            },
            bcode_command::CommandEffect::AppendText { text, format } => EffectOutcome::Emitted {
                content: EmittedContent::TranscriptText {
                    text: text.clone(),
                    format: *format,
                },
            },
            bcode_command::CommandEffect::CopyText { text } => {
                // Automation has no implicit desktop clipboard. Return exact literal text
                // to the caller rather than claiming a clipboard write occurred.
                EffectOutcome::Emitted {
                    content: EmittedContent::CopyText {
                        text: text.clone(),
                        clipboard_written: false,
                    },
                }
            }
            bcode_command::CommandEffect::OpenSession { session_id, focus } => {
                client.session_summary(*session_id).await.map_or_else(
                    |_| {
                        failed = true;
                        EffectOutcome::Failed {
                            code: "session_open_failed",
                            session_id: *session_id,
                        }
                    },
                    |session| EffectOutcome::Opened {
                        session: Box::new(session),
                        focus: *focus,
                        attached: false,
                    },
                )
            }
            bcode_command::CommandEffect::ToggleSurface { surface_id } => {
                failed = true;
                EffectOutcome::Unsupported {
                    code: "native_surface_unavailable",
                    fallback: SurfaceFallback::HostSurface {
                        surface_id: surface_id.clone(),
                        next_action: "inspect the stable surface identity; use a native frontend for presentation",
                    },
                }
            }
            bcode_command::CommandEffect::OpenPluginSurface {
                surface_kind,
                instance_id,
                options,
            } => {
                failed = true;
                EffectOutcome::Unsupported {
                    code: "native_surface_unavailable",
                    fallback: SurfaceFallback::PluginSurface {
                        owner_plugin_id: plugin_id.to_owned(),
                        surface_kind: surface_kind.clone(),
                        instance_id: instance_id.clone(),
                        options: options.clone(),
                        next_action: "inspect plugin-owned options using the owning schema; use typed domain operations for actions or a native frontend for presentation",
                    },
                }
            }
        };
        outcomes.push(outcome);
    }
    (outcomes, failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_command::CommandEffect;

    fn response(success: bool, effects: Vec<CommandEffect>) -> InvokeCommandResponse {
        InvokeCommandResponse {
            success,
            message: None,
            updated_model: None,
            updated_provider: None,
            updated_thinking: None,
            effects,
        }
    }

    #[tokio::test]
    async fn typed_unknown_outcome_preserves_uncertainty_without_effect_execution() {
        let client =
            bcode_client::BcodeClient::new(bcode_ipc::IpcEndpoint::unix_socket("unused.sock"));
        let result = bcode_command::application::CommandInvocationResult {
            invocation_id: "lost-response".into(),
            outcome: bcode_command::application::CommandInvocationOutcome::Unknown(
                bcode_command::application::CommandApplicationFailure {
                    code: "command_outcome_unknown".into(),
                    message: "Owner response unavailable; effects may have committed".into(),
                },
            ),
        };
        let (output, failed) = consume_invocation(&client, "test.command", result).await;
        assert!(failed);
        assert_eq!(output.invocation_id, "lost-response");
        assert!(output.effect_outcomes.is_empty());
        assert!(matches!(
            output.outcome,
            bcode_command::application::CommandInvocationOutcome::Unknown(_)
        ));
        let json = serde_json::to_value(output).unwrap();
        assert_eq!(json["outcome"]["status"], "unknown");
        let restored: bcode_command::application::CommandInvocationResult =
            serde_json::from_value(serde_json::json!({
                "invocation_id": json["invocation_id"], "outcome": json["outcome"],
            }))
            .unwrap();
        assert!(matches!(
            restored.outcome,
            bcode_command::application::CommandInvocationOutcome::Unknown(_)
        ));
    }

    #[tokio::test]
    async fn typed_terminal_failure_preserves_identity_without_effect_execution() {
        let client =
            bcode_client::BcodeClient::new(bcode_ipc::IpcEndpoint::unix_socket("unused.sock"));
        let failure = bcode_command::application::CommandApplicationFailure {
            code: "command_cancelled".to_owned(),
            message: "command execution cancelled".to_owned(),
        };
        let (result, failed) = consume_invocation(
            &client,
            "owner",
            bcode_command::application::CommandInvocationResult {
                invocation_id: "caller-operation-1".to_owned(),
                outcome: bcode_command::application::CommandInvocationOutcome::Failed(
                    failure.clone(),
                ),
            },
        )
        .await;
        assert!(failed);
        assert_eq!(result.invocation_id, "caller-operation-1");
        assert!(result.effect_outcomes.is_empty());
        assert_eq!(
            result.outcome,
            bcode_command::application::CommandInvocationOutcome::Failed(failure)
        );
    }

    #[tokio::test]
    async fn typed_unsuccessful_response_retains_effects_without_applying_them() {
        let client =
            bcode_client::BcodeClient::new(bcode_ipc::IpcEndpoint::unix_socket("unused.sock"));
        let response = response(
            false,
            vec![CommandEffect::OpenSession {
                session_id: SessionId::new(),
                focus: SessionOpenFocus::Composer,
            }],
        );
        let (result, failed) = consume_invocation(
            &client,
            "owner",
            bcode_command::application::CommandInvocationResult {
                invocation_id: "caller-operation-2".to_owned(),
                outcome: bcode_command::application::CommandInvocationOutcome::Completed(
                    response.clone(),
                ),
            },
        )
        .await;
        assert!(failed);
        assert_eq!(result.invocation_id, "caller-operation-2");
        assert_eq!(
            result.outcome,
            bcode_command::application::CommandInvocationOutcome::Completed(response)
        );
        assert!(matches!(
            &result.effect_outcomes[..],
            [EffectOutcome::NotApplied { .. }]
        ));
    }

    #[tokio::test]
    async fn content_effects_preserve_order_literal_text_and_format() {
        let client = bcode_client::BcodeClient::new(bcode_ipc::IpcEndpoint::unix_socket(
            std::path::PathBuf::from("unused.sock"),
        ));
        let response = response(
            true,
            vec![
                CommandEffect::Status {
                    message: "ready 🦀".to_owned(),
                },
                CommandEffect::AppendText {
                    text: "e\u{301}\n**literal**\n".to_owned(),
                    format: CommandTextFormat::Markdown,
                },
                CommandEffect::CopyText {
                    text: " exact\n".to_owned(),
                },
            ],
        );
        let (outcomes, failed) = consume_effects(&client, "producer", &response).await;
        assert!(!failed);
        let value = serde_json::to_value(outcomes).unwrap();
        assert_eq!(value[0]["kind"], "status");
        assert_eq!(value[0]["message"], "ready 🦀");
        assert_eq!(value[1]["kind"], "transcript_text");
        assert_eq!(value[1]["text"], "e\u{301}\n**literal**\n");
        assert_eq!(value[1]["format"], "markdown");
        assert_eq!(value[2]["kind"], "copy_text");
        assert_eq!(value[2]["text"], " exact\n");
        assert_eq!(value[2]["clipboard_written"], false);
    }

    #[tokio::test]
    async fn failed_command_does_not_drive_any_effect() {
        let client = bcode_client::BcodeClient::new(bcode_ipc::IpcEndpoint::unix_socket(
            std::path::PathBuf::from("unused.sock"),
        ));
        let failed_response = response(
            false,
            vec![CommandEffect::OpenSession {
                session_id: SessionId::new(),
                focus: SessionOpenFocus::Composer,
            }],
        );
        let (outcomes, failed) = consume_effects(&client, "producer", &failed_response).await;
        assert!(failed);
        assert!(matches!(
            outcomes.as_slice(),
            [EffectOutcome::NotApplied {
                reason: "command_failed"
            }]
        ));
        let (outcomes, failed) =
            consume_effects(&client, "producer", &response(false, Vec::new())).await;
        assert!(
            failed,
            "an inner failure with no effects must remain unsuccessful"
        );
        assert!(outcomes.is_empty());
    }

    #[tokio::test]
    async fn native_surface_fallback_preserves_opaque_plugin_options() {
        let client = bcode_client::BcodeClient::new(bcode_ipc::IpcEndpoint::unix_socket(
            std::path::PathBuf::from("unused.sock"),
        ));
        let options =
            serde_json::json!({"future_plugin_schema": {"version":97,"value":[true,null,"🦀"]}});
        let response = response(
            true,
            vec![
                CommandEffect::OpenPluginSurface {
                    surface_kind: "owned.inspection".to_owned(),
                    instance_id: "instance".to_owned(),
                    options: options.clone(),
                },
                CommandEffect::ToggleSurface {
                    surface_id: "host.surface".to_owned(),
                },
            ],
        );
        let (outcomes, failed) = consume_effects(&client, "producer", &response).await;
        assert!(failed);
        let value = serde_json::to_value(outcomes).unwrap();
        assert_eq!(value[0]["status"], "unsupported");
        assert_eq!(value[0]["owner_plugin_id"], "producer");
        assert_eq!(value[0]["surface_kind"], "owned.inspection");
        assert_eq!(value[0]["instance_id"], "instance");
        assert_eq!(value[0]["options"], options);
        assert!(value[0]["next_action"].is_string());
        assert_eq!(value[1]["surface_id"], "host.surface");
    }
}
