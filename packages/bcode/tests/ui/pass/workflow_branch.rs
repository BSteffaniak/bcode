use bcode::workflow::{Step, WorkflowBuilder, field};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
struct Verdict {
    needs_fixes: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct Outcome {
    status: String,
}

fn main() {
    let inspect = Step::map("inspect", |verdict: Verdict| Ok(verdict));
    let fix = Step::map("fix", |_verdict: Verdict| {
        Ok(Outcome {
            status: "fixed".to_string(),
        })
    });
    let finish = Step::map("finish", |_verdict: Verdict| {
        Ok(Outcome {
            status: "clean".to_string(),
        })
    });
    let flow = inspect.branch(
        "needs-fixes?",
        field::<Verdict>("needs_fixes").eq(true),
        fix,
        finish,
    );
    let workflow = WorkflowBuilder::new("branch", flow).build().unwrap();
    let _definition = workflow.definition();
}
