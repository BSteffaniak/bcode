use bcode_plugin_sdk::tui::{
    PluginTuiHost, PluginWorkflowBinding, PluginWorkflowControlAction,
    PluginWorkflowStartRequest,
};
use bcode_session_models::SessionId;
use bcode_workflow::{Step, WorkflowBuilder, WorkflowSpec};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema)]
struct ReviewInput {
    revision: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct ReviewReport {
    accepted: bool,
}

fn main() {
    let workflow = WorkflowBuilder::new(
        "review",
        Step::map("review", |_input: ReviewInput| {
            Ok(ReviewReport { accepted: true })
        }),
    )
    .build()
    .unwrap();
    let spec = WorkflowSpec::new("code-review.review", &workflow).unwrap();
    let session_id = SessionId::new();
    let request = PluginWorkflowStartRequest::typed(
        &spec,
        &ReviewInput {
            revision: "abc123".to_string(),
        },
        session_id,
        PluginWorkflowBinding {
            owner_plugin_id: "bcode.code-review".to_string(),
            workflow_kind: "code-review.review".to_string(),
            scope_key: session_id.to_string(),
            display_label: Some("Code review".to_string()),
            single_active: true,
        },
        Some("review-abc123".to_string()),
    )
    .unwrap();

    assert_eq!(request.identity.kind, "code-review.review");

    fn lifecycle(host: &dyn PluginTuiHost, binding: &PluginWorkflowBinding) {
        let _status = host.associated_workflow(binding.lookup());
        let _inspection = host.inspect_associated_workflow(binding.lookup(), 25);
        let _control = host.control_associated_workflow(
            binding.lookup(),
            PluginWorkflowControlAction::Cancel,
        );
    }
    let _lifecycle: fn(&dyn PluginTuiHost, &PluginWorkflowBinding) = lifecycle;
}
