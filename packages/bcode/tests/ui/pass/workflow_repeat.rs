use bcode::workflow::{Step, WorkflowBuilder, field};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
struct ReviewState {
    needs_fixes: bool,
    attempts: u32,
}

fn main() {
    let review_and_fix = Step::map("review-and-fix", |mut state: ReviewState| {
        state.attempts += 1;
        state.needs_fixes = state.attempts < 3;
        Ok(state)
    })
    .repeat_while(
        "review-cycle",
        field::<ReviewState>("needs_fixes").eq(true),
        3,
    );
    let workflow = WorkflowBuilder::new("bounded-review-cycle", review_and_fix)
        .build()
        .unwrap();
    let _definition = workflow.definition();
}
