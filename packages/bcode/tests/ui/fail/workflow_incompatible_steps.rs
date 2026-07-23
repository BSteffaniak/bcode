use bcode::workflow::{Step, WorkflowBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema)]
struct Task;

#[derive(Serialize, Deserialize, JsonSchema)]
struct ChangeSet;

#[derive(Serialize, Deserialize, JsonSchema)]
struct Review;

fn main() {
    let implement = Step::map("implement", |_task: Task| Ok(ChangeSet));
    let review = Step::map("review", |_review: Review| Ok(Review));
    let _workflow = WorkflowBuilder::new("bad", implement.then(review));
}
