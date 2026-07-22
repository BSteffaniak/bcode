use bcode::{Agent, TypedTool};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::rc::Rc;

#[derive(Deserialize, JsonSchema)]
struct Input;

#[derive(Serialize)]
struct Output;

fn main() {
    let state = Rc::new(());
    let _agent = Agent::builder()
        .typed_tool(TypedTool::<Input, Output>::new("bad", "bad"), move |_input| {
            let _state = Rc::clone(&state);
            Ok(Output)
        })
        .build();
}
