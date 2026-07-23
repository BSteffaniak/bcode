use bcode::workflow::{Step, WorkflowBuilder, parallel};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
struct ChangeSet {
    revision: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct CorrectnessReview {
    sound: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct SecurityReview {
    safe: bool,
}

fn main() {
    let correctness = Step::map("correctness", |_change: ChangeSet| {
        Ok(CorrectnessReview { sound: true })
    });
    let security = Step::map("security", |_change: ChangeSet| {
        Ok(SecurityReview { safe: true })
    });
    let reviews = parallel(correctness, security);
    let workflow = WorkflowBuilder::new("dual-review", reviews).build().unwrap();
    let _definition = workflow.definition();
}
