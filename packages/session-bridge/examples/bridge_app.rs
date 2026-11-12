#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! An application built outside the Bcode workspace that uses real Bcode sessions.
//!
//! It depends on `bcode_session_bridge` only, starts the installed Bcode's session bridge
//! (`BCODE_BIN`, else `bcode` on `PATH`), registers one tool and an agent profile that narrows
//! the default agent, creates a session tagged with its origin, sends a message with a durable
//! and a request-only context block, answers the tool call, prints the transcript it observed,
//! and the command that opens the session in `bcode tui`.
//!
//! ```sh
//! BCODE_BIN=target/debug/bcode cargo run -p bcode_session_bridge --example bridge_app -- \
//!     "tool-call example.lookup {\"key\":\"answer\"}"
//! ```

use std::time::Duration;

use bcode_session_bridge::BridgeClient;
use bcode_session_bridge::models::{
    AgentProfile, BridgeEvent, ContextBlock, Origin, OriginFilter, ToolDescriptor, ToolOutcome,
    ToolPolicy, TurnOutcome, ViewItemKind,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "What does the lookup tool say?".to_owned());
    let (bcode, mut events) = BridgeClient::spawn_installed("bridge-example").await?;
    let hello = bcode.hello();
    println!(
        "bridge protocol {} (Bcode {} accepts {}-{})",
        hello.version, hello.bcode_version, hello.bridge_versions.min, hello.bridge_versions.max
    );

    register(&bcode).await?;

    let origin = Origin {
        app_id: "example".into(),
        display_name: Some("Bridge example".into()),
        context: std::iter::once(("purpose".to_owned(), "demo".to_owned())).collect(),
        background: false,
    };
    let directory = std::env::current_dir()?.display().to_string();
    let session = bcode
        .create_session(
            Some("Bridge example".into()),
            directory,
            Some(origin),
            None,
            None,
        )
        .await?;
    println!("session: {}", session.id);

    bcode.observe(&session.id, 50).await?;
    bcode
        .submit(
            &session.id,
            prompt,
            vec![
                ContextBlock {
                    label: "screen".into(),
                    text: "the example's main window".into(),
                    request_only: false,
                },
                ContextBlock {
                    label: "selection".into(),
                    text: "nothing selected".into(),
                    request_only: true,
                },
            ],
            Some("example.reader".into()),
        )
        .await?;

    answer(&bcode, &mut events).await?;

    let mine = bcode
        .list_sessions(
            Some(OriginFilter {
                app_id: Some("example".into()),
                context: std::collections::BTreeMap::new(),
            }),
            None,
            None,
            20,
        )
        .await?;
    println!("sessions from this app: {}", mine.sessions.len());
    println!(
        "open in Bcode: {}",
        bcode.open_command(&session.id).await?.join(" ")
    );
    bcode.close().await;
    Ok(())
}

/// Register the example's tool and its agent profile, which narrows the default agent.
async fn register(bcode: &BridgeClient) -> Result<(), bcode_session_bridge::BridgeClientError> {
    bcode
        .register_tools(
            "example",
            vec![ToolDescriptor {
                name: "example.lookup".into(),
                description: "Look a key up in the example application's live state".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"key": {"type": "string"}},
                }),
                policy: ToolPolicy::ReadOnly,
                max_duration_ms: 10_000,
            }],
            vec![AgentProfile {
                id: "example.reader".into(),
                name: "Example reader".into(),
                description: "Answers from the example application's state".into(),
                base_agent: "build".into(),
                instructions: "Use example.lookup to read the application's state.".into(),
                tools: vec!["example.lookup".into()],
            }],
        )
        .await
}

/// Answer tool calls until an observed snapshot shows the finished answer, then print it.
async fn answer(
    bcode: &BridgeClient,
    events: &mut bcode_session_bridge::BridgeEvents,
) -> Result<(), Box<dyn std::error::Error>> {
    let finished =
        tokio::time::timeout(Duration::from_secs(120), async {
            while let Some(event) = events.recv().await {
                match event {
                    BridgeEvent::ToolInvoke(call) => {
                        println!("tool call: {} {}", call.tool_name, call.arguments_json);
                        bcode
                            .tool_result(
                                &call.invocation_id,
                                ToolOutcome::Completed {
                                    content: "live value: 42".into(),
                                },
                            )
                            .await?;
                    }
                    BridgeEvent::Observation { view, .. } => {
                        if let Some(turn) = view.last_turn.as_ref().filter(|turn| {
                            !view.turn_active && turn.outcome != TurnOutcome::Completed
                        }) {
                            return Err(format!(
                                "the turn ended {:?}: {}",
                                turn.outcome,
                                turn.message.as_deref().unwrap_or("no details")
                            )
                            .into());
                        }
                        let answered = view
                            .items
                            .iter()
                            .any(|item| item.kind == ViewItemKind::Assistant && !item.streaming);
                        if answered && !view.turn_active {
                            for item in &view.items {
                                println!("{:?}: {}", item.kind, item.text);
                            }
                            return Ok::<_, Box<dyn std::error::Error>>(());
                        }
                    }
                    _ => {}
                }
            }
            Err("the bridge closed".into())
        })
        .await;
    finished.map_err(|_| "no answer within two minutes")?
}
