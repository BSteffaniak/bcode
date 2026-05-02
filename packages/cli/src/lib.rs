#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Command-line interface for Bcode.

use bcode_client::{BcodeClient, ClientError};
use bcode_ipc::{Event, default_endpoint};
use bcode_session_models::{SessionEvent, SessionEventKind, SessionId};
use clap::{Parser, Subcommand};
use thiserror::Error;

/// Errors returned by the CLI.
#[derive(Debug, Error)]
pub enum CliError {
    #[error("client error: {0}")]
    Client(#[from] ClientError),
    #[error("server error: {0}")]
    Server(#[from] bcode_server::ServerError),
    #[error("interrupted: {0}")]
    Signal(#[from] std::io::Error),
}

/// Parse CLI arguments and run the requested command.
///
/// # Errors
///
/// Returns an error when the requested command fails.
pub async fn run() -> Result<(), CliError> {
    let cli = Cli::parse();
    match cli.command.unwrap_or_default() {
        Commands::Server { command } => match command {
            ServerCommand::Start => bcode_server::run(default_endpoint()).await?,
        },
        Commands::Session { command } => match command {
            SessionCommand::Create { name } => create_session(name).await?,
            SessionCommand::List => list_sessions().await?,
        },
        Commands::Attach { session_id } => attach_session(session_id).await?,
    }
    Ok(())
}

#[derive(Debug, Parser)]
#[command(name = "bcode", version, about = "TUI-first coding agent")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    Attach {
        session_id: SessionId,
    },
}

impl Default for Commands {
    fn default() -> Self {
        Self::Session {
            command: SessionCommand::List,
        }
    }
}

#[derive(Debug, Subcommand)]
enum ServerCommand {
    Start,
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    Create { name: Option<String> },
    List,
}

async fn create_session(name: Option<String>) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let session = client.create_session(name).await?;
    println!("{}", session.id);
    Ok(())
}

async fn list_sessions() -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let sessions = client.list_sessions().await?;
    if sessions.is_empty() {
        println!("no sessions");
        return Ok(());
    }
    for session in sessions {
        let name = session.name.unwrap_or_else(|| "<unnamed>".to_string());
        println!("{}\t{}\t{} clients", session.id, name, session.client_count);
    }
    Ok(())
}

async fn attach_session(session_id: SessionId) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let mut connection = client.connect("bcode-attach").await?;
    let history = connection.attach_session(session_id).await?;
    for event in history {
        print_session_event(&event);
    }

    loop {
        tokio::select! {
            event = connection.recv_event() => {
                match event? {
                    Event::Session(event) => print_session_event(&event),
                }
            }
            signal = tokio::signal::ctrl_c() => {
                signal?;
                break;
            }
        }
    }

    Ok(())
}

fn print_session_event(event: &SessionEvent) {
    match &event.kind {
        SessionEventKind::SessionCreated { name } => {
            let name = name.as_deref().unwrap_or("<unnamed>");
            println!("#{} session created: {name}", event.sequence);
        }
        SessionEventKind::ClientAttached { client_id } => {
            println!("#{} client attached: {client_id}", event.sequence);
        }
        SessionEventKind::ClientDetached { client_id } => {
            println!("#{} client detached: {client_id}", event.sequence);
        }
        SessionEventKind::UserMessage { client_id, text } => {
            println!("#{} {client_id}: {text}", event.sequence);
        }
        SessionEventKind::SystemMessage { text } => {
            println!("#{} system: {text}", event.sequence);
        }
    }
}
