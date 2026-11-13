//! The conversation's Bcode agent decides a native agent's permission requests.
//!
//! A harness plugin declares how its native tools map onto Bcode's tool policy (a shell
//! command, a read or write of paths, a web fetch). For each committed native approval, the
//! host evaluates that operation under the conversation's agent exactly as for Bcode's own
//! tools: an allow or deny is answered for the user, anything else is left for the user to
//! answer. Undeclared tools and unreadable inputs are always left to the user.

use bcode_agent_profile::{AgentDecision, ToolPolicyOperation};
use bcode_harness_models::{
    HarnessInteraction, HarnessInteractionKind, HarnessObservation, HarnessObservationKind,
};
use bcode_plugin::{NativeToolOperation, NativeToolPolicyDeclaration};
use bcode_session_models::SessionId;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Native approvals per session, process-local, awaiting the agent's decision.
#[derive(Debug, Default)]
pub struct NativeApprovals {
    pending: Mutex<BTreeMap<SessionId, BTreeMap<String, HarnessInteraction>>>,
}

impl NativeApprovals {
    /// No approvals awaiting a decision.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending: Mutex::new(BTreeMap::new()),
        }
    }

    /// Track approvals raised, and drop approvals withdrawn, in one committed page.
    pub fn observed(&self, session: SessionId, observations: &[HarnessObservation]) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        for observation in observations {
            match &observation.kind {
                HarnessObservationKind::Interaction { interaction }
                    if interaction.kind == HarnessInteractionKind::Approval =>
                {
                    pending
                        .entry(session)
                        .or_default()
                        .insert(interaction.request_id.clone(), interaction.clone());
                }
                HarnessObservationKind::InteractionWithdrawn { request_id } => {
                    if let Some(session) = pending.get_mut(&session) {
                        session.remove(request_id);
                    }
                }
                _ => {}
            }
        }
        pending.retain(|_, approvals| !approvals.is_empty());
    }

    /// Take every approval awaiting a decision for `session`. Each is decided at most once.
    pub fn take(&self, session: SessionId) -> Vec<HarnessInteraction> {
        self.pending
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(&session))
            .map(|approvals| approvals.into_values().collect())
            .unwrap_or_default()
    }
}

/// The policy operation a declared native tool performs with `input`, or `None` when the
/// input does not name what the declaration needs.
#[must_use]
pub fn policy_operation(
    declaration: &NativeToolPolicyDeclaration,
    input: &serde_json::Value,
) -> Option<ToolPolicyOperation> {
    let values: Vec<String> = declaration
        .fields
        .iter()
        .filter_map(|field| input.get(field)?.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .collect();
    Some(match declaration.operation {
        NativeToolOperation::Command => {
            let command = values.into_iter().next()?;
            let request =
                bcode_shell_command_analysis_models::ShellAnalysisRequest::posix(command.clone());
            let (analysis, analysis_error) = match bcode_shell_command_analysis::analyze(&request) {
                Ok(analysis) => (Some(analysis), None),
                Err(error) => (None, Some(error)),
            };
            ToolPolicyOperation::Command {
                command: Some(command),
                analysis,
                analysis_error,
            }
        }
        NativeToolOperation::Read if values.is_empty() => ToolPolicyOperation::ReadOnly,
        NativeToolOperation::Read => ToolPolicyOperation::Read { paths: values },
        NativeToolOperation::Write if values.is_empty() => return None,
        NativeToolOperation::Write => ToolPolicyOperation::Write {
            paths: values,
            category: "edit".to_owned(),
        },
        NativeToolOperation::Web => ToolPolicyOperation::Web {
            url: Some(values.into_iter().next()?),
        },
        NativeToolOperation::ReadOnly => ToolPolicyOperation::ReadOnly,
    })
}

/// The agent's decision for one native approval: `Some(true)` allow, `Some(false)` deny,
/// `None` when the user decides (the policy asks, or the tool or input is not understood).
pub async fn decide(
    state: &std::sync::Arc<crate::ServerState>,
    session_id: SessionId,
    plugin_id: &str,
    approval: &HarnessInteraction,
) -> Option<bool> {
    let tool_name = approval.tool_name.as_deref()?;
    let declaration = state
        .plugins
        .registry()
        .manifests()
        .get(plugin_id)?
        .native_tools
        .iter()
        .find(|declaration| declaration.tool_name == tool_name)?
        .clone();
    let input: serde_json::Value = serde_json::from_str(approval.input_preview.as_deref()?).ok()?;
    let operation = policy_operation(&declaration, &input)?;
    let agent_id = state
        .sessions
        .current_runtime_selection(session_id)
        .await
        .ok()?
        .agent_id
        .unwrap_or_else(|| "build".to_owned());
    let config = state.session_config(session_id).await;
    let cwd = state
        .sessions
        .session_summary(session_id)
        .await
        .ok()
        .map(|summary| summary.working_directory.to_string_lossy().into_owned());
    let request = bcode_agent_profile::EvaluateToolCallRequest {
        session_id,
        agent_id,
        tool_name: tool_name.to_owned(),
        requires_permission: !matches!(operation, ToolPolicyOperation::ReadOnly),
        operation,
        aliases: Vec::new(),
        claimed: false,
        policy_profile: None,
        cwd,
        effective_config_toml: bcode_config::encode_effective_config(&config)
            .ok()
            .map(Box::new),
    };
    let response = state
        .plugins
        .invoke_service_by_interface_json::<_, bcode_agent_profile::EvaluateToolCallResponse>(
            crate::AGENT_PROFILE_INTERFACE_ID,
            bcode_agent_profile::OP_EVALUATE_TOOL_CALL,
            &request,
        )
        .await
        .ok()?;
    match response.decision {
        AgentDecision::Allow => Some(true),
        AgentDecision::Deny => Some(false),
        AgentDecision::Ask => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration(operation: NativeToolOperation, fields: &[&str]) -> NativeToolPolicyDeclaration {
        NativeToolPolicyDeclaration {
            tool_name: "tool".into(),
            operation,
            fields: fields.iter().map(|field| (*field).to_owned()).collect(),
        }
    }

    #[test]
    fn declared_native_tools_become_policy_operations() {
        let command = policy_operation(
            &declaration(NativeToolOperation::Command, &["command"]),
            &serde_json::json!({"command": "cargo test"}),
        );
        assert!(matches!(
            command,
            Some(ToolPolicyOperation::Command { command: Some(text), analysis: Some(_), analysis_error: None })
                if text == "cargo test"
        ));
        assert_eq!(
            policy_operation(
                &declaration(NativeToolOperation::Write, &["file_path"]),
                &serde_json::json!({"file_path": "/repo/src/lib.rs", "content": "x"}),
            ),
            Some(ToolPolicyOperation::Write {
                paths: vec!["/repo/src/lib.rs".into()],
                category: "edit".into(),
            })
        );
        // A write that names no path is not understood, so the user decides.
        assert_eq!(
            policy_operation(
                &declaration(NativeToolOperation::Write, &["file_path"]),
                &serde_json::json!({}),
            ),
            None
        );
        assert_eq!(
            policy_operation(
                &declaration(NativeToolOperation::Read, &["path"]),
                &serde_json::json!({"pattern": "TODO"}),
            ),
            Some(ToolPolicyOperation::ReadOnly)
        );
        assert_eq!(
            policy_operation(
                &declaration(NativeToolOperation::Web, &["url"]),
                &serde_json::json!({"url": "https://example.com"}),
            ),
            Some(ToolPolicyOperation::Web {
                url: Some("https://example.com".into())
            })
        );
    }

    #[test]
    fn approvals_are_decided_once_and_withdrawn_ones_are_dropped() {
        let approvals = NativeApprovals::default();
        let session = SessionId::new();
        let interaction = |request_id: &str| HarnessObservation {
            observation_id: request_id.into(),
            session_id: session.to_string(),
            generation: 1,
            native_correlations: Vec::new(),
            completeness: bcode_harness_models::HarnessCompleteness::Complete,
            kind: HarnessObservationKind::Interaction {
                interaction: HarnessInteraction {
                    request_id: request_id.into(),
                    session_id: session.to_string(),
                    generation: 1,
                    kind: HarnessInteractionKind::Approval,
                    prompt: String::new(),
                    scope: String::new(),
                    choices: Vec::new(),
                    tool_name: Some("Bash".into()),
                    input_preview: None,
                },
            },
        };
        approvals.observed(session, &[interaction("kept"), interaction("withdrawn")]);
        approvals.observed(
            session,
            &[HarnessObservation {
                kind: HarnessObservationKind::InteractionWithdrawn {
                    request_id: "withdrawn".into(),
                },
                ..interaction("withdrawal")
            }],
        );
        let taken = approvals.take(session);
        assert_eq!(
            taken
                .iter()
                .map(|approval| approval.request_id.as_str())
                .collect::<Vec<_>>(),
            ["kept"]
        );
        assert_eq!(approvals.take(session).len(), 0);
    }
}
