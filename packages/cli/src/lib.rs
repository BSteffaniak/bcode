#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Command-line interface for Bcode.

use bcode_client::{BcodeClient, ClientError};
use bcode_ipc::{Event, default_endpoint};
use bcode_session_models::{SessionEvent, SessionEventKind, SessionId};
use clap::{Parser, Subcommand};
use std::process::{Command, Stdio};
use std::time::Duration;
use thiserror::Error;

/// Errors returned by the CLI.
#[derive(Debug, Error)]
pub enum CliError {
    #[error("client error: {0}")]
    Client(#[from] ClientError),
    #[error("config error: {0}")]
    Config(#[from] bcode_config::ConfigError),
    #[error("server error: {0}")]
    Server(#[from] bcode_server::ServerError),
    #[error("TUI error: {0}")]
    Tui(#[from] bcode_tui::TuiError),
    #[error("plugin error: {0}")]
    Plugin(#[from] bcode_plugin::PluginLoadError),
    #[error("interrupted: {0}")]
    Signal(#[from] std::io::Error),
    #[error("daemon did not become ready after auto-start")]
    DaemonStartTimeout,
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
            ServerCommand::Status => server_status().await?,
            ServerCommand::Stop => server_stop().await?,
        },
        Commands::Session { command } => match command {
            SessionCommand::Create { name } => create_session(name).await?,
            SessionCommand::List => list_sessions().await?,
            SessionCommand::History { session_id } => session_history(session_id).await?,
        },
        Commands::Plugin { command } => match command {
            PluginCommand::List { root } => list_plugins(&root)?,
            PluginCommand::Services { root, daemon } => {
                list_plugin_services(&root, daemon).await?;
            }
            PluginCommand::Check { root } => check_plugins(&root)?,
            PluginCommand::Invoke {
                root,
                daemon,
                plugin_id,
                interface_id,
                operation,
                payload,
            } => {
                invoke_plugin_service(
                    &root,
                    &plugin_id,
                    &interface_id,
                    &operation,
                    payload,
                    daemon,
                )
                .await?;
            }
            PluginCommand::Call {
                root,
                daemon,
                interface_id,
                operation,
                payload,
            } => call_plugin_service(&root, &interface_id, &operation, payload, daemon).await?,
            PluginCommand::Publish {
                root,
                topic,
                payload,
            } => publish_plugin_event(&root, &topic, payload)?,
        },
        Commands::Attach { session_id } => attach_session(session_id).await?,
        Commands::Tui { session_id } => {
            ensure_server_running().await?;
            bcode_tui::run(session_id).await?;
        }
        Commands::Send {
            session_id,
            message,
        } => send_message(session_id, message).await?,
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
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    Attach {
        session_id: SessionId,
    },
    Tui {
        session_id: Option<SessionId>,
    },
    Send {
        session_id: SessionId,
        message: String,
    },
}

impl Default for Commands {
    fn default() -> Self {
        Self::Tui { session_id: None }
    }
}

#[derive(Debug, Subcommand)]
enum ServerCommand {
    Start,
    Status,
    Stop,
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    Create { name: Option<String> },
    List,
    History { session_id: SessionId },
}

#[derive(Debug, Subcommand)]
enum PluginCommand {
    List {
        #[arg(long = "root")]
        root: Vec<std::path::PathBuf>,
    },
    Services {
        #[arg(long = "root")]
        root: Vec<std::path::PathBuf>,
        #[arg(long)]
        daemon: bool,
    },
    Check {
        #[arg(long = "root")]
        root: Vec<std::path::PathBuf>,
    },
    Invoke {
        #[arg(long = "root")]
        root: Vec<std::path::PathBuf>,
        #[arg(long)]
        daemon: bool,
        plugin_id: String,
        interface_id: String,
        operation: String,
        payload: Option<String>,
    },
    Call {
        #[arg(long = "root")]
        root: Vec<std::path::PathBuf>,
        #[arg(long)]
        daemon: bool,
        interface_id: String,
        operation: String,
        payload: Option<String>,
    },
    Publish {
        #[arg(long = "root")]
        root: Vec<std::path::PathBuf>,
        topic: String,
        payload: Option<String>,
    },
}

fn list_plugins(roots: &[std::path::PathBuf]) -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    let selection = bcode_plugin::PluginSelection::from(&config);
    let plugins =
        bcode_plugin::filter_selected_plugins(discover_plugins_for_cli(roots)?, &selection);

    if plugins.is_empty() {
        println!("no plugins discovered");
        return Ok(());
    }

    for plugin in plugins {
        println!(
            "{}\t{}\t{}\t{}",
            plugin.manifest.id,
            plugin.manifest.version,
            plugin.manifest.name,
            plugin.manifest_path.display()
        );
    }
    Ok(())
}

async fn list_plugin_services(roots: &[std::path::PathBuf], daemon: bool) -> Result<(), CliError> {
    if daemon {
        let services = BcodeClient::default_endpoint().plugin_services().await?;
        if services.is_empty() {
            println!("no plugin services discovered");
            return Ok(());
        }
        for service in services {
            println!(
                "{}\t{}\t{}",
                service.interface_id,
                service.plugin_id,
                service.name.unwrap_or_else(|| "<unnamed>".to_string())
            );
        }
        return Ok(());
    }

    let config = bcode_config::load_config()?;
    let selection = bcode_plugin::PluginSelection::from(&config);
    let plugins =
        bcode_plugin::filter_selected_plugins(discover_plugins_for_cli(roots)?, &selection);
    let mut has_services = false;
    for plugin in plugins {
        for service in plugin.manifest.services {
            has_services = true;
            println!(
                "{}\t{}\t{}",
                service.interface_id,
                plugin.manifest.id,
                service.name.unwrap_or_else(|| "<unnamed>".to_string())
            );
        }
    }
    if !has_services {
        println!("no plugin services discovered");
    }
    Ok(())
}

fn check_plugins(roots: &[std::path::PathBuf]) -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    let selection = bcode_plugin::PluginSelection::from(&config);
    let plugins =
        bcode_plugin::filter_selected_plugins(discover_plugins_for_cli(roots)?, &selection);
    if plugins.is_empty() {
        println!("no plugins discovered");
        return Ok(());
    }

    for plugin in plugins {
        let loaded = bcode_plugin::load_registered_plugin(&plugin)?;
        loaded.activate()?;
        loaded.deactivate()?;
        println!("{}\tOK", loaded.manifest().id);
    }
    Ok(())
}

async fn invoke_plugin_service(
    roots: &[std::path::PathBuf],
    plugin_id: &str,
    interface_id: &str,
    operation: &str,
    payload: Option<String>,
    daemon: bool,
) -> Result<(), CliError> {
    let payload = payload.unwrap_or_default().into_bytes();
    if daemon {
        let response = BcodeClient::default_endpoint()
            .invoke_plugin_service(
                plugin_id.to_string(),
                interface_id.to_string(),
                operation.to_string(),
                payload,
            )
            .await?;
        print_service_response(response);
        return Ok(());
    }

    let config = bcode_config::load_config()?;
    let selection = bcode_plugin::PluginSelection::from(&config);
    let plugins =
        bcode_plugin::filter_selected_plugins(discover_plugins_for_cli(roots)?, &selection);
    let mut host = bcode_plugin::PluginHost::load_registered_plugins(&plugins)?;
    let response = host.invoke_service(plugin_id, interface_id, operation, payload)?;
    host.deactivate_all()?;
    print_service_response(response);
    Ok(())
}

async fn call_plugin_service(
    roots: &[std::path::PathBuf],
    interface_id: &str,
    operation: &str,
    payload: Option<String>,
    daemon: bool,
) -> Result<(), CliError> {
    let payload = payload.unwrap_or_default().into_bytes();
    if daemon {
        let response = BcodeClient::default_endpoint()
            .call_plugin_service(interface_id.to_string(), operation.to_string(), payload)
            .await?;
        print_service_response(response);
        return Ok(());
    }

    let config = bcode_config::load_config()?;
    let selection = bcode_plugin::PluginSelection::from(&config);
    let plugins =
        bcode_plugin::filter_selected_plugins(discover_plugins_for_cli(roots)?, &selection);
    let mut host = bcode_plugin::PluginHost::load_registered_plugins(&plugins)?;
    let response = host.invoke_service_by_interface(interface_id, operation, payload)?;
    host.deactivate_all()?;
    print_service_response(response);
    Ok(())
}

fn print_service_response(response: impl Into<PrintableServiceResponse>) {
    let response = response.into();
    if let Some(error) = response.error {
        println!("ERROR\t{}\t{}", error.code, error.message);
    } else {
        println!("{}", String::from_utf8_lossy(&response.payload));
    }
}

struct PrintableServiceResponse {
    payload: Vec<u8>,
    error: Option<PrintableServiceError>,
}

struct PrintableServiceError {
    code: String,
    message: String,
}

impl From<bcode_plugin::ServiceResponse> for PrintableServiceResponse {
    fn from(value: bcode_plugin::ServiceResponse) -> Self {
        Self {
            payload: value.payload,
            error: value.error.map(|error| PrintableServiceError {
                code: error.code,
                message: error.message,
            }),
        }
    }
}

impl From<bcode_ipc::PluginServiceResponse> for PrintableServiceResponse {
    fn from(value: bcode_ipc::PluginServiceResponse) -> Self {
        Self {
            payload: value.payload,
            error: value.error.map(|error| PrintableServiceError {
                code: error.code,
                message: error.message,
            }),
        }
    }
}

fn publish_plugin_event(
    roots: &[std::path::PathBuf],
    topic: &str,
    payload: Option<String>,
) -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    let selection = bcode_plugin::PluginSelection::from(&config);
    let plugins =
        bcode_plugin::filter_selected_plugins(discover_plugins_for_cli(roots)?, &selection);
    let mut host = bcode_plugin::PluginHost::load_registered_plugins(&plugins)?;
    let payload = payload.unwrap_or_default().into_bytes();
    let delivered = host.publish_event(topic, &payload)?;
    host.deactivate_all()?;
    println!("delivered\t{delivered}");
    Ok(())
}

fn discover_plugins_for_cli(
    roots: &[std::path::PathBuf],
) -> Result<Vec<bcode_plugin::RegisteredPlugin>, CliError> {
    if roots.is_empty() {
        bcode_plugin::discover_plugins().map_err(CliError::Plugin)
    } else {
        bcode_plugin::discover_plugins_in_roots(roots).map_err(CliError::Plugin)
    }
}

async fn ensure_server_running() -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    if client.server_status().await.is_ok() {
        return Ok(());
    }

    let exe = std::env::current_exe()?;
    Command::new(exe)
        .args(["server", "start"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    for _ in 0..50 {
        if client.server_status().await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Err(CliError::DaemonStartTimeout)
}

async fn server_status() -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let status = client.server_status().await?;
    println!("connected clients: {}", status.connected_client_count);
    println!("sessions: {}", status.sessions.len());
    for session in status.sessions {
        let name = session.name.unwrap_or_else(|| "<unnamed>".to_string());
        println!("{}\t{}\t{} clients", session.id, name, session.client_count);
    }
    Ok(())
}

async fn server_stop() -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    client.server_stop().await?;
    println!("server stopping");
    Ok(())
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

async fn session_history(session_id: SessionId) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let history = client.session_history(session_id).await?;
    for event in history {
        print_session_event(&event);
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

async fn send_message(session_id: SessionId, message: String) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    client.send_user_message(session_id, message).await?;
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
        SessionEventKind::AssistantDelta { text } => {
            println!("#{} assistant delta: {text}", event.sequence);
        }
        SessionEventKind::AssistantMessage { text } => {
            println!("#{} assistant: {text}", event.sequence);
        }
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            tool_name,
        } => {
            println!(
                "#{} tool call requested: {tool_name} ({tool_call_id})",
                event.sequence
            );
        }
        SessionEventKind::ToolCallFinished {
            tool_call_id,
            result,
        } => {
            println!(
                "#{} tool call finished: {tool_call_id}: {result}",
                event.sequence
            );
        }
        SessionEventKind::ModelChanged { provider, model } => {
            println!("#{} model changed: {provider}/{model}", event.sequence);
        }
        SessionEventKind::SystemMessage { text } => {
            println!("#{} system: {text}", event.sequence);
        }
    }
}
