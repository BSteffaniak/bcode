use bcode::workflow::{Step, WorkflowBuilder, field};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
struct State {
    repeat: bool,
}

fn main() {
    let step = Step::map("work", |state: State| Ok(state));
    let condition = field::<State>("repeat").eq(true);
    let step = step.repeat_while("unbounded", condition);
    let _workflow = WorkflowBuilder::new("bad", step);
}
