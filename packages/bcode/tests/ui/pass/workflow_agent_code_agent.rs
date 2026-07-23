use bcode::{ProviderTurnEvent, StopReason, testing::ScriptedProviderTurn};
use bcode::workflow::{Step, WorkflowBuilder, agent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema)]
struct Task {
    request: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct Draft {
    text: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct NormalizedDraft {
    text: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct Review {
    approved: bool,
}

fn provider() -> bcode::testing::ScriptedProvider {
    bcode::testing::ScriptedProvider::new([ScriptedProviderTurn::new().events([
        ProviderTurnEvent::TextDelta {
            text: r#"{"text":"draft"}"#.to_string(),
        },
        ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::EndTurn,
        },
    ])])
}

fn reviewer() -> bcode::testing::ScriptedProvider {
    bcode::testing::ScriptedProvider::new([ScriptedProviderTurn::new().events([
        ProviderTurnEvent::TextDelta {
            text: r#"{"approved":true}"#.to_string(),
        },
        ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::EndTurn,
        },
    ])])
}

fn main() {
    let draft = agent::<Task, Draft, _, _>("draft", provider).build();
    let normalize = Step::map("normalize", |draft: Draft| {
        Ok(NormalizedDraft {
            text: draft.text.trim().to_string(),
        })
    });
    let review = agent::<NormalizedDraft, Review, _, _>("review", reviewer)
        .agent_id("plan")
        .read_only()
        .build();
    let workflow = WorkflowBuilder::new(
        "agent-code-agent",
        draft.then(normalize).then(review),
    )
    .build()
    .unwrap();
    let _definition = workflow.definition();
}
