use bcode::workflow::{Step, WorkflowBuilder, field};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
struct Input {
    choose_left: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct Left;

#[derive(Serialize, Deserialize, JsonSchema)]
struct Right;

fn main() {
    let inspect = Step::map("inspect", |input: Input| Ok(input));
    let left = Step::map("left", |_input: Input| Ok(Left));
    let right = Step::map("right", |_input: Input| Ok(Right));
    let _flow = inspect.branch(
        "choose",
        field::<Input>("choose_left").eq(true),
        left,
        right,
    );
}
