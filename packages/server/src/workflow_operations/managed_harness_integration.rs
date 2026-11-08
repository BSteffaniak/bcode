//! Production workflow/application integration with canonical session persistence and fake native I/O.
//! This explicitly exercises an existing legacy-family run; fresh representation admission is
//! independently capability-gated and must not be bypassed or described as covered by this test.

use super::*;
use crate::harness::{HarnessOwner, HarnessPluginTransport, ManagedHarnessRuntime};
use bcode_harness_models::*;
use bcode_plugin_sdk::{NativeServiceContext, RustPlugin, ServiceResponse};
use bcode_session_models::SessionId;
use bcode_session_models::external_harness::ExternalHarnessOwner;
use bcode_workflow::{NodeKind, ValueSchema, WorkflowBlockDefinition, WorkflowNodeDataflowPolicy};
use bcode_workflow_store::ActivationDispatchOwner as _;
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};

const PLUGIN: &str = "test.workflow-managed-boundary";
const RUN: &str = "managed-production-boundary";
// static_plugin_vtable! retains one plugin instance per macro callsite, not per host.
// These scenarios must own its entire lifetime, including state reset and host drop.
static FIXTURE_LIFETIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const MANIFEST: &str = r#"
id = "test.workflow-managed-boundary"
name = "Offline workflow integration"
version = "0.0.1"
[[services]]
interface_id = "bcode.agent-profile/v1"
[[services]]
interface_id = "bcode.harness/v3"
[[services]]
interface_id = "bcode.harness.models/v1"
[[services]]
interface_id = "bcode.harness.configuration/v1"
[[services]]
interface_id = "test.workflow-control/v1"
[[services]]
interface_id = "bcode.workflow-block/v1"
[[services.workflow_blocks]]
block_id = "harness.execute"
block_version = 2
plugin_id = "test.workflow-managed-boundary"
operation = "harness.execute"
effect = "mutating"
reconciliation = "receipt_status"
cancellation_supported = true
preparation_required = true
timeout_ms = 30000
[services.workflow_blocks.input]
type_name = "bcode.harness.managed-operation/v1"
schema = { type = "object", additionalProperties = false, required = ["request_json"], properties = { request_json = { type = "string" }, completion_requirement = { type = "string", enum = ["foreground", "associated_work", "async_handoff", "workspace_release"] } } }
[services.workflow_blocks.output]
type_name = "bcode.harness.assignment-result/v2"
schema = { type = "object", required = ["session_id", "submission_id", "native_turn_id", "result"] }
[services.workflow_blocks.authorization]
capability = "mutating"
explicit_grant_required = true
[[services.workflow_blocks]]
block_id = "harness.await"
block_version = 2
plugin_id = "test.workflow-managed-boundary"
operation = "harness.await"
effect = "mutating"
reconciliation = "receipt_status"
cancellation_supported = true
preparation_required = true
timeout_ms = 30000
[services.workflow_blocks.input]
type_name = "bcode.harness.managed-operation/v1"
schema = { type = "object", additionalProperties = false, required = ["request_json"], properties = { request_json = { type = "string" }, completion_requirement = { type = "string", enum = ["associated_work"] } } }
[services.workflow_blocks.output]
type_name = "bcode.harness.assignment-result/v2"
schema = { type = "object", required = ["session_id", "submission_id", "native_turn_id", "result"] }
[services.workflow_blocks.authorization]
capability = "mutating"
explicit_grant_required = true
[concurrency]
type = "exclusive"
[runtime]
type = "native"
abi_version = 5
library = "unused-test-library"
event_symbol = "bcode_plugin_handle_event_v1"
"#;

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
enum EvidencePhase {
    #[default]
    Progress,
    Terminal,
    Quiescent,
}

#[derive(Default)]
struct OfflineHarness {
    submissions: usize,
    released: EvidencePhase,
    emitted: EvidencePhase,
    polls: usize,
    accepted_submission: Option<(String, String)>,
    disposition: Option<HarnessTerminalDisposition>,
    execution_requirement: Option<HarnessCompletionRequirement>,
    background_failed: bool,
    selected: std::collections::BTreeMap<String, SelectedNativeSession>,
    selected_opens: usize,
    selected_submissions: usize,
    selected_unconfirmed: bool,
}

struct SelectedNativeSession {
    open: HarnessOpenRequest,
    instance: HarnessInstance,
    submission: Option<HarnessSubmission>,
    emitted: EvidencePhase,
    accepted: bool,
}

fn response(value: &impl serde::Serialize) -> ServiceResponse {
    ServiceResponse::json(value).expect("fixture response")
}

impl RustPlugin for OfflineHarness {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match context.request.interface_id.as_str() {
            "bcode.harness.configuration/v1" => response(&json!({"default_instance":null,
                "instances":{"native-a":selected_instance("native-a"),"native-b":selected_instance("native-b")}})),
            HARNESS_MODELS_INTERFACE_ID => {
                let request: HarnessModelsRequest = context.request.payload_json().unwrap();
                let kind = self.selected_models(&request);
                response(&HarnessModelsResponse {
                    schema_version: HARNESS_SCHEMA_VERSION,
                    request_id: request.request_id,
                    instance_id: request.instance.instance_id,
                    kind,
                })
            }
            "bcode.agent-profile/v1" => policy(&context),
            "bcode.workflow-block/v1" => preparation(&context),
            "test.workflow-control/v1" => {
                match context.request.operation.as_str() {
                    "selected_unconfirmed" => self.selected_unconfirmed = true,
                    "selected_progress" => self.released = EvidencePhase::Progress,
                    "selected_opens" => return response(&self.selected_opens),
                    "selected_submissions" => return response(&self.selected_submissions),
                    "reset" => *self = Self::default(),
                    "release" => self.released = EvidencePhase::Quiescent,
                    "terminal" => self.released = self.released.max(EvidencePhase::Terminal),
                    "execution" => {
                        self.execution_requirement =
                            Some(HarnessCompletionRequirement::AssociatedWork);
                    }
                    "foreground" => {
                        self.execution_requirement = Some(HarnessCompletionRequirement::Foreground);
                    }
                    "background_failed" => self.background_failed = true,
                    "fail" => self.disposition = Some(HarnessTerminalDisposition::Failed),
                    "interrupt" => self.disposition = Some(HarnessTerminalDisposition::Interrupted),
                    _ => {}
                }
                if context.request.operation == "polls" {
                    response(&self.polls)
                } else {
                    response(&self.submissions)
                }
            }
            HARNESS_INTERFACE_ID => {
                let request: HarnessRequest =
                    context.request.payload_json().expect("native request");
                let kind = if request.instance.instance_id.starts_with("native-") {
                    self.selected_native(&request)
                } else {
                    self.native(&request)
                };
                response(&HarnessServiceResponse {
                    schema_version: HARNESS_SCHEMA_VERSION,
                    request_id: request.request_id,
                    operation: request.kind.operation(),
                    kind,
                })
            }
            _ => panic!("unexpected fixture interface"),
        }
    }
}

fn policy(context: &NativeServiceContext) -> ServiceResponse {
    if context.request.operation == bcode_agent_profile::OP_LIST_AGENTS {
        let _: bcode_agent_profile::AgentListRequest = context.request.payload_json().unwrap();
        response(&bcode_agent_profile::AgentList {
            agents: vec![bcode_agent_profile::AgentInfo {
                id: "build".into(),
                name: "Offline integration build".into(),
                description: "Fixture profile for explicitly approved managed workflow execution"
                    .into(),
                badge: None,
                accent: None,
                aliases: vec![],
                is_default: true,
            }],
        })
    } else if context.request.operation == bcode_agent_profile::OP_RESOLVE_POLICY_PROFILE_IDENTITY {
        let request: bcode_agent_profile::ResolveAgentPolicyProfileIdentityRequest =
            context.request.payload_json().unwrap();
        assert_eq!(request.profile_id, "build");
        response(&bcode_agent_profile::AgentPolicyProfileIdentity {
            version: 1,
            provider_id: PLUGIN.into(),
            profile_id: "build".into(),
            policy_digest_sha256: "a".repeat(64),
        })
    } else if context.request.operation == bcode_agent_profile::OP_AGENT_CONTEXT {
        let request: bcode_agent_profile::AgentContextRequest =
            context.request.payload_json().unwrap();
        assert_eq!(request.agent_id, "build");
        response(&bcode_agent_profile::AgentContextResponse::default())
    } else {
        assert_eq!(
            context.request.operation,
            bcode_agent_profile::OP_EVALUATE_TOOL_CALL
        );
        response(&bcode_agent_profile::EvaluateToolCallResponse {
            decision: bcode_agent_profile::AgentDecision::Allow,
            reason: None,
            shell: None,
        })
    }
}

fn preparation(context: &NativeServiceContext) -> ServiceResponse {
    assert_eq!(
        context.request.operation,
        bcode_workflow::WORKFLOW_BLOCK_PREPARE_OPERATION
    );
    let request: bcode_workflow::WorkflowBlockPreparationRequest =
        context.request.payload_json().unwrap();
    request.validate().unwrap();
    let payload: Value =
        serde_json::from_str(request.input["request_json"].as_str().unwrap()).unwrap();
    let mut descriptor = json!({"interface_id":"bcode.managed-session/v1","operation":request.block.operation,"execution":true,"payload":payload});
    if request.block.operation == "harness.await" {
        descriptor["admission"] = json!("deferred_until_adopted");
    }
    response(&bcode_workflow::WorkflowBlockPreparationResponse {
        version: bcode_workflow::WORKFLOW_BLOCK_PREPARATION_VERSION,
        input_sha256: bcode_workflow::workflow_block_input_sha256(&request.input).unwrap(),
        owner_id: PLUGIN.into(),
        operation_facts: serde_json::to_value(
            bcode_agent_profile::ToolPolicyAuthorizationMetadata {
                requires_permission: true,
                aliases: vec!["harness.execute".into()],
                compatibility_aliases: vec![],
                capabilities: vec![],
                permission_category: None,
                operation: bcode_agent_profile::ToolPolicyOperation::Mutating,
            },
        )
        .unwrap(),
        descriptor,
        diagnostics: vec![],
    })
}

impl OfflineHarness {
    fn execution_attempt(&self) -> HarnessAttemptIdentity {
        let (submission_id, attempt_id) = self
            .accepted_submission
            .as_ref()
            .expect("retained submission");
        HarnessAttemptIdentity {
            submission_id: submission_id.clone(),
            attempt_id: attempt_id.clone(),
        }
    }
}

fn selected_instance(id: &str) -> HarnessInstance {
    HarnessInstance {
        instance_id: id.into(),
        plugin_id: PLUGIN.into(),
        executable: "/fixture/native".into(),
        config_namespace: id.into(),
        auth: HarnessAuthBinding {
            provider_id: "fixture-auth".into(),
            profile: format!("{id}-account"),
            credential_namespace: id.into(),
            custody: HarnessAuthCustody::NativeManaged,
        },
    }
}

impl OfflineHarness {
    fn selected_models(&mut self, request: &HarnessModelsRequest) -> HarnessModelsResponseKind {
        match request.kind.clone() {
            HarnessModelsRequestKind::DiscoverModels => HarnessModelsResponseKind::Models {
                native_version: "fixture-selected-1".into(),
                models: vec![HarnessModel {
                    model_id: "observed-model".into(),
                    display_name: "Observed".into(),
                    description: None,
                }],
            },
            HarnessModelsRequestKind::OpenSelected { open, selection } => {
                assert_eq!(selection.model_id, "observed-model");
                assert!(
                    !self.selected.contains_key(&open.session_id),
                    "selected Open cannot replay"
                );
                self.selected_opens += 1;
                let target = HarnessSessionTarget::Prepared(HarnessPreparedSession {
                    open_id: open.open_id.clone(),
                    session_id: open.session_id.clone(),
                    instance_id: request.instance.instance_id.clone(),
                    generation: 1,
                    native_version: "fixture-selected-1".into(),
                    requested_native_session_id: None,
                });
                self.selected.insert(
                    open.session_id.clone(),
                    SelectedNativeSession {
                        open: *open,
                        instance: request.instance.clone(),
                        submission: None,
                        emitted: EvidencePhase::Progress,
                        accepted: false,
                    },
                );
                HarnessModelsResponseKind::Selected {
                    target,
                    evidence: HarnessModelEvidence {
                        requested_model_id: selection.model_id,
                        effective_model_id: None,
                    },
                }
            }
        }
    }

    fn selected_observation_kinds(
        &mut self,
        query: &HarnessObserveRequest,
    ) -> Vec<HarnessObservationKind> {
        let native = self.selected.get_mut(query.target.session_id()).unwrap();
        let mut kinds = Vec::new();
        if let Some(submission) = &native.submission {
            let exact = HarnessAttemptIdentity {
                submission_id: submission.submission_id.clone(),
                attempt_id: submission.attempt_id.clone(),
            };
            let turn = format!("native-{}", native.open.session_id);
            if !native.accepted {
                native.accepted = true;
                if !self.selected_unconfirmed {
                    kinds.push(HarnessObservationKind::BindingConfirmed {
                        open_id: native.open.open_id.clone(),
                        binding: HarnessSessionBinding {
                            schema_version: HARNESS_SCHEMA_VERSION,
                            session_id: native.open.session_id.clone(),
                            instance_id: native.instance.instance_id.clone(),
                            native_session_id: format!("session-{}", native.open.session_id),
                            native_version: "fixture-selected-1".into(),
                            generation: 1,
                        },
                    });
                }
                kinds.push(HarnessObservationKind::SubmissionAccepted {
                    submission_id: exact.submission_id.clone(),
                    attempt_id: exact.attempt_id.clone(),
                    native_turn_id: turn.clone(),
                });
            }
            if self.released >= EvidencePhase::Terminal && native.emitted < EvidencePhase::Terminal
            {
                native.emitted = EvidencePhase::Terminal;
                kinds.push(HarnessObservationKind::Execution {
                    event: HarnessExecutionEvent::SubmissionTerminal {
                        attempt: exact.clone(),
                        native_turn_id: Some(turn),
                        native_frame_id: None,
                        disposition: HarnessTerminalDisposition::Completed,
                        output: Some(r#"{"ok":true}"#.into()),
                    },
                });
                kinds.push(HarnessObservationKind::Execution {
                    event: HarnessExecutionEvent::BackgroundWork {
                        work_id: "background".into(),
                        native_task_id: None,
                        origin: Some(exact.clone()),
                        parent_work_id: None,
                        work_kind: HarnessWorkKind::Command,
                        label: "background".into(),
                        state: HarnessWorkState::Running,
                        notification: HarnessNotificationState::NotExpected,
                    },
                });
            }
            if self.released == EvidencePhase::Quiescent
                && native.emitted < EvidencePhase::Quiescent
            {
                native.emitted = EvidencePhase::Quiescent;
                kinds.push(HarnessObservationKind::Execution {
                    event: HarnessExecutionEvent::BackgroundWork {
                        work_id: "background".into(),
                        native_task_id: None,
                        origin: Some(exact.clone()),
                        parent_work_id: None,
                        work_kind: HarnessWorkKind::Command,
                        label: "background".into(),
                        state: HarnessWorkState::Completed,
                        notification: HarnessNotificationState::NotExpected,
                    },
                });
                kinds.push(HarnessObservationKind::Execution {
                    event: HarnessExecutionEvent::Coverage {
                        attempt: exact,
                        background: HarnessTrackingCoverage::Complete,
                        continuations: HarnessTrackingCoverage::Complete,
                    },
                });
            }
        }
        kinds
    }

    fn selected_native(&mut self, request: &HarnessRequest) -> HarnessServiceResponseKind {
        match &request.kind {
            HarnessRequestKind::Inspect => {
                HarnessServiceResponseKind::Inspection(HarnessInspection {
                    native_version: "fixture-selected-1".into(),
                    executable_identity: request.instance.executable.clone(),
                    config_namespace: request.instance.config_namespace.clone(),
                    capabilities: capabilities(),
                })
            }
            HarnessRequestKind::AuthStatus { .. } => HarnessServiceResponseKind::AuthStatus(
                bcode_provider_auth_models::managed::ManagedAuthStatus {
                    namespace: bcode_provider_auth_models::managed::AuthNamespace {
                        plugin_id: PLUGIN.into(),
                        credential_namespace: request.instance.auth.credential_namespace.clone(),
                    },
                    custody: request.instance.auth.custody,
                    readiness: bcode_provider_auth_models::managed::AuthReadiness::Ready,
                },
            ),
            HarnessRequestKind::Submit { submission, .. } => {
                let native = self.selected.get_mut(&submission.session_id).unwrap();
                assert!(
                    native.submission.is_none(),
                    "selected submission cannot replay"
                );
                native.submission = Some(submission.clone());
                self.selected_submissions += 1;
                HarnessServiceResponseKind::Receipt(HarnessReceipt {
                    submission_id: submission.submission_id.clone(),
                    attempt_id: submission.attempt_id.clone(),
                    session_id: submission.session_id.clone(),
                    generation: 1,
                    state: HarnessDeliveryState::NativeAccepted,
                    native_request_id: None,
                })
            }
            HarnessRequestKind::Observe(query) => {
                let kinds = self.selected_observation_kinds(query);
                let observations = kinds
                    .into_iter()
                    .enumerate()
                    .map(|(index, kind)| HarnessObservation {
                        observation_id: format!(
                            "{}-{}-{index}",
                            query.target.session_id(),
                            self.polls
                        ),
                        session_id: query.target.session_id().to_owned(),
                        generation: 1,
                        native_correlations: Vec::new(),
                        completeness: HarnessCompleteness::Complete,
                        kind,
                    })
                    .collect();
                self.polls += 1;
                HarnessServiceResponseKind::Observations(HarnessObservationPage {
                    live: Vec::new(),
                    retention: None,
                    hydration_requests: Vec::new(),
                    inventory_page: None,
                    observations,
                    next_after: Some(format!("selected-{}", self.polls)),
                    completeness: HarnessCompleteness::Complete,
                    status: HarnessExecutionStatus {
                        connectivity: HarnessConnectivity::Connected,
                        activity: HarnessActivity::Running,
                        attention: HarnessAttention::None,
                        continuation: HarnessContinuation::Unavailable,
                        completeness: HarnessCompleteness::Complete,
                    },
                })
            }
            HarnessRequestKind::Close(control) | HarnessRequestKind::Interrupt(control) => {
                HarnessServiceResponseKind::Acknowledged {
                    mutation_id: control.mutation_id.clone(),
                }
            }
            _ => panic!("selected fixture forbids recovery, fallback or unrelated operations"),
        }
    }

    fn execution_work(&self, state: HarnessWorkState) -> HarnessExecutionEvent {
        HarnessExecutionEvent::BackgroundWork {
            work_id: "adapter-command".into(),
            native_task_id: Some("native-command".into()),
            origin: Some(self.execution_attempt()),
            parent_work_id: None,
            work_kind: HarnessWorkKind::Command,
            label: "fixture command".into(),
            state,
            notification: HarnessNotificationState::NotExpected,
        }
    }

    fn execution_events(
        &self,
        events: Vec<(&'static str, HarnessObservationKind)>,
    ) -> Vec<(&'static str, HarnessObservationKind)> {
        if self.execution_requirement.is_none() {
            return events;
        }
        let quiescent = events.iter().any(|(id, _)| *id == "settled");
        let mut events: Vec<_> = events
            .into_iter()
            .filter_map(|(id, kind)| {
                let kind = match kind {
                    HarnessObservationKind::Settlement { .. } => return None,
                    HarnessObservationKind::NativeTurnTerminal { disposition, .. } => {
                        HarnessObservationKind::Execution {
                            event: HarnessExecutionEvent::SubmissionTerminal {
                                attempt: self.execution_attempt(),
                                native_turn_id: None,
                                native_frame_id: Some("native-result-frame".into()),
                                disposition,
                                output: (disposition == HarnessTerminalDisposition::Completed)
                                    .then(|| "verified native result".into()),
                            },
                        }
                    }
                    other => other,
                };
                Some((id, kind))
            })
            .collect();
        if quiescent {
            events.push((
                "work-ended",
                HarnessObservationKind::Execution {
                    event: self.execution_work(if self.background_failed {
                        HarnessWorkState::Failed
                    } else {
                        HarnessWorkState::Completed
                    }),
                },
            ));
            events.push((
                "coverage",
                HarnessObservationKind::Execution {
                    event: HarnessExecutionEvent::Coverage {
                        attempt: self.execution_attempt(),
                        background: HarnessTrackingCoverage::Complete,
                        continuations: HarnessTrackingCoverage::Complete,
                    },
                },
            ));
        }
        events
    }

    fn native(&mut self, request: &HarnessRequest) -> HarnessServiceResponseKind {
        request.validate().unwrap();
        match &request.kind {
            HarnessRequestKind::Open(open) => {
                HarnessServiceResponseKind::Prepared(HarnessPreparedSession {
                    open_id: open.open_id.clone(),
                    session_id: open.session_id.clone(),
                    instance_id: request.instance.instance_id.clone(),
                    generation: open.generation,
                    native_version: "fixture-1".into(),
                    requested_native_session_id: None,
                })
            }
            HarnessRequestKind::Submit { target, submission } => {
                assert!(matches!(target, HarnessSessionTarget::Prepared(_)));
                assert_eq!(
                    self.submissions, 0,
                    "one fixture scenario must never redeliver its native submission"
                );
                self.submissions += 1;
                self.accepted_submission = Some((
                    submission.submission_id.clone(),
                    submission.attempt_id.clone(),
                ));
                HarnessServiceResponseKind::Receipt(HarnessReceipt {
                    submission_id: submission.submission_id.clone(),
                    attempt_id: submission.attempt_id.clone(),
                    session_id: submission.session_id.clone(),
                    generation: submission.generation,
                    state: HarnessDeliveryState::NativeAccepted,
                    native_request_id: Some("native-request".into()),
                })
            }
            HarnessRequestKind::Observe(query) => self.page(query),
            _ => panic!("fixture must not invoke model/auth/control operations"),
        }
    }

    fn page(&mut self, query: &HarnessObserveRequest) -> HarnessServiceResponseKind {
        self.polls += 1;
        let binding = HarnessSessionBinding {
            schema_version: HARNESS_SCHEMA_VERSION,
            session_id: query.target.session_id().into(),
            instance_id: query.target.instance_id().into(),
            native_session_id: "native-session".into(),
            native_version: "fixture-1".into(),
            generation: query.target.generation(),
        };
        let mut events = Vec::new();
        if query.after.is_none() {
            events.push((
                "confirmed",
                HarnessObservationKind::BindingConfirmed {
                    open_id: "open".into(),
                    binding,
                },
            ));
            let (submission_id, attempt_id) = self
                .accepted_submission
                .as_ref()
                .expect("observe follows native submit");
            events.push((
                "accepted",
                HarnessObservationKind::SubmissionAccepted {
                    submission_id: submission_id.clone(),
                    attempt_id: attempt_id.clone(),
                    native_turn_id: "native-turn".into(),
                },
            ));
            if self.execution_requirement.is_some() {
                events.push((
                    "work-started",
                    HarnessObservationKind::Execution {
                        event: self.execution_work(HarnessWorkState::Running),
                    },
                ));
            }
            if self.execution_requirement.is_none()
                && self
                    .disposition
                    .unwrap_or(HarnessTerminalDisposition::Completed)
                    == HarnessTerminalDisposition::Completed
            {
                events.push((
                    "result",
                    HarnessObservationKind::Result {
                        text: "verified native result".into(),
                    },
                ));
            }
        }
        if self.released >= EvidencePhase::Terminal && self.emitted < EvidencePhase::Terminal {
            self.emitted = EvidencePhase::Terminal;
            events.push((
                "terminal",
                HarnessObservationKind::NativeTurnTerminal {
                    native_turn_id: "native-turn".into(),
                    disposition: self
                        .disposition
                        .unwrap_or(HarnessTerminalDisposition::Completed),
                },
            ));
        }
        if self.released == EvidencePhase::Quiescent && self.emitted < EvidencePhase::Quiescent {
            self.emitted = EvidencePhase::Quiescent;
            events.push((
                "settled",
                HarnessObservationKind::Settlement {
                    native_turn_id: "native-turn".into(),
                },
            ));
        }
        let events = self.execution_events(events);
        self.observation_page(query, events)
    }

    fn observation_page(
        &self,
        query: &HarnessObserveRequest,
        events: Vec<(&str, HarnessObservationKind)>,
    ) -> HarnessServiceResponseKind {
        let observations: Vec<_> = events
            .into_iter()
            .map(|(id, kind)| HarnessObservation {
                observation_id: id.into(),
                session_id: query.target.session_id().into(),
                generation: query.target.generation(),
                native_correlations: vec!["native-turn".into()],
                completeness: HarnessCompleteness::Complete,
                kind,
            })
            .collect();
        HarnessServiceResponseKind::Observations(HarnessObservationPage {
            live: Vec::new(),
            retention: None,
            hydration_requests: vec![],
            inventory_page: None,
            next_after: Some(observations.last().map_or_else(
                || format!("idle-{}", self.polls),
                |event| event.observation_id.clone(),
            )),
            observations,
            completeness: HarnessCompleteness::Complete,
            status: HarnessExecutionStatus {
                connectivity: HarnessConnectivity::Connected,
                activity: HarnessActivity::Idle,
                attention: HarnessAttention::None,
                continuation: HarnessContinuation::Available,
                completeness: HarnessCompleteness::Complete,
            },
        })
    }
}

fn capabilities() -> HarnessCapabilities {
    let supported = HarnessCapability {
        support: HarnessSupportLevel::Supported,
        constraints: vec![],
    };
    HarnessCapabilities {
        model_switch: bcode_harness_models::HarnessCapability::unsupported(),
        permission_mode: bcode_harness_models::HarnessCapability::unsupported(),
        resume: supported.clone(),
        steering: supported.clone(),
        approval: supported.clone(),
        questions: supported.clone(),
        assistance: supported.clone(),
        interrupt: supported.clone(),
        authoritative_settlement: supported.clone(),
        background_continuation: supported.clone(),
        authentication: supported,
    }
}

fn fixture(root: &std::path::Path) -> Arc<ServerState> {
    fixture_with_workflow_family(root, false)
}

fn fixture_with_workflow_family(root: &std::path::Path, production: bool) -> Arc<ServerState> {
    let fingerprint = bcode_ipc::BUILD_FINGERPRINT.to_owned();
    let sessions = bcode_session::SessionManager::persistent_with_metrics_and_lease_owner(
        root,
        bcode_metrics::MetricsRegistry::disabled(),
        bcode_session::lease::SessionLeaseOwnerContext {
            build_fingerprint: Some(fingerprint),
            daemon_instance_id: Some("test-workflow-daemon".into()),
            ..bcode_session::lease::SessionLeaseOwnerContext::default()
        },
    )
    .unwrap();
    let plugin = bcode_plugin::StaticBundledPlugin::new(
        MANIFEST,
        bcode_plugin_sdk::static_plugin_vtable!(OfflineHarness, MANIFEST),
    );
    let provider = bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../../plugins/fake-provider-plugin/bcode-plugin.toml"),
        bcode_fake_provider_plugin::static_plugin(),
    );
    let plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::Explicit,
            enabled: BTreeSet::from([PLUGIN.into(), "bcode.fake-provider".into()]),
            disabled: BTreeSet::new(),
        },
        &[plugin, provider],
    )
    .unwrap();
    let mut state = crate::tests::test_server_state_with_plugins(sessions, plugins);
    state.state_root = root.into();
    state.workflow_store = std::sync::Mutex::new(if production {
        bcode_workflow_store::WorkflowStore::create_coexistence_baseline(root).unwrap()
    } else {
        bcode_workflow_store::WorkflowStore::open_in_state_dir(root).unwrap()
    })
    .into();
    state.selected_provider_plugin_id = Some("bcode.fake-provider".into());
    state.selected_model_id = Some("fake-echo".into());
    state.selected_provider_context.settings.insert(
        "fake_structured_output_json".into(),
        r#"{"ok":true}"#.into(),
    );
    let state = Arc::new(state);
    crate::harness::install_canonical_backend(&state).unwrap();
    state
}

async fn opened_child(
    state: &Arc<ServerState>,
    parent: SessionId,
    root: &std::path::Path,
) -> (HarnessRequest, HarnessSessionTarget) {
    let owner = ExternalHarnessOwner {
        session_id: SessionId::new(),
        generation: 1,
        artifact_id: state.daemon_status.build_fingerprint.clone(),
        daemon_id: state.daemon_status.instance_id.clone(),
    };
    let request = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "open".into(),
        instance: HarnessInstance {
            instance_id: "instance".into(),
            plugin_id: PLUGIN.into(),
            executable: "/never-spawned".into(),
            config_namespace: "isolated".into(),
            auth: HarnessAuthBinding {
                provider_id: "fixture".into(),
                profile: "native".into(),
                credential_namespace: "isolated".into(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        },
        kind: HarnessRequestKind::Open(HarnessOpenRequest {
            open_id: "open".into(),
            session_id: owner.session_id.to_string(),
            generation: 1,
            working_directory: root.to_str().unwrap().into(),
            resume: None,
            parent_assistance: None,
        }),
    };
    state
        .sessions
        .prepare_external_harness_open(&owner, &request, capabilities(), Some(parent))
        .await
        .unwrap();
    let runtime = ManagedHarnessRuntime::new(
        crate::harness::CanonicalHarnessStore::new(state),
        HarnessPluginTransport::new(Arc::clone(state)),
    );
    let HarnessRequestKind::Open(open) = &request.kind else {
        panic!("open")
    };
    // Session persistence uses the lease build fingerprint; runtime admission uses the
    // executable ArtifactId and canonical adapter translates it after verifying the daemon.
    let reply = runtime
        .open(
            &HarnessOwner {
                session_id: owner.session_id,
                generation: 1,
                artifact_id: current_artifact_id(state),
                daemon_id: owner.daemon_id,
            },
            open.clone(),
        )
        .await
        .unwrap();
    drop(runtime);
    let HarnessServiceResponseKind::Prepared(prepared) = reply.kind else {
        panic!("prepared")
    };
    (request, HarnessSessionTarget::Prepared(prepared))
}

fn definition(block: &WorkflowBlockDefinition) -> bcode_workflow::WorkflowDefinition {
    let envelope = ValueSchema {
        type_name: "mixed-envelope".into(),
        schema: json!({"type":"object"}),
    };
    let mut graph = bcode_workflow::WorkflowBuilder::new(
        "managed-mixed",
        bcode_workflow::Step::<Value, Value>::input("before")
            .then(bcode_workflow::Step::<Value, Value>::input("external"))
            .then(bcode_workflow::Step::<Value, Value>::input("after")),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    graph.input = envelope.clone();
    graph.output = envelope.clone();
    for node in graph.nodes.values_mut() {
        node.input = envelope.clone();
        node.output = envelope.clone();
    }
    let external = graph
        .nodes
        .values_mut()
        .find(|node| node.name == "external")
        .unwrap();
    external.kind = NodeKind::PluginBlock;
    external.dataflow = WorkflowNodeDataflowPolicy::StateEnvelopeV1;
    external.configuration = serde_json::to_value(block).unwrap();
    graph.validate().unwrap();
    graph
}

fn start_request(
    parent: SessionId,
    input: Value,
    root: &std::path::Path,
) -> bcode_workflow::WorkflowRunStartRequest {
    bcode_workflow::WorkflowRunStartRequest {
        definition_id: "managed-mixed".into(),
        definition_version: 1,
        run_id: Some(RUN.into()),
        workspace_snapshot: root.to_str().unwrap().into(),
        parent_session_id: parent,
        parent_session_generation: None,
        binding: None,
        input: Some(input),
        limits: bcode_workflow::WorkflowRunLimits::default(),
    }
}

// Existing legacy-family identity is deliberate. This tests real start/retry admission rather
// than falsely claiming fresh mutating-block support in the newer representation capability gate.
fn retain_legacy_run(
    state: &ServerState,
    request: &bcode_workflow::WorkflowRunStartRequest,
    graph: &bcode_workflow::WorkflowDefinition,
) {
    let mut store = state.workflow_store.lock().unwrap();
    store
        .persist_definition(&request.definition_id, request.definition_version, graph)
        .unwrap();
    store
        .create_run(&bcode_workflow_store::NewWorkflowRun {
            run_id: RUN.into(),
            definition_id: request.definition_id.clone(),
            definition_version: 1,
            workspace_snapshot: request.workspace_snapshot.clone(),
            parent_session_id: Some(request.parent_session_id.to_string()),
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: request.input.clone(),
            execution_authority: Some(crate::tests::test_workflow_execution_authority()),
            created_at_ms: crate::current_unix_millis(),
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: PLUGIN.into(),
                profile_id: "build".into(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: request.limits.clone(),
        })
        .unwrap();
    drop(store);
}

async fn resolve_gate(state: &Arc<ServerState>, name: &str, value: Value) {
    let waits = state
        .workflow_store
        .lock()
        .unwrap()
        .waiting_activations(RUN, 8)
        .unwrap();
    let definition = state
        .workflow_store
        .lock()
        .unwrap()
        .definition("managed-mixed", 1)
        .unwrap()
        .unwrap();
    let graph: bcode_workflow::WorkflowDefinition =
        serde_json::from_str(&definition.definition_json).unwrap();
    let node = graph.nodes.values().find(|node| node.name == name).unwrap();
    let wait = waits.iter().find(|wait| wait.node_id == node.id).unwrap();
    provide_input(state, RUN, &wait.node_id, &wait.activation_id, value)
        .await
        .unwrap();
}

async fn control(state: &ServerState, operation: &str) -> usize {
    state
        .plugins
        .invoke_service_json(PLUGIN, "test.workflow-control/v1", operation, &json!({}))
        .await
        .unwrap()
}

async fn selected_prompt_graph(
    state: &Arc<ServerState>,
    parent: SessionId,
    worker_instance: &str,
) -> bcode_workflow::WorkflowDefinition {
    let schema = ValueSchema {
        type_name: "selected-result/v1".into(),
        schema: json!({"type":"object",
            "additionalProperties":false,"required":["ok"],"properties":{"ok":{"type":"boolean"}}}),
    };
    let inherited = crate::session_model_selection(state, parent).await;
    let make_node = |id: &str, instance: &str| {
        let binding = selected_instance(instance);
        let selection = if instance == "provider" {
            bcode_workflow::ExecutionModelSelection {
                model_id: inherited.model_id.clone().unwrap(),
                route: Some(bcode_model::execution::ExecutionRoute::ModelProvider {
                    provider_plugin_id: inherited.provider_plugin_id.clone().unwrap(),
                }),
                account_reference: None,
            }
        } else {
            bcode_workflow::ExecutionModelSelection {
                model_id: "observed-model".into(),
                route: Some(bcode_model::execution::ExecutionRoute::ManagedHarness {
                    instance_id: instance.into(),
                    plugin_id: PLUGIN.into(),
                }),
                account_reference: Some(binding.auth.profile),
            }
        };
        let configuration = bcode_workflow::WorkflowPromptConfiguration::structured(
            "build",
            schema.clone(),
            "Return the requested JSON",
        )
        .with_execution_model_selection(selection);
        bcode_workflow::NodeDefinition {
            id: id.into(),
            name: id.into(),
            kind: NodeKind::Agent,
            dataflow: WorkflowNodeDataflowPolicy::Direct,
            input: schema.clone(),
            output: schema.clone(),
            resources: Vec::new(),
            configuration: serde_json::to_value(configuration).unwrap(),
        }
    };
    let graph = bcode_workflow::WorkflowDefinition {
        schema_version: bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION,
        name: "selected-coordinator-worker".into(),
        input: schema.clone(),
        output: schema.clone(),
        nodes: std::collections::BTreeMap::from([
            ("coordinator".into(), make_node("coordinator", "native-a")),
            (
                "worker".into(),
                make_node(
                    "worker",
                    if worker_instance == "unconfirmed" {
                        "native-a"
                    } else {
                        worker_instance
                    },
                ),
            ),
        ]),
        entries: vec!["coordinator".into()],
        exits: vec!["worker".into()],
        edges: vec![bcode_workflow::EdgeDefinition {
            from: "coordinator".into(),
            to: "worker".into(),
            kind: bcode_workflow::EdgeKind::Direct,
            transform: None,
        }],
    };
    graph.validate().unwrap();
    graph
}

async fn poll_selected_node(
    state: &Arc<ServerState>,
    session: SessionId,
    dispatch_identity: &str,
    phase: &str,
    caller: &crate::harness_dispatch::ManagedCaller,
) {
    let canonical = state
        .sessions
        .session_external_harness_state(session)
        .await
        .unwrap()
        .unwrap();
    let poll = bcode_ipc::harness::ManagedSessionCall {
        session_id: Some(session),
        parent_session_id: None,
        assistance_destination_session_id: None,
        harness: HarnessRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: format!("{dispatch_identity}:{phase}"),
            instance: canonical.instance,
            kind: HarnessRequestKind::Observe(HarnessObserveRequest {
                target: canonical.target.unwrap(),
                after: canonical.observation_after,
                limit: 128,
                wait_millis: 0,
                retention: None,
            }),
        },
    };
    state
        .managed_sessions
        .get()
        .unwrap()
        .execute(caller, poll)
        .await
        .unwrap();
}

async fn settle_selected_node(
    state: &Arc<ServerState>,
    session: SessionId,
    prepared: &bcode_workflow_store::PreparedActivationDispatch,
    reconciliation: &bcode_workflow_store::AttemptReconciliationRequest,
    worker_instance: &str,
) -> bool {
    // Drive only the admitted canonical observation boundary, never native transport.
    let canonical = state
        .sessions
        .session_external_harness_state(session)
        .await
        .unwrap()
        .unwrap();
    let caller = crate::harness_dispatch::ManagedCaller::Workflow {
        run_id: RUN.into(),
        dispatch_identity: prepared.dispatch_identity.clone(),
        session_id: session,
        plugin_id: PLUGIN.into(),
    };
    for phase in ["terminal", "release"] {
        control(state, phase).await;
        poll_selected_node(state, session, &prepared.dispatch_identity, phase, &caller).await;
        let observation = crate::harness_workflow::observe_selected_prompt(state, reconciliation)
            .await
            .unwrap();
        if worker_instance == "unconfirmed" {
            assert!(matches!(
                observation,
                bcode_workflow_store::AttemptObservation::Deferred { .. }
            ));
            assert!(
                !state
                    .workflow_store
                    .lock()
                    .unwrap()
                    .pending_activations_for_run(RUN, 8)
                    .unwrap()
                    .iter()
                    .any(|pending| pending.node_id == "worker")
            );
            if phase == "release" {
                assert_eq!(control(state, "selected_opens").await, 1);
                assert_eq!(control(state, "selected_submissions").await, 1);
                state
                    .managed_sessions
                    .get()
                    .unwrap()
                    .shutdown()
                    .await
                    .unwrap();
                return true;
            }
        } else if phase == "terminal" {
            assert!(matches!(
                observation,
                bcode_workflow_store::AttemptObservation::Deferred { .. }
            ));
        } else {
            assert!(
                matches!(&observation, bcode_workflow_store::AttemptObservation::Succeeded { output } if output.value == json!({"ok":true}))
            );
            let authority = state
                .workflow_store
                .lock()
                .unwrap()
                .execution_authority(RUN)
                .unwrap()
                .unwrap();
            state
                .workflow_store
                .lock()
                .unwrap()
                .apply_owned_attempt_observation(
                    &prepared.dispatch_identity,
                    &authority,
                    observation,
                    crate::current_unix_millis(),
                )
                .unwrap();
        }
    }
    let _ = canonical;
    // Reset only release phase, preserving retained per-session mutation counters.
    state
        .plugins
        .invoke_service_json::<_, usize>(
            PLUGIN,
            "test.workflow-control/v1",
            "selected_progress",
            &json!({}),
        )
        .await
        .unwrap();
    false
}

async fn dispatch_selected_node(
    state: &Arc<ServerState>,
    id: &str,
) -> (
    bcode_workflow_store::PreparedActivationDispatch,
    bcode_workflow::DispatchSideEffect,
    Value,
) {
    let pending = state
        .workflow_store
        .lock()
        .unwrap()
        .pending_activations_for_run(RUN, 8)
        .unwrap()
        .into_iter()
        .find(|pending| pending.node_id == id)
        .expect("dependency activated exactly once");
    let plan = crate::WorkflowPromptTurnOwner { state }
        .plan(&pending)
        .await
        .unwrap()
        .unwrap();
    let prepared = state
        .workflow_store
        .lock()
        .unwrap()
        .prepare_pending_activation(
            RUN,
            id,
            &pending.activation_id,
            plan.side_effect,
            plan.intent,
            crate::current_unix_millis(),
        )
        .unwrap()
        .unwrap();
    let receipt = crate::dispatch_workflow_prompt_turn(state, &prepared)
        .await
        .unwrap();
    (prepared, plan.side_effect, receipt)
}

async fn drive_selected_provider(state: &Arc<ServerState>) {
    for _ in 0..16 {
        crate::drive_workflow_run(state, RUN).await.unwrap();
        if crate::workflow_representation::status(state, RUN)
            .await
            .unwrap()
            .unwrap()
            .status
            == bcode_workflow::RunStatus::Completed
        {
            break;
        }
        tokio::task::yield_now().await;
    }
}

fn selected_workflow_database(state: &ServerState) -> switchy_database::rusqlite::RusqliteDatabase {
    let path = crate::workflow_store_path(state);
    switchy_database_connection::open_sqlite_rusqlite(
        &path,
        &switchy_database_connection::SqliteOpenOptions {
            access: switchy_database_connection::SqliteAccess::ReadOnly,
            allow_uri: false,
            busy_timeout: std::time::Duration::ZERO,
            foreign_keys: true,
            connections: std::num::NonZeroUsize::MIN,
        },
    )
    .unwrap()
}

async fn start_selected_production_graph(
    state: &Arc<ServerState>,
    graph: &bcode_workflow::WorkflowDefinition,
    request: bcode_workflow::WorkflowRunStartRequest,
) {
    state
        .workflow_store
        .lock()
        .unwrap()
        .persist_definition(&request.definition_id, request.definition_version, graph)
        .unwrap();
    start_run(state, request, None).await.unwrap();
}

#[tokio::test]
async fn selected_prompt_fresh_production_admission_and_dependencies() {
    use bcode_workflow_store::representation::{activity, runs};
    let _fixture_lifetime = FIXTURE_LIFETIME.lock().await;
    for worker in ["native-a", "native-b", "provider", "unconfirmed"] {
        let root = tempfile::tempdir().unwrap();
        let state = fixture_with_workflow_family(root.path(), true);
        control(&state, "reset").await;
        if worker == "unconfirmed" {
            control(&state, "selected_unconfirmed").await;
        }
        let parent = state
            .sessions
            .create_session(None, root.path().into())
            .await
            .unwrap();
        let graph = selected_prompt_graph(&state, parent.id, worker).await;
        validate_workflow_definition_for_production(&state, &graph).unwrap();
        let request = start_request(parent.id, json!({"ok":true}), root.path());
        start_selected_production_graph(&state, &graph, request).await;
        let db = selected_workflow_database(&state);
        for id in ["coordinator", "worker"] {
            if worker == "unconfirmed" && id == "worker" {
                break;
            }
            if worker == "provider" && id == "worker" {
                drive_selected_provider(&state).await;
                continue;
            }
            let page = activity::page(&db, RUN, None, activity::Section::Attempts, None, 16)
                .await
                .unwrap();
            let attempt = page
                .attempts
                .iter()
                .find(|attempt| attempt.node_id == id)
                .expect("production dependency dispatched");
            let receipt = crate::workflow_representation::owner::execution_receipt(
                &state,
                RUN,
                &attempt.dispatch_identity,
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(
                receipt.receipt["owner"],
                crate::harness_workflow::SELECTED_PROMPT_OWNER
            );
            let session = attempt
                .session
                .as_ref()
                .unwrap()
                .session_id
                .parse()
                .unwrap();
            let caller = crate::harness_dispatch::ManagedCaller::Workflow {
                run_id: RUN.into(),
                dispatch_identity: attempt.dispatch_identity.clone(),
                session_id: session,
                plugin_id: PLUGIN.into(),
            };
            for phase in ["terminal", "release"] {
                control(&state, phase).await;
                poll_selected_node(&state, session, &attempt.dispatch_identity, phase, &caller)
                    .await;
                crate::drive_workflow_run(&state, RUN).await.unwrap();
                let current = activity::page(&db, RUN, None, activity::Section::Attempts, None, 16)
                    .await
                    .unwrap();
                if worker == "unconfirmed" || (id == "coordinator" && phase == "terminal") {
                    assert!(
                        !current
                            .attempts
                            .iter()
                            .any(|attempt| attempt.node_id == "worker")
                    );
                }
            }
            control(&state, "selected_progress").await;
        }
        let summary = runs::summary(&db, RUN).await.unwrap().unwrap();
        if worker == "unconfirmed" {
            assert_ne!(summary.status, bcode_workflow::RunStatus::Completed);
            assert_eq!(control(&state, "selected_submissions").await, 1);
        } else {
            assert_eq!(summary.status, bcode_workflow::RunStatus::Completed);
            let expected = if worker == "provider" { 1 } else { 2 };
            assert_eq!(control(&state, "selected_opens").await, expected);
            assert_eq!(control(&state, "selected_submissions").await, expected);
        }
        state
            .managed_sessions
            .get()
            .unwrap()
            .shutdown()
            .await
            .unwrap();
        drop(state);
    }
}

#[tokio::test]
async fn selected_prompt_same_and_mixed_harness_dependencies_use_canonical_lifecycle() {
    let _fixture_lifetime = FIXTURE_LIFETIME.lock().await;
    'scenarios: for worker_instance in ["native-a", "native-b", "provider", "unconfirmed"] {
        let root = tempfile::tempdir().unwrap();
        let state = fixture(root.path());
        control(&state, "reset").await;
        if worker_instance == "unconfirmed" {
            control(&state, "selected_unconfirmed").await;
        }
        let parent = state
            .sessions
            .create_session(None, root.path().into())
            .await
            .unwrap();
        let inherited = crate::session_model_selection(&state, parent.id).await;
        let graph = selected_prompt_graph(&state, parent.id, worker_instance).await;
        let request = start_request(parent.id, json!({"ok":true}), root.path());
        retain_legacy_run(&state, &request, &graph);
        for id in ["coordinator", "worker"] {
            let (prepared, side_effect, receipt) = dispatch_selected_node(&state, id).await;
            if id == "worker" && worker_instance == "provider" {
                assert_eq!(receipt["owner"], crate::workflow_receipts::AGENT_TURN_OWNER);
                drive_selected_provider(&state).await;
                assert_eq!(
                    crate::session_model_selection(&state, parent.id)
                        .await
                        .provider_context,
                    inherited.provider_context
                );
                continue;
            }
            assert_eq!(
                receipt["owner"],
                crate::harness_workflow::SELECTED_PROMPT_OWNER
            );
            let session: SessionId = receipt["session_id"].as_str().unwrap().parse().unwrap();
            let reconciliation = bcode_workflow_store::AttemptReconciliationRequest {
                run_id: RUN.into(),
                node_id: id.into(),
                activation_id: prepared.activation.activation_id.clone(),
                attempt: prepared.attempt,
                dispatch_identity: prepared.dispatch_identity.clone(),
                side_effect,
                receipt,
            };
            let observation =
                crate::harness_workflow::observe_selected_prompt(&state, &reconciliation)
                    .await
                    .unwrap();
            assert!(matches!(
                observation,
                bcode_workflow_store::AttemptObservation::Deferred { .. }
            ));
            if id == "coordinator" {
                assert!(
                    !state
                        .workflow_store
                        .lock()
                        .unwrap()
                        .pending_activations_for_run(RUN, 8)
                        .unwrap()
                        .iter()
                        .any(|pending| pending.node_id == "worker")
                );
            }
            if settle_selected_node(&state, session, &prepared, &reconciliation, worker_instance)
                .await
            {
                continue 'scenarios;
            }
        }
        let expected_native = if worker_instance == "provider" { 1 } else { 2 };
        assert_eq!(control(&state, "selected_opens").await, expected_native);
        assert_eq!(
            control(&state, "selected_submissions").await,
            expected_native
        );
        assert_eq!(
            state
                .workflow_store
                .lock()
                .unwrap()
                .run_summary(RUN)
                .unwrap()
                .unwrap()
                .status,
            bcode_workflow_store::RunStatus::Completed
        );
        state
            .managed_sessions
            .get()
            .unwrap()
            .shutdown()
            .await
            .unwrap();
        drop(state);
    }
}

#[tokio::test]
async fn managed_workflow_production_dispatch_preserves_state_until_authoritative_settlement() {
    Box::pin(check_production_dispatch(
        HarnessTerminalDisposition::Completed,
    ))
    .await;
}

#[tokio::test]
async fn managed_workflow_production_failed_turn_settles_without_result() {
    Box::pin(check_production_dispatch(
        HarnessTerminalDisposition::Failed,
    ))
    .await;
}

#[tokio::test]
async fn managed_workflow_production_interrupted_turn_settles_without_result() {
    Box::pin(check_production_dispatch(
        HarnessTerminalDisposition::Interrupted,
    ))
    .await;
}

#[tokio::test]
async fn async_adoption_retains_one_native_effect_and_workspace_until_destination_settles() {
    Box::pin(check_async_adoption()).await;
}

fn adoption_submission(child: SessionId) -> HarnessSubmission {
    HarnessSubmission {
        submission_id: "submission".into(),
        attempt_id: "attempt".into(),
        session_id: child.to_string(),
        generation: 1,
        text: "single adopted task".into(),
        placement: HarnessPlacement::NextTurn,
    }
}

async fn check_async_adoption() {
    let _fixture_lifetime = FIXTURE_LIFETIME.lock().await;
    let root = tempfile::tempdir().unwrap();
    let state = fixture(root.path());
    assert_eq!(control(&state, "reset").await, 0);
    assert_eq!(control(&state, "execution").await, 0);
    let parent = state
        .sessions
        .create_session(None, root.path().into())
        .await
        .unwrap();
    let (opening, target) = Box::pin(opened_child(&state, parent.id, root.path())).await;
    let child: SessionId = target.session_id().parse().unwrap();
    let submission = adoption_submission(child);
    let encoded = serde_json::to_string(&json!({"session_id":child,"parent_session_id":null,
        "assistance_destination_session_id":null,"harness":{"schema_version":1,"request_id":"submission",
        "instance":opening.instance,"kind":{"operation":"submit","request":{"target":target,"submission":submission}}}})).unwrap();
    let input = json!({"schema_version":1,"state":{"normal_predecessor":"retained"},
        "value":{"request_json":encoded,"completion_requirement":"async_handoff"}});
    let blocks = state.plugins.registry().workflow_blocks();
    let source_block = blocks
        .iter()
        .find(|b| b.operation == "harness.execute")
        .unwrap();
    let await_block = blocks
        .iter()
        .find(|b| b.operation == "harness.await")
        .unwrap();
    let mut graph = definition(source_block);
    let source_id = graph
        .nodes
        .values()
        .find(|n| n.name == "external")
        .unwrap()
        .id
        .clone();
    let mut destination = graph.nodes[&source_id].clone();
    destination.id = "adopted-await".into();
    destination.name = "adopted-await".into();
    destination.configuration = serde_json::to_value(await_block).unwrap();
    let claim = bcode_workflow::ResourceClaim {
        resource: "fixture-workspace".into(),
        access: bcode_workflow::ResourceAccess::Write,
    };
    graph.nodes.get_mut(&source_id).unwrap().resources = vec![claim.clone()];
    destination.resources = vec![claim];
    let mut gate = graph
        .nodes
        .values()
        .find(|node| node.name == "before")
        .unwrap()
        .clone();
    gate.id = "adoption-input".into();
    gate.name = "adoption-input".into();
    graph.entries.push(gate.id.clone());
    graph.exits.push(destination.id.clone());
    graph
        .edges
        .push(serde_json::from_value(json!({"from":gate.id,"to":destination.id})).unwrap());
    graph.nodes.insert(gate.id.clone(), gate);
    graph.nodes.insert(destination.id.clone(), destination);
    let start = start_request(parent.id, input.clone(), root.path());
    retain_legacy_run(&state, &start, &graph);
    start_run(&state, start, None).await.unwrap();
    resolve_gate(&state, "before", input.clone()).await;
    approve_async_nodes(&state).await;
    wait_for_canonical_confirmation(&state, child).await;
    assert_eq!(control(&state, "count").await, 1);
    assert_eq!(control(&state, "terminal").await, 1);
    observe_adoption_foreground(&state, child).await;
    let mut destination_input = input.clone();
    destination_input["value"]["completion_requirement"] = json!("associated_work");
    resolve_gate(&state, "adoption-input", destination_input).await;
    approve_async_nodes(&state).await;
    let (source, destination, authority, revision, leases) = adoption_ready_snapshot(&state);
    Box::pin(complete_async_adoption(
        &state,
        child,
        &graph,
        source,
        destination,
        authority,
        (revision, leases),
    ))
    .await;
    assert_eq!(control(&state, "count").await, 1);
    drop(state);
}

async fn observe_adoption_foreground(state: &Arc<ServerState>, child: SessionId) {
    for _ in 0..8 {
        let _ = production_receipt_observation(state).await.unwrap();
        let evidence = state
            .sessions
            .session_external_harness_submission_evidence(child, "submission", "attempt")
            .await
            .unwrap()
            .unwrap()
            .1;
        if evidence.observations.iter().any(|o| {
            matches!(
                &o.kind,
                HarnessObservationKind::Execution {
                    event: HarnessExecutionEvent::SubmissionTerminal { .. }
                }
            )
        }) {
            break;
        }
    }
}

fn adoption_ready_snapshot(
    state: &Arc<ServerState>,
) -> (
    bcode_workflow::AttemptSummary,
    bcode_workflow_store::PendingActivation,
    bcode_workflow_store::WorkflowExecutionAuthority,
    u64,
    Vec<bcode_workflow::WorkflowResourceLease>,
) {
    let store = state.workflow_store.lock().unwrap();
    let source = store
        .attempt_history(RUN, None, 8)
        .unwrap()
        .into_iter()
        .find(|a| a.has_receipt)
        .unwrap();
    let authority = store.execution_authority(RUN).unwrap().unwrap();
    let pending = store
        .pending_activations_for_run(RUN, 8)
        .unwrap()
        .into_iter()
        .find(|a| a.node_id == "adopted-await")
        .unwrap();
    let revision = store.run_graph_revision(RUN).unwrap().unwrap();
    let leases = store.resource_leases_for_run(RUN, 100).unwrap();
    drop(store);
    (source, pending, authority, revision, leases)
}

fn adoption_grant_fixture(
    state: &Arc<ServerState>,
    destination: &bcode_workflow_store::PendingActivation,
    intent: &Value,
) -> (Value, bcode_workflow_store::WorkflowGrant) {
    let block: WorkflowBlockDefinition =
        serde_json::from_value(destination.node.configuration.clone()).unwrap();
    let preparation: bcode_workflow::WorkflowBlockPreparationResponse =
        serde_json::from_value(intent["preparation"].clone()).unwrap();
    let scope =
        crate::workflow_mutation_grant_scope(state, destination, &block, Some(&preparation))
            .unwrap();
    let scope = serde_json::to_value(scope).unwrap();
    let grant = state
        .workflow_store
        .lock()
        .unwrap()
        .grants_for_run(RUN, 100)
        .unwrap()
        .into_iter()
        .find(|grant| grant.scope == scope)
        .unwrap();
    (scope, grant)
}

async fn complete_async_adoption(
    state: &Arc<ServerState>,
    child: SessionId,
    graph: &bcode_workflow::WorkflowDefinition,
    source: bcode_workflow::AttemptSummary,
    destination: bcode_workflow_store::PendingActivation,
    authority: bcode_workflow_store::WorkflowExecutionAuthority,
    snapshot: (u64, Vec<bcode_workflow::WorkflowResourceLease>),
) {
    let (revision, leases) = snapshot;
    let plan = (crate::WorkflowActivationOwner { state })
        .plan(&destination)
        .await
        .unwrap()
        .unwrap();
    let (scope, grant) = adoption_grant_fixture(state, &destination, &plan.intent);
    let stored = state
        .workflow_store
        .lock()
        .unwrap()
        .dispatch_receipt(&source.dispatch_identity)
        .unwrap()
        .unwrap();
    let canonical = state
        .sessions
        .session_external_harness_state(child)
        .await
        .unwrap()
        .unwrap();
    let inventory = state
        .sessions
        .session_harness_inventory_page(
            &canonical.owner,
            &bcode_session_models::HarnessInventoryScope::Attempt(HarnessAttemptIdentity {
                submission_id: "submission".into(),
                attempt_id: "attempt".into(),
            }),
            bcode_session_models::HarnessInventoryKind::Terminal,
            None,
        )
        .await
        .unwrap();
    let summary = state
        .sessions
        .session_harness_inventory_summary(
            &canonical.owner,
            &bcode_session_models::HarnessInventoryScope::Attempt(HarnessAttemptIdentity {
                submission_id: "submission".into(),
                attempt_id: "attempt".into(),
            }),
            bcode_session_models::HarnessInventoryKind::Terminal,
        )
        .await
        .unwrap();
    assert_eq!(inventory.commitment, summary.commitment);
    let request = bcode_workflow::WorkflowObligationAdoption {
        obligation_id: source.dispatch_identity.clone(),
        run_id: RUN.into(),
        expected_revision: revision,
        ownership_generation: authority.generation,
        source: bcode_workflow::WorkflowObligationAttempt {
            node_id: source.node_id,
            activation_id: source.activation_id,
            attempt: source.attempt,
            dispatch_identity: source.dispatch_identity.clone(),
        },
        destination: bcode_workflow::WorkflowObligationAttempt {
            node_id: destination.node_id.clone(),
            activation_id: destination.activation_id.clone(),
            attempt: 1,
            dispatch_identity: bcode_workflow_store::dispatch_identity(
                RUN,
                &destination.node_id,
                &destination.activation_id,
                1,
            ),
        },
        source_receipt_sha256: bcode_workflow::workflow_canonical_value_sha256(&stored.receipt)
            .unwrap(),
        evidence_subject: bcode_workflow::workflow_canonical_value_sha256(
            &json!({"session_id":child,"generation":1,
            "submission_id":"submission","attempt_id":"attempt"}),
        )
        .unwrap(),
        evidence_revision: summary.observed_through_sequence,
        evidence_sha256: bcode_workflow::workflow_canonical_value_sha256(
            &serde_json::to_value(summary).unwrap(),
        )
        .unwrap(),
        resource_lease_ids: leases.into_iter().map(|lease| lease.lease_id).collect(),
        destination_grant_id: grant.grant_id,
        destination_grant_scope_sha256: bcode_workflow::workflow_canonical_value_sha256(&scope)
            .unwrap(),
    };
    reject_adoption_evidence_race(state, child, &request).await;
    Box::pin(settle_adopted_destination(
        state,
        child,
        graph,
        &destination,
        request,
        &canonical.owner,
    ))
    .await;
}

async fn reject_adoption_evidence_race(
    state: &Arc<ServerState>,
    child: SessionId,
    request: &bcode_workflow::WorkflowObligationAdoption,
) {
    let hook_state = Arc::clone(state);
    let hook = Arc::new(
        move || -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            let state = Arc::clone(&hook_state);
            Box::pin(async move {
                state.sessions.append_event(child,
                bcode_session_models::SessionEventKind::SystemMessage {
                    text:"canonical evidence revision advanced while destination planning awaited".into(),
                }).await.unwrap();
            })
        },
    );
    let rejected = Box::pin(crate::harness_workflow::with_adoption_after_planning(
        hook,
        crate::harness_workflow::adopt_execution(state, request),
    ))
    .await;
    assert!(
        rejected.is_err(),
        "canonical evidence changed during ordinary destination authorization"
    );
    assert_eq!(
        state
            .workflow_store
            .lock()
            .unwrap()
            .grant(&request.destination_grant_id)
            .unwrap()
            .unwrap()
            .uses_consumed,
        0
    );
    assert!(
        state
            .workflow_store
            .lock()
            .unwrap()
            .dispatch_receipt(&request.destination.dispatch_identity)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        state
            .workflow_store
            .lock()
            .unwrap()
            .resource_leases_for_run(RUN, 100)
            .unwrap()[0]
            .activation_id,
        request.source.activation_id
    );
}

async fn settle_adopted_destination(
    state: &Arc<ServerState>,
    child: SessionId,
    graph: &bcode_workflow::WorkflowDefinition,
    destination: &bcode_workflow_store::PendingActivation,
    request: bcode_workflow::WorkflowObligationAdoption,
    owner: &ExternalHarnessOwner,
) {
    // Reinspect and explicitly approve the new pinned evidence; the rejected operation
    // consumed neither the native effect nor the destination's canonical grant.
    let revised = state
        .sessions
        .session_harness_inventory_summary(
            owner,
            &bcode_session_models::HarnessInventoryScope::Attempt(HarnessAttemptIdentity {
                submission_id: "submission".into(),
                attempt_id: "attempt".into(),
            }),
            bcode_session_models::HarnessInventoryKind::Terminal,
        )
        .await
        .unwrap();
    let mut request = request;
    request.evidence_revision = revised.observed_through_sequence;
    request.evidence_sha256 =
        bcode_workflow::workflow_canonical_value_sha256(&serde_json::to_value(revised).unwrap())
            .unwrap();
    let proof = crate::harness_workflow::adopt_execution(state, &request)
        .await
        .unwrap();
    assert_eq!(
        crate::harness_workflow::adopt_execution(state, &request)
            .await
            .unwrap(),
        proof,
        "lost adoption acknowledgement must recover without native delivery"
    );
    let held = state
        .workflow_store
        .lock()
        .unwrap()
        .resource_leases_for_run(RUN, 100)
        .unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].activation_id, destination.activation_id);
    let output = wait_for_external_output(state, graph, child).await;
    assert_eq!(
        output.value["value"]["execution_handle"]["dispatch_identity"],
        request.destination.dispatch_identity
    );
    assert_eq!(control(state, "count").await, 1);
    assert_eq!(
        state
            .workflow_store
            .lock()
            .unwrap()
            .resource_leases_for_run(RUN, 100)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(control(state, "release").await, 1);
    let mut settled = false;
    for _ in 0..8 {
        crate::drive_workflow_run(state, RUN).await.unwrap();
        settled = state
            .workflow_store
            .lock()
            .unwrap()
            .attempt_history(RUN, None, 10)
            .unwrap()
            .into_iter()
            .any(|attempt| {
                attempt.dispatch_identity == request.destination.dispatch_identity
                    && attempt.status == "succeeded"
            });
        if settled {
            break;
        }
    }
    assert!(
        settled,
        "adopted operation must settle from canonical lifecycle evidence"
    );
    assert_eq!(
        state
            .workflow_store
            .lock()
            .unwrap()
            .resource_leases_for_run(RUN, 100)
            .unwrap(),
        [] as [bcode_workflow::WorkflowResourceLease; 0]
    );
    assert_eq!(control(state, "count").await, 1);
}

async fn approve_async_nodes(state: &Arc<ServerState>) {
    for _ in 0..4 {
        let approvals = state
            .workflow_store
            .lock()
            .unwrap()
            .pending_mutation_approvals(RUN, 8)
            .unwrap();
        if approvals.is_empty() {
            break;
        }
        for approval in approvals {
            resolve_mutation_approval(
                state,
                &approval.approval_id,
                bcode_workflow::WorkflowMutationApprovalDecision::Approve,
            )
            .await
            .unwrap();
        }
    }
}

async fn check_production_dispatch(disposition: HarnessTerminalDisposition) {
    Box::pin(check_execution_dispatch(disposition, None, false)).await;
}

#[tokio::test]
async fn production_foreground_completes_with_retained_background_handle() {
    Box::pin(check_execution_dispatch(
        HarnessTerminalDisposition::Completed,
        Some(HarnessCompletionRequirement::Foreground),
        false,
    ))
    .await;
}

#[tokio::test]
async fn production_associated_work_waits_and_does_not_resubmit() {
    Box::pin(check_execution_dispatch(
        HarnessTerminalDisposition::Completed,
        Some(HarnessCompletionRequirement::AssociatedWork),
        false,
    ))
    .await;
}

#[tokio::test]
async fn production_background_failure_fails_assignment_not_foreground() {
    Box::pin(check_execution_dispatch(
        HarnessTerminalDisposition::Completed,
        Some(HarnessCompletionRequirement::AssociatedWork),
        true,
    ))
    .await;
}

async fn check_execution_dispatch(
    disposition: HarnessTerminalDisposition,
    requirement: Option<HarnessCompletionRequirement>,
    background_failed: bool,
) {
    let _fixture_lifetime = FIXTURE_LIFETIME.lock().await;
    let root = tempfile::tempdir().unwrap();
    let state = fixture(root.path());
    assert_eq!(control(&state, "reset").await, 0);
    if let Some(requirement) = requirement {
        let operation = if requirement == HarnessCompletionRequirement::Foreground {
            "foreground"
        } else {
            "execution"
        };
        assert_eq!(control(&state, operation).await, 0);
    }
    if background_failed {
        assert_eq!(control(&state, "background_failed").await, 0);
    }
    match disposition {
        HarnessTerminalDisposition::Completed => {}
        HarnessTerminalDisposition::Failed => {
            assert_eq!(control(&state, "fail").await, 0);
        }
        HarnessTerminalDisposition::Interrupted => {
            assert_eq!(control(&state, "interrupt").await, 0);
        }
    }
    let parent = state
        .sessions
        .create_session(None, root.path().into())
        .await
        .unwrap();
    let (opening, target) = Box::pin(opened_child(&state, parent.id, root.path())).await;
    let child: SessionId = target.session_id().parse().unwrap();
    let submission = HarnessSubmission {
        submission_id: "submission".into(),
        attempt_id: "attempt".into(),
        session_id: target.session_id().into(),
        generation: 1,
        text: "offline native assignment".into(),
        placement: HarnessPlacement::NextTurn,
    };
    let mut input = json!({"schema_version":1,"state":{"normal_predecessor":"retained"},"value":{"request_json":serde_json::to_string(&json!({
        "session_id":target.session_id(),"parent_session_id":null,"assistance_destination_session_id":null,
        "harness":{"schema_version":1,"request_id":"submission","instance":opening.instance,"kind":{"operation":"submit","request":{"target":target,"submission":submission}}}
    })).unwrap()}});
    if let Some(requirement) = requirement {
        input["value"]["completion_requirement"] = serde_json::to_value(requirement).unwrap();
    }
    let block = state
        .plugins
        .registry()
        .workflow_blocks()
        .into_iter()
        .find(|block| block.operation == "harness.execute")
        .unwrap();
    let graph = definition(&block);
    let request = start_request(parent.id, input.clone(), root.path());
    retain_legacy_run(&state, &request, &graph);
    let started = start_run(&state, request, None).await.unwrap();
    assert_eq!(
        started.run.run_id, RUN,
        "unexpected start result: {started:?}"
    );
    assert_eq!(control(&state, "count").await, 0);
    resolve_gate(&state, "before", input).await;
    assert_eq!(
        control(&state, "count").await,
        0,
        "policy preparation cannot deliver native input"
    );
    approve_and_observe_execution(
        Arc::clone(&state),
        &graph,
        child,
        disposition,
        requirement,
        background_failed,
    )
    .await;
    // Retain the daemon, its canonical session writers, and workflow authority through
    // the final awaited application call; release only after all assertions finish.
    drop(state);
}

async fn approve_and_observe_execution(
    state: Arc<ServerState>,
    graph: &bcode_workflow::WorkflowDefinition,
    child: SessionId,
    disposition: HarnessTerminalDisposition,
    requirement: Option<HarnessCompletionRequirement>,
    background_failed: bool,
) {
    let approvals = state
        .workflow_store
        .lock()
        .unwrap()
        .pending_mutation_approvals(RUN, 8)
        .unwrap();
    assert_eq!(approvals.len(), 1);
    let decision = bcode_workflow::WorkflowMutationApprovalDecision::Approve;
    let resolved = resolve_mutation_approval(&state, &approvals[0].approval_id, decision)
        .await
        .unwrap();
    assert!(
        resolved.decision_applied(decision),
        "approval not applied: {resolved:?}"
    );
    assert_eq!(
        resolved.continuation,
        Some(bcode_workflow::WorkflowApprovalContinuation::Driven),
        "approval continuation did not drive: {resolved:?}; outcomes: {}",
        dispatch_outcomes(&state)
    );
    let attempts = state
        .workflow_store
        .lock()
        .unwrap()
        .attempt_history(RUN, None, 8)
        .unwrap();
    assert!(
        attempts
            .iter()
            .all(|attempt| !matches!(attempt.status.as_str(), "failed" | "repair_required")),
        "scheduler recorded failed dispatch before native delivery: {}",
        dispatch_outcomes(&state)
    );
    assert_canonical_workflow_admission(&state).await;
    let outcomes = dispatch_outcomes(&state);
    assert_eq!(
        control(&state, "count").await,
        1,
        "approval: {resolved:?}; persisted dispatch outcomes: {outcomes}"
    );
    wait_for_canonical_confirmation(&state, child).await;
    assert_typed_acceptance(&state, child).await;
    assert_no_external_output(&state);
    crate::drive_workflow_run(&state, RUN).await.unwrap();
    assert_eq!(
        control(&state, "count").await,
        1,
        "retry observation must not resubmit"
    );
    assert_no_external_output(&state);
    if requirement == Some(HarnessCompletionRequirement::Foreground) {
        // A completed foreground returns a retained handle; it must not discharge or
        // interrupt the independently running native background command.
        assert_eq!(control(&state, "terminal").await, 1);
    } else {
        assert_terminal_without_quiescence(&state, child, disposition).await;
        assert_eq!(control(&state, "release").await, 1);
    }
    if background_failed {
        assert_background_failure(&state, child).await;
        return;
    }
    if disposition != HarnessTerminalDisposition::Completed {
        assert_non_success_terminal(&state, child, disposition).await;
        return;
    }
    let output = wait_for_external_output(&state, graph, child).await;
    assert_foreground_handoff(state.as_ref(), child, requirement, &output.value).await;
    assert_eq!(output.value["state"]["normal_predecessor"], "retained");
    assert_eq!(output.value["value"]["result"], "verified native result");
    assert_eq!(output.value["value"]["submission_id"], "submission");
    resolve_gate(&state, "after", output.value.clone()).await;
    let run = state
        .workflow_store
        .lock()
        .unwrap()
        .run_summary(RUN)
        .unwrap()
        .unwrap();
    assert_eq!(run.status, bcode_workflow::RunStatus::Completed);
    assert_eq!(control(&state, "count").await, 1);
    drop(state);
}

async fn assert_foreground_handoff(
    state: &ServerState,
    child: SessionId,
    requirement: Option<HarnessCompletionRequirement>,
    output: &Value,
) {
    if requirement != Some(HarnessCompletionRequirement::Foreground) {
        return;
    }
    assert!(output["value"]["native_turn_id"].is_null());
    assert_eq!(output["value"]["execution_handle"]["attempt_id"], "attempt");
    let (_, evidence) = state
        .sessions
        .session_external_harness_submission_evidence(child, "submission", "attempt")
        .await
        .unwrap()
        .unwrap();
    assert!(evidence.observations.iter().any(|o| matches!(
        &o.kind,
        HarnessObservationKind::Execution {
            event: HarnessExecutionEvent::BackgroundWork {
                state: HarnessWorkState::Running,
                ..
            }
        }
    )));
}

async fn assert_terminal_without_quiescence(
    state: &Arc<ServerState>,
    child: SessionId,
    disposition: HarnessTerminalDisposition,
) {
    assert_eq!(control(state, "terminal").await, 1);
    for _ in 0..8 {
        assert!(matches!(
            production_receipt_observation(state).await.unwrap(),
            bcode_workflow_store::AttemptObservation::Deferred { .. }
        ));
        let (_, evidence) = state
            .sessions
            .session_external_harness_submission_evidence(child, "submission", "attempt")
            .await
            .unwrap()
            .unwrap();
        if evidence.observations.iter().any(|event| {
            matches!(
                &event.kind,
                HarnessObservationKind::NativeTurnTerminal { disposition: actual, .. }
                    if *actual == disposition
            ) || matches!(&event.kind, HarnessObservationKind::Execution {
                event: HarnessExecutionEvent::SubmissionTerminal { disposition: actual, .. }
            } if *actual == disposition)
        }) {
            assert!(
                !evidence
                    .observations
                    .iter()
                    .any(|event| matches!(&event.kind, HarnessObservationKind::Settlement { .. }))
            );
            assert_no_external_output(state);
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("native terminal evidence was not retained");
}

async fn assert_background_failure(state: &Arc<ServerState>, child: SessionId) {
    let mut failed = false;
    for _ in 0..8 {
        match production_receipt_observation(state).await.unwrap() {
            bcode_workflow_store::AttemptObservation::Failed { message } => {
                assert_eq!(message, "managed associated work failed");
                failed = true;
                break;
            }
            bcode_workflow_store::AttemptObservation::Deferred { .. } => {}
            other => panic!("background failure must not become success: {other:?}"),
        }
        tokio::task::yield_now().await;
    }
    assert!(failed);
    let (_, evidence) = state
        .sessions
        .session_external_harness_submission_evidence(child, "submission", "attempt")
        .await
        .unwrap()
        .unwrap();
    assert!(evidence.observations.iter().any(|o| matches!(
        &o.kind,
        HarnessObservationKind::Execution {
            event: HarnessExecutionEvent::SubmissionTerminal {
                disposition: HarnessTerminalDisposition::Completed,
                ..
            }
        }
    )));
    assert_no_external_output(state);
    for _ in 0..8 {
        crate::drive_workflow_run(state, RUN).await.unwrap();
        let failed = state
            .workflow_store
            .lock()
            .unwrap()
            .attempt_history(RUN, None, 8)
            .unwrap()
            .into_iter()
            .any(|attempt| attempt.has_receipt && attempt.status == "failed");
        if failed {
            assert_eq!(control(state, "count").await, 1);
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("associated failure did not persist as failed workflow attempt");
}

async fn assert_non_success_terminal(
    state: &Arc<ServerState>,
    child: SessionId,
    disposition: HarnessTerminalDisposition,
) {
    let mut observed = None;
    for _ in 0..8 {
        let observation = production_receipt_observation(state).await.unwrap();
        if !matches!(
            observation,
            bcode_workflow_store::AttemptObservation::Deferred { .. }
        ) {
            observed = Some(observation);
            break;
        }
        tokio::task::yield_now().await;
    }
    let expected_status = match (disposition, observed) {
        (
            HarnessTerminalDisposition::Interrupted,
            Some(bcode_workflow_store::AttemptObservation::Cancelled),
        ) => "cancelled",
        (
            HarnessTerminalDisposition::Failed,
            Some(bcode_workflow_store::AttemptObservation::Failed { message }),
        ) => {
            assert_eq!(message, "managed native turn failed");
            "failed"
        }
        other => panic!("expected authoritative non-success observation: {other:?}"),
    };
    let (_, evidence) = state
        .sessions
        .session_external_harness_submission_evidence(child, "submission", "attempt")
        .await
        .unwrap()
        .unwrap();
    assert!(evidence.observations.iter().any(|event| matches!(
        &event.kind,
        HarnessObservationKind::NativeTurnTerminal { native_turn_id, disposition: actual }
            if native_turn_id == "native-turn" && *actual == disposition
    )));
    assert!(evidence.observations.iter().any(|event| matches!(
        &event.kind,
        HarnessObservationKind::Settlement { native_turn_id } if native_turn_id == "native-turn"
    )));
    assert!(
        !evidence
            .observations
            .iter()
            .any(|event| matches!(&event.kind, HarnessObservationKind::Result { .. }))
    );
    for _ in 0..8 {
        crate::drive_workflow_run(state, RUN).await.unwrap();
        let terminal = state
            .workflow_store
            .lock()
            .unwrap()
            .attempt_history(RUN, None, 8)
            .unwrap()
            .into_iter()
            .any(|attempt| attempt.has_receipt && attempt.status == expected_status);
        if terminal {
            assert_no_external_output(state);
            assert_eq!(control(state, "count").await, 1);
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("native non-success did not reach persisted workflow terminal outcome");
}

async fn assert_typed_acceptance(state: &ServerState, child: SessionId) {
    let canonical = state
        .sessions
        .session_external_harness_state(child)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(canonical.target, Some(HarnessSessionTarget::Confirmed(_))),
        "init confirmation is retained without settling assignment"
    );
    let receipt = state
        .sessions
        .external_harness_receipt(&canonical.owner, "submission", "attempt")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.state, HarnessDeliveryState::NativeAccepted);
    assert_eq!(receipt.native_request_id.as_deref(), Some("native-request"));
    let acceptance = state
        .sessions
        .session_external_harness_submission_acceptance(child, "submission", "attempt")
        .await
        .unwrap()
        .expect("typed acceptance retained before completion");
    assert!(
        matches!(&acceptance.kind, HarnessObservationKind::SubmissionAccepted { submission_id, attempt_id, native_turn_id }
        if submission_id == "submission" && attempt_id == "attempt" && native_turn_id == "native-turn")
    );
    let (evidence_owner, evidence) = state
        .sessions
        .session_external_harness_submission_evidence(child, "submission", "attempt")
        .await
        .unwrap()
        .expect("canonical indexed submission evidence");
    assert_eq!(evidence_owner, canonical.owner);
    assert_eq!(evidence.completeness, HarnessCompleteness::Partial);
    assert!(evidence.observations.contains(&acceptance));
    assert!(!evidence.observations.iter().any(|event| matches!(
        &event.kind,
        HarnessObservationKind::NativeTurnTerminal { .. }
            | HarnessObservationKind::Settlement { .. }
    )));
}

async fn wait_for_external_output(
    state: &Arc<ServerState>,
    graph: &bcode_workflow::WorkflowDefinition,
    child: SessionId,
) -> bcode_workflow_store::ValidatedOutput {
    for _ in 0..8 {
        crate::drive_workflow_run(state, RUN).await.unwrap();
        let outputs = state
            .workflow_store
            .lock()
            .unwrap()
            .validated_outputs(RUN, 8)
            .unwrap();
        if let Some(output) = outputs
            .into_iter()
            .find(|output| graph.nodes[&output.node_id].name == "external")
        {
            return output;
        }
        tokio::task::yield_now().await;
    }
    let observation = production_receipt_observation(state).await;
    let canonical = state
        .sessions
        .session_external_harness_state(child)
        .await
        .unwrap()
        .unwrap();
    let delivery = state
        .sessions
        .external_harness_receipt(&canonical.owner, "submission", "attempt")
        .await
        .unwrap();
    let history = state
        .sessions
        .external_harness_history(child, None, 128)
        .await
        .unwrap();
    let evidence: Vec<_> = history
        .observations
        .iter()
        .map(|event| {
            (
                &event.observation_id,
                &event.native_correlations,
                event.completeness,
                match &event.kind {
                    HarnessObservationKind::NativeTurnTerminal { .. } => "terminal",
                    HarnessObservationKind::Settlement { .. } => "settlement",
                    HarnessObservationKind::Result { .. } => "result",
                    HarnessObservationKind::BindingConfirmed { .. } => "confirmation",
                    _ => "other",
                },
            )
        })
        .collect();
    panic!(
        "no validated external output after release: actual_observer_result={observation:?}; delivery={delivery:?}; cursor={:?}; evidence={evidence:?}; {}",
        canonical.observation_after,
        dispatch_outcomes(state)
    );
}

// Run bounded production observation passes, never the managed observer directly. A missing
// owner-routing hook must fail here rather than being hidden by fixture-generated evidence.
async fn wait_for_canonical_confirmation(state: &Arc<ServerState>, child: SessionId) {
    for _ in 0..8 {
        crate::drive_workflow_run(state, RUN).await.unwrap();
        let canonical = state
            .sessions
            .session_external_harness_state(child)
            .await
            .unwrap()
            .unwrap();
        if matches!(canonical.target, Some(HarnessSessionTarget::Confirmed(_))) {
            assert!(
                control(state, "polls").await > 0,
                "confirmation requires actual native observation"
            );
            assert_no_external_output(state);
            return;
        }
        tokio::task::yield_now().await;
    }
    let observation = production_receipt_observation(state).await;
    let polls = control(state, "polls").await;
    let canonical = state
        .sessions
        .session_external_harness_state(child)
        .await
        .unwrap()
        .unwrap();
    panic!(
        "production workflow observation did not commit confirmation: actual_observer_result={observation:?}; native_observe_calls={polls}; target={:?}; cursor={:?}; {}",
        canonical.target,
        canonical.observation_after,
        dispatch_outcomes(state)
    );
}

// Ask the same production observer used by the scheduler and retain its actual result.
// This diagnostic never calls the managed owner directly or converts deferral into success.
async fn production_receipt_observation(
    state: &ServerState,
) -> Result<bcode_workflow_store::AttemptObservation, bcode_workflow_store::WorkflowStoreError> {
    use bcode_workflow_store::AsyncAttemptStatusObserver as _;
    let request = {
        let store = state.workflow_store.lock().unwrap();
        let attempt = store
            .attempt_history(RUN, None, 8)
            .unwrap()
            .into_iter()
            .find(|attempt| attempt.has_receipt)
            .expect("workflow dispatch receipt");
        let receipt = store
            .dispatch_receipt(&attempt.dispatch_identity)
            .unwrap()
            .unwrap();
        drop(store);
        bcode_workflow_store::AttemptReconciliationRequest {
            run_id: attempt.run_id,
            node_id: attempt.node_id,
            activation_id: attempt.activation_id,
            attempt: attempt.attempt,
            dispatch_identity: attempt.dispatch_identity,
            side_effect: attempt.side_effect,
            receipt: receipt.receipt,
        }
    };
    crate::WorkflowTurnReceiptObserver { state }
        .observe_async(&request)
        .await
}

// Diagnose the real admission boundary without executing or replaying a retained mutation.
async fn assert_canonical_workflow_admission(state: &Arc<ServerState>) {
    use crate::harness_dispatch::ManagedSessionAdmission as _;
    let receipt = {
        let store = state.workflow_store.lock().unwrap();
        let attempt = store
            .attempt_history(RUN, None, 8)
            .unwrap()
            .into_iter()
            .find(|attempt| attempt.has_receipt)
            .expect("workflow dispatch receipt");
        store
            .dispatch_receipt(&attempt.dispatch_identity)
            .unwrap()
            .unwrap()
    };
    let value = &receipt.receipt;
    let call: bcode_ipc::harness::ManagedSessionCall =
        serde_json::from_value(value["call"].clone()).unwrap();
    let session_id = value["session_id"].as_str().unwrap().parse().unwrap();
    let plugin_id = value["plugin_id"].as_str().unwrap().to_owned();
    let _workflow_hold = crate::harness_workflow::authorize_call(
        state,
        RUN,
        &receipt.dispatch_identity,
        session_id,
        &plugin_id,
        &call,
    )
    .await
    .expect("exact workflow dispatch authority");
    let caller = crate::harness_dispatch::ManagedCaller::Workflow {
        run_id: RUN.into(),
        dispatch_identity: receipt.dispatch_identity,
        session_id,
        plugin_id,
    };
    let admission = crate::harness::CanonicalManagedAdmission::new(
        crate::harness::CanonicalHarnessStore::new(state),
    );
    let retained = admission.retained_result(&caller, &call).await;
    let existing = if matches!(&retained, Ok(None)) {
        Some(admission.existing(&caller, &call).await)
    } else {
        None
    };
    drop(admission);
    match retained {
        Ok(Some(_)) => {} // Identical historical acknowledgement needs no redelivery.
        Ok(None) => {
            let result = existing.expect("unrecorded mutation requires existing admission");
            assert!(
                result.is_ok(),
                "authority-verified workflow rejected by canonical admission before native delivery: {result:?}"
            );
        }
        Err(error) => panic!(
            "authority-verified workflow rejected by retained-result admission before native delivery: {error:?}"
        ),
    }
}

fn dispatch_outcomes(state: &ServerState) -> String {
    let store = state.workflow_store.lock().unwrap();
    let run = store.run_summary(RUN).unwrap();
    let attempts = store.attempt_history(RUN, None, 8).unwrap();
    let receipts = attempts
        .iter()
        .map(|attempt| {
            let receipt = store.dispatch_receipt(&attempt.dispatch_identity).unwrap();
            let handle = receipt.as_ref().map(|value| &value.receipt);
            // Inspect bounded identity/evidence, not retained prompt text or full request JSON.
            let summary = handle.map(|value| json!({
                "owner":value.get("owner"), "dispatch_identity":value.get("dispatch_identity"),
                "session_id":value.get("session_id"), "generation":value.get("generation"),
                "target":value.pointer("/call/harness/kind/request/target"),
                "submission_id":value.pointer("/call/harness/kind/request/submission/submission_id"),
            }));
            format!("attempt={attempt:?}; receipt={summary:?}")
        })
        .collect::<Vec<_>>();
    drop(store);
    format!("run={run:?}; attempts={receipts:?}")
}

fn assert_no_external_output(state: &ServerState) {
    let store = state.workflow_store.lock().unwrap();
    let graph: bcode_workflow::WorkflowDefinition = serde_json::from_str(
        &store
            .definition("managed-mixed", 1)
            .unwrap()
            .unwrap()
            .definition_json,
    )
    .unwrap();
    assert!(
        store
            .validated_outputs(RUN, 8)
            .unwrap()
            .iter()
            .all(|output| graph.nodes[&output.node_id].name != "external")
    );
    assert_ne!(
        store.run_summary(RUN).unwrap().unwrap().status,
        bcode_workflow::RunStatus::Completed
    );
    drop(store);
}
