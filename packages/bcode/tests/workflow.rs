#![cfg(feature = "testing")]

use bcode::workflow::{
    WorkflowApprovalResolver, WorkflowBuilder, WorkflowGrantScope, WorkflowPolicyGrant,
    WorkflowToolCapability, agent, authorize_workflow_policy,
};

use bcode::{
    ProviderError, ProviderErrorCategory, ProviderTurnEvent, StopReason, ToolApplicationError,
    ToolCall, ToolPolicyOperation, TypedTool,
    testing::{ScriptedProviderTurn, ScriptedRequestExpectation},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema)]
struct ReviewTask {
    diff: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
struct Review {
    approved: bool,
}

#[derive(Deserialize, JsonSchema)]
struct ToolInput;

#[derive(Serialize)]
struct ToolOutput;

fn provider_error(code: &str) -> ProviderError {
    ProviderError {
        code: code.to_string(),
        category: ProviderErrorCategory::ProviderInternal,
        message: code.to_string(),
        retryable: false,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    }
}

#[tokio::test]
async fn agent_step_surfaces_provider_failure_with_step_context() {
    let workflow = WorkflowBuilder::new(
        "failed-agent-review",
        agent::<ReviewTask, Review, _, _>("review", || {
            bcode::testing::ScriptedProvider::new([ScriptedProviderTurn::provider_error(
                provider_error("review_failed"),
            )])
        })
        .build(),
    )
    .build()
    .expect("workflow builds");

    let error = workflow
        .run(ReviewTask {
            diff: "+ broken".to_string(),
        })
        .await
        .expect_err("provider failure reaches workflow");
    assert!(error.to_string().contains("review"));
    assert!(error.to_string().contains("review_failed"));
}

#[tokio::test]
async fn agent_step_tool_restrictions_narrow_provider_exposure() {
    let provider = bcode::testing::ScriptedProvider::new([ScriptedProviderTurn::complete_text(
        r#"{"approved":true}"#,
    )]);
    let probe = provider.probe();
    let provider_factory = move || provider.clone();
    let workflow = WorkflowBuilder::new(
        "restricted-review",
        agent::<ReviewTask, Review, _, _>("review", provider_factory)
            .agent_id("build")
            .read_only()
            .restrict_tools(["inspect"])
            .configure_agent(|agent| {
                agent
                    .typed_tool(
                        TypedTool::<ToolInput, ToolOutput>::new("inspect", "Inspect")
                            .policy_operation(ToolPolicyOperation::ReadOnly),
                        |_input| Ok(ToolOutput),
                    )
                    .typed_tool_async(
                        TypedTool::<ToolInput, ToolOutput>::new("mutate", "Mutate")
                            .policy_operation(ToolPolicyOperation::Mutating),
                        |_input, _context| async move {
                            Ok::<_, ToolApplicationError<serde_json::Value>>(ToolOutput)
                        },
                    )
            })
            .build(),
    )
    .build()
    .expect("workflow builds");

    workflow
        .run(ReviewTask {
            diff: "+ safe".to_string(),
        })
        .await
        .expect("workflow runs");
    let inspect_definition = bcode_model::ToolDefinition {
        name: "inspect".to_string(),
        description: "Inspect".to_string(),
        input_schema: serde_json::to_value(schemars::schema_for!(ToolInput)).unwrap(),
    };
    probe
        .assert_requests(&[ScriptedRequestExpectation::new().tools([inspect_definition])])
        .expect("only the narrowed read-only tool reaches the provider");
}

#[tokio::test]
async fn cancelled_agent_work_does_not_acquire_provider() {
    let acquisitions = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = std::sync::Arc::clone(&acquisitions);
    let step = agent::<ReviewTask, Review, _, _>("review", move || {
        observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        bcode::testing::ScriptedProvider::new([])
    })
    .build();
    let workflow = WorkflowBuilder::new("cancel-before-acquisition", step)
        .build()
        .expect("workflow");
    let cancellation = bcode::workflow::WorkflowCancellation::new();
    cancellation.cancel();
    let result = workflow
        .run_with_cancellation(
            ReviewTask {
                diff: String::new(),
            },
            cancellation,
        )
        .await;
    assert!(matches!(
        result,
        Err(bcode::workflow::WorkflowError::Cancelled { .. })
    ));
    assert_eq!(acquisitions.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_during_prompt_prevents_provider_acquisition() {
    let cancellation = bcode::workflow::WorkflowCancellation::new();
    let cancel_from_prompt = cancellation.clone();
    let step =
        agent::<ReviewTask, Review, _, _>("review", || -> bcode::testing::ScriptedProvider {
            panic!("cancelled prompt must not acquire provider");
        })
        .prompt_with(move |_| {
            cancel_from_prompt.cancel();
            "cancelled".into()
        })
        .build();
    let workflow = WorkflowBuilder::new("cancel-in-prompt", step)
        .build()
        .expect("workflow");
    let result = workflow
        .run_with_cancellation(
            ReviewTask {
                diff: String::new(),
            },
            cancellation,
        )
        .await;
    assert!(matches!(
        result,
        Err(bcode::workflow::WorkflowError::Cancelled { .. })
    ));
}

#[tokio::test]
async fn agent_step_uses_explicit_agent_initialization() {
    let provider = bcode::testing::ScriptedProvider::new([ScriptedProviderTurn::complete_text(
        r#"{"approved":true}"#,
    )]);
    let probe = provider.probe();
    let builder = bcode::AgentBuilder::from_context(
        bcode::SessionId::default(),
        std::path::PathBuf::from("/explicit-workspace"),
    )
    .model("explicit-model");
    let step = bcode::workflow::AgentStep::<ReviewTask, Review>::with_agent_builder(
        "review",
        move || provider.clone(),
        builder,
    );
    let workflow = WorkflowBuilder::new("explicit-initialization", step.build())
        .build()
        .expect("workflow");
    let review = workflow
        .run(ReviewTask {
            diff: "+ safe".into(),
        })
        .await
        .expect("review");
    assert!(review.approved);
    probe
        .assert_requests(&[ScriptedRequestExpectation::new().model_id("explicit-model")])
        .expect("caller model reaches provider");
}

#[tokio::test]
async fn agent_step_requests_and_validates_structured_output() {
    let workflow = WorkflowBuilder::new(
        "agent-review",
        agent::<ReviewTask, Review, _, _>("review", || {
            bcode::testing::ScriptedProvider::new([ScriptedProviderTurn::new().events([
                ProviderTurnEvent::TextDelta {
                    text: r#"{"approved":true}"#.to_string(),
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                },
            ])])
        })
        .agent_id("plan")
        .system("Review without modifying the repository")
        .build(),
    )
    .build()
    .expect("workflow builds");

    let review = workflow
        .run(ReviewTask {
            diff: "+ safe change".to_string(),
        })
        .await
        .expect("workflow runs");
    assert_eq!(review, Review { approved: true });
    assert_eq!(
        workflow
            .definition()
            .node("review")
            .expect("agent node")
            .configuration["agent_id"],
        "plan"
    );
    assert_eq!(
        workflow
            .definition()
            .node("review")
            .expect("agent node")
            .configuration["prompt_mode"],
        "json_input"
    );
    assert_eq!(
        workflow
            .definition()
            .node("review")
            .expect("agent node")
            .configuration["system_prompt"],
        "Review without modifying the repository"
    );
    assert_eq!(
        workflow
            .definition()
            .node("review")
            .expect("agent node")
            .configuration["execution_target"],
        "fresh_isolated"
    );
}

#[test]
fn shared_parent_agent_target_is_explicit_and_changes_definition_identity() {
    let isolated = WorkflowBuilder::new(
        "targeted-agent",
        agent::<ReviewTask, Review, _, _>("review", || bcode::testing::ScriptedProvider::new([]))
            .build(),
    )
    .build()
    .expect("isolated workflow");
    let shared = WorkflowBuilder::new(
        "targeted-agent",
        agent::<ReviewTask, Review, _, _>("review", || bcode::testing::ScriptedProvider::new([]))
            .shared_parent_sequential()
            .build(),
    )
    .build()
    .expect("shared workflow");

    assert_eq!(
        shared
            .definition()
            .node("review")
            .expect("agent node")
            .configuration["execution_target"],
        "shared_parent_sequential"
    );
    assert_ne!(
        bcode_workflow::WorkflowDefinitionIdentity::for_definition(
            "targeted-agent",
            isolated.definition()
        )
        .expect("isolated identity")
        .definition_id,
        bcode_workflow::WorkflowDefinitionIdentity::for_definition(
            "targeted-agent",
            shared.definition()
        )
        .expect("shared identity")
        .definition_id
    );
}

#[tokio::test]
async fn read_only_reviewer_cannot_invoke_parent_build_tool() {
    let provider = bcode::testing::ScriptedProvider::new([
        ScriptedProviderTurn::new().events([
            ProviderTurnEvent::TurnStarted,
            ProviderTurnEvent::ToolCallFinished {
                call: ToolCall {
                    id: "call-mutate".to_string(),
                    name: "mutate".to_string(),
                    arguments: serde_json::Value::Null,
                },
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::ToolCall,
            },
        ]),
        ScriptedProviderTurn::complete_text(r#"{"approved":true}"#),
    ]);
    let probe = provider.probe();
    let workflow = WorkflowBuilder::new(
        "read-only-child",
        agent::<ReviewTask, Review, _, _>("review", move || provider.clone())
            .agent_id("build")
            .read_only()
            .configure_agent(|agent| {
                agent.typed_tool_async(
                    TypedTool::<ToolInput, ToolOutput>::new("mutate", "Mutate")
                        .policy_operation(ToolPolicyOperation::Mutating),
                    |_input, _context| async move {
                        Err::<ToolOutput, ToolApplicationError<serde_json::Value>>(
                            ToolApplicationError::new(
                                "unexpected_mutation",
                                "mutating tool must not execute in read-only child",
                                "tool unavailable",
                                serde_json::Value::Null,
                            ),
                        )
                    },
                )
            })
            .build(),
    )
    .build()
    .expect("workflow builds");

    let review = workflow
        .run(ReviewTask {
            diff: "+ safe".to_string(),
        })
        .await
        .expect("missing mutating tool is model-visible and review completes");
    assert_eq!(review, Review { approved: true });
    let requests = probe.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.request.tools.is_empty())
    );
}

#[tokio::test]
async fn mutating_agent_step_requires_profile_and_bounded_grant() {
    let scope = WorkflowGrantScope {
        definition: "commit-flow".to_string(),
        definition_version: 1,
        workspace: "snapshot-1".to_string(),
        node: "commit".to_string(),
        run: Some("run-1".to_string()),
    };
    let unconfigured = agent::<ReviewTask, Review, _, _>("commit", || {
        bcode::testing::ScriptedProvider::new([ScriptedProviderTurn::complete_text(
            r#"{"approved":true}"#,
        )])
    });
    let error = unconfigured
        .policy_request(
            WorkflowToolCapability::ReadOnly,
            WorkflowToolCapability::Mutating,
            scope.clone(),
            None,
        )
        .expect_err("implicit build profile cannot authorize mutation");
    assert!(matches!(
        error,
        bcode::workflow::WorkflowError::Build { ref path, .. } if path == "commit"
    ));

    let configured = agent::<ReviewTask, Review, _, _>("commit", || {
        bcode::testing::ScriptedProvider::new([ScriptedProviderTurn::complete_text(
            r#"{"approved":true}"#,
        )])
    })
    .agent_id("build");
    let request = configured
        .policy_request(
            WorkflowToolCapability::ReadOnly,
            WorkflowToolCapability::Mutating,
            scope.clone(),
            None,
        )
        .expect("configured request");
    struct Resolver(Option<WorkflowPolicyGrant>);
    impl WorkflowApprovalResolver for Resolver {
        fn request_approval<'a>(
            &'a self,
            _capability: WorkflowToolCapability,
            _scope: &'a WorkflowGrantScope,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            Option<WorkflowPolicyGrant>,
                            bcode::workflow::WorkflowError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            let grant = self.0.clone();
            Box::pin(async move { Ok(grant) })
        }
    }

    let denied = authorize_workflow_policy(&request, &Resolver(None))
        .await
        .expect_err("approval is mandatory");
    assert!(denied.to_string().contains("not approved"));

    let grant = WorkflowPolicyGrant {
        grant_id: "approval-1".to_string(),
        scope,
        capability: WorkflowToolCapability::Mutating,
    };
    let (effective, audit) = authorize_workflow_policy(&request, &Resolver(Some(grant)))
        .await
        .expect("bounded approval");
    assert_eq!(effective, WorkflowToolCapability::Mutating);
    assert!(audit.contains("grant=approval-1"));
}

#[tokio::test]
async fn agent_step_rejects_invalid_structured_output() {
    let workflow = WorkflowBuilder::new(
        "invalid-agent-review",
        agent::<ReviewTask, Review, _, _>("review", || {
            bcode::testing::ScriptedProvider::new([ScriptedProviderTurn::complete_text(
                r#"{"missing":true}"#,
            )])
        })
        .build(),
    )
    .build()
    .expect("workflow builds");

    let error = workflow
        .run(ReviewTask {
            diff: "+ unsafe change".to_string(),
        })
        .await
        .expect_err("invalid output fails");
    assert!(error.to_string().contains("review"));
}
