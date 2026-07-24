#![cfg(feature = "testing")]

use bcode::workflow::{
    WorkflowApprovalResolver, WorkflowBuilder, WorkflowGrantScope, WorkflowPolicyGrant,
    WorkflowToolCapability, agent, authorize_workflow_policy,
};

use bcode::{
    ProviderError, ProviderErrorCategory, ProviderTurnEvent, StopReason, ToolApplicationError,
    ToolSideEffect, TypedTool,
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
                            .side_effect(ToolSideEffect::ReadOnly),
                        |_input| Ok(ToolOutput),
                    )
                    .typed_tool_async(
                        TypedTool::<ToolInput, ToolOutput>::new("mutate", "Mutate")
                            .side_effect(ToolSideEffect::WriteFiles),
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
    assert!(error.to_string().contains("configured agent profile"));

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
