use bcode_plugin_sdk::tui::{PluginWorkflowBinding, PluginWorkflowStartRequest};
use bcode_session_models::SessionId;
use bcode_workflow::{Step, WorkflowBuilder, WorkflowSpec};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema)]
struct WorkflowInput {
    value: u64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct WrongInput {
    value: String,
}

fn main() {
    let workflow = WorkflowBuilder::new(
        "typed",
        Step::map("typed", |input: WorkflowInput| Ok(input)),
    )
    .build()
    .unwrap();
    let spec = WorkflowSpec::new("typed", &workflow).unwrap();
    let session_id = SessionId::new();
    let _request = PluginWorkflowStartRequest::typed(
        &spec,
        &WrongInput {
            value: "wrong".to_string(),
        },
        session_id,
        PluginWorkflowBinding {
            owner_plugin_id: "bcode.test".to_string(),
            workflow_kind: "typed".to_string(),
            scope_key: session_id.to_string(),
            display_label: None,
            single_active: false,
        },
        None,
    );
}
