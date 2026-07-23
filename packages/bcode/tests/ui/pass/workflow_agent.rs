use bcode::{ProviderTurnEvent, StopReason, testing::ScriptedProviderTurn};
use bcode::workflow::{WorkflowBuilder, agent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema)]
struct ReviewTask {
    diff: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct Review {
    approved: bool,
}

fn main() {
    let review = agent::<ReviewTask, Review, _, _>("review", || {
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
    .restrict_tools(["filesystem.read", "filesystem.grep"])
    .system("Review without modifying the repository")
    .max_repairs(1)
    .build();
    let workflow = WorkflowBuilder::new("agent-review", review).build().unwrap();
    let _definition = workflow.definition();
}
