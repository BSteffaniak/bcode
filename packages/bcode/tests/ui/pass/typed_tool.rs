use bcode::{Agent, ToolApplicationError, TypedTool};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, JsonSchema)]
struct Input {
    value: u64,
}

#[derive(Serialize)]
struct Output {
    value: u64,
}

fn assert_send<T: Send>() {}
fn assert_sync<T: Sync>() {}

fn main() {
    let agent = Agent::builder()
        .typed_tool(
            TypedTool::<Input, Output>::new("echo", "Echo one value"),
            |input| Ok(Output { value: input.value }),
        )
        .typed_tool_async(
            TypedTool::<Input, Output>::new("async_echo", "Echo asynchronously"),
            |input, _context| async move {
                Ok::<_, ToolApplicationError<serde_json::Value>>(Output { value: input.value })
            },
        )
        .build();
    assert_send::<bcode::Agent>();
    assert_sync::<bcode::Agent>();
    let _ = agent;
}
