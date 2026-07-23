use bcode::workflow::{Step, WorkflowBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema)]
struct Task {
    value: u64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct ChangeSet {
    revision: u64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct Summary {
    text: String,
}

fn main() {
    let implement = Step::map("implement", |task: Task| {
        Ok(ChangeSet {
            revision: task.value,
        })
    });
    let summarize = Step::map("summarize", |change: ChangeSet| {
        Ok(Summary {
            text: change.revision.to_string(),
        })
    });
    let workflow = WorkflowBuilder::new("typed-sequence", implement.then(summarize))
        .build()
        .unwrap();
    let _definition = workflow.definition();
}
