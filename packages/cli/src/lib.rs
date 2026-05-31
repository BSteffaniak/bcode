#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Command-line interface for Bcode.

mod blims;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bcode_client::{BcodeClient, ClientError};
use bcode_config::AuthMode;
use bcode_ipc::{Event, PermissionSummary, ServerStatus, default_endpoint};
use bcode_session_import::{
    DiscoverImportableSessionsRequest, DiscoverImportableSessionsResponse,
    ListImportSourcesResponse, OP_DISCOVER_IMPORTABLE_SESSIONS, OP_LIST_IMPORT_SOURCES,
    SESSION_IMPORT_INTERFACE_ID,
};
use bcode_session_models::{
    SessionEvent, SessionEventKind, SessionHistoryCursor, SessionHistoryDirection,
    SessionHistoryQuery, SessionId,
};
use bcode_worktree_models::{
    WorktreeBaseRef, WorktreeCreateRequest, WorktreeListRequest, WorktreeRemoveRequest,
};
use clap::{Parser, Subcommand, ValueEnum};
use rand::TryRngCore as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tracing_subscriber::util::SubscriberInitExt as _;
use zeroize::Zeroizing;

const SESSION_CLI_PAGE_LIMIT: usize = 500;

/// Errors returned by the CLI.
#[derive(Debug, Error)]
pub enum CliError {
    #[error("client error: {0}")]
    Client(#[from] ClientError),
    #[error("config error: {0}")]
    Config(#[from] bcode_config::ConfigError),
    #[error("server error: {0}")]
    Server(#[from] bcode_server::ServerError),
    #[error("session store error: {0}")]
    SessionStore(#[from] bcode_session::SessionStoreError),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("TUI error: {0}")]
    Tui(#[from] bcode_tui::TuiError),
    #[error("plugin error: {0}")]
    Plugin(#[from] bcode_plugin::PluginLoadError),
    #[error("interrupted: {0}")]
    Signal(#[from] std::io::Error),
    #[error(
        "daemon did not become ready after auto-start; log: {log_path}\ntry `bcode server run` to see startup failures in the foreground\n\n{recent_log}"
    )]
    DaemonStartTimeout {
        log_path: String,
        recent_log: String,
    },
    #[error(
        "daemon exited before becoming ready ({status}); log: {log_path}\ntry `bcode server run` to see startup failures in the foreground\n\n{recent_log}"
    )]
    DaemonExited {
        status: String,
        log_path: String,
        recent_log: String,
    },
    #[error(
        "daemon became ready but failed a follow-up health check; log: {log_path}\ntry `bcode server run` to see startup failures in the foreground\n\n{recent_log}"
    )]
    DaemonHealthCheckFailed {
        log_path: String,
        recent_log: String,
    },
    #[error("--new cannot be combined with a subcommand")]
    NewSessionWithCommand,
    #[error("{0}")]
    LoginProfile(String),
    #[error("Blims error: {0}")]
    Blims(String),
    #[error("bundled plugin install failed: {0}")]
    BundledPluginInstallFailed(String),
}

/// Parse CLI arguments and run the requested command.
///
/// # Errors
///
/// Returns an error when the requested command fails.
pub async fn run() -> Result<(), CliError> {
    init_tracing();
    let cli = Cli::parse();
    handle_cli(cli).await
}

async fn handle_cli(cli: Cli) -> Result<(), CliError> {
    let _config_override = cli.profile.as_deref().map(|profile| {
        bcode_config::push_process_config_overrides(
            bcode_config::ConfigLoadOverrides::from_env_with_cli(
                None,
                Some(bcode_config::model_profile_override_toml(profile)),
            ),
        )
    });
    if cli.new {
        if cli.command.is_some() {
            return Err(CliError::NewSessionWithCommand);
        }
        run_new_session_tui(cli.worktree).await?;
        return Ok(());
    }
    match cli.command.unwrap_or_default() {
        Commands::Server { command } => handle_server_command(command).await?,
        Commands::Session { command } => match command {
            SessionCommand::Create { name } => create_session(name).await?,
            SessionCommand::List => list_sessions().await?,
            SessionCommand::Rename { session_id, name } => rename_session(session_id, name).await?,
            SessionCommand::Delete { session_id } => delete_session(session_id).await?,
            SessionCommand::History { session_id } => session_history(session_id).await?,
            SessionCommand::Export { session_id, format } => {
                session_export(session_id, format).await?;
            }
            SessionCommand::Timeline { session_id } => session_timeline(session_id).await?,
            SessionCommand::Diagnose { session_id, json } => {
                session_diagnose(session_id, json).await?;
            }
            SessionCommand::Doctor {
                session_id,
                fix,
                force,
                json,
            } => session_doctor(session_id, fix, force, json)?,
            SessionCommand::Migrate { command } => handle_session_migrate_command(command)?,
            SessionCommand::Import { command } => handle_session_import_command(command).await?,
            SessionCommand::Reindex { session_id } => session_reindex(session_id)?,
            SessionCommand::Repair { session_id } => session_repair(session_id)?,
        },
        Commands::Worktree { command } => handle_worktree_command(command).await?,
        Commands::Blims { command } => blims::handle_blims_command(command).await?,
        Commands::Migrate { command } => handle_migrate_command(command)?,
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
        Commands::Auth { command } => handle_auth_command(command)?,
        Commands::Login { command } => handle_login_command(command).await?,
        Commands::Provider { command } => handle_provider_command(command)?,
        Commands::Permission { command } => handle_permission_command(command).await?,
        Commands::RuntimeWork { command } => handle_runtime_work_command(command).await?,
        command => handle_session_io_command(command).await?,
    }
    Ok(())
}

async fn handle_session_io_command(command: Commands) -> Result<(), CliError> {
    match command {
        Commands::Cancel {
            session_id,
            clear_queue,
        } => cancel_session_turn(session_id, clear_queue).await?,
        Commands::Attach { session_id } => attach_session(session_id).await?,
        Commands::Tui { session_id } => {
            ensure_server_running().await?;
            bcode_tui::run(session_id).await?;
        }
        Commands::Send {
            session_id,
            message,
        } => send_message(session_id, message).await?,
        Commands::Server { .. }
        | Commands::Session { .. }
        | Commands::Worktree { .. }
        | Commands::Blims { .. }
        | Commands::Migrate { .. }
        | Commands::Plugin { .. }
        | Commands::Model { .. }
        | Commands::Auth { .. }
        | Commands::Login { .. }
        | Commands::Provider { .. }
        | Commands::Permission { .. }
        | Commands::RuntimeWork { .. } => unreachable!("handled by handle_cli"),
    }
    Ok(())
}

async fn handle_permission_command(command: PermissionCommand) -> Result<(), CliError> {
    match command {
        PermissionCommand::List => list_permissions().await?,
        PermissionCommand::Approve { permission_id } => {
            resolve_permission(permission_id, true).await?;
        }
        PermissionCommand::Deny { permission_id } => {
            resolve_permission(permission_id, false).await?;
        }
        PermissionCommand::Add {
            agent,
            category,
            pattern,
            action,
        } => {
            add_permission_rule(&agent, &category, pattern, &action).await?;
        }
    }
    Ok(())
}

fn init_tracing() {
    let filter = std::env::var("BCODE_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .unwrap_or_else(|| {
            if std::env::var_os("BCODE_STARTUP_TRACE").is_some() {
                "bcode_server::startup=debug,bcode_plugin::startup=debug".to_string()
            } else {
                "off".to_string()
            }
        });
    let env_filter = tracing_subscriber::EnvFilter::try_new(filter).unwrap_or_else(|error| {
        eprintln!("bcode warning: invalid log filter; logging disabled: {error}");
        tracing_subscriber::EnvFilter::new("off")
    });
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .finish();
    let _ = subscriber.try_init();
}

#[derive(Debug, Parser)]
#[command(name = "bcode", version, about = "TUI-first coding agent")]
struct Cli {
    /// Create a new session and open it in the terminal UI.
    #[arg(short = 'n', long = "new")]
    new: bool,
    /// Create a new session in a new worktree and open it in the terminal UI.
    #[arg(long, value_name = "NAME", requires = "new")]
    worktree: Option<String>,
    /// Select a model profile from configuration for this client connection.
    #[arg(long, value_name = "MODEL_PROFILE")]
    profile: Option<String>,
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
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
    Blims {
        #[command(subcommand)]
        command: blims::BlimsCommand,
    },
    Migrate {
        #[command(subcommand)]
        command: MigrateCommand,
    },
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    Login {
        #[command(subcommand)]
        command: LoginCommand,
    },
    Provider {
        #[command(subcommand)]
        command: ProviderCommand,
    },
    Permission {
        #[command(subcommand)]
        command: PermissionCommand,
    },
    RuntimeWork {
        #[command(subcommand)]
        command: RuntimeWorkCommand,
    },
    Cancel {
        session_id: SessionId,
        #[arg(long)]
        clear_queue: bool,
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
enum RuntimeWorkCommand {
    List {
        session_id: SessionId,
    },
    Cancel {
        session_id: SessionId,
        work_id: String,
    },
    History {
        session_id: SessionId,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    Watch {
        session_id: SessionId,
    },
}

#[derive(Debug, Subcommand)]
enum ServerCommand {
    Start {
        #[arg(long)]
        foreground: bool,
    },
    Run,
    Status {
        #[arg(long)]
        verbose: bool,
    },
    Metrics {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        report: bool,
        #[arg(long)]
        dashboard: Option<PathBuf>,
    },
    Diagnose {
        #[arg(long)]
        json: bool,
    },
    Stop,
    Cleanup,
    StopAll,
}

#[derive(Debug, Clone, Copy, Subcommand)]
enum MigrateCommand {
    Status {
        #[arg(long)]
        json: bool,
    },
    Plan {
        #[arg(long)]
        json: bool,
    },
    Apply {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        backup: bool,
    },
}

#[derive(Debug, Subcommand)]
enum WorktreeCommand {
    List {
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    Create {
        name: String,
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(long)]
        path: Option<PathBuf>,
        #[arg(long)]
        session: Option<SessionId>,
        #[arg(long)]
        new_session: bool,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        new_branch: Option<String>,
        #[arg(long, value_enum)]
        base: Option<WorktreeBaseRefArg>,
        #[arg(long)]
        detach: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        no_setup: bool,
        #[arg(long)]
        json: bool,
    },
    Attach {
        session_id: SessionId,
        path: PathBuf,
    },
    Remove {
        path: PathBuf,
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum WorktreeBaseRefArg {
    Auto,
    DefaultBranch,
    Head,
}

impl WorktreeBaseRefArg {
    const fn into_config(self) -> bcode_config::WorktreeBaseRefConfig {
        match self {
            Self::Auto => bcode_config::WorktreeBaseRefConfig::Auto,
            Self::DefaultBranch => bcode_config::WorktreeBaseRefConfig::DefaultBranch,
            Self::Head => bcode_config::WorktreeBaseRefConfig::Head,
        }
    }
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    Create {
        name: Option<String>,
    },
    List,
    Rename {
        session_id: SessionId,
        name: String,
    },
    Delete {
        session_id: SessionId,
    },
    History {
        session_id: SessionId,
    },
    Export {
        session_id: SessionId,
        #[arg(long, value_enum, default_value_t = SessionExportFormat::Jsonl)]
        format: SessionExportFormat,
    },
    Timeline {
        session_id: SessionId,
    },
    Diagnose {
        session_id: SessionId,
        #[arg(long)]
        json: bool,
    },
    Doctor {
        session_id: Option<SessionId>,
        #[arg(long)]
        fix: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    Migrate {
        #[command(subcommand)]
        command: SessionMigrateCommand,
    },
    Import {
        #[command(subcommand)]
        command: SessionImportCommand,
    },
    Reindex {
        session_id: Option<SessionId>,
    },
    Repair {
        session_id: SessionId,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum SessionImportCommand {
    Sources,
    Discover {
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        diagnostics: bool,
    },
    Open {
        #[arg(long, default_value = "pi")]
        source: String,
        external_session_id: String,
    },
}

#[derive(Debug, Clone, Copy, Subcommand)]
enum SessionMigrateCommand {
    Status {
        #[arg(long)]
        json: bool,
    },
    Plan {
        #[arg(long)]
        json: bool,
    },
    Apply {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        backup: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SessionExportFormat {
    Jsonl,
}

#[derive(Debug, Subcommand)]
enum ModelCommand {
    List,
    Capabilities,
    Validate,
    Set {
        session_id: SessionId,
        model_id: String,
        #[arg(long)]
        provider: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    Status,
    Login {
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        vault: Option<PathBuf>,
        #[arg(long)]
        recipient_key: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ProviderCommand {
    Configure {
        #[command(subcommand)]
        command: ProviderConfigureCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ProviderConfigureCommand {
    /// Configure Amazon Bedrock using AWS's default credential chain.
    Bedrock {
        /// Bcode model profile name to create and select.
        #[arg(long, default_value = "bedrock")]
        profile: String,
        /// AWS shared-config profile name to use for credentials/region.
        #[arg(long)]
        aws_profile: Option<String>,
        /// AWS region for Bedrock Runtime.
        #[arg(long)]
        region: Option<String>,
        /// Optional Bedrock Runtime endpoint override.
        #[arg(long)]
        endpoint_url: Option<String>,
        /// Bedrock model ID or inference profile ID to use by default.
        #[arg(long)]
        model: String,
        /// Additional model IDs to show in `bcode model list`.
        #[arg(long = "model-id")]
        model_ids: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum LoginCommand {
    Openai {
        /// Store an `OpenAI` platform API key instead of using `ChatGPT` subscription OAuth.
        #[arg(long)]
        api_key: Option<String>,
        /// Store an OpenAI-compatible API base URL for API-key mode.
        #[arg(long)]
        base_url: Option<String>,
        /// Force `ChatGPT` subscription OAuth mode.
        #[arg(long)]
        chatgpt: bool,
        /// Use browser OAuth with a localhost callback. This is the default.
        #[arg(long)]
        browser: bool,
        /// Use device-code login. Requires `Codex` device authorization enabled in `ChatGPT` settings.
        #[arg(long)]
        headless: bool,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        vault: Option<PathBuf>,
        #[arg(long)]
        recipient_key: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
    /// Login for xAI (Grok) using the OpenAI-compatible provider.
    Xai {
        /// Store an xAI API key.
        #[arg(long)]
        api_key: Option<String>,
        /// Store an xAI-compatible API base URL (defaults to <https://api.x.ai/v1>).
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        vault: Option<PathBuf>,
        #[arg(long)]
        recipient_key: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
}

const OPENAI_CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_CODEX_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const OPENAI_CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_CODEX_SCOPE: &str = "openid profile email offline_access";
const OPENAI_CODEX_OAUTH_PORT: u16 = 1455;

#[derive(Debug, Deserialize)]
struct OpenAiOauthTokenResponse {
    access_token: String,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OpenAiDeviceUserCodeResponse {
    device_auth_id: String,
    user_code: String,
    interval: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiDeviceTokenResponse {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenAiLoginFlow {
    Browser,
    DeviceCode,
}

#[derive(Debug, Subcommand)]
enum PermissionCommand {
    List,
    Approve {
        permission_id: String,
    },
    Deny {
        permission_id: String,
    },
    /// Add or replace a permission rule under `[agent.<agent_id>.permission.<category>]`.
    Add {
        /// Agent ID that owns the rule (for example `build` or `plan`).
        #[arg(long)]
        agent: String,
        /// Permission category: `bash`, `read`, `write`, or `edit`.
        #[arg(long)]
        category: String,
        /// Glob pattern to match.
        #[arg(long)]
        pattern: String,
        /// Action: `allow`, `ask`, or `deny`.
        #[arg(long)]
        action: String,
    },
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
        ServerCommand::Status { verbose } => server_status(verbose).await?,
        ServerCommand::Metrics {
            json,
            report,
            dashboard,
        } => server_metrics(json, report, dashboard).await?,
        ServerCommand::Diagnose { json } => server_diagnose(json).await?,
        ServerCommand::Stop => server_stop().await?,
        ServerCommand::Cleanup => server_cleanup(false).await?,
        ServerCommand::StopAll => server_cleanup(true).await?,
    }
    Ok(())
}

async fn handle_model_command(command: ModelCommand) -> Result<(), CliError> {
    ensure_server_running().await?;
    match command {
        ModelCommand::List => list_models().await?,
        ModelCommand::Capabilities => model_capabilities().await?,
        ModelCommand::Validate => model_validate_config().await?,
        ModelCommand::Set {
            session_id,
            provider,
            model_id,
        } => set_session_model(session_id, provider, model_id).await?,
    }
    Ok(())
}

fn handle_provider_command(command: ProviderCommand) -> Result<(), CliError> {
    match command {
        ProviderCommand::Configure {
            command:
                ProviderConfigureCommand::Bedrock {
                    profile,
                    aws_profile,
                    region,
                    endpoint_url,
                    model,
                    mut model_ids,
                },
        } => {
            if !model_ids.contains(&model) {
                model_ids.insert(0, model.clone());
            }
            let config_path = bcode_config::set_bedrock_model_profile(
                &profile,
                model,
                aws_profile,
                region,
                endpoint_url.as_deref(),
                &model_ids,
            )?;
            println!(
                "Bedrock provider profile '{profile}' configured; config updated: {}",
                config_path.display()
            );
        }
    }
    Ok(())
}

fn handle_auth_command(command: AuthCommand) -> Result<(), CliError> {
    match command {
        AuthCommand::Status => auth_status(),
        AuthCommand::Login {
            profile,
            vault,
            recipient_key,
        } => auth_login(profile, vault, recipient_key),
    }
}

fn auth_status() -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    let selection = config.resolved_model_selection();
    let Some(auth_profile_name) = active_login_auth_profile(&config) else {
        println!("No active auth profile selected.");
        return Ok(());
    };
    let Some(auth_profile) = config.auth.profiles.get(&auth_profile_name) else {
        println!("Active auth profile: {auth_profile_name}");
        println!("Status: not declared in config");
        return Ok(());
    };
    let resolved = bcode_provider_auth::resolve_auth_profile(&auth_profile_name, auth_profile);
    println!("Auth profile: {auth_profile_name}");
    println!("Backend: {}", auth_profile.backend);
    if let Some(scheme) = &resolved.auth.scheme {
        println!("Scheme: {scheme}");
    }
    if let Some(provider) = auth_profile.settings.get("provider") {
        println!("Provider: {provider}");
    }
    if let Some(provider_plugin_id) = &selection.provider_plugin_id {
        println!("Provider plugin: {provider_plugin_id}");
    }
    match (&selection.selected_model_id, &selection.model_id) {
        (Some(configured_model), Some(resolved_model)) if configured_model != resolved_model => {
            println!("Configured model: {configured_model}");
            println!("Resolved model: {resolved_model}");
        }
        (_, Some(model_id)) => println!("Model: {model_id}"),
        (Some(model_id), None) => println!("Configured model: {model_id}"),
        (None, None) => {}
    }
    if !selection.request.is_empty() {
        println!("Request options:");
        for (key, value) in &selection.request {
            println!("  {key}: {}", format_provider_request_value(value));
        }
    }
    if resolved.auth.storage.is_empty() {
        println!("Credentials: no mapped credentials");
    } else {
        println!("Credentials:");
        for (credential, storage) in &resolved.auth.storage {
            let present = resolved.auth.credentials.contains_key(credential);
            println!(
                "  {credential}: {} ({}/{})",
                if present { "present" } else { "missing" },
                storage.backend,
                storage.key
            );
        }
    }
    Ok(())
}

fn format_provider_request_value(value: &bcode_model::ProviderRequestValue) -> String {
    match value {
        bcode_model::ProviderRequestValue::Null => "null".to_string(),
        bcode_model::ProviderRequestValue::Bool(value) => value.to_string(),
        bcode_model::ProviderRequestValue::Number(value)
        | bcode_model::ProviderRequestValue::String(value) => value.clone(),
        bcode_model::ProviderRequestValue::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(format_provider_request_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        bcode_model::ProviderRequestValue::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!("{key}: {}", format_provider_request_value(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn auth_login(
    profile: Option<String>,
    vault: Option<PathBuf>,
    recipient_key: Option<String>,
) -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    let auth_profile_name = profile
        .or_else(|| active_login_auth_profile(&config))
        .ok_or_else(|| {
            CliError::LoginProfile(
                "No active auth profile found; pass --profile or run a provider wrapper."
                    .to_string(),
            )
        })?;
    let auth_profile = config
        .auth
        .profiles
        .get(&auth_profile_name)
        .ok_or_else(|| {
            CliError::LoginProfile(format!(
                "Auth profile '{auth_profile_name}' is not declared in config."
            ))
        })?;
    if auth_profile.backend != "sshenv" {
        return Err(CliError::LoginProfile(format!(
            "Auth profile '{auth_profile_name}' uses backend '{}'; generic auth login only supports sshenv profiles.",
            auth_profile.backend
        )));
    }
    let api_key_env = auth_profile
        .map
        .get("api_key")
        .and_then(|mapping| mapping.env.as_ref().or(mapping.key.as_ref()))
        .or_else(|| auth_profile.settings.get("api_key_env"))
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .ok_or_else(|| CliError::LoginProfile(format!(
            "Auth profile '{auth_profile_name}' does not declare an api_key mapping. Use a provider-specific login command."
        )))?;
    let storage_profile = auth_profile
        .settings
        .get("profile")
        .cloned()
        .unwrap_or_else(|| auth_profile_name.clone());
    let vault_path = vault
        .or_else(|| auth_profile.settings.get("vault").map(PathBuf::from))
        .unwrap_or_else(bcode_config::default_auth_vault_path);
    let store = open_auth_store(&vault_path, recipient_key)?;
    let api_key = rpassword::prompt_password(format!("{api_key_env}: "))?;
    store
        .set_secret(&storage_profile, &api_key_env, Zeroizing::new(api_key))
        .map_err(|error| {
            CliError::BundledPluginInstallFailed(format!("failed to store API key: {error}"))
        })?;
    println!("API key saved");
    println!("Auth profile: {auth_profile_name}");
    println!("Credentials saved to sshenv vault profile: {storage_profile}");
    println!("API key environment variable: {api_key_env}");
    println!("Config is declarative; no config file update needed.");
    Ok(())
}

async fn handle_login_command(command: LoginCommand) -> Result<(), CliError> {
    match command {
        LoginCommand::Openai {
            api_key,
            base_url,
            chatgpt,
            browser,
            headless,
            profile,
            vault,
            recipient_key,
            model,
        } => {
            login_openai(OpenAiLoginOptions {
                api_key,
                base_url,
                chatgpt,
                browser,
                headless,
                profile,
                vault,
                recipient_key,
                model,
            })
            .await?;
        }
        LoginCommand::Xai {
            api_key,
            base_url,
            profile,
            vault,
            recipient_key,
            model,
        } => {
            login_xai(XaiLoginOptions {
                api_key,
                base_url,
                profile,
                vault,
                recipient_key,
                model,
            })?;
        }
    }
    Ok(())
}

struct OpenAiLoginOptions {
    api_key: Option<String>,
    base_url: Option<String>,
    chatgpt: bool,
    browser: bool,
    headless: bool,
    profile: Option<String>,
    vault: Option<PathBuf>,
    recipient_key: Option<String>,
    model: Option<String>,
}

struct XaiLoginOptions {
    api_key: Option<String>,
    base_url: Option<String>,
    profile: Option<String>,
    vault: Option<PathBuf>,
    recipient_key: Option<String>,
    model: Option<String>,
}

async fn login_openai(options: OpenAiLoginOptions) -> Result<(), CliError> {
    let target = resolve_login_target(LoginProvider::OpenAi, options.profile, options.vault)?;
    let store = open_auth_store(&target.vault_path, options.recipient_key)?;
    if options.api_key.is_some() || (options.base_url.is_some() && !options.chatgpt) {
        login_openai_api_key(
            &store,
            &target,
            options.api_key,
            options.base_url,
            options.model,
        )
    } else {
        let flow = if options.headless && !options.browser {
            OpenAiLoginFlow::DeviceCode
        } else {
            OpenAiLoginFlow::Browser
        };
        login_openai_chatgpt(&store, target, options.model, flow).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginProvider {
    OpenAi,
    Xai,
}

impl LoginProvider {
    const fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::Xai => "xAI",
        }
    }

    const fn prefix(self) -> &'static str {
        match self {
            Self::OpenAi => "OPENAI",
            Self::Xai => "XAI",
        }
    }

    const fn subcommand(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Xai => "xai",
        }
    }

    const fn wrapper_example(self) -> &'static str {
        match self {
            Self::OpenAi => "bcode-openai login openai",
            Self::Xai => "bcode-xai login xai",
        }
    }

    const fn explicit_example(self) -> &'static str {
        match self {
            Self::OpenAi => "bcode login openai --profile openai",
            Self::Xai => "bcode login xai --profile xai",
        }
    }

    fn accepts_config_provider(self, provider: &str) -> bool {
        match self {
            Self::OpenAi => !matches!(provider, "xai" | "grok"),
            Self::Xai => matches!(provider, "xai" | "grok"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginConfigUpdate {
    Declarative,
    Writable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LoginTarget {
    auth_profile: String,
    storage_profile: String,
    vault_path: PathBuf,
    api_key_env: Option<String>,
    config_update: LoginConfigUpdate,
}

fn resolve_login_target(
    provider: LoginProvider,
    explicit_profile: Option<String>,
    explicit_vault: Option<PathBuf>,
) -> Result<LoginTarget, CliError> {
    if let Some(profile) = explicit_profile {
        let config = bcode_config::load_config().ok();
        if let Some(auth_profile) = config
            .as_ref()
            .and_then(|config| config.auth.profiles.get(&profile))
        {
            return login_target_from_declarative_auth_profile(
                provider,
                &profile,
                auth_profile,
                explicit_vault,
            );
        }
        let vault_path = explicit_vault.unwrap_or_else(bcode_config::default_auth_vault_path);
        return Ok(LoginTarget {
            auth_profile: profile.clone(),
            storage_profile: profile,
            vault_path,
            api_key_env: None,
            config_update: LoginConfigUpdate::Writable,
        });
    }

    let config = bcode_config::load_config()?;
    let auth_profile = active_login_auth_profile(&config).ok_or_else(|| {
        CliError::LoginProfile(format!(
            "No active {} auth profile found.\n\nRun a provider wrapper such as:\n  {}\n\nOr pass one explicitly:\n  {}",
            provider.label(),
            provider.wrapper_example(),
            provider.explicit_example()
        ))
    })?;
    let Some(configured_auth_profile) = config.auth.profiles.get(&auth_profile) else {
        return Err(CliError::LoginProfile(format!(
            "Active {} auth profile '{auth_profile}' is selected, but it is not declared in [auth.profiles.{auth_profile}].\n\nUpdate the active config or pass a profile explicitly:\n  bcode login {} --profile {auth_profile}",
            provider.label(),
            provider.subcommand()
        )));
    };
    login_target_from_declarative_auth_profile(
        provider,
        &auth_profile,
        configured_auth_profile,
        explicit_vault,
    )
}

fn active_login_auth_profile(config: &bcode_config::BcodeConfig) -> Option<String> {
    std::env::var(bcode_config::BCODE_AUTH_PROFILE_ENV)
        .ok()
        .filter(|profile| !profile.trim().is_empty())
        .or_else(|| config.resolved_model_selection().auth_profile)
}

fn login_target_from_declarative_auth_profile(
    provider: LoginProvider,
    auth_profile_name: &str,
    auth_profile: &bcode_config::AuthProfileConfig,
    explicit_vault: Option<PathBuf>,
) -> Result<LoginTarget, CliError> {
    if auth_profile.backend != "sshenv" {
        return Err(CliError::LoginProfile(format!(
            "Auth profile '{auth_profile_name}' uses backend '{}', but `bcode login {}` can only update sshenv-backed auth profiles.",
            auth_profile.backend,
            provider.subcommand()
        )));
    }
    if let Some(config_provider) = auth_profile.settings.get("provider")
        && !provider.accepts_config_provider(config_provider)
    {
        return Err(CliError::LoginProfile(format!(
            "Auth profile '{auth_profile_name}' is configured for provider '{config_provider}', not {}.",
            provider.label()
        )));
    }
    let storage_profile = auth_profile
        .settings
        .get("profile")
        .cloned()
        .unwrap_or_else(|| auth_profile_name.to_string());
    let api_key_env = auth_profile
        .map
        .get("api_key")
        .and_then(|mapping| mapping.env.as_ref().or(mapping.key.as_ref()))
        .or_else(|| auth_profile.settings.get("api_key_env"))
        .filter(|value| !value.trim().is_empty())
        .cloned();
    let vault_path = auth_profile
        .settings
        .get("vault")
        .map(PathBuf::from)
        .or(explicit_vault)
        .unwrap_or_else(bcode_config::default_auth_vault_path);
    Ok(LoginTarget {
        auth_profile: auth_profile_name.to_string(),
        storage_profile,
        vault_path,
        api_key_env,
        config_update: LoginConfigUpdate::Declarative,
    })
}

fn open_auth_store(
    vault_path: &Path,
    recipient_key: Option<String>,
) -> Result<sshenv_vault::SshenvStore, CliError> {
    let store = sshenv_vault::SshenvStore::new(sshenv_vault::SshenvStoreConfig::new(
        vault_path.to_path_buf(),
    ));
    if !vault_path.exists() {
        let recipient_key = resolve_recipient_key(recipient_key)?;
        store.init(&recipient_key).map_err(|error| {
            CliError::BundledPluginInstallFailed(format!(
                "failed to initialize auth vault: {error}"
            ))
        })?;
    }
    Ok(store)
}

/// Generic helper for storing API-key auth for any OpenAI-compatible provider (`OpenAI`, xAI, etc.).
/// `prefix` is "OPENAI" or "XAI" (used for env-style secret keys stored in the vault).
fn login_compatible_api_key(
    store: &sshenv_vault::SshenvStore,
    target: &LoginTarget,
    api_key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    provider: LoginProvider,
) -> Result<(), CliError> {
    let prefix = provider.prefix();
    let prompt = format!("{prefix} API key: ");
    let api_key = match api_key {
        Some(api_key) => api_key,
        None => rpassword::prompt_password(&prompt)?,
    };
    let auth_mode_key = format!("BCODE_{prefix}_AUTH_MODE");
    let api_key_key = target
        .api_key_env
        .clone()
        .unwrap_or_else(|| format!("BCODE_{prefix}_API_KEY"));
    let base_url_key = format!("BCODE_{prefix}_BASE_URL");

    store
        .set_secret(
            &target.storage_profile,
            &auth_mode_key,
            Zeroizing::new("api_key".to_string()),
        )
        .map_err(|error| {
            CliError::BundledPluginInstallFailed(format!("failed to store auth mode: {error}"))
        })?;
    store
        .set_secret(
            &target.storage_profile,
            &api_key_key,
            Zeroizing::new(api_key),
        )
        .map_err(|error| {
            CliError::BundledPluginInstallFailed(format!(
                "failed to store {prefix} API key: {error}"
            ))
        })?;
    let config_base_url = base_url.clone();
    if let Some(base_url) = base_url {
        store
            .set_secret(
                &target.storage_profile,
                &base_url_key,
                Zeroizing::new(base_url),
            )
            .map_err(|error| {
                CliError::BundledPluginInstallFailed(format!(
                    "failed to store {prefix} base URL: {error}"
                ))
            })?;
    }

    // Always route through the shared OpenAI-compatible provider plugin.
    report_login_completion(
        &format!("{prefix} API credentials saved"),
        target,
        prefix,
        || {
            bcode_config::set_openai_compatible_sshenv_auth_mode(
                compatible_provider_name(prefix),
                target.auth_profile.clone(),
                target.vault_path.clone(),
                model,
                AuthMode::ApiKey,
                config_base_url.as_deref(),
            )
        },
    );
    Ok(())
}

fn compatible_provider_name(prefix: &str) -> &'static str {
    match prefix {
        "XAI" => "xai",
        _ => "openai",
    }
}

fn login_openai_api_key(
    store: &sshenv_vault::SshenvStore,
    target: &LoginTarget,
    api_key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
) -> Result<(), CliError> {
    login_compatible_api_key(
        store,
        target,
        api_key,
        base_url,
        model,
        LoginProvider::OpenAi,
    )
}

fn login_xai(options: XaiLoginOptions) -> Result<(), CliError> {
    let target = resolve_login_target(LoginProvider::Xai, options.profile, options.vault)?;
    let store = open_auth_store(&target.vault_path, options.recipient_key)?;
    login_compatible_api_key(
        &store,
        &target,
        options.api_key,
        Some(
            options
                .base_url
                .unwrap_or_else(|| "https://api.x.ai/v1".to_string()),
        ),
        options.model,
        LoginProvider::Xai,
    )
}

async fn login_openai_chatgpt(
    store: &sshenv_vault::SshenvStore,
    target: LoginTarget,
    model: Option<String>,
    flow: OpenAiLoginFlow,
) -> Result<(), CliError> {
    let oauth = run_openai_codex_oauth(flow).await?;
    let expires_at = unix_timestamp() + oauth.expires_in.unwrap_or(3600).saturating_sub(60);
    let account_id = oauth
        .id_token
        .as_deref()
        .and_then(chatgpt_account_id_from_access_token)
        .or_else(|| chatgpt_account_id_from_access_token(&oauth.access_token));
    store
        .set_secret(
            &target.storage_profile,
            "BCODE_OPENAI_AUTH_MODE",
            Zeroizing::new("chatgpt".to_string()),
        )
        .map_err(|error| {
            CliError::BundledPluginInstallFailed(format!("failed to store auth mode: {error}"))
        })?;
    store
        .set_secret(
            &target.storage_profile,
            "BCODE_OPENAI_CODEX_ACCESS_TOKEN",
            Zeroizing::new(oauth.access_token),
        )
        .map_err(|error| {
            CliError::BundledPluginInstallFailed(format!("failed to store access token: {error}"))
        })?;
    if let Some(id_token) = oauth.id_token {
        store
            .set_secret(
                &target.storage_profile,
                "BCODE_OPENAI_CODEX_ID_TOKEN",
                Zeroizing::new(id_token),
            )
            .map_err(|error| {
                CliError::BundledPluginInstallFailed(format!("failed to store ID token: {error}"))
            })?;
    }
    if let Some(refresh_token) = oauth.refresh_token {
        store
            .set_secret(
                &target.storage_profile,
                "BCODE_OPENAI_CODEX_REFRESH_TOKEN",
                Zeroizing::new(refresh_token),
            )
            .map_err(|error| {
                CliError::BundledPluginInstallFailed(format!(
                    "failed to store refresh token: {error}"
                ))
            })?;
    }
    store
        .set_secret(
            &target.storage_profile,
            "BCODE_OPENAI_CODEX_EXPIRES_AT",
            Zeroizing::new(expires_at.to_string()),
        )
        .map_err(|error| {
            CliError::BundledPluginInstallFailed(format!("failed to store token expiry: {error}"))
        })?;
    if let Some(account_id) = account_id {
        store
            .set_secret(
                &target.storage_profile,
                "BCODE_OPENAI_CODEX_ACCOUNT_ID",
                Zeroizing::new(account_id),
            )
            .map_err(|error| {
                CliError::BundledPluginInstallFailed(format!(
                    "failed to store ChatGPT account id: {error}"
                ))
            })?;
    }

    report_login_completion(
        "OpenAI ChatGPT subscription login saved",
        &target,
        "OPENAI",
        || {
            bcode_config::set_openai_sshenv_auth_mode(
                target.auth_profile.clone(),
                target.vault_path.clone(),
                model,
                AuthMode::ChatGpt,
            )
        },
    );
    Ok(())
}

fn report_login_completion(
    saved_message: &str,
    target: &LoginTarget,
    provider: &str,
    update_config: impl FnOnce() -> Result<PathBuf, bcode_config::ConfigError>,
) {
    println!("{saved_message}");
    println!("Auth profile: {}", target.auth_profile);
    println!(
        "Credentials saved to sshenv vault profile: {}",
        target.storage_profile
    );
    if let Some(api_key_env) = &target.api_key_env {
        println!("API key environment variable: {api_key_env}");
    }
    match target.config_update {
        LoginConfigUpdate::Declarative => {
            println!("Config is declarative; no config file update needed.");
        }
        LoginConfigUpdate::Writable => match update_config() {
            Ok(config_path) => println!("Config updated: {}", config_path.display()),
            Err(error) => {
                eprintln!("Config update failed: {error}");
                eprintln!(
                    "Credentials were saved. To use them, run a provider wrapper with a declarative {provider} auth profile or update a writable config."
                );
            }
        },
    }
}

async fn run_openai_codex_oauth(
    flow: OpenAiLoginFlow,
) -> Result<OpenAiOauthTokenResponse, CliError> {
    match flow {
        OpenAiLoginFlow::Browser => run_openai_codex_browser_oauth().await,
        OpenAiLoginFlow::DeviceCode => run_openai_codex_device_oauth().await,
    }
}

async fn run_openai_codex_browser_oauth() -> Result<OpenAiOauthTokenResponse, CliError> {
    let listeners = open_oauth_listeners()?;
    let redirect_uri = format!("http://localhost:{OPENAI_CODEX_OAUTH_PORT}/auth/callback");
    let state = random_urlsafe(32)?;
    let verifier = random_pkce_verifier(43)?;
    let challenge = pkce_challenge(&verifier);
    let authorize_url = openai_codex_authorize_url(&redirect_uri, &state, &challenge);
    println!("OpenAI ChatGPT subscription browser login");
    println!("Open this URL if your browser does not open automatically:\n{authorize_url}\n");
    println!(
        "After signing in, return here. If the browser cannot reach localhost, copy the full redirected localhost URL, paste it here, and press Enter."
    );
    open_browser(&authorize_url);
    let code = wait_for_oauth_code(&listeners, &state)?;
    exchange_openai_codex_code_async(&redirect_uri, &verifier, &code).await
}

async fn run_openai_codex_device_oauth() -> Result<OpenAiOauthTokenResponse, CliError> {
    let device = start_openai_codex_device_auth().await?;
    println!("OpenAI ChatGPT subscription device login");
    println!("Open this URL:\nhttps://auth.openai.com/codex/device\n");
    println!("Enter this code: {}", device.user_code);
    open_browser("https://auth.openai.com/codex/device");
    let interval = device.interval.parse::<u64>().unwrap_or(5).max(1);
    let token = poll_openai_codex_device_auth(&device, interval).await?;
    exchange_openai_codex_code_async(
        "https://auth.openai.com/deviceauth/callback",
        &token.code_verifier,
        &token.authorization_code,
    )
    .await
}

fn openai_codex_authorize_url(redirect_uri: &str, state: &str, challenge: &str) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", OPENAI_CODEX_CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("scope", OPENAI_CODEX_SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", "bcode"),
    ];
    let query = params
        .into_iter()
        .map(|(key, value)| format!("{}={}", pct_encode(key), pct_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{OPENAI_CODEX_AUTHORIZE_URL}?{query}")
}

async fn exchange_openai_codex_code_async(
    redirect_uri: &str,
    verifier: &str,
    code: &str,
) -> Result<OpenAiOauthTokenResponse, CliError> {
    let params = [
        ("grant_type", "authorization_code"),
        ("client_id", OPENAI_CODEX_CLIENT_ID),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
    ];
    let response = reqwest::Client::new()
        .post(OPENAI_CODEX_TOKEN_URL)
        .form(&params)
        .send()
        .await
        .map_err(|error| CliError::BundledPluginInstallFailed(error.to_string()))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| CliError::BundledPluginInstallFailed(error.to_string()))?;
    if !status.is_success() {
        return Err(CliError::BundledPluginInstallFailed(format!(
            "OpenAI OAuth token exchange failed with HTTP {status}: {body}"
        )));
    }
    serde_json::from_str(&body).map_err(CliError::Json)
}

async fn start_openai_codex_device_auth() -> Result<OpenAiDeviceUserCodeResponse, CliError> {
    let response = reqwest::Client::new()
        .post("https://auth.openai.com/api/accounts/deviceauth/usercode")
        .header("User-Agent", format!("bcode/{}", env!("CARGO_PKG_VERSION")))
        .json(&serde_json::json!({ "client_id": OPENAI_CODEX_CLIENT_ID }))
        .send()
        .await
        .map_err(|error| CliError::BundledPluginInstallFailed(error.to_string()))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| CliError::BundledPluginInstallFailed(error.to_string()))?;
    if !status.is_success() {
        return Err(CliError::BundledPluginInstallFailed(format!(
            "OpenAI device authorization failed with HTTP {status}: {body}"
        )));
    }
    serde_json::from_str(&body).map_err(CliError::Json)
}

async fn poll_openai_codex_device_auth(
    device: &OpenAiDeviceUserCodeResponse,
    interval_seconds: u64,
) -> Result<OpenAiDeviceTokenResponse, CliError> {
    loop {
        tokio::time::sleep(Duration::from_secs(interval_seconds + 3)).await;
        let response = reqwest::Client::new()
            .post("https://auth.openai.com/api/accounts/deviceauth/token")
            .header("User-Agent", format!("bcode/{}", env!("CARGO_PKG_VERSION")))
            .json(&serde_json::json!({
                "device_auth_id": device.device_auth_id,
                "user_code": device.user_code,
            }))
            .send()
            .await
            .map_err(|error| CliError::BundledPluginInstallFailed(error.to_string()))?;
        if response.status().is_success() {
            let body = response
                .text()
                .await
                .map_err(|error| CliError::BundledPluginInstallFailed(error.to_string()))?;
            return serde_json::from_str(&body).map_err(CliError::Json);
        }
        if !matches!(response.status().as_u16(), 403 | 404) {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(CliError::BundledPluginInstallFailed(format!(
                "OpenAI device authorization polling failed with HTTP {status}: {body}"
            )));
        }
    }
}

fn open_oauth_listeners() -> Result<Vec<TcpListener>, CliError> {
    let mut listeners = Vec::new();
    let mut errors = Vec::new();
    for address in ["127.0.0.1", "::1"] {
        match TcpListener::bind((address, OPENAI_CODEX_OAUTH_PORT)) {
            Ok(listener) => {
                listener.set_nonblocking(true)?;
                listeners.push(listener);
            }
            Err(error) => errors.push(format!("{address}: {error}")),
        }
    }
    if listeners.is_empty() {
        return Err(CliError::BundledPluginInstallFailed(format!(
            "failed to bind OpenAI OAuth callback server on localhost:{OPENAI_CODEX_OAUTH_PORT}: {}",
            errors.join("; ")
        )));
    }
    Ok(listeners)
}

fn wait_for_oauth_code(
    listeners: &[TcpListener],
    expected_state: &str,
) -> Result<String, CliError> {
    let manual_callback = spawn_manual_oauth_callback_reader();
    let deadline = Instant::now() + Duration::from_mins(5);
    loop {
        if let Some(code) = poll_manual_oauth_callback(&manual_callback, expected_state)? {
            return Ok(code);
        }
        if Instant::now() >= deadline {
            return Err(CliError::BundledPluginInstallFailed(
                "OpenAI OAuth callback timed out".to_string(),
            ));
        }
        if let Some(code) = poll_oauth_listeners(listeners, expected_state)? {
            return Ok(code);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn poll_oauth_listeners(
    listeners: &[TcpListener],
    expected_state: &str,
) -> Result<Option<String>, CliError> {
    for listener in listeners {
        match listener.accept() {
            Ok((mut stream, _)) => match handle_oauth_callback_stream(&mut stream, expected_state)?
            {
                OAuthCallback::Code(code) => return Ok(Some(code)),
                OAuthCallback::Ignored => {}
            },
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(None)
}

fn spawn_manual_oauth_callback_reader() -> Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        loop {
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() {
                break;
            }
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    receiver
}

fn poll_manual_oauth_callback(
    receiver: &Receiver<String>,
    expected_state: &str,
) -> Result<Option<String>, CliError> {
    match receiver.try_recv() {
        Ok(input) => manual_oauth_callback_code(&input, expected_state),
        Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => Ok(None),
    }
}

fn manual_oauth_callback_code(
    input: &str,
    expected_state: &str,
) -> Result<Option<String>, CliError> {
    if input.trim().is_empty() {
        return Ok(None);
    }
    match parse_oauth_callback(input.trim()) {
        OAuthCallbackParse::Code { code, state } if state == expected_state => Ok(Some(code)),
        OAuthCallbackParse::Code { .. } => {
            eprintln!(
                "Pasted OpenAI OAuth callback state did not match; paste the newest redirected URL from this login attempt."
            );
            Ok(None)
        }
        OAuthCallbackParse::Error(error) => Err(CliError::BundledPluginInstallFailed(format!(
            "OpenAI OAuth failed: {error}"
        ))),
        OAuthCallbackParse::Ignored => {
            eprintln!(
                "Pasted text was not an OpenAI OAuth callback URL; paste the full localhost callback URL."
            );
            Ok(None)
        }
    }
}

fn handle_oauth_callback_stream(
    stream: &mut std::net::TcpStream,
    expected_state: &str,
) -> Result<OAuthCallback, CliError> {
    let mut request = [0_u8; 8192];
    let size = stream.read(&mut request)?;
    let request = String::from_utf8_lossy(&request[..size]);
    let first_line = request.lines().next().unwrap_or_default();
    match parse_oauth_callback(first_line) {
        OAuthCallbackParse::Code { code, state } if state == expected_state => {
            write_oauth_response(stream, true)?;
            Ok(OAuthCallback::Code(code))
        }
        OAuthCallbackParse::Code { .. } => {
            write_oauth_response(stream, false)?;
            Err(CliError::BundledPluginInstallFailed(
                "OpenAI OAuth callback state did not match".to_string(),
            ))
        }
        OAuthCallbackParse::Error(error) => {
            write_oauth_response(stream, false)?;
            Err(CliError::BundledPluginInstallFailed(format!(
                "OpenAI OAuth failed: {error}"
            )))
        }
        OAuthCallbackParse::Ignored => {
            write_oauth_response(stream, false)?;
            Ok(OAuthCallback::Ignored)
        }
    }
}

fn write_oauth_response(stream: &mut std::net::TcpStream, success: bool) -> Result<(), CliError> {
    let response_body = if success {
        "Bcode OpenAI login complete. You can close this tab."
    } else {
        "Bcode OpenAI login did not complete. Return to your terminal."
    };
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        response_body.len(),
        response_body
    )?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum OAuthCallback {
    Code(String),
    Ignored,
}

#[derive(Debug, PartialEq, Eq)]
enum OAuthCallbackParse {
    Code { code: String, state: String },
    Error(String),
    Ignored,
}

fn parse_oauth_callback(input: &str) -> OAuthCallbackParse {
    let Some(path) = oauth_callback_path(input) else {
        return OAuthCallbackParse::Ignored;
    };
    if !path.starts_with("/auth/callback") {
        return OAuthCallbackParse::Ignored;
    }
    let Some(query) = path.split_once('?').map(|(_, query)| query) else {
        return OAuthCallbackParse::Ignored;
    };
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut error_description = None;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        match pct_decode(key).as_deref() {
            Some("code") => code = pct_decode(value),
            Some("state") => state = pct_decode(value),
            Some("error") => error = pct_decode(value),
            Some("error_description") => error_description = pct_decode(value),
            _ => {}
        }
    }
    if let Some(error) = error_description.or(error) {
        return OAuthCallbackParse::Error(error);
    }
    match (code, state) {
        (Some(code), Some(state)) => OAuthCallbackParse::Code { code, state },
        _ => OAuthCallbackParse::Ignored,
    }
}

fn oauth_callback_path(input: &str) -> Option<&str> {
    let candidate = if input.starts_with("GET ") || input.starts_with("POST ") {
        input.split_whitespace().nth(1)?
    } else {
        oauth_callback_url_from_text(input.trim())?
    };
    if candidate.starts_with("/auth/callback") {
        return Some(candidate);
    }
    let (_, without_scheme) = candidate.split_once("://")?;
    let path_start = without_scheme.find('/')?;
    Some(&without_scheme[path_start..])
}

fn oauth_callback_url_from_text(input: &str) -> Option<&str> {
    if input.starts_with("/auth/callback") {
        return Some(input);
    }
    let start = input
        .find("http://localhost:")
        .or_else(|| input.find("http://127.0.0.1:"))
        .or_else(|| input.find("http://[::1]:"))?;
    let rest = &input[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(&rest[..end])
}

fn random_urlsafe(bytes: usize) -> Result<String, CliError> {
    let mut data = vec![0_u8; bytes];
    rand::rngs::OsRng
        .try_fill_bytes(&mut data)
        .map_err(|error| CliError::BundledPluginInstallFailed(error.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(data))
}

fn random_pkce_verifier(length: usize) -> Result<String, CliError> {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut data = vec![0_u8; length];
    rand::rngs::OsRng
        .try_fill_bytes(&mut data)
        .map_err(|error| CliError::BundledPluginInstallFailed(error.to_string()))?;
    Ok(data
        .into_iter()
        .map(|byte| char::from(CHARS[usize::from(byte) % CHARS.len()]))
        .collect())
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn chatgpt_account_id_from_access_token(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims = serde_json::from_slice::<serde_json::Value>(&bytes).ok()?;
    claims
        .get("chatgpt_account_id")
        .or_else(|| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(|auth| auth.get("chatgpt_account_id"))
        })
        .or_else(|| {
            claims
                .get("organizations")
                .and_then(serde_json::Value::as_array)
                .and_then(|organizations| organizations.first())
                .and_then(|organization| organization.get("id"))
        })
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn pct_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => {
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

fn pct_decode(value: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut iter = value.as_bytes().iter().copied();
    while let Some(byte) = iter.next() {
        if byte == b'%' {
            let high = iter.next()?;
            let low = iter.next()?;
            bytes.push(hex_byte(high, low)?);
        } else if byte == b'+' {
            bytes.push(b' ');
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).ok()
}

fn hex_byte(high: u8, low: u8) -> Option<u8> {
    const fn digit(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    Some(digit(high)? << 4 | digit(low)?)
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let command = ("open", vec![url]);
    #[cfg(target_os = "windows")]
    let command = ("cmd", vec!["/C", "start", url]);
    #[cfg(all(unix, not(target_os = "macos")))]
    let command = ("xdg-open", vec![url]);
    let _ = Command::new(command.0)
        .args(command.1)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
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
    print_model_list(&models.models);
    Ok(())
}

fn print_model_list(models: &[bcode_model::ModelInfo]) {
    let model_width = models
        .iter()
        .map(|model| model.model_id.len())
        .max()
        .unwrap_or("MODEL".len())
        .max("MODEL".len());
    let display_name_width = models
        .iter()
        .map(|model| model.display_name.len())
        .max()
        .unwrap_or("DISPLAY NAME".len())
        .max("DISPLAY NAME".len());
    println!(
        "{:<model_width$}  {:<display_name_width$}  DEFAULT",
        "MODEL", "DISPLAY NAME"
    );
    for model in models {
        if model.is_default {
            println!(
                "{:<model_width$}  {:<display_name_width$}  yes",
                model.model_id, model.display_name
            );
        } else {
            println!("{:<model_width$}  {}", model.model_id, model.display_name);
        }
    }
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
    for (key, value) in capabilities.metadata {
        println!("metadata\t{key}\t{value}");
    }
    Ok(())
}

async fn model_validate_config() -> Result<(), CliError> {
    let response = call_model_provider_service(bcode_model::OP_VALIDATE_CONFIG).await?;
    if let Some(error) = response.error {
        println!("ERROR\t{}\t{}", error.code, error.message);
        return Ok(());
    }
    let validation: bcode_model::ValidateConfigResponse =
        serde_json::from_slice(&response.payload)?;
    println!("valid\t{}", validation.valid);
    if let Some(message) = validation.message {
        println!("message\t{message}");
    }
    for (key, value) in validation.metadata {
        println!("metadata\t{key}\t{value}");
    }
    Ok(())
}

async fn call_model_provider_service(
    operation: &str,
) -> Result<bcode_ipc::PluginServiceResponse, CliError> {
    let config = bcode_config::load_config()?;
    let client = BcodeClient::default_endpoint();
    let resolved_model = config.resolved_model_selection();
    if let Some(provider_plugin_id) = resolved_model.provider_plugin_id {
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
const BUNDLED_BEDROCK_SERVICES: &[BundledPluginServiceSpec] = &[BundledPluginServiceSpec {
    interface_id: "bcode.model-provider/v1",
    name: "Amazon Bedrock Model Provider",
    description: "Amazon Bedrock ConverseStream model provider",
}];
const BUNDLED_DEFAULT_AGENT_SERVICES: &[BundledPluginServiceSpec] = &[BundledPluginServiceSpec {
    interface_id: "bcode.agent-profile/v1",
    name: "Default Agent Profiles",
    description: "Default plan/build agent profile policy provider",
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
    BundledPluginSpec {
        id: "bcode.bedrock",
        package: "bcode_bedrock_provider_plugin",
        library_stem: "bcode_bedrock_provider_plugin",
        name: "Bcode Bedrock Provider",
        services: BUNDLED_BEDROCK_SERVICES,
    },
    BundledPluginSpec {
        id: "bcode.default-agents",
        package: "bcode_default_agents_plugin",
        library_stem: "bcode_default_agents_plugin",
        name: "Bcode Default Agents",
        services: BUNDLED_DEFAULT_AGENT_SERVICES,
    },
];

fn ensure_bundled_plugins_installed() -> Result<(), CliError> {
    if cfg!(feature = "_static-bundled")
        || std::env::var_os("BCODE_SKIP_BUNDLED_PLUGIN_INSTALL").is_some()
    {
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
    if bundled_plugin_libraries_current(executable_dir) {
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

fn bundled_plugin_libraries_current(executable_dir: &Path) -> bool {
    let Some(workspace_root) = workspace_root_from_executable_dir(executable_dir) else {
        return bundled_plugin_libraries_exist(executable_dir);
    };
    BUNDLED_PLUGIN_SPECS.iter().all(|spec| {
        let library = executable_dir.join(dynamic_library_name(spec.library_stem));
        library.exists() && library_is_newer_than_package_sources(&library, &workspace_root, spec)
    })
}

fn bundled_plugins_installed(executable_dir: &Path) -> bool {
    BUNDLED_PLUGIN_SPECS.iter().all(|spec| {
        let library_name = dynamic_library_name(spec.library_stem);
        let source_library = executable_dir.join(&library_name);
        let manifest_path = executable_dir
            .join("plugins")
            .join(spec.id)
            .join(bcode_plugin::DEFAULT_PLUGIN_MANIFEST_FILE);
        source_library.exists()
            && bundled_manifest_is_current(&manifest_path, spec, &library_name)
            && workspace_root_from_executable_dir(executable_dir).is_none_or(|workspace_root| {
                library_is_newer_than_package_sources(&source_library, &workspace_root, spec)
            })
    })
}

fn bundled_manifest_is_current(path: &Path, spec: &BundledPluginSpec, library_name: &str) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    contents == bundled_plugin_manifest(spec, &bundled_runtime_library_path(library_name))
}

fn library_is_newer_than_package_sources(
    library: &Path,
    workspace_root: &Path,
    spec: &BundledPluginSpec,
) -> bool {
    let Ok(library_modified) = std::fs::metadata(library).and_then(|metadata| metadata.modified())
    else {
        return false;
    };
    let package_dir = workspace_root.join(package_relative_dir(spec));
    newest_source_modified(&package_dir)
        .is_none_or(|source_modified| library_modified >= source_modified)
}

fn package_relative_dir(spec: &BundledPluginSpec) -> &'static str {
    match spec.id {
        "bcode.filesystem" => "plugins/filesystem-plugin",
        "bcode.shell" => "plugins/shell-plugin",
        "bcode.openai-compatible" => "plugins/openai-compatible-provider-plugin",
        "bcode.bedrock" => "plugins/bedrock-provider-plugin",
        "bcode.default-agents" => "plugins/default-agents-plugin",
        _ => ".",
    }
}

fn newest_source_modified(path: &Path) -> Option<std::time::SystemTime> {
    let mut newest = None;
    newest_source_modified_inner(path, &mut newest);
    newest
}

fn newest_source_modified_inner(path: &Path, newest: &mut Option<std::time::SystemTime>) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.is_file() {
        if path
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|extension| matches!(extension, "rs" | "toml"))
            && let Ok(modified) = metadata.modified()
            && newest.is_none_or(|current| modified > current)
        {
            *newest = Some(modified);
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        newest_source_modified_inner(&entry.path(), newest);
    }
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
    std::fs::write(
        plugin_dir.join(bcode_plugin::DEFAULT_PLUGIN_MANIFEST_FILE),
        bundled_plugin_manifest(spec, &bundled_runtime_library_path(&library_name)),
    )?;
    Ok(())
}

fn bundled_runtime_library_path(library_name: &str) -> String {
    format!("../../{library_name}")
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
    cleanup_old_daemons(false).await;
    if server_ping_ready(&client).await {
        if !quiet {
            println!("server already running");
            println!("namespace: {}", bcode_ipc::daemon_namespace());
            println!("log: {}", daemon_log_path().display());
        }
        return Ok(());
    }

    ensure_bundled_plugins_installed()?;

    let log_path = daemon_log_path();
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut log_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)?;
    writeln!(log_file, "--- bcode daemon start ---")?;
    let stderr_log = log_file.try_clone()?;

    let exe = std::env::current_exe()?;
    let mut child = Command::new(exe)
        .args(["server", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(stderr_log))
        .spawn()?;

    wait_for_server_ready(&client, &mut child, &log_path).await?;
    if !quiet {
        println!("server started");
        println!("namespace: {}", bcode_ipc::daemon_namespace());
        println!("log: {}", log_path.display());
    }
    Ok(())
}

fn daemon_log_path() -> PathBuf {
    std::env::var_os("BCODE_DAEMON_LOG").map_or_else(
        || {
            bcode_config::default_state_dir()
                .join("logs")
                .join(format!("daemon-{}.log", bcode_ipc::daemon_namespace()))
        },
        PathBuf::from,
    )
}

async fn wait_for_server_ready(
    client: &BcodeClient,
    child: &mut Child,
    log_path: &Path,
) -> Result<(), CliError> {
    for _ in 0..50 {
        if server_ping_ready(client).await {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if let Some(status) = child.try_wait()? {
                return Err(CliError::DaemonExited {
                    status: status.to_string(),
                    log_path: log_path.display().to_string(),
                    recent_log: recent_log_excerpt(log_path),
                });
            }
            if server_ping_ready(client).await {
                return Ok(());
            }
            return Err(CliError::DaemonHealthCheckFailed {
                log_path: log_path.display().to_string(),
                recent_log: recent_log_excerpt(log_path),
            });
        }
        if let Some(status) = child.try_wait()? {
            return Err(CliError::DaemonExited {
                status: status.to_string(),
                log_path: log_path.display().to_string(),
                recent_log: recent_log_excerpt(log_path),
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Err(CliError::DaemonStartTimeout {
        log_path: log_path.display().to_string(),
        recent_log: recent_log_excerpt(log_path),
    })
}

async fn server_ping_ready(client: &BcodeClient) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_millis(250), client.ping()).await,
        Ok(Ok(()))
    )
}

fn recent_log_excerpt(log_path: &Path) -> String {
    let Ok(contents) = std::fs::read_to_string(log_path) else {
        return "daemon log could not be read".to_string();
    };
    let lines = contents.lines().rev().take(30).collect::<Vec<_>>();
    if lines.is_empty() {
        return "daemon log is empty".to_string();
    }
    let mut excerpt = lines.into_iter().rev().collect::<Vec<_>>().join("\n");
    if !excerpt.ends_with('\n') {
        excerpt.push('\n');
    }
    excerpt
}

async fn server_status(verbose: bool) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let status = client.server_status().await?;
    println!("daemon: running");
    println!("namespace: {}", status.daemon.namespace);
    if verbose {
        println!(
            "pid: {}",
            status
                .daemon
                .pid
                .map_or_else(|| "<unknown>".to_string(), |pid| pid.to_string())
        );
        println!("instance: {}", status.daemon.instance_id);
        println!("build fingerprint: {}", status.daemon.build_fingerprint);
    }
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
    match &status.session_catalog_status {
        bcode_ipc::SessionCatalogStatus::Loaded => {
            println!("sessions: {}", status.sessions.len());
        }
        bcode_ipc::SessionCatalogStatus::Loading => {
            println!(
                "sessions: {} cached (catalog loading)",
                status.sessions.len()
            );
        }
        bcode_ipc::SessionCatalogStatus::NotStarted => {
            println!(
                "sessions: {} cached (catalog not started)",
                status.sessions.len()
            );
        }
        bcode_ipc::SessionCatalogStatus::Failed(message) => {
            println!(
                "sessions: {} cached (catalog failed: {message})",
                status.sessions.len()
            );
        }
    }
    print_runtime_summary(&status.plugin_runtime, verbose);
    if verbose {
        print_metrics_summary(&status.metrics);
    }
    println!("log: {}", daemon_log_path().display());
    for session in status.sessions {
        let name = session.name.unwrap_or_else(|| "<unnamed>".to_string());
        println!("{}	{}	{} clients", name, session.id, session.client_count);
    }
    Ok(())
}

async fn server_metrics(
    json: bool,
    report: bool,
    dashboard: Option<PathBuf>,
) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let status = client.server_status().await?;
    if let Some(path) = dashboard {
        let html = metrics_dashboard_html(&status.metrics_report)?;
        std::fs::write(&path, html)?;
        println!("metrics dashboard: {}", path.display());
    } else if json || report {
        if report {
            println!("{}", serde_json::to_string_pretty(&status.metrics_report)?);
        } else {
            println!("{}", serde_json::to_string_pretty(&status.metrics)?);
        }
    } else {
        print_metrics_summary(&status.metrics);
        println!(
            "metric events: {} recent persisted samples",
            status.metrics_report.events.len()
        );
    }
    Ok(())
}

async fn server_diagnose(json: bool) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let status = client.server_status().await?;
    let diagnosis = ServerDiagnosis::from_status(status);
    if json {
        println!("{}", serde_json::to_string_pretty(&diagnosis)?);
    } else {
        print_server_diagnosis(&diagnosis);
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
struct ServerDiagnosis {
    daemon: bcode_ipc::DaemonStatus,
    connected_client_count: usize,
    session_count: usize,
    sessions: Vec<SessionDiagnosisSummary>,
    selected_provider_plugin_id: Option<String>,
    selected_model_id: Option<String>,
    plugin_runtime: Vec<bcode_plugin::PluginExecutorStatus>,
    metrics: bcode_metrics::MetricsSnapshot,
    observations: Vec<DiagnosticObservation>,
}

#[derive(Debug, Clone, Serialize)]
struct SessionDiagnosisSummary {
    session_id: SessionId,
    name: Option<String>,
    client_count: usize,
    updated_at_ms: u64,
    working_directory: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
struct DiagnosticObservation {
    severity: DiagnosticSeverity,
    code: String,
    message: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum DiagnosticSeverity {
    Info,
    Warning,
}

impl ServerDiagnosis {
    fn from_status(status: ServerStatus) -> Self {
        let observations = diagnostic_observations(&status);
        Self {
            daemon: status.daemon,
            connected_client_count: status.connected_client_count,
            session_count: status.sessions.len(),
            sessions: status
                .sessions
                .into_iter()
                .map(|session| SessionDiagnosisSummary {
                    session_id: session.id,
                    name: session.name,
                    client_count: session.client_count,
                    updated_at_ms: session.updated_at_ms,
                    working_directory: session.working_directory,
                })
                .collect(),
            selected_provider_plugin_id: status.selected_provider_plugin_id,
            selected_model_id: status.selected_model_id,
            plugin_runtime: status.plugin_runtime,
            metrics: status.metrics,
            observations,
        }
    }
}

fn diagnostic_observations(status: &ServerStatus) -> Vec<DiagnosticObservation> {
    let mut observations = Vec::new();
    add_histogram_observation(
        &mut observations,
        status,
        "session.event_log.append_duration_ms",
        100,
        "slow_session_event_appends",
        "session event appends have exceeded 100ms",
    );
    add_histogram_observation(
        &mut observations,
        status,
        "session.metadata_index.write_duration_ms",
        100,
        "slow_session_metadata_writes",
        "session metadata index writes have exceeded 100ms",
    );
    add_histogram_observation(
        &mut observations,
        status,
        "model.request_build_duration_ms",
        500,
        "slow_model_request_builds",
        "model request construction has exceeded 500ms",
    );
    add_histogram_observation(
        &mut observations,
        status,
        "model.provider.start_turn_duration_ms",
        2_000,
        "slow_model_start_turn",
        "model provider start_turn has exceeded 2s",
    );
    add_histogram_observation(
        &mut observations,
        status,
        "model.provider.poll_turn_events_duration_ms",
        2_000,
        "slow_model_poll",
        "model provider poll_turn_events has exceeded 2s",
    );
    if status
        .metrics
        .counters
        .get("model.provider.poll_empty_total")
        .copied()
        .unwrap_or_default()
        > 100
    {
        observations.push(DiagnosticObservation {
            severity: DiagnosticSeverity::Info,
            code: "many_empty_model_polls".to_string(),
            message: "model provider has returned many empty poll responses".to_string(),
        });
    }
    observations
}

fn add_histogram_observation(
    observations: &mut Vec<DiagnosticObservation>,
    status: &ServerStatus,
    key: &str,
    threshold_ms: u64,
    code: &str,
    message: &str,
) {
    let Some(histogram) = status.metrics.histograms.get(key) else {
        return;
    };
    if histogram.max.is_some_and(|max| max >= threshold_ms) {
        observations.push(DiagnosticObservation {
            severity: DiagnosticSeverity::Warning,
            code: code.to_string(),
            message: format!(
                "{message}; max observed={}ms",
                histogram.max.unwrap_or_default()
            ),
        });
    }
}

fn print_server_diagnosis(diagnosis: &ServerDiagnosis) {
    println!("daemon: running");
    println!("namespace: {}", diagnosis.daemon.namespace);
    println!("connected clients: {}", diagnosis.connected_client_count);
    println!("sessions: {}", diagnosis.session_count);
    println!(
        "model provider: {}",
        diagnosis
            .selected_provider_plugin_id
            .as_deref()
            .unwrap_or("<auto>")
    );
    println!(
        "model: {}",
        diagnosis
            .selected_model_id
            .as_deref()
            .unwrap_or("<default>")
    );
    if diagnosis.observations.is_empty() {
        println!("observations: none");
    } else {
        println!("observations:");
        for observation in &diagnosis.observations {
            println!(
                "  {:?}\t{}\t{}",
                observation.severity, observation.code, observation.message
            );
        }
    }
    print_runtime_summary(&diagnosis.plugin_runtime, true);
    print_metrics_summary(&diagnosis.metrics);
}

fn metrics_dashboard_html(
    report: &bcode_metrics::MetricsReport,
) -> Result<String, serde_json::Error> {
    let json = serde_json::to_string(report)?.replace("</", "<\\/");
    Ok(format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Bcode Metrics Dashboard</title>
<style>
:root {{ color-scheme: dark; font-family: Inter, ui-sans-serif, system-ui, sans-serif; }}
body {{ margin: 0; background: #0b1020; color: #e5ecff; }}
header {{ padding: 24px; background: linear-gradient(135deg, #172554, #312e81); }}
main {{ padding: 20px; display: grid; gap: 18px; }}
.card {{ background: #111827; border: 1px solid #263244; border-radius: 14px; padding: 16px; box-shadow: 0 14px 32px #0006; }}
.controls {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(220px, 1fr)); gap: 12px; }}
label {{ display: grid; gap: 6px; color: #bac7e5; font-size: 13px; }}
input, select {{ background: #0b1220; color: #e5ecff; border: 1px solid #334155; border-radius: 8px; padding: 9px; }}
.stats {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(160px, 1fr)); gap: 12px; }}
.stat {{ background: #0b1220; border: 1px solid #263244; border-radius: 10px; padding: 12px; }}
.stat strong {{ display: block; font-size: 28px; }}
svg {{ width: 100%; height: 260px; background: #08111f; border-radius: 10px; }}
table {{ width: 100%; border-collapse: collapse; font-size: 13px; }}
th, td {{ border-bottom: 1px solid #263244; padding: 8px; text-align: left; vertical-align: top; }}
th {{ color: #93c5fd; cursor: pointer; position: sticky; top: 0; background: #111827; }}
.badge {{ display: inline-block; padding: 2px 7px; border-radius: 999px; background: #1e3a8a; color: #bfdbfe; }}
small {{ color: #94a3b8; }}
</style>
</head>
<body>
<header>
<h1>Bcode Metrics Dashboard</h1>
<small>Filters and sorting are persisted in the URL query string.</small>
</header>
<main>
<section class="card controls">
<label>Session IDs<input id="sessions" placeholder="comma-separated session ids"></label>
<label>Event types<input id="eventTypes" placeholder="comma-separated event types"></label>
<label>Metric names<input id="metrics" placeholder="comma-separated metric names"></label>
<label>Kind<select id="kind"><option value="">all</option><option>counter</option><option>gauge</option><option>histogram</option><option>event</option></select></label>
<label>Sort<select id="sort"><option value="time_desc">newest first</option><option value="time_asc">oldest first</option><option value="name_asc">name A-Z</option><option value="value_desc">value desc</option></select></label>
<label>Search<input id="search" placeholder="text search"></label>
</section>
<section class="stats" id="stats"></section>
<section class="card"><h2>Events over time</h2><svg id="timeline" viewBox="0 0 900 260" preserveAspectRatio="none"></svg></section>
<section class="card"><h2>Event type breakdown</h2><svg id="breakdown" viewBox="0 0 900 260" preserveAspectRatio="none"></svg></section>
<section class="card"><h2>Metrics</h2><div id="metricsTable"></div></section>
<section class="card"><h2>Recent events</h2><div id="eventsTable"></div></section>
</main>
<script id="metrics-data" type="application/json">{json}</script>
<script>
const report = JSON.parse(document.getElementById('metrics-data').textContent);
const params = new URLSearchParams(location.search);
const ids = ['sessions','eventTypes','metrics','kind','sort','search'];
for (const id of ids) {{ const el = document.getElementById(id); el.value = params.get(id) || ''; el.addEventListener('input', render); }}
function list(id) {{ return document.getElementById(id).value.split(',').map(s => s.trim()).filter(Boolean); }}
function save() {{ const p = new URLSearchParams(); for (const id of ids) {{ const v = document.getElementById(id).value; if (v) p.set(id, v); }} history.replaceState(null, '', location.pathname + (p.toString() ? '?' + p : '')); }}
function filtered() {{
  const sessions = new Set(list('sessions')); const types = new Set(list('eventTypes')); const metrics = new Set(list('metrics'));
  const kind = document.getElementById('kind').value; const search = document.getElementById('search').value.toLowerCase();
  let rows = report.events.filter(e => (!sessions.size || sessions.has(e.labels.session_id)) && (!types.size || types.has(e.labels.event_type)) && (!metrics.size || metrics.has(e.name)) && (!kind || e.kind === kind));
  if (search) rows = rows.filter(e => JSON.stringify(e).toLowerCase().includes(search));
  const sort = document.getElementById('sort').value;
  rows.sort((a,b) => sort === 'time_asc' ? a.unix_ms - b.unix_ms : sort === 'name_asc' ? a.name.localeCompare(b.name) : sort === 'value_desc' ? b.value - a.value : b.unix_ms - a.unix_ms);
  return rows;
}}
function esc(s) {{ return String(s ?? '').replace(/[&<>"']/g, c => ({{'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}}[c])); }}
function table(rows, cols) {{ return '<table><thead><tr>' + cols.map(c => '<th>' + esc(c[0]) + '</th>').join('') + '</tr></thead><tbody>' + rows.map(r => '<tr>' + cols.map(c => '<td>' + esc(c[1](r)) + '</td>').join('') + '</tr>').join('') + '</tbody></table>'; }}
function drawBars(svg, entries, valueOf, labelOf) {{
  const max = Math.max(1, ...entries.map(valueOf)); const w = 900 / Math.max(1, entries.length); svg.innerHTML = entries.map((e,i) => {{ const h = valueOf(e) / max * 210; return `<rect x="${{i*w+2}}" y="${{230-h}}" width="${{Math.max(2,w-4)}}" height="${{h}}" fill="#60a5fa"><title>${{labelOf(e)}}: ${{valueOf(e)}} </title></rect>`; }}).join('');
}}
function render() {{
  save(); const events = filtered(); const sessions = new Set(events.map(e => e.labels.session_id).filter(Boolean)); const eventTypes = new Set(events.map(e => e.labels.event_type).filter(Boolean));
  document.getElementById('stats').innerHTML = [`<div class="stat"><small>Events</small><strong>${{events.length}}</strong></div>`, `<div class="stat"><small>Sessions</small><strong>${{sessions.size}}</strong></div>`, `<div class="stat"><small>Event types</small><strong>${{eventTypes.size}}</strong></div>`, `<div class="stat"><small>Metrics</small><strong>${{Object.keys(report.descriptors).length}}</strong></div>`].join('');
  const buckets = new Map(); for (const e of events) {{ const k = new Date(e.unix_ms).toISOString().slice(0,16); buckets.set(k, (buckets.get(k)||0)+1); }} drawBars(document.getElementById('timeline'), [...buckets], e => e[1], e => e[0]);
  const types = new Map(); for (const e of events) {{ const k = e.labels.event_type || e.name; types.set(k, (types.get(k)||0)+1); }} drawBars(document.getElementById('breakdown'), [...types].sort((a,b)=>b[1]-a[1]).slice(0,40), e => e[1], e => e[0]);
  const metricRows = Object.values(report.descriptors).map(d => ({{...d, counter: report.snapshot.counters[d.name], gauge: report.snapshot.gauges[d.name], histogram: report.snapshot.histograms[d.name]}}));
  document.getElementById('metricsTable').innerHTML = table(metricRows, [['Name', r=>r.name], ['Kind', r=>r.kind], ['Unit', r=>r.unit||''], ['Value', r=>r.counter ?? r.gauge ?? (r.histogram ? `count=${{r.histogram.count}} avg=${{Math.round(r.histogram.sum/Math.max(1,r.histogram.count))}}` : '')], ['Labels', r=>(r.label_keys||[]).join(', ')]]);
  document.getElementById('eventsTable').innerHTML = table(events.slice(0,500), [['Time', r=>new Date(r.unix_ms).toISOString()], ['Name', r=>r.name], ['Kind', r=>r.kind], ['Value', r=>r.value], ['Session', r=>r.labels.session_id||''], ['Event type', r=>r.labels.event_type||''], ['Labels', r=>JSON.stringify(r.labels)]]);
}}
render();
</script>
</body>
</html>"##
    ))
}

fn print_metrics_summary(metrics: &bcode_metrics::MetricsSnapshot) {
    println!(
        "metrics: {} counters, {} gauges, {} histograms",
        metrics.counters.len(),
        metrics.gauges.len(),
        metrics.histograms.len()
    );
    if !metrics.counters.is_empty() {
        println!("metric counters:");
        for (key, value) in &metrics.counters {
            println!("  {key}\t{value}");
        }
    }
    if !metrics.gauges.is_empty() {
        println!("metric gauges:");
        for (key, value) in &metrics.gauges {
            println!("  {key}\t{value}");
        }
    }
    if !metrics.histograms.is_empty() {
        println!("metric histograms:");
        for (key, histogram) in &metrics.histograms {
            let avg = if histogram.count == 0 {
                0
            } else {
                histogram.sum / histogram.count
            };
            println!(
                "  {key}\tcount={} avg={} min={} max={}",
                histogram.count,
                avg,
                histogram
                    .min
                    .map_or_else(|| "<none>".to_string(), |value| value.to_string()),
                histogram
                    .max
                    .map_or_else(|| "<none>".to_string(), |value| value.to_string())
            );
        }
    }
}

fn print_runtime_summary(runtime: &[bcode_plugin::PluginExecutorStatus], verbose: bool) {
    let running = runtime.iter().map(|plugin| plugin.running).sum::<usize>();
    let queued = runtime.iter().map(|plugin| plugin.queued).sum::<usize>();
    let tool_queued = runtime
        .iter()
        .map(|plugin| plugin.queued_tool_execution)
        .sum::<usize>();
    println!("runtime: {running} running, {queued} queued ({tool_queued} tool queued)");
    if running == 0 && queued == 0 {
        println!("active work: none");
    } else {
        println!("active work: plugin work in progress; use --verbose for queue details");
    }
    if verbose && !runtime.is_empty() {
        println!("plugin runtime:");
        for plugin in runtime {
            println!(
                "  {}: policy={:?} running={} queued={} [control={} query={} tool={} model={} event={} service={}] completed={} failed={}",
                plugin.plugin_id,
                plugin.concurrency,
                plugin.running,
                plugin.queued,
                plugin.queued_control,
                plugin.queued_query,
                plugin.queued_tool_execution,
                plugin.queued_model_provider,
                plugin.queued_event_delivery,
                plugin.queued_service,
                plugin.completed,
                plugin.failed
            );
        }
    }
}

async fn server_cleanup(stop_current: bool) -> Result<(), CliError> {
    let summary = cleanup_daemons(stop_current, true).await;
    for line in summary.messages {
        println!("{line}");
    }
    println!(
        "daemon cleanup: {} stopped, {} stale records removed, {} skipped",
        summary.stopped, summary.removed, summary.skipped
    );
    Ok(())
}

async fn cleanup_old_daemons(verbose: bool) {
    let _ = tokio::time::timeout(
        Duration::from_millis(1_000),
        cleanup_daemons(false, verbose),
    )
    .await;
}

#[derive(Debug, Default)]
struct DaemonCleanupSummary {
    stopped: usize,
    removed: usize,
    skipped: usize,
    messages: Vec<String>,
}

async fn cleanup_daemons(stop_current: bool, verbose: bool) -> DaemonCleanupSummary {
    let state_dir = bcode_config::default_state_dir();
    let records = bcode_daemon_lifecycle::read_records(&state_dir);
    let mut summary = DaemonCleanupSummary::default();
    for (path, record) in records {
        if !stop_current && record.is_current_namespace() {
            summary.skipped = summary.skipped.saturating_add(1);
            continue;
        }
        let Some(endpoint) = record.endpoint.to_ipc_endpoint() else {
            summary.skipped = summary.skipped.saturating_add(1);
            if verbose {
                summary.messages.push(format!(
                    "skipped {}: unsupported endpoint",
                    record.namespace
                ));
            }
            continue;
        };
        let client = BcodeClient::new(endpoint);
        let status = tokio::time::timeout(Duration::from_millis(250), client.server_status()).await;
        match status {
            Ok(Ok(status)) if daemon_status_matches(&record, &status.daemon) => {
                let stop_result = if stop_current {
                    tokio::time::timeout(Duration::from_millis(250), client.server_stop()).await
                } else {
                    tokio::time::timeout(Duration::from_millis(250), client.server_stop_if_idle())
                        .await
                };
                if matches!(stop_result, Ok(Ok(()))) {
                    summary.stopped = summary.stopped.saturating_add(1);
                    if verbose {
                        summary
                            .messages
                            .push(format!("stopped {}", record.namespace));
                    }
                } else {
                    summary.skipped = summary.skipped.saturating_add(1);
                    if verbose {
                        summary.messages.push(format!(
                            "skipped {}: daemon busy or stop request failed",
                            record.namespace
                        ));
                    }
                }
            }
            Ok(Ok(_)) => {
                summary.skipped = summary.skipped.saturating_add(1);
                if verbose {
                    summary.messages.push(format!(
                        "skipped {}: registry identity did not match running daemon",
                        record.namespace
                    ));
                }
            }
            _ => {
                if bcode_daemon_lifecycle::remove_record_path(&path).is_ok() {
                    summary.removed = summary.removed.saturating_add(1);
                    remove_stale_socket(&record);
                    if verbose {
                        summary
                            .messages
                            .push(format!("removed stale record {}", record.namespace));
                    }
                } else {
                    summary.skipped = summary.skipped.saturating_add(1);
                }
            }
        }
    }
    summary
}

fn daemon_status_matches(
    record: &bcode_daemon_lifecycle::DaemonRecord,
    status: &bcode_ipc::DaemonStatus,
) -> bool {
    status.namespace == record.namespace && status.instance_id == record.instance_id
}

fn remove_stale_socket(record: &bcode_daemon_lifecycle::DaemonRecord) {
    #[cfg(unix)]
    if let bcode_daemon_lifecycle::DaemonEndpointRecord::UnixSocket { path } = &record.endpoint
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with("bcode-")
                    && Path::new(name)
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("sock"))
            })
    {
        let _ = std::fs::remove_file(path);
    }
}

async fn server_stop() -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    match client.server_stop().await {
        Ok(()) => println!("server stopping"),
        Err(error) if server_is_unreachable(&error) => println!("server not running"),
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn server_is_unreachable(error: &ClientError) -> bool {
    match error {
        ClientError::Transport(bcode_ipc::IpcTransportError::Io(error)) => matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
        ),
        ClientError::Codec(bcode_ipc::CodecError::Io(error)) => matches!(
            error.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::UnexpectedEof
        ),
        _ => false,
    }
}

async fn handle_worktree_command(command: WorktreeCommand) -> Result<(), CliError> {
    match command {
        WorktreeCommand::List { repo, json } => worktree_list(repo, json).await,
        WorktreeCommand::Create {
            name,
            repo,
            path,
            session,
            new_session,
            branch,
            new_branch,
            base,
            detach,
            force,
            no_setup,
            json,
        } => {
            let _config_override = base.map(|base| {
                bcode_config::push_process_config_overrides(
                    bcode_config::ConfigLoadOverrides::from_env_with_cli(
                        None,
                        Some(bcode_config::worktree_base_ref_override_toml(
                            base.into_config(),
                        )),
                    ),
                )
            });
            worktree_create(WorktreeCreateCliArgs {
                name,
                repo,
                path,
                session,
                new_session,
                branch,
                new_branch,
                base,
                detach,
                force,
                no_setup,
                json,
            })
            .await
        }
        WorktreeCommand::Attach { session_id, path } => {
            let client = BcodeClient::default_endpoint();
            let session = client
                .change_session_working_directory(session_id, path)
                .await?;
            println!("{}\t{}", session.id, session.working_directory.display());
            Ok(())
        }
        WorktreeCommand::Remove {
            path,
            repo,
            force,
            json,
        } => worktree_remove(path, repo, force, json).await,
    }
}

#[allow(clippy::struct_excessive_bools)]
struct WorktreeCreateCliArgs {
    name: String,
    repo: Option<PathBuf>,
    path: Option<PathBuf>,
    session: Option<SessionId>,
    new_session: bool,
    branch: Option<String>,
    new_branch: Option<String>,
    base: Option<WorktreeBaseRefArg>,
    detach: bool,
    force: bool,
    no_setup: bool,
    json: bool,
}

async fn worktree_list(repo: Option<PathBuf>, json: bool) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let response = client
        .list_worktrees(WorktreeListRequest { cwd: repo })
        .await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }
    println!("repo\t{}", response.repo_root.display());
    for worktree in response.worktrees {
        let marker = if worktree.is_main { "main" } else { "linked" };
        let branch = worktree.branch.unwrap_or_else(|| "<detached>".to_string());
        let commit = worktree.commit.unwrap_or_else(|| "-".to_string());
        println!(
            "{}\t{}\t{}\t{}",
            marker,
            branch,
            commit,
            worktree.path.display()
        );
    }
    Ok(())
}

async fn worktree_create(args: WorktreeCreateCliArgs) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let response = client
        .create_worktree(WorktreeCreateRequest {
            name: args.name,
            cwd: args.repo,
            path: args.path,
            branch: args.branch,
            new_branch: args.new_branch,
            base_ref: args.base.map(|base| match base.into_config() {
                bcode_config::WorktreeBaseRefConfig::Auto => WorktreeBaseRef::Auto,
                bcode_config::WorktreeBaseRefConfig::DefaultBranch => {
                    WorktreeBaseRef::DefaultBranch
                }
                bcode_config::WorktreeBaseRefConfig::Head => WorktreeBaseRef::Head,
            }),
            detach: args.detach,
            force: args.force,
            attach_session_id: args.session,
            new_session: args.new_session,
            no_setup: args.no_setup,
        })
        .await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }
    println!("created\t{}", response.path.display());
    if let Some(branch) = response.branch {
        println!("branch\t{branch}");
    }
    if let Some(session) = response.session {
        println!("session\t{}", session.id);
    }
    Ok(())
}

async fn worktree_remove(
    path: PathBuf,
    repo: Option<PathBuf>,
    force: bool,
    json: bool,
) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let response = client
        .remove_worktree(WorktreeRemoveRequest {
            cwd: repo,
            path,
            force,
        })
        .await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        println!("removed\t{}", response.path.display());
    }
    Ok(())
}

async fn run_new_session_tui(worktree: Option<String>) -> Result<(), CliError> {
    ensure_server_running().await?;
    let client = BcodeClient::default_endpoint();
    let session = if let Some(name) = worktree {
        client
            .create_worktree(WorktreeCreateRequest {
                name,
                cwd: None,
                path: None,
                branch: None,
                new_branch: None,
                base_ref: None,
                detach: false,
                force: false,
                attach_session_id: None,
                new_session: true,
                no_setup: false,
            })
            .await?
            .session
            .ok_or_else(|| {
                CliError::LoginProfile("worktree creation did not return a session".to_string())
            })?
    } else {
        client.create_session(None).await?
    };
    bcode_tui::run(Some(session.id)).await?;
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
        println!("{}\t{}\t{} clients", name, session.id, session.client_count);
    }
    Ok(())
}

async fn rename_session(session_id: SessionId, name: String) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let session = client.rename_session(session_id, Some(name)).await?;
    println!(
        "renamed {} to {}",
        session.id,
        session.name.unwrap_or_else(|| "<unnamed>".to_string())
    );
    Ok(())
}

async fn delete_session(session_id: SessionId) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let session = client.delete_session(session_id).await?;
    println!(
        "deleted {} ({})",
        session.name.unwrap_or_else(|| "<unnamed>".to_string()),
        session.id
    );
    Ok(())
}

async fn session_history(session_id: SessionId) -> Result<(), CliError> {
    for event in paged_session_history(session_id).await? {
        print_session_event(&event);
    }
    Ok(())
}

async fn session_export(
    session_id: SessionId,
    format: SessionExportFormat,
) -> Result<(), CliError> {
    match format {
        SessionExportFormat::Jsonl => {
            for event in paged_session_history(session_id).await? {
                println!("{}", serde_json::to_string(&event)?);
            }
        }
    }
    Ok(())
}

async fn session_timeline(session_id: SessionId) -> Result<(), CliError> {
    let history = paged_session_history(session_id).await?;
    let first_trace_time = history.iter().find_map(|event| match &event.kind {
        SessionEventKind::TraceEvent { trace } => Some(trace.timestamp_ms),
        _ => None,
    });
    for event in history {
        print_timeline_event(&event, first_trace_time);
    }
    Ok(())
}

async fn paged_session_history(session_id: SessionId) -> Result<Vec<SessionEvent>, CliError> {
    let client = BcodeClient::default_endpoint();
    let mut cursor = Some(SessionHistoryCursor { sequence: 0 });
    let mut history = Vec::new();
    while let Some(page_cursor) = cursor {
        let page = client
            .session_history_page(
                session_id,
                SessionHistoryQuery {
                    cursor: Some(page_cursor),
                    limit: SESSION_CLI_PAGE_LIMIT,
                    direction: SessionHistoryDirection::Forward,
                },
            )
            .await?;
        history.extend(page.events);
        cursor = page.next_cursor;
        if !page.has_more {
            break;
        }
    }
    Ok(history)
}

#[derive(Debug, Clone, Serialize)]
struct SessionDiagnosis {
    session_id: SessionId,
    event_count: usize,
    trace_event_count: usize,
    first_sequence: Option<u64>,
    last_sequence: Option<u64>,
    latest_events: Vec<SessionDiagnosisEvent>,
    latest_traces: Vec<SessionDiagnosisTrace>,
}

#[derive(Debug, Clone, Serialize)]
struct SessionDiagnosisEvent {
    sequence: u64,
    kind: String,
}

#[derive(Debug, Clone, Serialize)]
struct SessionDiagnosisTrace {
    sequence: u64,
    timestamp_ms: u64,
    turn_id: Option<String>,
    phase: String,
    payload: bcode_session_models::SessionTracePayload,
}

async fn session_diagnose(session_id: SessionId, json: bool) -> Result<(), CliError> {
    let history = paged_session_history(session_id).await?;
    let diagnosis = SessionDiagnosis::from_history(session_id, &history);
    if json {
        println!("{}", serde_json::to_string_pretty(&diagnosis)?);
    } else {
        print_session_diagnosis(&diagnosis);
    }
    Ok(())
}

impl SessionDiagnosis {
    fn from_history(session_id: SessionId, history: &[SessionEvent]) -> Self {
        let trace_event_count = history
            .iter()
            .filter(|event| matches!(event.kind, SessionEventKind::TraceEvent { .. }))
            .count();
        let latest_events = history
            .iter()
            .rev()
            .take(20)
            .map(|event| SessionDiagnosisEvent {
                sequence: event.sequence,
                kind: session_event_kind_name(&event.kind).to_string(),
            })
            .collect::<Vec<_>>();
        let latest_traces = history
            .iter()
            .rev()
            .filter_map(|event| match &event.kind {
                SessionEventKind::TraceEvent { trace } => Some(SessionDiagnosisTrace {
                    sequence: event.sequence,
                    timestamp_ms: trace.timestamp_ms,
                    turn_id: trace.turn_id.clone(),
                    phase: format!("{:?}", trace.phase),
                    payload: trace.payload.clone(),
                }),
                _ => None,
            })
            .take(50)
            .collect::<Vec<_>>();
        Self {
            session_id,
            event_count: history.len(),
            trace_event_count,
            first_sequence: history.first().map(|event| event.sequence),
            last_sequence: history.last().map(|event| event.sequence),
            latest_events,
            latest_traces,
        }
    }
}

fn print_session_diagnosis(diagnosis: &SessionDiagnosis) {
    println!("session: {}", diagnosis.session_id);
    println!("events: {}", diagnosis.event_count);
    println!("trace events: {}", diagnosis.trace_event_count);
    println!(
        "sequence range: {}..{}",
        diagnosis
            .first_sequence
            .map_or_else(|| "<none>".to_string(), |sequence| sequence.to_string()),
        diagnosis
            .last_sequence
            .map_or_else(|| "<none>".to_string(), |sequence| sequence.to_string())
    );
    println!("latest events:");
    for event in &diagnosis.latest_events {
        println!("  {}\t{}", event.sequence, event.kind);
    }
    println!("latest traces:");
    for trace in &diagnosis.latest_traces {
        println!(
            "  {}\t{}\t{}\t{}",
            trace.sequence,
            trace.timestamp_ms,
            trace.turn_id.as_deref().unwrap_or("<none>"),
            trace.phase
        );
    }
}

const fn session_event_kind_name(kind: &SessionEventKind) -> &'static str {
    match kind {
        SessionEventKind::SessionCreated { .. } => "session_created",
        SessionEventKind::ClientAttached { .. } => "client_attached",
        SessionEventKind::ClientDetached { .. } => "client_detached",
        SessionEventKind::UserMessage { .. } => "user_message",
        SessionEventKind::AssistantDelta { .. } => "assistant_delta",
        SessionEventKind::AssistantMessage { .. } => "assistant_message",
        SessionEventKind::ToolCallRequested { .. } => "tool_call_requested",
        SessionEventKind::ToolCallFinished { .. } => "tool_call_finished",
        SessionEventKind::PermissionRequested { .. } => "permission_requested",
        SessionEventKind::PermissionResolved { .. } => "permission_resolved",
        SessionEventKind::ModelChanged { .. } => "model_changed",
        SessionEventKind::SystemMessage { .. } => "system_message",
        SessionEventKind::AgentChanged { .. } => "agent_changed",
        SessionEventKind::ModelTurnStarted { .. } => "model_turn_started",
        SessionEventKind::ModelTurnFinished { .. } => "model_turn_finished",
        SessionEventKind::ModelUsage { .. } => "model_usage",
        SessionEventKind::ContextCompacted { .. } => "context_compacted",
        SessionEventKind::SessionRenamed { .. } => "session_renamed",
        SessionEventKind::TraceEvent { .. } => "trace_event",
        SessionEventKind::SkillInvoked { .. } => "skill_invoked",
        SessionEventKind::SkillSuggested { .. } => "skill_suggested",
        SessionEventKind::SkillActivated { .. } => "skill_activated",
        SessionEventKind::SkillDeactivated { .. } => "skill_deactivated",
        SessionEventKind::SkillContextLoaded { .. } => "skill_context_loaded",
        SessionEventKind::SkillInvocationFailed { .. } => "skill_invocation_failed",
        SessionEventKind::AssistantReasoningDelta { .. } => "assistant_reasoning_delta",
        SessionEventKind::AssistantReasoningMessage { .. } => "assistant_reasoning_message",
        SessionEventKind::RuntimeWorkStarted { .. } => "runtime_work_started",
        SessionEventKind::RuntimeWorkCancelRequested { .. } => "runtime_work_cancel_requested",
        SessionEventKind::RuntimeWorkFinished { .. } => "runtime_work_finished",
        SessionEventKind::RuntimeWorkProgress { .. } => "runtime_work_progress",
        SessionEventKind::ModelTurnCancelRequested { .. } => "model_turn_cancel_requested",
        SessionEventKind::ToolInvocationStream { .. } => "tool_invocation_stream",
        SessionEventKind::WorkingDirectoryChanged { .. } => "working_directory_changed",
        SessionEventKind::SessionImported { .. } => "session_imported",
    }
}

fn session_doctor(
    session_id: Option<SessionId>,
    fix: bool,
    force: bool,
    json: bool,
) -> Result<(), CliError> {
    let store = bcode_session::SessionEventStore::new(default_session_store_dir());
    let health = if let Some(session_id) = session_id {
        store
            .doctor_session_with_options(session_id, fix, force)?
            .into_iter()
            .collect()
    } else {
        store.doctor_all_with_options(fix, force)?
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&health)?);
        return Ok(());
    }
    if health.is_empty() {
        println!("no persisted sessions found");
        return Ok(());
    }
    for item in health {
        print_session_index_health(&item);
    }
    Ok(())
}

fn print_session_index_health(item: &bcode_session::SessionIndexHealth) {
    let state = if item.issue_count == 0 {
        "ok"
    } else {
        "degraded"
    };
    let freshness = if item.stale { "stale" } else { "fresh" };
    println!(
        "{}\t{}\t{}\tevents={}\tlast_good_offset={}\tissues={}",
        item.session_id,
        state,
        freshness,
        item.event_count,
        item.last_good_offset,
        item.issue_count
    );
}

fn handle_migrate_command(command: MigrateCommand) -> Result<(), CliError> {
    match command {
        MigrateCommand::Status { json } | MigrateCommand::Plan { json } => {
            session_migration_plan(json)
        }
        MigrateCommand::Apply { dry_run, backup } => session_migration_apply(dry_run, backup),
    }
}

async fn handle_session_import_command(command: SessionImportCommand) -> Result<(), CliError> {
    ensure_server_running().await?;
    let client = BcodeClient::default_endpoint();
    match command {
        SessionImportCommand::Sources => {
            let response = client
                .call_plugin_service(
                    SESSION_IMPORT_INTERFACE_ID.to_string(),
                    OP_LIST_IMPORT_SOURCES.to_string(),
                    Vec::new(),
                )
                .await?;
            let sources: ListImportSourcesResponse = serde_json::from_slice(&response.payload)?;
            for source in sources.sources {
                println!("{}\t{}", source.source_id, source.display_name);
            }
        }
        SessionImportCommand::Discover {
            source,
            json,
            diagnostics,
        } => {
            let request = serde_json::to_vec(&DiscoverImportableSessionsRequest {
                include_diagnostics: diagnostics,
                ..DiscoverImportableSessionsRequest::default()
            })?;
            let response = client
                .call_plugin_service(
                    SESSION_IMPORT_INTERFACE_ID.to_string(),
                    OP_DISCOVER_IMPORTABLE_SESSIONS.to_string(),
                    request,
                )
                .await?;
            let mut sessions: DiscoverImportableSessionsResponse =
                serde_json::from_slice(&response.payload)?;
            if let Some(source) = source {
                sessions
                    .sessions
                    .retain(|session| session.source_id == source);
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else if sessions.sessions.is_empty() {
                println!("no importable sessions");
            } else {
                for session in sessions.sessions {
                    let title = session.title.as_deref().unwrap_or("<untitled>");
                    let cwd = session
                        .working_directory
                        .as_ref()
                        .map_or_else(|| "-".to_owned(), |cwd| cwd.display().to_string());
                    let messages = session
                        .message_count
                        .map_or_else(|| "-".to_owned(), |count| count.to_string());
                    let updated = session
                        .updated_at_ms
                        .map_or_else(|| "-".to_owned(), |updated| updated.to_string());
                    let warning_count = session.warnings.len();
                    println!(
                        "[{}]\t{}\t{}\tmessages={}\tupdated={}\twarnings={}\tcwd={}",
                        session.source_id,
                        session.external_session_id,
                        title,
                        messages,
                        updated,
                        warning_count,
                        cwd
                    );
                }
            }
        }
        SessionImportCommand::Open {
            source,
            external_session_id,
        } => {
            let (session, warnings) = client
                .import_external_session(source.clone(), external_session_id)
                .await?;
            println!("{}", session.id);
            if !warnings.is_empty() {
                eprintln!("imported [{source}] with {} warnings", warnings.len());
                for warning in warnings {
                    eprintln!("{}: {}", warning.code, warning.message);
                }
            }
        }
    }
    Ok(())
}

fn handle_session_migrate_command(command: SessionMigrateCommand) -> Result<(), CliError> {
    match command {
        SessionMigrateCommand::Status { json } | SessionMigrateCommand::Plan { json } => {
            session_migration_plan(json)
        }
        SessionMigrateCommand::Apply { dry_run, backup } => {
            session_migration_apply(dry_run, backup)
        }
    }
}

fn session_migration_plan(json: bool) -> Result<(), CliError> {
    let store = bcode_session::SessionEventStore::new(default_session_store_dir());
    let plan = store.migration_plan()?;
    let recovery = store.migration_recovery_status()?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "plan": plan,
                "recovery": recovery,
            }))?
        );
        return Ok(());
    }
    match recovery {
        bcode_session::SessionMigrationRecoveryStatus::Clean => {}
        bcode_session::SessionMigrationRecoveryStatus::NeedsAttention(items) => {
            println!("migration recovery: {} run(s) need attention", items.len());
            for item in items {
                println!(
                    "{}	{}	{:?}	{:?}",
                    item.run_id, item.domain, item.status, item.error
                );
            }
        }
    }
    if plan.is_empty() {
        println!("{}: current", plan.domain);
        return Ok(());
    }
    println!("{}: {} migration item(s)", plan.domain, plan.items.len());
    for item in plan.items {
        let found_version = item
            .found_version
            .map_or_else(|| "none".to_string(), |version| version.to_string());
        let action = match item.action {
            bcode_session::SessionMigrationAction::None => "none",
            bcode_session::SessionMigrationAction::RebuildDerivedIndex => "rebuild-derived-index",
            bcode_session::SessionMigrationAction::RewriteCanonicalEvents => {
                "rewrite-canonical-events"
            }
        };
        let mode = if item.automatic {
            "automatic"
        } else {
            "manual"
        };
        println!(
            "{}\t{}\tfound={}\tcurrent={}\t{}\t{}",
            item.session_id, action, found_version, item.current_version, mode, item.reason
        );
    }
    Ok(())
}

fn session_migration_apply(dry_run: bool, backup: bool) -> Result<(), CliError> {
    let store = bcode_session::SessionEventStore::new(default_session_store_dir());
    let report =
        store.apply_migration_plan(bcode_session::SessionMigrationOptions { dry_run, backup })?;
    if report.items.is_empty() {
        println!("{}: current", report.domain);
        return Ok(());
    }
    if let Some(backup_dir) = &report.backup_dir {
        println!("backup: {}", backup_dir.display());
    }
    println!(
        "{}: {} migration item(s)",
        report.domain,
        report.items.len()
    );
    for item in report.items {
        let action = match item.action {
            bcode_session::SessionMigrationAction::None => "none",
            bcode_session::SessionMigrationAction::RebuildDerivedIndex => "rebuild-derived-index",
            bcode_session::SessionMigrationAction::RewriteCanonicalEvents => {
                "rewrite-canonical-events"
            }
        };
        let status = match item.status {
            bcode_session::SessionMigrationApplyStatus::Planned => "planned",
            bcode_session::SessionMigrationApplyStatus::Applied => "applied",
            bcode_session::SessionMigrationApplyStatus::Skipped => "skipped",
        };
        println!(
            "{}\t{}\t{}\t{}",
            item.session_id, action, status, item.message
        );
    }
    Ok(())
}

fn session_reindex(session_id: Option<SessionId>) -> Result<(), CliError> {
    let store = bcode_session::SessionEventStore::new(default_session_store_dir());
    if let Some(session_id) = session_id {
        store.reindex_session(session_id)?;
        println!("reindexed {session_id}");
        return Ok(());
    }
    let rebuilt = store.reindex_all()?;
    println!("reindexed {} session(s)", rebuilt.len());
    for session_id in rebuilt {
        println!("{session_id}");
    }
    Ok(())
}

fn session_repair(session_id: SessionId) -> Result<(), CliError> {
    let store = bcode_session::SessionEventStore::new(default_session_store_dir());
    match store.repair_session_tail(session_id)? {
        Some(backup) => println!(
            "repaired {session_id}; original backed up at {}",
            backup.display()
        ),
        None => println!("{session_id} has no unreadable tail; index rebuilt"),
    }
    Ok(())
}

fn default_session_store_dir() -> PathBuf {
    bcode_config::default_state_dir().join("sessions")
}

async fn handle_runtime_work_command(command: RuntimeWorkCommand) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    match command {
        RuntimeWorkCommand::List { session_id } => {
            for work in client.list_runtime_work(session_id).await? {
                println!(
                    "{} {:?} {:?} {} cancellable={}",
                    work.work_id, work.kind, work.status, work.label, work.cancellable
                );
            }
        }
        RuntimeWorkCommand::Cancel {
            session_id,
            work_id,
        } => {
            if client
                .cancel_runtime_work(
                    session_id,
                    bcode_session_models::RuntimeWorkId::new(work_id),
                )
                .await?
            {
                println!("runtime work cancellation requested");
            } else {
                println!("no active runtime work");
            }
        }
        RuntimeWorkCommand::History { session_id, limit } => {
            for span in client.runtime_work_spans(session_id, limit).await? {
                println!(
                    "{} status={:?} cancelled={} duration_ms={:?} parent={} label={}{}",
                    span.work_id,
                    span.status,
                    span.cancelled,
                    span.duration_ms(),
                    span.parent_work_id
                        .as_ref()
                        .map_or_else(|| "-".to_string(), ToString::to_string),
                    span.label,
                    span.message
                        .as_ref()
                        .map_or_else(String::new, |message| format!(" message={message}"))
                );
            }
        }
        RuntimeWorkCommand::Watch { session_id } => {
            let mut watcher = client.watch_runtime_work(session_id).await?;
            loop {
                let event = watcher.next_event().await?;
                print_session_event(&event);
            }
        }
    }
    Ok(())
}

async fn cancel_session_turn(session_id: SessionId, clear_queue: bool) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    if client
        .cancel_session_turn_with_options(session_id, clear_queue)
        .await?
    {
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

async fn add_permission_rule(
    agent_id: &str,
    category: &str,
    pattern: String,
    action: &str,
) -> Result<(), CliError> {
    let config_path = BcodeClient::default_endpoint()
        .add_permission_rule(
            agent_id.to_string(),
            category.to_string(),
            pattern,
            action.to_string(),
        )
        .await?;
    println!("permission rule added: {config_path}");
    Ok(())
}

fn print_permission(permission: &PermissionSummary) {
    println!(
        "{}\t{}\t{}\t{}\t{}\t{}",
        permission.permission_id,
        permission.session_id,
        permission.tool_call_id,
        permission.tool_name,
        permission.agent_id,
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
                    Event::Session(event) | Event::RuntimeWork(event) => print_session_event(&event),
                    Event::SessionCatalogUpdated { .. } => {}
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
        SessionEventKind::TraceEvent { trace } => print_trace_session_event(event, trace),
        _ => print_non_trace_session_event(event),
    }
}

#[allow(clippy::too_many_lines)]
fn print_non_trace_session_event(event: &SessionEvent) {
    match &event.kind {
        SessionEventKind::SessionCreated { name, .. } => {
            let name = name.as_deref().unwrap_or("<unnamed>");
            println!("#{} session created: {name}", event.sequence);
        }
        SessionEventKind::SessionRenamed { name } => {
            let name = name.as_deref().unwrap_or("<unnamed>");
            println!("#{} session renamed: {name}", event.sequence);
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
        SessionEventKind::AssistantReasoningDelta { text }
        | SessionEventKind::AssistantReasoningMessage { text } => {
            println!("thinking: {text}");
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
            output,
        } => {
            let status = if *is_error { "error" } else { "ok" };
            let artifact = output
                .as_ref()
                .map_or_else(String::new, |output| format!(" artifact={}", output.path));
            println!(
                "#{} tool call finished ({status}): {tool_call_id}: {result}{artifact}",
                event.sequence
            );
        }
        SessionEventKind::ToolInvocationStream {
            event: stream_event,
        } => {
            println!("#{} tool stream: {stream_event:?}", event.sequence);
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
        SessionEventKind::AgentChanged { agent_id } => {
            println!("#{} agent changed: {agent_id}", event.sequence);
        }
        SessionEventKind::SystemMessage { text } => {
            println!("#{} system: {text}", event.sequence);
        }
        SessionEventKind::WorkingDirectoryChanged {
            old_working_directory,
            new_working_directory,
        } => {
            println!(
                "#{} working directory changed: {} -> {}",
                event.sequence,
                old_working_directory.display(),
                new_working_directory.display()
            );
        }
        SessionEventKind::SessionImported {
            source_id,
            external_session_id,
            ..
        } => println!(
            "#{} session imported: [{source_id}] {external_session_id}",
            event.sequence
        ),
        SessionEventKind::ContextCompacted {
            compacted_through_sequence,
            ..
        } => println!(
            "#{} context compacted through #{compacted_through_sequence}",
            event.sequence
        ),
        SessionEventKind::ModelTurnStarted { turn_id } => {
            println!("#{} model turn started: {turn_id}", event.sequence);
        }
        SessionEventKind::ModelTurnCancelRequested { turn_id, .. } => {
            println!(
                "#{} model turn cancellation requested: {turn_id}",
                event.sequence
            );
        }
        SessionEventKind::ModelTurnFinished {
            turn_id,
            outcome,
            message,
        } => {
            println!(
                "#{} model turn finished: {turn_id} {outcome:?} {}",
                event.sequence,
                message.as_deref().unwrap_or("")
            );
        }
        SessionEventKind::ModelUsage { turn_id, usage } => {
            print_model_usage_event(event.sequence, turn_id, usage);
        }
        SessionEventKind::SkillInvoked {
            skill_id,
            arguments,
            ..
        } => println!("#{} skill invoked: {skill_id} {arguments}", event.sequence),
        SessionEventKind::SkillSuggested {
            skill_id, reason, ..
        } => println!(
            "#{} skill suggested: {skill_id} {}",
            event.sequence,
            reason.as_deref().unwrap_or("")
        ),
        SessionEventKind::SkillActivated { skill_id, .. } => {
            println!("#{} skill activated: {skill_id}", event.sequence);
        }
        SessionEventKind::SkillDeactivated { skill_id, .. } => {
            println!("#{} skill deactivated: {skill_id}", event.sequence);
        }
        SessionEventKind::SkillContextLoaded {
            skill_id,
            bytes_loaded,
            truncated,
            ..
        } => println!(
            "#{} skill context loaded: {skill_id} bytes={bytes_loaded} truncated={truncated}",
            event.sequence
        ),
        SessionEventKind::SkillInvocationFailed {
            skill_id, error, ..
        } => println!(
            "#{} skill invocation failed: {skill_id}: {error}",
            event.sequence
        ),
        SessionEventKind::RuntimeWorkStarted {
            work_id,
            kind,
            label,
            cancellable,
            ..
        } => println!(
            "#{} runtime work started: {work_id} {kind:?} {label} cancellable={cancellable}",
            event.sequence
        ),
        SessionEventKind::RuntimeWorkCancelRequested { work_id, .. } => println!(
            "#{} runtime work cancel requested: {work_id}",
            event.sequence
        ),
        SessionEventKind::RuntimeWorkProgress {
            work_id, message, ..
        } => println!(
            "#{} runtime work progress: {work_id} {}",
            event.sequence, message
        ),
        SessionEventKind::RuntimeWorkFinished {
            work_id,
            status,
            message,
            ..
        } => println!(
            "#{} runtime work finished: {work_id} {status:?} {}",
            event.sequence,
            message.as_deref().unwrap_or("")
        ),
        SessionEventKind::TraceEvent { .. } => {}
    }
}

fn print_trace_session_event(
    event: &SessionEvent,
    trace: &bcode_session_models::SessionTraceEvent,
) {
    println!(
        "#{} trace {:?}: {}",
        event.sequence,
        trace.phase,
        trace_payload_summary(&trace.payload)
    );
}

fn print_timeline_event(event: &SessionEvent, first_trace_time: Option<u64>) {
    let prefix = match &event.kind {
        SessionEventKind::TraceEvent { trace } => first_trace_time.map_or_else(
            || format!("#{}", event.sequence),
            |start| {
                format!(
                    "+{}.{:03}s #{}",
                    trace.timestamp_ms.saturating_sub(start) / 1000,
                    trace.timestamp_ms.saturating_sub(start) % 1000,
                    event.sequence
                )
            },
        ),
        _ => format!("          #{}", event.sequence),
    };
    match &event.kind {
        SessionEventKind::UserMessage { text, .. } => println!("{prefix} user: {}", one_line(text)),
        SessionEventKind::AssistantMessage { text } => {
            println!("{prefix} assistant: {}", one_line(text));
        }
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            tool_name,
            ..
        } => {
            println!("{prefix} tool requested: {tool_name} ({tool_call_id})");
        }
        SessionEventKind::ToolCallFinished {
            tool_call_id,
            is_error,
            ..
        } => {
            let status = if *is_error { "error" } else { "ok" };
            println!("{prefix} tool finished: {tool_call_id} {status}");
        }
        SessionEventKind::ModelTurnStarted { turn_id } => {
            println!("{prefix} model turn started: {turn_id}");
        }
        SessionEventKind::ModelTurnFinished {
            turn_id, outcome, ..
        } => {
            println!("{prefix} model turn finished: {turn_id} {outcome:?}");
        }
        SessionEventKind::ModelUsage { turn_id, usage } => {
            println!(
                "{prefix} usage: {turn_id} total={:?} cached={:?}",
                usage.metered_total_tokens(),
                usage.cached_input_tokens
            );
        }
        SessionEventKind::RuntimeWorkStarted { work_id, label, .. } => {
            println!("{prefix} runtime work started: {work_id} {label}");
        }
        SessionEventKind::RuntimeWorkCancelRequested { work_id, .. } => {
            println!("{prefix} runtime work cancel requested: {work_id}");
        }
        SessionEventKind::RuntimeWorkProgress {
            work_id, message, ..
        } => {
            println!("{prefix} runtime work progress: {work_id} {message}");
        }
        SessionEventKind::RuntimeWorkFinished {
            work_id, status, ..
        } => {
            println!("{prefix} runtime work finished: {work_id} {status:?}");
        }
        SessionEventKind::TraceEvent { trace } => {
            println!(
                "{prefix} trace {:?}: {}",
                trace.phase,
                trace_payload_summary(&trace.payload)
            );
        }
        _ => {}
    }
}

fn provider_stream_event_summary(event: &bcode_session_models::ProviderStreamEvent) -> String {
    match event {
        bcode_session_models::ProviderStreamEvent::TurnStarted => {
            "provider stream turn started".to_string()
        }
        bcode_session_models::ProviderStreamEvent::ToolCallStarted {
            tool_call_id,
            tool_name,
        } => format!("provider stream tool started {tool_name} ({tool_call_id})"),
        bcode_session_models::ProviderStreamEvent::ToolCallProgress {
            tool_call_id,
            tool_name,
            argument_bytes,
        } => format!(
            "provider stream tool assembled {tool_name} ({tool_call_id}) bytes={argument_bytes}"
        ),
        bcode_session_models::ProviderStreamEvent::ToolCallFinished {
            tool_call_id,
            tool_name,
        } => format!("provider stream tool finished {tool_name} ({tool_call_id})"),
        bcode_session_models::ProviderStreamEvent::NoProgressWarning {
            idle_seconds,
            active_tool_call,
        } => active_tool_call.as_ref().map_or_else(
            || format!("provider stream no progress idle_seconds={idle_seconds}"),
            |progress| {
                format!(
                    "provider stream no progress idle_seconds={idle_seconds} tool={} ({}) bytes={}",
                    progress.tool_name, progress.tool_call_id, progress.argument_bytes
                )
            },
        ),
    }
}

fn trace_payload_summary(payload: &bcode_session_models::SessionTracePayload) -> String {
    match payload {
        bcode_session_models::SessionTracePayload::ModelRequestBuilt {
            provider,
            model,
            message_count,
            tool_count,
            uses_previous_provider_response,
            ..
        } => format!(
            "model request provider={provider} model={model} messages={message_count} tools={tool_count} reuse={uses_previous_provider_response}"
        ),
        bcode_session_models::SessionTracePayload::ProviderRound {
            provider,
            provider_turn_id,
            stop_reason,
            duration_ms,
            error,
            ..
        } => format!(
            "provider round provider={provider} turn={} stop={} duration_ms={}{}",
            provider_turn_id.as_deref().unwrap_or("<none>"),
            stop_reason.as_deref().unwrap_or("<pending>"),
            duration_ms.map_or_else(|| "<pending>".to_string(), |value| value.to_string()),
            error
                .as_ref()
                .map_or_else(String::new, |error| format!(" error={}", one_line(error)))
        ),
        bcode_session_models::SessionTracePayload::ProviderEvent { event_type, detail } => {
            format!(
                "provider event {event_type}{}",
                detail
                    .as_ref()
                    .map_or_else(String::new, |detail| format!(" {}", one_line(detail)))
            )
        }
        bcode_session_models::SessionTracePayload::ProviderStreamEvent(event) => {
            provider_stream_event_summary(event)
        }
        bcode_session_models::SessionTracePayload::ToolInvocationStarted {
            tool_call_id,
            plugin_id,
            tool_name,
            ..
        } => {
            format!("tool started {tool_name} ({tool_call_id}) plugin={plugin_id}")
        }
        bcode_session_models::SessionTracePayload::ToolPolicyEvaluated {
            tool_call_id,
            agent_id,
            decision,
            reason,
        } => format!(
            "tool policy {tool_call_id} agent={agent_id} decision={decision}{}",
            reason.as_ref().map_or_else(String::new, |reason| format!(
                " reason={}",
                one_line(reason)
            ))
        ),
        bcode_session_models::SessionTracePayload::ToolPermissionWait {
            permission_id,
            tool_call_id,
            approved,
            duration_ms,
        } => format!(
            "permission {permission_id} tool={tool_call_id} approved={approved:?} duration_ms={duration_ms:?}"
        ),
        bcode_session_models::SessionTracePayload::ToolInvocationFinished {
            tool_call_id,
            duration_ms,
            is_error,
            output_bytes,
            ..
        } => format!(
            "tool finished {tool_call_id} duration_ms={duration_ms} error={is_error} output_bytes={output_bytes}"
        ),
        bcode_session_models::SessionTracePayload::ToolInvocationStreamEvent(event) => {
            format!("tool stream {event:?}")
        }
        bcode_session_models::SessionTracePayload::ContextCompaction {
            reason,
            projected_context_chars,
            compacted,
            message,
        } => format!(
            "context compaction reason={reason} projected_context_chars={projected_context_chars} compacted={compacted}{}",
            message.as_ref().map_or_else(String::new, |message| format!(
                " message={}",
                one_line(message)
            ))
        ),
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn print_model_usage_event(
    sequence: u64,
    turn_id: &str,
    usage: &bcode_session_models::SessionTokenUsage,
) {
    println!(
        "#{sequence} model usage: {turn_id} input={:?} output={:?} total={:?} cached={:?} cache_write={:?} reasoning={:?}",
        usage.input_tokens,
        usage.output_tokens,
        usage.metered_total_tokens(),
        usage.cached_input_tokens,
        usage.cache_write_input_tokens,
        usage.reasoning_tokens,
    );
}
