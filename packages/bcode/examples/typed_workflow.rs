use bcode::workflow::{WorkflowBuilder, agent, parallel};
use bcode::{ProviderTurnEvent, StopReason, testing::ScriptedProviderTurn};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
struct ChangeSet {
    revision: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct Review {
    approved: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> bcode::Result<()> {
    let reviewer = |name: &'static str| {
        agent::<ChangeSet, Review, _, _>(name, || {
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
        .read_only()
        .system("Review the supplied change without modifying the repository")
        .build()
    };

    let workflow = WorkflowBuilder::new(
        "dual-review",
        parallel(reviewer("correctness-review"), reviewer("security-review")),
    )
    .build()
    .expect("valid workflow");
    let (correctness, security) = workflow
        .run(ChangeSet {
            revision: "working-tree:abc123".to_string(),
        })
        .await
        .expect("workflow succeeds");

    assert!(correctness.approved && security.approved);
    println!(
        "{} nodes, {} edges",
        workflow.definition().nodes.len(),
        workflow.definition().edges.len()
    );
    Ok(())
}
