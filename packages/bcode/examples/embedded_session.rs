#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! An application that keeps its conversations as real Bcode sessions.
//!
//! The app attaches to the user's installed Bcode daemon, so everything it creates lands in the
//! user's session store and shows up in `bcode tui` and `bcode session list`:
//!
//! 1. It registers a tool that runs in its own process (`example-app.clock`) and creates a
//!    session tagged with its origin. Creating the session on the provider connection claims it,
//!    so turns in that session can call the tool; Bcode's permission policy governs every call.
//! 2. It migrates a conversation it kept in an SDK `LocalSessionStore` JSON file into a canonical
//!    session with the same origin.
//!
//! The daemon must come from the same Bcode build as this example. Build Bcode from this
//! checkout and point the example at it, with isolated state:
//!
//! ```sh
//! cargo build -p bcode_cli --bin bcode
//! export BCODE_BIN="$PWD/target/debug/bcode" BCODE_STATE_DIR=/tmp/example-state
//! cargo run -p bcode --features daemon-client --example embedded_session
//! "$BCODE_BIN" session list --origin example-app
//! ```
//!
//! Pass `--serve` to keep answering tool calls until one arrives (for example after asking the
//! session to "use example-app.clock" from `bcode tui`).

use std::path::Path;

use bcode::{LocalSessionStore, MessageRole, ModelContentBlock, ModelMessage, PersistedSession};
use bcode_client::InstalledDaemonStart;
use bcode_client::client_tools::ClientToolRequest;
use bcode_session_models::{SessionId, SessionOrigin};
use bcode_tool::client_tool::{ClientToolDescriptor, ClientToolOutcome, ClientToolPolicy};

const APP_ID: &str = "example-app";

fn origin() -> SessionOrigin {
    SessionOrigin::new(APP_ID)
        .with_display_name("Example app")
        .with_context("purpose", "demo")
}

fn clock_tool() -> ClientToolDescriptor {
    ClientToolDescriptor {
        name: format!("{APP_ID}.clock"),
        description: "Read the example app's clock (seconds since the Unix epoch)".to_owned(),
        input_schema: serde_json::json!({"type": "object", "properties": {}}),
        policy: ClientToolPolicy::ReadOnly,
        max_duration_ms: 5_000,
    }
}

/// Write a conversation the way an SDK-local app keeps it today.
fn write_sdk_conversation(path: &Path) -> Result<(), String> {
    let store = LocalSessionStore::new(path);
    store
        .save(&PersistedSession {
            schema_version: 1,
            session_id: SessionId::new(),
            messages: vec![
                ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text {
                        text: "What does the example app do?".to_owned(),
                    }],
                },
                ModelMessage {
                    role: MessageRole::Assistant,
                    content: vec![ModelContentBlock::Text {
                        text: "It shows how an app keeps its conversations in Bcode.".to_owned(),
                    }],
                },
            ],
            memories: Vec::new(),
        })
        .map_err(|error| error.to_string())
}

async fn run() -> Result<(), String> {
    let serve = std::env::args().any(|argument| argument == "--serve");
    let installed = bcode::BcodeClient::for_installed_bcode(InstalledDaemonStart::IfNeeded)
        .await
        .map_err(|error| error.to_string())?;
    let client = installed.client();
    let directory = std::env::current_dir().map_err(|error| error.to_string())?;

    let mut provider = client
        .client_tool_provider(APP_ID, vec![clock_tool()])
        .await
        .map_err(|error| format!("register tools: {error}"))?;
    let session = provider
        .create_session(
            Some("Example app conversation".to_owned()),
            directory.clone(),
            Some(origin()),
        )
        .await
        .map_err(|error| format!("create session: {error}"))?;
    println!("created session {} (origin {APP_ID})", session.id);

    let scratch = std::env::temp_dir().join(format!("example-app-{}.json", std::process::id()));
    write_sdk_conversation(&scratch)?;
    let (imported, warnings) = client
        .import_session_file("sdk-json", scratch.clone(), directory, Some(origin()))
        .await
        .map_err(|error| format!("import: {error}"))?;
    println!(
        "imported {} as session {} ({} warnings)",
        scratch.display(),
        imported.id,
        warnings.len()
    );
    let _ = std::fs::remove_file(&scratch);
    println!(
        "list them with: {} session list --origin {APP_ID}",
        installed.executable().display()
    );

    if !serve {
        return Ok(());
    }
    loop {
        match provider
            .next_request()
            .await
            .map_err(|error| error.to_string())?
        {
            ClientToolRequest::Invoke(invocation) => {
                let seconds = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |elapsed| elapsed.as_secs());
                provider
                    .respond(
                        invocation.invocation_id,
                        ClientToolOutcome::Completed {
                            content: seconds.to_string(),
                        },
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                println!("answered {}", invocation.tool_name);
                return Ok(());
            }
            ClientToolRequest::Cancel { invocation_id } => println!("cancelled {invocation_id}"),
            ClientToolRequest::ClaimLost { session_id } => println!("claim lost: {session_id}"),
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
