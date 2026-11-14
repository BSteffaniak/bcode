//! Host-shell effects the application drivers answer as the daemon would: message submissions
//! and builtin slash commands. Both drivers keep the newest of each when their update queues
//! it and deliver its result through the root model's effect-completion path.

use std::future::Future as _;

use bcode_session_models::{
    ClientId, PromptSubmission, SessionEventKind, SubmissionId, SubmissionPlacement,
    SubmissionReceipt, SubmissionRecord, SubmissionStatus, SubmissionTarget, TurnAdmissionMetadata,
    TurnId,
};

use super::driver::SubmissionOutcome;
use super::session::ScenarioSession;
use crate::effects::{SubmitMessageResult, TuiEffect, TuiEffectResult};

/// The newest submission and slash command the application queued.
#[derive(Default)]
pub struct ShellEffects {
    submission: Option<(SubmissionId, String)>,
    slash: Option<TuiEffect>,
}

impl ShellEffects {
    /// Keep `effect` when it is a submission or a slash command.
    pub fn observe(&mut self, effect: TuiEffect) {
        match effect {
            TuiEffect::SubmitMessage { request } => {
                self.submission = Some((request.submission_id.clone(), request.message.clone()));
            }
            TuiEffect::ExecuteSlashCommand { .. } => self.slash = Some(effect),
            _ => {}
        }
    }

    /// The daemon's answer to the newest submission: accepted into `session` (which the
    /// submission creates when the composer targets a draft), or rejected with a server error.
    ///
    /// Acceptance appends the submission's `SubmissionAccepted` and `SubmissionActivated`
    /// events to the canonical session (the daemon persists them before it answers) and
    /// answers with the production result shape of the model composer: the submission record
    /// and the model receipt. The client then sees the events through history loads and the
    /// live stream, not in the result.
    ///
    /// # Panics
    ///
    /// Panics when no submission was queued.
    pub fn submission_result(
        &mut self,
        session: &mut ScenarioSession,
        outcome: &SubmissionOutcome,
    ) -> TuiEffectResult {
        let (submission_id, message) = self
            .submission
            .take()
            .expect("the scenario submitted a message");
        let result = match outcome {
            SubmissionOutcome::Rejected { code } => Err(bcode_client::ClientError::Server {
                code: code.clone(),
                message: "scenario daemon rejected the submission".to_owned(),
            }),
            SubmissionOutcome::Accepted => {
                let record = accept(session, &submission_id, &message);
                Ok(SubmitMessageResult {
                    session_id: session.id(),
                    created_session: None,
                    receipt: Some(crate::effects::ComposerReceipt::Model(
                        record.receipt.clone(),
                    )),
                    submission: Ok(Some(record)),
                    prepared_request: None,
                    committed_agent_id: None,
                    applied_native_setting: None,
                    committed_reasoning_effort_generation: None,
                    // The created session's event stream: the scenario delivers its events live.
                    event_task: Some(tokio::spawn(std::future::pending())),
                    event_stream_release: None,
                })
            }
        };
        TuiEffectResult::SubmitMessage {
            submission_id,
            message,
            result: Box::new(result),
        }
    }

    /// The result of the newest slash command, executed by the production slash command
    /// executor. Only builtins that need no daemon complete (for example `/version`).
    ///
    /// # Panics
    ///
    /// Panics when no slash command was queued or the command needs a daemon.
    pub fn slash_result(&mut self) -> TuiEffectResult {
        let Some(TuiEffect::ExecuteSlashCommand {
            session_id,
            working_directory,
            current_agent_id,
            reasoning_display_mode,
            reasoning_visible,
            message,
        }) = self.slash.take()
        else {
            panic!("the scenario ran no slash command");
        };
        let client = bcode_client::BcodeClient::default_endpoint()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let future = std::pin::pin!(async {
            let resolution = crate::slash_registry::resolve(&client, &message).await?;
            crate::slash_commands::execute_resolved(
                &client,
                session_id,
                crate::slash_commands::SlashExecutionContext {
                    working_directory: &working_directory,
                    current_agent_id: &current_agent_id,
                    reasoning_display_mode,
                    reasoning_visible,
                },
                &message,
                resolution,
            )
            .await
        });
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        let std::task::Poll::Ready(result) = future.poll(&mut context) else {
            panic!("the scenario slash command needs a daemon");
        };
        TuiEffectResult::SlashCommandExecuted {
            message: message.clone(),
            result,
        }
    }
}

/// Accept `text` as submission `id` into `session` and activate it (an idle session runs it
/// at once), returning the daemon's submission record.
fn accept(session: &mut ScenarioSession, id: &SubmissionId, text: &str) -> SubmissionRecord {
    let accepted_sequence = session.next_sequence();
    let turn_id = TurnId::from_accepted_event(session.id(), accepted_sequence);
    let client_id = ClientId::new();
    let submission = PromptSubmission {
        id: id.clone(),
        client_id,
        text: text.to_owned(),
        admission: TurnAdmissionMetadata::default(),
        requested_placement: SubmissionPlacement::FollowUp,
        target: SubmissionTarget::OwnTurn {
            turn_id: turn_id.clone(),
        },
        execution_descriptor: None,
    };
    session.push_kind(SessionEventKind::SubmissionAccepted {
        submission: submission.clone(),
    });
    let activated = session.push_kind(SessionEventKind::SubmissionActivated {
        submission_id: id.clone(),
        turn_id: turn_id.clone(),
        client_id,
        text: text.to_owned(),
        admission: TurnAdmissionMetadata::default(),
    });
    SubmissionRecord {
        receipt: SubmissionReceipt {
            submission_id: id.clone(),
            accepted_event_sequence: accepted_sequence,
        },
        submission,
        status: SubmissionStatus::Activated {
            turn_id,
            conversation_sequence: activated.sequence,
        },
        context_observation: None,
        recovery_authorization: None,
    }
}
