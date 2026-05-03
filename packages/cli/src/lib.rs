#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Command-line interface for Bcode.

use bcode_client::{BcodeClient, ClientError};
use bcode_ipc::{Event, PermissionSummary, default_endpoint};
use bcode_session_models::{SessionEvent, SessionEventKind, SessionId};
use clap::{Parser, Subcommand};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use thiserror::Error;
use zeroize::Zeroizing;

/// Errors returned by the CLI.
#[derive(Debug, Error)]
pub enum CliError {
    #[error("client error: {0}")]
    Client(#[from] ClientError),
    #[error("config error: {0}")]
    Config(#[from] bcode_config::ConfigError),
    #[error("server error: {0}")]
    Server(#[from] bcode_server::ServerError),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("TUI error: {0}")]
    Tui(#[from] bcode_tui::TuiError),
    #[error("plugin error: {0}")]
    Plugin(#[from] bcode_plugin::PluginLoadError),
    #[error("interrupted: {0}")]
    Signal(#[from] std::io::Error),
    #[error("daemon did not become ready after auto-start")]
    DaemonStartTimeout,
    #[error("bundled plugin install failed: {0}")]
    BundledPluginInstallFailed(String),
}

/// Parse CLI arguments and run the requested command.
///
/// # Errors
///
/// Returns an error when the requested command fails.
pub async fn run() -> Result<(), CliError> {
    let cli = Cli::parse();
    match cli.command.unwrap_or_default() {
        Commands::Server { command } => handle_server_command(command).await?,
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
                daemon,
                topic,
                payload,
            } => publish_plugin_event(&root, &topic, payload, daemon).await?,
        },
        Commands::Model { command } => handle_model_command(command).await?,
        Commands::Login { command } => handle_login_command(command)?,
        Commands::Permission { command } => match command {
            PermissionCommand::List => list_permissions().await?,
            PermissionCommand::Approve { permission_id } => {
                resolve_permission(permission_id, true).await?;
            }
            PermissionCommand::Deny { permission_id } => {
                resolve_permission(permission_id, false).await?;
            }
            PermissionCommand::AllowTool { tool_name } => {
                add_permission_rule("allow_tool", tool_name).await?;
            }
            PermissionCommand::DenyTool { tool_name } => {
                add_permission_rule("deny_tool", tool_name).await?;
            }
            PermissionCommand::AllowShellPrefix { prefix } => {
                add_permission_rule("allow_shell_command_prefix", prefix).await?;
            }
            PermissionCommand::DenyShellPrefix { prefix } => {
                add_permission_rule("deny_shell_command_prefix", prefix).await?;
            }
            PermissionCommand::AllowPathPrefix { prefix } => {
                add_permission_rule("allow_path_prefix", prefix).await?;
            }
            PermissionCommand::DenyPathPrefix { prefix } => {
                add_permission_rule("deny_path_prefix", prefix).await?;
            }
        },
        Commands::Cancel { session_id } => cancel_session_turn(session_id).await?,
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
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    Login {
        #[command(subcommand)]
        command: LoginCommand,
    },
    Permission {
        #[command(subcommand)]
        command: PermissionCommand,
    },
    Cancel {
        session_id: SessionId,
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
    Start {
        #[arg(long)]
        foreground: bool,
    },
    Run,
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
enum ModelCommand {
    List,
    Capabilities,
    Set {
        session_id: SessionId,
        model_id: String,
        #[arg(long)]
        provider: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum LoginCommand {
    Openai {
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long, default_value = "bcode-openai")]
        profile: String,
        #[arg(long)]
        vault: Option<PathBuf>,
        #[arg(long)]
        recipient_key: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum PermissionCommand {
    List,
    Approve { permission_id: String },
    Deny { permission_id: String },
    AllowTool { tool_name: String },
    DenyTool { tool_name: String },
    AllowShellPrefix { prefix: String },
    DenyShellPrefix { prefix: String },
    AllowPathPrefix { prefix: String },
    DenyPathPrefix { prefix: String },
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
        #[arg(long)]
        daemon: bool,
        topic: String,
        payload: Option<String>,
    },
}

async fn handle_server_command(command: ServerCommand) -> Result<(), CliError> {
    match command {
        ServerCommand::Start { foreground } => {
            if foreground {
                run_server_foreground().await?;
            } else {
                start_server_daemon(false).await?;
            }
        }
        ServerCommand::Run => run_server_foreground().await?,
        ServerCommand::Status => server_status().await?,
        ServerCommand::Stop => server_stop().await?,
    }
    Ok(())
}

async fn handle_model_command(command: ModelCommand) -> Result<(), CliError> {
    ensure_server_running().await?;
    match command {
        ModelCommand::List => list_models().await?,
        ModelCommand::Capabilities => model_capabilities().await?,
        ModelCommand::Set {
            session_id,
            provider,
            model_id,
        } => set_session_model(session_id, provider, model_id).await?,
    }
    Ok(())
}

fn handle_login_command(command: LoginCommand) -> Result<(), CliError> {
    match command {
        LoginCommand::Openai {
            api_key,
            base_url,
            profile,
            vault,
            recipient_key,
            model,
        } => login_openai(api_key, base_url, profile, vault, recipient_key, model)?,
    }
    Ok(())
}

fn login_openai(
    api_key: Option<String>,
    base_url: Option<String>,
    profile: String,
    vault: Option<PathBuf>,
    recipient_key: Option<String>,
    model: Option<String>,
) -> Result<(), CliError> {
    let vault_path = vault.unwrap_or_else(bcode_config::default_auth_vault_path);
    let store =
        sshenv_vault::SshenvStore::new(sshenv_vault::SshenvStoreConfig::new(vault_path.clone()));
    if !vault_path.exists() {
        let recipient_key = resolve_recipient_key(recipient_key)?;
        store.init(&recipient_key).map_err(|error| {
            CliError::BundledPluginInstallFailed(format!(
                "failed to initialize auth vault: {error}"
            ))
        })?;
    }

    let api_key = match api_key {
        Some(api_key) => api_key,
        None => rpassword::prompt_password("OpenAI API key: ")?,
    };
    store
        .set_secret(&profile, "BCODE_OPENAI_API_KEY", Zeroizing::new(api_key))
        .map_err(|error| {
            CliError::BundledPluginInstallFailed(format!("failed to store OpenAI API key: {error}"))
        })?;
    if let Some(base_url) = base_url {
        store
            .set_secret(&profile, "BCODE_OPENAI_BASE_URL", Zeroizing::new(base_url))
            .map_err(|error| {
                CliError::BundledPluginInstallFailed(format!(
                    "failed to store OpenAI base URL: {error}"
                ))
            })?;
    }

    let config_path = bcode_config::set_openai_sshenv_auth(profile, vault_path, model)?;
    println!(
        "OpenAI credentials saved; config updated: {}",
        config_path.display()
    );
    Ok(())
}

fn resolve_recipient_key(recipient_key: Option<String>) -> Result<String, CliError> {
    if let Some(recipient_key) = recipient_key {
        return public_key_line_from_path_or_literal(&recipient_key);
    }
    let Some(path) = sshenv_vault::identity::discover_public_key_paths()
        .into_iter()
        .next()
    else {
        return Err(CliError::BundledPluginInstallFailed(
            "no SSH public key found; pass --recipient-key <path-or-public-key>".to_string(),
        ));
    };
    public_key_line_from_path_or_literal(&path.display().to_string())
}

fn public_key_line_from_path_or_literal(value: &str) -> Result<String, CliError> {
    if value.starts_with("ssh-") {
        return Ok(value.to_string());
    }
    let contents = std::fs::read_to_string(value)?;
    contents
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToString::to_string)
        .ok_or_else(|| {
            CliError::BundledPluginInstallFailed(format!("no public key line found in {value}"))
        })
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

async fn publish_plugin_event(
    roots: &[std::path::PathBuf],
    topic: &str,
    payload: Option<String>,
    daemon: bool,
) -> Result<(), CliError> {
    let payload = payload.unwrap_or_default().into_bytes();
    if daemon {
        let delivered = BcodeClient::default_endpoint()
            .publish_plugin_event(topic.to_string(), payload)
            .await?;
        println!("delivered\t{delivered}");
        return Ok(());
    }

    let config = bcode_config::load_config()?;
    let selection = bcode_plugin::PluginSelection::from(&config);
    let plugins =
        bcode_plugin::filter_selected_plugins(discover_plugins_for_cli(roots)?, &selection);
    let mut host = bcode_plugin::PluginHost::load_registered_plugins(&plugins)?;
    let delivered = host.publish_event(topic, &payload)?;
    host.deactivate_all()?;
    println!("delivered\t{delivered}");
    Ok(())
}

async fn list_models() -> Result<(), CliError> {
    let response = call_model_provider_service(bcode_model::OP_MODELS).await?;
    if let Some(error) = response.error {
        println!("ERROR\t{}\t{}", error.code, error.message);
        return Ok(());
    }
    let models: bcode_model::ModelList = serde_json::from_slice(&response.payload)?;
    for model in models.models {
        let default_marker = if model.is_default { "\tdefault" } else { "" };
        println!(
            "{}\t{}{}",
            model.model_id, model.display_name, default_marker
        );
    }
    Ok(())
}

async fn set_session_model(
    session_id: SessionId,
    provider_plugin_id: Option<String>,
    model_id: String,
) -> Result<(), CliError> {
    BcodeClient::default_endpoint()
        .set_session_model(session_id, provider_plugin_id, model_id)
        .await?;
    println!("session model set");
    Ok(())
}

async fn model_capabilities() -> Result<(), CliError> {
    let response = call_model_provider_service(bcode_model::OP_CAPABILITIES).await?;
    if let Some(error) = response.error {
        println!("ERROR\t{}\t{}", error.code, error.message);
        return Ok(());
    }
    let capabilities: bcode_model::ProviderCapabilities =
        serde_json::from_slice(&response.payload)?;
    println!(
        "{}\t{}",
        capabilities.provider_id, capabilities.display_name
    );
    for capability in capabilities.capabilities {
        println!("capability\t{capability:?}");
    }
    Ok(())
}

async fn call_model_provider_service(
    operation: &str,
) -> Result<bcode_ipc::PluginServiceResponse, CliError> {
    let config = bcode_config::load_config()?;
    let client = BcodeClient::default_endpoint();
    if let Some(provider_plugin_id) = config.model.provider_plugin_id {
        client
            .invoke_plugin_service(
                provider_plugin_id,
                bcode_model::MODEL_PROVIDER_INTERFACE_ID.to_string(),
                operation.to_string(),
                Vec::new(),
            )
            .await
            .map_err(CliError::from)
    } else {
        client
            .call_plugin_service(
                bcode_model::MODEL_PROVIDER_INTERFACE_ID.to_string(),
                operation.to_string(),
                Vec::new(),
            )
            .await
            .map_err(CliError::from)
    }
}

fn discover_plugins_for_cli(
    roots: &[std::path::PathBuf],
) -> Result<Vec<bcode_plugin::RegisteredPlugin>, CliError> {
    if roots.is_empty() {
        ensure_bundled_plugins_installed()?;
        bcode_plugin::discover_plugins().map_err(CliError::Plugin)
    } else {
        bcode_plugin::discover_plugins_in_roots(roots).map_err(CliError::Plugin)
    }
}

#[derive(Debug, Clone, Copy)]
struct BundledPluginSpec {
    id: &'static str,
    package: &'static str,
    library_stem: &'static str,
    name: &'static str,
    services: &'static [BundledPluginServiceSpec],
}

#[derive(Debug, Clone, Copy)]
struct BundledPluginServiceSpec {
    interface_id: &'static str,
    name: &'static str,
    description: &'static str,
}

const BUNDLED_FILESYSTEM_SERVICES: &[BundledPluginServiceSpec] = &[
    BundledPluginServiceSpec {
        interface_id: "bcode.filesystem/v1",
        name: "Filesystem",
        description: "Filesystem read/write utility service",
    },
    BundledPluginServiceSpec {
        interface_id: "bcode.tool/v1",
        name: "Filesystem Tools",
        description: "Model-callable filesystem tools",
    },
];
const BUNDLED_SHELL_SERVICES: &[BundledPluginServiceSpec] = &[BundledPluginServiceSpec {
    interface_id: "bcode.tool/v1",
    name: "Shell Tools",
    description: "Permissioned model-callable shell execution tools",
}];
const BUNDLED_OPENAI_SERVICES: &[BundledPluginServiceSpec] = &[BundledPluginServiceSpec {
    interface_id: "bcode.model-provider/v1",
    name: "OpenAI-Compatible Model Provider",
    description: "OpenAI-compatible chat-completions model provider",
}];
const BUNDLED_PLUGIN_SPECS: &[BundledPluginSpec] = &[
    BundledPluginSpec {
        id: "bcode.filesystem",
        package: "bcode_filesystem_plugin",
        library_stem: "bcode_filesystem_plugin",
        name: "Bcode Filesystem Plugin",
        services: BUNDLED_FILESYSTEM_SERVICES,
    },
    BundledPluginSpec {
        id: "bcode.shell",
        package: "bcode_shell_plugin",
        library_stem: "bcode_shell_plugin",
        name: "Bcode Shell Plugin",
        services: BUNDLED_SHELL_SERVICES,
    },
    BundledPluginSpec {
        id: "bcode.openai-compatible",
        package: "bcode_openai_compatible_provider_plugin",
        library_stem: "bcode_openai_compatible_provider_plugin",
        name: "Bcode OpenAI-Compatible Provider",
        services: BUNDLED_OPENAI_SERVICES,
    },
];

fn ensure_bundled_plugins_installed() -> Result<(), CliError> {
    if std::env::var_os("BCODE_SKIP_BUNDLED_PLUGIN_INSTALL").is_some() {
        return Ok(());
    }
    let executable_dir = executable_dir()?;
    if bundled_plugins_installed(&executable_dir) {
        return Ok(());
    }
    build_missing_bundled_plugin_libraries(&executable_dir)?;
    for spec in BUNDLED_PLUGIN_SPECS {
        install_bundled_plugin(&executable_dir, spec)?;
    }
    Ok(())
}

fn executable_dir() -> Result<PathBuf, CliError> {
    std::env::current_exe()?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            CliError::BundledPluginInstallFailed(
                "current executable has no parent directory".to_string(),
            )
        })
}

fn build_missing_bundled_plugin_libraries(executable_dir: &Path) -> Result<(), CliError> {
    if bundled_plugin_libraries_exist(executable_dir) {
        return Ok(());
    }
    let Some(workspace_root) = workspace_root_from_executable_dir(executable_dir) else {
        return Err(CliError::BundledPluginInstallFailed(format!(
            "bundled plugin libraries are missing from {} and no workspace root was found",
            executable_dir.display()
        )));
    };
    let status = Command::new("cargo")
        .arg("build")
        .arg("--quiet")
        .args(
            BUNDLED_PLUGIN_SPECS
                .iter()
                .flat_map(|spec| ["-p", spec.package]),
        )
        .current_dir(&workspace_root)
        .status()?;
    if status.success() && bundled_plugin_libraries_exist(executable_dir) {
        Ok(())
    } else {
        Err(CliError::BundledPluginInstallFailed(format!(
            "cargo build did not produce all bundled plugin libraries in {}",
            executable_dir.display()
        )))
    }
}

fn bundled_plugin_libraries_exist(executable_dir: &Path) -> bool {
    BUNDLED_PLUGIN_SPECS.iter().all(|spec| {
        executable_dir
            .join(dynamic_library_name(spec.library_stem))
            .exists()
    })
}

fn bundled_plugins_installed(executable_dir: &Path) -> bool {
    BUNDLED_PLUGIN_SPECS.iter().all(|spec| {
        let library_name = dynamic_library_name(spec.library_stem);
        let plugin_dir = executable_dir.join("plugins").join(spec.id);
        plugin_dir
            .join(bcode_plugin::DEFAULT_PLUGIN_MANIFEST_FILE)
            .exists()
            && plugin_dir.join(library_name).exists()
    })
}

fn workspace_root_from_executable_dir(executable_dir: &Path) -> Option<PathBuf> {
    let target_dir = executable_dir.parent()?;
    let workspace_root = target_dir.parent()?;
    workspace_root
        .join("Cargo.toml")
        .exists()
        .then(|| workspace_root.to_path_buf())
}

fn install_bundled_plugin(executable_dir: &Path, spec: &BundledPluginSpec) -> Result<(), CliError> {
    let library_name = dynamic_library_name(spec.library_stem);
    let source_library = executable_dir.join(&library_name);
    if !source_library.exists() {
        return Err(CliError::BundledPluginInstallFailed(format!(
            "bundled plugin library is missing: {}",
            source_library.display()
        )));
    }
    let plugin_dir = executable_dir.join("plugins").join(spec.id);
    std::fs::create_dir_all(&plugin_dir)?;
    std::fs::copy(&source_library, plugin_dir.join(&library_name))?;
    std::fs::write(
        plugin_dir.join(bcode_plugin::DEFAULT_PLUGIN_MANIFEST_FILE),
        bundled_plugin_manifest(spec, &library_name),
    )?;
    Ok(())
}

fn bundled_plugin_manifest(spec: &BundledPluginSpec, library_name: &str) -> String {
    let mut manifest = format!(
        "id = \"{}\"\nname = \"{}\"\nversion = \"0.0.1\"\n\n",
        spec.id, spec.name
    );
    for service in spec.services {
        let _ = write!(
            manifest,
            "[[services]]\ndescription = \"{}\"\ninterface_id = \"{}\"\nname = \"{}\"\n\n",
            service.description, service.interface_id, service.name
        );
    }
    let _ = write!(
        manifest,
        "[runtime]\ntype = \"native\"\nabi_version = 1\nlibrary = \"{library_name}\"\nevent_symbol = \"bcode_plugin_handle_event_v1\"\nservice_symbol = \"bcode_plugin_invoke_service_v1\"\n"
    );
    manifest
}

fn dynamic_library_name(library_stem: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{library_stem}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{library_stem}.dylib")
    } else {
        format!("lib{library_stem}.so")
    }
}

async fn ensure_server_running() -> Result<(), CliError> {
    start_server_daemon(true).await
}

async fn run_server_foreground() -> Result<(), CliError> {
    ensure_bundled_plugins_installed()?;
    bcode_server::run(default_endpoint()).await?;
    Ok(())
}

async fn start_server_daemon(quiet: bool) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    if client.server_status().await.is_ok() {
        if !quiet {
            println!("server already running");
        }
        return Ok(());
    }

    ensure_bundled_plugins_installed()?;

    let exe = std::env::current_exe()?;
    Command::new(exe)
        .args(["server", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    wait_for_server_ready(&client).await?;
    if !quiet {
        println!("server started");
    }
    Ok(())
}

async fn wait_for_server_ready(client: &BcodeClient) -> Result<(), CliError> {
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
    println!(
        "model provider: {}",
        status
            .selected_provider_plugin_id
            .as_deref()
            .unwrap_or("<auto>")
    );
    println!(
        "model: {}",
        status.selected_model_id.as_deref().unwrap_or("<default>")
    );
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

async fn cancel_session_turn(session_id: SessionId) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    if client.cancel_session_turn(session_id).await? {
        println!("turn cancellation requested");
    } else {
        println!("no active turn");
    }
    Ok(())
}

async fn list_permissions() -> Result<(), CliError> {
    let permissions = BcodeClient::default_endpoint().list_permissions().await?;
    for permission in permissions {
        print_permission(&permission);
    }
    Ok(())
}

async fn resolve_permission(permission_id: String, approved: bool) -> Result<(), CliError> {
    let resolved = BcodeClient::default_endpoint()
        .resolve_permission(permission_id, approved)
        .await?;
    println!("resolved: {resolved}");
    Ok(())
}

async fn add_permission_rule(kind: &str, value: String) -> Result<(), CliError> {
    let config_path = BcodeClient::default_endpoint()
        .add_permission_rule(kind.to_string(), value)
        .await?;
    println!("permission rule added: {config_path}");
    Ok(())
}

fn print_permission(permission: &PermissionSummary) {
    println!(
        "{}\t{}\t{}\t{}\t{}",
        permission.permission_id,
        permission.session_id,
        permission.tool_call_id,
        permission.tool_name,
        permission.arguments_json
    );
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
            arguments_json,
        } => {
            println!(
                "#{} tool call requested: {tool_name} ({tool_call_id}) {}",
                event.sequence, arguments_json
            );
        }
        SessionEventKind::ToolCallFinished {
            tool_call_id,
            result,
            is_error,
        } => {
            let status = if *is_error { "error" } else { "ok" };
            println!(
                "#{} tool call finished ({status}): {tool_call_id}: {result}",
                event.sequence
            );
        }
        SessionEventKind::PermissionRequested {
            permission_id,
            tool_call_id,
            tool_name,
            arguments_json,
        } => {
            println!(
                "#{} permission requested: {permission_id} {tool_name} ({tool_call_id}) {}",
                event.sequence, arguments_json
            );
        }
        SessionEventKind::PermissionResolved {
            permission_id,
            approved,
        } => {
            println!(
                "#{} permission resolved: {permission_id} approved={approved}",
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
