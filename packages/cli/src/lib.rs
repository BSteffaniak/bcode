#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Command-line interface for Bcode.

mod plugin_cli;
pub mod retired_catalogs;
mod session_migration_adapter;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bcode_client::{BcodeClient, ClientError, DaemonAvailability, SessionWatchEvent};
use bcode_config::AuthMode;
use bcode_ipc::{ServerStatus, default_endpoint};
use bcode_model_provider_runtime::{
    BlockingModelProviderInvoker, SingleTurnRequest, SingleTurnStatus, run_single_turn_blocking,
};
use bcode_plugin_sdk::path::{display, display_from_current_dir};
use bcode_session_import::{
    DiscoverImportableSessionsRequest, DiscoverImportableSessionsResponse,
    ListImportSourcesResponse, OP_DISCOVER_IMPORTABLE_SESSIONS, OP_LIST_IMPORT_SOURCES,
    SESSION_IMPORT_INTERFACE_ID,
};
use bcode_session_migration::{
    SessionDiagnosisClassification, SessionDiagnosisCompatibility, classify_session_diagnosis,
};
use bcode_session_models::PermissionSummary;
use bcode_session_models::{
    SessionEvent, SessionEventCompatibilityIssue, SessionEventCompatibilityKind, SessionEventKind,
    SessionHistoryAroundQuery, SessionHistoryCursor, SessionHistoryDirection, SessionHistoryQuery,
    SessionId, SessionInspectionCategory, SessionInspectionQuery, SessionLiveEvent,
    SessionLiveEventKind,
};
use bcode_worktree_models::WorktreeCreateRequest;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use rand::TryRngCore as _;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::future::Future;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
    #[error("daemon lifecycle error: {0}")]
    DaemonLifecycle(#[from] bcode_daemon_lifecycle::DaemonLifecycleError),
    #[error("daemon start error: {0}")]
    DaemonStart(#[from] bcode_daemon_lifecycle::DaemonStartError),
    #[error("config error: {0}")]
    Config(#[from] bcode_config::ConfigError),
    #[error("server error: {0}")]
    Server(#[from] bcode_server::ServerError),
    #[error("workflow store error: {0}")]
    WorkflowStore(#[from] bcode_workflow_store::WorkflowStoreError),
    #[error("session database error: {0}")]
    SessionDb(#[from] bcode_session::db::SessionDbError),
    #[error("session lease error: {0}")]
    SessionLease(#[from] bcode_session::lease::SessionLeaseError),
    #[error("session store error: {0}")]
    SessionStore(#[from] bcode_session::SessionStoreError),
    #[error("session error: {0}")]
    Session(#[from] bcode_session::SessionError),
    #[error("session migration backup error: {0}")]
    SessionMigrationBackup(#[from] bcode_session_migration::MigrationBackupError),
    #[error("session migration storage error: {0}")]
    SessionMigrationStorage(#[from] bcode_session::ownership::SessionStorageRecoveryError),
    #[error("session repair error: {0}")]
    SessionRepair(#[from] bcode_session::repair::SessionRepairError),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("settings error: {0}")]
    Settings(#[from] bcode_settings::SettingsError),
    #[error("TUI error: {0}")]
    Tui(#[from] bcode_tui::TuiError),
    #[error("plugin error: {0}")]
    Plugin(#[from] bcode_plugin::PluginLoadError),
    #[error("plugin service call error: {0}")]
    PluginServiceCall(#[from] bcode_plugin::PluginServiceCallError),
    #[cfg(feature = "web-renderer")]
    #[error("HyperChad renderer error: {0}")]
    HyperChadRender(String),
    #[error("sshenv error: {0}")]
    Sshenv(String),
    #[error("session history accepts only one of --after or --before")]
    InvalidSessionHistoryRange,
    /// I/O failure, including signal-listener setup failures (not a received signal).
    #[error("I/O error: {0}")]
    Signal(#[from] std::io::Error),
    #[error("--new cannot be combined with a subcommand")]
    NewSessionWithCommand,
    #[error("{0}")]
    LoginProfile(String),
    /// Provider authentication ended with an explicit cancelled outcome.
    #[error("Provider authentication flow was cancelled.")]
    AuthFlowCancelled,
    #[error("bundled plugin install failed: {0}")]
    BundledPluginInstallFailed(String),
    #[error("plugin service error {code}: {message}")]
    PluginService { code: String, message: String },
    #[error("session maintenance blocked by incompatible live daemon: {0}")]
    IncompatibleDaemonStorage(String),
    #[error("session repair usage error: {0}")]
    SessionRepairUsage(String),
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("turn was rejected: {0:?}")]
    TurnRejected(bcode_session_models::TurnRejectionReason),
    #[error("turn was cancelled before start")]
    TurnCancelledBeforeStart,
    #[error("session-search backfill was cancelled")]
    SessionSearchBackfillCancelled,
    #[error("session-search backfill did not complete")]
    SessionSearchBackfillIncomplete,
    #[error("tool exchange resolution is malformed or unsupported")]
    InvalidExchangeResolution,
    #[error(transparent)]
    PluginCliComposition(#[from] plugin_cli::CompositionError),
    #[error("plugin CLI command failed: {0}")]
    PluginCli(String),
    #[error("theme command failed: {0}")]
    Theme(String),
    #[error("theme filesystem error: {0}")]
    ThemeIo(std::io::Error),
    #[error("plugin surface repository path error: {0}")]
    SurfaceRepoPath(String),
    #[error("{0}")]
    AuthPrimeFailed(String),
}

impl CliError {
    /// Stable process exit status for this CLI failure category.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::InvalidArguments(_)
            | Self::InvalidSessionHistoryRange
            | Self::NewSessionWithCommand
            | Self::SessionRepairUsage(_)
            | Self::InvalidExchangeResolution
            | Self::Json(_) => 2,
            Self::Client(ClientError::Server { code, .. })
                if matches!(
                    code.as_str(),
                    "invalid_exchange_resolution"
                        | "invalid_invocation_input_producer"
                        | "invalid_invocation_input_schema"
                        | "invalid_invocation_input_id"
                        | "invocation_input_too_large"
                ) =>
            {
                2
            }
            Self::Client(ClientError::Server { code, .. })
                if matches!(
                    code.as_str(),
                    "authorization_denied" | "workflow_operation_unauthorized"
                ) =>
            {
                3
            }
            Self::TurnRejected(bcode_session_models::TurnRejectionReason::ExecutionPolicy) => 3,
            Self::TurnRejected(bcode_session_models::TurnRejectionReason::SessionUnavailable) => 1,
            Self::Client(ClientError::Server { code, .. })
                if matches!(
                    code.as_str(),
                    "cancelled" | "workflow_computation_cancelled" | "worktree_create_cancelled"
                ) =>
            {
                4
            }
            Self::TurnCancelledBeforeStart
            | Self::SessionSearchBackfillCancelled
            | Self::AuthFlowCancelled => 4,
            Self::SessionSearchBackfillIncomplete => 1,
            Self::Signal(_) => 1,
            Self::Client(_)
            | Self::DaemonLifecycle(_)
            | Self::DaemonStart(_)
            | Self::Config(_)
            | Self::Server(_)
            | Self::WorkflowStore(_)
            | Self::SessionDb(_)
            | Self::SessionLease(_)
            | Self::SessionStore(_)
            | Self::Session(_)
            | Self::SessionMigrationBackup(_)
            | Self::SessionMigrationStorage(_)
            | Self::SessionRepair(_)
            | Self::Settings(_)
            | Self::Tui(_)
            | Self::Plugin(_)
            | Self::PluginServiceCall(_)
            | Self::LoginProfile(_)
            | Self::BundledPluginInstallFailed(_)
            | Self::PluginService { .. }
            | Self::IncompatibleDaemonStorage(_)
            | Self::PluginCliComposition(_)
            | Self::PluginCli(_)
            | Self::Theme(_)
            | Self::ThemeIo(_)
            | Self::SurfaceRepoPath(_)
            | Self::AuthPrimeFailed(_)
            | Self::Sshenv(_) => 1,
            #[cfg(feature = "web-renderer")]
            Self::HyperChadRender(_) => 1,
        }
    }
}

use std::sync::OnceLock;

static STATIC_BUNDLED_PLUGINS: OnceLock<Vec<bcode_plugin::StaticBundledPlugin>> = OnceLock::new();
static STATIC_BUNDLED_PLUGIN_IDS: OnceLock<Vec<String>> = OnceLock::new();
static STATIC_BUNDLED_DEFAULT_PLUGIN_IDS: OnceLock<Vec<String>> = OnceLock::new();
static BUILD_INFO: OnceLock<bcode_build_info::BuildInfo> = OnceLock::new();

fn build_info() -> &'static bcode_build_info::BuildInfo {
    BUILD_INFO
        .get()
        .expect("Bcode CLI build information must be initialized")
}

/// Parse CLI arguments and run the requested command.
///
/// # Errors
///
/// Returns an error when the requested command fails.
pub async fn run(build_info: bcode_build_info::BuildInfo) -> Result<(), CliError> {
    run_with_static_bundled(build_info, Vec::new()).await
}

/// Parse CLI arguments and run with caller-provided static bundled plugins.
///
/// # Errors
///
/// Returns an error when the requested command fails.
///
/// # Panics
///
/// Panics when CLI startup is initialized more than once in one process with
/// independently supplied build information.
pub async fn run_with_static_bundled(
    artifact_build_info: bcode_build_info::BuildInfo,
    static_plugins: Vec<bcode_plugin::StaticBundledPlugin>,
) -> Result<(), CliError> {
    init_tracing();
    register_workflow_artifact_retention();
    BUILD_INFO
        .set(artifact_build_info.clone())
        .expect("Bcode CLI build information initialized more than once");
    bcode_tui::initialize_build_info(artifact_build_info);
    bcode_daemon_lifecycle::initialize_artifact_bootstrap()?;
    let static_plugin_ids = bcode_plugin::static_bundled_plugin_ids(&static_plugins)?;
    let static_default_plugin_ids =
        bcode_plugin::static_bundled_default_plugin_ids(&static_plugins)?;
    let _ = STATIC_BUNDLED_PLUGINS.set(static_plugins);
    let _ = STATIC_BUNDLED_PLUGIN_IDS.set(static_plugin_ids);
    let _ = STATIC_BUNDLED_DEFAULT_PLUGIN_IDS.set(static_default_plugin_ids);
    let registrations = plugin_cli::registrations(
        STATIC_BUNDLED_PLUGINS.get().map_or(&[][..], Vec::as_slice),
        STATIC_BUNDLED_PLUGIN_IDS
            .get()
            .map_or(&[][..], Vec::as_slice),
    )?;
    let mut command =
        plugin_cli::compose(root_command_with_build_info(build_info()), &registrations);
    command = command.version(build_info().display_version());
    let matches = command.get_matches();
    let _config_override = config_override_from_matches(&matches);
    let _state_location = state_location_from_matches(&matches)?;
    let exceptional_execution_mode = matches.get_flag("dangerously_bypass_all_permissions")
        || matches.get_flag("disable_all_tools");
    if let Some(plugin) = plugin_cli::matched(&matches, &registrations)
        && let Some((_, subcommand_matches)) = matches.subcommand()
    {
        if exceptional_execution_mode {
            return Err(CliError::InvalidArguments(
                "execution-mode flags are not supported by plugin CLI commands".to_owned(),
            ));
        }
        if plugin.requires_daemon {
            ensure_server_running().await?;
        }
        let outcome = plugin
            .invoke(subcommand_matches.clone())
            .await
            .map_err(CliError::PluginCli)?;
        match outcome.host_action {
            Some(bcode_plugin_sdk::StaticCliHostAction::OpenTuiSurface {
                surface_kind,
                repo_path,
                options,
            }) => {
                let repo_path = resolve_surface_repo_path(repo_path)?;
                ensure_server_running().await?;
                bcode_tui::run_plugin_surface(surface_kind, repo_path, options).await?;
            }
            Some(bcode_plugin_sdk::StaticCliHostAction::AttachSession { session_id }) => {
                ensure_server_running().await?;
                attach_session(session_id).await?;
            }
            None => {}
        }
        return Ok(());
    }
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(error) => error.exit(),
    };
    Box::pin(handle_cli(cli)).await
}

/// Resolve a plugin-supplied surface repository path against the CLI process working directory.
///
/// Plugin CLI handlers express "current directory" as a relative path such as `.`. That relative
/// path must be resolved here, in the client process, because the surface context is forwarded to
/// the daemon, which has its own unrelated working directory inherited from whichever invocation
/// first started it.
fn resolve_surface_repo_path(
    repo_path: Option<std::path::PathBuf>,
) -> Result<Option<std::path::PathBuf>, CliError> {
    let caller_cwd = std::env::current_dir().map_err(|error| {
        CliError::SurfaceRepoPath(format!("current working directory is unavailable: {error}"))
    })?;
    let requested = repo_path.map_or_else(
        || caller_cwd.clone(),
        |path| {
            if path.is_absolute() {
                path
            } else {
                caller_cwd.join(path)
            }
        },
    );
    let resolved = fs::canonicalize(&requested).map_err(|error| {
        CliError::SurfaceRepoPath(format!(
            "{} is unavailable: {error}",
            display_from_current_dir(&requested)
        ))
    })?;
    Ok(Some(resolved))
}

fn config_override_from_matches(
    matches: &clap::ArgMatches,
) -> Option<bcode_config::ConfigOverrideGuard> {
    let context = matches.get_one::<String>("context");
    let profile = matches.get_one::<String>("profile");
    let request_timeout_secs = matches.get_one::<u64>("request_timeout_secs");
    if profile.is_none() && request_timeout_secs.is_none() && context.is_none() {
        return None;
    }
    let mut override_toml = String::new();
    if let Some(profile) = profile {
        override_toml.push_str(&bcode_config::model_profile_override_toml(profile));
    }
    if let Some(timeout) = request_timeout_secs {
        use std::fmt::Write as _;
        writeln!(override_toml, "[client]\nrequest_timeout_secs = {timeout}")
            .expect("writing to string should not fail");
    }
    if let Some(context) = context {
        use std::fmt::Write as _;
        writeln!(
            override_toml,
            "\n[contexts]\nactive = {}",
            toml::Value::String(context.clone())
        )
        .expect("writing to string should not fail");
    }
    Some(bcode_config::push_process_config_overrides(
        bcode_config::ConfigLoadOverrides::from_env_with_cli(None, Some(override_toml)),
    ))
}

fn state_location_from_matches(
    matches: &clap::ArgMatches,
) -> Result<Option<bcode_config::StateLocationGuard>, CliError> {
    let selection = bcode_config::StateLocationSelection {
        root: matches.get_one::<PathBuf>("state_root").cloned(),
        profile: matches.get_one::<String>("state_profile").cloned(),
    };
    if selection.is_empty() {
        return Ok(None);
    }
    let config = bcode_config::load_config()?;
    let resolved = bcode_config::resolve_state_location_set(&config.state, &selection)
        .map_err(|error| CliError::InvalidArguments(error.to_string()))?;
    Ok(Some(bcode_config::push_process_state_location(
        resolved.primary(),
    )))
}

async fn handle_cli(cli: Cli) -> Result<(), CliError> {
    let _ = (&cli.profile, cli.request_timeout_secs);
    let _ = (&cli.state_root, &cli.state_profile);
    let launch_options = cli.launch_options();
    if launch_options != bcode_tui::TuiLaunchOptions::default() && !cli.supports_execution_mode() {
        return Err(CliError::InvalidArguments(
            "execution-mode flags are supported only for TUI launches, --new, and send".to_owned(),
        ));
    }
    if cli.new {
        if cli.command.is_some() {
            return Err(CliError::NewSessionWithCommand);
        }
        Box::pin(run_new_session_tui(cli.worktree, launch_options)).await?;
        return Ok(());
    }
    if cli.onboard || (cli.command.is_none() && should_auto_start_onboarding()?) {
        handle_onboard_command(&OnboardOptions {
            no_credential_discovery: cli.no_credential_discovery,
            ..OnboardOptions::default()
        })
        .await?;
        return Ok(());
    }
    match cli.command.unwrap_or_default() {
        Commands::Settings {
            file,
            key,
            value,
            remove,
        } => credential_setup::edit_setting(file, &key, value.as_deref(), remove)?,
        Commands::Onboard {
            reset,
            dry_run,
            non_interactive,
            provider,
            skip_launch,
            control_center,
            secure_import_env,
        } => {
            handle_onboard_flags(
                reset,
                onboard_output_mode(dry_run, non_interactive),
                provider,
                if skip_launch {
                    OnboardLaunchMode::SkipLaunch
                } else {
                    OnboardLaunchMode::LaunchWhenReady
                },
                if control_center {
                    OnboardExperienceMode::ControlCenter
                } else {
                    OnboardExperienceMode::FirstRun
                },
                secure_import_env,
                cli.no_credential_discovery,
            )
            .await?;
        }
        Commands::ArtifactId => {
            let mut output = std::io::stdout().lock();
            writeln!(output, "{}", bcode_ipc::ArtifactId::current())?;
            output.flush()?;
        }
        Commands::Server { command } => handle_server_command(command).await?,
        Commands::Session { command } => handle_session_command(Box::new(command)).await?,
        Commands::State { command } => handle_state_command(command)?,
        #[cfg(feature = "web-renderer")]
        Commands::Web {
            bind,
            port,
            allow_non_loopback,
        } => handle_web_command(bind, port, allow_non_loopback).await?,
        Commands::Plugin { command } => handle_plugin_command(command).await?,
        Commands::Theme { command } => handle_theme_command(command)?,
        Commands::Model { command } => handle_model_command(command).await?,
        Commands::Auth { command } => {
            if cli.no_credential_discovery && matches!(command, AuthCommand::Discover) {
                println!("Automatic credential discovery is disabled.");
            } else {
                handle_auth_command(command).await?;
            }
        }
        Commands::Login { command } => handle_login_command(command).await?,
        Commands::Permission { command } => handle_permission_command(command).await?,
        Commands::Interaction { command } => handle_interaction_command(command).await?,
        Commands::Worktree { command } => handle_worktree_command(command).await?,
        Commands::RuntimeWork { command } => handle_runtime_work_command(command).await?,
        Commands::Workflow { command } => handle_workflow_command(Box::new(command)).await?,
        command => Box::pin(handle_session_io_command(command, launch_options)).await?,
    }
    Ok(())
}

// Bound user-supplied theme documents before parsing, including growing files.
const MAX_CLI_THEME_BYTES: usize = 1024 * 1024;

fn theme_catalog_json(catalog: &bcode_tui::theme::definition::ThemeCatalog) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "themes": catalog.definitions().map(|definition| serde_json::json!({
            "id": definition.id(),
            "display_name": definition.display_name(),
            "source": "bundled",
            "dark": definition.has_dark_variant(),
            "light": definition.has_light_variant(),
        })).collect::<Vec<_>>()
    })
}

const fn theme_variant_label(dark: bool, light: bool) -> &'static str {
    match (dark, light) {
        (true, true) => "dark,light",
        (true, false) => "dark",
        (false, true) => "light",
        (false, false) => "-",
    }
}

fn theme_copy_json(
    builtin: &str,
    path: &Path,
    json: bool,
) -> Result<Option<serde_json::Value>, CliError> {
    // Validate path serialization before creating or replacing the destination.
    if !json {
        return Ok(None);
    }
    Ok(Some(serde_json::json!({
        "schema_version": 1,
        "builtin": builtin,
        "path": serde_json::to_value(path)?,
    })))
}

fn handle_theme_command(command: ThemeCommand) -> Result<(), CliError> {
    write_theme_command(command, &mut std::io::stdout().lock())
}

fn write_theme_command(
    command: ThemeCommand,
    writer: &mut impl std::io::Write,
) -> Result<(), CliError> {
    use bcode_tui::theme::definition::{ThemeCatalog, ThemeSelection, parse_theme_definition};

    match command {
        ThemeCommand::List { json } => {
            let catalog =
                ThemeCatalog::bundled().map_err(|error| CliError::Theme(error.to_string()))?;
            if json {
                return write_json_result(writer, &theme_catalog_json(&catalog));
            }
            for definition in catalog.definitions() {
                let variants = theme_variant_label(
                    definition.has_dark_variant(),
                    definition.has_light_variant(),
                );
                writeln!(
                    writer,
                    "{}\t{}\tbundled\t{}",
                    definition.id(),
                    definition.display_name(),
                    variants
                )?;
            }
        }
        ThemeCommand::Validate { path, json } => {
            let bytes = read_bytes_with_limit(
                std::fs::File::open(&path).map_err(CliError::ThemeIo)?,
                MAX_CLI_THEME_BYTES,
                "theme definition",
            )?;
            let source = String::from_utf8(bytes).map_err(|_| {
                CliError::InvalidArguments("theme definition must be valid UTF-8".to_owned())
            })?;
            let definition = parse_theme_definition(path.display().to_string(), &source)
                .map_err(|error| CliError::Theme(error.to_string()))?;
            let id = definition.id().to_owned();
            let mut catalog =
                ThemeCatalog::bundled().map_err(|error| CliError::Theme(error.to_string()))?;
            catalog.insert(definition);
            let resolved = catalog
                .resolve(&ThemeSelection::new(&id))
                .map_err(|error| CliError::Theme(error.to_string()))?;
            if json {
                return write_json_result(
                    writer,
                    &serde_json::json!({
                        "schema_version": 1,
                        "valid": true,
                        "id": id,
                        "fingerprint": resolved.fingerprint,
                    }),
                );
            }
            writeln!(writer, "valid\t{id}\t{}", resolved.fingerprint)?;
        }
        ThemeCommand::Copy {
            builtin,
            path,
            force,
            json,
        } => {
            let result = theme_copy_json(&builtin, &path, json)?;
            let source = ThemeCatalog::bundled_source(&builtin)
                .ok_or_else(|| CliError::Theme(format!("unknown bundled theme {builtin:?}")))?;
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent).map_err(CliError::ThemeIo)?;
            }
            let mut destination = std::fs::OpenOptions::new();
            destination.write(true);
            if force {
                destination.create(true).truncate(true);
            } else {
                destination.create_new(true);
            }
            let mut file = destination.open(&path).map_err(|error| {
                if !force && error.kind() == std::io::ErrorKind::AlreadyExists {
                    CliError::Theme(format!(
                        "destination already exists: {} (use --force to replace)",
                        path.display()
                    ))
                } else {
                    CliError::ThemeIo(error)
                }
            })?;
            std::io::Write::write_all(&mut file, source.as_bytes()).map_err(CliError::ThemeIo)?;
            if let Some(result) = result {
                return write_json_result(writer, &result);
            }
            writeln!(writer, "{}", path.display())?;
        }
    }
    writer.flush()?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn handle_workflow_command(command: Box<WorkflowCommand>) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    match *command {
        WorkflowCommand::StageRunEdit { file } => {
            let request: bcode_workflow::WorkflowRunGraphEditBatch =
                serde_json::from_value(read_bounded_json(&file)?)?;
            let created = Box::pin(client.stage_workflow_run_graph_edit(request)).await?;
            print_json(
                &serde_json::json!({"staged": true, "created": created, "published": false}),
            )?;
        }
        WorkflowCommand::Author { command } => {
            Box::pin(handle_workflow_author_command(&client, command)).await?;
        }
        WorkflowCommand::Package { command } => {
            Box::pin(handle_workflow_package_command(&client, command)).await?;
        }
        WorkflowCommand::Events {
            run_id,
            after_sequence,
            limit,
        } => {
            print_json(
                &Box::pin(client.workflow_event_history(run_id, after_sequence, limit)).await?,
            )?;
        }
        WorkflowCommand::Attempts {
            run_id,
            after_prepared_at_ms,
            after_dispatch_identity,
            limit,
        } => {
            let cursor = match (after_prepared_at_ms, after_dispatch_identity) {
                (Some(prepared_at_ms), Some(dispatch_identity)) => {
                    Some(bcode_workflow_store::AttemptCursor {
                        prepared_at_ms,
                        dispatch_identity,
                    })
                }
                (None, None) => None,
                _ => {
                    return Err(CliError::InvalidArguments(
                        "attempt cursor requires both timestamp and dispatch identity".to_string(),
                    ));
                }
            };
            print_json(
                &client
                    .workflow_attempt_history(run_id, cursor, limit)
                    .await?,
            )?;
        }
        WorkflowCommand::Definitions { limit } => {
            print_json(&Box::pin(client.list_workflow_definitions(limit)).await?)?;
        }
        WorkflowCommand::DescribeDefinition {
            definition_id,
            version,
        } => {
            print_json(
                &Box::pin(client.describe_workflow_definition(definition_id, version)).await?,
            )?;
        }
        WorkflowCommand::Doctor { run_id, limit } => {
            print_json(&Box::pin(client.doctor_workflow_run(run_id, limit)).await?)?;
        }
        WorkflowCommand::CatalogView { query } => {
            let request =
                serde_json::from_value(read_workflow_input_value(&query)?).map_err(|_| {
                    CliError::InvalidArguments("invalid workflow catalog query".to_owned())
                })?;
            print_json(&Box::pin(client.workflow_catalog_view(request)).await?)?;
        }
        WorkflowCommand::RunView { run_id, limit } => {
            print_json(&Box::pin(client.workflow_run_view(run_id, limit)).await?)?;
        }
        WorkflowCommand::RunStatus { run_id } => {
            print_json(&Box::pin(client.workflow_run_status(run_id)).await?)?;
        }
        WorkflowCommand::Runs { limit } => {
            print_json(&Box::pin(client.list_workflow_runs(limit)).await?)?;
        }
        WorkflowCommand::CancelRun { run_id } => {
            print_json(&Box::pin(client.cancel_workflow_run(run_id)).await?)?;
        }
        WorkflowCommand::PauseRun { run_id } => {
            print_json(&Box::pin(client.pause_workflow_run(run_id)).await?)?;
        }
        WorkflowCommand::ResumeRun { run_id } => {
            print_json(&Box::pin(client.resume_workflow_run(run_id)).await?)?;
        }
        WorkflowCommand::RetryNode {
            run_id,
            node_id,
            activation_id,
            failed_attempt,
        } => {
            print_json(
                &Box::pin(client.retry_workflow_node(
                    run_id,
                    node_id,
                    activation_id,
                    failed_attempt,
                ))
                .await?,
            )?;
        }
        WorkflowCommand::Waits { run_id, limit } => {
            print_json(&Box::pin(client.list_workflow_waits(run_id, limit)).await?)?;
        }
        WorkflowCommand::InspectRun { run_id, limit } => {
            print_json(&Box::pin(client.inspect_workflow_run(run_id, limit)).await?)?;
        }
        WorkflowCommand::RunOutput { run_id, limit } => {
            print_json(&Box::pin(client.workflow_run_outputs(run_id, limit)).await?)?;
        }
        WorkflowCommand::ProvideInput {
            run_id,
            node_id,
            activation_id,
            value,
        } => {
            let value = read_workflow_input_value(&value)?;
            print_json(
                &client
                    .provide_workflow_input(run_id, node_id, activation_id, value)
                    .await?,
            )?;
        }
        WorkflowCommand::ResolveApproval {
            run_id,
            node_id,
            activation_id,
            approve,
            deny,
        } => {
            if approve == deny {
                return Err(CliError::InvalidArguments(
                    "exactly one of --approve or --deny is required".to_string(),
                ));
            }
            print_json(
                &client
                    .resolve_workflow_approval(run_id, node_id, activation_id, approve)
                    .await?,
            )?;
        }
        WorkflowCommand::ResolveMutationApproval {
            approval_id,
            approve,
            deny,
        } => {
            if approve == deny {
                return Err(CliError::InvalidArguments(
                    "exactly one of --approve or --deny is required".to_string(),
                ));
            }
            let decision = if approve {
                bcode_workflow_store::WorkflowMutationApprovalDecision::Approve
            } else {
                bcode_workflow_store::WorkflowMutationApprovalDecision::Deny
            };
            print_json(
                &client
                    .resolve_workflow_mutation_approval(approval_id, decision)
                    .await?,
            )?;
        }
        WorkflowCommand::MutationApprovals { run_id, limit } => {
            let approvals = if let Some(run_id) = run_id {
                client
                    .list_workflow_mutation_approvals(run_id, limit)
                    .await?
            } else {
                client.list_all_workflow_mutation_approvals(limit).await?
            };
            print_json(&approvals)?;
        }
        WorkflowCommand::CancelComputation { operation_id } => {
            print_json(&client.cancel_workflow_computation(operation_id).await?)?;
        }
        WorkflowCommand::ReconcileOrphans { apply, limit, json } => {
            let report = client
                .reconcile_orphaned_workflow_runs(apply, limit)
                .await?;
            if json {
                print_json(&report)?;
            } else {
                print_orphaned_workflow_report(&report);
            }
        }
        WorkflowCommand::MigrateStore => {
            print_json(
                &bcode_workflow_store::WorkflowStore::migrate_to_current_in_state_dir(
                    &bcode_config::default_state_dir(),
                    current_unix_time_ms()?,
                )?,
            )?;
        }
        WorkflowCommand::ResetStore { confirm } => {
            if confirm != "DELETE-INCOMPATIBLE-WORKFLOW-STATE" {
                return Err(CliError::InvalidArguments(
                    "workflow store reset requires --confirm DELETE-INCOMPATIBLE-WORKFLOW-STATE"
                        .to_string(),
                ));
            }
            print_json(&bcode_server::reset_incompatible_workflow_store_offline(
                &bcode_config::default_state_dir(),
                &confirm,
                current_unix_time_ms()?,
            )?)?;
        }
        WorkflowCommand::Start {
            selection,
            parent_session_id,
            run_id,
            workspace_snapshot,
            parent_session_generation,
            configuration,
            input,
        } => {
            let input = input.as_deref().map(read_bounded_json).transpose()?;
            let selection = match selection {
                WorkflowStartSelection::Revision {
                    workflow_id,
                    revision,
                } => bcode_ipc::AuthoredWorkflowRunSelection::Revision {
                    workflow_id,
                    revision,
                },
                WorkflowStartSelection::Active { workflow_id } => {
                    bcode_ipc::AuthoredWorkflowRunSelection::Active { workflow_id }
                }
                WorkflowStartSelection::Preset {
                    workflow_id,
                    preset_id,
                    preset_generation,
                } => bcode_ipc::AuthoredWorkflowRunSelection::Preset {
                    workflow_id,
                    preset_id,
                    preset_generation,
                },
                WorkflowStartSelection::PackageExport {
                    package_id,
                    export,
                    package_lock_digest_sha256,
                } => {
                    let configuration = configuration
                        .as_deref()
                        .map(read_bounded_json)
                        .transpose()?;
                    print_json(
                        &client
                            .start_workflow_package_export(workflow_package_start_request(
                                bcode_workflow::WorkflowPackageExportIdentity {
                                    package_id,
                                    export,
                                    package_lock_digest_sha256,
                                },
                                run_id,
                                parent_session_id,
                                parent_session_generation,
                                workspace_snapshot,
                                configuration,
                                input,
                            ))
                            .await?,
                    )?;
                    return Ok(());
                }
            };
            let configuration = configuration
                .as_deref()
                .map(read_bounded_json)
                .transpose()?;
            print_json(
                &client
                    .start_authored_workflow(bcode_ipc::StartAuthoredWorkflowRequest {
                        selection,
                        run_id,
                        parent_session_id,
                        workspace_snapshot,
                        parent_session_generation,
                        configuration,
                        input,
                    })
                    .await?,
            )?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines, clippy::large_stack_frames)]
fn handle_workflow_author_command(
    client: &BcodeClient,
    command: Box<WorkflowAuthorCommand>,
) -> std::pin::Pin<Box<dyn Future<Output = Result<(), CliError>> + Send + '_>> {
    Box::pin(async move {
        match *command {
            WorkflowAuthorCommand::Create {
                file,
                source_format,
                draft_id,
            } => {
                let document =
                    read_workflow_authoring_document(client, &file, source_format.as_deref())
                        .await?;
                print_json(
                    &client
                        .create_authored_workflow(bcode_ipc::CreateAuthoredWorkflowRequest {
                            document,
                            draft_id,
                        })
                        .await?,
                )?;
            }
            WorkflowAuthorCommand::Apply {
                file,
                source_format,
                draft_id,
            } => {
                let loaded = read_workflow_source_file(&file, source_format.as_deref())?;
                print_json(
                    &client
                        .apply_workflow_source(loaded.source_format, loaded.source, draft_id)
                        .await?,
                )?;
            }
            WorkflowAuthorCommand::List {
                cursor_updated_at_ms,
                cursor_workflow_id,
                limit,
            } => print_json(
                &client
                    .list_authored_workflows(
                        authoring_list_cursor(cursor_updated_at_ms, cursor_workflow_id)?,
                        limit,
                    )
                    .await?,
            )?,
            WorkflowAuthorCommand::Get { workflow_id } => {
                print_json(&client.authored_workflow(workflow_id).await?)?;
            }
            WorkflowAuthorCommand::Inspect { workflow_id, limit } => {
                print_json(&client.inspect_authored_workflow(workflow_id, limit).await?)?;
            }
            WorkflowAuthorCommand::Draft { command } => {
                handle_workflow_draft_command(client, command).await?;
            }
            WorkflowAuthorCommand::Revision { command } => {
                handle_workflow_revision_command(client, command).await?;
            }
            WorkflowAuthorCommand::Update {
                file,
                source_format,
                workflow_id,
                draft_id,
                expected_generation,
            } => {
                let document =
                    read_workflow_authoring_document(client, &file, source_format.as_deref())
                        .await?;
                let producer = document.producer.clone();
                print_json(
                    &client
                        .update_workflow_draft(bcode_ipc::UpdateWorkflowDraftRequest {
                            workflow_id,
                            draft_id,
                            expected_generation,
                            document,
                            producer,
                        })
                        .await?,
                )?;
            }
            WorkflowAuthorCommand::Publish {
                workflow_id,
                draft_id,
                expected_generation,
                configuration,
                activate,
                expected_active_revision,
                operation_id,
                timeout_ms,
            } => {
                let configuration = configuration
                    .as_deref()
                    .map(read_bounded_json)
                    .transpose()?;
                print_json(
                    &client
                        .publish_workflow_draft(bcode_ipc::PublishWorkflowDraftRequest {
                            workflow_id,
                            draft_id,
                            expected_generation,
                            configuration,
                            activate,
                            expected_active_revision,
                            control: workflow_computation_control(operation_id, timeout_ms),
                        })
                        .await?,
                )?;
            }
            WorkflowAuthorCommand::PublishAndStart {
                workflow_id,
                draft_id,
                expected_generation,
                configuration,
                activate,
                expected_active_revision,
                operation_id,
                timeout_ms,
                parent_session_id,
                run_id,
                workspace_snapshot,
            } => {
                let configuration = configuration
                    .as_deref()
                    .map(read_bounded_json)
                    .transpose()?;
                print_json(
                    &Box::pin(client.publish_and_start_workflow(
                        bcode_ipc::PublishAndStartWorkflowRequest {
                            publication: bcode_ipc::PublishWorkflowDraftRequest {
                                workflow_id,
                                draft_id,
                                expected_generation,
                                configuration,
                                activate,
                                expected_active_revision,
                                control: workflow_computation_control(operation_id, timeout_ms),
                            },
                            run_id,
                            parent_session_id,
                            workspace_snapshot,
                        },
                    ))
                    .await?,
                )?;
            }
            WorkflowAuthorCommand::Activate {
                workflow_id,
                revision,
                expected_active_revision,
            } => print_json(
                &client
                    .activate_workflow_revision(bcode_ipc::ActivateWorkflowRevisionRequest {
                        workflow_id,
                        revision,
                        expected_active_revision,
                    })
                    .await?,
            )?,
            WorkflowAuthorCommand::Archive {
                workflow_id,
                archived,
            } => print_json(
                &client
                    .set_authored_workflow_archived(bcode_ipc::SetAuthoredWorkflowArchivedRequest {
                        workflow_id,
                        archived,
                    })
                    .await?,
            )?,
            WorkflowAuthorCommand::Discard {
                workflow_id,
                draft_id,
                expected_generation,
            } => print_json(
                &client
                    .discard_workflow_draft(bcode_ipc::DiscardWorkflowDraftRequest {
                        workflow_id,
                        draft_id,
                        expected_generation,
                    })
                    .await?,
            )?,
            WorkflowAuthorCommand::Fork {
                workflow_id,
                draft_id,
                source_draft,
                source_revision,
            } => {
                let source = match (source_draft, source_revision) {
                    (Some(draft_id), None) => {
                        bcode_ipc::WorkflowDraftForkSource::Draft { draft_id }
                    }
                    (None, Some(revision)) => {
                        bcode_ipc::WorkflowDraftForkSource::Revision { revision }
                    }
                    _ => {
                        return Err(CliError::InvalidArguments(
                            "exactly one of --source-draft or --source-revision is required"
                                .to_string(),
                        ));
                    }
                };
                print_json(
                    &client
                        .fork_workflow_draft(bcode_ipc::ForkWorkflowDraftRequest {
                            workflow_id,
                            source,
                            draft_id,
                            producer: cli_workflow_producer(),
                        })
                        .await?,
                )?;
            }
            WorkflowAuthorCommand::Preset { command } => {
                handle_workflow_preset_command(client, command).await?;
            }
            WorkflowAuthorCommand::Export {
                workflow_id,
                revision,
            } => print_json(
                &client
                    .export_workflow_revision(bcode_ipc::ExportWorkflowRevisionRequest {
                        workflow_id,
                        revision,
                    })
                    .await?,
            )?,
            WorkflowAuthorCommand::ImportPreview {
                file,
                target_workflow_id,
                operation_id,
                timeout_ms,
            } => {
                let bundle = serde_json::from_value(read_bounded_json(&file)?)?;
                print_json(
                    &client
                        .preview_workflow_import(bcode_ipc::PreviewWorkflowImportRequest {
                            bundle,
                            target_workflow_id,
                            control: workflow_computation_control(operation_id, timeout_ms),
                        })
                        .await?,
                )?;
            }
            WorkflowAuthorCommand::Import {
                file,
                target_workflow_id,
                draft_id,
                operation_id,
                timeout_ms,
            } => {
                let bundle = serde_json::from_value(read_bounded_json(&file)?)?;
                print_json(
                    &client
                        .import_workflow(bcode_ipc::ImportWorkflowRequest {
                            bundle,
                            target_workflow_id,
                            draft_id,
                            collision_policy:
                                bcode_ipc::WorkflowImportCollisionPolicy::RequireNewWorkflow,
                            control: workflow_computation_control(operation_id, timeout_ms),
                        })
                        .await?,
                )?;
            }
            WorkflowAuthorCommand::ImportDraft {
                file,
                workflow_id,
                draft_id,
                operation_id,
                timeout_ms,
            } => {
                handle_workflow_import_draft_command(
                    client,
                    file,
                    workflow_id,
                    draft_id,
                    operation_id,
                    timeout_ms,
                )
                .await?;
            }
            WorkflowAuthorCommand::ImportRevision {
                file,
                workflow_id,
                revision,
                activate,
                expected_active_revision,
                operation_id,
                timeout_ms,
            } => {
                let bundle = serde_json::from_value(read_bounded_json(&file)?)?;
                print_json(
                    &client
                        .import_workflow_revision(bcode_ipc::ImportWorkflowRevisionRequest {
                            bundle,
                            workflow_id,
                            revision,
                            activate,
                            expected_active_revision,
                            collision_policy: bcode_ipc::WorkflowImportCollisionPolicy::RequireExistingWorkflowNextRevision,
                            control: workflow_computation_control(operation_id, timeout_ms),
                        })
                        .await?,
                )?;
            }
            WorkflowAuthorCommand::Catalog => {
                print_json(&client.workflow_authoring_catalog().await?)?;
            }
            WorkflowAuthorCommand::Validate {
                file,
                source_format,
                operation_id,
                timeout_ms,
            } => {
                let loaded = read_workflow_source_file(&file, source_format.as_deref())?;
                print_json(
                    &client
                        .validate_workflow_source(bcode_ipc::WorkflowSourceComputationRequest {
                            source_format: loaded.source_format,
                            source: loaded.source,
                            control: workflow_computation_control(operation_id, timeout_ms),
                        })
                        .await?,
                )?;
            }
            WorkflowAuthorCommand::Preview {
                file,
                source_format,
                configuration,
                operation_id,
                timeout_ms,
            } => {
                if file == Path::new("-") && configuration.as_deref() == Some(Path::new("-")) {
                    return Err(CliError::InvalidArguments(
                        "workflow document and configuration cannot both read from stdin"
                            .to_string(),
                    ));
                }
                let loaded = read_workflow_source_file(&file, source_format.as_deref())?;
                let configuration = configuration
                    .as_deref()
                    .map(read_bounded_json)
                    .transpose()?;
                print_json(
                    &client
                        .preview_workflow_source(bcode_ipc::WorkflowSourcePreviewRequest {
                            source_format: loaded.source_format,
                            source: loaded.source,
                            configuration,
                            control: workflow_computation_control(operation_id, timeout_ms),
                        })
                        .await?,
                )?;
            }
        }
        Ok(())
    })
}

fn handle_workflow_import_draft_command(
    client: &BcodeClient,
    file: PathBuf,
    workflow_id: String,
    draft_id: String,
    operation_id: Option<String>,
    timeout_ms: u64,
) -> std::pin::Pin<Box<dyn Future<Output = Result<(), CliError>> + Send + '_>> {
    Box::pin(async move {
        let bundle = serde_json::from_value(read_bounded_json(&file)?)?;
        print_json(
            &client
                .import_workflow_draft(bcode_ipc::ImportWorkflowDraftRequest {
                    bundle,
                    workflow_id,
                    draft_id,
                    collision_policy:
                        bcode_ipc::WorkflowImportCollisionPolicy::RequireExistingWorkflowNewDraft,
                    control: workflow_computation_control(operation_id, timeout_ms),
                })
                .await?,
        )?;
        Ok(())
    })
}

async fn handle_workflow_draft_command(
    client: &BcodeClient,
    command: WorkflowDraftCommand,
) -> Result<(), CliError> {
    match command {
        WorkflowDraftCommand::List {
            workflow_id,
            cursor_updated_at_ms,
            cursor_draft_id,
            limit,
        } => print_json(
            &client
                .list_workflow_drafts(
                    workflow_id,
                    authoring_list_cursor(cursor_updated_at_ms, cursor_draft_id)?,
                    limit,
                )
                .await?,
        )?,
        WorkflowDraftCommand::Get {
            workflow_id,
            draft_id,
        } => print_json(&client.workflow_draft(workflow_id, draft_id).await?)?,
    }
    Ok(())
}

async fn handle_workflow_revision_command(
    client: &BcodeClient,
    command: WorkflowRevisionCommand,
) -> Result<(), CliError> {
    match command {
        WorkflowRevisionCommand::List {
            workflow_id,
            before_revision,
            limit,
        } => print_json(
            &client
                .list_workflow_revisions(
                    workflow_id,
                    before_revision
                        .map(|revision| bcode_workflow::WorkflowRevisionListCursor { revision }),
                    limit,
                )
                .await?,
        )?,
        WorkflowRevisionCommand::Inspect {
            workflow_id,
            revision,
        } => print_json(
            &client
                .workflow_revision_requirement_inspection(workflow_id, revision)
                .await?,
        )?,
        WorkflowRevisionCommand::Get {
            workflow_id,
            revision,
        } => print_json(&client.workflow_revision(workflow_id, revision).await?)?,
    }
    Ok(())
}

fn authoring_list_cursor(
    updated_at_ms: Option<u64>,
    entity_id: Option<String>,
) -> Result<Option<bcode_workflow::WorkflowAuthoringListCursor>, CliError> {
    match (updated_at_ms, entity_id) {
        (None, None) => Ok(None),
        (Some(updated_at_ms), Some(entity_id)) => {
            Ok(Some(bcode_workflow::WorkflowAuthoringListCursor {
                updated_at_ms,
                entity_id,
            }))
        }
        _ => Err(CliError::InvalidArguments(
            "both cursor timestamp and cursor identity are required".to_string(),
        )),
    }
}

fn workflow_computation_control(
    operation_id: Option<String>,
    timeout_ms: u64,
) -> bcode_ipc::WorkflowComputationControl {
    bcode_ipc::WorkflowComputationControl {
        operation_id: operation_id.unwrap_or_default(),
        timeout_ms,
    }
}

fn cli_workflow_producer() -> bcode_workflow::WorkflowProducerProvenance {
    bcode_workflow::WorkflowProducerProvenance {
        kind: bcode_workflow::WorkflowProducerKind::Cli,
        producer_id: Some("bcode-cli".to_string()),
        source_revision: None,
    }
}

async fn handle_workflow_preset_command(
    client: &BcodeClient,
    command: WorkflowPresetCommand,
) -> Result<(), CliError> {
    match command {
        WorkflowPresetCommand::List {
            workflow_id,
            cursor_updated_at_ms,
            cursor_preset_id,
            limit,
        } => print_json(
            &client
                .list_workflow_presets(
                    workflow_id,
                    authoring_list_cursor(cursor_updated_at_ms, cursor_preset_id)?,
                    limit,
                )
                .await?,
        )?,
        WorkflowPresetCommand::Get {
            workflow_id,
            preset_id,
        } => print_json(&client.workflow_preset(workflow_id, preset_id).await?)?,
        WorkflowPresetCommand::Create { file } => {
            let preset = serde_json::from_value(read_bounded_json(&file)?)?;
            print_json(
                &client
                    .create_workflow_preset(bcode_ipc::CreateWorkflowPresetRequest { preset })
                    .await?,
            )?;
        }
        WorkflowPresetCommand::Update {
            file,
            expected_generation,
        } => {
            let preset = serde_json::from_value(read_bounded_json(&file)?)?;
            print_json(
                &client
                    .update_workflow_preset(bcode_ipc::UpdateWorkflowPresetRequest {
                        expected_generation,
                        preset,
                    })
                    .await?,
            )?;
        }
        WorkflowPresetCommand::Delete {
            workflow_id,
            preset_id,
            expected_generation,
        } => print_json(
            &client
                .delete_workflow_preset(bcode_ipc::DeleteWorkflowPresetRequest {
                    workflow_id,
                    preset_id,
                    expected_generation,
                })
                .await?,
        )?,
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CliWorkflowPackageImport {
    import_id: String,
    package_id: String,
    export: String,
    #[serde(default)]
    manifest: Option<String>,
    #[serde(default)]
    target: Option<bcode_workflow::WorkflowCallTarget>,
    #[serde(default)]
    package_lock_digest_sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CliWorkflowPackageManifest {
    version: u32,
    package_id: String,
    exports: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    external_dependencies: std::collections::BTreeMap<String, bcode_workflow::WorkflowCallTarget>,
    #[serde(default)]
    imports: Vec<CliWorkflowPackageImport>,
    members: Vec<CliWorkflowPackageMember>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CliWorkflowPackageMember {
    member_id: String,
    source_name: String,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    external_dependencies: Vec<String>,
}

const fn workflow_package_start_request(
    package_export: bcode_workflow::WorkflowPackageExportIdentity,
    run_id: Option<String>,
    parent_session_id: SessionId,
    parent_session_generation: Option<u64>,
    workspace_snapshot: Option<String>,
    configuration: Option<serde_json::Value>,
    input: Option<serde_json::Value>,
) -> bcode_ipc::StartWorkflowPackageExportRequest {
    bcode_ipc::StartWorkflowPackageExportRequest {
        package_export,
        run_id,
        parent_session_id,
        workspace_snapshot,
        parent_session_generation,
        configuration,
        input,
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_workflow_package_command(
    client: &BcodeClient,
    command: WorkflowPackageCommand,
) -> Result<(), CliError> {
    if let WorkflowPackageCommand::Discover { workspace, limit } = command {
        let workspace = workspace.map_or_else(std::env::current_dir, Ok)?;
        let page = client
            .workflow_launch_catalog(bcode_workflow::WorkflowLaunchCatalogRequest {
                version: bcode_workflow::WORKFLOW_LAUNCH_CATALOG_VERSION,
                workspace,
                limit,
                cursor: None,
                search: None,
                source_kind: None,
                readiness: None,
            })
            .await?;
        print_json(&page)?;
        return Ok(());
    }
    if let WorkflowPackageCommand::Publish {
        lock,
        expected_generations,
    } = command
    {
        let expected_lock: bcode_workflow::WorkflowPackageLock =
            serde_json::from_value(read_bounded_json(&lock)?)?;
        let request = bcode_workflow::WorkflowPackagePublishRequest {
            version: bcode_workflow::WORKFLOW_PACKAGE_MUTATION_VERSION,
            package_id: expected_lock.package_id.clone(),
            expected_lock,
            expected_generations: parse_package_expected_generations(&expected_generations)?,
        };
        request
            .validate()
            .map_err(|error| CliError::InvalidArguments(error.to_string()))?;
        print_json(
            &client
                .publish_workflow_package(bcode_ipc::PublishWorkflowPackageRequest {
                    request,
                    published_at_ms: current_unix_time_ms()?,
                })
                .await?,
        )?;
        return Ok(());
    }
    let (manifest_path, operation_id, timeout_ms, operation) = match command {
        WorkflowPackageCommand::Validate {
            manifest,
            operation_id,
            timeout_ms,
        } => (
            manifest,
            operation_id,
            timeout_ms,
            PackageCliOperation::Validate,
        ),
        WorkflowPackageCommand::Preview {
            manifest,
            operation_id,
            timeout_ms,
        } => (
            manifest,
            operation_id,
            timeout_ms,
            PackageCliOperation::Preview,
        ),
        WorkflowPackageCommand::Apply {
            manifest,
            expected_generations,
            operation_id,
            timeout_ms,
        } => (
            manifest,
            operation_id,
            timeout_ms,
            PackageCliOperation::Apply(parse_package_expected_generations(&expected_generations)?),
        ),
        WorkflowPackageCommand::Publish { .. } | WorkflowPackageCommand::Discover { .. } => {
            unreachable!("handled above")
        }
    };
    let closure = read_workflow_package_closure(&manifest_path)?;
    let result = client
        .validate_workflow_package(bcode_ipc::WorkflowPackageComputationRequest {
            closure,
            control: workflow_computation_control(operation_id.clone(), timeout_ms),
        })
        .await?;
    match operation {
        PackageCliOperation::Validate => print_json(&result)?,
        PackageCliOperation::Preview => {
            let entry_index = result
                .plan
                .packages
                .iter()
                .position(|package| package.package_id == result.plan.entry_package_id)
                .ok_or_else(|| {
                    CliError::InvalidArguments(
                        "planned package closure has no entry package".to_string(),
                    )
                })?;
            let entry_plan = result.plan.packages[entry_index].plan.clone();
            let dependency_plans = result.plan.packages[..entry_index]
                .iter()
                .map(|package| package.plan.clone())
                .collect();
            print_json(
                &client
                    .preview_workflow_package(bcode_ipc::WorkflowPackagePreviewRequest {
                        plan: entry_plan,
                        dependency_plans,
                        configurations: std::collections::BTreeMap::new(),
                        control: workflow_computation_control(operation_id, timeout_ms),
                    })
                    .await?,
            )?;
        }
        PackageCliOperation::Apply(expected_generations) => {
            let mut applied = Vec::with_capacity(result.plan.packages.len());
            for package in &result.plan.packages {
                let package_expected_generations =
                    if package.package_id == result.plan.entry_package_id {
                        expected_generations.clone()
                    } else {
                        Vec::new()
                    };
                applied.push(
                    client
                        .apply_workflow_package(bcode_ipc::ApplyWorkflowPackageRequest {
                            request: bcode_workflow::WorkflowPackageApplyRequest {
                                version: bcode_workflow::WORKFLOW_PACKAGE_MUTATION_VERSION,
                                plan: package.plan.clone(),
                                expected_generations: package_expected_generations,
                            },
                            applied_at_ms: current_unix_time_ms()?,
                        })
                        .await?,
                );
            }
            print_json(&applied)?;
        }
    }
    Ok(())
}

#[derive(Debug)]
enum PackageCliOperation {
    Validate,
    Preview,
    Apply(Vec<bcode_workflow::WorkflowPackageExpectedGeneration>),
}

fn current_unix_time_ms() -> Result<u64, CliError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            CliError::InvalidArguments(format!("system clock precedes Unix epoch: {error}"))
        })?
        .as_millis()
        .try_into()
        .map_err(|_| CliError::InvalidArguments("current timestamp exceeds u64".to_string()))
}

fn parse_package_expected_generations(
    facts: &[String],
) -> Result<Vec<bcode_workflow::WorkflowPackageExpectedGeneration>, CliError> {
    let mut parsed = facts
        .iter()
        .map(|fact| {
            let (member_id, generation) = fact.split_once('=').ok_or_else(|| {
                CliError::InvalidArguments(format!(
                    "expected generation '{fact}' must use MEMBER_ID=GENERATION"
                ))
            })?;
            let expected_generation = generation.parse::<u64>().map_err(|error| {
                CliError::InvalidArguments(format!(
                    "expected generation '{fact}' is invalid: {error}"
                ))
            })?;
            if member_id.is_empty() || expected_generation == 0 {
                return Err(CliError::InvalidArguments(format!(
                    "expected generation '{fact}' requires a member and nonzero generation"
                )));
            }
            Ok(bcode_workflow::WorkflowPackageExpectedGeneration {
                member_id: member_id.to_string(),
                expected_generation,
            })
        })
        .collect::<Result<Vec<_>, CliError>>()?;
    parsed.sort_by(|left, right| left.member_id.cmp(&right.member_id));
    if parsed
        .windows(2)
        .any(|pair| pair[0].member_id == pair[1].member_id)
    {
        return Err(CliError::InvalidArguments(
            "expected generations must be unique by member".to_string(),
        ));
    }
    Ok(parsed)
}

#[cfg(test)]
fn read_workflow_package_manifest(
    path: &Path,
) -> Result<bcode_workflow::WorkflowPackageManifest, CliError> {
    read_workflow_package_manifest_with_budget(
        path,
        bcode_workflow::MAX_WORKFLOW_PACKAGE_SOURCE_BYTES,
    )
}

#[allow(clippy::too_many_lines)]
fn read_workflow_package_manifest_with_budget(
    path: &Path,
    source_budget: usize,
) -> Result<bcode_workflow::WorkflowPackageManifest, CliError> {
    if path == Path::new("-") {
        return Err(CliError::InvalidArguments(
            "workflow package manifests must be local files so member paths can be confined"
                .to_string(),
        ));
    }
    let manifest_path = fs::canonicalize(path)?;
    let package_root = manifest_path.parent().ok_or_else(|| {
        CliError::InvalidArguments("workflow package manifest has no parent directory".to_string())
    })?;
    let manifest_bytes = read_bytes_with_limit(
        fs::File::open(&manifest_path)?,
        bcode_workflow::MAX_WORKFLOW_PACKAGE_SOURCE_BYTES,
        "workflow package manifest",
    )?;
    let manifest_source = String::from_utf8(manifest_bytes).map_err(|_| {
        CliError::InvalidArguments("workflow package manifest is not valid UTF-8".to_string())
    })?;
    let file_name = manifest_path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| {
            CliError::InvalidArguments("workflow package manifest name is not UTF-8".to_string())
        })?;
    let decoded: CliWorkflowPackageManifest =
        match bcode_workflow::WorkflowSourceFormat::from_file_name(file_name)
            .map_err(|error| CliError::InvalidArguments(error.to_string()))?
        {
            bcode_workflow::WorkflowSourceFormat::Json => serde_json::from_str(&manifest_source)
                .map_err(|_| {
                    CliError::InvalidArguments("invalid workflow package JSON manifest".to_string())
                })?,
            bcode_workflow::WorkflowSourceFormat::Yaml => yaml_serde::from_str(&manifest_source)
                .map_err(|_| {
                    CliError::InvalidArguments("invalid workflow package YAML manifest".to_string())
                })?,
            bcode_workflow::WorkflowSourceFormat::Toml => toml::from_str(&manifest_source)
                .map_err(|_| {
                    CliError::InvalidArguments("invalid workflow package TOML manifest".to_string())
                })?,
        };
    let mut manifest = bcode_workflow::WorkflowPackageManifest {
        version: decoded.version,
        package_id: decoded.package_id,
        exports: decoded.exports,
        external_dependencies: decoded.external_dependencies,
        imports: decoded
            .imports
            .into_iter()
            .map(|import| bcode_workflow::WorkflowPackageImport {
                import_id: import.import_id,
                package_id: import.package_id,
                export: import.export,
                manifest: import.manifest,
                target: import.target,
                package_lock_digest_sha256: import.package_lock_digest_sha256,
            })
            .collect(),
        members: decoded
            .members
            .into_iter()
            .map(|member| bcode_workflow::WorkflowPackageMember {
                member_id: member.member_id,
                source_name: member.source_name,
                format: bcode_workflow::WorkflowSourceFormat::Json,
                source: String::new(),
                dependencies: member.dependencies,
                external_dependencies: member.external_dependencies,
            })
            .collect(),
    };
    let mut total_bytes = 0_usize;
    for member in &mut manifest.members {
        let relative = Path::new(&member.source_name);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(CliError::InvalidArguments(format!(
                "workflow package member path '{}' is not confined",
                member.source_name
            )));
        }
        let member_path = fs::canonicalize(package_root.join(relative))?;
        if !member_path.starts_with(package_root) || !member_path.is_file() {
            return Err(CliError::InvalidArguments(format!(
                "workflow package member '{}' escapes the package root or is not a file",
                member.source_name
            )));
        }
        let bytes = read_bytes_with_limit(
            fs::File::open(&member_path)?,
            source_budget - total_bytes,
            "workflow package sources",
        )?;
        total_bytes += bytes.len();
        let source = String::from_utf8(bytes).map_err(|_| {
            CliError::InvalidArguments("workflow package source is not valid UTF-8".to_string())
        })?;
        member.format = bcode_workflow::WorkflowSourceFormat::from_file_name(&member.source_name)
            .map_err(|error| CliError::InvalidArguments(error.to_string()))?;
        member.source = source;
    }
    manifest
        .validate()
        .map_err(|error| CliError::InvalidArguments(error.to_string()))?;
    Ok(manifest)
}

fn read_workflow_package_closure(
    path: &Path,
) -> Result<bcode_workflow::WorkflowPackageClosure, CliError> {
    read_workflow_package_closure_in_root(path, None)
}

fn read_workflow_package_closure_in_root(
    path: &Path,
    authorized_root: Option<&Path>,
) -> Result<bcode_workflow::WorkflowPackageClosure, CliError> {
    if path == Path::new("-") {
        return Err(CliError::InvalidArguments(
            "workflow package manifests must be local files so import paths can be confined"
                .to_string(),
        ));
    }
    let entry_path = fs::canonicalize(path)?;
    let entry_parent = entry_path.parent().ok_or_else(|| {
        CliError::InvalidArguments("workflow package manifest has no parent directory".to_string())
    })?;
    let authorized_root =
        authorized_root.map_or_else(|| Ok(entry_parent.to_path_buf()), fs::canonicalize)?;
    if !entry_path.starts_with(&authorized_root) {
        return Err(CliError::InvalidArguments(
            "workflow package entry escapes the authorized package root".to_string(),
        ));
    }
    let mut pending = vec![(entry_path.clone(), 1_usize)];
    let mut visited = std::collections::BTreeSet::new();
    let mut packages = Vec::new();
    let mut source_budget = bcode_workflow::MAX_WORKFLOW_PACKAGE_SOURCE_BYTES;
    while let Some((manifest_path, depth)) = pending.pop() {
        if depth > bcode_workflow::MAX_WORKFLOW_PACKAGE_DEPTH {
            return Err(CliError::InvalidArguments(
                "workflow package import depth exceeds the package bound".to_string(),
            ));
        }
        if !visited.insert(manifest_path.clone()) {
            continue;
        }
        if packages.len() >= bcode_workflow::MAX_WORKFLOW_PACKAGE_CLOSURE_PACKAGES {
            return Err(CliError::InvalidArguments(
                "workflow package closure exceeds the package-count bound".to_string(),
            ));
        }
        let manifest = read_workflow_package_manifest_with_budget(&manifest_path, source_budget)?;
        for member in &manifest.members {
            source_budget -= member.source.len();
        }
        let package_root = manifest_path.parent().ok_or_else(|| {
            CliError::InvalidArguments(
                "workflow package manifest has no parent directory".to_string(),
            )
        })?;
        let mut import_paths = Vec::new();
        for import in &manifest.imports {
            if let Some(relative) = &import.manifest {
                let import_path = fs::canonicalize(package_root.join(relative))?;
                if !import_path.starts_with(&authorized_root) || !import_path.is_file() {
                    return Err(CliError::InvalidArguments(format!(
                        "workflow package import '{relative}' escapes the authorized package root or is not a file"
                    )));
                }
                import_paths.push(import_path);
            }
        }
        import_paths.sort();
        pending.extend(
            import_paths
                .into_iter()
                .rev()
                .map(|import_path| (import_path, depth + 1)),
        );
        let source_name = manifest_path
            .strip_prefix(&authorized_root)
            .map_err(|_| {
                CliError::InvalidArguments(
                    "workflow package manifest escapes the authorized package root".to_string(),
                )
            })?
            .to_string_lossy()
            .into_owned();
        packages.push(bcode_workflow::WorkflowPackageClosureSource {
            package_id: manifest.package_id.clone(),
            source_name: Some(source_name),
            manifest,
        });
    }
    let entry_package_id = packages
        .first()
        .ok_or_else(|| CliError::InvalidArguments("empty workflow package closure".to_string()))?
        .package_id
        .clone();
    Ok(bcode_workflow::WorkflowPackageClosure {
        version: bcode_workflow::WORKFLOW_PACKAGE_CLOSURE_VERSION,
        entry_package_id,
        packages,
    })
}

struct WorkflowSourceFile {
    source_format: bcode_workflow::WorkflowSourceFormat,
    source: String,
}

fn read_workflow_source_file(
    path: &Path,
    explicit_format: Option<&str>,
) -> Result<WorkflowSourceFile, CliError> {
    if path == Path::new("-") {
        return Err(CliError::InvalidArguments(
            "workflow source apply from stdin is not yet supported".to_string(),
        ));
    }
    let bytes = read_bytes_with_limit(
        fs::File::open(path)?,
        bcode_workflow::MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES,
        "workflow source",
    )?;
    let source = String::from_utf8(bytes).map_err(|_| {
        CliError::InvalidArguments("workflow source is not valid UTF-8".to_string())
    })?;
    let source_format = match explicit_format {
        Some("json") => bcode_workflow::WorkflowSourceFormat::Json,
        Some("yaml" | "yml") => bcode_workflow::WorkflowSourceFormat::Yaml,
        Some("toml") => bcode_workflow::WorkflowSourceFormat::Toml,
        Some(format) => {
            return Err(CliError::InvalidArguments(format!(
                "unsupported workflow source format '{format}'"
            )));
        }
        None => bcode_workflow::WorkflowSourceFormat::from_file_name(
            path.file_name()
                .and_then(std::ffi::OsStr::to_str)
                .ok_or_else(|| {
                    CliError::InvalidArguments(
                        "workflow source file name is not valid UTF-8".to_string(),
                    )
                })?,
        )
        .map_err(|error| CliError::InvalidArguments(error.to_string()))?,
    };
    Ok(WorkflowSourceFile {
        source_format,
        source,
    })
}

struct LoadedWorkflowSource {
    lowering: bcode_workflow::WorkflowSourceLoweringResult,
}

async fn read_workflow_source_lowering(
    client: &BcodeClient,
    path: &Path,
    explicit_format: Option<&str>,
) -> Result<LoadedWorkflowSource, CliError> {
    let max_bytes = bcode_workflow::MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES;
    let bytes = if path == Path::new("-") {
        read_bytes_with_limit(std::io::stdin().lock(), max_bytes, "workflow source")?
    } else {
        read_bytes_with_limit(fs::File::open(path)?, max_bytes, "workflow source")?
    };
    let source = std::str::from_utf8(&bytes).map_err(|error| {
        CliError::InvalidArguments(format!("workflow source is not valid UTF-8: {error}"))
    })?;
    let format = match explicit_format {
        Some("json") => bcode_workflow::WorkflowSourceFormat::Json,
        Some("yaml" | "yml") => bcode_workflow::WorkflowSourceFormat::Yaml,
        Some("toml") => bcode_workflow::WorkflowSourceFormat::Toml,
        Some(format) => {
            return Err(CliError::InvalidArguments(format!(
                "unsupported workflow source format '{format}'"
            )));
        }
        None if path == Path::new("-") => {
            return Err(CliError::InvalidArguments(
                "workflow source from stdin requires --source-format json|yaml|toml".to_string(),
            ));
        }
        None => bcode_workflow::WorkflowSourceFormat::from_file_name(
            path.file_name()
                .and_then(std::ffi::OsStr::to_str)
                .ok_or_else(|| {
                    CliError::InvalidArguments(
                        "workflow source file name is not valid UTF-8".to_string(),
                    )
                })?,
        )
        .map_err(|error| CliError::InvalidArguments(error.to_string()))?,
    };
    let catalog = client.workflow_authoring_catalog().await?;
    let lowering = bcode_workflow::lower_workflow_authoring_source(source, format, &catalog)
        .map_err(|error| CliError::InvalidArguments(error.to_string()))?;
    Ok(LoadedWorkflowSource { lowering })
}

async fn read_workflow_authoring_document(
    client: &BcodeClient,
    path: &Path,
    explicit_format: Option<&str>,
) -> Result<bcode_workflow::WorkflowAuthoringDocument, CliError> {
    read_workflow_source_lowering(client, path, explicit_format)
        .await
        .map(|loaded| loaded.lowering.document)
}

const MAX_CLI_INTERACTION_JSON_BYTES: usize = 256 * 1024;

fn read_workflow_input_value(value: &str) -> Result<serde_json::Value, CliError> {
    let path = Path::new(value);
    if value == "-" || path.is_file() {
        read_bounded_json(path)
    } else {
        read_json_from_reader(
            value.as_bytes(),
            bcode_workflow::MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES,
            "workflow JSON",
        )
    }
}

fn read_bounded_json(path: &Path) -> Result<serde_json::Value, CliError> {
    read_json_with_limit(
        path,
        bcode_workflow::MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES,
        "workflow JSON",
    )
}

fn read_bounded_interaction_json(path: &Path) -> Result<serde_json::Value, CliError> {
    read_json_with_limit(path, MAX_CLI_INTERACTION_JSON_BYTES, "interaction JSON")
}

fn read_json_with_limit(
    path: &Path,
    max_bytes: usize,
    description: &str,
) -> Result<serde_json::Value, CliError> {
    if path == Path::new("-") {
        read_json_from_reader(std::io::stdin().lock(), max_bytes, description)
    } else {
        read_json_from_reader(fs::File::open(path)?, max_bytes, description)
    }
}

fn read_json_from_reader(
    reader: impl std::io::Read,
    max_bytes: usize,
    description: &str,
) -> Result<serde_json::Value, CliError> {
    let bytes = read_bytes_with_limit(reader, max_bytes, description)?;
    serde_json::from_slice(&bytes).map_err(CliError::Json)
}

fn read_bytes_with_limit(
    reader: impl std::io::Read,
    max_bytes: usize,
    description: &str,
) -> Result<Vec<u8>, CliError> {
    // Read one sentinel byte beyond the limit to distinguish an exact-size input
    // from an oversized one. Bound the read itself, not mutable file metadata.
    let read_limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Vec::new();
    reader.take(read_limit).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(CliError::InvalidArguments(format!(
            "{description} exceeds {max_bytes} bytes"
        )));
    }
    Ok(bytes)
}

fn print_json<T: Serialize>(value: &T) -> Result<(), CliError> {
    write_json_result(&mut std::io::stdout().lock(), value)
}

fn write_json_result<W: std::io::Write, T: Serialize>(
    output: &mut W,
    value: &T,
) -> Result<(), CliError> {
    let result = serde_json::to_string_pretty(value)?;
    writeln!(output, "{result}")?;
    output.flush()?;
    Ok(())
}

fn print_json_line<T: Serialize>(value: &T) -> Result<(), CliError> {
    write_json_stream_record(&mut std::io::stdout().lock(), value)
}

fn write_json_stream_record<W: std::io::Write, T: Serialize>(
    output: &mut W,
    value: &T,
) -> Result<(), CliError> {
    // Serialize before writing so a serialization error cannot leave a partial record.
    let record = serde_json::to_string(value)?;
    writeln!(output, "{record}")?;
    // Live consumers must receive each record without waiting for the buffer to fill.
    output.flush()?;
    Ok(())
}

async fn handle_worktree_command(command: WorktreeCommand) -> Result<(), CliError> {
    match command {
        WorktreeCommand::List { cwd, json } => {
            ensure_server_running().await?;
            let client = BcodeClient::default_endpoint();
            let response = client
                .list_worktrees(bcode_worktree_models::WorktreeListRequest { cwd })
                .await?;
            if json {
                print_json(&response)?;
            } else {
                write_worktree_list(&mut std::io::stdout().lock(), &response.worktrees)?;
            }
        }
        WorktreeCommand::Create {
            name,
            cwd,
            path,
            branch,
            new_branch,
            detach,
            force,
            attach_session_id,
            new_session,
            no_setup,
            json,
        } => {
            ensure_server_running().await?;
            let client = BcodeClient::default_endpoint();
            let response = client
                .create_worktree(bcode_worktree_models::WorktreeCreateRequest {
                    name,
                    cwd,
                    path,
                    branch,
                    new_branch,
                    base_ref: None,
                    detach,
                    force,
                    attach_session_id,
                    new_session,
                    no_setup,
                })
                .await?;
            if json {
                print_json(&response)?;
            } else {
                write_worktree_path(&mut std::io::stdout().lock(), &response.path)?;
            }
        }
        WorktreeCommand::Remove {
            path,
            cwd,
            force,
            yes,
            json,
        } => {
            if !yes {
                return Err(CliError::InvalidArguments(
                    "worktree removal requires --yes".to_owned(),
                ));
            }
            ensure_server_running().await?;
            let client = BcodeClient::default_endpoint();
            let response = client
                .remove_worktree(bcode_worktree_models::WorktreeRemoveRequest { cwd, path, force })
                .await?;
            if json {
                print_json(&response)?;
            } else {
                write_worktree_path(&mut std::io::stdout().lock(), &response.path)?;
            }
        }
    }
    Ok(())
}

fn write_worktree_list<W: std::io::Write>(
    output: &mut W,
    worktrees: &[bcode_worktree_models::WorktreeInfo],
) -> Result<(), CliError> {
    for worktree in worktrees {
        writeln!(
            output,
            "{}\t{}\t{}\t{}",
            worktree.path.display(),
            worktree.branch.as_deref().unwrap_or("-"),
            worktree.commit.as_deref().unwrap_or("-"),
            if worktree.is_main { "main" } else { "linked" },
        )?;
    }
    output.flush()?;
    Ok(())
}

fn write_worktree_path<W: std::io::Write>(output: &mut W, path: &Path) -> Result<(), CliError> {
    writeln!(output, "{}", path.display())?;
    output.flush()?;
    Ok(())
}

fn filter_session_exchanges(
    exchanges: &mut Vec<bcode_session_models::PendingToolExchangeSummary>,
    session_id: Option<SessionId>,
) {
    if let Some(session_id) = session_id {
        exchanges.retain(|exchange| exchange.session_id == session_id);
    }
}

async fn handle_interaction_command(command: InteractionCommand) -> Result<(), CliError> {
    match command {
        InteractionCommand::Input {
            session_id,
            payload,
            json,
        } => {
            let input = read_invocation_input(&payload)?;
            ensure_server_running().await?;
            BcodeClient::default_endpoint()
                .send_invocation_input(session_id, input)
                .await?;
            write_invocation_input_receipt(&mut std::io::stdout().lock(), json)?;
        }
        InteractionCommand::Inspect { exchange_id } => {
            ensure_server_running().await?;
            let exchange =
                find_pending_exchange(&BcodeClient::default_endpoint(), &exchange_id).await?;
            print_json(&exchange)?;
        }
        InteractionCommand::List { session_id, json } => {
            ensure_server_running().await?;
            let client = BcodeClient::default_endpoint();
            let mut exchanges = client.list_pending_tool_exchanges().await?;
            filter_session_exchanges(&mut exchanges, session_id);
            write_interaction_list(&mut std::io::stdout().lock(), &exchanges, json)?;
        }
        InteractionCommand::Respond {
            exchange_id,
            payload,
            json,
        } => {
            let payload = read_bounded_interaction_json(&payload)?;
            ensure_server_running().await?;
            let client = BcodeClient::default_endpoint();
            let client = compatible_interaction_client(&client, &exchange_id).await?;
            let resolved = client
                .resolve_tool_exchange(
                    exchange_id,
                    bcode_session_models::ToolExchangeResolution::Responded { payload },
                )
                .await?;
            print_interaction_resolution(resolved, json)?;
        }
        InteractionCommand::Cancel { exchange_id, json } => {
            ensure_server_running().await?;
            let client = BcodeClient::default_endpoint();
            let client = compatible_interaction_client(&client, &exchange_id).await?;
            let resolved = client
                .resolve_tool_exchange(
                    exchange_id,
                    bcode_session_models::ToolExchangeResolution::Cancelled,
                )
                .await?;
            print_interaction_resolution(resolved, json)?;
        }
    }
    Ok(())
}

fn read_invocation_input(path: &Path) -> Result<bcode_tool_models::ToolInvocationInput, CliError> {
    serde_json::from_value(read_bounded_interaction_json(path)?)
        .map_err(|_| CliError::InvalidArguments("invalid invocation input envelope".to_owned()))
}

fn write_invocation_input_receipt(
    output: &mut impl std::io::Write,
    json: bool,
) -> Result<(), CliError> {
    if json {
        writeln!(output, "{{\"accepted\":true}}")?;
    } else {
        writeln!(output, "Invocation input accepted")?;
    }
    output.flush()?;
    Ok(())
}

async fn find_pending_exchange(
    client: &BcodeClient,
    exchange_id: &str,
) -> Result<bcode_session_models::PendingToolExchangeSummary, CliError> {
    client
        .inspect_pending_tool_exchange(exchange_id)
        .await?
        .ok_or_else(|| {
            CliError::InvalidArguments(format!("pending interaction not found: {exchange_id}"))
        })
}

async fn compatible_interaction_client(
    client: &BcodeClient,
    exchange_id: &str,
) -> Result<BcodeClient, CliError> {
    let exchange = find_pending_exchange(client, exchange_id).await?;
    Ok(client.clone().with_interaction_adapter(
        bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability {
            producer_id: exchange.request.producer_id,
            exchange_schema: exchange.request.schema,
            min_schema_version: exchange.request.schema_version,
            max_schema_version: exchange.request.schema_version,
            platform_id: "cli".to_owned(),
            priority: 0,
            interaction_kind: "bcode.cli.schema-aware".to_owned(),
            tui_surface_kind: None,
        },
    ))
}

fn write_interaction_list<W: std::io::Write>(
    output: &mut W,
    exchanges: &[bcode_session_models::PendingToolExchangeSummary],
    json: bool,
) -> Result<(), CliError> {
    if json {
        return write_json_result(output, &exchanges);
    }
    if exchanges.is_empty() {
        writeln!(output, "no pending interactions")?;
    } else {
        for exchange in exchanges {
            writeln!(
                output,
                "{}\t{}\t{}\t{}\t{}\t{:?}",
                exchange.request.exchange_id,
                exchange.session_id,
                exchange.request.producer_id,
                exchange.request.schema,
                exchange.request.schema_version,
                exchange.request.response_policy,
            )?;
        }
    }
    output.flush()?;
    Ok(())
}

fn print_interaction_resolution(resolved: bool, json: bool) -> Result<(), CliError> {
    write_interaction_resolution(&mut std::io::stdout().lock(), resolved, json)
}

fn write_interaction_resolution<W: std::io::Write>(
    output: &mut W,
    resolved: bool,
    json: bool,
) -> Result<(), CliError> {
    if json {
        write_json_result(output, &serde_json::json!({ "resolved": resolved }))
    } else {
        writeln!(output, "resolved: {resolved}")?;
        output.flush()?;
        Ok(())
    }
}

async fn handle_plugin_command(command: PluginCommand) -> Result<(), CliError> {
    match command {
        PluginCommand::List { root, json } => list_plugins(&root, json)?,
        PluginCommand::Services { root, daemon, json } => {
            list_plugin_services(&root, daemon, json).await?;
        }
        PluginCommand::Check { root, json } => check_plugins(&root, json)?,
        PluginCommand::Invoke {
            root,
            daemon,
            plugin_id,
            interface_id,
            operation,
            payload,
            json,
        } => {
            invoke_plugin_service(
                &root,
                &plugin_id,
                &interface_id,
                &operation,
                payload,
                daemon,
                json,
            )
            .await?;
        }
        PluginCommand::Call {
            root,
            daemon,
            interface_id,
            operation,
            payload,
            json,
        } => call_plugin_service(&root, &interface_id, &operation, payload, daemon, json).await?,
        PluginCommand::Publish {
            root,
            daemon,
            topic,
            payload,
            json,
        } => publish_plugin_event(&root, &topic, payload, daemon, json).await?,
    }
    Ok(())
}

#[cfg(feature = "web-renderer")]
fn random_web_access_token() -> Result<String, CliError> {
    let mut data = [0_u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut data)
        .map_err(|error| CliError::HyperChadRender(error.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(data))
}

#[cfg(feature = "web-renderer")]
async fn handle_web_command(
    bind: std::net::IpAddr,
    requested_port: Option<u16>,
    allow_non_loopback: bool,
) -> Result<(), CliError> {
    let bind = bcode_hyperchad::validate_bind_address(bind, allow_non_loopback)
        .map_err(|error| CliError::HyperChadRender(error.to_owned()))?;
    let port = requested_port.unwrap_or(0);
    let access_token = random_web_access_token()?;
    let config = bcode_config::load_config()?;
    let state =
        bcode_hyperchad::HyperChadAppState::with_streaming_presentation_policy_in_working_directory(
            BcodeClient::default_endpoint(),
            access_token.clone(),
            bcode_ipc::current_working_directory(),
            config.presentation.streaming.policy(),
        );
    let builder = bcode_hyperchad::init(&state).await?;
    let launch_token = access_token;
    let builder = builder
        .with_actix_bind_address(bind.to_string())
        .with_actix_port(port)
        .with_actix_on_bound(move |address| {
            let launch_url = bcode_hyperchad::build_launch_url(address, &launch_token, None);
            if open::that_detached(&launch_url).is_err() {
                eprintln!("Bcode could not open the HyperChad application in a browser.");
                eprintln!(
                    "Restart `bcode web` after checking that a graphical browser is available. The private launch capability was not printed."
                );
            } else {
                println!("Bcode HyperChad application opened at http://{address}/");
            }
        });
    let (app, renderer_runtime) = bcode_hyperchad::build_app_with_runtime(builder)
        .map_err(|error| CliError::HyperChadRender(error.to_string()))?;
    bcode_hyperchad::configure_live_updates(&app.renderer, &state);
    let result = tokio::task::spawn_blocking(move || app.handle_serve())
        .await
        .map_err(|error| {
            CliError::HyperChadRender(format!("HyperChad web renderer task failed: {error}"))
        })?
        .map_err(|error| CliError::HyperChadRender(error.to_string()));
    drop(renderer_runtime);
    result
}

async fn handle_onboard_flags(
    reset: bool,
    output_mode: OnboardOutputMode,
    provider: Option<String>,
    launch_mode: OnboardLaunchMode,
    experience_mode: OnboardExperienceMode,
    secure_import_env: Option<String>,
    no_credential_discovery: bool,
) -> Result<(), CliError> {
    handle_onboard_command(&OnboardOptions {
        reset,
        output_mode,
        provider,
        launch_mode,
        experience_mode,
        secure_import_env,
        no_credential_discovery,
    })
    .await
}

const fn onboard_output_mode(dry_run: bool, non_interactive: bool) -> OnboardOutputMode {
    if dry_run {
        OnboardOutputMode::DryRun
    } else if non_interactive {
        OnboardOutputMode::NonInteractive
    } else {
        OnboardOutputMode::Preview
    }
}

fn should_auto_start_onboarding() -> Result<bool, CliError> {
    if std::env::var_os("CI").is_some() || std::env::var_os("BCODE_NO_ONBOARD").is_some() {
        return Ok(false);
    }
    let store = bcode_settings::SettingsStore::default();
    let config = bcode_config::load_config()?;
    let summary = bcode_settings::SetupConfigSummary::from_config(&config);
    let progress = store.onboarding_progress()?;
    Ok(bcode_settings::should_auto_start_onboarding(
        bcode_settings::OnboardingStartupCommand::NormalTui,
        std::io::stdout().is_terminal(),
        progress.as_ref(),
        &summary,
    )
    .should_start)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum OnboardOutputMode {
    #[default]
    Preview,
    DryRun,
    NonInteractive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum OnboardLaunchMode {
    #[default]
    LaunchWhenReady,
    SkipLaunch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum OnboardExperienceMode {
    #[default]
    FirstRun,
    ControlCenter,
}

#[derive(Debug, Clone, Default)]
struct OnboardOptions {
    reset: bool,
    output_mode: OnboardOutputMode,
    provider: Option<String>,
    launch_mode: OnboardLaunchMode,
    experience_mode: OnboardExperienceMode,
    secure_import_env: Option<String>,
    no_credential_discovery: bool,
}

fn import_onboarding_env_credential(
    env_var: &str,
    plans: &[bcode_settings::SecureCredentialImportPlan],
    imported_at_ms: u64,
) -> Result<(), CliError> {
    let Some(plan) = plans.iter().find(|plan| plan.env_var == env_var) else {
        println!("no detected secure-import plan for {env_var}");
        return Ok(());
    };
    let Some(value) = std::env::var_os(env_var) else {
        println!("{env_var} is not present; nothing imported");
        return Ok(());
    };
    let value = value.to_string_lossy().into_owned();
    let vault = bcode_config::default_auth_vault_path();
    let store = sshenv_vault::SshenvStore::new(
        sshenv_vault::SshenvStoreConfig::new(vault.clone()).with_private_key_paths(
            bcode_provider_auth::security::vault_private_key_paths(&vault),
        ),
    );
    store
        .set_secret(
            &plan.auth_profile,
            &plan.credential_key,
            zeroize::Zeroizing::new(value),
        )
        .map_err(|error| CliError::Sshenv(error.to_string()))?;
    bcode_settings::SettingsStore::default().put_control_state(
        "onboarding.secure_import.last",
        &serde_json::json!({
            "env_var": env_var,
            "auth_profile": plan.auth_profile,
            "credential_key": plan.credential_key,
            "imported_at_ms": imported_at_ms,
            "raw_value_stored": false,
        }),
        imported_at_ms,
    )?;
    println!(
        "imported {env_var} into sshenv profile '{}' without storing the raw value in settings",
        plan.auth_profile
    );
    Ok(())
}

fn print_onboarding_preview(
    config: &bcode_config::BcodeConfig,
    store: &bcode_settings::SettingsStore,
    discovery_enabled: bool,
    options: &OnboardOptions,
) {
    let summary = bcode_settings::SetupConfigSummary::from_config(config);
    let shell = bcode_tui::onboarding::OnboardingShell::from_reconciliation(
        &[],
        &summary.reconciliation_input(),
    );
    let readiness = bcode_settings::setup_readiness_report(shell.sections(), &[]);
    println!("Bcode setup\n");
    println!(
        "{}",
        shell
            .render_model(&store.health(), Some(readiness))
            .snapshot_text()
    );
    println!("Automatic credential discovery: {discovery_enabled}");
    if options.reset || options.secure_import_env.is_some() {
        println!("Preview only: reset and credential import were not performed.");
    }
}

async fn handle_onboard_command(options: &OnboardOptions) -> Result<(), CliError> {
    let store = bcode_settings::SettingsStore::default();
    let config = bcode_config::load_config()?;
    let discovery_enabled = config.onboarding.credential_discovery_enabled(
        options.no_credential_discovery,
        &bcode_config::ProcessConfigEnvironment,
    );
    if options.output_mode != OnboardOutputMode::Preview {
        print_onboarding_preview(&config, &store, discovery_enabled, options);
        return Ok(());
    }
    if options.reset {
        store.reset_database()?;
    }
    credential_setup::discover_for_onboarding(discovery_enabled)?;
    let now_ms = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX);
    let detection = bcode_settings::detect_setup_environment_with_policy(discovery_enabled, now_ms);
    store.put_control_state(
        "onboarding.experience_mode",
        &serde_json::json!({
            "mode": match options.experience_mode {
                OnboardExperienceMode::FirstRun => "first_run",
                OnboardExperienceMode::ControlCenter => "control_center",
            },
            "selected_at_ms": now_ms,
        }),
        now_ms,
    )?;
    store.save_setup_detection_snapshot(&detection)?;
    let config = bcode_config::load_config()?;
    let auth_detection = bcode_settings::detect_auth_security_from_config(&config);
    let secure_import_plans =
        bcode_settings::secure_import_plans_from_detection(&detection.entries);
    if let Some(env_var) = options.secure_import_env.as_deref() {
        // Explicit import selects exactly one supported source, independently of
        // automatic discovery. Do not enumerate the environment to plan it.
        let selected = bcode_settings::detect_setup_environment_from_vars(
            &std::collections::BTreeMap::from([(env_var.to_owned(), "present".to_owned())]),
            now_ms,
        );
        let plans = bcode_settings::secure_import_plans_from_detection(&selected.entries);
        if options.output_mode == OnboardOutputMode::Preview {
            import_onboarding_env_credential(env_var, &plans, now_ms)?;
        }
    }
    let secure_story =
        bcode_settings::secure_credential_story_panel(&secure_import_plans, &auth_detection);
    let draft = store.onboarding_draft_setup()?;
    let questionnaire = bcode_settings::deterministic_onboarding_questionnaire(&draft, &detection);
    store.put_control_state(
        "onboarding.questionnaire",
        &serde_json::to_value(&questionnaire)?,
        now_ms,
    )?;
    store.put_control_state(
        "onboarding.secure_credential_story",
        &serde_json::to_value(&secure_story)?,
        now_ms,
    )?;
    store.visit_onboarding_section(bcode_settings::SetupSectionId::Welcome, now_ms)?;
    let summary = bcode_settings::SetupConfigSummary::from_config(&config);
    let mut input = summary.reconciliation_input();
    if let Some(provider) = options.provider.as_deref() {
        println!("onboarding provider hint (not configured): {provider}");
    }
    let progress = store.onboarding_progress()?;
    input.current_section = progress
        .and_then(|progress| progress.last_section)
        .as_deref()
        .and_then(onboard_section_from_str);
    let persisted_sections = store.onboarding_sections()?;
    let recommendations = if discovery_enabled {
        store.setup_recommendations()?
    } else {
        Vec::new()
    };
    let shell =
        bcode_tui::onboarding::OnboardingShell::from_reconciliation(&persisted_sections, &input);
    let readiness_report =
        bcode_settings::setup_readiness_report(shell.sections(), &recommendations);
    store.save_readiness_report(&readiness_report, now_ms)?;
    let launch = Box::pin(credential_setup::run_setup(discovery_enabled)).await?;
    if launch && options.launch_mode == OnboardLaunchMode::LaunchWhenReady {
        Box::pin(start_validated_onboarding_session(&store, now_ms)).await?;
    }
    Ok(())
}

async fn start_validated_onboarding_session(
    store: &bcode_settings::SettingsStore,
    now_ms: u64,
) -> Result<(), CliError> {
    credential_setup::validate_launch_selection()?;
    ensure_server_running().await?;
    Box::pin(model_validate_config(false)).await?;
    store.complete_onboarding(now_ms)?;
    Box::pin(run_new_session_tui(
        None,
        bcode_tui::TuiLaunchOptions::default(),
    ))
    .await
}

fn onboard_section_from_str(value: &str) -> Option<bcode_settings::SetupSectionId> {
    bcode_settings::SetupSectionId::all()
        .into_iter()
        .find(|section| section.as_str() == value)
}

async fn handle_session_io_command(
    command: Commands,
    launch_options: bcode_tui::TuiLaunchOptions,
) -> Result<(), CliError> {
    match command {
        Commands::Cancel {
            session_id,
            clear_queue,
            json,
        } => cancel_session_turn(session_id, clear_queue, json).await?,
        Commands::Attach { session_id } => attach_session(session_id).await?,
        Commands::Tui { session_id } => {
            bcode_tui::run_with_static_bundled_and_options(
                session_id,
                &static_bundled_plugins(),
                build_info().clone(),
                launch_options,
            )
            .await?;
        }
        Commands::Send {
            session_id,
            message,
            file,
            stdin,
            follow_up,
            producer,
            idempotency_key,
            background,
            json,
        } => {
            Box::pin(send_message(
                session_id,
                SendOptions {
                    input: PromptInput {
                        message,
                        file,
                        stdin,
                    },
                    follow_up,
                    producer,
                    idempotency_key,
                    background,
                    json,
                    launch_options,
                },
            ))
            .await?;
        }
        Commands::Onboard { .. }
        | Commands::Settings { .. }
        | Commands::ArtifactId
        | Commands::Server { .. }
        | Commands::Session { .. }
        | Commands::State { .. }
        | Commands::Plugin { .. }
        | Commands::Theme { .. }
        | Commands::Model { .. }
        | Commands::Auth { .. }
        | Commands::Login { .. }
        | Commands::Permission { .. }
        | Commands::Interaction { .. }
        | Commands::Worktree { .. }
        | Commands::RuntimeWork { .. }
        | Commands::Workflow { .. } => unreachable!("handled by handle_cli"),
        #[cfg(feature = "web-renderer")]
        Commands::Web { .. } => unreachable!("handled by handle_cli"),
    }
    Ok(())
}

async fn handle_permission_command(command: PermissionCommand) -> Result<(), CliError> {
    match command {
        PermissionCommand::Status { json } => {
            ensure_server_running().await?;
            let status = BcodeClient::default_endpoint()
                .agent_policy_status()
                .await?;
            if json {
                print_json(&status)?;
            } else {
                write_permission_status(
                    &mut std::io::stdout().lock(),
                    &mut std::io::stderr().lock(),
                    &status.source,
                    status.using_default,
                    &status.build_enabled_tools,
                    &status.plan_enabled_tools,
                    &status.diagnostics,
                )?;
            }
        }
        PermissionCommand::List { session_id, json } => {
            list_permissions(session_id, json).await?;
        }
        PermissionCommand::Approve {
            permission_id,
            remember,
            json,
        } => {
            resolve_permission(permission_id, true, remember, json).await?;
        }
        PermissionCommand::Deny {
            permission_id,
            json,
        } => {
            resolve_permission(permission_id, false, false, json).await?;
        }
        PermissionCommand::ResolveBatch {
            batch_id,
            approve,
            deny,
            json,
        } => {
            debug_assert_ne!(approve, deny);
            resolve_permission_batch(batch_id, approve, json).await?;
        }
        PermissionCommand::Add {
            agent,
            category,
            pattern,
            action,
            json,
        } => {
            add_permission_rule(&agent, &category, pattern, &action, json).await?;
        }
    }
    Ok(())
}

fn foreground_server_requested_from<I, S>(arguments: I, inherited_endpoint: bool) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut previous_was_server = false;
    for argument in arguments {
        if previous_was_server && argument.as_ref() == "run" {
            return !inherited_endpoint;
        }
        previous_was_server = argument.as_ref() == "server";
    }
    false
}

fn foreground_server_requested() -> bool {
    foreground_server_requested_from(
        std::env::args_os(),
        std::env::var_os(bcode_ipc::BCODE_IPC_ENDPOINT_NAMESPACE_ENV).is_some(),
    )
}

fn init_tracing() {
    let foreground_server = foreground_server_requested();
    let filter = std::env::var("BCODE_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .unwrap_or_else(|| {
            if std::env::var_os("BCODE_STARTUP_TRACE").is_some() {
                "bcode_client::startup=debug,bcode_server::startup=debug,bcode_plugin::startup=debug,bcode_daemon_lifecycle::startup=debug".to_string()
            } else if foreground_server {
                "info".to_string()
            } else {
                // Background daemons are otherwise silent; lifecycle transitions that explain why
                // a daemon is still alive are low-volume and always worth a log line.
                "off,bcode_server::idle_shutdown=info,bcode_server::session_stream=info,bcode_tui::session_stream=info".to_string()
            }
        });
    let (env_filter, invalid_filter) = match tracing_subscriber::EnvFilter::try_new(filter) {
        Ok(filter) => (filter, None),
        Err(error) => (tracing_subscriber::EnvFilter::new("off"), Some(error)),
    };
    if foreground_server {
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_ansi(std::io::stderr().is_terminal())
            .with_writer(std::io::stderr)
            .finish();
        let _ = subscriber.try_init();
    } else {
        let log_path = bcode_daemon_lifecycle::default_daemon_log_path();
        let log_parent_ready = log_path
            .parent()
            .is_none_or(|parent| fs::create_dir_all(parent).is_ok());
        let writer_path = log_path;
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_ansi(false)
            .with_writer(move || -> Box<dyn std::io::Write> {
                if log_parent_ready
                    && let Ok(file) = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&writer_path)
                {
                    return Box::new(file);
                }
                Box::new(std::io::sink())
            })
            .finish();
        let _ = subscriber.try_init();
    }
    if let Some(error) = invalid_filter {
        tracing::warn!(%error, "invalid log filter; logging disabled");
    }
}

fn root_command_with_build_info(build_info: &'static bcode_build_info::BuildInfo) -> clap::Command {
    Cli::command().version(build_info.display_version())
}

/// Return the root `bcode` CLI command definition.
///
/// This keeps generated documentation, completions, and help snapshots in sync
/// with the actual parser without exposing parser internals as public API.
#[must_use]
pub fn root_command() -> clap::Command {
    Cli::command()
}

#[cfg(test)]
mod build_version_tests {
    use super::*;

    #[test]
    fn root_command_uses_exact_canonical_build_label() {
        let cases = [
            (
                bcode_build_info::BuildMode::Distribution,
                bcode_build_info::GitState::Unavailable,
                "bcode v1.2.3\n",
            ),
            (
                bcode_build_info::BuildMode::Developer,
                bcode_build_info::GitState::Revision {
                    short_commit: "abcdef12".to_owned(),
                    dirty: false,
                },
                "bcode v1.2.3-dev.gabcdef12.b1234abcd\n",
            ),
            (
                bcode_build_info::BuildMode::Developer,
                bcode_build_info::GitState::Revision {
                    short_commit: "abcdef12".to_owned(),
                    dirty: true,
                },
                "bcode v1.2.3-dev.gabcdef12.dirty.b1234abcd\n",
            ),
            (
                bcode_build_info::BuildMode::Developer,
                bcode_build_info::GitState::Unavailable,
                "bcode v1.2.3-dev.nogit.b1234abcd\n",
            ),
        ];
        for (mode, git, expected) in cases {
            let info = Box::leak(Box::new(
                bcode_build_info::BuildInfo::new("1.2.3", mode, git, "1234abcd")
                    .expect("build info"),
            ));
            let output = root_command_with_build_info(info).render_version().clone();
            assert_eq!(output, expected);
        }
    }
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
    /// Select a user-defined configuration context for this invocation.
    #[arg(long, global = true, value_name = "CONTEXT")]
    context: Option<String>,
    /// Select a model profile from configuration for this client connection.
    #[arg(long, value_name = "MODEL_PROFILE")]
    profile: Option<String>,
    /// Override the local client/daemon IPC request timeout in seconds.
    #[arg(
        long,
        global = true,
        value_name = "SECONDS",
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    request_timeout_secs: Option<u64>,
    /// Use an explicit absolute durable state root for this invocation.
    #[arg(
        long = "state-root",
        global = true,
        value_name = "DIR",
        conflicts_with = "state_profile"
    )]
    state_root: Option<PathBuf>,
    /// Select a named `[state.profile.<name>]` durable state location.
    #[arg(long = "state-profile", global = true, value_name = "PROFILE")]
    state_profile: Option<String>,
    /// Do not automatically discover external credentials or suggest cached discoveries.
    #[arg(long, global = true)]
    no_credential_discovery: bool,
    /// Force the onboarding/setup-map flow.
    #[arg(long = "onboard", global = true)]
    onboard: bool,
    #[command(flatten)]
    execution_mode: ExecutionModeArgs,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, clap::Args)]
struct ExecutionModeArgs {
    /// Allow structurally valid tool operations without agent or skill permission prompts.
    #[arg(
        long = "dangerously-bypass-all-permissions",
        visible_alias = "yolo",
        global = true,
        conflicts_with = "disable_all_tools"
    )]
    dangerously_bypass_all_permissions: bool,
    /// Do not expose or execute agent tools.
    #[arg(long = "disable-all-tools", visible_alias = "no-tools", global = true)]
    disable_all_tools: bool,
}

const fn execution_mode_launch_options(
    dangerously_bypass_all_permissions: bool,
    disable_all_tools: bool,
) -> bcode_tui::TuiLaunchOptions {
    bcode_tui::TuiLaunchOptions {
        permission_mode: if dangerously_bypass_all_permissions {
            bcode_session_models::TurnPermissionMode::Bypass
        } else {
            bcode_session_models::TurnPermissionMode::Enforce
        },
        tool_policy: if disable_all_tools {
            bcode_session_models::TurnToolPolicy::Disabled
        } else {
            bcode_session_models::TurnToolPolicy::Enabled
        },
    }
}

impl Cli {
    const fn launch_options(&self) -> bcode_tui::TuiLaunchOptions {
        execution_mode_launch_options(
            self.execution_mode.dangerously_bypass_all_permissions,
            self.execution_mode.disable_all_tools,
        )
    }

    fn supports_execution_mode(&self) -> bool {
        !self.onboard
            && (self.new
                || self.command.as_ref().is_none_or(|command| {
                    matches!(command, Commands::Tui { .. } | Commands::Send { .. })
                }))
    }
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Review a scoped configuration change while preserving unrelated TOML.
    Settings {
        /// Exact configuration file to edit.
        #[arg(long)]
        file: PathBuf,
        /// TOML key segments (separate arguments preserve dots in plugin IDs).
        #[arg(long, num_args = 1.., required = true)]
        key: Vec<String>,
        /// New TOML value, e.g. '"model-name"' or 'false'.
        #[arg(long, conflicts_with = "remove")]
        value: Option<String>,
        /// Remove the selected override.
        #[arg(long)]
        remove: bool,
    },
    Onboard {
        /// Reset onboarding progress before launching the setup map.
        #[arg(long)]
        reset: bool,
        /// Print detected onboarding state without launching the TUI.
        #[arg(long)]
        dry_run: bool,
        /// Print a non-interactive onboarding summary.
        #[arg(long)]
        non_interactive: bool,
        /// Preselect a provider path for onboarding.
        #[arg(long, value_name = "PROVIDER")]
        provider: Option<String>,
        /// Do not launch a session after onboarding completes.
        #[arg(long)]
        skip_launch: bool,
        /// Reopen the setup map as Settings / Control Center.
        #[arg(long)]
        control_center: bool,
        /// Securely import one detected environment credential into sshenv.
        #[arg(long = "secure-import-env", value_name = "ENV_VAR")]
        secure_import_env: Option<String>,
    },
    /// Print the exact identity embedded in this produced artifact.
    #[command(hide = true)]
    ArtifactId,
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Inspect configured durable state locations.
    State {
        #[command(subcommand)]
        command: StateCommand,
    },
    #[cfg(feature = "web-renderer")]
    Web {
        /// Address to bind. Defaults to IPv4 loopback.
        #[arg(long, default_value_t = bcode_hyperchad::DEFAULT_BIND_ADDRESS)]
        bind: std::net::IpAddr,
        /// Port to bind. Defaults to an OS-assigned available port.
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        port: Option<u16>,
        /// Explicitly allow binding to a non-loopback address.
        #[arg(long, requires = "bind")]
        allow_non_loopback: bool,
    },
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    Theme {
        #[command(subcommand)]
        command: ThemeCommand,
    },
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Deprecated compatibility login commands. Use `bcode auth login <provider>`.
    Login {
        #[command(subcommand)]
        command: LoginCommand,
    },
    Permission {
        #[command(subcommand)]
        command: PermissionCommand,
    },
    /// Inspect and resolve renderer-neutral pending tool interactions.
    Interaction {
        #[command(subcommand)]
        command: InteractionCommand,
    },
    /// Manage Git worktrees through the daemon-owned worktree application API.
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
    Workflow {
        #[command(subcommand)]
        command: WorkflowCommand,
    },
    RuntimeWork {
        #[command(subcommand)]
        command: RuntimeWorkCommand,
    },
    Cancel {
        session_id: SessionId,
        #[arg(long)]
        clear_queue: bool,
        /// Print the cancellation request result as JSON.
        #[arg(long)]
        json: bool,
    },
    Attach {
        session_id: SessionId,
    },
    Tui {
        session_id: Option<SessionId>,
    },
    Send {
        session_id: SessionId,
        /// Prompt text. Omit when using --file or --stdin.
        message: Option<String>,
        /// Read the prompt from a bounded UTF-8 file.
        #[arg(long, value_name = "FILE", conflicts_with_all = ["message", "stdin"])]
        file: Option<PathBuf>,
        /// Read the prompt from bounded UTF-8 stdin.
        #[arg(long, conflicts_with_all = ["message", "file"])]
        stdin: bool,
        /// Queue explicitly as a follow-up instead of default steering semantics.
        /// Incompatible with turn-admission producer, idempotency, and priority options.
        #[arg(long, conflicts_with_all = ["producer", "idempotency_key", "background"])]
        follow_up: bool,
        /// Stable producer namespace for durable turn admission.
        #[arg(long, default_value = "bcode.cli")]
        producer: String,
        /// Optional producer-owned idempotency key.
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Admit the turn at background scheduling priority.
        #[arg(long)]
        background: bool,
        /// Print exact canonical `TurnAdmission` as JSON.
        #[arg(long)]
        json: bool,
    },
}

impl Default for Commands {
    fn default() -> Self {
        Self::Tui { session_id: None }
    }
}

#[derive(Debug, Subcommand)]
enum ThemeCommand {
    /// List bundled themes in stable id order.
    List {
        /// Emit the bundled theme catalog as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Validate one theme file using the runtime parser and resolver.
    Validate {
        #[arg(value_name = "PATH")]
        path: PathBuf,
        /// Emit the successful validation result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Copy one bundled theme definition to an editable file.
    Copy {
        #[arg(value_name = "BUILTIN")]
        builtin: String,
        #[arg(value_name = "PATH")]
        path: PathBuf,
        /// Replace an existing destination file.
        #[arg(long)]
        force: bool,
        /// Emit the completed copy result as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum WorkflowCommand {
    /// Persist a live run edit candidate; does not publish topology or execute the edit.
    StageRunEdit {
        /// Bounded JSON `WorkflowRunGraphEditBatch`, including run, revision, and mutation identity.
        #[arg(long)]
        file: PathBuf,
    },
    /// Runtime-authored workflow operations.
    Author {
        #[command(subcommand)]
        command: Box<WorkflowAuthorCommand>,
    },
    /// Source-controlled workflow package operations.
    Package {
        #[command(subcommand)]
        command: WorkflowPackageCommand,
    },
    /// Explicitly migrate the immediately preceding workflow store schema without data loss.
    MigrateStore,
    /// Explicitly back up and delete only incompatible workflow-owned state.
    ResetStore {
        /// Required destructive-operation acknowledgement.
        #[arg(long, value_name = "DELETE-INCOMPATIBLE-WORKFLOW-STATE")]
        confirm: String,
    },
    /// Start one immutable authored-workflow revision.
    Start {
        #[command(subcommand)]
        selection: WorkflowStartSelection,
        #[arg(long)]
        parent_session_id: SessionId,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long)]
        workspace_snapshot: Option<String>,
        /// Exact accepted parent-session generation required by fixed-generation prompt nodes.
        #[arg(long)]
        parent_session_generation: Option<u64>,
        #[arg(long, value_name = "FILE")]
        configuration: Option<PathBuf>,
        #[arg(long, value_name = "JSON_FILE")]
        input: Option<PathBuf>,
    },
    /// Return one bounded page of workflow events as JSON (not a live watch).
    Events {
        #[arg(long)]
        run_id: String,
        /// Return events after this sequence; omit for the first page.
        #[arg(long)]
        after_sequence: Option<u64>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Return one bounded page of workflow attempts as JSON.
    Attempts {
        #[arg(long)]
        run_id: String,
        /// Timestamp from the last attempt on the preceding page.
        #[arg(long, requires = "after_dispatch_identity")]
        after_prepared_at_ms: Option<u64>,
        /// Dispatch identity from the same last attempt.
        #[arg(long, requires = "after_prepared_at_ms")]
        after_dispatch_identity: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// List bounded, checksum-verified registered definitions as JSON.
    Definitions {
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Inspect one exact registered definition version as JSON, or null when absent.
    DescribeDefinition {
        #[arg(long)]
        definition_id: String,
        #[arg(long)]
        version: u32,
    },
    /// Inspect one workflow run for damage or ambiguous attempts without repairing it.
    Doctor {
        #[arg(long)]
        run_id: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Return one durable run summary as JSON, or null when absent.
    RunStatus {
        #[arg(long)]
        run_id: String,
    },
    /// List bounded workflow run summaries as JSON.
    Runs {
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Request durable run cancellation; print whether it was recorded as JSON.
    CancelRun {
        #[arg(long)]
        run_id: String,
    },
    /// Pause further scheduler admission; print the transition result as JSON.
    PauseRun {
        #[arg(long)]
        run_id: String,
    },
    /// Resume a paused workflow; print the transition result as JSON.
    ResumeRun {
        #[arg(long)]
        run_id: String,
    },
    /// Retry one exact failed node attempt and return the admission result as JSON.
    RetryNode {
        #[arg(long)]
        run_id: String,
        #[arg(long)]
        node_id: String,
        #[arg(long)]
        activation_id: String,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        failed_attempt: u32,
    },
    /// List bounded durable input and approval waits as JSON.
    Waits {
        #[arg(long)]
        run_id: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Return the bounded renderer-neutral workflow catalog as JSON.
    CatalogView {
        /// Typed catalog query as inline JSON, a JSON file, or `-` for stdin.
        #[arg(long, value_name = "JSON_OR_FILE")]
        query: String,
    },
    /// Return the bounded renderer-neutral workflow run projection as JSON.
    RunView {
        #[arg(long)]
        run_id: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Return one bounded public run inspection including canonical output and descendants.
    InspectRun {
        #[arg(long)]
        run_id: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Return bounded canonical validated output values for one run.
    RunOutput {
        #[arg(long)]
        run_id: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Resolve one exact durable typed input wait.
    ProvideInput {
        #[arg(long)]
        run_id: String,
        #[arg(long)]
        node_id: String,
        #[arg(long)]
        activation_id: String,
        /// Inline JSON, a JSON file, or `-` for stdin; capped at the workflow document byte limit.
        #[arg(long, value_name = "JSON_OR_FILE")]
        value: String,
    },
    /// Resolve one exact durable approval wait.
    ResolveApproval {
        #[arg(long)]
        run_id: String,
        #[arg(long)]
        node_id: String,
        #[arg(long)]
        activation_id: String,
        #[arg(long, conflicts_with = "deny", required_unless_present = "deny")]
        approve: bool,
        #[arg(long, conflicts_with = "approve")]
        deny: bool,
    },
    /// Resolve one exact pending workflow mutation approval and return JSON.
    ResolveMutationApproval {
        #[arg(long)]
        approval_id: String,
        #[arg(long, conflicts_with = "deny", required_unless_present = "deny")]
        approve: bool,
        #[arg(long, conflicts_with = "approve")]
        deny: bool,
    },
    /// List bounded pending workflow mutation approvals as JSON.
    MutationApprovals {
        /// Restrict results to one workflow run; omit to list across runs.
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Cancel one exact in-flight validation or compilation operation.
    CancelComputation {
        #[arg(long)]
        operation_id: String,
    },
    /// Explicit maintenance: find nonterminal workflow runs whose coordinator daemon verifiably
    /// ended (for example a loop paused under a build that no longer runs) and, with `--apply`,
    /// reassign them to the current daemon and cancel them so their sessions can start new
    /// loops. Without `--apply` this only reports what would change.
    ReconcileOrphans {
        /// Apply the reassignment and cancellation instead of only reporting.
        #[arg(long)]
        apply: bool,
        /// Maximum nonterminal runs to inspect.
        #[arg(long, default_value_t = 200)]
        limit: usize,
        /// Print the structured report as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum WorkflowPackageCommand {
    /// Discover bounded package manifests from canonical workspace and configured roots.
    Discover {
        #[arg(long, value_name = "PATH")]
        workspace: Option<PathBuf>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Validate and deterministically plan a bounded package manifest.
    Validate {
        #[arg(value_name = "MANIFEST")]
        manifest: PathBuf,
        #[arg(long)]
        operation_id: Option<String>,
        #[arg(long, default_value_t = bcode_ipc::DEFAULT_WORKFLOW_COMPUTATION_TIMEOUT_MS)]
        timeout_ms: u64,
    },
    /// Validate, plan, and compile-preview every package member without mutation.
    Preview {
        #[arg(value_name = "MANIFEST")]
        manifest: PathBuf,
        #[arg(long)]
        operation_id: Option<String>,
        #[arg(long, default_value_t = bcode_ipc::DEFAULT_WORKFLOW_COMPUTATION_TIMEOUT_MS)]
        timeout_ms: u64,
    },
    /// Validate, plan, and atomically apply all package members as canonical package drafts.
    Apply {
        #[arg(value_name = "MANIFEST")]
        manifest: PathBuf,
        /// Exact `MEMBER_ID=GENERATION` facts for every existing member; omitted members are create-only.
        #[arg(long = "expected-generation", value_name = "MEMBER_ID=GENERATION")]
        expected_generations: Vec<String>,
        #[arg(long)]
        operation_id: Option<String>,
        #[arg(long, default_value_t = bcode_ipc::DEFAULT_WORKFLOW_COMPUTATION_TIMEOUT_MS)]
        timeout_ms: u64,
    },
    /// Atomically publish exact package draft generations from an applied lock candidate.
    Publish {
        #[arg(long, value_name = "LOCK_JSON")]
        lock: PathBuf,
        /// One exact `MEMBER_ID=GENERATION` fact for every locked member.
        #[arg(long = "expected-generation", value_name = "MEMBER_ID=GENERATION")]
        expected_generations: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum WorkflowStartSelection {
    /// Start one exact immutable revision.
    Revision {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        revision: u64,
    },
    /// Resolve and start the current active revision.
    Active {
        #[arg(long)]
        workflow_id: String,
    },
    /// Resolve and start one exact published package export.
    PackageExport {
        #[arg(long)]
        package_id: String,
        #[arg(long)]
        export: String,
        #[arg(long)]
        package_lock_digest_sha256: Option<String>,
    },
    /// Resolve and start one exact preset generation.
    Preset {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        preset_id: String,
        #[arg(long)]
        preset_generation: u64,
    },
}

#[derive(Debug, Subcommand)]
enum WorkflowAuthorCommand {
    /// Create one logical workflow and initial draft from JSON, YAML, or TOML.
    Create {
        #[arg(value_name = "FILE", default_value = "-")]
        file: PathBuf,
        /// Explicit source format for stdin or to override the file extension.
        #[arg(long, value_parser = ["json", "yaml", "yml", "toml"])]
        source_format: Option<String>,
        /// Stable identity for the initial mutable draft.
        #[arg(long)]
        draft_id: String,
    },
    /// Apply one source file to the default or selected source draft without publishing or starting.
    Apply {
        #[arg(value_name = "FILE", default_value = "-")]
        file: PathBuf,
        /// Explicit source format for stdin or to override the file extension.
        #[arg(long, value_parser = ["json", "yaml", "yml", "toml"])]
        source_format: Option<String>,
        /// Stable source-backed draft identity.
        #[arg(long, default_value = bcode_workflow::DEFAULT_WORKFLOW_SOURCE_DRAFT_ID)]
        draft_id: String,
    },
    /// List one bounded keyset page of logical workflows.
    List {
        #[arg(long, requires = "cursor_workflow_id")]
        cursor_updated_at_ms: Option<u64>,
        #[arg(long, requires = "cursor_updated_at_ms")]
        cursor_workflow_id: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Get one logical workflow.
    Get {
        #[arg(long)]
        workflow_id: String,
    },
    /// Inspect one bounded aggregate authored-workflow snapshot.
    Inspect {
        #[arg(long)]
        workflow_id: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Read mutable workflow drafts.
    Draft {
        #[command(subcommand)]
        command: WorkflowDraftCommand,
    },
    /// Read immutable workflow revisions.
    Revision {
        #[command(subcommand)]
        command: WorkflowRevisionCommand,
    },
    /// Replace one exact draft generation from JSON, YAML, or TOML.
    Update {
        #[arg(value_name = "FILE", default_value = "-")]
        file: PathBuf,
        /// Explicit source format for stdin or to override the file extension.
        #[arg(long, value_parser = ["json", "yaml", "yml", "toml"])]
        source_format: Option<String>,
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        draft_id: String,
        #[arg(long)]
        expected_generation: u64,
    },
    /// Publish one exact draft generation.
    Publish {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        draft_id: String,
        #[arg(long)]
        expected_generation: u64,
        #[arg(long, value_name = "FILE")]
        configuration: Option<PathBuf>,
        #[arg(long)]
        activate: bool,
        #[arg(long)]
        expected_active_revision: Option<u64>,
        /// Stable identity usable by `workflow cancel-computation`.
        #[arg(long)]
        operation_id: Option<String>,
        /// Server-enforced compilation deadline.
        #[arg(long, default_value_t = bcode_ipc::DEFAULT_WORKFLOW_COMPUTATION_TIMEOUT_MS)]
        timeout_ms: u64,
    },
    /// Publish one exact draft and then attempt separately reported durable run admission.
    PublishAndStart {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        draft_id: String,
        #[arg(long)]
        expected_generation: u64,
        #[arg(long, value_name = "FILE")]
        configuration: Option<PathBuf>,
        #[arg(long)]
        activate: bool,
        #[arg(long)]
        expected_active_revision: Option<u64>,
        /// Stable identity usable by `workflow cancel-computation`.
        #[arg(long)]
        operation_id: Option<String>,
        /// Server-enforced compilation deadline.
        #[arg(long, default_value_t = bcode_ipc::DEFAULT_WORKFLOW_COMPUTATION_TIMEOUT_MS)]
        timeout_ms: u64,
        #[arg(long)]
        parent_session_id: SessionId,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long)]
        workspace_snapshot: Option<String>,
    },
    /// Compare-and-set one published revision as active.
    Activate {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        expected_active_revision: Option<u64>,
    },
    /// Archive or unarchive one logical workflow.
    Archive {
        #[arg(long)]
        workflow_id: String,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        archived: bool,
    },
    /// Discard one exact mutable draft generation.
    Discard {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        draft_id: String,
        #[arg(long)]
        expected_generation: u64,
    },
    /// Fork one exact draft or immutable revision.
    Fork {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        draft_id: String,
        #[arg(long, conflicts_with = "source_revision")]
        source_draft: Option<String>,
        #[arg(long, conflicts_with = "source_draft")]
        source_revision: Option<u64>,
    },
    /// Mutate revision-bound workflow presets.
    Preset {
        #[command(subcommand)]
        command: WorkflowPresetCommand,
    },
    /// Export one exact immutable revision as portable JSON.
    Export {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        revision: u64,
    },
    /// Preview one portable export bundle import without mutation.
    ImportPreview {
        #[arg(value_name = "FILE", default_value = "-")]
        file: PathBuf,
        #[arg(long)]
        target_workflow_id: String,
        #[arg(long)]
        operation_id: Option<String>,
        #[arg(long, default_value_t = bcode_ipc::DEFAULT_WORKFLOW_COMPUTATION_TIMEOUT_MS)]
        timeout_ms: u64,
    },
    /// Import one portable bundle as a new logical workflow and initial draft.
    Import {
        #[arg(value_name = "FILE", default_value = "-")]
        file: PathBuf,
        #[arg(long)]
        target_workflow_id: String,
        #[arg(long)]
        draft_id: String,
        #[arg(long)]
        operation_id: Option<String>,
        #[arg(long, default_value_t = bcode_ipc::DEFAULT_WORKFLOW_COMPUTATION_TIMEOUT_MS)]
        timeout_ms: u64,
    },
    /// Import one portable bundle as a new draft in an existing workflow.
    ImportDraft {
        #[arg(value_name = "FILE", default_value = "-")]
        file: PathBuf,
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        draft_id: String,
        #[arg(long)]
        operation_id: Option<String>,
        #[arg(long, default_value_t = bcode_ipc::DEFAULT_WORKFLOW_COMPUTATION_TIMEOUT_MS)]
        timeout_ms: u64,
    },
    /// Import one portable bundle as the exact next immutable revision of an existing workflow.
    ImportRevision {
        #[arg(value_name = "FILE", default_value = "-")]
        file: PathBuf,
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        activate: bool,
        #[arg(long)]
        expected_active_revision: Option<u64>,
        #[arg(long)]
        operation_id: Option<String>,
        #[arg(long, default_value_t = bcode_ipc::DEFAULT_WORKFLOW_COMPUTATION_TIMEOUT_MS)]
        timeout_ms: u64,
    },
    /// Print the portable authoring catalog as JSON.
    Catalog,
    /// Validate one authoring document from JSON, YAML, or TOML (stdin is `-`).
    Validate {
        #[arg(value_name = "FILE", default_value = "-")]
        file: PathBuf,
        /// Explicit source format for stdin or to override the file extension.
        #[arg(long, value_parser = ["json", "yaml", "yml", "toml"])]
        source_format: Option<String>,
        #[arg(long)]
        operation_id: Option<String>,
        #[arg(long, default_value_t = bcode_ipc::DEFAULT_WORKFLOW_COMPUTATION_TIMEOUT_MS)]
        timeout_ms: u64,
    },
    /// Compile and preview one authoring document without persistence or dispatch.
    Preview {
        #[arg(value_name = "FILE", default_value = "-")]
        file: PathBuf,
        /// Explicit source format for stdin or to override the file extension.
        #[arg(long, value_parser = ["json", "yaml", "yml", "toml"])]
        source_format: Option<String>,
        /// Optional runtime configuration JSON file or stdin (`-`).
        #[arg(long, value_name = "FILE")]
        configuration: Option<PathBuf>,
        #[arg(long)]
        operation_id: Option<String>,
        #[arg(long, default_value_t = bcode_ipc::DEFAULT_WORKFLOW_COMPUTATION_TIMEOUT_MS)]
        timeout_ms: u64,
    },
}

#[derive(Debug, Subcommand)]
enum WorkflowDraftCommand {
    /// List one bounded keyset page of drafts.
    List {
        #[arg(long)]
        workflow_id: String,
        #[arg(long, requires = "cursor_draft_id")]
        cursor_updated_at_ms: Option<u64>,
        #[arg(long, requires = "cursor_updated_at_ms")]
        cursor_draft_id: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Get one exact mutable draft.
    Get {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        draft_id: String,
    },
}

#[derive(Debug, Subcommand)]
enum WorkflowRevisionCommand {
    /// List one bounded keyset page of immutable revisions.
    List {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        before_revision: Option<u64>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Inspect current requirement availability separately from immutable revision facts.
    Inspect {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        revision: u64,
    },
    /// Get one exact immutable revision.
    Get {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        revision: u64,
    },
}

#[derive(Debug, Subcommand)]
enum WorkflowPresetCommand {
    /// List one bounded keyset page of presets.
    List {
        #[arg(long)]
        workflow_id: String,
        #[arg(long, requires = "cursor_preset_id")]
        cursor_updated_at_ms: Option<u64>,
        #[arg(long, requires = "cursor_updated_at_ms")]
        cursor_preset_id: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Get one exact revision-bound preset.
    Get {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        preset_id: String,
    },
    /// Create one generation-1 preset from a JSON mutation payload.
    Create {
        #[arg(value_name = "FILE", default_value = "-")]
        file: PathBuf,
    },
    /// Replace one exact preset generation from a JSON mutation payload.
    Update {
        #[arg(value_name = "FILE", default_value = "-")]
        file: PathBuf,
        #[arg(long)]
        expected_generation: u64,
    },
    /// Delete one exact preset generation.
    Delete {
        #[arg(long)]
        workflow_id: String,
        #[arg(long)]
        preset_id: String,
        #[arg(long)]
        expected_generation: u64,
    },
}

#[derive(Debug, Subcommand)]
enum RuntimeWorkCommand {
    List {
        session_id: SessionId,
        #[arg(long)]
        json: bool,
    },
    Cancel {
        session_id: SessionId,
        work_id: String,
        #[arg(long)]
        json: bool,
    },
    History {
        session_id: SessionId,
        /// Event budget clamped by the daemon to 1–1000; zero is not unlimited.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    Watch {
        session_id: SessionId,
        /// Emit one JSON object per line.
        #[arg(long)]
        json: bool,
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
    /// Measure one verified connection using the normal client availability policy.
    #[command(hide = true)]
    StartupProbe,
    Metrics {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        report: bool,
    },
    Diagnose {
        #[arg(long)]
        json: bool,
    },
    Stop {
        /// Force termination if graceful shutdown does not complete.
        #[arg(long)]
        force: bool,
        /// Confirm forceful daemon termination.
        #[arg(long, requires = "force")]
        yes: bool,
    },
    Cleanup,
    StopAll {
        /// Confirm stopping all registered daemons.
        #[arg(long)]
        yes: bool,
    },
    /// Gracefully stop every live daemon whose storage writer epoch is incompatible.
    RetireIncompatible {
        /// Confirm retiring every verified incompatible daemon.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum StateCommand {
    /// List configured state locations, their roles, and their availability.
    ///
    /// Read-only: this resolves configuration and probes availability without creating,
    /// migrating, or repairing anything.
    Locations {
        /// Print the inventory as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Inventory staging directories left behind by interrupted relocations.
    ///
    /// Without `--apply` this is a non-mutating inventory. Staging still held by a running
    /// relocation is reported as live and never pruned.
    PruneStaging {
        /// State root whose sessions root should be scanned. Defaults to the current location.
        #[arg(long = "root", value_name = "ROOT")]
        root: Option<PathBuf>,
        /// Remove abandoned staging directories. Without this flag, nothing is deleted.
        #[arg(long)]
        apply: bool,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, clap::Args)]
struct ArtifactRangeArgs {
    session_id: SessionId,
    artifact_id: String,
    reference_key: String,
    /// Starting byte offset within the referenced artifact.
    #[arg(long, default_value_t = 0)]
    offset: u64,
    /// Maximum bytes to read (1 through the shared artifact-range limit).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=i64::from(bcode_session_models::MAX_SESSION_ARTIFACT_RANGE_BYTES)))]
    length: u32,
    /// Write only returned bytes, without a newline or metadata. Empty output may mean EOF.
    #[arg(long)]
    raw: bool,
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// Measure session database, artifact, and other file bytes without replaying history (JSON).
    StorageUsage {
        session_id: SessionId,
        /// Maximum directory entries visited; partial results are explicitly marked.
        #[arg(long, default_value_t = 10_000, value_parser = clap::value_parser!(u32).range(1..=100_000))]
        entry_budget: u32,
    },
    /// Read a bounded artifact byte range as JSON, including reference and availability metadata.
    ArtifactRange(ArtifactRangeArgs),
    /// List agent profiles available from the daemon for session selection.
    Agents {
        #[arg(long)]
        json: bool,
    },
    /// List available skills and discovery diagnostics from the daemon.
    Skills {
        #[arg(long)]
        json: bool,
    },
    /// Inspect the daemon's manifest for one available skill.
    DescribeSkill {
        skill_id: String,
        #[arg(long)]
        json: bool,
    },
    Create {
        name: Option<String>,
        /// Create the session in this working directory instead of the current directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Print the created session summary as JSON.
        #[arg(long)]
        json: bool,
    },
    List {
        /// Discover sessions for this working directory instead of the current directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Include catalog health, source statuses, and revision in the JSON result.
        #[arg(long, requires = "json")]
        with_status: bool,
        /// Print the session summaries as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Explicitly refresh catalog discovery; the returned snapshot may still be loading.
    Refresh {
        /// Discovery directory; defaults to the caller's working directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Refresh only these source IDs (repeatable); omit to refresh all sources.
        #[arg(long = "source")]
        sources: Vec<String>,
        /// Emit a versioned snapshot including catalog health and source statuses.
        #[arg(long)]
        json: bool,
    },
    Rename {
        session_id: SessionId,
        name: String,
        /// Print the renamed session summary as JSON.
        #[arg(long)]
        json: bool,
    },
    Delete {
        session_id: SessionId,
        /// Confirm permanent session deletion.
        #[arg(long)]
        yes: bool,
        /// Print the deleted session summary as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Change the canonical working directory for one session.
    SetWorkingDirectory {
        session_id: SessionId,
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Set the active agent profile for one session.
    SetAgent {
        session_id: SessionId,
        agent_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Set the model selection for one session.
    SetModel {
        session_id: SessionId,
        model_id: String,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Set provider-neutral reasoning selections for one session.
    SetReasoning {
        session_id: SessionId,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long)]
        summary: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Set or clear the interactive preferred profile for an auth pool.
    SetAuthPool {
        pool: String,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        clear: bool,
        #[arg(long)]
        json: bool,
    },
    /// Return active skill contexts for one session.
    ActiveSkills {
        session_id: SessionId,
        #[arg(long)]
        json: bool,
    },
    /// Invoke one skill for a model turn.
    InvokeSkill {
        session_id: SessionId,
        skill_id: String,
        /// Optional skill arguments passed verbatim to the skill runtime.
        #[arg(default_value = "")]
        arguments: String,
        #[arg(long)]
        json: bool,
    },
    /// Activate one skill for a session.
    ActivateSkill {
        session_id: SessionId,
        skill_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Deactivate one skill for a session.
    DeactivateSkill {
        session_id: SessionId,
        skill_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Explicitly compact one session's model context.
    Compact {
        session_id: SessionId,
        #[arg(long)]
        json: bool,
    },
    /// Watch bounded initial state followed by ordered durable/live session events.
    Watch {
        session_id: SessionId,
        /// Maximum initial durable events.
        #[arg(long, default_value_t = SESSION_CLI_PAGE_LIMIT)]
        limit: usize,
        /// Emit one JSON object per line.
        #[arg(long)]
        json: bool,
    },
    History {
        session_id: SessionId,
        /// Return events starting at this canonical sequence.
        #[arg(long)]
        after: Option<u64>,
        /// Return the newest events at or before this canonical sequence.
        #[arg(long)]
        before: Option<u64>,
        /// Maximum events to return from this bounded read.
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Print the bounded page as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Return bounded canonical context around one event sequence.
    Around {
        session_id: SessionId,
        sequence: u64,
        #[arg(long, default_value_t = 20)]
        before: usize,
        #[arg(long, default_value_t = 20)]
        after: usize,
        /// Print the complete window, including coverage metadata, as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Inspect one high-value semantic event category with bounded canonical work.
    Inspect {
        session_id: SessionId,
        #[arg(value_enum)]
        category: SessionInspectionCategoryArg,
        #[arg(long)]
        after: Option<u64>,
        #[arg(long)]
        before: Option<u64>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Print the structured page and coverage metadata as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Search optional derived providers without opening canonical session storage.
    Search {
        query: String,
        /// Match semantics requested from eligible providers.
        #[arg(long = "match", value_enum, default_value_t = SessionSearchMatchArg::Terms)]
        match_mode: SessionSearchMatchArg,
        /// Restrict matching to stable semantic record fields.
        #[arg(long = "field", value_enum)]
        fields: Vec<SessionSearchFieldArg>,
        /// Restrict search to semantic content categories.
        #[arg(long = "content", value_enum)]
        content: Vec<SessionSearchContentArg>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Overall provider deadline in milliseconds.
        #[arg(long, default_value_t = 5_000)]
        deadline_ms: u64,
        /// Hydrate exact canonical event locators through bounded daemon reads.
        #[arg(long)]
        hydrate: bool,
        /// Permit providers that perform explicit cold/deep scans.
        #[arg(long)]
        deep: bool,
        /// Restrict search to canonical session IDs.
        #[arg(long = "session")]
        sessions: Vec<SessionId>,
        /// Restrict search to an exact normalized working directory.
        #[arg(long)]
        working_directory: Option<PathBuf>,
        /// Restrict search to records at or after this Unix timestamp in milliseconds.
        #[arg(long)]
        after_timestamp_ms: Option<u64>,
        /// Restrict search to records at or before this Unix timestamp in milliseconds.
        #[arg(long)]
        before_timestamp_ms: Option<u64>,
        /// Restrict search to normalized tool names.
        #[arg(long = "tool")]
        tools: Vec<String>,
        /// Restrict search to normalized tool statuses.
        #[arg(long = "tool-status")]
        tool_statuses: Vec<String>,
        /// Restrict search to model-provider IDs recorded in projected content.
        #[arg(long = "provider")]
        providers: Vec<String>,
        /// Restrict search to model IDs recorded in projected content.
        #[arg(long = "model")]
        models: Vec<String>,
        /// Restrict search to agent IDs recorded in projected content.
        #[arg(long = "agent")]
        agents: Vec<String>,
        /// Restrict search to import source IDs.
        #[arg(long = "import-source")]
        import_sources: Vec<String>,
        /// Print hits, provider coverage, and failures as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Inventory compatibility without mutating canonical sessions.
    MigrateInventory {
        /// Restrict inventory to canonical session IDs.
        #[arg(long = "session")]
        sessions: Vec<SessionId>,
        #[arg(long)]
        after_timestamp_ms: Option<u64>,
        #[arg(long)]
        before_timestamp_ms: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Start explicit bulk canonical migration.
    MigrateStart {
        /// Restrict migration to canonical session IDs.
        #[arg(long = "session")]
        sessions: Vec<SessionId>,
        #[arg(long)]
        after_timestamp_ms: Option<u64>,
        #[arg(long)]
        before_timestamp_ms: Option<u64>,
        /// Exact destructive-operation confirmation token.
        #[arg(long)]
        confirm: String,
        /// Wait until the transient aggregate operation terminates.
        #[arg(long)]
        foreground: bool,
        #[arg(long)]
        json: bool,
    },
    /// Read transient aggregate bulk migration status.
    MigrateStatus {
        operation_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Wait for newer transient aggregate bulk migration status.
    MigrateWait {
        operation_id: String,
        #[arg(long, default_value_t = 0)]
        after_revision: u64,
        #[arg(long, default_value_t = 30_000)]
        timeout_ms: u64,
        #[arg(long)]
        json: bool,
    },
    /// Request cooperative cancellation between per-session migrations.
    MigrateCancel {
        operation_id: String,
        #[arg(long)]
        json: bool,
    },
    /// List discovered provider capabilities, versions, quota, and coverage.
    SearchStatus {
        #[arg(long)]
        json: bool,
    },
    /// Explicitly purge one provider's disposable derived search state.
    SearchPurge {
        #[arg(long)]
        provider: String,
        /// Exact provider-defined confirmation token.
        #[arg(long)]
        confirm: String,
        #[arg(long)]
        json: bool,
    },
    /// Explicitly recreate one provider's empty derived search state.
    SearchRebuild {
        #[arg(long)]
        provider: String,
        /// Exact provider-defined confirmation token.
        #[arg(long)]
        confirm: String,
        #[arg(long)]
        json: bool,
    },
    /// Start an addressable complete historical backfill operation.
    SearchBackfillStart {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long = "session")]
        sessions: Vec<SessionId>,
        #[arg(long)]
        after_timestamp_ms: Option<u64>,
        #[arg(long)]
        before_timestamp_ms: Option<u64>,
        /// Unsupported legacy option; complete backfill does not accept a catalog cursor.
        #[arg(long, value_parser = parse_session_search_backfill_cursor)]
        cursor: Option<bcode_session_search::SessionSearchBackfillCursor>,
        /// Wall-clock budget per backfill slice, not a total operation timeout.
        #[arg(long, default_value_t = 30_000)]
        deadline_ms: u64,
        #[arg(long)]
        json: bool,
    },
    /// Read status for an addressable historical backfill operation.
    SearchBackfillStatus {
        operation_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Wait for a newer operation revision or timeout.
    SearchBackfillWait {
        operation_id: String,
        #[arg(long, default_value_t = 0)]
        after_revision: u64,
        #[arg(long, default_value_t = 30_000)]
        timeout_ms: u64,
        #[arg(long)]
        json: bool,
    },
    /// Request cancellation of an addressable historical backfill operation.
    SearchBackfillCancel {
        operation_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Explicitly backfill the complete selected catalog into all enabled or one scoped provider.
    SearchBackfill {
        #[arg(long)]
        provider: Option<String>,
        /// Restrict maintenance to selected canonical session IDs.
        #[arg(long = "session")]
        sessions: Vec<SessionId>,
        /// Select catalog sessions updated at or after this Unix timestamp in milliseconds.
        #[arg(long)]
        after_timestamp_ms: Option<u64>,
        /// Select catalog sessions updated at or before this Unix timestamp in milliseconds.
        #[arg(long)]
        before_timestamp_ms: Option<u64>,
        /// Unsupported legacy option; complete backfill does not accept a catalog cursor.
        #[arg(long, value_parser = parse_session_search_backfill_cursor)]
        cursor: Option<bcode_session_search::SessionSearchBackfillCursor>,
        /// Wall-clock budget per backfill slice, not a total operation timeout.
        #[arg(long, default_value_t = 30_000)]
        deadline_ms: u64,
        #[arg(long)]
        json: bool,
    },
    /// Explain provider selection for a query without invoking provider searches.
    SearchExplain {
        query: String,
        /// Match semantics requested from eligible providers.
        #[arg(long = "match", value_enum, default_value_t = SessionSearchMatchArg::Terms)]
        match_mode: SessionSearchMatchArg,
        /// Restrict matching to stable semantic record fields.
        #[arg(long = "field", value_enum)]
        fields: Vec<SessionSearchFieldArg>,
        #[arg(long = "content", value_enum)]
        content: Vec<SessionSearchContentArg>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = 5_000)]
        deadline_ms: u64,
        /// Explain inclusion of cold/deep scan providers.
        #[arg(long)]
        deep: bool,
        /// Restrict search to canonical session IDs.
        #[arg(long = "session")]
        sessions: Vec<SessionId>,
        /// Restrict search to an exact normalized working directory.
        #[arg(long)]
        working_directory: Option<PathBuf>,
        /// Restrict search to records at or after this Unix timestamp in milliseconds.
        #[arg(long)]
        after_timestamp_ms: Option<u64>,
        /// Restrict search to records at or before this Unix timestamp in milliseconds.
        #[arg(long)]
        before_timestamp_ms: Option<u64>,
        /// Restrict search to normalized tool names.
        #[arg(long = "tool")]
        tools: Vec<String>,
        /// Restrict search to normalized tool statuses.
        #[arg(long = "tool-status")]
        tool_statuses: Vec<String>,
        /// Restrict search to model-provider IDs recorded in projected content.
        #[arg(long = "provider")]
        providers: Vec<String>,
        /// Restrict search to model IDs recorded in projected content.
        #[arg(long = "model")]
        models: Vec<String>,
        /// Restrict search to agent IDs recorded in projected content.
        #[arg(long = "agent")]
        agents: Vec<String>,
        /// Restrict search to import source IDs.
        #[arg(long = "import-source")]
        import_sources: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Export complete canonical events through the daemon-owned strict history boundary.
    Export {
        session_id: SessionId,
        #[arg(long, value_enum, default_value_t = SessionExportFormat::Jsonl)]
        format: SessionExportFormat,
    },
    Timeline {
        session_id: SessionId,
    },
    /// Report writer, migration, projection, ownership, and recovery state without mutation.
    Diagnose {
        session_id: SessionId,
        #[arg(long)]
        json: bool,
    },
    /// Inspect database/WAL health without mutation; use repair or reindex explicitly afterward.
    Doctor {
        session_id: Option<SessionId>,
        #[arg(long)]
        catalog: bool,
        #[arg(long)]
        scan: bool,
        #[arg(long)]
        json: bool,
    },
    RetiredCatalogs {
        /// Apply cleanup. Without this flag, the command is a non-mutating inventory.
        #[arg(long)]
        apply: bool,
        /// Print the inventory/cleanup report as JSON.
        #[arg(long)]
        json: bool,
    },
    Repair {
        session_id: Option<SessionId>,
        #[arg(long)]
        catalog: bool,
        #[arg(long)]
        scan: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Recalculate derived cost for a request timestamp range from a JSON catalog snapshot.
    Reprice {
        session_id: SessionId,
        /// Inclusive RFC 3339 datetime (with timezone).
        #[arg(long = "from", value_parser = parse_reprice_datetime)]
        from_ms: u64,
        /// Exclusive RFC 3339 datetime (with timezone).
        #[arg(long = "to", value_parser = parse_reprice_datetime)]
        to_ms: u64,
        /// Explicit model catalog snapshot (`CatalogDocument` JSON).
        #[arg(long)]
        catalog: PathBuf,
    },
    /// Rebuild model-context and transcript indexes from canonical history.
    Reindex {
        session_id: SessionId,
    },
    /// Move canonical session storage to another configured state location.
    ///
    /// Relocation is an explicit maintenance operation: it refuses live owners, verifies every
    /// copied byte before publishing, and only unlinks the source after a verified publish, so an
    /// interruption always leaves the source authoritative.
    Relocate {
        /// Destination state root, or a `[state] profile` name via --to-profile.
        #[arg(long = "to", value_name = "ROOT", conflicts_with = "to_profile")]
        to: Option<PathBuf>,
        /// Destination state profile name from `[state] profile`.
        #[arg(long = "to-profile", value_name = "PROFILE")]
        to_profile: Option<String>,
        /// Sessions to relocate. Omit to consider every session in the source location.
        #[arg(long = "session", value_name = "SESSION_ID")]
        session: Vec<SessionId>,
        /// Report the plan without copying, publishing, or deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Print the plan or report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Ask the verified daemon owning a session to release ownership when quiescent.
    ReleaseOwner {
        session_id: SessionId,
    },
    /// Gracefully stop the verified daemon owning a session.
    StopOwner {
        session_id: SessionId,
    },
    /// Forcefully terminate the verified daemon owning a session.
    KillOwner {
        session_id: SessionId,
        /// Skip the destructive-action confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    Import {
        #[command(subcommand)]
        command: SessionImportCommand,
    },
}

#[derive(Debug, Clone, Subcommand)]
enum SessionImportCommand {
    Sources {
        #[arg(long)]
        json: bool,
    },
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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SessionSearchMatchArg {
    Terms,
    Phrase,
    Prefix,
    Regex,
    Fuzzy,
}

impl From<SessionSearchMatchArg> for bcode_session_search::TextMatchMode {
    fn from(value: SessionSearchMatchArg) -> Self {
        match value {
            SessionSearchMatchArg::Terms => Self::Terms,
            SessionSearchMatchArg::Phrase => Self::Phrase,
            SessionSearchMatchArg::Prefix => Self::Prefix,
            SessionSearchMatchArg::Regex => Self::Regex,
            SessionSearchMatchArg::Fuzzy => Self::Fuzzy,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SessionSearchFieldArg {
    Title,
    Text,
    Command,
    StandardOutput,
    StandardError,
    ToolName,
    ToolArguments,
    ErrorMessage,
    WorkingDirectory,
    Provider,
    Model,
    Agent,
    Source,
}

impl From<SessionSearchFieldArg> for bcode_session_search::SearchField {
    fn from(value: SessionSearchFieldArg) -> Self {
        match value {
            SessionSearchFieldArg::Title => Self::Title,
            SessionSearchFieldArg::Text => Self::Text,
            SessionSearchFieldArg::Command => Self::Command,
            SessionSearchFieldArg::StandardOutput => Self::StandardOutput,
            SessionSearchFieldArg::StandardError => Self::StandardError,
            SessionSearchFieldArg::ToolName => Self::ToolName,
            SessionSearchFieldArg::ToolArguments => Self::ToolArguments,
            SessionSearchFieldArg::ErrorMessage => Self::ErrorMessage,
            SessionSearchFieldArg::WorkingDirectory => Self::WorkingDirectory,
            SessionSearchFieldArg::Provider => Self::Provider,
            SessionSearchFieldArg::Model => Self::Model,
            SessionSearchFieldArg::Agent => Self::Agent,
            SessionSearchFieldArg::Source => Self::Source,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SessionSearchContentArg {
    SessionTitle,
    UserMessage,
    AssistantMessage,
    AssistantReasoning,
    SystemMessage,
    ShellCommand,
    ShellOutput,
    ToolArguments,
    ToolOutput,
    ToolError,
    Permission,
    RuntimeDiagnostic,
    Compaction,
    TraceMetadata,
    ArtifactMetadata,
}

impl From<SessionSearchContentArg> for bcode_session_search::SearchContentKind {
    fn from(value: SessionSearchContentArg) -> Self {
        match value {
            SessionSearchContentArg::SessionTitle => Self::SessionTitle,
            SessionSearchContentArg::UserMessage => Self::UserMessage,
            SessionSearchContentArg::AssistantMessage => Self::AssistantMessage,
            SessionSearchContentArg::AssistantReasoning => Self::AssistantReasoning,
            SessionSearchContentArg::SystemMessage => Self::SystemMessage,
            SessionSearchContentArg::ShellCommand => Self::ShellCommand,
            SessionSearchContentArg::ShellOutput => Self::ShellOutput,
            SessionSearchContentArg::ToolArguments => Self::ToolArguments,
            SessionSearchContentArg::ToolOutput => Self::ToolOutput,
            SessionSearchContentArg::ToolError => Self::ToolError,
            SessionSearchContentArg::Permission => Self::Permission,
            SessionSearchContentArg::RuntimeDiagnostic => Self::RuntimeDiagnostic,
            SessionSearchContentArg::Compaction => Self::Compaction,
            SessionSearchContentArg::TraceMetadata => Self::TraceMetadata,
            SessionSearchContentArg::ArtifactMetadata => Self::ArtifactMetadata,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SessionInspectionCategoryArg {
    FailedToolCalls,
    Permissions,
    SelectionChanges,
    RuntimeWork,
    Compactions,
    TerminalOutcomes,
}

impl From<SessionInspectionCategoryArg> for SessionInspectionCategory {
    fn from(value: SessionInspectionCategoryArg) -> Self {
        match value {
            SessionInspectionCategoryArg::FailedToolCalls => Self::FailedToolCalls,
            SessionInspectionCategoryArg::Permissions => Self::Permissions,
            SessionInspectionCategoryArg::SelectionChanges => Self::SelectionChanges,
            SessionInspectionCategoryArg::RuntimeWork => Self::RuntimeWork,
            SessionInspectionCategoryArg::Compactions => Self::Compactions,
            SessionInspectionCategoryArg::TerminalOutcomes => Self::TerminalOutcomes,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SessionExportFormat {
    Jsonl,
}

#[derive(Debug, Subcommand)]
enum ModelCommand {
    List {
        /// Print raw JSON including context metadata.
        #[arg(long)]
        json: bool,
        /// Provider plugin id to query.
        #[arg(long)]
        provider: Option<String>,
    },
    Status {
        /// Session id to inspect. Defaults to the draft/default model status.
        session_id: Option<SessionId>,
        /// Print raw JSON.
        #[arg(long)]
        json: bool,
    },
    Capabilities {
        /// Print the complete normalized capability contract as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Inspect normalized embedded/remote model-catalog state.
    Diagnostics {
        /// Print the complete diagnostics contract as JSON.
        #[arg(long)]
        json: bool,
    },
    Validate {
        /// Print the normalized validation result as JSON.
        #[arg(long)]
        json: bool,
    },
    Ignore {
        model_id: String,
        #[arg(long)]
        provider: Option<String>,
        /// Print the mutation receipt as JSON.
        #[arg(long)]
        json: bool,
    },
    Unignore {
        model_id: String,
        #[arg(long)]
        provider: Option<String>,
        /// Print the mutation receipt as JSON.
        #[arg(long)]
        json: bool,
    },
    Ignored {
        #[arg(long)]
        provider: Option<String>,
        /// Print provider-keyed ignore rules as JSON.
        #[arg(long)]
        json: bool,
    },
    Verify {
        /// Prompt sent to each model.
        #[arg(long, default_value = "say ok")]
        prompt: String,
        /// Maximum number of models to verify after filtering.
        #[arg(long)]
        max_models: Option<usize>,
        /// Model id wildcard filter. Supports `*` globs.
        #[arg(long)]
        id_pattern: Option<String>,
        /// Print candidate models without sending verification requests.
        #[arg(long)]
        dry_run: bool,
        /// Output JSON report path.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Request timeout in seconds.
        #[arg(long, default_value_t = 20)]
        timeout_seconds: u64,
    },
    /// Verify prompt-cache behavior against the configured provider's live models.
    ///
    /// Runs the capability-derived prompt-cache scenario suite (cold/warm requests, growing
    /// conversation, tool loop, TTL matrix, mode off, budget overflow) and fails when any
    /// applicable scenario fails. Models that do not advertise caching are skipped.
    VerifyCache(VerifyCacheArgs),
    /// Verify image context through inline replay and optionally provider continuation.
    VerifyImages(VerifyImagesArgs),
    Set {
        session_id: SessionId,
        model_id: String,
        #[arg(long)]
        provider: Option<String>,
        /// Print the session operation result as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Arguments for bounded, explicitly requested image verification.
#[derive(Debug, clap::Args)]
struct VerifyImagesArgs {
    /// Generate two nonsensitive, order-sensitive images from a reproducible seed.
    #[arg(long, conflicts_with_all = ["image", "question", "expected_answer"])]
    generated_seed: Option<u64>,
    /// Ordered PNG, JPEG, GIF, or WebP fixtures; repeat --image (maximum eight).
    #[arg(long)]
    image: Vec<PathBuf>,
    /// Question about the image; the answer must not appear in this question.
    #[arg(long)]
    question: Option<String>,
    /// Exact expected answer, kept out of model requests and reports.
    #[arg(long)]
    expected_answer: Option<String>,
    /// Probe an image in a correlated tool result instead of a user message.
    #[arg(long)]
    tool_result: bool,
    /// Authorize provider-side conversation storage for the continuation probe.
    #[arg(long)]
    allow_conversation_storage: bool,
    /// Discover candidates without sending image requests.
    #[arg(long)]
    dry_run: bool,
    /// Model id wildcard filter.
    #[arg(long)]
    id_pattern: Option<String>,
    /// Maximum models to probe (at most five turns per model).
    #[arg(long, default_value_t = 1)]
    max_models: usize,
    /// Per-turn timeout; provider operations also need configured network timeouts.
    #[arg(long, default_value_t = 60)]
    timeout_seconds: u64,
}

/// Arguments for `bcode model verify-cache`.
#[derive(Debug, clap::Args)]
struct VerifyCacheArgs {
    /// Maximum number of models to verify after filtering.
    #[arg(long)]
    max_models: Option<usize>,
    /// Model id wildcard filter. Supports `*` globs.
    #[arg(long)]
    id_pattern: Option<String>,
    /// Print candidate models without sending verification requests.
    #[arg(long)]
    dry_run: bool,
    /// Number of tool rounds in the tool-loop scenario.
    #[arg(long, default_value_t = 6)]
    tool_rounds: usize,
    /// Number of appended exchanges in the growing-conversation scenario.
    #[arg(long, default_value_t = 4)]
    conversation_turns: usize,
    /// Override the minimum cacheable prefix when the catalog does not declare one.
    #[arg(long)]
    min_prefix_tokens: Option<u64>,
    /// Print the complete JSON report instead of a summary table.
    #[arg(long)]
    json: bool,
    /// Output JSON report path.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Per-turn timeout in seconds.
    #[arg(long, default_value_t = 120)]
    timeout_seconds: u64,
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// Discover provider-declared external API keys without displaying their values.
    Discover,
    /// Review and copy one provider-declared external API key into an owned sshenv profile.
    Import {
        /// Registered provider ID.
        provider: String,
        /// Registered secret-field authentication method.
        #[arg(long, default_value = "api_key")]
        method: String,
        /// Canonical credential field.
        #[arg(long, default_value = "api_key")]
        credential: String,
        /// Source index shown by `auth discover` (explicit import works with discovery disabled).
        #[arg(long)]
        source: usize,
        /// Destination profile; defaults to the provider's selected profile.
        #[arg(long)]
        profile: Option<String>,
        /// Destination sshenv vault.
        #[arg(long)]
        vault: Option<PathBuf>,
    },
    /// List authentication providers registered by enabled plugins.
    Providers,
    Status {
        /// Optional provider ID. Omit to retain legacy all-profile status.
        provider: Option<String>,
        /// Explicit auth profile.
        #[arg(long)]
        profile: Option<String>,
    },
    Profile {
        #[command(subcommand)]
        command: AuthProfileCommand,
    },
    Pool {
        #[command(subcommand)]
        command: AuthPoolCommand,
    },
    Prime {
        #[command(subcommand)]
        command: AuthPrimeCommand,
    },
    Resets {
        #[command(subcommand)]
        command: AuthResetsCommand,
    },
    Usage {
        #[command(subcommand)]
        command: AuthUsageCommand,
    },
    /// Inspect the selected profile's vault security as JSON without exposing credentials.
    Security {
        /// Optional provider ID. Omit to inspect a declared profile directly.
        #[arg(long)]
        provider: Option<String>,
        /// Explicit auth profile name.
        #[arg(long)]
        profile: String,
        /// Explicit vault path. Defaults to the resolved profile vault.
        #[arg(long)]
        vault: Option<PathBuf>,
        /// Require this device-seal backend for the inspection result.
        #[arg(long)]
        require_backend: Option<String>,
    },
    Login {
        /// Optional provider ID. Omit to retain active-profile enrollment compatibility.
        provider: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        vault: Option<PathBuf>,
        #[arg(long)]
        recipient_key: Option<String>,
        /// Do not bind newly saved credentials to this device.
        #[arg(long)]
        no_device_seal: bool,
        /// Add the enrolled profile to this runtime authentication pool without changing the provider binding.
        #[arg(long)]
        pool: Option<String>,
        /// Explicit provider-local authentication method ID.
        #[arg(long)]
        method: Option<String>,
        /// Explicitly ask a supporting provider to verify enrolled credentials.
        #[arg(long)]
        verify: bool,
    },
    /// Delete locally stored credentials for one dynamically registered provider.
    Logout {
        provider: String,
        #[arg(long)]
        profile: Option<String>,
        /// Explicitly ask a supporting provider to revoke credentials remotely first.
        #[arg(long)]
        revoke: bool,
    },
}

#[derive(Debug, Subcommand)]
enum AuthProfileCommand {
    List,
    Show { profile: String },
}

#[derive(Debug, Subcommand)]
enum AuthPoolCommand {
    List,
    Profiles {
        #[arg(default_value = "openai")]
        pool: String,
    },
    Status {
        #[arg(default_value = "openai")]
        pool: String,
    },
    ResetCooldown {
        #[arg(default_value = "openai")]
        pool: String,
        profile: Option<String>,
    },
    /// Move one profile to the front of the pool using interactive user state.
    Promote {
        pool: String,
        profile: String,
    },
    /// Clear the interactive preferred-profile override.
    ClearPreference {
        pool: String,
    },
}

#[derive(Debug, Subcommand)]
enum AuthUsageCommand {
    /// Report provider auth usage windows for a provider/auth pool.
    Status {
        #[arg(default_value = "openai")]
        pool: String,
        /// Only report one auth profile.
        #[arg(long)]
        profile: Option<String>,
        /// Exclude the primary auth profile.
        #[arg(long)]
        no_primary: bool,
        /// Refresh provider usage windows before reporting.
        #[arg(long)]
        refresh: bool,
        /// Print JSON output.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum AuthResetsCommand {
    /// Report banked rate-limit reset credits for a provider/auth pool.
    Status {
        #[arg(default_value = "openai")]
        pool: String,
        /// Only report one auth profile.
        #[arg(long)]
        profile: Option<String>,
        /// Exclude the primary auth profile.
        #[arg(long)]
        no_primary: bool,
        /// Print detailed provider fields.
        #[arg(long)]
        verbose: bool,
        /// Print JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Consume one banked rate-limit reset credit.
    Use {
        #[arg(default_value = "openai")]
        pool: String,
        /// Auth profile whose reset credit should be consumed.
        #[arg(long)]
        profile: Option<String>,
        /// Opaque reset credit id to consume. When omitted, the provider chooses one.
        #[arg(long)]
        credit: Option<String>,
        /// Show the request that would be sent without consuming a credit.
        #[arg(long)]
        dry_run: bool,
        /// Skip confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// Print JSON output.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum AuthPrimeCommand {
    /// Prime all subscription auth profiles in a provider/auth pool.
    Run {
        #[arg(default_value = "openai")]
        pool: String,
        /// Only prime one auth profile.
        #[arg(long)]
        profile: Option<String>,
        /// Exclude the primary auth profile.
        #[arg(long)]
        no_primary: bool,
        /// Deprecated alias retained for compatibility; primary is included by default.
        #[arg(long, hide = true)]
        include_primary: bool,
        /// Prime even when windows appear already active.
        #[arg(long)]
        force: bool,
        /// Show what would be primed without sending requests.
        #[arg(long)]
        dry_run: bool,
        /// Print JSON output.
        #[arg(long)]
        json: bool,
        /// Request timeout in seconds.
        #[arg(long, default_value_t = 20)]
        timeout_seconds: u64,
        /// Maximum priming attempts per profile before reporting a failure.
        #[arg(long, default_value_t = 100)]
        max_attempts: u64,
        /// Disable the maximum priming attempt cap.
        #[arg(long)]
        no_max_attempts: bool,
        /// Delay between repeated priming attempts in seconds.
        #[arg(long, default_value_t = 1)]
        delay_seconds: u64,
    },
    /// Report priming window status for a provider/auth pool.
    Status {
        #[arg(default_value = "openai")]
        pool: String,
        /// Only report one auth profile.
        #[arg(long)]
        profile: Option<String>,
        /// Exclude the primary auth profile.
        #[arg(long)]
        no_primary: bool,
        /// Deprecated alias retained for compatibility; primary is included by default.
        #[arg(long, hide = true)]
        include_primary: bool,
        /// Refresh provider usage windows before reporting.
        #[arg(long)]
        refresh: bool,
        /// Print JSON output.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum LoginCommand {
    /// Deprecated: use `bcode auth login openai` (for example, `--method chatgpt`).
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
        /// Add this login as another `ChatGPT` subscription in the runtime `OpenAI` failover pool.
        /// Use `--profile openai-2` to refresh an existing secondary subscription.
        #[arg(long)]
        add_subscription: bool,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        vault: Option<PathBuf>,
        #[arg(long)]
        recipient_key: Option<String>,
        /// Do not bind saved credentials to this device.
        #[arg(long)]
        no_device_seal: bool,
        #[arg(long)]
        model: Option<String>,
    },
    /// Deprecated: use `bcode auth login xai`.
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
        /// Do not bind saved credentials to this device.
        #[arg(long)]
        no_device_seal: bool,
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenAiLoginFlow {
    Browser,
    DeviceCode,
}

#[derive(Debug, Subcommand)]
enum PermissionCommand {
    /// Inspect the daemon's effective agent-policy summary and degradation diagnostics.
    Status {
        #[arg(long)]
        json: bool,
    },
    List {
        /// Restrict pending permissions to one canonical session.
        #[arg(long)]
        session_id: Option<SessionId>,
        /// Print complete structured permission summaries as JSON.
        #[arg(long)]
        json: bool,
    },
    Approve {
        permission_id: String,
        /// Persist the approved operation as a policy rule when supported.
        #[arg(long)]
        remember: bool,
        /// Print the resolution result as JSON.
        #[arg(long)]
        json: bool,
    },
    Deny {
        permission_id: String,
        /// Print the resolution result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Resolve every still-pending member of one canonical permission batch.
    ResolveBatch {
        batch_id: String,
        #[arg(long, conflicts_with = "deny", required_unless_present = "deny")]
        approve: bool,
        #[arg(long, conflicts_with = "approve", required_unless_present = "approve")]
        deny: bool,
        /// Print the resolution result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Add or replace a permission rule under `[agent.<agent_id>.permission.<category>]`.
    Add {
        /// Agent ID that owns the rule (for example `build` or `plan`).
        #[arg(long)]
        agent: String,
        /// Permission category: `command`, `read`, `write`, `edit`, or `web`.
        #[arg(long)]
        category: String,
        /// Glob pattern to match.
        #[arg(long)]
        pattern: String,
        /// Action: `allow`, `ask`, or `deny`.
        #[arg(long)]
        action: String,
        /// Print the resulting configuration path as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum InteractionCommand {
    /// Print one pending exchange as JSON, preserving its producer schema and payload.
    Inspect {
        /// Pending exchange identifier.
        exchange_id: String,
    },
    /// Enqueue schema-aware input for an active invocation; acceptance is not completion.
    Input {
        /// Canonical session owning the active invocation.
        session_id: SessionId,
        /// Complete `ToolInvocationInput` JSON envelope from file or stdin (`-`).
        /// Input is capped at 256 KiB; the daemon enforces a 64-KiB encoded envelope limit.
        #[arg(long, value_name = "FILE")]
        payload: PathBuf,
        /// Print a JSON acceptance receipt. This does not confer retry safety.
        #[arg(long)]
        json: bool,
    },
    /// List pending renderer-neutral tool exchanges.
    List {
        /// Restrict pending exchanges to one canonical session.
        #[arg(long)]
        session_id: Option<SessionId>,
        /// Print the complete structured exchange envelopes as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Respond to one exchange with producer-schema JSON from a file or stdin (`-`).
    Respond {
        exchange_id: String,
        /// JSON file or `-` for stdin. Input is capped at 256 KiB; the daemon separately
        /// limits the encoded resolution to 64 KiB, including envelope and escaping.
        #[arg(long, value_name = "FILE")]
        payload: PathBuf,
        /// Print the resolution result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Cancel one pending exchange.
    Cancel {
        exchange_id: String,
        /// Print the resolution result as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum WorktreeCommand {
    /// List registered worktrees for a repository discovered from `--cwd`.
    List {
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Create a worktree and optionally attach or create a canonical session.
    Create {
        name: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        path: Option<PathBuf>,
        #[arg(long, conflicts_with_all = ["new_branch", "detach"])]
        branch: Option<String>,
        #[arg(long, conflicts_with_all = ["branch", "detach"])]
        new_branch: Option<String>,
        #[arg(long, conflicts_with_all = ["branch", "new_branch"])]
        detach: bool,
        #[arg(long)]
        force: bool,
        #[arg(long, conflicts_with = "new_session")]
        attach_session_id: Option<SessionId>,
        #[arg(long, conflicts_with = "attach_session_id")]
        new_session: bool,
        #[arg(long)]
        no_setup: bool,
        #[arg(long)]
        json: bool,
    },
    /// Remove one registered worktree.
    Remove {
        path: PathBuf,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        force: bool,
        /// Required confirmation for worktree removal.
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum PluginCommand {
    /// Discover selected local plugin manifests without loading native libraries.
    List {
        #[arg(long = "root")]
        root: Vec<std::path::PathBuf>,
        /// Print structured plugin manifests as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Inspect local service manifests or query services registered by the running host.
    Services {
        #[arg(long = "root")]
        root: Vec<std::path::PathBuf>,
        /// Query the running host instead of local plugin roots.
        #[arg(long, conflicts_with = "root")]
        daemon: bool,
        /// Print structured service summaries as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Load selected native plugins and run their activation/deactivation callbacks.
    ///
    /// This executes plugin code; use `plugin list` for manifest-only discovery.
    Check {
        #[arg(long = "root")]
        root: Vec<std::path::PathBuf>,
        /// Print structured plugin check results as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Invoke a service on a specific plugin.
    ///
    /// A service error is printed in the response envelope and exits with status 1.
    /// Failure does not imply rollback or that retrying is safe.
    Invoke {
        #[arg(long = "root")]
        root: Vec<std::path::PathBuf>,
        /// Invoke on the running host instead of local plugin roots.
        #[arg(long, conflicts_with = "root")]
        daemon: bool,
        plugin_id: String,
        interface_id: String,
        operation: String,
        payload: Option<String>,
        /// Print the structured plugin response as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Call the loaded plugin that provides a service interface.
    ///
    /// A service error is printed in the response envelope and exits with status 1.
    /// Failure does not imply rollback or that retrying is safe.
    Call {
        #[arg(long = "root")]
        root: Vec<std::path::PathBuf>,
        /// Call on the running host instead of local plugin roots.
        #[arg(long, conflicts_with = "root")]
        daemon: bool,
        interface_id: String,
        operation: String,
        payload: Option<String>,
        /// Print the structured plugin response as JSON.
        #[arg(long)]
        json: bool,
    },
    Publish {
        #[arg(long = "root")]
        root: Vec<std::path::PathBuf>,
        /// Publish on the running host instead of local plugin roots.
        #[arg(long, conflicts_with = "root")]
        daemon: bool,
        topic: String,
        payload: Option<String>,
        /// Print the delivery count as JSON.
        #[arg(long)]
        json: bool,
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
        ServerCommand::StartupProbe => daemon_startup_probe().await?,
        ServerCommand::Metrics { json, report } => server_metrics(json, report).await?,
        ServerCommand::Diagnose { json } => server_diagnose(json).await?,
        ServerCommand::Stop { force, yes } => {
            if force && !yes {
                return Err(CliError::InvalidArguments(
                    "forced server stop requires --yes".to_owned(),
                ));
            }
            server_stop(force).await?;
        }
        ServerCommand::Cleanup => server_cleanup(false).await?,
        ServerCommand::StopAll { yes } => {
            if !yes {
                return Err(CliError::InvalidArguments(
                    "stopping all registered daemons requires --yes".to_owned(),
                ));
            }
            server_cleanup(true).await?;
        }
        ServerCommand::RetireIncompatible { yes } => {
            if !yes {
                return Err(CliError::InvalidArguments(
                    "retiring incompatible daemons requires --yes".to_owned(),
                ));
            }
            retire_incompatible_daemons().await?;
        }
    }
    Ok(())
}

fn write_skill_list(
    output: &mut impl std::io::Write,
    diagnostics: &mut impl std::io::Write,
    result: &bcode_skill_models::SkillList,
) -> Result<(), CliError> {
    for skill in &result.skills {
        writeln!(
            output,
            "{}\t{}\t{}",
            skill.id,
            skill.name,
            skill.description.as_deref().unwrap_or_default()
        )?;
    }
    output.flush()?;
    for diagnostic in &result.diagnostics {
        writeln!(
            diagnostics,
            "{:?}: {}",
            diagnostic.severity, diagnostic.message
        )?;
    }
    diagnostics.flush()?;
    Ok(())
}

fn write_artifact_range(
    output: &mut impl std::io::Write,
    range: &bcode_session_models::SessionArtifactRange,
    raw: bool,
) -> Result<(), CliError> {
    if raw {
        output.write_all(&range.bytes)?;
        output.flush()?;
        Ok(())
    } else {
        write_json_result(output, range)
    }
}

async fn read_artifact_range_to(
    client: &BcodeClient,
    args: ArtifactRangeArgs,
    output: &mut impl std::io::Write,
) -> Result<(), CliError> {
    let range = client
        .session_artifact_range(
            args.session_id,
            args.artifact_id,
            args.reference_key,
            args.offset,
            args.length,
        )
        .await?;
    write_artifact_range(output, &range, args.raw)
}

async fn describe_skill(skill_id: String, json: bool) -> Result<(), CliError> {
    ensure_server_running().await?;
    let skill = BcodeClient::default_endpoint()
        .describe_skill(bcode_skill_models::SkillId::new(skill_id))
        .await?;
    if json {
        print_json(&skill)?;
    } else {
        let mut output = std::io::stdout().lock();
        writeln!(output, "{} ({})", skill.summary.name, skill.summary.id)?;
        if let Some(description) = skill.summary.description {
            writeln!(output, "{description}")?;
        }
        writeln!(output, "{}", skill.instructions)?;
        output.flush()?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn handle_session_command(command: Box<SessionCommand>) -> Result<(), CliError> {
    match *command {
        SessionCommand::StorageUsage {
            session_id,
            entry_budget,
        } => {
            ensure_server_running().await?;
            let usage = BcodeClient::default_endpoint()
                .session_storage_usage(session_id, entry_budget)
                .await?;
            print_json(&usage)?;
        }
        SessionCommand::ArtifactRange(args) => {
            Box::pin(read_artifact_range_to(
                &BcodeClient::default_endpoint(),
                args,
                &mut std::io::stdout(),
            ))
            .await?;
        }
        SessionCommand::Agents { json } => {
            ensure_server_running().await?;
            let agents = BcodeClient::default_endpoint().list_agents().await?;
            if json {
                print_json(&agents)?;
            } else {
                let mut output = std::io::stdout().lock();
                for agent in agents {
                    writeln!(
                        output,
                        "{}\t{}\t{}",
                        agent.id, agent.name, agent.description
                    )?;
                }
                output.flush()?;
            }
        }
        SessionCommand::Skills { json } => {
            ensure_server_running().await?;
            let result = BcodeClient::default_endpoint().list_skills().await?;
            if json {
                print_json(&result)?;
            } else {
                write_skill_list(
                    &mut std::io::stdout().lock(),
                    &mut std::io::stderr().lock(),
                    &result,
                )?;
            }
        }
        SessionCommand::DescribeSkill { skill_id, json } => {
            Box::pin(describe_skill(skill_id, json)).await?;
        }
        SessionCommand::Create { name, cwd, json } => {
            Box::pin(create_session(name, cwd, json)).await?;
        }
        SessionCommand::List {
            cwd,
            with_status,
            json,
        } => Box::pin(list_sessions(cwd, with_status, json)).await?,
        SessionCommand::Refresh { cwd, sources, json } => {
            let directory = cwd.map_or_else(std::env::current_dir, std::path::absolute)?;
            let catalog = BcodeClient::default_endpoint()
                .refresh_session_catalog_in_working_directory(
                    directory,
                    (!sources.is_empty()).then_some(sources),
                )
                .await?;
            write_catalog_refresh(&mut std::io::stdout().lock(), &catalog, json)?;
        }
        SessionCommand::Rename {
            session_id,
            name,
            json,
        } => Box::pin(rename_session(session_id, name, json)).await?,
        SessionCommand::Delete {
            session_id,
            yes,
            json,
        } => Box::pin(delete_session(session_id, yes, json)).await?,
        SessionCommand::SetWorkingDirectory {
            session_id,
            path,
            json,
        } => Box::pin(set_session_working_directory(session_id, path, json)).await?,
        SessionCommand::SetAgent {
            session_id,
            agent_id,
            json,
        } => Box::pin(set_session_agent(session_id, agent_id, json)).await?,
        SessionCommand::SetModel {
            session_id,
            model_id,
            provider,
            json,
        } => {
            Box::pin(set_session_model_selection(
                session_id, provider, model_id, json,
            ))
            .await?;
        }
        SessionCommand::SetReasoning {
            session_id,
            effort,
            summary,
            json,
        } => Box::pin(set_session_reasoning(session_id, effort, summary, json)).await?,
        SessionCommand::SetAuthPool {
            pool,
            profile,
            clear,
            json,
        } => Box::pin(set_auth_pool_preference(pool, profile, clear, json)).await?,
        SessionCommand::ActiveSkills { session_id, json } => {
            Box::pin(list_active_skills(session_id, json)).await?;
        }
        SessionCommand::InvokeSkill {
            session_id,
            skill_id,
            arguments,
            json,
        } => Box::pin(invoke_session_skill(session_id, skill_id, arguments, json)).await?,
        SessionCommand::ActivateSkill {
            session_id,
            skill_id,
            json,
        } => Box::pin(set_session_skill(session_id, skill_id, true, json)).await?,
        SessionCommand::DeactivateSkill {
            session_id,
            skill_id,
            json,
        } => Box::pin(set_session_skill(session_id, skill_id, false, json)).await?,
        SessionCommand::Compact { session_id, json } => {
            Box::pin(compact_session(session_id, json)).await?;
        }
        SessionCommand::Watch {
            session_id,
            limit,
            json,
        } => Box::pin(watch_session(session_id, limit, json)).await?,
        SessionCommand::History {
            session_id,
            after,
            before,
            limit,
            json,
        } => Box::pin(session_history(session_id, after, before, limit, json)).await?,
        SessionCommand::Around {
            session_id,
            sequence,
            before,
            after,
            json,
        } => Box::pin(session_around(session_id, sequence, before, after, json)).await?,
        SessionCommand::Inspect {
            session_id,
            category,
            after,
            before,
            limit,
            json,
        } => {
            Box::pin(session_inspect(
                session_id, category, after, before, limit, json,
            ))
            .await?;
        }
        command @ (SessionCommand::MigrateInventory { .. }
        | SessionCommand::MigrateStart { .. }
        | SessionCommand::MigrateStatus { .. }
        | SessionCommand::MigrateWait { .. }
        | SessionCommand::MigrateCancel { .. }) => {
            Box::pin(handle_session_migration_subcommand(command)).await?;
        }
        command @ (SessionCommand::Search { .. }
        | SessionCommand::SearchStatus { .. }
        | SessionCommand::SearchPurge { .. }
        | SessionCommand::SearchRebuild { .. }
        | SessionCommand::SearchBackfillStart { .. }
        | SessionCommand::SearchBackfillStatus { .. }
        | SessionCommand::SearchBackfillWait { .. }
        | SessionCommand::SearchBackfillCancel { .. }
        | SessionCommand::SearchBackfill { .. }
        | SessionCommand::SearchExplain { .. }) => {
            Box::pin(handle_session_search_subcommand(command)).await?;
        }
        SessionCommand::Export { session_id, format } => {
            Box::pin(session_export(session_id, format)).await?;
        }
        SessionCommand::Timeline { session_id } => Box::pin(session_timeline(session_id)).await?,
        SessionCommand::Diagnose { session_id, json } => {
            Box::pin(session_diagnose(session_id, json)).await?;
        }
        SessionCommand::Doctor {
            session_id,
            catalog,
            scan,
            json,
        } => {
            Box::pin(run_session_repair_command(SessionRepairCliOptions {
                target: repair_cli_target(session_id, catalog, scan),
                mode: SessionRepairCliMode::DryRun,
                output: repair_cli_output(json),
            }))
            .await?;
        }
        SessionCommand::RetiredCatalogs { apply, json } => {
            Box::pin(retired_catalogs(apply, json)).await?;
        }
        SessionCommand::Repair {
            session_id,
            catalog,
            scan,
            dry_run,
            json,
        } => {
            Box::pin(run_session_repair_command(SessionRepairCliOptions {
                target: repair_cli_target(session_id, catalog, scan),
                mode: repair_cli_mode(dry_run),
                output: repair_cli_output(json),
            }))
            .await?;
        }
        SessionCommand::Reprice {
            session_id,
            from_ms,
            to_ms,
            catalog,
        } => {
            Box::pin(reprice_session_command(
                session_id, from_ms, to_ms, &catalog,
            ))
            .await?;
        }
        SessionCommand::Reindex { session_id } => {
            Box::pin(reindex_session_model_context(session_id)).await?;
        }
        SessionCommand::Relocate {
            to,
            to_profile,
            session,
            dry_run,
            json,
        } => {
            relocate_sessions_command(to, to_profile, &session, dry_run, json)?;
        }
        SessionCommand::ReleaseOwner { session_id } => {
            Box::pin(release_session_owner(session_id)).await?;
        }
        SessionCommand::StopOwner { session_id } => {
            Box::pin(stop_session_owner(session_id, false)).await?;
        }
        SessionCommand::KillOwner { session_id, yes } => {
            if !yes {
                return Err(CliError::InvalidArguments(
                    "force-killing a session owner requires --yes".to_owned(),
                ));
            }
            Box::pin(stop_session_owner(session_id, true)).await?;
        }
        SessionCommand::Import { command } => {
            Box::pin(handle_session_import_command(command)).await?;
        }
    }
    Ok(())
}

async fn reprice_session_command(
    session_id: SessionId,
    from_ms: u64,
    to_ms: u64,
    catalog: &Path,
) -> Result<(), CliError> {
    use std::io::Read as _;
    let range = bcode_session_models::SessionCostRange {
        from_timestamp_ms: from_ms,
        to_timestamp_ms: to_ms,
    };
    range.validate().map_err(CliError::InvalidArguments)?;
    let mut bytes = Vec::new();
    fs::File::open(catalog)?
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(CliError::InvalidArguments(
            "catalog snapshot exceeds 16 MiB".into(),
        ));
    }
    let snapshot = serde_json::from_slice(&bytes).map_err(|_| {
        CliError::InvalidArguments("catalog snapshot is not a valid catalog document".to_owned())
    })?;
    drop(bytes);
    let report = BcodeClient::default_endpoint()
        .reprice_session(session_id, range, snapshot)
        .await?;
    print_json(&report)
}

fn parse_reprice_datetime(value: &str) -> Result<u64, String> {
    let timestamp = chrono::DateTime::parse_from_rfc3339(value)
        .map_err(|_| "expected RFC 3339 datetime with timezone".to_owned())?
        .timestamp_millis();
    u64::try_from(timestamp).map_err(|_| "datetime must not precede the Unix epoch".to_owned())
}

/// Resolve the current state location set, or fail closed with an actionable message.
///
/// Resolution never substitutes a different location, so an unavailable location is reported
/// rather than silently replaced.
fn resolve_current_state_locations() -> Result<bcode_config::StateLocationSet, CliError> {
    let config = bcode_config::load_config()?;
    bcode_config::resolve_state_location_set(
        &config.state,
        &bcode_config::StateLocationSelection::default(),
    )
    .map_err(|error| CliError::InvalidArguments(error.to_string()))
}

fn handle_state_command(command: StateCommand) -> Result<(), CliError> {
    match command {
        StateCommand::Locations { json } => list_state_locations(json),
        StateCommand::PruneStaging { root, apply, json } => {
            prune_relocation_staging_command(root, apply, json)
        }
    }
}

/// Inventory, and optionally remove, staging directories from interrupted relocations.
///
/// Staging is derived state, so removing it never risks canonical history. Staging still held by a
/// running relocation is reported as live and left alone.
fn prune_relocation_staging_command(
    root: Option<PathBuf>,
    apply: bool,
    json: bool,
) -> Result<(), CliError> {
    let sessions_root = match root {
        Some(root) => {
            if !root.is_absolute() {
                return Err(CliError::InvalidArguments(format!(
                    "state root must be an absolute path, got {}",
                    root.display()
                )));
            }
            root.join("sessions")
        }
        None => bcode_config::default_session_store_dir(),
    };
    let report = bcode_session_migration::prune_relocation_staging(&sessions_root, apply)
        .map_err(|error| CliError::InvalidArguments(error.to_string()))?;

    if json {
        return print_json(&report);
    }

    if report.is_empty() {
        println!(
            "No interrupted relocation staging found under {}.",
            sessions_root.display()
        );
        return Ok(());
    }
    if !report.live.is_empty() {
        println!(
            "Live relocations in progress ({}); not pruned:",
            report.live.len()
        );
        for entry in &report.live {
            println!(
                "  {} — staging held by a running relocation",
                entry.session_id
            );
        }
    }
    if !report.prunable.is_empty() {
        let verb = if apply { "Pruned" } else { "Prunable" };
        println!(
            "{verb} abandoned staging ({}, {} byte(s)):",
            report.prunable.len(),
            report.prunable_bytes()
        );
        for entry in &report.prunable {
            let stage = entry.stage.as_deref().unwrap_or("unknown");
            println!(
                "  {} — interrupted while {stage}, {} byte(s)",
                entry.session_id, entry.staged_bytes
            );
        }
        if !apply {
            println!("Re-run with --apply to remove them.");
        }
    }
    Ok(())
}

/// Print the configured state location inventory.
///
/// This is read-only: it resolves configuration and reports availability without creating,
/// migrating, or repairing anything.
fn list_state_locations(json: bool) -> Result<(), CliError> {
    let resolved = resolve_current_state_locations()?;
    let primary_id = resolved.primary().id().as_str().to_owned();
    let rows = resolved
        .readable()
        .iter()
        .map(|location| {
            let sessions_root = location.sessions_root();
            serde_json::json!({
                "location_id": location.id().as_str(),
                "profile": location.profile(),
                "primary": location.id().as_str() == primary_id,
                "provenance": format!("{:?}", location.provenance()),
                "root": location.root(),
                "sessions_root": sessions_root,
                "available": sessions_root.is_dir(),
            })
        })
        .collect::<Vec<_>>();

    if json {
        return print_json(&serde_json::json!({ "locations": rows }));
    }

    println!("Configured state locations ({}):", rows.len());
    for row in &rows {
        let role = if row["primary"] == serde_json::json!(true) {
            "primary"
        } else {
            "readable"
        };
        let label = row["profile"].as_str().map_or_else(
            || row["location_id"].as_str().unwrap_or("?").to_owned(),
            |profile| format!("{profile} ({})", row["location_id"].as_str().unwrap_or("?")),
        );
        let availability = if row["available"] == serde_json::json!(true) {
            "available"
        } else {
            "unavailable"
        };
        println!(
            "  {label} [{role}, {availability}] selected by {}",
            row["provenance"].as_str().unwrap_or("?")
        );
        println!("    state root:    {}", row["root"].as_str().unwrap_or("?"));
        println!(
            "    sessions root: {}",
            row["sessions_root"].as_str().unwrap_or("?")
        );
    }
    Ok(())
}

/// Resolve the relocation destination sessions root from `--to` or `--to-profile`.
fn relocation_destination(
    to: Option<PathBuf>,
    to_profile: Option<String>,
) -> Result<PathBuf, CliError> {
    match (to, to_profile) {
        (Some(root), _) => {
            if !root.is_absolute() {
                return Err(CliError::InvalidArguments(format!(
                    "relocation destination must be an absolute path, got {}",
                    root.display()
                )));
            }
            Ok(root.join("sessions"))
        }
        (None, Some(profile)) => {
            let config = bcode_config::load_config()?;
            let resolved = bcode_config::resolve_state_location_set(
                &config.state,
                &bcode_config::StateLocationSelection {
                    root: None,
                    profile: Some(profile),
                },
            )
            .map_err(|error| CliError::InvalidArguments(error.to_string()))?;
            Ok(resolved.primary().sessions_root().to_path_buf())
        }
        (None, None) => Err(CliError::InvalidArguments(
            "relocation requires --to <ROOT> or --to-profile <PROFILE>".to_owned(),
        )),
    }
}

/// Plan or apply an explicit cross-location session relocation.
///
/// Ownership is fenced by the session domain's exclusive maintenance guard, which refuses every
/// live owner, so a session can never be moved out from under a running daemon.
fn relocate_sessions_command(
    to: Option<PathBuf>,
    to_profile: Option<String>,
    sessions: &[SessionId],
    dry_run: bool,
    json: bool,
) -> Result<(), CliError> {
    let destination_root = relocation_destination(to, to_profile)?;
    let source_root = bcode_config::default_session_store_dir();

    let plan = bcode_session_migration::plan_session_relocation(
        &source_root,
        &destination_root,
        sessions,
        |session_id, root| {
            // A live owner is authoritative: probe without taking the guard so planning stays
            // non-mutating.
            bcode_session::lease::active_session_owners(root, session_id).map(|owners| {
                if owners.is_empty() {
                    bcode_session_migration::SessionRelocationOwnership::Available
                } else {
                    bcode_session_migration::SessionRelocationOwnership::BlockedByOwner
                }
            })
        },
    )
    .map_err(|error| CliError::InvalidArguments(error.to_string()))?;

    if dry_run {
        print_relocation_plan(&plan, &source_root, &destination_root, json)?;
        return Ok(());
    }

    std::fs::create_dir_all(&destination_root).map_err(|error| {
        CliError::InvalidArguments(format!(
            "relocation destination {} is unavailable: {error}",
            destination_root.display()
        ))
    })?;

    let report = bcode_session_migration::relocate_sessions(
        &source_root,
        &destination_root,
        &plan,
        |session_id, apply| {
            // Exclusive maintenance refuses every live owner and is held for the whole move.
            let Ok(guard) =
                bcode_session::lease::acquire_session_maintenance_guard(&source_root, session_id)
            else {
                return Ok::<_, bcode_session::lease::SessionLeaseError>(
                    bcode_session_migration::SessionRelocationOwnership::BlockedByOwner,
                );
            };
            apply();
            drop(guard);
            Ok(bcode_session_migration::SessionRelocationOwnership::Available)
        },
    )
    .map_err(|error| CliError::InvalidArguments(error.to_string()))?;

    print_relocation_report(&report, json)
}

fn print_relocation_plan(
    plan: &bcode_session_migration::SessionRelocationPlan,
    source_root: &Path,
    destination_root: &Path,
    json: bool,
) -> Result<(), CliError> {
    if json {
        return print_json(&serde_json::json!({
            "source_sessions_root": source_root,
            "destination_sessions_root": destination_root,
            "relocatable": plan.relocatable,
            "blocked": plan.blocked,
            "total_bytes": plan.total_bytes(),
            "pinned_artifact_sessions": plan.pinned_artifact_sessions(),
        }));
    }
    println!("Relocation plan (dry run; nothing was copied or deleted):");
    println!("  source:      {}", source_root.display());
    println!("  destination: {}", destination_root.display());
    println!(
        "  relocatable: {} session(s), {} byte(s)",
        plan.relocatable.len(),
        plan.total_bytes()
    );
    for entry in &plan.relocatable {
        let pinned = if entry.has_pinned_artifacts {
            " (has location-pinned artifacts; source copies are retained)"
        } else {
            ""
        };
        println!(
            "    {} — {} byte(s){pinned}",
            entry.session_id, entry.canonical_bytes
        );
    }
    if !plan.blocked.is_empty() {
        println!("  blocked: {} session(s)", plan.blocked.len());
        for entry in &plan.blocked {
            println!("    {} — {}", entry.session_id, describe_block(entry));
        }
    }
    Ok(())
}

const fn describe_block(entry: &bcode_session_migration::SessionRelocationEntry) -> &'static str {
    match entry.blocked {
        Some(bcode_session_migration::SessionRelocationBlock::BlockedByOwner) => {
            "a live owner holds this session; stop it first"
        }
        Some(bcode_session_migration::SessionRelocationBlock::DestinationConflict) => {
            "the destination already claims this session ID; resolve the conflict explicitly"
        }
        Some(bcode_session_migration::SessionRelocationBlock::Unreadable) => {
            "canonical storage is missing or unreadable"
        }
        None => "relocatable",
    }
}

fn print_relocation_report(
    report: &bcode_session_migration::SessionRelocationReport,
    json: bool,
) -> Result<(), CliError> {
    if json {
        return print_json(report);
    }
    println!("Relocated {} session(s).", report.relocated.len());
    for session_id in &report.relocated {
        println!("  {session_id}");
    }
    if !report.retained_pinned_artifacts.is_empty() {
        println!(
            "Retained source artifact copies for {} session(s) so recorded absolute URIs stay resolvable:",
            report.retained_pinned_artifacts.len()
        );
        for session_id in &report.retained_pinned_artifacts {
            println!("  {session_id}");
        }
    }
    if !report.blocked.is_empty() {
        println!("Skipped {} session(s):", report.blocked.len());
        for entry in &report.blocked {
            println!("  {} — {}", entry.session_id, describe_block(entry));
        }
    }
    Ok(())
}

fn default_model_provider_id() -> Result<String, CliError> {
    bcode_config::load_config()?
        .resolved_model_selection()
        .provider_plugin_id
        .ok_or_else(|| {
            CliError::PluginCli("no model provider is configured; pass --provider".to_string())
        })
}

fn list_ignored_models(provider: Option<&str>, json: bool) -> Result<(), CliError> {
    let mut state = bcode_config::load_model_ignores_state()?;
    state.retain(|provider_id, _| provider.is_none_or(|filter| filter == provider_id));
    if json {
        return print_json(&state);
    }
    let mut output = std::io::stdout().lock();
    for (provider_id, rules) in state {
        writeln!(output, "{provider_id}")?;
        for model in rules.models {
            writeln!(output, "  model {model}")?;
        }
        for pattern in rules.patterns {
            writeln!(output, "  pattern {pattern}")?;
        }
    }
    output.flush()?;
    Ok(())
}

async fn handle_model_command(command: ModelCommand) -> Result<(), CliError> {
    match command {
        ModelCommand::Ignore {
            model_id,
            provider,
            json,
        } => {
            let provider = match provider {
                Some(provider) => provider,
                None => default_model_provider_id()?,
            };
            let path = bcode_config::ignore_model_in_state(&provider, model_id.clone())?;
            if json {
                print_json(
                    &serde_json::json!({"operation": "model_ignored", "provider": provider, "model_id": model_id, "state_path": path}),
                )?;
            } else {
                let mut output = std::io::stdout().lock();
                writeln!(
                    output,
                    "Ignored model '{model_id}' for provider '{provider}' in {}",
                    display_from_current_dir(&path)
                )?;
                output.flush()?;
            }
        }
        ModelCommand::Unignore {
            model_id,
            provider,
            json,
        } => {
            let provider = match provider {
                Some(provider) => provider,
                None => default_model_provider_id()?,
            };
            let path = bcode_config::unignore_model_in_state(&provider, &model_id)?;
            if json {
                print_json(
                    &serde_json::json!({"operation": "model_unignored", "provider": provider, "model_id": model_id, "state_path": path}),
                )?;
            } else {
                let mut output = std::io::stdout().lock();
                writeln!(
                    output,
                    "Removed state ignore for model '{model_id}' and provider '{provider}' in {}",
                    display_from_current_dir(&path)
                )?;
                output.flush()?;
            }
        }
        ModelCommand::Ignored { provider, json } => list_ignored_models(provider.as_deref(), json)?,
        ModelCommand::Verify {
            prompt,
            max_models,
            id_pattern,
            dry_run,
            output,
            timeout_seconds,
        } => {
            verify_models(
                prompt,
                max_models,
                id_pattern.as_ref(),
                dry_run,
                output,
                timeout_seconds,
            )?;
        }
        ModelCommand::VerifyCache(args) => verify_model_caches(&args).await?,
        ModelCommand::VerifyImages(args) => verify_model_images(&args).await?,
        other => {
            ensure_server_running().await?;
            match other {
                ModelCommand::List { json, provider } => list_models(json, provider).await?,
                ModelCommand::Status { session_id, json } => {
                    model_status(session_id, json).await?;
                }
                ModelCommand::Capabilities { json } => model_capabilities(json).await?,
                ModelCommand::Diagnostics { json } => model_catalog_diagnostics(json).await?,
                ModelCommand::Validate { json } => model_validate_config(json).await?,
                ModelCommand::Set {
                    session_id,
                    provider,
                    model_id,
                    json,
                } => set_session_model_selection(session_id, provider, model_id, json).await?,
                ModelCommand::Verify { .. }
                | ModelCommand::VerifyCache(_)
                | ModelCommand::VerifyImages(_)
                | ModelCommand::Ignore { .. }
                | ModelCommand::Unignore { .. }
                | ModelCommand::Ignored { .. } => unreachable!("handled above"),
            }
        }
    }
    Ok(())
}

async fn handle_auth_command(command: AuthCommand) -> Result<(), CliError> {
    match command {
        AuthCommand::Discover | AuthCommand::Import { .. } => {
            credential_setup::handle_command(command)
        }
        AuthCommand::Providers => auth_providers(),
        AuthCommand::Status { provider, profile } => provider
            .map_or_else(auth_status, |provider| {
                auth_provider_status(&provider, profile.as_deref())
            }),
        AuthCommand::Profile { command } => match command {
            AuthProfileCommand::List => auth_profile_list(),
            AuthProfileCommand::Show { profile } => auth_profile_show(&profile),
        },
        AuthCommand::Pool { command } => match command {
            AuthPoolCommand::List => auth_pool_list(),
            AuthPoolCommand::Profiles { pool } | AuthPoolCommand::Status { pool } => {
                auth_pool_status(&pool)
            }
            AuthPoolCommand::ResetCooldown { pool, profile } => {
                auth_pool_reset_cooldown(&pool, profile.as_deref());
                Ok(())
            }
            AuthPoolCommand::Promote { pool, profile } => {
                let path = bcode_provider_auth::set_auth_pool_preference(&pool, Some(&profile))?;
                println!(
                    "Preferred auth profile for '{pool}' is now '{profile}' ({}).",
                    display_from_current_dir(&path)
                );
                Ok(())
            }
            AuthPoolCommand::ClearPreference { pool } => {
                let path = bcode_provider_auth::set_auth_pool_preference(&pool, None)?;
                println!(
                    "Cleared interactive auth preference for '{pool}' ({}).",
                    display_from_current_dir(&path)
                );
                Ok(())
            }
        },
        AuthCommand::Prime { command } => handle_auth_prime_command(command),
        AuthCommand::Resets { command } => handle_auth_resets_command(command),
        AuthCommand::Usage { command } => handle_auth_usage_command(command),
        AuthCommand::Security {
            provider,
            profile,
            vault,
            require_backend,
        } => auth_security(
            provider.as_deref(),
            &profile,
            vault,
            require_backend.as_deref(),
        ),
        AuthCommand::Login {
            provider,
            profile,
            vault,
            recipient_key,
            no_device_seal,
            pool,
            method,
            verify,
        } => {
            if let Some(provider) = provider {
                auth_provider_login(
                    &provider,
                    AuthProviderLoginOptions {
                        explicit_profile: profile.as_deref(),
                        explicit_vault: vault,
                        recipient_key: recipient_key.as_deref(),
                        no_device_seal,
                        pool: pool.as_deref(),
                        requested_method: method.as_deref(),
                        verify,
                    },
                )
                .await
            } else {
                auth_login(profile, vault, recipient_key)
            }
        }
        AuthCommand::Logout {
            provider,
            profile,
            revoke,
        } => auth_provider_logout(&provider, profile.as_deref(), revoke).await,
    }
}

fn auth_security_status(
    provider: Option<&str>,
    profile_name: &str,
    explicit_vault: Option<PathBuf>,
) -> Result<bcode_provider_auth::security::AuthSecurityStatus, CliError> {
    let config = bcode_config::load_config()?;
    let runtime = bcode_config::load_runtime_auth_subscriptions();
    let mut runtime_profile;
    let auth_profile = if let Some(profile) = config.auth.profiles.get(profile_name) {
        profile
    } else if let Some(profile) = runtime.profiles.get(profile_name) {
        runtime_profile = bcode_config::AuthProfileConfig {
            provider_id: Some(profile.provider_id.clone()),
            owner_plugin_id: Some(profile.owner_plugin_id.clone()),
            backend: profile.backend.clone(),
            scheme: Some(profile.scheme.clone()),
            map: profile.map.clone(),
            settings: BTreeMap::from([
                ("profile".to_owned(), profile.storage_profile.clone()),
                ("vault".to_owned(), profile.vault.display().to_string()),
            ]),
        };
        if let Some(device_seal) = &profile.device_seal {
            runtime_profile
                .settings
                .insert("device_seal".to_owned(), device_seal.clone());
        }
        &runtime_profile
    } else {
        return Err(CliError::LoginProfile(format!(
            "Auth profile '{profile_name}' is not declared or registered in runtime state."
        )));
    };
    if let Some(provider) = provider
        && auth_profile.provider_id.as_deref() != Some(provider)
    {
        return Err(CliError::LoginProfile(format!(
            "Auth profile '{profile_name}' does not belong to provider '{provider}'."
        )));
    }
    if auth_profile.backend != "sshenv" {
        return Err(CliError::LoginProfile(format!(
            "Auth profile '{profile_name}' uses backend '{}'; vault security inspection requires sshenv.",
            auth_profile.backend
        )));
    }
    let storage_profile = auth_profile
        .settings
        .get("profile")
        .map_or(profile_name, String::as_str);
    let vault = explicit_vault
        .or_else(|| auth_profile.settings.get("vault").map(PathBuf::from))
        .unwrap_or_else(bcode_config::default_auth_vault_path);
    let policy = bcode_provider_auth::security::device_seal_policy_for_auth_profile(auth_profile);
    Ok(bcode_provider_auth::security::inspect_auth_vault_security(
        &vault,
        storage_profile,
        policy,
    ))
}

fn auth_security(
    provider: Option<&str>,
    profile_name: &str,
    explicit_vault: Option<PathBuf>,
    required_backend: Option<&str>,
) -> Result<(), CliError> {
    let status = auth_security_status(provider, profile_name, explicit_vault)?;
    if let Some(required_backend) = required_backend
        && status.device_seal_backend.as_deref() != Some(required_backend)
    {
        return Err(CliError::LoginProfile(format!(
            "Auth profile '{profile_name}' uses device seal backend {:?}; required '{required_backend}'.",
            status.device_seal_backend
        )));
    }
    print_json_line(&status)
}

fn handle_auth_usage_command(command: AuthUsageCommand) -> Result<(), CliError> {
    match command {
        AuthUsageCommand::Status {
            pool,
            profile,
            no_primary,
            refresh,
            json,
        } => auth_usage_status(&pool, profile.as_deref(), !no_primary, refresh, json),
    }
}

fn handle_auth_resets_command(command: AuthResetsCommand) -> Result<(), CliError> {
    match command {
        AuthResetsCommand::Status {
            pool,
            profile,
            no_primary,
            verbose,
            json,
        } => auth_resets_status(&pool, profile.as_deref(), !no_primary, verbose, json),
        AuthResetsCommand::Use {
            pool,
            profile,
            credit,
            dry_run,
            yes,
            json,
        } => auth_resets_use(
            &pool,
            profile.as_deref(),
            credit.as_deref(),
            dry_run,
            yes,
            json,
        ),
    }
}

fn handle_auth_prime_command(command: AuthPrimeCommand) -> Result<(), CliError> {
    match command {
        AuthPrimeCommand::Run {
            pool,
            profile,
            no_primary,
            include_primary: _include_primary,
            force,
            dry_run,
            json,
            timeout_seconds,
            max_attempts,
            no_max_attempts,
            delay_seconds,
        } => {
            let options = AuthPrimeRunOptions {
                pool: &pool,
                profile: profile.as_deref(),
                include_primary: !no_primary,
                force,
                dry_run,
                json,
                timeout_seconds,
                max_attempts: (!no_max_attempts).then_some(max_attempts),
                delay_seconds,
            };
            auth_prime_run(&options)
        }
        AuthPrimeCommand::Status {
            pool,
            profile,
            no_primary,
            include_primary: _include_primary,
            refresh,
            json,
        } => auth_prime_status(&pool, profile.as_deref(), !no_primary, refresh, json),
    }
}

#[derive(Debug, Clone)]
struct AuthPrimeProfileTarget {
    profile: String,
    source: String,
    candidate: bcode_model::ProviderAuthCandidate,
    primary: bool,
}

#[derive(Debug, Clone)]
struct AuthPrimePlan {
    pool: String,
    provider_plugin_id: String,
    required_windows: BTreeMap<String, Vec<String>>,
    targets: Vec<AuthPrimeProfileTarget>,
}

#[derive(Debug, Clone, Serialize)]
struct AuthPrimeReport {
    pool: String,
    provider_plugin_id: String,
    refreshed: bool,
    dry_run: bool,
    profiles: Vec<AuthPrimeProfileReport>,
}

#[derive(Debug, Clone, Serialize)]
struct AuthResetsReport {
    pool: String,
    provider_plugin_id: String,
    profiles: Vec<AuthResetsProfileReport>,
}

#[derive(Debug, Clone, Serialize)]
struct AuthResetsProfileReport {
    profile: String,
    source: String,
    primary: bool,
    status: String,
    available_count: Option<u32>,
    reason: Option<String>,
    credits: Vec<AuthResetCreditReport>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    debug: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
struct AuthResetCreditReport {
    credit_id: String,
    reset_type: String,
    status: String,
    granted_at: String,
    expires_at: Option<String>,
    title: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct AuthResetConsumeReport {
    pool: String,
    provider_plugin_id: String,
    profile: String,
    dry_run: bool,
    credit_id: Option<String>,
    redeem_request_id: String,
    status: String,
    provider_code: Option<String>,
    windows_reset: Option<u32>,
    message: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    debug: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
struct AuthUsageReport {
    pool: String,
    provider_plugin_id: String,
    refreshed: bool,
    profiles: Vec<AuthUsageProfileReport>,
}

#[derive(Debug, Clone, Serialize)]
struct AuthUsageProfileReport {
    profile: String,
    source: String,
    primary: bool,
    status: String,
    reason: Option<String>,
    windows: Vec<AuthPrimeWindowReport>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    debug: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
struct AuthPrimeProfileReport {
    profile: String,
    source: String,
    primary: bool,
    status: String,
    needs_priming: bool,
    reason: Option<String>,
    attempts: u64,
    limit_hit: bool,
    failure_code: Option<String>,
    diagnostic: Option<String>,
    remaining_windows: Vec<String>,
    windows: Vec<AuthPrimeWindowReport>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    debug: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
struct AuthPrimeWindowReport {
    meter_id: String,
    window_id: String,
    status: String,
    used_percent: Option<u32>,
    window_duration_secs: Option<u64>,
    resets_at_unix: Option<u64>,
    observed_at_unix: Option<u64>,
    primed_at_unix: Option<u64>,
    source: Option<String>,
    detail: String,
}

fn auth_resets_status(
    pool: &str,
    profile: Option<&str>,
    include_primary: bool,
    verbose: bool,
    json: bool,
) -> Result<(), CliError> {
    let plan = auth_prime_plan(pool, profile, include_primary)?;
    let mut host = load_cli_plugin_host()?;
    let mut profiles = Vec::new();
    for target in &plan.targets {
        let request = bcode_model::AuthResetCreditsRequest {
            provider_context: provider_context_for_prime_target(&plan, target),
        };
        let response = host.invoke_service_json::<_, bcode_model::AuthResetCreditsResponse>(
            &plan.provider_plugin_id,
            bcode_model::MODEL_PROVIDER_INTERFACE_ID,
            bcode_model::OP_AUTH_RESET_CREDITS,
            &request,
        );
        profiles.push(auth_resets_profile_report(target, response));
    }
    host.deactivate_all()?;
    print_auth_resets_report(
        &AuthResetsReport {
            pool: plan.pool,
            provider_plugin_id: plan.provider_plugin_id,
            profiles,
        },
        verbose,
        json,
    )
}

#[allow(clippy::fn_params_excessive_bools)]
fn auth_resets_use(
    pool: &str,
    profile: Option<&str>,
    credit_id: Option<&str>,
    dry_run: bool,
    yes: bool,
    json: bool,
) -> Result<(), CliError> {
    let plan = auth_prime_plan(pool, profile, true)?;
    let target = match (profile, plan.targets.as_slice()) {
        (Some(profile), targets) => targets
            .iter()
            .find(|target| target.profile == profile)
            .ok_or_else(|| {
                CliError::AuthPrimeFailed(format!(
                    "auth profile '{profile}' is not in pool '{pool}'"
                ))
            })?,
        (None, [target]) => target,
        (None, []) => {
            return Err(CliError::AuthPrimeFailed(format!(
                "auth pool '{pool}' has no profiles to use reset credits for"
            )));
        }
        (None, _) => {
            return Err(CliError::AuthPrimeFailed(
                "--profile is required when a pool has multiple profiles".to_string(),
            ));
        }
    };
    let redeem_request_id = random_urlsafe(18)?;
    if dry_run {
        return print_auth_reset_consume_report(
            &AuthResetConsumeReport {
                pool: plan.pool,
                provider_plugin_id: plan.provider_plugin_id,
                profile: target.profile.clone(),
                dry_run,
                credit_id: credit_id.map(str::to_string),
                redeem_request_id,
                status: "dry_run".to_string(),
                provider_code: None,
                windows_reset: None,
                message: Some("no reset credit was consumed".to_string()),
                debug: BTreeMap::new(),
            },
            json,
        );
    }
    if !yes && !confirm_auth_reset_use(&target.profile, credit_id)? {
        return Err(CliError::AuthPrimeFailed(
            "reset credit consume cancelled".to_string(),
        ));
    }
    let request = bcode_model::AuthResetCreditConsumeRequest {
        provider_context: provider_context_for_prime_target(&plan, target),
        redeem_request_id: redeem_request_id.clone(),
        credit_id: credit_id.map(str::to_string),
    };
    let mut host = load_cli_plugin_host()?;
    let response = host.invoke_service_json::<_, bcode_model::AuthResetCreditConsumeResponse>(
        &plan.provider_plugin_id,
        bcode_model::MODEL_PROVIDER_INTERFACE_ID,
        bcode_model::OP_AUTH_RESET_CREDIT_CONSUME,
        &request,
    );
    let deactivation = host.deactivate_all();
    let mut response = response?;
    deactivation?;
    print_auth_reset_consume_report(
        &AuthResetConsumeReport {
            pool: plan.pool,
            provider_plugin_id: plan.provider_plugin_id,
            profile: target.profile.clone(),
            dry_run,
            credit_id: credit_id.map(str::to_string),
            redeem_request_id,
            status: auth_reset_consume_status_label(response.status).to_string(),
            provider_code: response.provider_code.take(),
            windows_reset: response.windows_reset,
            message: response.message.take(),
            debug: response.debug,
        },
        json,
    )
}

fn auth_resets_profile_report(
    target: &AuthPrimeProfileTarget,
    response: Result<bcode_model::AuthResetCreditsResponse, bcode_plugin::PluginServiceCallError>,
) -> AuthResetsProfileReport {
    match response {
        Ok(response) => AuthResetsProfileReport {
            profile: target.profile.clone(),
            source: target.source.clone(),
            primary: target.primary,
            status: if response.supported {
                "available"
            } else {
                "unsupported"
            }
            .to_string(),
            available_count: response.supported.then_some(response.available_count),
            reason: response.degraded_reason,
            credits: response
                .credits
                .into_iter()
                .map(|credit| AuthResetCreditReport {
                    credit_id: credit.credit_id,
                    reset_type: credit.reset_type,
                    status: credit.status,
                    granted_at: credit.granted_at,
                    expires_at: credit.expires_at,
                    title: credit.title,
                    description: credit.description,
                })
                .collect(),
            debug: response.debug,
        },
        Err(error) => AuthResetsProfileReport {
            profile: target.profile.clone(),
            source: target.source.clone(),
            primary: target.primary,
            status: "error".to_string(),
            available_count: None,
            reason: Some(error.to_string()),
            credits: Vec::new(),
            debug: BTreeMap::new(),
        },
    }
}

const fn auth_reset_consume_status_label(
    status: bcode_model::AuthResetCreditConsumeStatus,
) -> &'static str {
    match status {
        bcode_model::AuthResetCreditConsumeStatus::Unsupported => "unsupported",
        bcode_model::AuthResetCreditConsumeStatus::Reset => "reset",
        bcode_model::AuthResetCreditConsumeStatus::NothingToReset => "nothing_to_reset",
        bcode_model::AuthResetCreditConsumeStatus::NoCredit => "no_credit",
        bcode_model::AuthResetCreditConsumeStatus::AlreadyRedeemed => "already_redeemed",
        bcode_model::AuthResetCreditConsumeStatus::Failed => "failed",
    }
}

fn confirm_auth_reset_use(profile: &str, credit_id: Option<&str>) -> Result<bool, CliError> {
    read_auth_reset_confirmation(
        &mut std::io::stdin().lock(),
        &mut std::io::stderr().lock(),
        profile,
        credit_id,
    )
}

fn read_auth_reset_confirmation(
    input: &mut impl std::io::BufRead,
    output: &mut impl std::io::Write,
    profile: &str,
    credit_id: Option<&str>,
) -> Result<bool, CliError> {
    use std::io::BufRead as _;
    const MAX_CONFIRMATION_BYTES: u16 = 64;
    let credit = credit_id.unwrap_or("provider-selected credit");
    write!(
        output,
        "Consume one banked rate-limit reset for auth profile '{profile}' ({credit})? Type 'yes' to continue: "
    )?;
    output.flush()?;
    let mut line = String::new();
    input
        .take(u64::from(MAX_CONFIRMATION_BYTES) + 1)
        .read_line(&mut line)?;
    Ok(line.len() <= usize::from(MAX_CONFIRMATION_BYTES) && line.trim() == "yes")
}

fn auth_usage_status(
    pool: &str,
    profile: Option<&str>,
    include_primary: bool,
    refresh: bool,
    json: bool,
) -> Result<(), CliError> {
    let plan = auth_prime_plan(pool, profile, include_primary)?;
    let refresh_debug = if refresh {
        refresh_prime_usage_windows(&plan)?
    } else {
        BTreeMap::new()
    };
    let report = auth_usage_report(&plan, refresh, &refresh_debug);
    print_auth_usage_report(&report, json)
}

#[allow(clippy::fn_params_excessive_bools)]
fn auth_prime_status(
    pool: &str,
    profile: Option<&str>,
    include_primary: bool,
    refresh: bool,
    json: bool,
) -> Result<(), CliError> {
    let plan = auth_prime_plan(pool, profile, include_primary)?;
    let refresh_debug = if refresh {
        refresh_prime_usage_windows(&plan)?
    } else {
        BTreeMap::new()
    };
    let report = auth_prime_report(&plan, refresh, false, &refresh_debug);
    print_auth_prime_report(&report, json)
}

#[allow(clippy::fn_params_excessive_bools)]
#[allow(clippy::struct_excessive_bools)]
struct AuthPrimeRunOptions<'a> {
    pool: &'a str,
    profile: Option<&'a str>,
    include_primary: bool,
    force: bool,
    dry_run: bool,
    json: bool,
    timeout_seconds: u64,
    max_attempts: Option<u64>,
    delay_seconds: u64,
}

fn auth_prime_run(options: &AuthPrimeRunOptions<'_>) -> Result<(), CliError> {
    let plan = auth_prime_plan(options.pool, options.profile, options.include_primary)?;
    let _refresh_debug = refresh_prime_usage_windows(&plan)?;
    let mut report = auth_prime_report(&plan, true, options.dry_run, &BTreeMap::new());
    let failures = if options.dry_run {
        Vec::new()
    } else {
        let config = bcode_config::load_config()?;
        let selected_model_id = config.resolved_model_selection().selected_model_id;
        let mut host = load_cli_plugin_host()?;
        let failures = run_auth_prime_targets(
            &plan,
            &mut report,
            &host,
            selected_model_id.as_deref(),
            options,
        )?;
        host.deactivate_all()?;
        failures
    };
    if failures.is_empty() {
        return print_auth_prime_report(&report, options.json);
    }
    print_auth_prime_report(&report, options.json)?;
    Err(CliError::AuthPrimeFailed(failures.join("; ")))
}

fn run_auth_prime_targets(
    plan: &AuthPrimePlan,
    report: &mut AuthPrimeReport,
    host: &bcode_plugin::PluginHost,
    selected_model_id: Option<&str>,
    options: &AuthPrimeRunOptions<'_>,
) -> Result<Vec<String>, CliError> {
    let mut failures = Vec::new();
    for (index, target) in plan.targets.iter().enumerate() {
        if !options.force && !report.profiles[index].needs_priming {
            continue;
        }
        if let Some(failure) = run_auth_prime_target(
            plan,
            target,
            index,
            report,
            host,
            selected_model_id,
            options,
        )? {
            failures.push(failure);
            break;
        }
    }
    Ok(failures)
}

fn run_auth_prime_target(
    plan: &AuthPrimePlan,
    target: &AuthPrimeProfileTarget,
    index: usize,
    report: &mut AuthPrimeReport,
    host: &bcode_plugin::PluginHost,
    selected_model_id: Option<&str>,
    options: &AuthPrimeRunOptions<'_>,
) -> Result<Option<String>, CliError> {
    let mut attempts = 0_u64;
    loop {
        if let Some(limit) = options.max_attempts
            && attempts >= limit
        {
            let Some(profile_report) = report.profiles.get_mut(index) else {
                return Ok(Some(format!(
                    "priming did not complete for {} after {attempts} attempts",
                    target.profile
                )));
            };
            return Ok(Some(record_auth_prime_limit_hit(
                profile_report,
                &target.profile,
                attempts,
                limit,
            )));
        }

        attempts = attempts.saturating_add(1);
        let response = send_auth_prime_request(plan, target, host, selected_model_id, options)?;
        let status = response.status;
        let message = response.message.clone();
        let has_after_usage = response.after.is_some();
        if let Some(usage) = response.after.as_ref().or(response.before.as_ref()) {
            bcode_provider_auth::auth_pool_state::record_profile_usage_windows(
                Some(&plan.pool),
                Some(&target.profile),
                &usage.meters,
            );
        }
        *report = auth_prime_report(plan, true, options.dry_run, &BTreeMap::new());
        let Some(profile_report) = report.profiles.get_mut(index) else {
            return Ok(None);
        };
        profile_report.attempts = attempts;
        profile_report.status = auth_prime_status_label(status).to_string();
        profile_report.reason = message;

        if status == bcode_model::AuthPrimeStatus::Primed || !profile_report.needs_priming {
            bcode_provider_auth::auth_pool_state::mark_profile_primed(
                Some(&plan.pool),
                Some(&target.profile),
            );
            profile_report.status = "primed".to_string();
            return Ok(None);
        }

        if status == bcode_model::AuthPrimeStatus::Unsupported {
            return Ok(Some(record_auth_prime_unsupported(
                profile_report,
                &target.profile,
            )));
        }

        if status != bcode_model::AuthPrimeStatus::Failed || !has_after_usage {
            return Ok(Some(record_auth_prime_request_failure(
                profile_report,
                &target.profile,
            )));
        }

        if options.delay_seconds > 0 {
            std::thread::sleep(Duration::from_secs(options.delay_seconds));
        }
    }
}

fn send_auth_prime_request(
    plan: &AuthPrimePlan,
    target: &AuthPrimeProfileTarget,
    host: &bcode_plugin::PluginHost,
    selected_model_id: Option<&str>,
    options: &AuthPrimeRunOptions<'_>,
) -> Result<bcode_model::AuthPrimeResponse, CliError> {
    let mut provider_context = provider_context_for_prime_target(plan, target);
    provider_context.auth_pool_selection_reason = Some("manual_prime".to_string());
    let request = bcode_model::AuthPrimeRequest {
        provider_context,
        required_windows: plan.required_windows.clone(),
        model_id: selected_model_id.map(str::to_string),
        timeout_seconds: Some(options.timeout_seconds),
        force: options.force,
    };
    host.invoke_service_json(
        &plan.provider_plugin_id,
        bcode_model::MODEL_PROVIDER_INTERFACE_ID,
        bcode_model::OP_AUTH_PRIME,
        &request,
    )
    .map_err(plugin_service_call_error)
}

const fn auth_prime_status_label(status: bcode_model::AuthPrimeStatus) -> &'static str {
    match status {
        bcode_model::AuthPrimeStatus::Primed => "primed",
        bcode_model::AuthPrimeStatus::AlreadyPrimed => "already_primed",
        bcode_model::AuthPrimeStatus::Unsupported => "unsupported",
        bcode_model::AuthPrimeStatus::Failed => "failed",
    }
}

fn record_auth_prime_limit_hit(
    report: &mut AuthPrimeProfileReport,
    profile: &str,
    attempts: u64,
    limit: u64,
) -> String {
    report.status = "failed".to_string();
    report.reason = Some(format!("max attempts reached after {attempts} attempts"));
    report.attempts = attempts;
    report.limit_hit = true;
    report.failure_code = Some("max_attempts_reached".to_string());
    report.diagnostic = Some(format!(
        "Priming did not complete after {limit} attempts. This likely indicates provider usage did not advance for one or more required windows, or Bcode is targeting the wrong usage meter/profile."
    ));
    report.remaining_windows = remaining_prime_window_ids(report);
    format!("priming did not complete for {profile} after {attempts} attempts")
}

fn record_auth_prime_unsupported(report: &mut AuthPrimeProfileReport, profile: &str) -> String {
    report.status = "failed".to_string();
    report.failure_code = Some("unsupported".to_string());
    report.diagnostic = Some(
        "Provider does not support priming/usage verification for this auth profile.".to_string(),
    );
    report.remaining_windows = remaining_prime_window_ids(report);
    format!("provider does not support priming/usage verification for {profile}")
}

fn record_auth_prime_request_failure(report: &mut AuthPrimeProfileReport, profile: &str) -> String {
    report.status = "failed".to_string();
    report.failure_code = Some("priming_request_failed".to_string());
    report.diagnostic = Some(
        "Priming request failed before provider usage could be verified for all required windows."
            .to_string(),
    );
    report.remaining_windows = remaining_prime_window_ids(report);
    format!("priming request failed before verification completed for {profile}")
}

fn auth_prime_plan(
    pool: &str,
    profile: Option<&str>,
    include_primary: bool,
) -> Result<AuthPrimePlan, CliError> {
    let config = bcode_config::load_config()?;
    let registry = bcode_config::load_runtime_auth_subscriptions();
    let declared_pool = config.auth.pools.get(pool);
    let runtime_pool = registry.pools.get(pool);
    let resolved_selection = config.resolved_model_selection();
    let selected_primary_profile = resolved_selection.auth_profile.clone();
    if declared_pool.is_none()
        && runtime_pool.is_none()
        && !(pool == "openai" && selected_primary_profile.is_some())
    {
        return Err(CliError::LoginProfile(format!(
            "Auth pool '{pool}' is not declared or registered."
        )));
    }
    let provider_plugin_id = declared_pool
        .and_then(|pool| pool.provider_plugin_id.clone())
        .or_else(|| runtime_pool.and_then(|pool| pool.provider_plugin_id.clone()))
        .or_else(|| resolved_selection.provider_plugin_id.clone())
        .ok_or_else(|| {
            CliError::LoginProfile(format!(
                "Auth pool '{pool}' does not declare a provider and no model provider is configured."
            ))
        })?;
    let required_windows = required_prime_windows(pool, declared_pool);
    let include_primary = include_primary || profile.is_some();
    let mut targets = Vec::new();
    let mut seen = BTreeSet::new();
    let primary_profile = selected_primary_profile.or_else(|| {
        (resolved_selection.auth_pool.as_deref() == Some(pool))
            .then(|| declared_pool.and_then(|pool| pool.profiles.first().cloned()))
            .flatten()
    });
    let mut all_profiles = Vec::<(String, String)>::new();
    if let Some(primary_profile) = &primary_profile {
        all_profiles.push((primary_profile.clone(), "primary".to_string()));
    }
    if let Some(pool_config) = declared_pool {
        all_profiles.extend(
            pool_config
                .profiles
                .iter()
                .map(|profile| (profile.clone(), "declared".to_string())),
        );
    }
    if let Some(pool_config) = runtime_pool {
        all_profiles.extend(
            pool_config
                .profiles
                .iter()
                .map(|profile| (profile.auth_profile.clone(), "runtime".to_string())),
        );
    }
    for (profile_name, source) in all_profiles {
        if !seen.insert(profile_name.clone()) {
            continue;
        }
        let primary = primary_profile.as_deref() == Some(profile_name.as_str());
        if primary && !include_primary {
            continue;
        }
        if profile.is_some_and(|requested| requested != profile_name) {
            continue;
        }
        if let Some(candidate) = auth_prime_candidate(&config, &registry, pool, &profile_name) {
            targets.push(AuthPrimeProfileTarget {
                profile: profile_name,
                source,
                candidate,
                primary,
            });
        }
    }
    Ok(AuthPrimePlan {
        pool: pool.to_string(),
        provider_plugin_id,
        required_windows,
        targets,
    })
}

fn auth_prime_candidate(
    config: &bcode_config::BcodeConfig,
    registry: &bcode_config::RuntimeAuthSubscriptions,
    pool: &str,
    profile_name: &str,
) -> Option<bcode_model::ProviderAuthCandidate> {
    if let Some(auth_profile) = config.auth.profiles.get(profile_name) {
        let resolved = bcode_provider_auth::resolve_auth_profile(profile_name, auth_profile);
        return Some(bcode_model::ProviderAuthCandidate {
            profile: Some(profile_name.to_string()),
            auth: resolved.auth,
            env: resolved.env,
        });
    }
    let runtime_profile = registry
        .pools
        .get(pool)?
        .profiles
        .iter()
        .find(|candidate| candidate.auth_profile == profile_name)?;
    let auth_profile = runtime_subscription_auth_profile_config(runtime_profile);
    let resolved = bcode_provider_auth::resolve_auth_profile(profile_name, &auth_profile);
    Some(bcode_model::ProviderAuthCandidate {
        profile: Some(profile_name.to_string()),
        auth: resolved.auth,
        env: resolved.env,
    })
}

fn runtime_subscription_auth_profile_config(
    profile: &bcode_config::RuntimeAuthSubscriptionProfile,
) -> bcode_config::AuthProfileConfig {
    bcode_config::AuthProfileConfig {
        backend: "sshenv".to_string(),
        provider_id: None,
        owner_plugin_id: None,
        scheme: Some(profile.scheme.clone()),
        map: BTreeMap::new(),
        settings: BTreeMap::from([
            ("provider".to_string(), profile.provider.clone()),
            ("profile".to_string(), profile.storage_profile.clone()),
            ("vault".to_string(), profile.vault.display().to_string()),
            ("mode".to_string(), "chatgpt".to_string()),
        ]),
    }
}

fn required_prime_windows(
    pool: &str,
    declared_pool: Option<&bcode_config::AuthPoolConfig>,
) -> BTreeMap<String, Vec<String>> {
    let configured = declared_pool
        .map(|pool| pool.priming.required_windows.clone())
        .unwrap_or_default();
    if !configured.is_empty() {
        return configured;
    }
    if pool == "openai" {
        return BTreeMap::from([(
            "codex".to_string(),
            vec!["primary".to_string(), "secondary".to_string()],
        )]);
    }
    BTreeMap::new()
}

fn provider_context_for_prime_target(
    plan: &AuthPrimePlan,
    target: &AuthPrimeProfileTarget,
) -> bcode_model::ProviderRequestContext {
    bcode_model::ProviderRequestContext {
        auth_profile: Some(target.profile.clone()),
        auth_pool: Some(plan.pool.clone()),
        auth_pool_routing: bcode_model::ProviderAuthPoolRouting {
            priming_enabled: true,
            priming_include_primary: true,
            priming_provider_windows: true,
            priming_required_windows: plan.required_windows.clone(),
            ..bcode_model::ProviderAuthPoolRouting::default()
        },
        auth: Some(target.candidate.auth.clone()),
        env: target.candidate.env.clone(),
        ..bcode_model::ProviderRequestContext::default()
    }
}

fn refresh_prime_usage_windows(
    plan: &AuthPrimePlan,
) -> Result<BTreeMap<String, BTreeMap<String, String>>, CliError> {
    let mut refresh_debug = BTreeMap::new();
    let mut host = load_cli_plugin_host()?;
    for target in &plan.targets {
        let request = bcode_model::AuthUsageRequest {
            provider_context: provider_context_for_prime_target(plan, target),
            meter_ids: plan.required_windows.keys().cloned().collect(),
        };
        let response = host.invoke_service_json::<_, bcode_model::AuthUsageResponse>(
            &plan.provider_plugin_id,
            bcode_model::MODEL_PROVIDER_INTERFACE_ID,
            bcode_model::OP_AUTH_USAGE,
            &request,
        );
        match response {
            Ok(response) => {
                refresh_debug.insert(target.profile.clone(), response.debug.clone());
                if response.supported {
                    bcode_provider_auth::auth_pool_state::record_profile_usage_windows(
                        Some(&plan.pool),
                        Some(&target.profile),
                        &response.meters,
                    );
                }
            }
            Err(error) => {
                refresh_debug.insert(
                    target.profile.clone(),
                    BTreeMap::from([("error".to_string(), error.to_string())]),
                );
            }
        }
    }
    host.deactivate_all()?;
    Ok(refresh_debug)
}

fn auth_usage_report(
    plan: &AuthPrimePlan,
    refreshed: bool,
    refresh_debug: &BTreeMap<String, BTreeMap<String, String>>,
) -> AuthUsageReport {
    let state = load_openai_auth_pool_state();
    let now = unix_now_secs();
    let profiles = plan
        .targets
        .iter()
        .map(|target| auth_usage_profile_report(plan, target, &state, now, refresh_debug))
        .collect();
    AuthUsageReport {
        pool: plan.pool.clone(),
        provider_plugin_id: plan.provider_plugin_id.clone(),
        refreshed,
        profiles,
    }
}

fn auth_usage_profile_report(
    plan: &AuthPrimePlan,
    target: &AuthPrimeProfileTarget,
    state: &bcode_provider_auth::auth_pool_state::AuthPoolState,
    now: u64,
    refresh_debug: &BTreeMap<String, BTreeMap<String, String>>,
) -> AuthUsageProfileReport {
    let key = format!("{}/{}", plan.pool, target.profile);
    let entry = state.entries.get(&key);
    let windows = auth_usage_window_reports(entry, now);
    let status = if windows.is_empty() {
        "unknown"
    } else if windows.iter().any(|window| window.status == "expired") {
        "expired"
    } else {
        "available"
    };
    let mut debug = refresh_debug
        .get(&target.profile)
        .cloned()
        .unwrap_or_default();
    if let Some(entry) = entry
        && let Some(last_success_unix) = entry.last_success_unix
    {
        debug.insert(
            "last_success_unix".to_string(),
            last_success_unix.to_string(),
        );
    }
    AuthUsageProfileReport {
        profile: target.profile.clone(),
        source: target.source.clone(),
        primary: target.primary,
        status: status.to_string(),
        reason: windows
            .iter()
            .find(|window| window.status == "missing" || window.status == "expired")
            .map(|window| window.detail.clone()),
        windows,
        debug,
    }
}

fn auth_prime_report(
    plan: &AuthPrimePlan,
    refreshed: bool,
    dry_run: bool,
    refresh_debug: &BTreeMap<String, BTreeMap<String, String>>,
) -> AuthPrimeReport {
    let state = load_openai_auth_pool_state();
    let now = unix_now_secs();
    let profiles = plan
        .targets
        .iter()
        .map(|target| auth_prime_profile_report(plan, target, &state, now, refresh_debug))
        .collect();
    AuthPrimeReport {
        pool: plan.pool.clone(),
        provider_plugin_id: plan.provider_plugin_id.clone(),
        refreshed,
        dry_run,
        profiles,
    }
}

fn auth_prime_profile_report(
    plan: &AuthPrimePlan,
    target: &AuthPrimeProfileTarget,
    state: &bcode_provider_auth::auth_pool_state::AuthPoolState,
    now: u64,
    refresh_debug: &BTreeMap<String, BTreeMap<String, String>>,
) -> AuthPrimeProfileReport {
    let key = format!("{}/{}", plan.pool, target.profile);
    let entry = state.entries.get(&key);
    let windows = auth_prime_window_reports(&plan.required_windows, entry, now);
    let needs_priming = bcode_provider_auth::auth_pool_state::profile_needs_priming_with_windows(
        Some(&plan.pool),
        Some(&target.profile),
        &plan.required_windows,
        None,
    );
    let status = if windows.is_empty() {
        "unknown"
    } else if needs_priming {
        "needs_priming"
    } else {
        "primed"
    };
    let mut debug = refresh_debug
        .get(&target.profile)
        .cloned()
        .unwrap_or_default();
    if let Some(entry) = entry {
        if let Some(last_success_unix) = entry.last_success_unix {
            debug.insert(
                "last_success_unix".to_string(),
                last_success_unix.to_string(),
            );
        }
        if let Some(primed_unix) = entry.primed_unix {
            debug.insert("primed_unix".to_string(), primed_unix.to_string());
        }
    }
    let remaining_windows = remaining_prime_window_ids_from_windows(&windows);
    AuthPrimeProfileReport {
        profile: target.profile.clone(),
        source: target.source.clone(),
        primary: target.primary,
        status: status.to_string(),
        needs_priming,
        reason: windows
            .iter()
            .find(|window| window.status != "active")
            .map(|window| window.detail.clone()),
        attempts: 0,
        limit_hit: false,
        failure_code: None,
        diagnostic: None,
        remaining_windows,
        windows,
        debug,
    }
}

fn remaining_prime_window_ids(report: &AuthPrimeProfileReport) -> Vec<String> {
    remaining_prime_window_ids_from_windows(&report.windows)
}

fn remaining_prime_window_ids_from_windows(windows: &[AuthPrimeWindowReport]) -> Vec<String> {
    windows
        .iter()
        .filter(|window| window.status != "active")
        .map(|window| format!("{}.{}", window.meter_id, window.window_id))
        .collect()
}

fn auth_usage_window_reports(
    entry: Option<&bcode_provider_auth::auth_pool_state::AuthPoolProfileState>,
    now: u64,
) -> Vec<AuthPrimeWindowReport> {
    let Some(entry) = entry else {
        return Vec::new();
    };
    entry
        .usage_windows
        .iter()
        .flat_map(|(meter_id, windows)| {
            windows.iter().map(|(window_id, window)| {
                auth_usage_window_report(meter_id, window_id, window, now)
            })
        })
        .collect()
}

fn auth_usage_window_report(
    meter_id: &str,
    window_id: &str,
    window: &bcode_provider_auth::auth_pool_state::AuthPoolUsageWindowState,
    now: u64,
) -> AuthPrimeWindowReport {
    let status = if window
        .resets_at_unix
        .is_some_and(|resets_at| resets_at <= now)
    {
        "expired"
    } else {
        "available"
    };
    let detail = if status == "expired" {
        "provider usage window has reset".to_string()
    } else {
        usage_detail(window, now)
    };
    AuthPrimeWindowReport {
        meter_id: meter_id.to_string(),
        window_id: window_id.to_string(),
        status: status.to_string(),
        used_percent: window.used_percent,
        window_duration_secs: window.window_duration_secs,
        resets_at_unix: window.resets_at_unix,
        observed_at_unix: Some(window.observed_at_unix),
        primed_at_unix: window.primed_at_unix,
        source: window.source.clone(),
        detail,
    }
}

fn auth_prime_window_reports(
    required_windows: &BTreeMap<String, Vec<String>>,
    entry: Option<&bcode_provider_auth::auth_pool_state::AuthPoolProfileState>,
    now: u64,
) -> Vec<AuthPrimeWindowReport> {
    let mut targets = BTreeSet::<(String, String)>::new();
    for (meter_id, windows) in required_windows {
        for window_id in windows {
            targets.insert((meter_id.clone(), window_id.clone()));
        }
    }
    if targets.is_empty()
        && let Some(entry) = entry
    {
        for (meter_id, windows) in &entry.usage_windows {
            for window_id in windows.keys() {
                targets.insert((meter_id.clone(), window_id.clone()));
            }
        }
    }
    targets
        .into_iter()
        .map(|(meter_id, window_id)| {
            let window = entry
                .and_then(|entry| entry.usage_windows.get(&meter_id))
                .and_then(|windows| windows.get(&window_id));
            auth_prime_window_report(&meter_id, &window_id, window, now)
        })
        .collect()
}

fn auth_prime_window_report(
    meter_id: &str,
    window_id: &str,
    window: Option<&bcode_provider_auth::auth_pool_state::AuthPoolUsageWindowState>,
    now: u64,
) -> AuthPrimeWindowReport {
    let (status, detail) = match window {
        None => ("missing", "no provider usage snapshot".to_string()),
        Some(window)
            if window
                .resets_at_unix
                .is_some_and(|resets_at| resets_at <= now) =>
        {
            ("expired", "provider usage window has reset".to_string())
        }
        Some(window) if window.used_percent.is_some_and(|percent| percent > 0) => {
            ("active", usage_detail(window, now))
        }
        Some(window) => (
            "needs_priming",
            format!(
                "{}; provider reports 0% used and no local prime touch",
                usage_detail(window, now)
            ),
        ),
    };
    AuthPrimeWindowReport {
        meter_id: meter_id.to_string(),
        window_id: window_id.to_string(),
        status: status.to_string(),
        used_percent: window.and_then(|window| window.used_percent),
        window_duration_secs: window.and_then(|window| window.window_duration_secs),
        resets_at_unix: window.and_then(|window| window.resets_at_unix),
        observed_at_unix: window.map(|window| window.observed_at_unix),
        primed_at_unix: window.and_then(|window| window.primed_at_unix),
        source: window.and_then(|window| window.source.clone()),
        detail,
    }
}

fn usage_detail(
    window: &bcode_provider_auth::auth_pool_state::AuthPoolUsageWindowState,
    now: u64,
) -> String {
    let mut parts = Vec::new();
    if let Some(used_percent) = window.used_percent {
        parts.push(format!(
            "{used_percent}% used / {}% remaining",
            100_u32.saturating_sub(used_percent)
        ));
    }
    if let Some(duration) = window.window_duration_secs {
        parts.push(format!("{} window", format_duration(duration)));
    }
    if let Some(resets_at) = window.resets_at_unix {
        parts.push(format!(
            "resets at {} (in {})",
            format_unix_timestamp(resets_at),
            format_duration(resets_at.saturating_sub(now))
        ));
    }
    if parts.is_empty() {
        "provider usage window is active".to_string()
    } else {
        parts.join(", ")
    }
}

fn print_auth_resets_report(
    report: &AuthResetsReport,
    verbose: bool,
    json: bool,
) -> Result<(), CliError> {
    if json {
        return print_json(report);
    }

    if report.profiles.len() == 1 {
        print_single_auth_resets_profile(report, &report.profiles[0], verbose);
    } else {
        print_auth_resets_profile_summary(report);
    }
    Ok(())
}

fn print_auth_resets_profile_summary(report: &AuthResetsReport) {
    println!("Banked Codex resets: {}", report.pool);
    println!();
    println!("PROFILE\tAVAILABLE\tNEXT EXPIRY\tSTATUS");
    for profile in &report.profiles {
        let available = profile
            .available_count
            .map_or_else(|| "-".to_string(), |count| count.to_string());
        let next_expiry = next_reset_credit_expiry(profile).map_or_else(
            || "-".to_string(),
            |expiry| format_reset_credit_date(Some(expiry)),
        );
        println!(
            "{}\t{}\t{}\t{}",
            profile.profile, available, next_expiry, profile.status
        );
        if let Some(reason) = &profile.reason {
            println!("  {reason}");
        }
    }
    println!();
    println!("Run with --profile <name> to see individual reset credits.");
}

fn print_single_auth_resets_profile(
    report: &AuthResetsReport,
    profile: &AuthResetsProfileReport,
    verbose: bool,
) {
    println!("Banked Codex resets: {} / {}", report.pool, profile.profile);
    if profile.status == "unsupported" {
        println!();
        println!("Banked Codex resets are not supported for this auth profile.");
        if let Some(reason) = &profile.reason {
            println!("Reason: {reason}");
        }
        return;
    }
    if profile.status == "error" {
        println!();
        println!("Could not load reset credits for {}.", profile.profile);
        if let Some(reason) = &profile.reason {
            println!("Error: {reason}");
        }
        return;
    }

    let available = profile.available_count.unwrap_or(0);
    println!();
    println!("Available resets: {available}");
    if available == 0 || profile.credits.is_empty() {
        println!();
        println!("No banked Codex resets are available for this profile.");
        return;
    }

    println!();
    if verbose || reset_credit_output_should_use_blocks() {
        print_auth_reset_credit_blocks(profile, verbose);
    } else {
        print_auth_reset_credit_table(profile);
    }

    println!();
    println!("Use one:");
    println!(
        "  bcode auth resets use {} --profile {}",
        report.pool, profile.profile
    );
    println!();
    println!("Use a specific reset:");
    println!(
        "  bcode auth resets use {} --profile {} --credit <id>",
        report.pool, profile.profile
    );
}

fn print_auth_reset_credit_table(profile: &AuthResetsProfileReport) {
    println!("RESET\tEXPIRES\tSTATUS\tDESCRIPTION");
    for (index, credit) in profile.credits.iter().enumerate() {
        println!(
            "#{}\t{}\t{}\t{}",
            index + 1,
            format_reset_credit_date(credit.expires_at.as_deref()),
            credit.status,
            reset_credit_description(credit)
        );
    }
}

fn print_auth_reset_credit_blocks(profile: &AuthResetsProfileReport, verbose: bool) {
    for (index, credit) in profile.credits.iter().enumerate() {
        println!("#{} {}", index + 1, credit.status);
        println!(
            "  Expires: {}",
            format_reset_credit_date(credit.expires_at.as_deref())
        );
        println!("  Description: {}", reset_credit_description(credit));
        println!("  ID: {}", credit.credit_id);
        if verbose {
            println!("  Type: {}", credit.reset_type);
            println!(
                "  Granted: {}",
                format_reset_credit_date(Some(&credit.granted_at))
            );
        } else if reset_credit_type_label(&credit.reset_type).is_some() {
            println!("  Type: {}", credit.reset_type);
        }
        println!();
    }
}

fn reset_credit_output_should_use_blocks() -> bool {
    terminal_width().is_some_and(|width| width < 90)
}

fn terminal_width() -> Option<u16> {
    crossterm::terminal::size().ok().map(|(columns, _)| columns)
}

fn next_reset_credit_expiry(profile: &AuthResetsProfileReport) -> Option<&str> {
    profile
        .credits
        .iter()
        .filter_map(|credit| credit.expires_at.as_deref())
        .min()
}

fn format_reset_credit_date(timestamp: Option<&str>) -> String {
    let Some(timestamp) = timestamp else {
        return "-".to_string();
    };
    timestamp
        .get(..10)
        .filter(|date| {
            let bytes = date.as_bytes();
            bytes.len() == 10
                && bytes[4] == b'-'
                && bytes[7] == b'-'
                && bytes
                    .iter()
                    .enumerate()
                    .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
        })
        .map_or_else(|| timestamp.to_string(), ToString::to_string)
}

fn reset_credit_description(credit: &AuthResetCreditReport) -> &str {
    credit
        .title
        .as_deref()
        .or(credit.description.as_deref())
        .unwrap_or("-")
}

fn reset_credit_type_label(reset_type: &str) -> Option<&str> {
    (reset_type != "codex_rate_limits").then_some(reset_type)
}

fn print_auth_reset_consume_report(
    report: &AuthResetConsumeReport,
    json: bool,
) -> Result<(), CliError> {
    write_auth_reset_consume_report(&mut std::io::stdout().lock(), report, json)
}

fn write_auth_reset_consume_report(
    output: &mut impl std::io::Write,
    report: &AuthResetConsumeReport,
    json: bool,
) -> Result<(), CliError> {
    if json {
        return write_json_result(output, report);
    }
    if report.dry_run {
        writeln!(output, "Dry run: no reset was consumed.")?;
        writeln!(output)?;
        writeln!(output, "Would use:")?;
        writeln!(output, "  Profile: {}", report.profile)?;
        writeln!(
            output,
            "  Credit: {}",
            report.credit_id.as_deref().unwrap_or("provider-selected")
        )?;
    } else {
        match report.status.as_str() {
            "reset" => {
                writeln!(
                    output,
                    "Used one banked Codex reset for {}.",
                    report.profile
                )?;
                writeln!(output)?;
                if let Some(windows_reset) = report.windows_reset {
                    writeln!(output, "Windows reset: {windows_reset}")?;
                }
                writeln!(output, "Provider result: reset")?;
            }
            "nothing_to_reset" => {
                writeln!(output, "No reset was used.")?;
                writeln!(output)?;
                writeln!(
                    output,
                    "Reason: no current rate-limit window is eligible for reset."
                )?;
                writeln!(output, "Your banked reset should still be available.")?;
            }
            "no_credit" => {
                writeln!(
                    output,
                    "No banked reset is available for {}.",
                    report.profile
                )?;
            }
            "already_redeemed" => {
                writeln!(
                    output,
                    "This reset request already completed successfully for {}.",
                    report.profile
                )?;
            }
            _ => {
                writeln!(output, "Reset consume status: {}", report.status)?;
                if let Some(message) = &report.message {
                    writeln!(output, "Detail: {message}")?;
                }
            }
        }
    }
    output.flush()?;
    Ok(())
}

fn print_auth_usage_report(report: &AuthUsageReport, json: bool) -> Result<(), CliError> {
    if json {
        return print_json(report);
    }
    println!("Auth usage: {}", report.pool);
    println!("Provider plugin: {}", report.provider_plugin_id);
    if report.refreshed {
        println!("Usage windows: refreshed");
        println!("Debug metadata is included in `--json` output.");
    }
    println!();
    println!("PROFILE\tSTATUS\tDETAIL");
    for profile in &report.profiles {
        let detail = profile.reason.as_deref().unwrap_or("-");
        println!("{}\t{}\t{}", profile.profile, profile.status, detail);
        for window in &profile.windows {
            println!(
                "  {}.{}\t{}\t{}",
                window.meter_id, window.window_id, window.status, window.detail
            );
        }
    }
    Ok(())
}

fn print_auth_prime_report(report: &AuthPrimeReport, json: bool) -> Result<(), CliError> {
    if json {
        return print_json(report);
    }
    println!("Prime status: {}", report.pool);
    println!("Provider plugin: {}", report.provider_plugin_id);
    if report.dry_run {
        println!("Mode: dry run");
    }
    if report.refreshed {
        println!("Usage windows: refreshed");
        println!("Debug metadata is included in `--json` output.");
    }
    println!();
    println!("PROFILE\tSTATUS\tDETAIL");
    for profile in &report.profiles {
        let detail = profile.reason.as_deref().unwrap_or("-");
        println!("{}\t{}\t{}", profile.profile, profile.status, detail);
        if profile.limit_hit || profile.failure_code.is_some() {
            println!("  ERROR: priming did not complete for {}.", profile.profile);
            if let Some(failure_code) = &profile.failure_code {
                println!("  Failure code: {failure_code}");
            }
            println!("  Attempts: {}", profile.attempts);
            if !profile.remaining_windows.is_empty() {
                println!(
                    "  Remaining windows: {}",
                    profile.remaining_windows.join(", ")
                );
            }
            if let Some(diagnostic) = &profile.diagnostic {
                println!("  Diagnostic: {diagnostic}");
            }
        } else if profile.attempts > 0 {
            println!("  Attempts: {}", profile.attempts);
        }
        for window in &profile.windows {
            println!(
                "  {}.{}\t{}\t{}",
                window.meter_id, window.window_id, window.status, window.detail
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthProfileSummary {
    profile: String,
    source: &'static str,
    backend: String,
    scheme: Option<String>,
    provider: Option<String>,
    storage_profile: Option<String>,
    vault: Option<PathBuf>,
}

fn auth_profile_summaries(config: &bcode_config::BcodeConfig) -> Vec<AuthProfileSummary> {
    let registry = bcode_config::load_runtime_auth_subscriptions();
    let mut summaries = Vec::new();
    let mut seen = BTreeSet::new();
    for (profile, auth_profile) in &config.auth.profiles {
        seen.insert(profile.clone());
        summaries.push(AuthProfileSummary {
            profile: profile.clone(),
            source: "declared",
            backend: auth_profile.backend.clone(),
            scheme: auth_profile.scheme.clone(),
            provider: auth_profile.settings.get("provider").cloned(),
            storage_profile: auth_profile.settings.get("profile").cloned(),
            vault: auth_profile.settings.get("vault").map(PathBuf::from),
        });
    }
    for pool in registry.pools.values() {
        for profile in &pool.profiles {
            if !seen.insert(profile.auth_profile.clone()) {
                continue;
            }
            summaries.push(AuthProfileSummary {
                profile: profile.auth_profile.clone(),
                source: "runtime",
                backend: "sshenv".to_string(),
                scheme: Some(profile.scheme.clone()),
                provider: Some(profile.provider.clone()),
                storage_profile: Some(profile.storage_profile.clone()),
                vault: Some(profile.vault.clone()),
            });
        }
    }
    summaries.sort_by(|left, right| left.profile.cmp(&right.profile));
    summaries
}

fn auth_profile_list() -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    let summaries = auth_profile_summaries(&config);
    if summaries.is_empty() {
        println!("No auth profiles declared or registered.");
        return Ok(());
    }
    println!("PROFILE\tSOURCE\tBACKEND\tSCHEME\tPROVIDER\tSTORAGE\tVAULT");
    for summary in summaries {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            summary.profile,
            summary.source,
            summary.backend,
            summary.scheme.as_deref().unwrap_or("-"),
            summary.provider.as_deref().unwrap_or("-"),
            summary.storage_profile.as_deref().unwrap_or("-"),
            summary.vault.as_ref().map_or_else(
                || "-".to_string(),
                |vault| display_from_current_dir(vault).to_string()
            )
        );
    }
    Ok(())
}

fn auth_profile_show(profile: &str) -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    let Some(summary) = auth_profile_summaries(&config)
        .into_iter()
        .find(|summary| summary.profile == profile)
    else {
        println!("Auth profile '{profile}' is not declared or registered.");
        return Ok(());
    };
    println!("Auth profile: {}", summary.profile);
    println!("Source: {}", summary.source);
    println!("Backend: {}", summary.backend);
    if let Some(scheme) = summary.scheme {
        println!("Scheme: {scheme}");
    }
    if let Some(provider) = summary.provider {
        println!("Provider: {provider}");
    }
    if let Some(storage_profile) = summary.storage_profile {
        println!("Storage profile: {storage_profile}");
    }
    if let Some(vault) = summary.vault {
        println!("Vault: {}", display_from_current_dir(&vault));
    }
    Ok(())
}

fn load_openai_auth_pool_state() -> bcode_provider_auth::auth_pool_state::AuthPoolState {
    bcode_provider_auth::auth_pool_state::load_state()
}

fn auth_pool_list() -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    let registry = bcode_config::load_runtime_auth_subscriptions();
    let names = config
        .auth
        .pools
        .keys()
        .chain(registry.pools.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    if names.is_empty() {
        println!("No auth pools declared or registered.");
        return Ok(());
    }
    for name in names {
        let declared_count = config
            .auth
            .pools
            .get(&name)
            .map_or(0, |pool| pool.profiles.len());
        let runtime_count = registry
            .pools
            .get(&name)
            .map_or(0, |pool| pool.profiles.len());
        println!(
            "{name}: {declared_count} declared profile(s), {runtime_count} runtime subscription(s)"
        );
    }
    Ok(())
}

fn auth_pool_status(pool_name: &str) -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    let registry = bcode_config::load_runtime_auth_subscriptions();
    let declared_pool = config.auth.pools.get(pool_name);
    let runtime_pool = registry.pools.get(pool_name);
    if declared_pool.is_none() && runtime_pool.is_none() {
        println!("Auth pool '{pool_name}' is not declared or registered.");
        return Ok(());
    }
    println!("Auth pool: {pool_name}");
    if let Some(provider_plugin_id) = declared_pool
        .and_then(|pool| pool.provider_plugin_id.as_ref())
        .or_else(|| runtime_pool.and_then(|pool| pool.provider_plugin_id.as_ref()))
    {
        println!("Provider plugin: {provider_plugin_id}");
    }
    if let Some(pool) = declared_pool {
        println!("Strategy: {:?}", pool.strategy);
        println!(
            "Priming: {}{}{}",
            if pool.priming.enabled {
                "enabled"
            } else {
                "disabled"
            },
            if pool.priming.include_primary {
                ", includes primary"
            } else {
                ""
            },
            pool.priming
                .reprime_after
                .as_ref()
                .map_or_else(String::new, |duration| format!(
                    ", reprime after {duration}"
                ))
        );
    }
    let resolved = config.resolved_model_selection();
    let selected_profile = (resolved.auth_pool.as_deref() == Some(pool_name))
        .then_some(resolved.auth_profile.as_deref())
        .flatten();
    let order =
        bcode_config::effective_auth_pool_order(&config, &registry, pool_name, selected_profile);
    println!(
        "Preferred profile: {}{}",
        order.preferred_profile.as_deref().unwrap_or("none"),
        order
            .preference_source
            .as_deref()
            .map_or_else(String::new, |source| format!(" ({source})"))
    );
    println!("Effective order: {}", order.profiles.join(" -> "));
    if let Some(reason) = &order.degraded_reason {
        println!("Degraded: {reason}");
    }
    let profiles = order.profiles;
    let runtime_profiles = Vec::new();
    if profiles.is_empty() && runtime_profiles.is_empty() {
        println!("Profiles: none");
        return Ok(());
    }
    let state = load_openai_auth_pool_state();
    if let Some(last_selected_profile) = state
        .pools
        .get(pool_name)
        .and_then(|pool| pool.last_selected_profile.as_ref())
    {
        println!("Runtime routing: last selected profile {last_selected_profile}");
    }
    let now = unix_now_secs();
    println!("Profiles:");
    for profile in profiles {
        print_auth_pool_profile_status(&config, pool_name, &profile, "declared", &state, now);
    }
    for profile in runtime_profiles {
        if declared_pool.is_some_and(|pool| pool.profiles.contains(&profile)) {
            continue;
        }
        print_auth_pool_profile_status(&config, pool_name, &profile, "runtime", &state, now);
    }
    Ok(())
}

fn print_auth_pool_profile_status(
    config: &bcode_config::BcodeConfig,
    pool_name: &str,
    profile: &str,
    source: &str,
    state: &bcode_provider_auth::auth_pool_state::AuthPoolState,
    now: u64,
) {
    let config_status = if config.auth.profiles.contains_key(profile) {
        "configured"
    } else if source == "runtime" {
        "registered"
    } else {
        "missing"
    };
    let key = format!("{pool_name}/{profile}");
    let last_success = state
        .entries
        .get(&key)
        .and_then(|entry| entry.last_success_unix)
        .map_or_else(
            || "never used".to_string(),
            |timestamp| {
                format!(
                    "last success {} ago",
                    format_duration(now.saturating_sub(timestamp))
                )
            },
        );
    let priming = state
        .entries
        .get(&key)
        .and_then(|entry| entry.primed_unix)
        .map_or("unprimed", |_| "primed");
    if let Some(entry) = state.entries.get(&key)
        && entry.cooldown_until_unix > now
    {
        println!(
            "  {profile}: {source}, {config_status}, storage {storage}, vault {vault}, {last_success}, {priming}, cooldown {} remaining, reason: {}",
            format_duration(entry.cooldown_until_unix.saturating_sub(now)),
            entry.reason,
            storage = auth_pool_profile_storage(config, profile).unwrap_or_else(|| "-".to_string()),
            vault = auth_pool_profile_vault(config, profile).unwrap_or_else(|| "-".to_string()),
        );
        return;
    }
    println!(
        "  {profile}: {source}, {config_status}, storage {storage}, vault {vault}, available, {last_success}, {priming}",
        storage = auth_pool_profile_storage(config, profile).unwrap_or_else(|| "-".to_string()),
        vault = auth_pool_profile_vault(config, profile).unwrap_or_else(|| "-".to_string()),
    );
}

fn auth_pool_profile_storage(config: &bcode_config::BcodeConfig, profile: &str) -> Option<String> {
    auth_profile_summaries(config)
        .into_iter()
        .find(|summary| summary.profile == profile)
        .and_then(|summary| summary.storage_profile)
}

fn auth_pool_profile_vault(config: &bcode_config::BcodeConfig, profile: &str) -> Option<String> {
    auth_profile_summaries(config)
        .into_iter()
        .find(|summary| summary.profile == profile)
        .and_then(|summary| summary.vault)
        .map(|vault| display_from_current_dir(&vault).to_string())
}

mod credential_setup;

fn auth_providers() -> Result<(), CliError> {
    let mut host = load_cli_plugin_host()?;
    for provider in host.auth_provider_registry().providers() {
        println!(
            "{}\t{}\t{}\t{}",
            provider.contribution.provider_id,
            provider.contribution.display_name,
            provider.plugin_id,
            provider
                .contribution
                .methods
                .iter()
                .map(bcode_provider_auth_models::AuthMethodContribution::method_id)
                .collect::<Vec<_>>()
                .join(",")
        );
    }
    host.deactivate_all()?;
    Ok(())
}

fn registered_auth_provider(
    host: &bcode_plugin::PluginHost,
    provider_id: &str,
) -> Result<bcode_plugin::RegisteredAuthProvider, CliError> {
    host.auth_provider(provider_id).cloned().ok_or_else(|| {
        CliError::LoginProfile(format!(
            "Authentication provider '{provider_id}' is not registered by an enabled plugin. Run `bcode auth providers`."
        ))
    })
}

fn selected_auth_method<'a>(
    provider: &'a bcode_plugin::RegisteredAuthProvider,
    requested_method: Option<&str>,
) -> Result<&'a bcode_provider_auth_models::AuthMethodContribution, CliError> {
    if let Some(method_id) = requested_method {
        return provider
            .contribution
            .methods
            .iter()
            .find(|method| method.method_id() == method_id)
            .ok_or_else(|| {
                CliError::LoginProfile(format!(
                    "Authentication method '{method_id}' is not registered for provider '{}'.",
                    provider.contribution.provider_id
                ))
            });
    }
    if provider.contribution.methods.len() == 1 {
        return Ok(&provider.contribution.methods[0]);
    }
    Err(CliError::LoginProfile(format!(
        "Provider '{}' has multiple authentication methods; pass --method with one of: {}",
        provider.contribution.provider_id,
        provider
            .contribution
            .methods
            .iter()
            .map(bcode_provider_auth_models::AuthMethodContribution::method_id)
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

fn resolved_auth_method<'a>(
    provider: &'a bcode_plugin::RegisteredAuthProvider,
    resolved: &bcode_provider_auth::ResolvedAuthProfile,
) -> Result<&'a bcode_provider_auth_models::AuthMethodContribution, CliError> {
    let scheme = resolved.profile.scheme.as_deref().ok_or_else(|| {
        CliError::LoginProfile(format!(
            "Auth profile '{}' has no authentication scheme.",
            resolved.profile_name
        ))
    })?;
    selected_auth_method(provider, Some(scheme))
}

fn owned_ambient_auth_profile_hint(
    config: &bcode_config::BcodeConfig,
    runtime: &bcode_config::RuntimeAuthSubscriptions,
    provider: &bcode_plugin::RegisteredAuthProvider,
    profile: Option<String>,
) -> Option<String> {
    let profile = profile.filter(|profile| !profile.trim().is_empty())?;
    bcode_provider_auth::resolve_auth_provider_profile(
        config,
        &provider.contribution.provider_id,
        &provider.plugin_id,
        Some(&profile),
        runtime,
    )
    .is_ok()
    .then_some(profile)
}

fn registered_auth_profile_hint_from(
    config: &bcode_config::BcodeConfig,
    runtime: &bcode_config::RuntimeAuthSubscriptions,
    provider: &bcode_plugin::RegisteredAuthProvider,
    explicit_profile: Option<&str>,
    ambient_auth_profile: Option<String>,
) -> Option<String> {
    if let Some(profile) = explicit_profile {
        return Some(profile.to_owned());
    }
    if let Some(profile) =
        owned_ambient_auth_profile_hint(config, runtime, provider, ambient_auth_profile)
    {
        return Some(profile);
    }
    let selection = config.model.profile.as_deref().map_or_else(
        || config.resolved_model_selection(),
        |profile| {
            config
                .resolved_model_profile(profile)
                .unwrap_or_else(|| config.resolved_model_selection())
        },
    );
    owned_ambient_auth_profile_hint(config, runtime, provider, selection.auth_profile)
}

fn registered_auth_profile_hint(
    config: &bcode_config::BcodeConfig,
    runtime: &bcode_config::RuntimeAuthSubscriptions,
    provider: &bcode_plugin::RegisteredAuthProvider,
    explicit_profile: Option<&str>,
) -> Option<String> {
    registered_auth_profile_hint_from(
        config,
        runtime,
        provider,
        explicit_profile,
        std::env::var(bcode_config::BCODE_AUTH_PROFILE_ENV).ok(),
    )
}

fn lookup_registered_auth_profile_from(
    config: &bcode_config::BcodeConfig,
    runtime: &bcode_config::RuntimeAuthSubscriptions,
    provider: &bcode_plugin::RegisteredAuthProvider,
    explicit_profile: Option<&str>,
) -> Result<bcode_provider_auth::AuthProviderProfileLookup, CliError> {
    let profile_hint = registered_auth_profile_hint(config, runtime, provider, explicit_profile);
    bcode_provider_auth::lookup_auth_provider_profile(
        config,
        &provider.contribution.provider_id,
        &provider.plugin_id,
        profile_hint.as_deref(),
        runtime,
    )
    .map_err(|error| CliError::LoginProfile(error.to_string()))
}

fn resolve_registered_auth_profile_from(
    config: &bcode_config::BcodeConfig,
    runtime: &bcode_config::RuntimeAuthSubscriptions,
    provider: &bcode_plugin::RegisteredAuthProvider,
    explicit_profile: Option<&str>,
) -> Result<bcode_provider_auth::ResolvedAuthProfile, CliError> {
    let profile_hint = registered_auth_profile_hint(config, runtime, provider, explicit_profile);
    bcode_provider_auth::resolve_auth_provider_profile(
        config,
        &provider.contribution.provider_id,
        &provider.plugin_id,
        profile_hint.as_deref(),
        runtime,
    )
    .map_err(|error| CliError::LoginProfile(error.to_string()))
}

fn resolve_registered_auth_profile(
    provider: &bcode_plugin::RegisteredAuthProvider,
    explicit_profile: Option<&str>,
) -> Result<bcode_provider_auth::ResolvedAuthProfile, CliError> {
    let config = bcode_config::load_config()?;
    let runtime = bcode_config::load_runtime_auth_subscriptions();
    resolve_registered_auth_profile_from(&config, &runtime, provider, explicit_profile)
}

fn resolve_or_prepare_auth_profile_from(
    config: &bcode_config::BcodeConfig,
    runtime: &bcode_config::RuntimeAuthSubscriptions,
    provider: &bcode_plugin::RegisteredAuthProvider,
    method: &bcode_provider_auth_models::AuthMethodContribution,
    explicit_profile: Option<&str>,
    explicit_vault: Option<PathBuf>,
    recipient_key: Option<&str>,
) -> Result<(bcode_provider_auth::ResolvedAuthProfile, bool), CliError> {
    let profile_hint = registered_auth_profile_hint(config, runtime, provider, explicit_profile);
    let prepared = bcode_provider_auth::enrollment::prepare(
        config,
        runtime,
        &provider.contribution,
        &provider.plugin_id,
        method.method_id(),
        bcode_provider_auth::enrollment::EnrollmentDestination {
            profile: profile_hint,
            vault: explicit_vault,
            recipient_key: recipient_key.map(str::to_owned),
        },
    )
    .map_err(|error| CliError::LoginProfile(error.to_string()))?;
    Ok((prepared.resolved, prepared.publish_runtime))
}

fn resolve_or_prepare_auth_profile(
    provider: &bcode_plugin::RegisteredAuthProvider,
    method: &bcode_provider_auth_models::AuthMethodContribution,
    explicit_profile: Option<&str>,
    explicit_vault: Option<PathBuf>,
    recipient_key: Option<&str>,
) -> Result<(bcode_provider_auth::ResolvedAuthProfile, bool), CliError> {
    let config = bcode_config::load_config()?;
    let runtime = bcode_config::try_load_runtime_auth_subscriptions()?;
    resolve_or_prepare_auth_profile_from(
        &config,
        &runtime,
        provider,
        method,
        explicit_profile,
        explicit_vault,
        recipient_key,
    )
}

fn runtime_pool_profile(
    resolved: &bcode_provider_auth::ResolvedAuthProfile,
) -> bcode_config::RuntimeAuthSubscriptionProfile {
    let storage_profile = resolved
        .profile
        .settings
        .get("profile")
        .cloned()
        .unwrap_or_else(|| resolved.profile_name.clone());
    let vault = resolved
        .profile
        .settings
        .get("vault")
        .map_or_else(bcode_config::default_auth_vault_path, PathBuf::from);
    bcode_config::RuntimeAuthSubscriptionProfile {
        auth_profile: resolved.profile_name.clone(),
        storage_profile,
        vault,
        provider: resolved.provider_id.clone(),
        scheme: resolved.profile.scheme.clone().unwrap_or_default(),
        owner_plugin_id: Some(resolved.owner_plugin_id.clone()),
        map: resolved.profile.map.clone(),
        device_seal: resolved.profile.settings.get("device_seal").cloned(),
    }
}

fn persist_runtime_pool_profile(
    pool: &str,
    resolved: &bcode_provider_auth::ResolvedAuthProfile,
) -> Result<(), CliError> {
    bcode_config::register_runtime_auth_subscription(pool, runtime_pool_profile(resolved))?;
    Ok(())
}

fn persist_prepared_runtime_profile(
    resolved: &bcode_provider_auth::ResolvedAuthProfile,
) -> Result<(), CliError> {
    bcode_provider_auth::enrollment::publish(resolved)?;
    Ok(())
}

struct AuthProviderLoginOptions<'a> {
    explicit_profile: Option<&'a str>,
    explicit_vault: Option<PathBuf>,
    recipient_key: Option<&'a str>,
    no_device_seal: bool,
    pool: Option<&'a str>,
    requested_method: Option<&'a str>,
    verify: bool,
}

struct AuthProviderLoginResult {
    resolved: bcode_provider_auth::ResolvedAuthProfile,
    persisted_runtime: bool,
}

fn compatible_login_profile(
    provider_id: &str,
    explicit_profile: Option<&str>,
    allocate_pool_profile: bool,
) -> Result<String, CliError> {
    if let Some(profile) = explicit_profile {
        return Ok(profile.to_owned());
    }
    if !allocate_pool_profile {
        return Ok(provider_id.to_owned());
    }
    let config = bcode_config::load_config()?;
    let runtime = bcode_config::load_runtime_auth_subscriptions();
    let profile = next_compatible_pool_profile(&config, &runtime, provider_id, None);
    println!("Adding new OpenAI subscription auth profile '{profile}'.");
    Ok(profile)
}

fn next_compatible_pool_profile(
    config: &bcode_config::BcodeConfig,
    runtime: &bcode_config::RuntimeAuthSubscriptions,
    provider_id: &str,
    explicit_profile: Option<&str>,
) -> String {
    if let Some(profile) = explicit_profile {
        return profile.to_owned();
    }
    for index in 2_u64.. {
        let candidate = format!("{provider_id}-{index}");
        if !config.auth.profiles.contains_key(&candidate)
            && !runtime.profiles.contains_key(&candidate)
            && !runtime.pools.values().any(|pool| {
                pool.profiles
                    .iter()
                    .any(|profile| profile.auth_profile == candidate)
            })
        {
            return candidate;
        }
    }
    unreachable!("unbounded subscription profile search should return")
}

async fn enroll_registered_auth_provider(
    provider_id: &str,
    options: AuthProviderLoginOptions<'_>,
    supplied: BTreeMap<String, String>,
    replace_owned: bool,
) -> Result<AuthProviderLoginResult, CliError> {
    let mut host = load_cli_plugin_host()?;
    let result =
        enroll_auth_provider_with_host(&host, provider_id, options, supplied, replace_owned).await;
    let cleanup = host.deactivate_all().map_err(CliError::from);
    result.and_then(|result| cleanup.map(|()| result))
}

async fn enroll_auth_provider_with_host(
    host: &bcode_plugin::PluginHost,
    provider_id: &str,
    options: AuthProviderLoginOptions<'_>,
    supplied: BTreeMap<String, String>,
    replace_owned: bool,
) -> Result<AuthProviderLoginResult, CliError> {
    let AuthProviderLoginOptions {
        explicit_profile,
        explicit_vault,
        recipient_key,
        no_device_seal,
        pool,
        requested_method,
        verify,
    } = options;
    let provider = registered_auth_provider(host, provider_id)?;
    let method = selected_auth_method(&provider, requested_method)?;
    let (mut resolved, persist_runtime) = resolve_or_prepare_auth_profile(
        &provider,
        method,
        explicit_profile,
        explicit_vault,
        recipient_key,
    )?;
    if no_device_seal {
        resolved
            .profile
            .settings
            .insert("device_seal".to_owned(), "off".to_owned());
    }
    match method {
        bcode_provider_auth_models::AuthMethodContribution::SecretFields {
            supports_verification,
            ..
        } => {
            if verify && !supports_verification {
                return Err(CliError::LoginProfile(format!(
                    "Provider '{provider_id}' does not support credential verification."
                )));
            }
            enroll_registered_secret_values(
                provider_id,
                &provider,
                method,
                &resolved,
                supplied,
                replace_owned,
            )?;
            if verify {
                run_auth_interactive_flow(host, &provider, method, &resolved, true, false).await?;
            }
        }
        bcode_provider_auth_models::AuthMethodContribution::Interactive { .. } => {
            if !supplied.is_empty() {
                return Err(CliError::LoginProfile(format!(
                    "Provider '{provider_id}' selected method '{}' does not accept supplied secret fields.",
                    method.method_id()
                )));
            }
            run_auth_interactive_flow(host, &provider, method, &resolved, verify, false).await?;
        }
    }
    if let Some(pool) = pool {
        persist_runtime_pool_profile(pool, &resolved)?;
    } else if persist_runtime {
        persist_prepared_runtime_profile(&resolved)?;
    }
    Ok(AuthProviderLoginResult {
        resolved,
        persisted_runtime: pool.is_some() || persist_runtime,
    })
}

async fn auth_provider_login(
    provider_id: &str,
    options: AuthProviderLoginOptions<'_>,
) -> Result<(), CliError> {
    let result =
        enroll_registered_auth_provider(provider_id, options, BTreeMap::new(), false).await?;
    println!(
        "Authentication saved for provider '{provider_id}' in profile '{}'.",
        result.resolved.profile_name
    );
    Ok(())
}

async fn auth_provider_logout(
    provider_id: &str,
    explicit_profile: Option<&str>,
    revoke: bool,
) -> Result<(), CliError> {
    let mut host = load_cli_plugin_host()?;
    let result = logout_auth_provider(&host, provider_id, explicit_profile, revoke).await;
    let cleanup = host.deactivate_all().map_err(CliError::from);
    result.and(cleanup)
}

async fn logout_auth_provider(
    host: &bcode_plugin::PluginHost,
    provider_id: &str,
    explicit_profile: Option<&str>,
    revoke: bool,
) -> Result<(), CliError> {
    let provider = registered_auth_provider(host, provider_id)?;
    let resolved = resolve_registered_auth_profile(&provider, explicit_profile)?;
    let method = resolved_auth_method(&provider, &resolved)?;
    if revoke {
        let supports = match method {
            bcode_provider_auth_models::AuthMethodContribution::SecretFields {
                supports_revocation,
                ..
            }
            | bcode_provider_auth_models::AuthMethodContribution::Interactive {
                supports_revocation,
                ..
            } => *supports_revocation,
        };
        if !supports {
            return Err(CliError::LoginProfile(format!(
                "Provider '{provider_id}' does not support remote revocation."
            )));
        }
        run_auth_interactive_flow(host, &provider, method, &resolved, false, true).await?;
    }
    bcode_provider_auth::lifecycle::AuthVaultLifecycle::new(
        &resolved,
        provider_id,
        &provider.plugin_id,
        method,
    )
    .map_err(|error| CliError::LoginProfile(error.to_string()))?
    .delete()
    .map_err(|error| CliError::LoginProfile(error.to_string()))?;
    // Runtime-registered profiles also carry non-secret routing metadata (pool membership,
    // bindings). Leaving it behind would keep routing turns at a profile with no credentials.
    let removal = if resolved.source == bcode_provider_auth::AuthProfileSource::Runtime {
        bcode_config::remove_runtime_auth_profile(&resolved.profile_name, &provider.plugin_id)?
    } else {
        bcode_config::RuntimeAuthProfileRemoval::default()
    };
    let mut writer = std::io::stdout().lock();
    writeln!(
        writer,
        "Local authentication removed for provider '{provider_id}'."
    )?;
    if !removal.removed_from_pools.is_empty() {
        writeln!(
            writer,
            "Removed profile '{}' from auth pool(s): {}.",
            resolved.profile_name,
            removal.removed_from_pools.join(", ")
        )?;
    }
    writer.flush()?;
    Ok(())
}

fn unconfigured_auth_provider_status_lines(
    provider: &bcode_plugin::RegisteredAuthProvider,
    profile_name: &str,
) -> Vec<String> {
    vec![
        format!("Provider: {}", provider.contribution.display_name),
        format!("Plugin: {}", provider.plugin_id),
        format!("Profile: {profile_name}"),
        "Configured: false".to_owned(),
        "Available: false".to_owned(),
        format!(
            "Diagnostic [auth_profile_missing]: Authentication has not been configured for provider '{}'.",
            provider.contribution.provider_id
        ),
        format!(
            "  remediation: Run `bcode auth login {}`.",
            provider.contribution.provider_id
        ),
    ]
}

fn print_unconfigured_auth_provider_status(
    provider: &bcode_plugin::RegisteredAuthProvider,
    profile_name: &str,
) -> Result<(), CliError> {
    let mut writer = std::io::stdout().lock();
    for line in unconfigured_auth_provider_status_lines(provider, profile_name) {
        writeln!(writer, "{line}")?;
    }
    writer.flush()?;
    Ok(())
}

fn auth_provider_status(provider_id: &str, explicit_profile: Option<&str>) -> Result<(), CliError> {
    let mut host = load_cli_plugin_host()?;
    let result = inspect_auth_provider_status(&host, provider_id, explicit_profile);
    let cleanup = host.deactivate_all().map_err(CliError::from);
    result.and(cleanup)
}

fn inspect_auth_provider_status(
    host: &bcode_plugin::PluginHost,
    provider_id: &str,
    explicit_profile: Option<&str>,
) -> Result<(), CliError> {
    let provider = registered_auth_provider(host, provider_id)?;
    let config = bcode_config::load_config()?;
    let runtime = bcode_config::load_runtime_auth_subscriptions();
    let resolved = match lookup_registered_auth_profile_from(
        &config,
        &runtime,
        &provider,
        explicit_profile,
    )? {
        bcode_provider_auth::AuthProviderProfileLookup::Configured(resolved) => resolved,
        bcode_provider_auth::AuthProviderProfileLookup::Unconfigured { profile_name } => {
            return print_unconfigured_auth_provider_status(&provider, &profile_name);
        }
    };
    let method = resolved_auth_method(&provider, &resolved)?;
    let mut writer = std::io::stdout().lock();
    writeln!(writer, "Provider: {}", provider.contribution.display_name)?;
    writeln!(writer, "Plugin: {}", provider.plugin_id)?;
    writeln!(writer, "Profile: {}", resolved.profile_name)?;
    writeln!(writer, "Configured: true")?;
    let status = bcode_provider_auth::lifecycle::AuthVaultLifecycle::new(
        &resolved,
        provider_id,
        &provider.plugin_id,
        method,
    )
    .map_err(|error| CliError::LoginProfile(error.to_string()))?
    .inspect()
    .map_err(|error| CliError::LoginProfile(error.to_string()))?;
    writeln!(
        writer,
        "Available: {}",
        status.profile_exists && !status.present_credentials.is_empty()
    )?;
    for diagnostic in status.diagnostics {
        writeln!(
            writer,
            "Diagnostic [{}]: {}",
            diagnostic.code, diagnostic.message
        )?;
        if let Some(remediation) = diagnostic.remediation {
            writeln!(writer, "  remediation: {remediation}")?;
        }
    }
    writer.flush()?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn run_auth_interactive_flow(
    host: &bcode_plugin::PluginHost,
    provider: &bcode_plugin::RegisteredAuthProvider,
    method: &bcode_provider_auth_models::AuthMethodContribution,
    resolved: &bcode_provider_auth::ResolvedAuthProfile,
    verify: bool,
    revoke: bool,
) -> Result<(), CliError> {
    let bcode_provider_auth_models::AuthMethodContribution::Interactive { operation, .. } = method
    else {
        if verify || revoke {
            return Err(CliError::LoginProfile(
                "This provider method does not define an interactive verification/revocation flow."
                    .to_owned(),
            ));
        }
        return Ok(());
    };
    let mut request = bcode_provider_auth_models::AuthFlowRequest {
        schema_version: bcode_provider_auth_models::AUTH_FLOW_SCHEMA_VERSION,
        provider_id: provider.contribution.provider_id.clone(),
        method_id: method.method_id().to_owned(),
        profile: resolved.profile_name.clone(),
        operation: bcode_provider_auth_models::AuthFlowOperation::Begin,
        state: None,
        input: None,
        verify,
        revoke,
    };
    loop {
        request
            .validate()
            .map_err(|error| CliError::LoginProfile(error.to_string()))?;
        let response = host
            .invoke_service_json::<_, bcode_provider_auth_models::AuthFlowResponse>(
                &provider.plugin_id,
                bcode_provider_auth_models::AUTH_INTERFACE_ID,
                operation,
                &request,
            )
            .map_err(plugin_service_call_error)?;
        // Validate and report diagnostics before returning terminal failures;
        // failed/cancelled flows must not execute further interactive effects.
        let terminal = prepare_auth_flow_response(&mut std::io::stderr().lock(), &response)?;
        let mut input = None;
        for effect in &response.effects {
            write_auth_effect(&mut std::io::stdout().lock(), effect)?;
            match effect {
                bcode_provider_auth_models::AuthFlowEffect::OpenBrowser { url } => {
                    open_browser(url);
                }
                bcode_provider_auth_models::AuthFlowEffect::DisplayDeviceCode {
                    verification_url,
                    ..
                } => {
                    open_browser(verification_url);
                }
                bcode_provider_auth_models::AuthFlowEffect::Prompt { prompt_id, .. } => {
                    let value = read_stdin_line()?;
                    effect
                        .validate_answer(&value)
                        .map_err(|error| CliError::InvalidArguments(error.to_string()))?;
                    input = Some(bcode_provider_auth_models::AuthFlowInput {
                        prompt_id: prompt_id.clone(),
                        value,
                    });
                }
                bcode_provider_auth_models::AuthFlowEffect::Wait { millis } => {
                    tokio::time::sleep(Duration::from_millis(*millis)).await;
                }
                bcode_provider_auth_models::AuthFlowEffect::Message { .. } => {}
            }
        }
        if terminal {
            if !response.credentials.is_empty() {
                bcode_provider_auth::lifecycle::AuthVaultLifecycle::new(
                    resolved,
                    &provider.contribution.provider_id,
                    &provider.plugin_id,
                    method,
                )
                .map_err(|error| CliError::LoginProfile(error.to_string()))?
                .replace_owned(response.credentials)
                .map_err(|error| CliError::LoginProfile(error.to_string()))?;
            }
            return Ok(());
        }
        request.operation = bcode_provider_auth_models::AuthFlowOperation::Continue;
        request.state = response.state;
        request.input = input;
    }
}

fn write_auth_effect(
    writer: &mut impl std::io::Write,
    effect: &bcode_provider_auth_models::AuthFlowEffect,
) -> Result<(), CliError> {
    use bcode_provider_auth_models::AuthFlowEffect;
    match effect {
        AuthFlowEffect::OpenBrowser { url } => writeln!(writer, "Open in browser: {url}")?,
        AuthFlowEffect::DisplayDeviceCode {
            verification_url,
            user_code,
            ..
        } => {
            writeln!(writer, "Open {verification_url} and enter code {user_code}")?;
        }
        AuthFlowEffect::Prompt {
            message, choices, ..
        } => {
            writeln!(writer, "{message}")?;
            if !choices.is_empty() {
                writeln!(writer, "Choices: {}", choices.join(", "))?;
            }
        }
        AuthFlowEffect::Message { message } => writeln!(writer, "{message}")?,
        AuthFlowEffect::Wait { .. } => return Ok(()),
    }
    writer.flush()?;
    Ok(())
}

fn prepare_auth_flow_response(
    writer: &mut impl std::io::Write,
    response: &bcode_provider_auth_models::AuthFlowResponse,
) -> Result<bool, CliError> {
    response
        .validate()
        .map_err(|error| CliError::LoginProfile(error.to_string()))?;
    for diagnostic in &response.diagnostics {
        write_auth_diagnostic(writer, &diagnostic.code, &diagnostic.message)?;
        if let Some(remediation) = &diagnostic.remediation {
            writeln!(writer, "  remediation: {remediation}")?;
            writer.flush()?;
        }
    }
    Ok(auth_flow_terminal_result(response.status)?.is_some())
}

fn write_auth_diagnostic(
    writer: &mut impl std::io::Write,
    code: &str,
    message: &str,
) -> Result<(), CliError> {
    writeln!(writer, "Diagnostic [{code}]: {message}")?;
    writer.flush()?;
    Ok(())
}

fn auth_flow_terminal_result(
    status: bcode_provider_auth_models::AuthFlowStatus,
) -> Result<Option<()>, CliError> {
    match status {
        bcode_provider_auth_models::AuthFlowStatus::Pending => Ok(None),
        bcode_provider_auth_models::AuthFlowStatus::Succeeded => Ok(Some(())),
        bcode_provider_auth_models::AuthFlowStatus::Failed => Err(CliError::LoginProfile(
            "Provider authentication flow failed.".to_owned(),
        )),
        bcode_provider_auth_models::AuthFlowStatus::Cancelled => Err(CliError::AuthFlowCancelled),
    }
}

const MAX_CLI_AUTH_INPUT_BYTES: usize = bcode_provider_auth_models::MAX_AUTH_TEXT_BYTES;

fn read_stdin_line() -> Result<String, CliError> {
    read_auth_input_line(std::io::stdin().lock())
}

fn read_auth_input_line(reader: impl std::io::BufRead) -> Result<String, CliError> {
    // Allow a full-size answer plus CRLF and one byte to detect overflow.
    let mut limited = std::io::Read::take(reader, (MAX_CLI_AUTH_INPUT_BYTES + 3) as u64);
    let mut bytes = Vec::new();
    std::io::BufRead::read_until(&mut limited, b'\n', &mut bytes)?;
    if bytes.is_empty() {
        return Err(CliError::InvalidArguments(
            "authentication input ended before a response".to_owned(),
        ));
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    if bytes.len() > MAX_CLI_AUTH_INPUT_BYTES {
        return Err(CliError::InvalidArguments(format!(
            "authentication response exceeds {MAX_CLI_AUTH_INPUT_BYTES} bytes"
        )));
    }
    String::from_utf8(bytes).map_err(|_| {
        CliError::InvalidArguments("authentication response must be valid UTF-8".to_owned())
    })
}

fn auth_pool_reset_cooldown(pool_name: &str, profile: Option<&str>) {
    let removed = bcode_provider_auth::auth_pool_state::reset_cooldowns(pool_name, profile);
    println!(
        "Reset {removed} cooldown entr{} for auth pool '{pool_name}'.",
        if removed == 1 { "y" } else { "ies" }
    );
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn format_duration(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

fn format_unix_timestamp(timestamp: u64) -> String {
    timestamp.to_string()
}

#[allow(clippy::too_many_lines)]
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
    println!("Auth vault security:");
    let options = bcode_provider_auth::security::device_seal_options_for_auth_profile(auth_profile);
    let policy = options.policy;
    let vault_path = auth_profile
        .settings
        .get("vault")
        .map_or_else(bcode_config::default_auth_vault_path, PathBuf::from);
    let storage_profile = auth_profile
        .settings
        .get("profile")
        .map_or(auth_profile_name.as_str(), String::as_str);
    let security_status = bcode_provider_auth::security::inspect_auth_vault_security(
        &vault_path,
        storage_profile,
        policy,
    );
    println!(
        "  Vault: {}",
        display_from_current_dir(&security_status.vault_path)
    );
    println!("  Vault exists: {}", security_status.vault_exists);
    match security_status.vault_version {
        Some(version) => println!("  Vault format: v{version}"),
        None => println!("  Vault format: unknown"),
    }
    println!(
        "  Profile: {} ({})",
        security_status.profile,
        if security_status.profile_exists {
            "present"
        } else {
            "missing"
        }
    );
    println!(
        "  Profile keys: {}",
        if security_status.profile_keys_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("  Configured device_seal: {policy:?}");
    println!(
        "  Configured device_seal mode: {}",
        format_auth_device_seal_selection(options.seal.selection)
    );
    println!("  Configured device_seal strict: {}", options.seal.strict);
    println!(
        "  Profile device seal: {}",
        if security_status.profile_device_sealed {
            "enabled"
        } else {
            "missing"
        }
    );
    if let Some(backend) = &security_status.device_seal_backend {
        println!("  Profile device seal backend: {backend}");
    }
    if let Some(mode) = &security_status.device_seal_mode {
        println!("  Profile device seal mode: {mode}");
    }
    if let Some(strict) = security_status.device_seal_strict {
        println!("  Profile device seal strict: {strict}");
    }
    println!(
        "  Policy status: {}",
        if security_status.policy_satisfied {
            "satisfied"
        } else {
            "not satisfied"
        }
    );
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
    if !security_status.diagnostics.is_empty() || !resolved.auth.diagnostics.is_empty() {
        println!("Auth security diagnostics:");
        for diagnostic in &security_status.diagnostics {
            println!(
                "  {} [{}]: {}",
                diagnostic.severity.as_str(),
                diagnostic.code,
                diagnostic.message
            );
            if let Some(remediation) = &diagnostic.remediation {
                println!("    remediation: {remediation}");
            }
        }
        for diagnostic in &resolved.auth.diagnostics {
            println!(
                "  {} [{}]: {}",
                diagnostic.severity, diagnostic.code, diagnostic.message
            );
            if let Some(remediation) = &diagnostic.remediation {
                println!("    remediation: {remediation}");
            }
        }
    }
    Ok(())
}

const fn format_auth_device_seal_selection(
    selection: sshenv_vault::device::DeviceSealSelection,
) -> &'static str {
    match selection {
        sshenv_vault::device::DeviceSealSelection::Policy(policy) => match policy {
            sshenv_vault::device::DeviceSealPolicy::Default => "default",
            sshenv_vault::device::DeviceSealPolicy::TransparentDeviceOnly => {
                "transparent-device-only"
            }
        },
        sshenv_vault::device::DeviceSealSelection::Backend(backend) => match backend {
            sshenv_vault::device::DeviceSealBackendSelection::MacosKeychain => "macos-keychain",
            sshenv_vault::device::DeviceSealBackendSelection::MacosKeychainDeviceOnly => {
                "macos-keychain-device-only"
            }
            sshenv_vault::device::DeviceSealBackendSelection::MacosKeychainDeviceOnlyAnyApplication => {
                "macos-keychain-device-only-any-application"
            }
            sshenv_vault::device::DeviceSealBackendSelection::WindowsDpapiCurrentUser => {
                "windows-dpapi-current-user"
            }
            sshenv_vault::device::DeviceSealBackendSelection::LinuxTpm => "linux-tpm",
            sshenv_vault::device::DeviceSealBackendSelection::LinuxSecretService => {
                "linux-secret-service"
            }
            sshenv_vault::device::DeviceSealBackendSelection::SecureEnclave => "secure-enclave",
            sshenv_vault::device::DeviceSealBackendSelection::LocalFile => "local-file",
        },
    }
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
    let recipient_key_hint = recipient_key.or_else(|| {
        auth_profile
            .settings
            .get("recipient_key")
            .map(String::to_string)
    });
    let store = open_auth_store(&vault_path)?;
    let device_seal_policy =
        bcode_provider_auth::security::device_seal_policy_for_auth_profile(auth_profile);
    let api_key = rpassword::prompt_password(format!("{api_key_env}: "))?;
    let target = LoginTarget {
        storage_profile: storage_profile.clone(),
    };
    upsert_auth_profile_secrets(
        &store,
        &target,
        BTreeMap::from([(api_key_env.clone(), api_key)]),
        &[],
    )?;
    apply_auth_device_seal_policy(
        &vault_path,
        &storage_profile,
        device_seal_policy,
        recipient_key_hint.as_deref(),
    )?;
    println!("API key saved");
    println!("Auth profile: {auth_profile_name}");
    println!("Credentials saved to sshenv vault profile: {storage_profile}");
    println!("API key environment variable: {api_key_env}");
    println!("Config is declarative; no config file update needed.");
    Ok(())
}

async fn handle_login_command(command: LoginCommand) -> Result<(), CliError> {
    eprintln!("warning: `bcode login` is deprecated; use `bcode auth login <provider>` instead");
    run_compatible_login(compatible_login_plan(command)?).await
}

fn compatible_login_plan(command: LoginCommand) -> Result<CompatibleLoginPlan, CliError> {
    match command {
        LoginCommand::Openai {
            api_key,
            base_url,
            chatgpt,
            browser,
            headless,
            add_subscription,
            profile,
            vault,
            recipient_key,
            no_device_seal,
            model,
        } => plan_openai_login(OpenAiLoginOptions {
            api_key,
            base_url,
            mode: OpenAiLoginMode {
                auth: if add_subscription {
                    OpenAiLoginKind::AddSubscription
                } else if chatgpt {
                    OpenAiLoginKind::ChatGpt
                } else {
                    OpenAiLoginKind::Auto
                },
                flow: if headless && !browser {
                    OpenAiLoginFlow::DeviceCode
                } else {
                    OpenAiLoginFlow::Browser
                },
            },
            profile,
            vault,
            recipient_key,
            no_device_seal,
            model,
        }),
        LoginCommand::Xai {
            api_key,
            base_url,
            profile,
            vault,
            recipient_key,
            no_device_seal,
            model,
        } => Ok(plan_xai_login(XaiLoginOptions {
            api_key,
            base_url,
            profile,
            vault,
            recipient_key,
            no_device_seal,
            model,
        })),
    }
}

struct OpenAiLoginOptions {
    api_key: Option<String>,
    base_url: Option<String>,
    mode: OpenAiLoginMode,
    profile: Option<String>,
    vault: Option<PathBuf>,
    recipient_key: Option<String>,
    no_device_seal: bool,
    model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OpenAiLoginMode {
    auth: OpenAiLoginKind,
    flow: OpenAiLoginFlow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenAiLoginKind {
    Auto,
    ChatGpt,
    AddSubscription,
}

impl OpenAiLoginKind {
    const fn is_add_subscription(self) -> bool {
        matches!(self, Self::AddSubscription)
    }

    const fn is_chatgpt(self) -> bool {
        matches!(self, Self::ChatGpt | Self::AddSubscription)
    }
}

struct XaiLoginOptions {
    api_key: Option<String>,
    base_url: Option<String>,
    profile: Option<String>,
    vault: Option<PathBuf>,
    recipient_key: Option<String>,
    no_device_seal: bool,
    model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompatibleLoginPlan {
    provider: LoginProvider,
    method_id: &'static str,
    explicit_profile: Option<String>,
    pool: Option<&'static str>,
    supplied: BTreeMap<String, String>,
    replace_owned: bool,
    no_device_seal: bool,
    vault: Option<PathBuf>,
    recipient_key: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    mode: AuthMode,
    add_subscription: bool,
}

fn plan_openai_login(options: OpenAiLoginOptions) -> Result<CompatibleLoginPlan, CliError> {
    if options.mode.auth.is_add_subscription()
        && (options.api_key.is_some() || options.base_url.is_some())
    {
        return Err(CliError::LoginProfile(
            "`bcode login openai --add-subscription` adds ChatGPT subscription OAuth accounts; API-key pooled auth is not supported yet. Remove --api-key/--base-url or omit --add-subscription.".to_string(),
        ));
    }
    let method_id = if options.api_key.is_some()
        || (options.base_url.is_some() && !options.mode.auth.is_chatgpt())
    {
        "api_key"
    } else {
        match options.mode.flow {
            OpenAiLoginFlow::Browser => "chatgpt",
            OpenAiLoginFlow::DeviceCode => "device",
        }
    };
    let mut supplied = BTreeMap::new();
    if let Some(api_key) = options.api_key {
        supplied.insert("api_key".to_owned(), api_key);
    }
    Ok(CompatibleLoginPlan {
        provider: LoginProvider::OpenAi,
        method_id,
        explicit_profile: options.profile,
        pool: options.mode.auth.is_add_subscription().then_some("openai"),
        supplied,
        replace_owned: method_id == "api_key",
        no_device_seal: options.no_device_seal,
        vault: options.vault,
        recipient_key: options.recipient_key,
        model: options.model,
        base_url: options.base_url,
        mode: if method_id == "api_key" {
            AuthMode::ApiKey
        } else {
            AuthMode::ChatGpt
        },
        add_subscription: options.mode.auth.is_add_subscription(),
    })
}

fn plan_xai_login(options: XaiLoginOptions) -> CompatibleLoginPlan {
    let mut supplied = BTreeMap::new();
    if let Some(api_key) = options.api_key {
        supplied.insert("api_key".to_owned(), api_key);
    }
    CompatibleLoginPlan {
        provider: LoginProvider::Xai,
        method_id: "api_key",
        explicit_profile: options.profile,
        pool: None,
        supplied,
        replace_owned: true,
        no_device_seal: options.no_device_seal,
        vault: options.vault,
        recipient_key: options.recipient_key,
        model: options.model,
        base_url: Some(
            options
                .base_url
                .unwrap_or_else(|| "https://api.x.ai/v1".to_owned()),
        ),
        mode: AuthMode::ApiKey,
        add_subscription: false,
    }
}

fn enroll_registered_secret_values(
    provider_id: &str,
    provider: &bcode_plugin::RegisteredAuthProvider,
    method: &bcode_provider_auth_models::AuthMethodContribution,
    resolved: &bcode_provider_auth::ResolvedAuthProfile,
    mut supplied: BTreeMap<String, String>,
    replace_owned: bool,
) -> Result<(), CliError> {
    let bcode_provider_auth_models::AuthMethodContribution::SecretFields { fields, .. } = method
    else {
        return Err(CliError::LoginProfile(format!(
            "Provider '{provider_id}' selected method '{}' is not a generic secret-field method.",
            method.method_id()
        )));
    };
    let mut credentials = BTreeMap::new();
    for field in fields {
        let value = supplied.remove(&field.credential_id).map_or_else(
            || rpassword::prompt_password(format!("{}: ", field.prompt)),
            Ok,
        )?;
        if value.is_empty() && field.optional {
            continue;
        }
        field
            .validation
            .validate_secret(&value)
            .map_err(|error| CliError::LoginProfile(error.to_string()))?;
        credentials.insert(field.credential_id.clone(), value);
    }
    if let Some(credential_id) = supplied.keys().next() {
        return Err(CliError::LoginProfile(format!(
            "Provider '{provider_id}' does not declare credential '{credential_id}'."
        )));
    }
    let lifecycle = bcode_provider_auth::lifecycle::AuthVaultLifecycle::new(
        resolved,
        provider_id,
        &provider.plugin_id,
        method,
    )
    .map_err(|error| CliError::LoginProfile(error.to_string()))?;
    if replace_owned {
        lifecycle.replace_owned(credentials)
    } else {
        lifecycle.upsert(credentials)
    }
    .map(|_| ())
    .map_err(|error| CliError::LoginProfile(error.to_string()))
}

async fn run_compatible_login(plan: CompatibleLoginPlan) -> Result<(), CliError> {
    let compatible_profile = if plan.add_subscription {
        Some(compatible_login_profile(
            plan.provider.subcommand(),
            plan.explicit_profile.as_deref(),
            true,
        )?)
    } else {
        plan.explicit_profile.clone()
    };
    let result = enroll_registered_auth_provider(
        plan.provider.subcommand(),
        AuthProviderLoginOptions {
            explicit_profile: compatible_profile.as_deref(),
            explicit_vault: plan.vault.clone(),
            recipient_key: plan.recipient_key.as_deref(),
            no_device_seal: plan.no_device_seal,
            pool: plan.pool,
            requested_method: Some(plan.method_id),
            verify: false,
        },
        plan.supplied,
        plan.replace_owned,
    )
    .await?;
    apply_legacy_login_configuration(LegacyLoginConfiguration {
        provider: plan.provider,
        resolved: &result.resolved,
        persisted_runtime: result.persisted_runtime,
        model: plan.model,
        base_url: plan.base_url.as_deref(),
        mode: plan.mode,
        method_id: plan.method_id,
        add_subscription: plan.add_subscription,
    });
    Ok(())
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LoginTarget {
    storage_profile: String,
}

fn active_login_auth_profile(config: &bcode_config::BcodeConfig) -> Option<String> {
    std::env::var(bcode_config::BCODE_AUTH_PROFILE_ENV)
        .ok()
        .filter(|profile| !profile.trim().is_empty())
        .or_else(|| config.resolved_model_selection().auth_profile)
}

fn apply_auth_device_seal_policy(
    vault_path: &Path,
    profile: &str,
    policy: bcode_provider_auth::security::AuthDeviceSealPolicy,
    recipient_key: Option<&str>,
) -> Result<(), CliError> {
    let options = bcode_provider_auth::security::AuthDeviceSealOptions::from_policy(policy);
    match bcode_provider_auth::security::reconcile_auth_vault_security_report_with_options(
        vault_path,
        profile,
        options,
        recipient_key,
    )
    .diagnostics
    .as_slice()
    {
        [] => Ok(()),
        diagnostics => {
            for diagnostic in diagnostics {
                println!(
                    "Auth vault security {} [{}]: {}",
                    diagnostic.severity.as_str(),
                    diagnostic.code,
                    diagnostic.message
                );
                if let Some(remediation) = &diagnostic.remediation {
                    println!("  remediation: {remediation}");
                }
            }
            if diagnostics.iter().any(|diagnostic| {
                diagnostic.severity
                    == bcode_provider_auth::security::AuthSecurityDiagnosticSeverity::Error
            }) {
                Err(CliError::BundledPluginInstallFailed(
                    "auth vault security requirement is not satisfied".to_string(),
                ))
            } else {
                Ok(())
            }
        }
    }
}

fn open_auth_store(vault_path: &Path) -> Result<sshenv_vault::SshenvStore, CliError> {
    let managed_recipient_key =
        bcode_provider_auth::security::ensure_vault_recipient_key(vault_path).map_err(|error| {
            CliError::BundledPluginInstallFailed(format!(
                "failed to prepare Bcode-managed auth vault key: {error}"
            ))
        })?;
    let private_key_paths = bcode_provider_auth::security::vault_private_key_paths(vault_path);
    let store = sshenv_vault::SshenvStore::new(
        sshenv_vault::SshenvStoreConfig::new(vault_path.to_path_buf())
            .with_private_key_paths(private_key_paths.clone()),
    );
    if !vault_path.exists() {
        initialize_auth_vault(vault_path, &store, &managed_recipient_key)?;
    } else if let Err(error) = sshenv_vault::load_and_unlock_metadata_with_private_key_paths(
        vault_path,
        &private_key_paths,
    ) {
        let archive_path = archive_incompatible_auth_vault(vault_path, &error)?;
        println!(
            "Archived incompatible auth vault to {}; initialized a fresh Bcode-managed auth vault.",
            display_from_current_dir(&archive_path)
        );
        initialize_auth_vault(vault_path, &store, &managed_recipient_key)?;
    }
    Ok(store)
}

fn archive_incompatible_auth_vault(
    vault_path: &Path,
    unlock_error: &dyn std::fmt::Display,
) -> Result<PathBuf, CliError> {
    let file_name = vault_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("vault");
    let parent = vault_path.parent().unwrap_or_else(|| Path::new("."));
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    for attempt in 0_u16..1000 {
        let archive_name = if attempt == 0 {
            format!("{file_name}.legacy-{timestamp}")
        } else {
            format!("{file_name}.legacy-{timestamp}-{attempt}")
        };
        let archive_path = parent.join(archive_name);
        if archive_path.exists() {
            continue;
        }
        fs::rename(vault_path, &archive_path).map_err(|error| {
            CliError::BundledPluginInstallFailed(format!(
                "failed to archive incompatible auth vault {} after Bcode-managed unlock failed ({unlock_error}): {error}",
                display_from_current_dir(vault_path)
            ))
        })?;
        return Ok(archive_path);
    }
    Err(CliError::BundledPluginInstallFailed(format!(
        "failed to choose archive path for incompatible auth vault {} after Bcode-managed unlock failed ({unlock_error})",
        display_from_current_dir(vault_path)
    )))
}

fn initialize_auth_vault(
    vault_path: &Path,
    store: &sshenv_vault::SshenvStore,
    recipient_key: &str,
) -> Result<(), CliError> {
    store.init(recipient_key).map_err(|error| {
        CliError::BundledPluginInstallFailed(format!("failed to initialize auth vault: {error}"))
    })?;
    let (mut vault, data_key) = store.load_and_unlock().map_err(|error| {
        CliError::BundledPluginInstallFailed(format!(
            "failed to unlock initialized auth vault: {error}"
        ))
    })?;
    vault
        .migrate_to_v2(&[recipient_key.to_string()])
        .map_err(|error| {
            CliError::BundledPluginInstallFailed(format!(
                "failed to migrate auth vault to v2: {error}"
            ))
        })?;
    vault.enable_profile_keys().map_err(|error| {
        CliError::BundledPluginInstallFailed(format!("failed to enable auth profile keys: {error}"))
    })?;
    vault.save(vault_path, &data_key).map_err(|error| {
        CliError::BundledPluginInstallFailed(format!(
            "failed to save initialized auth vault: {error}"
        ))
    })
}

fn upsert_auth_profile_secrets(
    store: &sshenv_vault::SshenvStore,
    target: &LoginTarget,
    values: BTreeMap<String, String>,
    remove_keys: &[String],
) -> Result<(), CliError> {
    let mut profile_values = match store.get_profile(&target.storage_profile) {
        Ok(Some(values)) => values,
        Ok(None) => BTreeMap::new(),
        Err(error) => {
            println!(
                "Auth vault profile {} could not be unlocked with the Bcode-managed vault key ({error}); resetting it with fresh login credentials.",
                target.storage_profile
            );
            BTreeMap::new()
        }
    };

    for key in remove_keys {
        profile_values.remove(key);
    }
    for (key, value) in values {
        profile_values.insert(key, Zeroizing::new(value));
    }

    store
        .replace_profile(&target.storage_profile, profile_values)
        .map_err(|error| {
            CliError::BundledPluginInstallFailed(format!("failed to save auth profile: {error}"))
        })
}

struct LegacyLoginConfiguration<'a> {
    provider: LoginProvider,
    resolved: &'a bcode_provider_auth::ResolvedAuthProfile,
    persisted_runtime: bool,
    model: Option<String>,
    base_url: Option<&'a str>,
    mode: AuthMode,
    method_id: &'a str,
    add_subscription: bool,
}

fn apply_legacy_login_configuration(options: LegacyLoginConfiguration<'_>) {
    let LegacyLoginConfiguration {
        provider,
        resolved,
        persisted_runtime,
        model,
        base_url,
        mode,
        method_id,
        add_subscription,
    } = options;
    println!("{} authentication saved", provider.label());
    println!("Auth profile: {}", resolved.profile_name);
    let storage_profile = resolved
        .profile
        .settings
        .get("profile")
        .map_or(resolved.profile_name.as_str(), String::as_str);
    println!("Credentials saved to sshenv vault profile: {storage_profile}");
    if resolved.source == bcode_provider_auth::AuthProfileSource::Declarative {
        println!("Config is declarative; no config file update needed.");
        return;
    }
    if persisted_runtime {
        println!("Runtime auth metadata updated.");
    }
    let vault = resolved
        .profile
        .settings
        .get("vault")
        .map_or_else(bcode_config::default_auth_vault_path, PathBuf::from);
    let update = if add_subscription {
        Ok(bcode_config::runtime_auth_subscriptions_path())
    } else {
        bcode_config::set_openai_compatible_sshenv_auth_method(
            bcode_config::OpenAiCompatibleAuthConfigUpdate {
                provider: provider.subcommand(),
                profile: resolved.profile_name.clone(),
                vault,
                model_id: model,
                mode,
                method: method_id,
                base_url,
            },
        )
    };
    match update {
        Ok(config_path) => println!("Config updated: {}", display_from_current_dir(&config_path)),
        Err(error) => {
            println!("Config update failed: {error}");
            println!(
                "Credentials were saved. To use them, run a provider wrapper with a declarative {} auth profile or update a writable config.",
                provider.prefix()
            );
        }
    }
}

fn random_urlsafe(bytes: usize) -> Result<String, CliError> {
    let mut data = vec![0_u8; bytes];
    rand::rngs::OsRng
        .try_fill_bytes(&mut data)
        .map_err(|error| CliError::BundledPluginInstallFailed(error.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(data))
}

fn plugin_selection_for_config(
    config: &bcode_config::BcodeConfig,
) -> bcode_plugin::PluginSelection {
    let default_plugin_ids = STATIC_BUNDLED_DEFAULT_PLUGIN_IDS
        .get()
        .map_or_else(Vec::new, Clone::clone);
    bcode_config::plugin_selection_with_default_plugin_ids(config, &default_plugin_ids)
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

fn list_plugins(roots: &[std::path::PathBuf], json: bool) -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    let selection = plugin_selection_for_config(&config);
    let plugins =
        bcode_plugin::filter_selected_plugins(discover_plugins_for_cli(roots)?, &selection);

    if json {
        let output = plugins
            .iter()
            .map(|plugin| {
                serde_json::json!({
                    "plugin_id": plugin.manifest.id,
                    "version": plugin.manifest.version.to_string(),
                    "name": plugin.manifest.name,
                    "manifest_path": display_from_current_dir(&plugin.manifest_path).to_string(),
                })
            })
            .collect::<Vec<_>>();
        return print_json(&output);
    }
    let mut output = std::io::stdout().lock();
    if plugins.is_empty() {
        writeln!(output, "no plugins discovered")?;
    }

    for plugin in plugins {
        writeln!(
            output,
            "{}\t{}\t{}\t{}",
            plugin.manifest.id,
            plugin.manifest.version,
            plugin.manifest.name,
            display_from_current_dir(&plugin.manifest_path)
        )?;
    }
    output.flush()?;
    Ok(())
}

async fn list_plugin_services(
    roots: &[std::path::PathBuf],
    daemon: bool,
    json: bool,
) -> Result<(), CliError> {
    if daemon {
        let services = BcodeClient::default_endpoint().plugin_services().await?;
        if json {
            return print_json(&services);
        }
        return write_plugin_services(
            &mut std::io::stdout().lock(),
            services.iter().map(|service| {
                (
                    service.interface_id.as_str(),
                    service.plugin_id.as_str(),
                    service.name.as_deref(),
                )
            }),
        );
    }

    let config = bcode_config::load_config()?;
    let selection = plugin_selection_for_config(&config);
    let plugins =
        bcode_plugin::filter_selected_plugins(discover_plugins_for_cli(roots)?, &selection);
    if json {
        let services = plugins
            .iter()
            .flat_map(|plugin| {
                plugin.manifest.services.iter().map(|service| {
                    serde_json::json!({
                        "plugin_id": plugin.manifest.id,
                        "interface_id": service.interface_id,
                        "name": service.name,
                        "description": service.description,
                    })
                })
            })
            .collect::<Vec<_>>();
        return print_json(&services);
    }
    write_plugin_services(
        &mut std::io::stdout().lock(),
        plugins.iter().flat_map(|plugin| {
            plugin.manifest.services.iter().map(|service| {
                (
                    service.interface_id.as_str(),
                    plugin.manifest.id.as_str(),
                    service.name.as_deref(),
                )
            })
        }),
    )
}

fn write_plugin_services<'a, W: std::io::Write>(
    output: &mut W,
    services: impl IntoIterator<Item = (&'a str, &'a str, Option<&'a str>)>,
) -> Result<(), CliError> {
    let mut has_services = false;
    for (interface_id, plugin_id, name) in services {
        has_services = true;
        writeln!(
            output,
            "{interface_id}\t{plugin_id}\t{}",
            name.unwrap_or("<unnamed>")
        )?;
    }
    if !has_services {
        writeln!(output, "no plugin services discovered")?;
    }
    output.flush()?;
    Ok(())
}

fn check_plugins(roots: &[std::path::PathBuf], json: bool) -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    let selection = plugin_selection_for_config(&config);
    let plugins =
        bcode_plugin::filter_selected_plugins(discover_plugins_for_cli(roots)?, &selection);
    if json {
        let mut checked = Vec::with_capacity(plugins.len());
        for plugin in plugins {
            let loaded = bcode_plugin::load_registered_plugin(&plugin)?;
            loaded.activate()?;
            loaded.deactivate()?;
            checked.push(serde_json::json!({
                "plugin_id": loaded.manifest().id,
                "status": "ok",
            }));
        }
        return print_json(&checked);
    }
    if plugins.is_empty() {
        let mut output = std::io::stdout().lock();
        writeln!(output, "no plugins discovered")?;
        output.flush()?;
        return Ok(());
    }

    for plugin in plugins {
        let loaded = bcode_plugin::load_registered_plugin(&plugin)?;
        loaded.activate()?;
        loaded.deactivate()?;
        // Do not hold the stdout lock while executing plugin lifecycle callbacks.
        let mut output = std::io::stdout().lock();
        writeln!(output, "{}\tOK", loaded.manifest().id)?;
        output.flush()?;
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
    json: bool,
) -> Result<(), CliError> {
    validate_plugin_service_route(Some(plugin_id), interface_id, operation)?;
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
        return print_service_response(response, json);
    }

    let config = bcode_config::load_config()?;
    let selection = plugin_selection_for_config(&config);
    let plugins =
        bcode_plugin::filter_selected_plugins(discover_plugins_for_cli(roots)?, &selection);
    let mut host = bcode_plugin::PluginHost::load_registered_plugins(&plugins)?;
    let response = host.invoke_service(plugin_id, interface_id, operation, payload);
    let deactivation = host.deactivate_all();
    let response = response?;
    deactivation?;
    print_service_response(response, json)
}

async fn call_plugin_service(
    roots: &[std::path::PathBuf],
    interface_id: &str,
    operation: &str,
    payload: Option<String>,
    daemon: bool,
    json: bool,
) -> Result<(), CliError> {
    validate_plugin_service_route(None, interface_id, operation)?;
    let payload = payload.unwrap_or_default().into_bytes();
    if daemon {
        let response = BcodeClient::default_endpoint()
            .call_plugin_service(interface_id.to_string(), operation.to_string(), payload)
            .await?;
        return print_service_response(response, json);
    }

    let config = bcode_config::load_config()?;
    let selection = plugin_selection_for_config(&config);
    let plugins =
        bcode_plugin::filter_selected_plugins(discover_plugins_for_cli(roots)?, &selection);
    let mut host = bcode_plugin::PluginHost::load_registered_plugins(&plugins)?;
    let response = host.invoke_service_by_interface(interface_id, operation, payload);
    let deactivation = host.deactivate_all();
    let response = response?;
    deactivation?;
    print_service_response(response, json)
}

fn validate_plugin_service_route(
    plugin_id: Option<&str>,
    interface_id: &str,
    operation: &str,
) -> Result<(), CliError> {
    for (name, value) in [
        ("plugin id", plugin_id),
        ("interface id", Some(interface_id)),
        ("operation", Some(operation)),
    ] {
        if value.is_some_and(|value| value.trim().is_empty()) {
            return Err(CliError::InvalidArguments(format!(
                "plugin service {name} must not be empty"
            )));
        }
    }
    Ok(())
}

fn print_service_response(
    response: impl Into<PrintableServiceResponse>,
    json: bool,
) -> Result<(), CliError> {
    write_service_response(&mut std::io::stdout().lock(), response.into(), json)
}

fn write_service_response<W: std::io::Write>(
    output: &mut W,
    response: PrintableServiceResponse,
    json: bool,
) -> Result<(), CliError> {
    if json {
        write_json_result(
            output,
            &serde_json::json!({
                "payload": response.payload,
                "error": response.error.as_ref().map(|error| serde_json::json!({
                    "code": error.code,
                    "message": error.message,
                })),
            }),
        )?;
    } else {
        if let Some(error) = &response.error {
            writeln!(output, "ERROR\t{}\t{}", error.code, error.message)?;
        } else {
            writeln!(output, "{}", String::from_utf8_lossy(&response.payload))?;
        }
        output.flush()?;
    }
    if let Some(error) = response.error {
        return Err(CliError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    Ok(())
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
    json: bool,
) -> Result<(), CliError> {
    let payload = payload.unwrap_or_default().into_bytes();
    if daemon {
        let delivered = BcodeClient::default_endpoint()
            .publish_plugin_event(topic.to_string(), payload)
            .await?;
        return print_plugin_delivery(delivered, json);
    }

    let config = bcode_config::load_config()?;
    let selection = plugin_selection_for_config(&config);
    let plugins =
        bcode_plugin::filter_selected_plugins(discover_plugins_for_cli(roots)?, &selection);
    let mut host = bcode_plugin::PluginHost::load_registered_plugins(&plugins)?;
    let delivered = host.publish_event(topic, &payload)?;
    host.deactivate_all()?;
    print_plugin_delivery(delivered, json)
}

fn print_plugin_delivery(delivered: usize, json: bool) -> Result<(), CliError> {
    write_plugin_delivery(&mut std::io::stdout().lock(), delivered, json)
}

fn write_plugin_delivery<W: std::io::Write>(
    output: &mut W,
    delivered: usize,
    json: bool,
) -> Result<(), CliError> {
    if json {
        write_json_result(output, &serde_json::json!({ "delivered": delivered }))
    } else {
        writeln!(output, "delivered\t{delivered}")?;
        output.flush()?;
        Ok(())
    }
}

async fn list_models(json: bool, provider: Option<String>) -> Result<(), CliError> {
    let models = BcodeClient::default_endpoint()
        .session_model_list(provider)
        .await?;
    if json {
        print_json(&models)?;
    } else {
        write_model_list(&mut std::io::stdout().lock(), &models.models)?;
    }
    Ok(())
}

async fn model_status(session_id: Option<SessionId>, json: bool) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let status = if let Some(session_id) = session_id {
        client.session_model_status(session_id).await?
    } else {
        client.default_model_status().await?
    };
    if json {
        print_json(&status)?;
    } else {
        write_model_status(&mut std::io::stdout().lock(), &status)?;
    }
    Ok(())
}

fn write_model_status(
    output: &mut impl std::io::Write,
    status: &bcode_model::SessionModelStatus,
) -> Result<(), CliError> {
    writeln!(
        output,
        "provider\t{}",
        status.provider_plugin_id.as_deref().unwrap_or("<auto>")
    )?;
    writeln!(
        output,
        "model\t{}",
        status.model_id.as_deref().unwrap_or("<default>")
    )?;
    writeln!(
        output,
        "context_window\t{}",
        status
            .context_window
            .map_or_else(|| "<none>".to_string(), |value| value.to_string())
    )?;
    writeln!(
        output,
        "max_output_tokens\t{}",
        status
            .max_output_tokens
            .map_or_else(|| "<none>".to_string(), |value| value.to_string())
    )?;
    writeln!(
        output,
        "metadata_source\t{}",
        status
            .metadata_source
            .map_or_else(|| "<none>".to_string(), |source| format!("{source:?}"))
    )?;
    output.flush()?;
    Ok(())
}

fn write_model_list(
    output: &mut impl std::io::Write,
    models: &[bcode_model::ModelInfo],
) -> Result<(), CliError> {
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
    writeln!(
        output,
        "{:<model_width$}  {:<display_name_width$}  {:>10}  {:>10}  {:<16}  DEFAULT",
        "MODEL", "DISPLAY NAME", "CTX", "MAX OUT", "METADATA"
    )?;
    for model in models {
        let context = model
            .context_window
            .map_or_else(|| "-".to_string(), |value| value.to_string());
        let max_output = model
            .max_output_tokens
            .map_or_else(|| "-".to_string(), |value| value.to_string());
        let metadata = model
            .metadata_source
            .map_or_else(|| "-".to_string(), |source| format!("{source:?}"));
        if model.is_default {
            writeln!(
                output,
                "{:<model_width$}  {:<display_name_width$}  {:>10}  {:>10}  {:<16}  yes",
                model.model_id, model.display_name, context, max_output, metadata
            )?;
        } else {
            writeln!(
                output,
                "{:<model_width$}  {:<display_name_width$}  {:>10}  {:>10}  {:<16}",
                model.model_id, model.display_name, context, max_output, metadata
            )?;
        }
    }
    output.flush()?;
    Ok(())
}

async fn model_catalog_diagnostics(json: bool) -> Result<(), CliError> {
    let diagnostics = BcodeClient::default_endpoint()
        .model_catalog_diagnostics()
        .await?;
    if json {
        print_json(&diagnostics)?;
    } else {
        write_model_catalog_diagnostics(&mut std::io::stdout().lock(), &diagnostics)?;
    }
    Ok(())
}

fn write_model_catalog_diagnostics(
    output: &mut impl std::io::Write,
    diagnostics: &bcode_ipc::ModelCatalogDiagnostics,
) -> Result<(), CliError> {
    writeln!(
        output,
        "embedded revision: {}",
        diagnostics.embedded_revision
    )?;
    writeln!(
        output,
        "remote revision: {}",
        diagnostics.remote_revision.as_deref().unwrap_or("-")
    )?;
    writeln!(output, "remote enabled: {}", diagnostics.remote_enabled)?;
    writeln!(output, "cache state: {}", diagnostics.cache_state)?;
    writeln!(
        output,
        "cache age seconds: {}",
        diagnostics
            .cache_age_seconds
            .map_or_else(|| "-".to_owned(), |seconds| seconds.to_string())
    )?;
    writeln!(
        output,
        "refresh in progress: {}",
        diagnostics.refresh_in_progress
    )?;
    if let Some(error) = &diagnostics.last_refresh_error {
        writeln!(output, "last refresh error: {error}")?;
    }
    output.flush()?;
    Ok(())
}

async fn model_capabilities(json: bool) -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    let selection = config.resolved_model_selection();
    let request = bcode_model::ProviderCapabilitiesRequest {
        provider_context: configured_provider_context(&config),
        selected_model_id: selection.selected_model_id,
    };
    let response = call_model_provider_service_payload(
        bcode_model::OP_CAPABILITIES,
        serde_json::to_vec(&request)?,
    )
    .await?;
    write_model_capabilities(&mut std::io::stdout().lock(), response, json)
}

fn write_model_capabilities(
    output: &mut impl std::io::Write,
    response: bcode_ipc::PluginServiceResponse,
    json: bool,
) -> Result<(), CliError> {
    if let Some(error) = response.error {
        return Err(CliError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let capabilities: bcode_model::ProviderCapabilities =
        serde_json::from_slice(&response.payload)?;
    if json {
        return write_json_result(output, &capabilities);
    }
    writeln!(
        output,
        "{}\t{}",
        capabilities.provider_id, capabilities.display_name
    )?;
    for capability in capabilities.capabilities {
        writeln!(output, "capability\t{capability:?}")?;
    }
    for (key, value) in capabilities.metadata {
        writeln!(output, "metadata\t{key}\t{value}")?;
    }
    output.flush()?;
    Ok(())
}

fn verify_models(
    prompt: String,
    max_models: Option<usize>,
    id_pattern: Option<&String>,
    dry_run: bool,
    output: Option<PathBuf>,
    timeout_seconds: u64,
) -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    let context = configured_provider_context(&config);
    let selection = config.resolved_model_selection();
    let provider_plugin_id = selection.provider_plugin_id.clone().ok_or_else(|| {
        CliError::PluginCli("no model provider is configured; pass --provider".to_string())
    })?;
    let mut host = load_cli_plugin_host()?;
    let list_request = bcode_model::ModelListRequest {
        provider_context: context,
        selected_model_id: selection.selected_model_id,
    };
    let models: bcode_model::ModelList = host
        .invoke_service_json(
            &provider_plugin_id,
            bcode_model::MODEL_PROVIDER_INTERFACE_ID,
            bcode_model::OP_MODELS,
            &list_request,
        )
        .map_err(plugin_service_call_error)?;
    let mut candidates = models
        .models
        .into_iter()
        .map(|model| model.model_id)
        .filter(|model_id| id_pattern.is_none_or(|pattern| wildcard_match(pattern, model_id)))
        .collect::<Vec<_>>();
    if let Some(max_models) = max_models {
        candidates.truncate(max_models);
    }
    let mut results = BTreeMap::new();
    let mut invoker = CliPluginTurnInvoker { host: &mut host };
    for model_id in &candidates {
        let result = if dry_run {
            CliVerifyModelResult {
                status: "dry_run".to_string(),
                latency_ms: None,
                error_code: None,
                message: None,
            }
        } else {
            verify_one_model(
                &mut invoker,
                &provider_plugin_id,
                &list_request.provider_context,
                model_id,
                &prompt,
                timeout_seconds,
            )
        };
        println!(
            "{model_id}\t{}\t{}",
            result.status,
            result
                .latency_ms
                .map_or_else(|| "-".to_string(), |latency| format!("{latency}ms"))
        );
        results.insert(model_id.clone(), result);
    }
    let report = CliVerifyReport {
        provider: "configured".to_string(),
        verified_at: unix_timestamp_string(),
        prompt,
        dry_run,
        total_models: candidates.len(),
        results,
    };
    // Provider work is complete; output failures must not skip plugin deactivation.
    host.deactivate_all()?;
    let body = serde_json::to_string_pretty(&report)?;
    if let Some(output) = output {
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&output, body)?;
        println!("wrote {}", display_from_current_dir(&output));
    } else {
        print_json(&report)?;
    }
    Ok(())
}

fn verify_one_model(
    invoker: &mut CliPluginTurnInvoker<'_>,
    provider_plugin_id: &str,
    context: &bcode_model::ProviderRequestContext,
    model_id: &str,
    prompt: &str,
    timeout_seconds: u64,
) -> CliVerifyModelResult {
    let Ok(result) = run_single_turn_blocking(
        invoker,
        SingleTurnRequest {
            provider_plugin_id: Some(provider_plugin_id.to_string()),
            model_id: model_id.to_string(),
            provider_context: context.clone(),
            prompt: prompt.to_string(),
            system_prompt: Some("You are Bcode's model verification probe. Follow the user's instruction exactly and answer briefly.".to_string()),
            parameters: bcode_model::ModelParameters::default(),
            metadata: BTreeMap::from([(
                "bcode_request_kind".to_string(),
                "model_verification".to_string(),
            )]),
            timeout: std::time::Duration::from_secs(timeout_seconds),
        },
    ) else {
        return auth_diagnostics_verify_result(context);
    };
    let status = match result.status {
        SingleTurnStatus::Finished => "working",
        SingleTurnStatus::Cancelled | SingleTurnStatus::ProviderError => "provider_error",
        SingleTurnStatus::Timeout => "timeout",
    };
    CliVerifyModelResult {
        status: result
            .error
            .as_ref()
            .map_or_else(|| status.to_string(), provider_error_status),
        latency_ms: Some(result.latency_ms),
        error_code: result.error.as_ref().map(|error| error.code.clone()),
        message: result.error.map(|error| error.message),
    }
}

fn auth_diagnostics_verify_result(
    context: &bcode_model::ProviderRequestContext,
) -> CliVerifyModelResult {
    CliVerifyModelResult {
        status: "unauthorized".to_string(),
        latency_ms: None,
        error_code: Some("missing_openai_auth".to_string()),
        message: Some(auth_diagnostics_message(context)),
    }
}

fn auth_diagnostics_message(context: &bcode_model::ProviderRequestContext) -> String {
    let mut parts = Vec::new();
    if let Some(profile) = &context.auth_profile {
        parts.push(format!("auth_profile={profile}"));
    }
    if let Some(auth) = &context.auth {
        if let Some(backend) = &auth.backend {
            parts.push(format!("backend={backend}"));
        }
        if let Some(scheme) = &auth.scheme {
            parts.push(format!("scheme={scheme}"));
        }
        let mut credential_names = auth.credentials.keys().cloned().collect::<Vec<_>>();
        credential_names.sort();
        parts.push(format!("credentials_present={credential_names:?}"));
        for diagnostic in &auth.diagnostics {
            parts.push(format!(
                "diagnostic[{}:{}]={}",
                diagnostic.severity, diagnostic.code, diagnostic.message
            ));
        }
    }
    if parts.is_empty() {
        "auth context did not include credentials or diagnostics".to_string()
    } else {
        parts.join("; ")
    }
}

fn provider_error_status(error: &bcode_model::ProviderError) -> String {
    let message = error.message.to_lowercase();
    if message.contains("model is not supported")
        || message.contains("model is unsupported")
        || message.contains("unsupported model")
    {
        return "not_supported".to_string();
    }
    match error.category {
        bcode_model::ProviderErrorCategory::Auth => "unauthorized",
        bcode_model::ProviderErrorCategory::ModelNotFound => "not_found",
        bcode_model::ProviderErrorCategory::RateLimit => "rate_limited",
        bcode_model::ProviderErrorCategory::Timeout => "timeout",
        bcode_model::ProviderErrorCategory::Network => "network_error",
        _ => "provider_error",
    }
    .to_string()
}

/// Current schema version for the `verify-cache` CLI report envelope.
const CLI_VERIFY_CACHE_REPORT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize)]
struct CliVerifyCacheReport {
    schema_version: u32,
    provider_plugin_id: String,
    verified_at: String,
    dry_run: bool,
    total_models: usize,
    passed: bool,
    results: BTreeMap<String, CliVerifyCacheModelResult>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
enum CliVerifyCacheModelResult {
    /// Candidate listed without running (dry run).
    DryRun,
    /// The scenario suite produced a report; see its per-scenario outcomes.
    Verified {
        report: bcode_prompt_cache::PromptCacheVerificationReport,
    },
    /// The suite could not run to completion for this model.
    Error { message: String },
}

/// List candidate models for `verify-cache`, resolved through the model catalog exactly as the
/// daemon does: catalog-expanded models the provider cannot discover on its own become candidates,
/// and every candidate carries the catalog's cache claims (mechanism, TTLs, minimum prefix) that
/// the scenario suite judges by.
async fn verify_cache_candidates(
    host: &bcode_plugin::PluginHost,
    provider_plugin_id: &str,
    context: &bcode_model::ProviderRequestContext,
    selected_model_id: Option<&str>,
    args: &VerifyCacheArgs,
) -> Result<Vec<bcode_model::ModelInfo>, CliError> {
    let models: bcode_model::ModelList = host
        .invoke_service_json(
            provider_plugin_id,
            bcode_model::MODEL_PROVIDER_INTERFACE_ID,
            bcode_model::OP_MODELS,
            &bcode_model::ModelListRequest {
                provider_context: context.clone(),
                selected_model_id: selected_model_id.map(ToOwned::to_owned),
            },
        )
        .map_err(plugin_service_call_error)?;
    let catalog = bcode_model_catalog::ModelCatalogResolver::new(
        bcode_model_catalog::RemoteCatalogOptions::default(),
    )
    .map_err(|error| CliError::PluginCli(format!("model catalog unavailable: {error}")))?;
    let models = catalog
        .resolve_selection(models, selected_model_id, None)
        .await;
    let mut candidates = models
        .models
        .into_iter()
        .filter(|model| {
            args.id_pattern
                .as_deref()
                .is_none_or(|pattern| wildcard_match(pattern, &model.model_id))
        })
        .collect::<Vec<_>>();
    if let Some(max_models) = args.max_models {
        candidates.truncate(max_models);
    }
    Ok(candidates)
}

fn image_verification_fixture(
    args: &VerifyImagesArgs,
) -> Result<Option<Vec<bcode_model::ImageContent>>, CliError> {
    use std::io::Read as _;

    if args.max_models == 0 || args.timeout_seconds == 0 {
        return Err(CliError::PluginCli(
            "image probe model budget and timeout must be positive".to_string(),
        ));
    }
    let fixture = if args.dry_run {
        None
    } else {
        if args.image.is_empty() || args.image.len() > 8 {
            return Err(CliError::PluginCli(
                "provide one to eight --image fixtures".to_string(),
            ));
        }
        if args
            .question
            .as_deref()
            .is_none_or(|text| text.trim().is_empty())
            || args
                .expected_answer
                .as_deref()
                .is_none_or(|text| text.trim().is_empty())
        {
            return Err(CliError::PluginCli(
                "--question and --expected-answer are required".to_string(),
            ));
        }
        let mut remaining = 3 * 1024 * 1024;
        let mut images = Vec::new();
        for path in &args.image {
            let mut bytes = Vec::new();
            fs::File::open(path)?
                .take(remaining as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > remaining {
                return Err(CliError::PluginCli(
                    "image probe fixture exceeds 3 MiB".to_string(),
                ));
            }
            let mime_type = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
                "image/png"
            } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
                "image/jpeg"
            } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
                "image/gif"
            } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
                "image/webp"
            } else {
                return Err(CliError::PluginCli(
                    "unsupported image fixture signature".to_string(),
                ));
            };
            remaining -= bytes.len();
            images.push(bcode_model::ImageContent {
                mime_type: mime_type.to_string(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                metadata: bcode_model::ImageMetadata::default(),
            });
        }
        Some(images)
    };
    Ok(fixture)
}

fn prepare_image_probe(
    args: &VerifyImagesArgs,
) -> Result<(Option<Vec<bcode_model::ImageContent>>, String, String), CliError> {
    let generated = args
        .generated_seed
        .map(bcode_model_provider_runtime::image_fixtures::generate_image_fixture)
        .transpose()
        .map_err(CliError::PluginCli)?;
    let fixture = if generated.is_some() && !args.dry_run {
        if args.max_models == 0 || args.timeout_seconds == 0 {
            return Err(CliError::PluginCli(
                "image probe model budget and timeout must be positive".to_string(),
            ));
        }
        generated.as_ref().map(|fixture| fixture.images.clone())
    } else {
        image_verification_fixture(args)?
    };
    let question = generated.as_ref().map_or_else(
        || args.question.clone().unwrap_or_default(),
        |fixture| fixture.question.clone(),
    );
    let expected_answer = generated.as_ref().map_or_else(
        || args.expected_answer.clone().unwrap_or_default(),
        |fixture| fixture.expected_answer.clone(),
    );
    Ok((fixture, question, expected_answer))
}

async fn verify_model_images(args: &VerifyImagesArgs) -> Result<(), CliError> {
    use bcode_model_provider_runtime::image_verification::{
        ImageVerificationOptions, run_image_verification,
    };
    let (fixture, question, expected_answer) = prepare_image_probe(args)?;
    let config = bcode_config::load_config()?;
    let context = configured_provider_context(&config);
    let selection = config.resolved_model_selection();
    let provider = selection.provider_plugin_id.ok_or_else(|| {
        CliError::PluginCli("no provider configured; pass --provider".to_string())
    })?;
    let mut host = load_cli_plugin_host()?;
    let candidates = verify_cache_candidates(
        &host,
        &provider,
        &context,
        selection.selected_model_id.as_deref(),
        &VerifyCacheArgs {
            max_models: Some(args.max_models),
            id_pattern: args.id_pattern.clone(),
            dry_run: true,
            tool_rounds: 0,
            conversation_turns: 0,
            min_prefix_tokens: None,
            json: true,
            output: None,
            timeout_seconds: args.timeout_seconds,
        },
    )
    .await;
    let candidates = match candidates {
        Ok(models) => models,
        Err(error) => {
            host.deactivate_all()?;
            return Err(error);
        }
    };
    let mut results = BTreeMap::new();
    let mut failed = candidates.is_empty();
    let mut invoker = CliPluginTurnInvoker { host: &mut host };
    for model in candidates {
        let model_id = model.model_id.clone();
        let result = if let Some(image) = &fixture {
            let mut provider_context = context.clone();
            if provider_context.api_surface.is_none() {
                provider_context.api_surface = model.api_surface;
            }
            match run_image_verification(
                &mut invoker,
                &ImageVerificationOptions {
                    provider_plugin_id: Some(provider.clone()),
                    provider_context,
                    model,
                    images: image.clone(),
                    source: if args.tool_result {
                        bcode_model_provider_runtime::image_verification::ImageVerificationSource::ToolResult
                    } else {
                        bcode_model_provider_runtime::image_verification::ImageVerificationSource::User
                    },
                    question: question.clone(),
                    expected_answer: expected_answer.clone(),
                    allow_conversation_storage: args.allow_conversation_storage,
                    timeout: Duration::from_secs(args.timeout_seconds),
                },
            ) {
                Ok(report) => {
                    failed |= report.has_failures();
                    serde_json::json!({"status": "observed", "report": report})
                }
                Err(error) => {
                    failed = true;
                    serde_json::json!({"status": "error", "message": error})
                }
            }
        } else {
            serde_json::json!({"status": "dry_run", "media_input": model.feature_support.media_input})
        };
        results.insert(model_id, result);
    }
    host.deactivate_all()?;
    print_json(
        &serde_json::json!({"schema_version": 1, "provider": provider, "dry_run": args.dry_run, "results": results}),
    )?;
    if failed {
        Err(CliError::PluginCli(
            "image verification failed or no models selected".to_string(),
        ))
    } else {
        Ok(())
    }
}

async fn verify_model_caches(args: &VerifyCacheArgs) -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    let context = configured_provider_context(&config);
    let selection = config.resolved_model_selection();
    let provider_plugin_id = selection.provider_plugin_id.clone().ok_or_else(|| {
        CliError::PluginCli("no model provider is configured; pass --provider".to_string())
    })?;
    let mut host = load_cli_plugin_host()?;
    let candidates = verify_cache_candidates(
        &host,
        &provider_plugin_id,
        &context,
        selection.selected_model_id.as_deref(),
        args,
    )
    .await?;

    let mut results = BTreeMap::new();
    let mut passed = true;
    let mut invoker = CliPluginTurnInvoker { host: &mut host };
    for model in &candidates {
        let model_id = &model.model_id;
        let result = if args.dry_run {
            CliVerifyCacheModelResult::DryRun
        } else {
            let mut provider_context = context.clone();
            if provider_context.api_surface.is_none() {
                provider_context.api_surface = model.api_surface;
            }
            let options = bcode_prompt_cache::scenarios::PromptCacheScenarioOptions {
                provider_plugin_id: Some(provider_plugin_id.clone()),
                provider_context,
                model_id: Some(model_id.clone()),
                resolved_model: Some(model.clone()),
                overrides: bcode_prompt_cache::expectations::PromptCacheExpectationOverrides {
                    min_prefix_tokens: args.min_prefix_tokens,
                    ..Default::default()
                },
                tool_rounds: args.tool_rounds,
                conversation_turns: args.conversation_turns,
                turn_timeout: std::time::Duration::from_secs(args.timeout_seconds),
                ..Default::default()
            };
            match bcode_prompt_cache::scenarios::run_prompt_cache_scenarios(&mut invoker, &options)
            {
                Ok(report) => {
                    passed &= report.is_success();
                    CliVerifyCacheModelResult::Verified { report }
                }
                Err(error) => {
                    passed = false;
                    CliVerifyCacheModelResult::Error {
                        message: error.to_string(),
                    }
                }
            }
        };
        if !args.json {
            print_verify_cache_result(model_id, &result);
        }
        results.insert(model_id.clone(), result);
    }

    let report = CliVerifyCacheReport {
        schema_version: CLI_VERIFY_CACHE_REPORT_SCHEMA_VERSION,
        provider_plugin_id: provider_plugin_id.clone(),
        verified_at: unix_timestamp_string(),
        dry_run: args.dry_run,
        total_models: candidates.len(),
        passed,
        results,
    };
    // Provider work is complete; output failures must not skip plugin deactivation.
    host.deactivate_all()?;
    let body = serde_json::to_string_pretty(&report)?;
    if let Some(output) = &args.output {
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(output, &body)?;
        if args.json {
            eprintln!("wrote {}", display_from_current_dir(output));
        } else {
            println!("wrote {}", display_from_current_dir(output));
        }
    }
    if args.json {
        print_json(&report)?;
    }
    if passed {
        Ok(())
    } else {
        Err(CliError::PluginCli(format!(
            "prompt-cache verification failed for provider {provider_plugin_id}"
        )))
    }
}

fn print_verify_cache_result(model_id: &str, result: &CliVerifyCacheModelResult) {
    use bcode_prompt_cache::PromptCacheScenarioOutcome;
    match result {
        CliVerifyCacheModelResult::DryRun => println!("{model_id}\tdry_run"),
        CliVerifyCacheModelResult::Error { message } => println!("{model_id}\terror\t{message}"),
        CliVerifyCacheModelResult::Verified { report } => {
            let mechanism = report
                .expectations
                .as_ref()
                .map_or("not advertised", |expectations| {
                    match expectations.mechanism {
                        bcode_prompt_cache::PromptCacheMechanism::ExplicitPoints => {
                            "explicit points"
                        }
                        bcode_prompt_cache::PromptCacheMechanism::AutomaticPrefix => {
                            "automatic prefix"
                        }
                    }
                });
            println!(
                "{model_id}\t{}\tcache={mechanism}",
                if report.is_success() { "pass" } else { "FAIL" }
            );
            for scenario in &report.scenarios {
                let (status, detail) = match &scenario.outcome {
                    PromptCacheScenarioOutcome::Passed => ("pass", String::new()),
                    PromptCacheScenarioOutcome::Skipped { reason } => ("skip", reason.clone()),
                    PromptCacheScenarioOutcome::Failed { reason } => ("FAIL", reason.clone()),
                };
                let measurements = [
                    bcode_prompt_cache::measurement::WARM_READ_RATIO,
                    bcode_prompt_cache::measurement::HIT_ROUND_RATIO,
                    bcode_prompt_cache::measurement::LATE_UNCACHED_RATIO,
                    bcode_prompt_cache::measurement::WRITE_AMPLIFICATION,
                    bcode_prompt_cache::measurement::DROPPED_CACHE_POINTS,
                ]
                .into_iter()
                .filter_map(|key| {
                    scenario.measurements.get(key).map(|value| {
                        format!("{}={value:.2}", key.trim_start_matches("prompt_cache."))
                    })
                })
                .collect::<Vec<_>>()
                .join(" ");
                println!(
                    "  {:<22}{status:<6}{measurements}{detail}",
                    scenario.scenario
                );
            }
        }
    }
}

struct CliPluginTurnInvoker<'a> {
    host: &'a mut bcode_plugin::PluginHost,
}

impl BlockingModelProviderInvoker for CliPluginTurnInvoker<'_> {
    fn start_turn(
        &mut self,
        provider_plugin_id: Option<&str>,
        request: &bcode_model::ModelTurnRequest,
    ) -> Result<bcode_model::StartTurnResponse, String> {
        use bcode_provider_auth::auth_pool_routing::{
            apply_auth_pool_selection, prepare_auth_pool_usage, record_auth_pool_usage,
        };
        let mut request = request.clone();
        for usage_request in prepare_auth_pool_usage(&mut request.provider_context) {
            if let Ok(response) = self.invoke_json::<_, bcode_model::AuthUsageResponse>(
                provider_plugin_id,
                bcode_model::OP_AUTH_USAGE,
                &usage_request,
            ) {
                record_auth_pool_usage(&usage_request, &response);
            }
        }
        apply_auth_pool_selection(&mut request.provider_context);
        self.invoke_json(provider_plugin_id, bcode_model::OP_START_TURN, &request)
    }

    fn invoke_json<Q, R>(
        &mut self,
        provider_plugin_id: Option<&str>,
        operation: &'static str,
        request: &Q,
    ) -> Result<R, String>
    where
        Q: serde::Serialize,
        R: serde::de::DeserializeOwned,
    {
        let plugin_id =
            provider_plugin_id.ok_or_else(|| "missing provider plugin id".to_string())?;
        self.host
            .invoke_service_json(
                plugin_id,
                bcode_model::MODEL_PROVIDER_INTERFACE_ID,
                operation,
                request,
            )
            .map_err(|error| error.to_string())
    }
}

#[derive(Debug, Serialize)]
struct CliVerifyReport {
    provider: String,
    verified_at: String,
    prompt: String,
    dry_run: bool,
    total_models: usize,
    results: BTreeMap<String, CliVerifyModelResult>,
}

#[derive(Debug, Serialize)]
struct CliVerifyModelResult {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    latency_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let parts = pattern.split('*').collect::<Vec<_>>();
    if parts.len() == 1 {
        return pattern == value;
    }
    let mut remaining = value;
    if let Some(first) = parts.first()
        && !first.is_empty()
    {
        let Some(stripped) = remaining.strip_prefix(first) else {
            return false;
        };
        remaining = stripped;
    }
    for part in parts.iter().skip(1).take(parts.len().saturating_sub(2)) {
        if part.is_empty() {
            continue;
        }
        let Some(index) = remaining.find(part) else {
            return false;
        };
        remaining = &remaining[index + part.len()..];
    }
    if let Some(last) = parts.last()
        && !last.is_empty()
    {
        return remaining.ends_with(last);
    }
    true
}

fn unix_timestamp_string() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or_else(
            |_| "0".to_string(),
            |duration| duration.as_secs().to_string(),
        )
}

async fn model_validate_config(json: bool) -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    let provider = config.resolved_model_selection().provider_plugin_id;
    let client = BcodeClient::default_endpoint();
    let validation = client.validate_model_config(provider).await?;
    write_typed_model_validation(&mut std::io::stdout().lock(), &validation, json)
}

#[cfg(test)]
fn write_model_validation(
    output: &mut impl std::io::Write,
    response: bcode_ipc::PluginServiceResponse,
    json: bool,
) -> Result<(), CliError> {
    if let Some(error) = response.error {
        return Err(CliError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    let validation: bcode_model::ValidateConfigResponse =
        serde_json::from_slice(&response.payload)?;
    write_typed_model_validation(output, &validation, json)
}

fn write_typed_model_validation(
    output: &mut impl std::io::Write,
    validation: &bcode_model::ValidateConfigResponse,
    json: bool,
) -> Result<(), CliError> {
    if json {
        write_json_result(output, validation)?;
    } else {
        writeln!(output, "valid\t{}", validation.valid)?;
        if let Some(message) = &validation.message {
            writeln!(output, "message\t{message}")?;
        }
        for failure in &validation.failures {
            writeln!(
                output,
                "failure\t{}\t{:?}\t{}\t{:?}",
                failure.provider_id, failure.source_kind, failure.source, failure.capability
            )?;
            writeln!(output, "remediation\t{}", failure.remediation)?;
        }
        for (key, value) in &validation.metadata {
            writeln!(output, "metadata\t{key}\t{value}")?;
        }
        output.flush()?;
    }
    if !validation.valid {
        return Err(CliError::PluginService {
            code: "invalid_configuration".to_owned(),
            message: "model provider configuration is invalid".to_owned(),
        });
    }
    Ok(())
}

fn plugin_service_call_error(error: bcode_plugin::PluginServiceCallError) -> CliError {
    match error {
        bcode_plugin::PluginServiceCallError::Invoke(error) => CliError::Plugin(error),
        bcode_plugin::PluginServiceCallError::Service { code, message } => {
            CliError::PluginService { code, message }
        }
        bcode_plugin::PluginServiceCallError::RequestEncode(error)
        | bcode_plugin::PluginServiceCallError::ResponseDecode(error) => CliError::Json(error),
    }
}

fn load_cli_plugin_host() -> Result<bcode_plugin::PluginHost, CliError> {
    let config = bcode_config::load_config()?;
    let selection = plugin_selection_for_config(&config);
    let static_plugins = static_bundled_plugins();
    bcode_plugin::PluginHost::load_defaults_with_static_bundled(&selection, &static_plugins)
        .map_err(CliError::Plugin)
}

/// Return caller-provided statically bundled plugin registrations.
#[must_use]
fn static_bundled_plugins() -> Vec<bcode_plugin::StaticBundledPlugin> {
    STATIC_BUNDLED_PLUGINS.get().cloned().unwrap_or_default()
}

fn configured_provider_context(
    config: &bcode_config::BcodeConfig,
) -> bcode_model::ProviderRequestContext {
    bcode_provider_auth::resolve_provider_request_context(
        bcode_provider_auth::ProviderRequestContextResolution {
            config,
            selection: config.resolved_model_selection(),
        },
    )
}

async fn call_model_provider_service_payload(
    operation: &str,
    payload: Vec<u8>,
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
                payload,
            )
            .await
            .map_err(CliError::from)
    } else {
        client
            .call_plugin_service(
                bcode_model::MODEL_PROVIDER_INTERFACE_ID.to_string(),
                operation.to_string(),
                payload,
            )
            .await
            .map_err(CliError::from)
    }
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
    BcodeClient::default_endpoint()
        .ensure_daemon_available()
        .await?;
    Ok(())
}

async fn run_server_foreground() -> Result<(), CliError> {
    bcode_server::run_with_static_bundled(default_endpoint(), &static_bundled_plugins()).await?;
    Ok(())
}

async fn start_server_daemon(quiet: bool) -> Result<(), CliError> {
    bcode_daemon_lifecycle::ensure_daemon_running(&bcode_daemon_lifecycle::EnsureDaemonOptions {
        endpoint: default_endpoint(),
        quiet,
        log_path: daemon_log_path(),
    })
    .await?;
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

fn print_server_identity(status: &ServerStatus, verbose: bool) -> Result<(), CliError> {
    println!("daemon: running");
    println!("namespace: {}", status.daemon.namespace);
    println!(
        "artifact identity: {}",
        status
            .daemon
            .artifact_id
            .as_ref()
            .map_or("<unknown>", bcode_ipc::ArtifactId::as_str)
    );
    println!(
        "executable identity: {}",
        status
            .daemon
            .executable_digest
            .as_deref()
            .unwrap_or("<unknown>")
    );
    if verbose {
        let (client_executable, client_digest) =
            bcode_daemon_lifecycle::current_executable_identity()?;
        let record = bcode_daemon_lifecycle::read_records(&bcode_config::default_state_dir())
            .into_iter()
            .find_map(|(_path, record)| {
                (record.namespace == status.daemon.namespace).then_some(record)
            });
        println!(
            "client executable: {}",
            display_from_current_dir(&client_executable)
        );
        println!("client executable identity: {client_digest}");
        println!(
            "daemon executable: {}",
            record
                .as_ref()
                .and_then(|record| record.executable_path.as_deref())
                .map_or_else(
                    || "<unknown>".to_owned(),
                    |path| display_from_current_dir(path).to_string()
                )
        );
        println!(
            "registry identity: {}",
            if record
                .as_ref()
                .is_some_and(|record| daemon_status_matches(record, &status.daemon))
            {
                "consistent"
            } else {
                "missing or inconsistent"
            }
        );
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
    Ok(())
}

async fn daemon_startup_probe() -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let started = Instant::now();
    client.connect("bcode-daemon-startup-probe").await?;
    println!("{}", started.elapsed().as_micros());
    Ok(())
}

async fn server_status(verbose: bool) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let status = client.verified_server_status().await?;
    print_server_identity(&status, verbose)?;
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
        bcode_ipc::SessionCatalogStatus::Degraded(message) => {
            println!(
                "sessions: {} cached (catalog degraded: {message})",
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
    print_active_runtime_work(
        &status.active_runtime_work,
        status.idle_shutdown_blocker.as_deref(),
    );
    if verbose {
        print_metrics_summary(&status.metrics);
    }
    println!("log: {}", display_from_current_dir(daemon_log_path()));
    for session in status.sessions {
        println!(
            "{}\t{}\t{} clients",
            session.display_title(),
            session.id,
            session.client_count
        );
    }
    Ok(())
}

async fn server_metrics(json: bool, report: bool) -> Result<(), CliError> {
    let status = BcodeClient::default_endpoint().server_status().await?;
    if json || report {
        let value = if report {
            serde_json::to_value(&status.metrics_report)?
        } else {
            serde_json::to_value(&status.metrics)?
        };
        print_json(&value)?;
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
    let status = client.verified_server_status().await?;
    let diagnosis = ServerDiagnosis::from_status(status)?;
    if json {
        print_json(&diagnosis)?;
    } else {
        print_server_diagnosis(&diagnosis);
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
struct ServerDiagnosis {
    daemon: bcode_ipc::DaemonStatus,
    client_executable_path: PathBuf,
    client_executable_digest: String,
    daemon_executable_path: Option<PathBuf>,
    registry_identity_consistent: bool,
    connected_client_count: usize,
    session_count: usize,
    sessions: Vec<SessionDiagnosisSummary>,
    selected_provider_plugin_id: Option<String>,
    selected_model_id: Option<String>,
    plugin_runtime: Vec<bcode_plugin::PluginExecutorStatus>,
    active_runtime_work: Vec<bcode_ipc::SessionRuntimeWork>,
    idle_shutdown_blocker: Option<String>,
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
    fn from_status(status: ServerStatus) -> Result<Self, CliError> {
        let observations = diagnostic_observations(&status);
        let (client_executable_path, client_executable_digest) =
            bcode_daemon_lifecycle::current_executable_identity()?;
        let record = bcode_daemon_lifecycle::read_records(&bcode_config::default_state_dir())
            .into_iter()
            .find_map(|(_path, record)| {
                (record.namespace == status.daemon.namespace).then_some(record)
            });
        let daemon_executable_path = record
            .as_ref()
            .and_then(|record| record.executable_path.clone());
        let registry_identity_consistent = record
            .as_ref()
            .is_some_and(|record| daemon_status_matches(record, &status.daemon));
        Ok(Self {
            daemon: status.daemon,
            client_executable_path,
            client_executable_digest,
            daemon_executable_path,
            registry_identity_consistent,
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
            active_runtime_work: status.active_runtime_work,
            idle_shutdown_blocker: status.idle_shutdown_blocker,
            metrics: status.metrics,
            observations,
        })
    }
}

fn diagnostic_observations(status: &ServerStatus) -> Vec<DiagnosticObservation> {
    let mut observations = Vec::new();
    add_histogram_observation(
        &mut observations,
        status,
        "session.actor.append_event.duration_ms",
        100,
        "slow_session_event_appends",
        "canonical session event appends have exceeded 100ms",
    );
    add_histogram_average_observation(
        &mut observations,
        status,
        "session.actor.append_event.db_append_duration_ms",
        15,
        50,
        "high_average_session_append_latency",
        "canonical session appends average over 15ms; check per-append statement count, lock contention, and disk sync cost",
    );
    add_histogram_observation(
        &mut observations,
        status,
        "session.catalog.flush_duration_ms",
        500,
        "slow_session_catalog_flush",
        "session catalog flushes have exceeded 500ms",
    );
    add_histogram_observation(
        &mut observations,
        status,
        "model.request_build_duration_ms",
        500,
        "slow_model_request_builds",
        "model request construction has exceeded 500ms",
    );
    add_histogram_average_observation(
        &mut observations,
        status,
        "model.provider.service.duration_ms",
        100,
        20,
        "slow_model_provider_service_calls",
        "model provider control-plane calls (models/capabilities) average over 100ms; request construction repeats them several times per turn",
    );
    add_histogram_observation(
        &mut observations,
        status,
        "database.operation.duration_ms",
        1_000,
        "slow_database_operation",
        "a single session database operation has exceeded 1s; check lock contention between daemons",
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
    add_histogram_observation(
        &mut observations,
        status,
        "model.provider.first_output_latency_ms",
        10_000,
        "slow_time_to_first_output",
        "model provider took over 10s to produce first output",
    );
    add_histogram_observation(
        &mut observations,
        status,
        "model.provider.poll_idle_wait_duration_ms",
        5_000,
        "high_poll_idle_wait",
        "Bcode spent over 5s of a turn waiting between provider polls",
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
    add_session_search_retry_observation(&mut observations, status);
    observations
}

/// Flag a session-search ingestion worker that is failing far more often than it succeeds.
///
/// A degraded or locked provider used to requeue every dirty session in a hot loop, which was
/// visible only as steadily rising daemon CPU and database operation counts. The failure/completion
/// ratio is the direct signal for that regression.
fn add_session_search_retry_observation(
    observations: &mut Vec<DiagnosticObservation>,
    status: &ServerStatus,
) {
    let counter = |name: &str| {
        status
            .metrics
            .counters
            .get(name)
            .copied()
            .unwrap_or_default()
    };
    let failed = counter("server.session_search.ingestion_session_failed_total");
    let completed = counter("server.session_search.ingestion_session_completed_total");
    let requeued = counter("server.session_search.ingestion_session_requeued_total");
    if failed >= 100 && failed > completed.saturating_mul(4) {
        observations.push(DiagnosticObservation {
            severity: DiagnosticSeverity::Warning,
            code: "session_search_ingestion_retry_storm".to_string(),
            message: format!(
                "session-search ingestion failed {failed} times against {completed} completions ({requeued} requeues); run `bcode session search-status` to find the degraded provider"
            ),
        });
    }
}

/// Flag a histogram whose average exceeds `threshold_ms` once it has enough samples to be stable.
fn add_histogram_average_observation(
    observations: &mut Vec<DiagnosticObservation>,
    status: &ServerStatus,
    key: &str,
    threshold_ms: u64,
    minimum_samples: u64,
    code: &str,
    message: &str,
) {
    let Some(histogram) = status.metrics.histograms.get(key) else {
        return;
    };
    if histogram.count < minimum_samples {
        return;
    }
    let average_ms = histogram.sum / histogram.count.max(1);
    if average_ms >= threshold_ms {
        observations.push(DiagnosticObservation {
            severity: DiagnosticSeverity::Warning,
            code: code.to_string(),
            message: format!(
                "{message}; average={average_ms}ms over {} samples, max={}ms",
                histogram.count,
                histogram.max.unwrap_or_default()
            ),
        });
    }
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
    println!(
        "client executable: {}",
        display_from_current_dir(&diagnosis.client_executable_path)
    );
    println!(
        "client executable identity: {}",
        diagnosis.client_executable_digest
    );
    println!(
        "daemon executable: {}",
        diagnosis.daemon_executable_path.as_ref().map_or_else(
            || "<unknown>".to_owned(),
            |path| display_from_current_dir(path).to_string()
        )
    );
    println!(
        "daemon executable identity: {}",
        diagnosis
            .daemon
            .executable_digest
            .as_deref()
            .unwrap_or("<unknown>")
    );
    println!(
        "registry identity: {}",
        if diagnosis.registry_identity_consistent {
            "consistent"
        } else {
            "missing or inconsistent"
        }
    );
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
    print_active_runtime_work(
        &diagnosis.active_runtime_work,
        diagnosis.idle_shutdown_blocker.as_deref(),
    );
    print_metrics_summary(&diagnosis.metrics);
}

fn print_orphaned_workflow_report(report: &bcode_ipc::OrphanedWorkflowRunReport) {
    let verb = if report.applied {
        "reconciled"
    } else {
        "would reconcile"
    };
    println!(
        "{verb} {} orphaned workflow run(s); skipped {}",
        report.reconciled.len(),
        report.skipped.len()
    );
    for run in &report.reconciled {
        println!(
            "  {} {:?} kind={} session={} ended-daemon={} artifact={} image={}",
            run.run_id,
            run.status,
            run.workflow_kind.as_deref().unwrap_or("<none>"),
            run.parent_session_id.as_deref().unwrap_or("<none>"),
            run.ended_daemon_instance_id,
            run.ended_target_artifact_id,
            if run.artifact_image_available {
                "present"
            } else {
                "missing"
            }
        );
    }
    if !report.skipped.is_empty() {
        println!("skipped:");
        for run in &report.skipped {
            println!("  {} {:?}: {}", run.run_id, run.status, run.reason);
        }
    }
    if !report.applied && !report.reconciled.is_empty() {
        println!("re-run with --apply to reassign and cancel the runs listed above");
    }
}

fn print_active_runtime_work(work: &[bcode_ipc::SessionRuntimeWork], blocker: Option<&str>) {
    if work.is_empty() {
        println!("runtime work: none");
    } else {
        println!("runtime work: {} active registration(s)", work.len());
        for entry in work {
            println!(
                "  {} {:?} {} [{}] session={}",
                entry.work.work_id,
                entry.work.kind,
                entry.work.label,
                runtime_work_status_word(entry.work.status),
                entry.session_id
            );
        }
    }
    match blocker {
        Some(blocker) => println!("idle shutdown: blocked ({blocker})"),
        None => println!("idle shutdown: eligible"),
    }
}

const fn runtime_work_status_word(status: bcode_session_models::RuntimeWorkStatus) -> &'static str {
    use bcode_session_models::RuntimeWorkStatus;
    match status {
        RuntimeWorkStatus::Queued => "queued",
        RuntimeWorkStatus::Running => "running",
        RuntimeWorkStatus::Cancelling => "cancelling",
        RuntimeWorkStatus::Completed => "completed",
        RuntimeWorkStatus::Failed => "failed",
        RuntimeWorkStatus::TimedOut => "timed_out",
        RuntimeWorkStatus::Cancelled => "cancelled",
        RuntimeWorkStatus::Suspended => "suspended",
    }
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
            let avg = histogram.sum.checked_div(histogram.count).unwrap_or(0);
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

#[derive(Debug, Default)]
struct DaemonCleanupSummary {
    stopped: usize,
    removed: usize,
    skipped: usize,
    messages: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonControlPolicy {
    /// The current build can decode the daemon protocol; stop over IPC.
    GracefulIpc,
    /// The current build cannot decode the daemon protocol, but independent process evidence
    /// exactly identifies the daemon. Graceful stop is delegated to the daemon's own retained
    /// content-addressed executable; forced termination remains a reviewed explicit action.
    DelegatedGraceful,
    /// Identity is mismatched or unverifiable; preserve the record and refuse control.
    PreserveAndRefuse,
    /// Positive evidence proves the process is gone; prune the registry record.
    PruneStale,
}

const fn daemon_control_policy(
    classification: bcode_daemon_lifecycle::DaemonRecordClassification,
) -> DaemonControlPolicy {
    match classification {
        bcode_daemon_lifecycle::DaemonRecordClassification::CurrentHealthy
        | bcode_daemon_lifecycle::DaemonRecordClassification::HistoricalExactResponsive => {
            DaemonControlPolicy::GracefulIpc
        }
        bcode_daemon_lifecycle::DaemonRecordClassification::HistoricalProcessVerifiedProtocolUnsupported => {
            DaemonControlPolicy::DelegatedGraceful
        }
        bcode_daemon_lifecycle::DaemonRecordClassification::ResponsiveIdentityMismatch
        | bcode_daemon_lifecycle::DaemonRecordClassification::Unverifiable => {
            DaemonControlPolicy::PreserveAndRefuse
        }
        bcode_daemon_lifecycle::DaemonRecordClassification::UnreachableStale => {
            DaemonControlPolicy::PruneStale
        }
    }
}

async fn cleanup_delegated_graceful_daemon(
    record: &bcode_daemon_lifecycle::DaemonRecord,
    classification: bcode_daemon_lifecycle::DaemonRecordClassification,
    stop_current: bool,
    verbose: bool,
    summary: &mut DaemonCleanupSummary,
) {
    if !stop_current {
        // `cleanup` only stops idle daemons. This build cannot ask a protocol-unsupported daemon
        // whether it is idle, so preserve it.
        summary.skipped = summary.skipped.saturating_add(1);
        if verbose {
            summary.messages.push(format!(
                "preserved {}: {classification:?} (use `bcode server stop-all --yes` to stop it through its own executable)",
                record.namespace
            ));
        }
        return;
    }
    match stop_protocol_unsupported_daemon_via_own_executable(record).await {
        Ok(()) => {
            summary.stopped = summary.stopped.saturating_add(1);
            if verbose {
                summary.messages.push(format!(
                    "stopped {} through its own executable",
                    record.namespace
                ));
            }
        }
        Err(error) => {
            summary.skipped = summary.skipped.saturating_add(1);
            if verbose {
                summary.messages.push(format!(
                    "skipped {}: delegated stop failed: {error}",
                    record.namespace
                ));
            }
        }
    }
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
        let classification = bcode_daemon_lifecycle::classify_daemon_record(&record).await;
        match daemon_control_policy(classification) {
            DaemonControlPolicy::GracefulIpc => {
                let Some(endpoint) = record.endpoint.to_ipc_endpoint() else {
                    summary.skipped = summary.skipped.saturating_add(1);
                    continue;
                };
                let client = BcodeClient::new(endpoint)
                    .with_daemon_availability(DaemonAvailability::RequireRunning);
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
            DaemonControlPolicy::PruneStale => {
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
            DaemonControlPolicy::DelegatedGraceful => {
                cleanup_delegated_graceful_daemon(
                    &record,
                    classification,
                    stop_current,
                    verbose,
                    &mut summary,
                )
                .await;
            }
            DaemonControlPolicy::PreserveAndRefuse => {
                summary.skipped = summary.skipped.saturating_add(1);
                if verbose {
                    summary.messages.push(format!(
                        "preserved {}: {classification:?}",
                        record.namespace
                    ));
                }
            }
        }
    }
    if let Ok(removed_images) =
        bcode_daemon_lifecycle::cleanup_stale_daemon_images_retaining_artifacts(
            &state_dir,
            &workflow_owning_artifact_ids(&state_dir),
        )
        && verbose
        && removed_images > 0
    {
        summary
            .messages
            .push(format!("removed {removed_images} stale daemon image(s)"));
    }
    summary
}

/// Return artifact identities that still own resumable workflow runs.
///
/// Image cleanup must not delete the only launchable daemon for an artifact that still owns
/// `running`/`paused` work, because execution authority is fenced to that exact artifact.
///
/// This is best-effort retention evidence: when the workflow store cannot be inspected, the empty
/// set is returned so an unavailable optional domain never blocks daemon maintenance.
fn workflow_owning_artifact_ids(state_dir: &Path) -> BTreeSet<String> {
    let path = bcode_workflow_store::workflow_database_path(state_dir);
    if !path.exists() {
        return BTreeSet::new();
    }
    bcode_workflow_store::WorkflowStore::open_at_path(&path)
        .and_then(|store| store.active_target_artifact_ids(1_000))
        .map(BTreeSet::from_iter)
        .unwrap_or_default()
}

/// Retention provider registered with daemon lifecycle image cleanup.
fn default_state_workflow_owning_artifact_ids() -> BTreeSet<String> {
    workflow_owning_artifact_ids(&bcode_config::default_state_dir())
}

/// Register workflow-owned artifact retention with daemon image cleanup.
///
/// Keeps background cleanup from stranding durable workflow work whose execution authority is
/// fenced to an artifact that would otherwise lose its only launchable image.
pub fn register_workflow_artifact_retention() {
    bcode_daemon_lifecycle::register_retained_artifact_provider(
        default_state_workflow_owning_artifact_ids,
    );
}

fn daemon_status_matches(
    record: &bcode_daemon_lifecycle::DaemonRecord,
    status: &bcode_ipc::DaemonStatus,
) -> bool {
    status.namespace == record.namespace
        && status.instance_id == record.instance_id
        && status.artifact_id == record.artifact_id
        && status.build_fingerprint == record.build_fingerprint
        && status.executable_digest == record.executable_digest
        && status.storage_writer_epoch == record.storage_writer_epoch
}

#[cfg(unix)]
fn remove_stale_socket(record: &bcode_daemon_lifecycle::DaemonRecord) {
    if let bcode_daemon_lifecycle::DaemonEndpointRecord::UnixSocket { path } = &record.endpoint
        && is_bcode_socket_path(path)
        && !unix_socket_has_listener(path)
    {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(not(unix))]
const fn remove_stale_socket(_record: &bcode_daemon_lifecycle::DaemonRecord) {}

#[cfg(unix)]
fn is_bcode_socket_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with("bcode-")
                && Path::new(name)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("sock"))
        })
}

#[cfg(unix)]
fn unix_socket_has_listener(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

async fn retire_incompatible_daemons() -> Result<(), CliError> {
    let state_dir = bcode_config::default_state_dir();
    let incompatible = bcode_daemon_lifecycle::incompatible_storage_writer_records(
        &state_dir,
        bcode_session::lease::CURRENT_SESSION_STORAGE_WRITER_EPOCH,
    )
    .await;
    if incompatible.is_empty() {
        println!("No incompatible live Bcode daemons found");
        return Ok(());
    }
    for (record_path, record) in incompatible {
        let endpoint = record.endpoint.to_ipc_endpoint().ok_or_else(|| {
            CliError::IncompatibleDaemonStorage(format!(
                "cannot retire namespace {}: unsupported endpoint {:?}",
                record.namespace, record.endpoint
            ))
        })?;
        let client = BcodeClient::new(endpoint)
            .with_request_timeout(Duration::from_secs(2))
            .with_daemon_availability(DaemonAvailability::RequireRunning);
        let status = client.server_status().await?;
        if !daemon_status_matches(&record, &status.daemon) {
            return Err(CliError::IncompatibleDaemonStorage(format!(
                "refusing to stop namespace {}: registry instance no longer matches the responding daemon",
                record.namespace
            )));
        }
        client.server_stop().await?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let still_live = bcode_daemon_lifecycle::live_records(&state_dir)
                .await
                .into_iter()
                .any(|(_, live)| live.instance_id == record.instance_id);
            if !still_live {
                break;
            }
            if Instant::now() >= deadline {
                return Err(CliError::IncompatibleDaemonStorage(format!(
                    "daemon {} ({}) did not stop within 5 seconds",
                    record.namespace, record.instance_id
                )));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        bcode_daemon_lifecycle::remove_record_if_instance(&record_path, &record.instance_id)?;
        println!(
            "Retired incompatible daemon {} (pid {:?}, build {}, writer epoch {:?})",
            record.namespace, record.pid, record.build_fingerprint, record.storage_writer_epoch
        );
    }
    Ok(())
}

async fn session_owner_record(
    session_id: SessionId,
) -> Result<bcode_daemon_lifecycle::DaemonRecord, CliError> {
    let root = bcode_config::default_session_store_dir();
    let owners = bcode_session::lease::active_session_owners(&root, session_id)?;
    let classified =
        bcode_daemon_lifecycle::classified_records(&bcode_config::default_state_dir()).await;
    let matching = owners
        .into_iter()
        .filter_map(|owner| {
            let instance_id = owner.daemon_instance_id.as_deref()?;
            classified
                .iter()
                .find(|(_, record, classification)| {
                    record.instance_id == instance_id
                        && record.pid == Some(owner.pid)
                        && matches!(
                            classification,
                            bcode_daemon_lifecycle::DaemonRecordClassification::CurrentHealthy
                                | bcode_daemon_lifecycle::DaemonRecordClassification::HistoricalExactResponsive
                                | bcode_daemon_lifecycle::DaemonRecordClassification::HistoricalProcessVerifiedProtocolUnsupported
                        )
                })
                .map(|(_, record, _)| record.clone())
        })
        .collect::<Vec<_>>();
    match matching.as_slice() {
        [record] => Ok(record.clone()),
        [] => Err(CliError::InvalidArguments(format!(
            "no verified live Bcode daemon owner was found for session {session_id}"
        ))),
        _ => Err(CliError::InvalidArguments(format!(
            "session {session_id} has multiple verified daemon owners; refusing to target an arbitrary process"
        ))),
    }
}

const fn session_ownership_blocker_label(
    blocker: bcode_ipc::SessionOwnershipBlocker,
) -> &'static str {
    match blocker {
        bcode_ipc::SessionOwnershipBlocker::AttachedClient => "attached client",
        bcode_ipc::SessionOwnershipBlocker::PendingAttach => "pending attach",
        bcode_ipc::SessionOwnershipBlocker::QueuedCommand => "queued command",
        bcode_ipc::SessionOwnershipBlocker::ActiveRuntime => "active runtime",
        bcode_ipc::SessionOwnershipBlocker::RuntimeWork => "runtime work",
        bcode_ipc::SessionOwnershipBlocker::PluginInvocation => "plugin invocation",
        bcode_ipc::SessionOwnershipBlocker::Migration => "migration",
        bcode_ipc::SessionOwnershipBlocker::DatabaseHandleRetained => {
            "retained session database handle"
        }
    }
}

fn session_ownership_release_message(
    instance_id: &str,
    outcome: bcode_ipc::SessionOwnershipReleaseOutcome,
) -> Result<String, CliError> {
    match outcome {
        bcode_ipc::SessionOwnershipReleaseOutcome::Released
        | bcode_ipc::SessionOwnershipReleaseOutcome::AlreadyUnowned => Ok(format!(
            "released session ownership from daemon {instance_id}"
        )),
        bcode_ipc::SessionOwnershipReleaseOutcome::Blocked { blockers } => {
            let blockers = blockers
                .into_iter()
                .map(session_ownership_blocker_label)
                .collect::<Vec<_>>()
                .join(", ");
            Err(CliError::InvalidArguments(format!(
                "daemon {instance_id} refused ownership release; blockers: {blockers}"
            )))
        }
    }
}

async fn release_session_owner(session_id: SessionId) -> Result<(), CliError> {
    let record = session_owner_record(session_id).await?;
    let endpoint = record.endpoint.to_ipc_endpoint().ok_or_else(|| {
        CliError::InvalidArguments(format!(
            "daemon {} has no supported IPC endpoint",
            record.instance_id
        ))
    })?;
    let client =
        BcodeClient::new(endpoint).with_daemon_availability(DaemonAvailability::RequireRunning);
    let message = session_ownership_release_message(
        &record.instance_id,
        client.release_session_ownership(session_id).await?,
    )?;
    println!("{message}");
    Ok(())
}

async fn stop_session_owner(session_id: SessionId, force: bool) -> Result<(), CliError> {
    let record = session_owner_record(session_id).await?;
    if force {
        terminate_verified_daemon(&record).await?;
        println!("terminated session owner {}", record.instance_id);
        return Ok(());
    }
    let classification = bcode_daemon_lifecycle::classify_daemon_record(&record).await;
    if matches!(
        classification,
        bcode_daemon_lifecycle::DaemonRecordClassification::HistoricalProcessVerifiedProtocolUnsupported
    ) {
        stop_protocol_unsupported_daemon_via_own_executable(&record).await?;
        println!(
            "stopped session owner {} through its own executable",
            record.instance_id
        );
        return Ok(());
    }
    if !matches!(
        classification,
        bcode_daemon_lifecycle::DaemonRecordClassification::CurrentHealthy
            | bcode_daemon_lifecycle::DaemonRecordClassification::HistoricalExactResponsive
    ) {
        return Err(CliError::InvalidArguments(format!(
            "refusing graceful stop because daemon identity is {classification:?}"
        )));
    }
    let endpoint = record.endpoint.to_ipc_endpoint().ok_or_else(|| {
        CliError::InvalidArguments(format!(
            "daemon {} has no supported IPC endpoint",
            record.instance_id
        ))
    })?;
    BcodeClient::new(endpoint)
        .with_daemon_availability(DaemonAvailability::RequireRunning)
        .server_stop()
        .await?;
    wait_for_daemon_exit(&record).await?;
    println!("stopped session owner {}", record.instance_id);
    Ok(())
}

/// Gracefully stop a process-verified daemon whose protocol this build cannot decode by
/// delegating the stop request to the daemon's own retained executable.
///
/// The record must still classify as `HistoricalProcessVerifiedProtocolUnsupported`, and the
/// recorded executable path must be an immutable content-addressed daemon image whose bytes still
/// match the recorded digest. That executable speaks the daemon's exact protocol, so it can request
/// a graceful stop the same way the daemon's own clients would, and the daemon keeps its normal
/// refusal semantics for in-flight work.
///
/// # Errors
///
/// * The record no longer classifies as process-verified and protocol-unsupported.
/// * The record has no executable path or digest.
/// * The executable path is not a content-addressed daemon image, or its bytes no longer match the
///   recorded digest.
/// * The delegated stop command fails to launch or exits unsuccessfully.
/// * The daemon does not exit within the bounded wait.
async fn stop_protocol_unsupported_daemon_via_own_executable(
    expected: &bcode_daemon_lifecycle::DaemonRecord,
) -> Result<(), CliError> {
    let classification = bcode_daemon_lifecycle::classify_daemon_record(expected).await;
    if !matches!(
        classification,
        bcode_daemon_lifecycle::DaemonRecordClassification::HistoricalProcessVerifiedProtocolUnsupported
    ) {
        return Err(CliError::InvalidArguments(format!(
            "refusing delegated stop because daemon identity is {classification:?}"
        )));
    }
    let executable = expected.executable_path.as_deref().ok_or_else(|| {
        CliError::InvalidArguments(format!(
            "daemon {} has no recorded executable path for delegated stop",
            expected.instance_id
        ))
    })?;
    let digest = expected.executable_digest.as_deref().ok_or_else(|| {
        CliError::InvalidArguments(format!(
            "daemon {} has no recorded executable digest for delegated stop",
            expected.instance_id
        ))
    })?;
    if !bcode_daemon_lifecycle::executable_path_matches_digest(executable, digest) {
        return Err(CliError::InvalidArguments(format!(
            "refusing delegated stop for daemon {}: executable {} is not a content-addressed daemon image",
            expected.instance_id,
            executable.display()
        )));
    }
    let actual_digest = bcode_daemon_lifecycle::executable_sha256(executable)?;
    if actual_digest != digest {
        return Err(CliError::InvalidArguments(format!(
            "refusing delegated stop for daemon {}: executable {} no longer matches its recorded digest",
            expected.instance_id,
            executable.display()
        )));
    }
    let status = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(executable)
            .args(["server", "stop"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status(),
    )
    .await
    .map_err(|_| {
        CliError::InvalidArguments(format!(
            "delegated stop for daemon {} did not complete within 5 seconds",
            expected.instance_id
        ))
    })??;
    if !status.success() {
        return Err(CliError::InvalidArguments(format!(
            "delegated stop for daemon {} exited with {status}",
            expected.instance_id
        )));
    }
    wait_for_daemon_exit(expected).await
}

async fn wait_for_daemon_exit(
    expected: &bcode_daemon_lifecycle::DaemonRecord,
) -> Result<(), CliError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let still_live = matches!(
            bcode_daemon_lifecycle::classify_daemon_record(expected).await,
            bcode_daemon_lifecycle::DaemonRecordClassification::CurrentHealthy
                | bcode_daemon_lifecycle::DaemonRecordClassification::HistoricalExactResponsive
                | bcode_daemon_lifecycle::DaemonRecordClassification::HistoricalProcessVerifiedProtocolUnsupported
        );
        if !still_live {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(CliError::InvalidArguments(format!(
                "daemon {} did not exit within 5 seconds",
                expected.instance_id
            )));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn terminate_verified_daemon(
    expected: &bcode_daemon_lifecycle::DaemonRecord,
) -> Result<(), CliError> {
    let classification = bcode_daemon_lifecycle::classify_daemon_record(expected).await;
    if !matches!(
        classification,
        bcode_daemon_lifecycle::DaemonRecordClassification::CurrentHealthy
            | bcode_daemon_lifecycle::DaemonRecordClassification::HistoricalExactResponsive
            | bcode_daemon_lifecycle::DaemonRecordClassification::HistoricalProcessVerifiedProtocolUnsupported
    ) {
        return Err(CliError::InvalidArguments(format!(
            "refusing termination because daemon identity is {classification:?}"
        )));
    }
    let pid = expected.pid.ok_or_else(|| {
        CliError::InvalidArguments("verified daemon record has no process id".to_owned())
    })?;
    #[cfg(unix)]
    {
        let status = tokio::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .await?;
        if !status.success() {
            return Err(CliError::InvalidArguments(format!(
                "failed to terminate verified daemon process {pid}"
            )));
        }
        if wait_for_daemon_exit(expected).await.is_ok() {
            return Ok(());
        }
        let still_verified = matches!(
            bcode_daemon_lifecycle::classify_daemon_record(expected).await,
            bcode_daemon_lifecycle::DaemonRecordClassification::CurrentHealthy
                | bcode_daemon_lifecycle::DaemonRecordClassification::HistoricalExactResponsive
                | bcode_daemon_lifecycle::DaemonRecordClassification::HistoricalProcessVerifiedProtocolUnsupported
        );
        if !still_verified {
            return Err(CliError::InvalidArguments(
                "refusing SIGKILL because daemon identity changed after SIGTERM".to_owned(),
            ));
        }
        let status = tokio::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .await?;
        if !status.success() {
            return Err(CliError::InvalidArguments(format!(
                "failed to force-kill verified daemon process {pid}"
            )));
        }
        return wait_for_daemon_exit(expected).await;
    }
    #[cfg(windows)]
    let status = tokio::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status()
        .await?;
    #[cfg(not(any(unix, windows)))]
    return Err(CliError::InvalidArguments(
        "forced daemon termination is unsupported on this platform".to_owned(),
    ));
    #[cfg(windows)]
    if !status.success() {
        return Err(CliError::InvalidArguments(format!(
            "failed to terminate verified daemon process {pid}"
        )));
    }
    #[cfg(windows)]
    wait_for_daemon_exit(expected).await
}

async fn server_stop(force: bool) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint()
        .with_daemon_availability(DaemonAvailability::RequireRunning);
    if force {
        let status = match client.server_status().await {
            Ok(status) => status,
            Err(error) if server_is_unreachable(&error) => {
                println!("server not running");
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let record = bcode_daemon_lifecycle::read_records(&bcode_config::default_state_dir())
            .into_iter()
            .map(|(_, record)| record)
            .find(|record| daemon_status_matches(record, &status.daemon))
            .ok_or_else(|| {
                CliError::InvalidArguments(
                    "refusing forced stop because the responding daemon does not match its registry identity"
                        .to_owned(),
                )
            })?;
        terminate_verified_daemon(&record).await?;
        println!("server stopped");
        return Ok(());
    }
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

async fn run_new_session_tui(
    worktree: Option<String>,
    launch_options: bcode_tui::TuiLaunchOptions,
) -> Result<(), CliError> {
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
    bcode_tui::run_with_static_bundled_and_options(
        Some(session.id),
        &static_bundled_plugins(),
        build_info().clone(),
        launch_options,
    )
    .await?;
    Ok(())
}

async fn session_owner_client(session_id: SessionId) -> Result<BcodeClient, CliError> {
    let record = session_owner_record(session_id).await?;
    let endpoint = record.endpoint.to_ipc_endpoint().ok_or_else(|| {
        CliError::InvalidArguments(format!(
            "daemon {} has no supported IPC endpoint",
            record.instance_id
        ))
    })?;
    let client =
        BcodeClient::new(endpoint).with_daemon_availability(DaemonAvailability::RequireRunning);
    let status = client.server_status().await?;
    if !daemon_status_matches(&record, &status.daemon) {
        return Err(CliError::InvalidArguments(format!(
            "refusing session read because daemon identity changed for session {session_id}"
        )));
    }
    Ok(client)
}

async fn session_read_client(session_id: SessionId) -> Result<BcodeClient, CliError> {
    match session_owner_client(session_id).await {
        Ok(client) => Ok(client),
        Err(CliError::InvalidArguments(message))
            if message.contains("no verified live Bcode daemon owner was found") =>
        {
            Ok(BcodeClient::default_endpoint())
        }
        Err(error) => Err(error),
    }
}

async fn create_session(
    name: Option<String>,
    cwd: Option<PathBuf>,
    json: bool,
) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let session = match cwd {
        Some(directory) => {
            client
                .create_session_in_working_directory(name, std::path::absolute(directory)?)
                .await?
        }
        None => client.create_session(name).await?,
    };
    write_created_session(&mut std::io::stdout().lock(), &session, json)
}

fn write_created_session<W: std::io::Write>(
    output: &mut W,
    session: &bcode_session_models::SessionSummary,
    json: bool,
) -> Result<(), CliError> {
    if json {
        return write_json_stream_record(output, session);
    }
    writeln!(output, "{}", session.id)?;
    output.flush()?;
    Ok(())
}

async fn list_sessions(
    cwd: Option<PathBuf>,
    with_status: bool,
    json: bool,
) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let catalog = match cwd {
        Some(directory) => {
            client
                .list_sessions_in_working_directory(std::path::absolute(directory)?)
                .await?
        }
        None => client.list_sessions_with_status().await?,
    };
    let mut output = std::io::stdout().lock();
    if with_status {
        write_session_catalog(&mut output, &catalog)
    } else {
        write_session_list(&mut output, &catalog.sessions, json)
    }
}

fn write_catalog_refresh<W: std::io::Write>(
    output: &mut W,
    catalog: &bcode_client::SessionList,
    json: bool,
) -> Result<(), CliError> {
    if json {
        return write_session_catalog(output, catalog);
    }
    writeln!(
        output,
        "catalog revision {}: {}",
        catalog.catalog_revision,
        serde_json::to_string(&catalog.catalog_status)?
    )?;
    for source in &catalog.catalog_sources {
        writeln!(
            output,
            "{}\t{}\t{}",
            source.source_id,
            source.display_name,
            serde_json::to_string(&source.status)?
        )?;
    }
    output.flush()?;
    Ok(())
}

fn write_session_catalog<W: std::io::Write>(
    output: &mut W,
    catalog: &bcode_client::SessionList,
) -> Result<(), CliError> {
    #[derive(Serialize)]
    struct CatalogOutput<'a> {
        schema_version: u32,
        sessions: &'a [bcode_session_models::SessionSummary],
        catalog_status: &'a bcode_session_models::SessionCatalogStatus,
        catalog_sources: &'a [bcode_session_models::SessionCatalogSourceStatus],
        catalog_revision: u64,
    }
    write_json_result(
        output,
        &CatalogOutput {
            schema_version: 1,
            sessions: &catalog.sessions,
            catalog_status: &catalog.catalog_status,
            catalog_sources: &catalog.catalog_sources,
            catalog_revision: catalog.catalog_revision,
        },
    )
}

fn write_session_list<W: std::io::Write>(
    output: &mut W,
    sessions: &[bcode_session_models::SessionSummary],
    json: bool,
) -> Result<(), CliError> {
    if json {
        return write_json_result(output, &sessions);
    }
    if sessions.is_empty() {
        writeln!(output, "no sessions")?;
    }
    for session in sessions {
        writeln!(
            output,
            "{}\t{}\t{} clients",
            session.display_title(),
            session.id,
            session.client_count
        )?;
    }
    output.flush()?;
    Ok(())
}

async fn rename_session(session_id: SessionId, name: String, json: bool) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let session = client.rename_session(session_id, Some(name)).await?;
    if json {
        print_json_line(&session)?;
    } else {
        println!("renamed {} to {}", session.id, session.display_title());
    }
    Ok(())
}

async fn delete_session(session_id: SessionId, yes: bool, json: bool) -> Result<(), CliError> {
    if !yes {
        return Err(CliError::InvalidArguments(
            "session deletion requires --yes".to_owned(),
        ));
    }
    let client = BcodeClient::default_endpoint();
    let session = client.delete_session(session_id).await?;
    if json {
        print_json_line(&session)?;
    } else {
        println!("deleted {} ({})", session.display_title(), session.id);
    }
    Ok(())
}

async fn set_session_working_directory(
    session_id: SessionId,
    path: PathBuf,
    json: bool,
) -> Result<(), CliError> {
    let session = BcodeClient::default_endpoint()
        .change_session_working_directory(session_id, path)
        .await?;
    print_session_operation_result("working_directory_changed", &session, json)
}

async fn set_session_agent(
    session_id: SessionId,
    agent_id: String,
    json: bool,
) -> Result<(), CliError> {
    BcodeClient::default_endpoint()
        .set_session_agent(session_id, agent_id)
        .await?;
    print_unit_session_result("agent_set", session_id, json)
}

async fn set_session_model_selection(
    session_id: SessionId,
    provider: Option<String>,
    model_id: String,
    json: bool,
) -> Result<(), CliError> {
    BcodeClient::default_endpoint()
        .set_session_model(session_id, provider, model_id)
        .await?;
    print_unit_session_result("model_set", session_id, json)
}

async fn set_session_reasoning(
    session_id: SessionId,
    effort: Option<String>,
    summary: Option<String>,
    json: bool,
) -> Result<(), CliError> {
    BcodeClient::default_endpoint()
        .set_session_reasoning(session_id, effort, summary)
        .await?;
    print_unit_session_result("reasoning_set", session_id, json)
}

async fn set_auth_pool_preference(
    pool: String,
    profile: Option<String>,
    clear: bool,
    json: bool,
) -> Result<(), CliError> {
    if clear == profile.is_some() {
        return Err(CliError::InvalidArguments(
            "set-auth-pool requires exactly one of --profile or --clear".to_owned(),
        ));
    }
    BcodeClient::default_endpoint()
        .set_auth_pool_preference(pool.clone(), profile.clone())
        .await?;
    if json {
        print_json(&serde_json::json!({
            "status": "auth_pool_preference_set",
            "pool": pool,
            "profile": profile,
        }))
    } else {
        println!("auth pool preference set");
        Ok(())
    }
}

async fn list_active_skills(session_id: SessionId, json: bool) -> Result<(), CliError> {
    let skills = BcodeClient::default_endpoint()
        .active_skills(session_id)
        .await?;
    if json {
        print_json(&skills)
    } else {
        for skill in skills {
            println!("{}", skill.skill_id);
        }
        Ok(())
    }
}

async fn invoke_session_skill(
    session_id: SessionId,
    skill_id: String,
    arguments: String,
    json: bool,
) -> Result<(), CliError> {
    let display_text = if arguments.trim().is_empty() {
        format!("/{skill_id}")
    } else {
        format!("/{skill_id} {arguments}")
    };
    let acceptance = BcodeClient::default_endpoint()
        .invoke_skill(
            session_id,
            bcode_skill_models::SkillId::new(skill_id),
            arguments,
            display_text,
        )
        .await?;
    if json {
        print_json(&serde_json::json!({
            "session_id": session_id,
            "queued": acceptance.queued,
            "queue_position": acceptance.queue_position,
            "disposition": acceptance.disposition,
        }))
    } else {
        println!("{:?}", acceptance.disposition);
        Ok(())
    }
}

async fn set_session_skill(
    session_id: SessionId,
    skill_id: String,
    activate: bool,
    json: bool,
) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let skill_id = bcode_skill_models::SkillId::new(skill_id);
    if activate {
        client.activate_skill(session_id, skill_id).await?;
    } else {
        client.deactivate_skill(session_id, skill_id).await?;
    }
    print_unit_session_result(
        if activate {
            "skill_activated"
        } else {
            "skill_deactivated"
        },
        session_id,
        json,
    )
}

async fn compact_session(session_id: SessionId, json: bool) -> Result<(), CliError> {
    let message = BcodeClient::default_endpoint()
        .compact_session(session_id)
        .await?;
    if json {
        print_json(&serde_json::json!({
            "status": "context_compacted",
            "session_id": session_id,
            "message": message,
        }))
    } else {
        println!("{message}");
        Ok(())
    }
}

fn print_unit_session_result(
    status: &str,
    session_id: SessionId,
    json: bool,
) -> Result<(), CliError> {
    if json {
        print_json(&serde_json::json!({ "status": status, "session_id": session_id }))
    } else {
        println!("{status}: {session_id}");
        Ok(())
    }
}

fn print_session_operation_result(
    status: &str,
    session: &bcode_session_models::SessionSummary,
    json: bool,
) -> Result<(), CliError> {
    if json {
        print_json(&serde_json::json!({ "status": status, "session": session }))
    } else {
        println!("{status}: {}", session.id);
        Ok(())
    }
}

#[derive(Debug, Default)]
struct PagedSessionHistory {
    events: Vec<SessionEvent>,
    compatibility_issues: Vec<SessionEventCompatibilityIssue>,
}

async fn session_history(
    session_id: SessionId,
    after: Option<u64>,
    before: Option<u64>,
    limit: usize,
    json: bool,
) -> Result<(), CliError> {
    if after.is_some() && before.is_some() {
        return Err(CliError::InvalidSessionHistoryRange);
    }
    let direction = if before.is_some() {
        SessionHistoryDirection::Backward
    } else {
        SessionHistoryDirection::Forward
    };
    let cursor = before
        .or(after)
        .map(|sequence| SessionHistoryCursor { sequence });
    let owner_client = session_read_client(session_id).await?;
    let page = owner_client
        .session_history_page(
            session_id,
            SessionHistoryQuery {
                cursor,
                limit,
                direction,
            },
        )
        .await?;
    if json {
        print_json(&page)?;
        return Ok(());
    }
    for issue in &page.compatibility_issues {
        eprintln!("{}", format_session_compatibility_issue(issue));
    }
    for event in page.events {
        print_session_event(&event);
    }
    if page.has_more {
        eprintln!(
            "more history is available; next cursor: {}",
            page.next_cursor.map_or_else(
                || "unavailable".to_string(),
                |cursor| cursor.sequence.to_string()
            )
        );
    }
    Ok(())
}

async fn session_around(
    session_id: SessionId,
    sequence: u64,
    before: usize,
    after: usize,
    json: bool,
) -> Result<(), CliError> {
    let window = session_owner_client(session_id)
        .await?
        .session_history_around(
            session_id,
            SessionHistoryAroundQuery {
                sequence,
                before,
                after,
            },
        )
        .await?;
    if json {
        print_json(&window)?;
        return Ok(());
    }
    for issue in &window.compatibility_issues {
        eprintln!("{}", format_session_compatibility_issue(issue));
    }
    if !window.anchor_present {
        eprintln!("canonical event #{sequence} is not present");
    }
    for event in window.events {
        print_session_event(&event);
    }
    Ok(())
}

async fn session_inspect(
    session_id: SessionId,
    category: SessionInspectionCategoryArg,
    after: Option<u64>,
    before: Option<u64>,
    limit: usize,
    json: bool,
) -> Result<(), CliError> {
    if after.is_some() && before.is_some() {
        return Err(CliError::InvalidSessionHistoryRange);
    }
    let direction = if before.is_some() {
        SessionHistoryDirection::Backward
    } else {
        SessionHistoryDirection::Forward
    };
    let cursor = before
        .or(after)
        .map(|sequence| SessionHistoryCursor { sequence });
    let page = session_owner_client(session_id)
        .await?
        .session_inspection(
            session_id,
            SessionInspectionQuery {
                category: category.into(),
                cursor,
                limit,
                direction,
            },
        )
        .await?;
    if json {
        print_json(&page)?;
        return Ok(());
    }
    for issue in &page.compatibility_issues {
        eprintln!("{}", format_session_compatibility_issue(issue));
    }
    for event in page.events {
        print_session_event(&event);
    }
    if page.has_more {
        eprintln!(
            "more matching history may be available; next cursor: {}",
            page.next_cursor.map_or_else(
                || "unavailable".to_string(),
                |cursor| cursor.sequence.to_string()
            )
        );
    }
    Ok(())
}

async fn handle_session_search_cli(command: SessionCommand) -> Result<(), CliError> {
    let SessionCommand::Search {
        query,
        match_mode,
        fields,
        content,
        limit,
        deadline_ms,
        hydrate,
        deep,
        sessions,
        working_directory,
        after_timestamp_ms,
        before_timestamp_ms,
        tools,
        tool_statuses,
        providers,
        models,
        agents,
        import_sources,
        json,
    } = command
    else {
        unreachable!("session search handler received a different command")
    };
    handle_session_search_command(SessionSearchCliCommand {
        query,
        match_mode,
        fields,
        content,
        limit,
        deadline_ms,
        hydrate,
        scope: SessionSearchCliScope {
            deep,
            sessions,
            working_directory,
            after_timestamp_ms,
            before_timestamp_ms,
            tools,
            tool_statuses,
            providers,
            models,
            agents,
            import_sources,
        },
        json,
    })
    .await
}

async fn handle_session_search_explain_cli(command: SessionCommand) -> Result<(), CliError> {
    let SessionCommand::SearchExplain {
        query,
        match_mode,
        fields,
        content,
        limit,
        deadline_ms,
        deep,
        sessions,
        working_directory,
        after_timestamp_ms,
        before_timestamp_ms,
        tools,
        tool_statuses,
        providers,
        models,
        agents,
        import_sources,
        json,
    } = command
    else {
        unreachable!("session search explain handler received a different command")
    };
    handle_session_search_explain_command(SessionSearchCliCommand {
        query,
        match_mode,
        fields,
        content,
        limit,
        deadline_ms,
        hydrate: false,
        scope: SessionSearchCliScope {
            deep,
            sessions,
            working_directory,
            after_timestamp_ms,
            before_timestamp_ms,
            tools,
            tool_statuses,
            providers,
            models,
            agents,
            import_sources,
        },
        json,
    })
    .await
}

#[derive(Debug)]
struct SessionSearchCliCommand {
    query: String,
    match_mode: SessionSearchMatchArg,
    fields: Vec<SessionSearchFieldArg>,
    content: Vec<SessionSearchContentArg>,
    limit: usize,
    deadline_ms: u64,
    hydrate: bool,
    scope: SessionSearchCliScope,
    json: bool,
}

async fn handle_session_search_command(command: SessionSearchCliCommand) -> Result<(), CliError> {
    session_search(command).await
}

async fn handle_session_search_explain_command(
    command: SessionSearchCliCommand,
) -> Result<(), CliError> {
    session_search_explain(command).await
}

#[derive(Debug, Default)]
struct SessionSearchCliScope {
    deep: bool,
    sessions: Vec<SessionId>,
    working_directory: Option<PathBuf>,
    after_timestamp_ms: Option<u64>,
    before_timestamp_ms: Option<u64>,
    tools: Vec<String>,
    tool_statuses: Vec<String>,
    providers: Vec<String>,
    models: Vec<String>,
    agents: Vec<String>,
    import_sources: Vec<String>,
}

impl SessionSearchCliScope {
    fn filters(
        self,
        content: Vec<SessionSearchContentArg>,
    ) -> bcode_session_search::SessionSearchFilters {
        bcode_session_search::SessionSearchFilters {
            session_ids: self.sessions.into_iter().collect(),
            working_directory: self.working_directory,
            after_timestamp_ms: self.after_timestamp_ms,
            before_timestamp_ms: self.before_timestamp_ms,
            content_kinds: content.into_iter().map(Into::into).collect(),
            tool_names: self.tools.into_iter().collect(),
            tool_statuses: self.tool_statuses.into_iter().collect(),
            providers: self.providers.into_iter().collect(),
            models: self.models.into_iter().collect(),
            agents: self.agents.into_iter().collect(),
            sources: self.import_sources.into_iter().collect(),
            ..bcode_session_search::SessionSearchFilters::default()
        }
    }

    fn policy(&self, deadline_ms: u64) -> bcode_session_search::SessionSearchPlanPolicy {
        bcode_session_search::SessionSearchPlanPolicy {
            execution_class: if self.deep {
                bcode_session_search::SessionSearchExecutionClass::Deep
            } else {
                bcode_session_search::SessionSearchExecutionClass::Ordinary
            },
            per_provider_deadline_ms: deadline_ms.clamp(1, 2_000),
            ..bcode_session_search::SessionSearchPlanPolicy::default()
        }
    }
}

const fn session_search_request(
    query: String,
    match_mode: bcode_session_search::TextMatchMode,
    fields: BTreeSet<bcode_session_search::SearchField>,
    filters: bcode_session_search::SessionSearchFilters,
    limit: usize,
    deadline_ms: u64,
) -> bcode_session_search::SessionSearchRequest {
    bcode_session_search::SessionSearchRequest {
        query: bcode_session_search::SessionSearchQuery::Text {
            text: query,
            mode: match_mode,
            fields,
        },
        filters,
        sort: bcode_session_search::SessionSearchSort::ProviderRelevance,
        limit,
        cursor: None,
        deadline_ms: Some(deadline_ms),
    }
}

fn session_search_cli_outcome(
    response: &bcode_session_search::FederatedSessionSearchResponse,
    hydrated_hits: &[bcode_session_search::HydratedSessionSearchHit],
) -> &'static str {
    use bcode_session_search::{SearchErrorCode, SearchHitHydrationOutcome};

    if hydrated_hits
        .iter()
        .any(|hit| hit.outcome == SearchHitHydrationOutcome::RepairRequired)
    {
        "canonical_repair_required"
    } else if hydrated_hits
        .iter()
        .any(|hit| hit.outcome == SearchHitHydrationOutcome::Incompatible)
    {
        "canonical_incompatible"
    } else if response
        .failures
        .iter()
        .any(|failure| failure.error.code == SearchErrorCode::DeadlineExceeded)
    {
        "provider_timeout"
    } else if response
        .failures
        .iter()
        .any(|failure| failure.error.code == SearchErrorCode::StaleIndex)
    {
        "stale_index"
    } else if response
        .failures
        .iter()
        .any(|failure| failure.error.code == SearchErrorCode::UnsupportedQuery)
    {
        "unsupported_query"
    } else if response.providers.is_empty() {
        "no_eligible_provider"
    } else if response.hits.is_empty() && response.query_complete && response.coverage_complete {
        "no_results"
    } else if !response.query_complete {
        "incomplete_query"
    } else if !response.coverage_complete {
        "incomplete_coverage"
    } else {
        "complete"
    }
}

fn session_search_json(
    execution_class: bcode_session_search::SessionSearchExecutionClass,
    response: &bcode_session_search::FederatedSessionSearchResponse,
    hydrated_hits: &[bcode_session_search::HydratedSessionSearchHit],
) -> serde_json::Value {
    let hits = response
        .hits
        .iter()
        .map(|hit| {
            let hydrated = hydrated_hits
                .iter()
                .find(|candidate| candidate.hit.locator == hit.locator);
            serde_json::json!({
                "session_id": hit.locator.session_id,
                "event_sequence": hit.locator.sequence,
                "record_id": hit.locator.record_id,
                "content_kind": hit.content_kind,
                "matched_field": hit.matched_field,
                "timestamp_ms": hydrated
                    .and_then(|candidate| candidate.event.as_deref())
                    .map(|event| event.timestamp_ms),
                "preview": hit.preview,
                "preview_truncated": hit.preview_truncated,
                "provider_id": hit.provider_id,
                "provider_rank": hit.provider_rank,
                "provider_score": hit.provider_score,
                "hydration_outcome": hydrated.map(|candidate| candidate.outcome),
                "hydration_message": hydrated.and_then(|candidate| candidate.message.as_deref()),
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "outcome": session_search_cli_outcome(response, hydrated_hits),
        "execution_class": execution_class,
        "query_complete": response.query_complete,
        "coverage_complete": response.coverage_complete,
        "hits": hits,
        "providers": response.providers,
        "failures": response.failures,
    })
}

async fn session_search(command: SessionSearchCliCommand) -> Result<(), CliError> {
    let policy = command.scope.policy(command.deadline_ms);
    let execution_class = policy.execution_class;
    let request = session_search_request(
        command.query,
        command.match_mode.into(),
        command.fields.into_iter().map(Into::into).collect(),
        command.scope.filters(command.content),
        command.limit,
        command.deadline_ms,
    );
    let (response, hydrated_hits) = BcodeClient::default_endpoint()
        .session_search(request, policy, Vec::new(), command.hydrate)
        .await?;
    if command.json {
        return print_json(&session_search_json(
            execution_class,
            &response,
            &hydrated_hits,
        ));
    }
    for hit in &response.hits {
        println!(
            "{} #{} {:?} [{} rank {}]: {}{}",
            hit.locator.session_id,
            hit.locator.sequence,
            hit.content_kind,
            hit.provider_id,
            hit.provider_rank,
            hit.preview.as_deref().unwrap_or("<no provider preview>"),
            if hit.preview_truncated {
                " [truncated]"
            } else {
                ""
            }
        );
    }
    for provider in &response.providers {
        eprintln!(
            "provider {}: {:?}, query_complete={}, coverage_complete={}, elapsed={}ms",
            provider.provider_id,
            provider.outcome,
            provider.query_complete,
            provider.coverage_complete,
            provider.elapsed_ms
        );
    }
    for failure in &response.failures {
        eprintln!(
            "provider {} {:?} ({:?}, elapsed={}ms, content={:?}): {}",
            failure.plugin_id,
            failure.stage,
            failure.error.code,
            failure.elapsed_ms,
            failure.content,
            failure.error.message
        );
    }
    for hydrated in &hydrated_hits {
        if hydrated.outcome != bcode_session_search::SearchHitHydrationOutcome::Hydrated {
            eprintln!(
                "hydration {} #{}: {:?}{}",
                hydrated.hit.locator.session_id,
                hydrated.hit.locator.sequence,
                hydrated.outcome,
                hydrated
                    .message
                    .as_deref()
                    .map_or_else(String::new, |message| format!(" ({message})"))
            );
        }
    }
    let outcome = session_search_cli_outcome(&response, &hydrated_hits);
    if outcome != "complete" {
        eprintln!("search outcome: {outcome}");
    }
    if !response.query_complete || !response.coverage_complete {
        eprintln!("search result is partial; inspect provider status/failures above");
    }
    Ok(())
}

async fn handle_session_migration_subcommand(command: SessionCommand) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let (status, json, foreground) = match command {
        SessionCommand::MigrateInventory {
            sessions,
            after_timestamp_ms,
            before_timestamp_ms,
            json,
        } => (
            client
                .start_session_bulk_migration(bcode_ipc::SessionBulkMigrationStartRequest {
                    mode: bcode_ipc::SessionBulkMigrationMode::Inventory,
                    session_ids: sessions.into_iter().collect(),
                    after_timestamp_ms,
                    before_timestamp_ms,
                    confirmation: None,
                })
                .await?,
            json,
            true,
        ),
        SessionCommand::MigrateStart {
            sessions,
            after_timestamp_ms,
            before_timestamp_ms,
            confirm,
            foreground,
            json,
        } => (
            client
                .start_session_bulk_migration(bcode_ipc::SessionBulkMigrationStartRequest {
                    mode: bcode_ipc::SessionBulkMigrationMode::Migrate,
                    session_ids: sessions.into_iter().collect(),
                    after_timestamp_ms,
                    before_timestamp_ms,
                    confirmation: Some(confirm),
                })
                .await?,
            json,
            foreground,
        ),
        SessionCommand::MigrateStatus { operation_id, json } => (
            client.session_bulk_migration_status(operation_id).await?,
            json,
            false,
        ),
        SessionCommand::MigrateWait {
            operation_id,
            after_revision,
            timeout_ms,
            json,
        } => (
            client
                .wait_session_bulk_migration(operation_id, after_revision, timeout_ms)
                .await?,
            json,
            false,
        ),
        SessionCommand::MigrateCancel { operation_id, json } => (
            client.cancel_session_bulk_migration(operation_id).await?,
            json,
            false,
        ),
        _ => unreachable!("session migration handler received another command"),
    };
    let status = if foreground {
        wait_for_session_bulk_migration(&client, status).await?
    } else {
        status
    };
    print_session_bulk_migration_status(&status, json)
}

async fn wait_for_session_bulk_migration(
    client: &BcodeClient,
    mut status: bcode_ipc::SessionBulkMigrationOperationStatus,
) -> Result<bcode_ipc::SessionBulkMigrationOperationStatus, CliError> {
    while matches!(
        status.state,
        bcode_ipc::SessionBulkMigrationState::Running
            | bcode_ipc::SessionBulkMigrationState::CancellationRequested
    ) {
        status = client
            .wait_session_bulk_migration(status.operation_id.clone(), status.revision, 30_000)
            .await?;
    }
    Ok(status)
}

fn print_session_bulk_migration_status(
    status: &bcode_ipc::SessionBulkMigrationOperationStatus,
    json: bool,
) -> Result<(), CliError> {
    if json {
        return print_json(status);
    }
    println!(
        "{}: {:?} mode={:?} revision={} selected={} visited={} migrated={} blocked={} failed={}",
        status.operation_id,
        status.state,
        status.mode,
        status.revision,
        status.selected,
        status.visited,
        status.migrated,
        status.blocked,
        status.failed
    );
    for outcome in &status.outcomes {
        println!(
            "  {}: {:?}, action={:?}{}",
            outcome.session_id,
            outcome.category,
            outcome.action,
            outcome
                .message
                .as_deref()
                .map_or_else(String::new, |message| format!(": {message}"))
        );
    }
    Ok(())
}

async fn session_search_status(json: bool) -> Result<(), CliError> {
    let response = BcodeClient::default_endpoint()
        .session_search_providers()
        .await?;
    if json {
        return print_json(&response);
    }
    for provider in response.providers {
        println!(
            "{}: {:?}, execution={:?}, content={:?}, features={:?}, index={}/{}, documents={}, pending={}, schema={}/{}/{}",
            provider.plugin_id,
            provider.status.state,
            provider.capabilities.execution,
            provider.capabilities.content_kinds,
            provider.capabilities.features,
            provider.status.index_bytes,
            provider.status.quota_bytes,
            provider.status.document_count,
            provider.status.pending_sessions,
            provider.status.record_schema_version,
            provider.status.normalization_version,
            provider.status.policy_version
        );
        for coverage in provider.status.coverage {
            println!(
                "  session {} generation={} tail={:?} through={:?}, content={:?}, text_bytes={}, complete={}, skipped={}, truncated={}, exclusions={:?}",
                coverage.generation.session_id,
                coverage.generation.fingerprint,
                coverage.generation.last_sequence,
                coverage.indexed_through_sequence,
                coverage.content_kinds,
                coverage.indexed_text_bytes,
                coverage.complete,
                coverage.skipped_records,
                coverage.truncated_records,
                coverage.exclusions
            );
        }
        if let Some(reason) = provider.status.degraded_reason {
            println!("  degraded: {reason}");
        }
    }
    for failure in response.failures {
        eprintln!(
            "provider {} {:?} ({:?}, elapsed={}ms): {}",
            failure.plugin_id,
            failure.stage,
            failure.error.code,
            failure.elapsed_ms,
            failure.error.message
        );
    }
    Ok(())
}

async fn session_search_maintenance(
    provider: String,
    confirmation: String,
    json: bool,
    rebuild: bool,
) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let response = if rebuild {
        client
            .session_search_rebuild(provider, confirmation)
            .await?
    } else {
        client.session_search_purge(provider, confirmation).await?
    };
    if json {
        print_json(&response)?;
    } else {
        println!(
            "{} {} complete: state={:?}, index={}/{}, documents={}",
            response.provider_id,
            response.operation,
            response.status.state,
            response.status.index_bytes,
            response.status.quota_bytes,
            response.status.document_count
        );
    }
    Ok(())
}

async fn handle_session_search_subcommand(command: SessionCommand) -> Result<(), CliError> {
    match command {
        command @ SessionCommand::Search { .. } => handle_session_search_cli(command).await,
        SessionCommand::SearchStatus { json } => session_search_status(json).await,
        SessionCommand::SearchPurge {
            provider,
            confirm,
            json,
        } => session_search_maintenance(provider, confirm, json, false).await,
        SessionCommand::SearchRebuild {
            provider,
            confirm,
            json,
        } => session_search_maintenance(provider, confirm, json, true).await,
        command @ SessionCommand::SearchBackfillStart { .. } => {
            handle_session_search_backfill_start_cli(command).await
        }
        SessionCommand::SearchBackfillStatus { operation_id, json } => {
            session_search_backfill_operation(operation_id, json, false).await
        }
        SessionCommand::SearchBackfillWait {
            operation_id,
            after_revision,
            timeout_ms,
            json,
        } => {
            let status = BcodeClient::default_endpoint()
                .session_search_backfill_wait(operation_id, after_revision, timeout_ms)
                .await?;
            print_session_search_backfill_operation(&status, json)
        }
        SessionCommand::SearchBackfillCancel { operation_id, json } => {
            session_search_backfill_operation(operation_id, json, true).await
        }
        command @ SessionCommand::SearchBackfill { .. } => {
            handle_session_search_backfill_cli(command).await
        }
        command @ SessionCommand::SearchExplain { .. } => {
            handle_session_search_explain_cli(command).await
        }
        _ => unreachable!("session search handler received another command"),
    }
}

async fn handle_session_search_backfill_start_cli(command: SessionCommand) -> Result<(), CliError> {
    let SessionCommand::SearchBackfillStart {
        provider,
        sessions,
        after_timestamp_ms,
        before_timestamp_ms,
        cursor,
        deadline_ms,
        json,
    } = command
    else {
        unreachable!("session search backfill start handler received another command")
    };
    validate_complete_backfill_cursor(cursor.as_ref())?;
    let response = BcodeClient::default_endpoint()
        .session_search_complete_backfill_start(
            bcode_session_search::CompleteSessionSearchBackfillRequest {
                provider_id: provider,
                session_ids: sessions.into_iter().collect(),
                after_timestamp_ms,
                before_timestamp_ms,
                slice_deadline_ms: deadline_ms,
            },
        )
        .await?;
    if json {
        print_json(&response)?;
    } else {
        println!(
            "started {} for {}",
            response.operation_id, response.provider_id
        );
    }
    Ok(())
}

async fn session_search_backfill_operation(
    operation_id: String,
    json: bool,
    cancel: bool,
) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let status = if cancel {
        client.session_search_backfill_cancel(operation_id).await?
    } else {
        client.session_search_backfill_status(operation_id).await?
    };
    print_session_search_backfill_operation(&status, json)
}

fn print_session_search_backfill_operation_if_changed(
    status: &bcode_session_search::SessionSearchBackfillOperationStatus,
    json: bool,
    last_printed_revision: &mut Option<u64>,
) -> Result<(), CliError> {
    if last_printed_revision.is_some_and(|revision| revision == status.revision) {
        return Ok(());
    }
    print_session_search_backfill_operation(status, json)?;
    *last_printed_revision = Some(status.revision);
    Ok(())
}

fn print_session_search_backfill_operation(
    status: &bcode_session_search::SessionSearchBackfillOperationStatus,
    json: bool,
) -> Result<(), CliError> {
    if json {
        print_json_line(status)?;
    } else {
        println!(
            "{}: {:?} provider={} revision={}",
            status.operation_id, status.state, status.provider_id, status.revision
        );
        if let Some(progress) = &status.complete_progress {
            println!(
                "progress: pass={}, providers={}/{}, current={}, selected={}, visited={}, complete={}, incomplete={}, failed={}",
                progress.convergence_pass,
                progress.providers_completed,
                progress.provider_ids.len(),
                progress.current_provider_id.as_deref().unwrap_or("none"),
                progress.selected_sessions,
                progress.visited_sessions,
                progress.completed_sessions,
                progress.incomplete_sessions,
                progress.failed_sessions
            );
            for provider in &progress.providers {
                println!(
                    "  {}: selected={}, complete={}, incomplete={}, failed={}, pages={}{}",
                    provider.provider_id,
                    provider.selected_sessions,
                    provider.completed_sessions,
                    provider.incomplete_sessions,
                    provider.failed_sessions,
                    provider.catalog_pages,
                    provider
                        .error
                        .as_ref()
                        .map_or(String::new(), |error| format!(": {}", error.message))
                );
            }
        }
        if let Some(response) = &status.complete_response {
            println!(
                "providers={} revisions={}->{} passes={} cancelled={}",
                response.provider_ids.len(),
                response.catalog_revision_started,
                response.catalog_revision_completed,
                response.convergence_passes,
                response.cancelled
            );
            for provider in &response.providers {
                println!(
                    "  {}: selected={}, complete={}, incomplete={}, failed={}, pages={}{}",
                    provider.provider_id,
                    provider.selected_sessions,
                    provider.completed_sessions,
                    provider.incomplete_sessions,
                    provider.failed_sessions,
                    provider.catalog_pages,
                    provider
                        .error
                        .as_ref()
                        .map_or(String::new(), |error| format!(": {}", error.message))
                );
            }
        }
    }
    Ok(())
}

async fn handle_session_search_backfill_cli(command: SessionCommand) -> Result<(), CliError> {
    let SessionCommand::SearchBackfill {
        provider,
        sessions,
        after_timestamp_ms,
        before_timestamp_ms,
        cursor,
        deadline_ms,
        json,
    } = command
    else {
        unreachable!("session search backfill handler received another command")
    };
    session_search_backfill(
        provider,
        sessions,
        after_timestamp_ms,
        before_timestamp_ms,
        cursor,
        deadline_ms,
        json,
    )
    .await
}

fn parse_session_search_backfill_cursor(
    value: &str,
) -> Result<bcode_session_search::SessionSearchBackfillCursor, String> {
    let (updated_at_ms, session_id) = value
        .split_once(':')
        .ok_or_else(|| "backfill cursor must be UPDATED_AT_MS:SESSION_ID".to_owned())?;
    Ok(bcode_session_search::SessionSearchBackfillCursor {
        updated_at_ms: updated_at_ms
            .parse()
            .map_err(|_| "backfill cursor timestamp must be an unsigned integer".to_owned())?,
        session_id: session_id
            .parse()
            .map_err(|_| "backfill cursor session ID is invalid".to_owned())?,
    })
}

const fn complete_backfill_terminal_result(
    status: &bcode_session_search::SessionSearchBackfillOperationStatus,
) -> Result<(), CliError> {
    match status.state {
        bcode_session_search::SessionSearchBackfillOperationState::Completed => Ok(()),
        bcode_session_search::SessionSearchBackfillOperationState::Cancelled => {
            Err(CliError::SessionSearchBackfillCancelled)
        }
        _ => Err(CliError::SessionSearchBackfillIncomplete),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionBackfillWaitRecovery {
    ReturnWait,
    ReadStatus,
    ReinvokeAfterRestart,
}

fn classify_session_backfill_wait_recovery(
    result: &Result<bcode_session_search::SessionSearchBackfillOperationStatus, ClientError>,
    daemon_instance_id: &str,
    current_daemon_instance_id: Option<&str>,
) -> SessionBackfillWaitRecovery {
    if result.is_ok() {
        return SessionBackfillWaitRecovery::ReturnWait;
    }
    if !matches!(result, Err(ClientError::RequestTimeout { .. })) {
        return SessionBackfillWaitRecovery::ReturnWait;
    }
    if current_daemon_instance_id == Some(daemon_instance_id) {
        SessionBackfillWaitRecovery::ReadStatus
    } else {
        SessionBackfillWaitRecovery::ReinvokeAfterRestart
    }
}

async fn session_search_backfill_wait_recovering(
    client: &BcodeClient,
    operation_id: &str,
    after_revision: u64,
    timeout_ms: u64,
    daemon_instance_id: &str,
) -> Result<bcode_session_search::SessionSearchBackfillOperationStatus, ClientError> {
    let wait = client
        .session_search_backfill_wait(operation_id.to_owned(), after_revision, timeout_ms)
        .await;
    if !matches!(wait, Err(ClientError::RequestTimeout { .. })) {
        return wait;
    }
    let status = client.server_status().await?;
    match classify_session_backfill_wait_recovery(
        &wait,
        daemon_instance_id,
        Some(&status.daemon.instance_id),
    ) {
        SessionBackfillWaitRecovery::ReadStatus => {
            client
                .session_search_backfill_status(operation_id.to_owned())
                .await
        }
        SessionBackfillWaitRecovery::ReinvokeAfterRestart => Err(ClientError::Protocol(
            "session-search backfill operation state was lost with the prior daemon; explicitly re-invoke backfill to continue from provider checkpoints"
                .to_owned(),
        )),
        SessionBackfillWaitRecovery::ReturnWait => wait,
    }
}

fn validate_complete_backfill_cursor(
    cursor: Option<&bcode_session_search::SessionSearchBackfillCursor>,
) -> Result<(), CliError> {
    if cursor.is_some() {
        return Err(CliError::InvalidArguments(
            "--cursor is unsupported for complete backfill; use operation status/wait to observe an existing operation".to_owned(),
        ));
    }
    Ok(())
}

async fn session_search_backfill(
    provider: Option<String>,
    sessions: Vec<SessionId>,
    after_timestamp_ms: Option<u64>,
    before_timestamp_ms: Option<u64>,
    cursor: Option<bcode_session_search::SessionSearchBackfillCursor>,
    deadline_ms: u64,
    json: bool,
) -> Result<(), CliError> {
    validate_complete_backfill_cursor(cursor.as_ref())?;
    let client = BcodeClient::default_endpoint();
    let daemon_instance_id = client.server_status().await?.daemon.instance_id;
    let started = client
        .session_search_complete_backfill_start(
            bcode_session_search::CompleteSessionSearchBackfillRequest {
                provider_id: provider,
                session_ids: sessions.into_iter().collect(),
                after_timestamp_ms,
                before_timestamp_ms,
                slice_deadline_ms: deadline_ms,
            },
        )
        .await?;
    let mut revision = 0;
    let mut last_printed_revision = None;
    loop {
        let wait = session_search_backfill_wait_recovering(
            &client,
            &started.operation_id,
            revision,
            30_000,
            &daemon_instance_id,
        );
        let status = tokio::select! {
            result = wait => result?,
            signal = tokio::signal::ctrl_c() => {
                signal?;
                let status = client
                    .session_search_backfill_cancel(started.operation_id.clone())
                    .await?;
                print_session_search_backfill_operation_if_changed(
                    &status,
                    json,
                    &mut last_printed_revision,
                )?;
                return complete_backfill_terminal_result(&status);
            }
        };
        revision = status.revision;
        if matches!(
            status.state,
            bcode_session_search::SessionSearchBackfillOperationState::Completed
                | bcode_session_search::SessionSearchBackfillOperationState::NeedsAttention
                | bcode_session_search::SessionSearchBackfillOperationState::Cancelled
                | bcode_session_search::SessionSearchBackfillOperationState::Failed
        ) {
            print_session_search_backfill_operation_if_changed(
                &status,
                json,
                &mut last_printed_revision,
            )?;
            return complete_backfill_terminal_result(&status);
        }
        print_session_search_backfill_operation_if_changed(
            &status,
            json,
            &mut last_printed_revision,
        )?;
    }
}

async fn session_search_explain(command: SessionSearchCliCommand) -> Result<(), CliError> {
    let policy = command.scope.policy(command.deadline_ms);
    let plan = BcodeClient::default_endpoint()
        .session_search_explain(
            session_search_request(
                command.query,
                command.match_mode.into(),
                command.fields.into_iter().map(Into::into).collect(),
                command.scope.filters(command.content),
                command.limit,
                command.deadline_ms,
            ),
            policy,
            Vec::new(),
        )
        .await?;
    if command.json {
        return print_json(&plan);
    }
    for provider in plan.providers {
        println!(
            "selected {}: {:?}, content={:?}",
            provider.plugin_id, provider.status.state, provider.capabilities.content_kinds
        );
    }
    for failure in plan.failures {
        eprintln!(
            "excluded {} ({:?}): {}",
            failure.plugin_id, failure.error.code, failure.error.message
        );
    }
    Ok(())
}

async fn session_export(
    session_id: SessionId,
    format: SessionExportFormat,
) -> Result<(), CliError> {
    let client = session_read_client(session_id).await?;
    let mut cursor = Some(SessionHistoryCursor { sequence: 0 });
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
        if !page.compatibility_issues.is_empty() {
            return Err(CliError::InvalidArguments(format!(
                "session export encountered {} compatibility issue(s); retry with a compatible build",
                page.compatibility_issues.len()
            )));
        }
        match format {
            SessionExportFormat::Jsonl => {
                for event in page.events {
                    print_json_line(&event)?;
                }
            }
        }
        cursor = page.next_cursor;
        if !page.has_more {
            break;
        }
    }
    Ok(())
}

async fn session_timeline(session_id: SessionId) -> Result<(), CliError> {
    let history = paged_session_history(session_id).await?;
    for issue in &history.compatibility_issues {
        eprintln!("{}", format_session_compatibility_issue(issue));
    }
    let first_trace_time = history.events.iter().find_map(|event| match &event.kind {
        SessionEventKind::TraceEvent { trace } => Some(trace.timestamp_ms),
        _ => None,
    });
    for event in history.events {
        print_timeline_event(&event, first_trace_time);
    }
    Ok(())
}

async fn paged_session_history(session_id: SessionId) -> Result<PagedSessionHistory, CliError> {
    let client = session_read_client(session_id).await?;
    let mut cursor = Some(SessionHistoryCursor { sequence: 0 });
    let mut history = PagedSessionHistory::default();
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
        history.events.extend(page.events);
        history
            .compatibility_issues
            .extend(page.compatibility_issues);
        cursor = page.next_cursor;
        if !page.has_more {
            break;
        }
    }
    history
        .compatibility_issues
        .sort_by_key(|issue| issue.sequence);
    history
        .compatibility_issues
        .dedup_by_key(|issue| issue.sequence);
    Ok(history)
}

/// Verified evidence about which daemon holds a lock-blocked session database.
///
/// A database lock can outlive its lease record, in which case lease-based owner resolution reports
/// no owner even though the database is locked. This reports live-daemon evidence so the holder is
/// identifiable instead of the diagnosis dead-ending on a bare lock error.
#[derive(Debug, Clone, Serialize)]
struct SessionLockHolderCandidate {
    namespace: String,
    artifact_id: Option<String>,
    daemon_instance_id: String,
    pid: Option<u32>,
    build_fingerprint: String,
    storage_writer_epoch: Option<u32>,
    classification: String,
    /// Whether a lease record for this session also names this daemon.
    named_by_lease_record: bool,
}

/// Bounded, non-mutating diagnosis for a session whose database cannot be opened due to a lock.
#[derive(Debug, Clone, Serialize)]
struct SessionLockedDiagnosis {
    session_id: SessionId,
    database_path: PathBuf,
    lock_error: String,
    owner_observations: Vec<bcode_session::lease::SessionOwnerObservation>,
    lease_named_owners: usize,
    holder_candidates: Vec<SessionLockHolderCandidate>,
    recovery_guidance: String,
}

#[derive(Debug, Clone, Serialize)]
struct SessionDiagnosis {
    session_id: SessionId,
    database_path: PathBuf,
    writer_epoch: Option<u64>,
    expected_writer_epoch: u64,
    canonical_tail: Option<u64>,
    canonical_event_count: u64,
    event_schema_counts: BTreeMap<u16, u64>,
    event_kind_counts: BTreeMap<String, u64>,
    first_unknown_event_schema: Option<u16>,
    first_unknown_event_kind: Option<String>,
    strict_history_error: Option<String>,
    classification: String,
    migration_source_writer_epoch: Option<u64>,
    migration_target_writer_epoch: Option<u64>,
    migration_step_ids: Vec<String>,
    waiting_for_owner: bool,
    retained_backup: Option<bcode_session_migration::RetainedMigrationBackupDiagnosis>,
    write_readiness: String,
    model_context_status: String,
    projections: Vec<SessionProjectionDiagnosis>,
    owner_observations: Vec<bcode_session::lease::SessionOwnerObservation>,
    daemon_owner_classifications: Vec<SessionDaemonOwnerClassification>,
    active_owners: Vec<bcode_session::lease::SessionLeaseOwner>,
    recovery_guidance: Option<String>,
    event_count: usize,
    trace_event_count: usize,
    first_sequence: Option<u64>,
    last_sequence: Option<u64>,
    latest_events: Vec<SessionDiagnosisEvent>,
    latest_traces: Vec<SessionDiagnosisTrace>,
}

#[derive(Debug, Clone, Serialize)]
struct SessionDaemonOwnerClassification {
    daemon_instance_id: String,
    classification: bcode_daemon_lifecycle::DaemonRecordClassification,
}

#[derive(Debug, Clone, Serialize)]
struct SessionProjectionDiagnosis {
    projection: String,
    schema_version: Option<u64>,
    expected_schema_version: u32,
    checkpoint: Option<u64>,
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

#[allow(clippy::too_many_lines)]
async fn collect_session_diagnosis(
    session_id: SessionId,
    root: &Path,
) -> Result<SessionDiagnosis, CliError> {
    let database_path = bcode_session::db::session_db_path(root, session_id);
    let db = bcode_session::db::SessionDb::open_existing_turso_in_root(session_id, root).await?;
    let writer_epoch = db.storage_writer_epoch().await.ok();
    let compatibility = db.storage_compatibility().await;
    let migration_plan = compatibility.as_ref().ok().and_then(|compatibility| {
        let bcode_session::db::SessionStorageCompatibility::KnownLegacy { writer_epoch } =
            compatibility
        else {
            return None;
        };
        u32::try_from(*writer_epoch)
            .ok()
            .and_then(|epoch| bcode_session_migration::plan_writer_epoch_migration(epoch).ok())
    });
    let (canonical_event_count, inventory_tail, event_schema_counts, event_kind_counts) =
        db.canonical_event_inventory().await?;
    let first_unknown_event = db.first_unknown_event_envelope().await?;
    let (first_unknown_event_schema, first_unknown_event_kind) =
        first_unknown_event.map_or((None, None), |(schema, kind)| (Some(schema), Some(kind)));
    let canonical_tail = inventory_tail;
    let write_readiness = db
        .validate_write_readiness()
        .await
        .map_or_else(|error| error.to_string(), |()| "ready".to_string());
    let model_context_status = format!("{:?}", db.model_context_projection_status().await?);
    let mut projections = Vec::new();
    for projection in bcode_session::db::MaterializedProjection::all() {
        projections.push(SessionProjectionDiagnosis {
            projection: projection.as_str().to_string(),
            schema_version: db
                .materialized_projection_schema_version(*projection)
                .await?,
            expected_schema_version: projection.schema_version(),
            checkpoint: db.materialized_projection_checkpoint(*projection).await?,
        });
    }
    let strict_history = db.all_events_strict().await;
    let strict_history_error = strict_history.as_ref().err().map(ToString::to_string);
    let history = strict_history.unwrap_or_default();
    let owner_observations = bcode_session::lease::session_owner_observations(root, session_id)?;
    let active_owners = owner_observations
        .iter()
        .filter(|observation| {
            matches!(
                observation.liveness,
                bcode_session::lease::SessionOwnerLiveness::Live
                    | bcode_session::lease::SessionOwnerLiveness::Unverifiable
            )
        })
        .map(|observation| observation.owner.clone())
        .collect::<Vec<_>>();
    let classified_records =
        bcode_daemon_lifecycle::classified_records(&bcode_config::default_state_dir()).await;
    let daemon_owner_classifications = active_owners
        .iter()
        .filter_map(|owner| {
            let daemon_instance_id = owner.daemon_instance_id.as_ref()?;
            classified_records
                .iter()
                .find(|(_, record, _)| &record.instance_id == daemon_instance_id)
                .map(|(_, _, classification)| SessionDaemonOwnerClassification {
                    daemon_instance_id: daemon_instance_id.clone(),
                    classification: *classification,
                })
        })
        .collect::<Vec<_>>();
    let waiting_for_owner = migration_plan.is_some() && !active_owners.is_empty();
    let retained_backup =
        bcode_session_migration::latest_retained_migration_backup(root, session_id)?;
    let classification = session_diagnosis_classification(
        compatibility.as_ref(),
        &write_readiness,
        strict_history_error.as_deref(),
        waiting_for_owner,
    );
    let recovery_guidance =
        (classification != SessionDiagnosisClassification::CurrentReady).then(|| {
            match classification {
                SessionDiagnosisClassification::Migratable => format!(
                    "Open or attach this session with a current daemon to migrate it to writer epoch {}.",
                    bcode_session_migration::CURRENT_WRITER_EPOCH
                ),
                SessionDiagnosisClassification::BlockedOwner => {
                    "Wait for the owning daemon to release the session; migration will not run underneath a live owner."
                        .to_owned()
                }
                SessionDiagnosisClassification::UnsupportedFuture => {
                    "Use a Bcode build that supports this newer writer epoch.".to_owned()
                }
                SessionDiagnosisClassification::CurrentReady
                | SessionDiagnosisClassification::StructurallyCorrupt
                | SessionDiagnosisClassification::RepairRequired => format!(
                    "Run `bcode session doctor {session_id}` first; if canonical history is healthy and projections require rebuilding, run `bcode session reindex {session_id}`."
                ),
            }
        });
    Ok(SessionDiagnosis::from_history(
        session_id,
        &history,
        SessionStorageDiagnosis {
            database_path,
            writer_epoch,
            canonical_tail,
            canonical_event_count,
            event_schema_counts,
            event_kind_counts,
            first_unknown_event_schema,
            first_unknown_event_kind,
            strict_history_error,
            classification: classification.as_str().to_owned(),
            migration_source_writer_epoch: migration_plan
                .as_ref()
                .map(|plan| u64::from(plan.source_writer_epoch)),
            migration_target_writer_epoch: migration_plan
                .as_ref()
                .map(|plan| u64::from(plan.target_writer_epoch)),
            migration_step_ids: migration_plan.as_ref().map_or_else(Vec::new, |plan| {
                plan.steps.iter().map(|step| step.id.to_owned()).collect()
            }),
            waiting_for_owner,
            retained_backup,
            write_readiness,
            model_context_status,
            projections,
            owner_observations,
            daemon_owner_classifications,
            active_owners,
            recovery_guidance,
        },
    ))
}

async fn session_diagnose(session_id: SessionId, json: bool) -> Result<(), CliError> {
    let root = bcode_config::default_session_store_dir();
    match collect_session_diagnosis(session_id, &root).await {
        Ok(diagnosis) => {
            if json {
                print_json(&diagnosis)?;
            } else {
                print_session_diagnosis(&diagnosis);
            }
            Ok(())
        }
        // A lock-blocked database must still produce actionable diagnosis. Failing here is what
        // made an orphaned lock unrecoverable: the error named no holder and every ownership
        // command resolves owners from lease records that may already be gone.
        Err(CliError::SessionDb(error)) if error.is_lock_error() => {
            let diagnosis =
                collect_session_locked_diagnosis(session_id, &root, &error.to_string()).await?;
            if json {
                print_json(&diagnosis)?;
            } else {
                print_session_locked_diagnosis(&diagnosis);
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Collect bounded, non-mutating holder evidence for a lock-blocked session database.
///
/// This never opens the canonical database and never mutates lease or daemon state. It combines
/// lease observations with verified live daemon records so the holder can be identified even when
/// no lease record names it.
async fn collect_session_locked_diagnosis(
    session_id: SessionId,
    root: &Path,
    lock_error: &str,
) -> Result<SessionLockedDiagnosis, CliError> {
    let owner_observations = bcode_session::lease::session_owner_observations(root, session_id)?;
    let lease_named_instance_ids = owner_observations
        .iter()
        .filter_map(|observation| observation.owner.daemon_instance_id.clone())
        .collect::<BTreeSet<_>>();
    let classified_records =
        bcode_daemon_lifecycle::classified_records(&bcode_config::default_state_dir()).await;
    let holder_candidates = classified_records
        .iter()
        .filter(|(_, _, classification)| {
            // Only verified-identity live daemons are reportable. Ambiguous or stale evidence must
            // not be presented as a holder, so unverifiable ownership still fails closed.
            matches!(
                classification,
                bcode_daemon_lifecycle::DaemonRecordClassification::CurrentHealthy
                    | bcode_daemon_lifecycle::DaemonRecordClassification::HistoricalExactResponsive
                    | bcode_daemon_lifecycle::DaemonRecordClassification::HistoricalProcessVerifiedProtocolUnsupported
            )
        })
        .map(|(_, record, classification)| SessionLockHolderCandidate {
            namespace: record.namespace.clone(),
            artifact_id: record.artifact_id.as_ref().map(ToString::to_string),
            daemon_instance_id: record.instance_id.clone(),
            pid: record.pid,
            build_fingerprint: record.build_fingerprint.clone(),
            storage_writer_epoch: record.storage_writer_epoch,
            classification: format!("{classification:?}"),
            named_by_lease_record: lease_named_instance_ids.contains(&record.instance_id),
        })
        .collect::<Vec<_>>();
    Ok(SessionLockedDiagnosis {
        session_id,
        database_path: bcode_session::db::session_db_path(root, session_id),
        lock_error: lock_error.to_owned(),
        lease_named_owners: lease_named_instance_ids.len(),
        owner_observations,
        holder_candidates,
        recovery_guidance: session_locked_recovery_guidance(session_id),
    })
}

fn session_locked_recovery_guidance(session_id: SessionId) -> String {
    format!(
        "The canonical database is locked by another process. Ask each verified daemon below which sessions it holds with `bcode server status` using that daemon's own executable, then run `bcode session release-owner {session_id}` for non-destructive release. A daemon that is still serving other clients is not a stale owner: Bcode runs one daemon per distinct build by design, so never stop a daemon merely because it is older. Ownership normally clears once that daemon's last client for this session exits."
    )
}

fn print_session_locked_diagnosis(diagnosis: &SessionLockedDiagnosis) {
    println!("session: {}", diagnosis.session_id);
    println!(
        "database: {}",
        display_from_current_dir(&diagnosis.database_path)
    );
    println!("status: locked (canonical database not opened)");
    println!("lock error: {}", diagnosis.lock_error);
    println!("lease-named owners: {}", diagnosis.lease_named_owners);
    println!("owner observations: {}", diagnosis.owner_observations.len());
    for observation in &diagnosis.owner_observations {
        println!(
            "  lease_token={} pid={} liveness={:?} daemon_instance={:?}",
            observation.owner.lease_token,
            observation.owner.pid,
            observation.liveness,
            observation.owner.daemon_instance_id
        );
    }
    println!(
        "verified live daemon candidates: {}",
        diagnosis.holder_candidates.len()
    );
    for candidate in &diagnosis.holder_candidates {
        println!(
            "  namespace={} artifact={:?} instance={} pid={:?} build={} epoch={:?} classification={} named_by_lease={}",
            candidate.namespace,
            candidate.artifact_id,
            candidate.daemon_instance_id,
            candidate.pid,
            candidate.build_fingerprint,
            candidate.storage_writer_epoch,
            candidate.classification,
            candidate.named_by_lease_record
        );
    }
    println!("recovery: {}", diagnosis.recovery_guidance);
}

fn session_diagnosis_classification(
    compatibility: Result<
        &bcode_session::db::SessionStorageCompatibility,
        &bcode_session::db::SessionDbError,
    >,
    write_readiness: &str,
    strict_history_error: Option<&str>,
    waiting_for_owner: bool,
) -> SessionDiagnosisClassification {
    let compatibility = match compatibility {
        Ok(bcode_session::db::SessionStorageCompatibility::KnownLegacy { .. }) => {
            SessionDiagnosisCompatibility::ReleasedHistorical
        }
        Err(bcode_session::db::SessionDbError::WriterIncompatible {
            actual: Some(actual),
            expected,
        }) if actual > expected => SessionDiagnosisCompatibility::UnknownFuture,
        Err(_) => SessionDiagnosisCompatibility::StructurallyCorrupt,
        Ok(bcode_session::db::SessionStorageCompatibility::Current { .. }) => {
            SessionDiagnosisCompatibility::Current
        }
    };
    classify_session_diagnosis(
        compatibility,
        write_readiness == "ready",
        strict_history_error.is_some(),
        waiting_for_owner,
    )
}

struct SessionRepairCliOptions {
    target: SessionRepairCliTarget,
    mode: SessionRepairCliMode,
    output: SessionRepairCliOutput,
}

enum SessionRepairCliTarget {
    Explicit {
        session_id: Option<SessionId>,
        catalog: bool,
    },
    Scan,
}

enum SessionRepairCliMode {
    DryRun,
    Repair,
}

enum SessionRepairCliOutput {
    Text,
    Json,
}

const fn repair_cli_target(
    session_id: Option<SessionId>,
    catalog: bool,
    scan: bool,
) -> SessionRepairCliTarget {
    if scan {
        SessionRepairCliTarget::Scan
    } else {
        SessionRepairCliTarget::Explicit {
            session_id,
            catalog,
        }
    }
}

const fn repair_cli_mode(dry_run: bool) -> SessionRepairCliMode {
    if dry_run {
        SessionRepairCliMode::DryRun
    } else {
        SessionRepairCliMode::Repair
    }
}

const fn repair_cli_output(json: bool) -> SessionRepairCliOutput {
    if json {
        SessionRepairCliOutput::Json
    } else {
        SessionRepairCliOutput::Text
    }
}

async fn reindex_session_model_context(session_id: SessionId) -> Result<(), CliError> {
    let root = bcode_config::default_session_store_dir();
    let maintenance = bcode_session::lease::acquire_session_maintenance_guard(&root, session_id)?;
    let write = bcode_session::lease::acquire_maintenance_session_write_lock(
        &maintenance,
        &root,
        session_id,
    )?;
    let db = bcode_session::db::SessionDb::open_existing_turso_in_root(session_id, &root).await?;
    db.validate_write_readiness().await?;
    let event_count = db.reindex_session_projections(&maintenance, &write).await?;
    let canonical_tail = db.last_event_sequence().await?;
    let model_context = db.model_context_projection_status().await?;
    let verified = matches!(
        model_context,
        bcode_session::db::ModelContextProjectionStatus::Fresh { checkpoint }
            if Some(checkpoint) == canonical_tail
    );
    println!(
        "Reindexed session projections for session {session_id} from {event_count} canonical events"
    );
    println!(
        "verification: canonical_tail={canonical_tail:?} model_context={model_context:?} verified={verified}"
    );
    if !verified {
        return Err(CliError::PluginCli(
            "model-context reindex verification failed".to_string(),
        ));
    }
    Ok(())
}

async fn retired_catalogs(apply: bool, json: bool) -> Result<(), CliError> {
    let session_root = bcode_config::default_session_store_dir();
    let state_dir = session_root.parent().unwrap_or(&session_root);
    let reports =
        retired_catalogs::retired_catalog_reports(state_dir, &session_root, apply).await?;
    if json {
        print_json(&reports)?;
    } else if reports.is_empty() {
        println!("No retired build-scoped session catalogs found");
    } else {
        for report in reports {
            println!("namespace: {}", report.namespace);
            println!("path: {}", display_from_current_dir(&report.path));
            println!("classification: {:?}", report.classification);
            println!("daemon evidence: {:?}", report.daemon_evidence);
            println!("action: {:?}", report.action);
            println!(
                "bytes: db={} wal={} shm={} removed={}",
                report.database_bytes, report.wal_bytes, report.shm_bytes, report.removed_bytes
            );
            println!(
                "drafts: found={} migrated={} skipped_conflicts={}",
                report.draft_rows, report.migrated_drafts, report.skipped_draft_conflicts
            );
            if let Some(error) = report.error {
                println!("error: {error}");
            }
            println!();
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum SessionDoctorCategory {
    Ready,
    MigrationRequired,
    OwnerBlocked,
    TemporarilyLocked,
    RepairRequired,
    FormatIncompatible,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SessionDoctorAction {
    None,
    Retry,
    Migrate,
    Repair,
    Upgrade,
    Locate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct SessionDoctorCategorySummary {
    count: usize,
    action: SessionDoctorAction,
    retryable: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    samples: Vec<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct SessionDoctorFinding {
    session_id: SessionId,
    category: SessionDoctorCategory,
    action: SessionDoctorAction,
    retryable: bool,
}

#[derive(Debug, Serialize)]
struct SessionDoctorScanReport {
    historical_storage: bcode_session_migration::HistoricalStorageDiagnosis,
    category_counts: BTreeMap<SessionDoctorCategory, usize>,
    category_summaries: BTreeMap<SessionDoctorCategory, SessionDoctorCategorySummary>,
    findings: Vec<SessionDoctorFinding>,
    reports: Vec<bcode_session::repair::RepairReport>,
}

const MAX_SESSION_DOCTOR_SAMPLES_PER_CATEGORY: usize = 3;

#[allow(clippy::too_many_lines)]
async fn classify_session_doctor_report(
    root: &Path,
    report: &bcode_session::repair::RepairReport,
) -> (SessionDoctorCategory, SessionDoctorAction, bool) {
    use bcode_session::repair::RepairStatus;

    let bcode_session::repair::RepairTarget::Session { session_id } = report.target else {
        return match report.status {
            RepairStatus::Ok => (
                SessionDoctorCategory::Ready,
                SessionDoctorAction::None,
                false,
            ),
            RepairStatus::RefusedOwnedElsewhere => (
                SessionDoctorCategory::OwnerBlocked,
                SessionDoctorAction::Retry,
                true,
            ),
            RepairStatus::WouldRepair | RepairStatus::Repaired | RepairStatus::ManualRequired => (
                SessionDoctorCategory::RepairRequired,
                SessionDoctorAction::Repair,
                false,
            ),
        };
    };
    if !report.db_path.exists() {
        return (
            SessionDoctorCategory::Missing,
            SessionDoctorAction::Locate,
            false,
        );
    }
    if report.status == RepairStatus::RefusedOwnedElsewhere {
        return (
            SessionDoctorCategory::OwnerBlocked,
            SessionDoctorAction::Retry,
            true,
        );
    }
    match bcode_session::db::SessionDb::open_existing_turso_in_root(session_id, root).await {
        Ok(db) => {
            if let Ok((envelopes, truncated)) = db.event_envelope_inventory().await {
                if truncated
                    || envelopes.iter().any(|(schema, kind)| {
                        matches!(
                            bcode_session_migration::classify_event_kind_schema(kind, *schema),
                            bcode_session_migration::ReleasedEventKindClassification::Unknown
                        )
                    })
                {
                    return (
                        SessionDoctorCategory::FormatIncompatible,
                        SessionDoctorAction::Upgrade,
                        false,
                    );
                }
                if envelopes.iter().any(|(schema, kind)| {
                    matches!(
                        bcode_session_migration::classify_event_kind_schema(kind, *schema),
                        bcode_session_migration::ReleasedEventKindClassification::ReleasedHistorical
                    )
                }) {
                    return (
                        SessionDoctorCategory::MigrationRequired,
                        SessionDoctorAction::Migrate,
                        false,
                    );
                }
            }
            match report.status {
                RepairStatus::Ok => (
                    SessionDoctorCategory::Ready,
                    SessionDoctorAction::None,
                    false,
                ),
                RepairStatus::WouldRepair
                | RepairStatus::Repaired
                | RepairStatus::ManualRequired => (
                    SessionDoctorCategory::RepairRequired,
                    SessionDoctorAction::Repair,
                    false,
                ),
                RepairStatus::RefusedOwnedElsewhere => unreachable!("handled before database open"),
            }
        }
        Err(error) if error.is_lock_error() => (
            SessionDoctorCategory::TemporarilyLocked,
            SessionDoctorAction::Retry,
            true,
        ),
        Err(bcode_session::db::SessionDbError::WriterIncompatible { actual, expected }) => {
            if actual.is_some_and(|actual| actual < expected) {
                (
                    SessionDoctorCategory::MigrationRequired,
                    SessionDoctorAction::Migrate,
                    false,
                )
            } else {
                (
                    SessionDoctorCategory::FormatIncompatible,
                    SessionDoctorAction::Upgrade,
                    false,
                )
            }
        }
        Err(_) => (
            SessionDoctorCategory::RepairRequired,
            SessionDoctorAction::Repair,
            false,
        ),
    }
}

async fn summarize_session_doctor_reports(
    root: &Path,
    reports: &[bcode_session::repair::RepairReport],
) -> (
    BTreeMap<SessionDoctorCategory, usize>,
    BTreeMap<SessionDoctorCategory, SessionDoctorCategorySummary>,
    Vec<SessionDoctorFinding>,
) {
    let mut counts = BTreeMap::new();
    let mut summaries = BTreeMap::new();
    let mut findings = Vec::new();
    for report in reports {
        let (category, action, retryable) = classify_session_doctor_report(root, report).await;
        *counts.entry(category).or_insert(0) += 1;
        let summary = summaries
            .entry(category)
            .or_insert_with(|| SessionDoctorCategorySummary {
                count: 0,
                action,
                retryable,
                samples: Vec::new(),
            });
        summary.count += 1;
        if let bcode_session::repair::RepairTarget::Session { session_id } = report.target {
            findings.push(SessionDoctorFinding {
                session_id,
                category,
                action,
                retryable,
            });
            if summary.samples.len() < MAX_SESSION_DOCTOR_SAMPLES_PER_CATEGORY {
                summary.samples.push(session_id);
            }
        }
    }
    (counts, summaries, findings)
}

fn write_json_line<W: std::io::Write, T: Serialize>(
    output: &mut W,
    value: &T,
) -> Result<(), serde_json::Error> {
    serde_json::to_writer_pretty(&mut *output, value)?;
    output.write_all(b"\n").map_err(serde_json::Error::io)
}

fn write_json_stdout<T: Serialize>(value: &T) -> Result<(), CliError> {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    match write_json_line(&mut output, value) {
        Ok(()) => Ok(()),
        Err(error) if error.io_error_kind() == Some(std::io::ErrorKind::BrokenPipe) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn run_session_repair_command(options: SessionRepairCliOptions) -> Result<(), CliError> {
    let root = bcode_config::default_session_store_dir();
    let dry_run = matches!(options.mode, SessionRepairCliMode::DryRun);
    let json = matches!(options.output, SessionRepairCliOutput::Json);
    let scan = matches!(&options.target, SessionRepairCliTarget::Scan);
    let mut historical_storage = None;
    let mut reports = Vec::new();
    match options.target {
        SessionRepairCliTarget::Scan => {
            let historical = collect_historical_storage_diagnosis(root.parent().unwrap_or(&root))?;
            if !json {
                print_historical_storage_diagnosis(&historical);
            }
            historical_storage = Some(historical);
            reports.push(repair_catalog_report(&root, dry_run).await?);
            for session_id in discover_session_ids(&root)? {
                reports.push(repair_session_report(&root, session_id, dry_run).await?);
            }
        }
        SessionRepairCliTarget::Explicit {
            session_id,
            catalog,
        } => {
            if catalog {
                reports.push(repair_catalog_report(&root, dry_run).await?);
            }
            if let Some(session_id) = session_id {
                reports.push(repair_session_report(&root, session_id, dry_run).await?);
            }
        }
    }
    if reports.is_empty() {
        return Err(CliError::SessionRepairUsage(
            "provide a session id, --catalog, or --scan".to_string(),
        ));
    }
    let (category_counts, category_summaries, findings) =
        summarize_session_doctor_reports(&root, &reports).await;
    if json {
        if let Some(historical_storage) = historical_storage {
            write_json_stdout(&SessionDoctorScanReport {
                historical_storage,
                category_counts,
                category_summaries,
                findings,
                reports,
            })?;
        } else {
            write_json_stdout(&reports)?;
        }
    } else {
        if scan {
            println!("compatibility summary:");
            for (category, summary) in &category_summaries {
                println!(
                    "  {category:?}: count={}, action={:?}, retryable={}",
                    summary.count, summary.action, summary.retryable
                );
            }
            println!();
        }
        for report in &reports {
            print_repair_report(report);
        }
    }
    Ok(())
}

fn collect_historical_storage_diagnosis(
    state_dir: &Path,
) -> Result<bcode_session_migration::HistoricalStorageDiagnosis, CliError> {
    Ok(session_migration_adapter::diagnose_historical_session_storage(state_dir)?)
}

fn print_historical_storage_diagnosis(
    diagnosis: &bcode_session_migration::HistoricalStorageDiagnosis,
) {
    println!("target: historical storage");
    println!("path: {}", display_from_current_dir(&diagnosis.root));
    println!("status: {:?}", diagnosis.status);
    for note in &diagnosis.notes {
        println!("note: {note}");
    }
    println!();
}

async fn repair_session_report(
    root: &Path,
    session_id: SessionId,
    dry_run: bool,
) -> Result<bcode_session::repair::RepairReport, CliError> {
    if dry_run {
        Ok(bcode_session::repair::doctor_session(root, session_id).await?)
    } else {
        Ok(bcode_session::repair::repair_session(
            root,
            session_id,
            bcode_session::repair::RepairOptions { dry_run },
        )
        .await?)
    }
}

async fn repair_catalog_report(
    root: &Path,
    dry_run: bool,
) -> Result<bcode_session::repair::RepairReport, CliError> {
    if dry_run {
        Ok(bcode_session::repair::doctor_catalog(root).await?)
    } else {
        Ok(bcode_session::repair::repair_catalog(
            root,
            bcode_session::repair::RepairOptions { dry_run },
        )
        .await?)
    }
}

fn discover_session_ids(root: &Path) -> Result<Vec<SessionId>, CliError> {
    let mut ids = Vec::new();
    if !root.exists() {
        return Ok(ids);
    }
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(session_id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.parse::<SessionId>().ok())
        {
            ids.push(session_id);
        }
    }
    ids.sort();
    Ok(ids)
}

fn print_repair_report(report: &bcode_session::repair::RepairReport) {
    println!("target: {}", repair_target_label(&report.target));
    println!("status: {:?}", report.status);
    println!("db: {}", display_from_current_dir(&report.db_path));
    if let Some(backup_path) = &report.backup_path {
        println!("backup: {}", display_from_current_dir(backup_path));
    }
    if let Some(error) = &report.initial_error {
        println!("initial error: {error}");
    }
    if let Some(error) = &report.final_error {
        println!("final error: {error}");
    }
    for action in &report.actions {
        println!("action: {} — {}", action.kind, action.detail);
    }
    for note in &report.notes {
        println!("note: {note}");
    }
    println!();
}

fn repair_target_label(target: &bcode_session::repair::RepairTarget) -> String {
    match target {
        bcode_session::repair::RepairTarget::Session { session_id } => {
            format!("session {session_id}")
        }
        bcode_session::repair::RepairTarget::Catalog => "catalog".to_string(),
    }
}

struct SessionStorageDiagnosis {
    database_path: PathBuf,
    writer_epoch: Option<u64>,
    canonical_tail: Option<u64>,
    canonical_event_count: u64,
    event_schema_counts: BTreeMap<u16, u64>,
    event_kind_counts: BTreeMap<String, u64>,
    first_unknown_event_schema: Option<u16>,
    first_unknown_event_kind: Option<String>,
    strict_history_error: Option<String>,
    classification: String,
    migration_source_writer_epoch: Option<u64>,
    migration_target_writer_epoch: Option<u64>,
    migration_step_ids: Vec<String>,
    waiting_for_owner: bool,
    retained_backup: Option<bcode_session_migration::RetainedMigrationBackupDiagnosis>,
    write_readiness: String,
    model_context_status: String,
    projections: Vec<SessionProjectionDiagnosis>,
    owner_observations: Vec<bcode_session::lease::SessionOwnerObservation>,
    daemon_owner_classifications: Vec<SessionDaemonOwnerClassification>,
    active_owners: Vec<bcode_session::lease::SessionLeaseOwner>,
    recovery_guidance: Option<String>,
}

impl SessionDiagnosis {
    fn from_history(
        session_id: SessionId,
        history: &[SessionEvent],
        storage: SessionStorageDiagnosis,
    ) -> Self {
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
            database_path: storage.database_path,
            writer_epoch: storage.writer_epoch,
            expected_writer_epoch: u64::from(
                bcode_session::lease::CURRENT_SESSION_STORAGE_WRITER_EPOCH,
            ),
            canonical_tail: storage.canonical_tail,
            canonical_event_count: storage.canonical_event_count,
            event_schema_counts: storage.event_schema_counts,
            event_kind_counts: storage.event_kind_counts,
            first_unknown_event_schema: storage.first_unknown_event_schema,
            first_unknown_event_kind: storage.first_unknown_event_kind,
            strict_history_error: storage.strict_history_error,
            classification: storage.classification,
            migration_source_writer_epoch: storage.migration_source_writer_epoch,
            migration_target_writer_epoch: storage.migration_target_writer_epoch,
            migration_step_ids: storage.migration_step_ids,
            waiting_for_owner: storage.waiting_for_owner,
            retained_backup: storage.retained_backup,
            write_readiness: storage.write_readiness,
            model_context_status: storage.model_context_status,
            projections: storage.projections,
            owner_observations: storage.owner_observations,
            daemon_owner_classifications: storage.daemon_owner_classifications,
            active_owners: storage.active_owners,
            recovery_guidance: storage.recovery_guidance,
            event_count: history.len(),
            trace_event_count,
            first_sequence: history.first().map(|event| event.sequence),
            last_sequence: history.last().map(|event| event.sequence),
            latest_events,
            latest_traces,
        }
    }
}

#[allow(clippy::too_many_lines)]
fn print_session_diagnosis(diagnosis: &SessionDiagnosis) {
    println!("session: {}", diagnosis.session_id);
    println!(
        "database: {}",
        display_from_current_dir(&diagnosis.database_path)
    );
    println!(
        "writer epoch: {:?} (expected {})",
        diagnosis.writer_epoch, diagnosis.expected_writer_epoch
    );
    println!("classification: {}", diagnosis.classification);
    if let (Some(source), Some(target)) = (
        diagnosis.migration_source_writer_epoch,
        diagnosis.migration_target_writer_epoch,
    ) {
        println!("migration: writer epoch {source} -> {target}");
        println!("migration steps: {:?}", diagnosis.migration_step_ids);
    }
    println!("waiting for owner: {}", diagnosis.waiting_for_owner);
    if let Some(backup) = &diagnosis.retained_backup {
        println!(
            "retained backup: {} (operation={} source_epoch={} target_epoch={})",
            display_from_current_dir(&backup.path),
            backup.manifest.operation_id,
            backup.manifest.source_writer_epoch,
            backup.manifest.target_writer_epoch
        );
    }
    println!("canonical tail: {:?}", diagnosis.canonical_tail);
    println!("canonical events: {}", diagnosis.canonical_event_count);
    println!("event schemas: {:?}", diagnosis.event_schema_counts);
    println!("event kinds: {:?}", diagnosis.event_kind_counts);
    if let (Some(schema), Some(kind)) = (
        diagnosis.first_unknown_event_schema,
        diagnosis.first_unknown_event_kind.as_deref(),
    ) {
        println!("first unknown event: schema={schema} kind={kind}");
    }
    if let Some(error) = &diagnosis.strict_history_error {
        println!("strict history: unavailable ({error})");
    }
    println!("write readiness: {}", diagnosis.write_readiness);
    println!("model context: {}", diagnosis.model_context_status);
    println!("projections:");
    for projection in &diagnosis.projections {
        println!(
            "  {} schema={:?}/{} checkpoint={:?}",
            projection.projection,
            projection.schema_version,
            projection.expected_schema_version,
            projection.checkpoint
        );
    }
    println!("owner observations: {}", diagnosis.owner_observations.len());
    for observation in &diagnosis.owner_observations {
        let owner = &observation.owner;
        println!(
            "  liveness={:?} schema={} token={} pid={} instance={:?} acquired_at_ms={} namespace={:?} build={:?} writer_epoch={:?} endpoint={:?}",
            observation.liveness,
            owner.schema_version,
            owner.lease_token,
            owner.pid,
            owner.daemon_instance_id,
            owner.acquired_at_ms,
            owner.daemon_namespace,
            owner.build_fingerprint,
            owner.storage_writer_epoch,
            owner.endpoint
        );
    }
    println!("active owners: {}", diagnosis.active_owners.len());
    for owner in &diagnosis.active_owners {
        println!(
            "  pid={} namespace={:?} build={:?} writer_epoch={:?} endpoint={:?}",
            owner.pid,
            owner.daemon_namespace,
            owner.build_fingerprint,
            owner.storage_writer_epoch,
            owner.endpoint
        );
    }
    println!(
        "daemon owner classifications: {}",
        diagnosis.daemon_owner_classifications.len()
    );
    for owner in &diagnosis.daemon_owner_classifications {
        println!(
            "  instance={} classification={:?}",
            owner.daemon_instance_id, owner.classification
        );
    }
    if let Some(guidance) = &diagnosis.recovery_guidance {
        println!("recovery: {guidance}");
    }
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
        SessionEventKind::AssistantResponseSegment { .. } => "assistant_response_segment",
        SessionEventKind::PositionedAssistantResponseSegment { .. } => {
            "positioned_assistant_response_segment"
        }
        SessionEventKind::PositionedAssistantReasoningActivity { .. } => {
            "positioned_assistant_reasoning_activity"
        }
        SessionEventKind::PositionedToolCallRequested { .. } => "positioned_tool_call_requested",
        SessionEventKind::ToolCallRequested { .. } => "tool_call_requested",
        SessionEventKind::PermissionRequested { .. } => "permission_requested",
        SessionEventKind::PermissionResolved { .. } => "permission_resolved",
        SessionEventKind::ModelChanged { .. } => "model_changed",
        SessionEventKind::ReasoningChanged { .. } => "reasoning_changed",
        SessionEventKind::SystemMessage { .. } => "system_message",
        SessionEventKind::AgentChanged { .. } => "agent_changed",
        SessionEventKind::ModelTurnStarted { .. } => "model_turn_started",
        SessionEventKind::ModelFeatureFidelityNegotiated { .. } => {
            "model_feature_fidelity_negotiated"
        }
        SessionEventKind::ModelTurnFinished { .. } => "model_turn_finished",
        SessionEventKind::ModelUsage { .. } => "model_usage",
        SessionEventKind::ContextCompacted { .. } => "context_compacted",
        SessionEventKind::ProviderContextCompacted { .. } => "provider_context_compacted",
        SessionEventKind::RequestContextObserved { .. } => "request_context_observed",
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
        SessionEventKind::ToolInvocationLifecycle { .. } => "tool_invocation_lifecycle",
        SessionEventKind::ToolInvocationResultRecorded { .. } => "tool_invocation_result_recorded",
        SessionEventKind::ToolContribution { .. } => "tool_contribution",
        SessionEventKind::ToolContributionPlaced { .. } => "tool_contribution_placed",
        SessionEventKind::ToolExchangeRequested { .. } => "tool_exchange_requested",
        SessionEventKind::ToolExchangeResolved { .. } => "tool_exchange_resolved",
        SessionEventKind::WorkingDirectoryChanged { .. } => "working_directory_changed",
        SessionEventKind::SessionImported { .. } => "session_imported",
        SessionEventKind::SessionDerived { .. } => "session_derived",
        SessionEventKind::ExecutionSessionCreated { .. } => "execution_session_created",
        SessionEventKind::AssistantReasoningActivity { .. } => "assistant_reasoning_activity",
        SessionEventKind::RalphLifecycle { .. } => "ralph_lifecycle",
        SessionEventKind::PluginStatusNote { .. } => "plugin_status_note",
        SessionEventKind::InertHistory { .. } => "inert_history",
    }
}

async fn handle_session_import_command(command: SessionImportCommand) -> Result<(), CliError> {
    ensure_server_running().await?;
    let client = BcodeClient::default_endpoint();
    match command {
        SessionImportCommand::Sources { json } => {
            let response = client
                .call_plugin_service(
                    SESSION_IMPORT_INTERFACE_ID.to_string(),
                    OP_LIST_IMPORT_SOURCES.to_string(),
                    Vec::new(),
                )
                .await?;
            let sources: ListImportSourcesResponse = serde_json::from_slice(&response.payload)?;
            if json {
                print_json(&sources)?;
            } else {
                for source in sources.sources {
                    println!("{}\t{}", source.source_id, source.display_name);
                }
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
                print_json(&sessions)?;
            } else if sessions.sessions.is_empty() {
                println!("no importable sessions");
            } else {
                for session in sessions.sessions {
                    let title = session.title.as_deref().unwrap_or("<untitled>");
                    let cwd = session.working_directory.as_ref().map_or_else(
                        || "-".to_owned(),
                        |cwd| display_from_current_dir(cwd).to_string(),
                    );
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
                println!("imported [{source}] with {} warnings", warnings.len());
                for warning in warnings {
                    println!("{}: {}", warning.code, warning.message);
                }
            }
        }
    }
    Ok(())
}

async fn handle_runtime_work_command(command: RuntimeWorkCommand) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    match command {
        RuntimeWorkCommand::List { session_id, json } => {
            let work = client.list_runtime_work(session_id).await?;
            write_runtime_work_list(&mut std::io::stdout().lock(), &work, json)?;
        }
        RuntimeWorkCommand::Cancel {
            session_id,
            work_id,
            json,
        } => {
            let cancelled = client
                .cancel_runtime_work(session_id, bcode_session_models::WorkId::new(work_id))
                .await?;
            print_cancellation_result("runtime_work", cancelled, json)?;
        }
        RuntimeWorkCommand::History {
            session_id,
            limit,
            json,
        } => {
            let spans = client.runtime_work_spans(session_id, limit).await?;
            write_runtime_work_history(&mut std::io::stdout().lock(), &spans, json)?;
        }
        RuntimeWorkCommand::Watch { session_id, json } => {
            let mut watcher = client.watch_runtime_work(session_id).await?;
            loop {
                let event = watcher.next_event().await?;
                if json {
                    print_json_line(&serde_json::json!({
                        "type": "runtime_work_event",
                        "session_id": session_id,
                        "event": event,
                    }))?;
                } else {
                    print_session_event(&event);
                }
            }
        }
    }
    Ok(())
}

fn write_runtime_work_list(
    output: &mut impl std::io::Write,
    work: &[bcode_session_models::RuntimeWorkSnapshot],
    json: bool,
) -> Result<(), CliError> {
    if json {
        return write_json_result(output, &work);
    }
    for work in work {
        writeln!(
            output,
            "{} {:?} {:?} {} cancellable={}",
            work.work_id, work.kind, work.status, work.label, work.cancellable
        )?;
    }
    output.flush()?;
    Ok(())
}

fn write_runtime_work_history(
    output: &mut impl std::io::Write,
    spans: &[bcode_session_models::RuntimeWorkSpan],
    json: bool,
) -> Result<(), CliError> {
    if json {
        return write_json_result(output, &spans);
    }
    for span in spans {
        writeln!(
            output,
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
        )?;
    }
    output.flush()?;
    Ok(())
}

async fn cancel_session_turn(
    session_id: SessionId,
    clear_queue: bool,
    json: bool,
) -> Result<(), CliError> {
    let cancelled = BcodeClient::default_endpoint()
        .cancel_session_turn_with_options(session_id, clear_queue)
        .await?;
    print_cancellation_result("turn", cancelled, json)
}

fn print_cancellation_result(kind: &str, cancelled: bool, json: bool) -> Result<(), CliError> {
    write_cancellation_result(&mut std::io::stdout().lock(), kind, cancelled, json)
}

fn write_cancellation_result(
    output: &mut impl std::io::Write,
    kind: &str,
    cancelled: bool,
    json: bool,
) -> Result<(), CliError> {
    if json {
        write_json_result(
            output,
            &serde_json::json!({
                "kind": kind,
                "cancellation_requested": cancelled,
            }),
        )
    } else {
        if cancelled {
            writeln!(output, "{kind} cancellation requested")?;
        } else {
            writeln!(output, "no active {kind}")?;
        }
        output.flush()?;
        Ok(())
    }
}

fn write_permission_status<W: std::io::Write, E: std::io::Write>(
    output: &mut W,
    diagnostics: &mut E,
    source: &str,
    using_default: bool,
    build_tools: &[String],
    plan_tools: &[String],
    messages: &[String],
) -> Result<(), CliError> {
    writeln!(output, "source: {source}")?;
    writeln!(output, "using default policy: {using_default}")?;
    writeln!(output, "build enabled tools: {}", build_tools.join(", "))?;
    writeln!(output, "plan enabled tools: {}", plan_tools.join(", "))?;
    output.flush()?;
    for message in messages {
        writeln!(diagnostics, "{message}")?;
    }
    diagnostics.flush()?;
    Ok(())
}

async fn list_permissions(session_id: Option<SessionId>, json: bool) -> Result<(), CliError> {
    let mut permissions = BcodeClient::default_endpoint().list_permissions().await?;
    if let Some(session_id) = session_id {
        permissions.retain(|permission| permission.session_id == session_id);
    }
    write_permission_list(&mut std::io::stdout().lock(), &permissions, json)
}

async fn resolve_permission(
    permission_id: String,
    approved: bool,
    remember: bool,
    json: bool,
) -> Result<(), CliError> {
    let resolved = BcodeClient::default_endpoint()
        .resolve_permission_with_remember(permission_id, approved, remember)
        .await?;
    print_permission_resolution(resolved, json)
}

async fn resolve_permission_batch(
    batch_id: String,
    approved: bool,
    json: bool,
) -> Result<(), CliError> {
    let resolved_count = BcodeClient::default_endpoint()
        .resolve_permission_batch(batch_id, approved)
        .await?;
    write_permission_receipt(
        &mut std::io::stdout().lock(),
        &serde_json::json!({ "resolved_count": resolved_count }),
        format_args!("resolved: {resolved_count}"),
        json,
    )
}

fn print_permission_resolution(resolved: bool, json: bool) -> Result<(), CliError> {
    write_permission_receipt(
        &mut std::io::stdout().lock(),
        &serde_json::json!({ "resolved": resolved }),
        format_args!("resolved: {resolved}"),
        json,
    )
}

async fn add_permission_rule(
    agent_id: &str,
    category: &str,
    pattern: String,
    action: &str,
    json: bool,
) -> Result<(), CliError> {
    let config_path = BcodeClient::default_endpoint()
        .add_permission_rule(
            agent_id.to_string(),
            category.to_string(),
            pattern,
            action.to_string(),
        )
        .await?;
    write_permission_receipt(
        &mut std::io::stdout().lock(),
        &serde_json::json!({ "config_path": config_path }),
        format_args!("permission rule added: {config_path}"),
        json,
    )
}

fn write_permission_receipt<W: std::io::Write>(
    output: &mut W,
    value: &serde_json::Value,
    text: std::fmt::Arguments<'_>,
    json: bool,
) -> Result<(), CliError> {
    if json {
        write_json_result(output, value)
    } else {
        writeln!(output, "{text}")?;
        output.flush()?;
        Ok(())
    }
}

fn write_permission_list<W: std::io::Write>(
    output: &mut W,
    permissions: &[PermissionSummary],
    json: bool,
) -> Result<(), CliError> {
    if json {
        return write_json_result(output, &permissions);
    }
    for permission in permissions {
        writeln!(
            output,
            "{}\t{}\t{}\t{}\t{}\t{}",
            permission.permission_id,
            permission.session_id,
            permission.tool_call_id,
            permission.tool_name,
            permission.agent_id,
            permission.arguments_json
        )?;
    }
    output.flush()?;
    Ok(())
}

async fn watch_session(session_id: SessionId, limit: usize, json: bool) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let mut watcher = client.watch_session(session_id, limit).await?;
    let initial = watcher
        .take_initial()
        .expect("new session watcher must include bounded initial state");
    if json {
        print_json_line(&serde_json::json!({
            "type": "snapshot",
            "session_id": session_id,
            "session": initial.session,
            "history": initial.history,
            "runtime_selection": initial.runtime_selection,
            "projection_window": initial.projection_window,
        }))?;
    } else {
        for event in initial.history {
            print_session_event(&event);
        }
    }

    loop {
        match watcher.next_event().await? {
            SessionWatchEvent::Durable(event) => {
                if json {
                    print_json_line(&serde_json::json!({
                        "type": "durable_event",
                        "session_id": session_id,
                        "event": event,
                    }))?;
                } else {
                    print_session_event(&event);
                }
            }
            SessionWatchEvent::Live(event) => {
                if json {
                    print_json_line(&serde_json::json!({
                        "type": "live_event",
                        "session_id": session_id,
                        "event": event,
                    }))?;
                } else {
                    print_session_live_event(&event);
                }
            }
            SessionWatchEvent::ResyncRequired => {
                if json {
                    print_json_line(&serde_json::json!({
                        "type": "resync_required",
                        "session_id": session_id,
                    }))?;
                } else {
                    eprintln!("session view resync required; reconnect to replace bounded state");
                }
                return Ok(());
            }
        }
    }
}

async fn attach_session(session_id: SessionId) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let mut watcher = client
        .watch_session(session_id, SESSION_CLI_PAGE_LIMIT)
        .await?;
    for event in watcher
        .take_initial()
        .expect("new session watcher must include bounded initial state")
        .history
    {
        print_session_event(&event);
    }

    loop {
        tokio::select! {
            event = watcher.next_event() => {
                match event? {
                    SessionWatchEvent::Durable(event) => print_session_event(&event),
                    SessionWatchEvent::Live(event) => print_session_live_event(&event),
                    SessionWatchEvent::ResyncRequired => {
                        println!("session view resync required; replacing from bounded recent history");
                        watcher = client.watch_session(session_id, SESSION_CLI_PAGE_LIMIT).await?;
                        for event in watcher
                            .take_initial()
                            .expect("replacement watcher must include bounded initial state")
                            .history
                        {
                            print_session_event(&event);
                        }
                    }
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

fn print_session_live_event(event: &SessionLiveEvent) {
    println!("{}", session_live_event_description(event));
}

fn session_live_event_description(event: &SessionLiveEvent) -> String {
    match &event.kind {
        SessionLiveEventKind::ToolContributionPlaced { envelope } => format!(
            "live contribution {:?} {}:{} sequence={} schema={}@{} operation={:?}",
            envelope.placement,
            envelope.contribution.invocation_id,
            envelope.contribution.contribution_id,
            envelope.contribution.sequence,
            envelope.contribution.schema,
            envelope.contribution.schema_version,
            envelope.contribution.operation,
        ),
        SessionLiveEventKind::AssistantTextStreamUpdated {
            turn_id,
            segment_id,
            update,
            ..
        } => format!(
            "live assistant stream ({turn_id}/{segment_id}) generation={} revision={}: {:?}",
            update.generation, update.revision, update.operation
        ),
        SessionLiveEventKind::AssistantTextDelta { turn_id, text, .. } => {
            format!("live assistant delta ({turn_id}): {text}")
        }
        SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
            turn_id,
            activity_id,
            part_id,
            update,
            ..
        } => format!(
            "live reasoning stream ({turn_id}/{activity_id}/{part_id}) generation={} revision={}: {:?}",
            update.generation, update.revision, update.operation
        ),
        SessionLiveEventKind::AssistantReasoningDelta { turn_id, text } => {
            format!("live reasoning delta ({turn_id}): {text}")
        }
        SessionLiveEventKind::AssistantReasoningActivity { turn_id, event, .. } => {
            format!("live reasoning activity ({turn_id}): {event:?}")
        }
        SessionLiveEventKind::ProviderStreamProgress { turn_id, event } => {
            format!("live provider progress ({turn_id}): {event:?}")
        }
        SessionLiveEventKind::ToolPresentationUpdated { update } => format!(
            "live tool presentation call={} identity={:?} generation={} revision={} schema={}@{}",
            update.invocation_id,
            update.identity,
            update.generation,
            update.revision,
            update.schema,
            update.schema_version
        ),
        SessionLiveEventKind::ToolRequestDraft { event } => format!(
            "live tool request draft ({}) call={} generation={} revision={} bytes={} truncated={}",
            event.turn_id,
            event.tool_call_id,
            event.generation,
            event.revision,
            event.argument_bytes,
            event.truncated
        ),
        SessionLiveEventKind::ToolInvocationProgress { event } => format!(
            "live tool progress call={} sequence={} stage={:?}",
            event.invocation_id, event.sequence, event.stage
        ),
        SessionLiveEventKind::UsageSummaryChanged { summary } => {
            format!(
                "live cumulative cost: {:?}; unavailable requests: {}",
                summary.totals_micros, summary.unavailable_usage_count
            )
        }
        SessionLiveEventKind::RequestContextOccupancyChanged { occupancy } => {
            format!("live context occupancy: {occupancy:?}")
        }
    }
}

const MAX_CLI_PROMPT_BYTES: usize = 1024 * 1024;

struct PromptInput {
    message: Option<String>,
    file: Option<PathBuf>,
    stdin: bool,
}

struct SendOptions {
    input: PromptInput,
    follow_up: bool,
    producer: String,
    idempotency_key: Option<String>,
    background: bool,
    json: bool,
    launch_options: bcode_tui::TuiLaunchOptions,
}

impl PromptInput {
    fn read(self) -> Result<String, CliError> {
        let supplied = usize::from(self.message.is_some())
            + usize::from(self.file.is_some())
            + usize::from(self.stdin);
        if supplied != 1 {
            return Err(CliError::InvalidArguments(
                "send requires exactly one prompt source: MESSAGE, --file, or --stdin".to_owned(),
            ));
        }
        let text = if let Some(message) = self.message {
            message
        } else {
            let bytes = if self.stdin {
                read_prompt_bytes(std::io::stdin())?
            } else if let Some(path) = self.file {
                let metadata = fs::metadata(&path)?;
                if metadata.len() > MAX_CLI_PROMPT_BYTES as u64 {
                    return Err(prompt_too_large_error());
                }
                read_prompt_bytes(fs::File::open(path)?)?
            } else {
                unreachable!("prompt source count was validated")
            };
            String::from_utf8(bytes).map_err(|_| {
                CliError::InvalidArguments("prompt input must be valid UTF-8".to_owned())
            })?
        };
        if text.is_empty() {
            return Err(CliError::InvalidArguments(
                "prompt input must not be empty".to_owned(),
            ));
        }
        if text.len() > MAX_CLI_PROMPT_BYTES {
            return Err(prompt_too_large_error());
        }
        Ok(text)
    }
}

fn read_prompt_bytes(reader: impl std::io::Read) -> Result<Vec<u8>, CliError> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_CLI_PROMPT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CLI_PROMPT_BYTES {
        return Err(prompt_too_large_error());
    }
    Ok(bytes)
}

#[cfg(test)]
mod prompt_input_tests {
    use super::{CliError, MAX_CLI_PROMPT_BYTES, PromptInput};

    #[test]
    fn prompt_reader_stops_without_waiting_for_eof() {
        struct Endless {
            consumed: usize,
        }
        impl std::io::Read for Endless {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                assert!(self.consumed + buffer.len() <= MAX_CLI_PROMPT_BYTES + 1);
                buffer.fill(b'x');
                self.consumed += buffer.len();
                Ok(buffer.len())
            }
        }
        let mut reader = Endless { consumed: 0 };
        assert!(
            matches!(super::read_prompt_bytes(&mut reader), Err(CliError::InvalidArguments(message)) if message.contains("exceeds"))
        );
        assert_eq!(reader.consumed, MAX_CLI_PROMPT_BYTES + 1);
    }

    #[test]
    fn prompt_file_enforces_size_and_utf8_without_changing_content() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let read = || {
            PromptInput {
                message: None,
                file: Some(file.path().to_path_buf()),
                stdin: false,
            }
            .read()
        };
        for text in ["hello\n", "  keep whitespace  ", "unicode: 🦀"] {
            std::fs::write(file.path(), text).unwrap();
            assert_eq!(read().unwrap(), text);
        }
        let exact = "x".repeat(MAX_CLI_PROMPT_BYTES);
        std::fs::write(file.path(), &exact).unwrap();
        assert_eq!(read().unwrap(), exact);
        std::fs::write(file.path(), vec![b'x'; MAX_CLI_PROMPT_BYTES + 1]).unwrap();
        assert!(
            matches!(read(), Err(CliError::InvalidArguments(message)) if message.contains("exceeds"))
        );
        std::fs::write(file.path(), [0xff]).unwrap();
        assert!(
            matches!(read(), Err(CliError::InvalidArguments(message)) if message.contains("UTF-8"))
        );
        std::fs::write(file.path(), []).unwrap();
        assert!(
            matches!(read(), Err(CliError::InvalidArguments(message)) if message.contains("empty"))
        );
    }
}

fn prompt_too_large_error() -> CliError {
    CliError::InvalidArguments(format!("prompt input exceeds {MAX_CLI_PROMPT_BYTES} bytes"))
}

fn write_follow_up_disposition(
    output: &mut impl std::io::Write,
    disposition: &impl std::fmt::Debug,
) -> Result<(), CliError> {
    writeln!(output, "{disposition:?}")?;
    output.flush()?;
    Ok(())
}

async fn send_message(session_id: SessionId, options: SendOptions) -> Result<(), CliError> {
    let SendOptions {
        input,
        follow_up,
        producer,
        idempotency_key,
        background,
        json,
        launch_options,
    } = options;
    let text = input.read()?;
    let client = BcodeClient::default_endpoint();
    if follow_up {
        let acceptance = client
            .send_user_message_with_execution(
                session_id,
                text,
                bcode_session_models::PromptPlacement::FollowUp,
                launch_options.turn_execution_options(),
            )
            .await?;
        if json {
            print_json(&serde_json::json!({
                "session_id": session_id,
                "queued": acceptance.queued,
                "queue_position": acceptance.queue_position,
                "disposition": acceptance.disposition,
            }))?;
        } else {
            write_follow_up_disposition(&mut std::io::stdout().lock(), &acceptance.disposition)?;
        }
    } else {
        let admission = client
            .submit_turn(
                session_id,
                text,
                bcode_session_models::TurnAdmissionMetadata {
                    origin: Some(bcode_session_models::TurnOrigin {
                        producer,
                        correlation_id: None,
                        display_label: Some("Bcode CLI".to_owned()),
                    }),
                    priority: if background {
                        bcode_session_models::TurnPriority::Background
                    } else {
                        bcode_session_models::TurnPriority::Interactive
                    },
                    idempotency_key,
                    execution: launch_options.turn_execution_options(),
                },
            )
            .await?;
        write_turn_admission(&mut std::io::stdout().lock(), session_id, &admission, json)?;
    }
    Ok(())
}

fn write_turn_admission(
    output: &mut impl std::io::Write,
    session_id: SessionId,
    admission: &bcode_session_models::TurnAdmission,
    json: bool,
) -> Result<(), CliError> {
    if json {
        write_json_result(
            output,
            &serde_json::json!({ "session_id": session_id, "admission": admission }),
        )?;
    } else {
        writeln!(output, "{admission:?}")?;
        output.flush()?;
    }
    if let Some(error) = turn_admission_failure(admission) {
        return Err(error);
    }
    Ok(())
}

fn turn_admission_failure(admission: &bcode_session_models::TurnAdmission) -> Option<CliError> {
    match admission {
        bcode_session_models::TurnAdmission::Rejected(reason) => {
            Some(CliError::TurnRejected(reason.clone()))
        }
        bcode_session_models::TurnAdmission::CancelledBeforeStart(_) => {
            Some(CliError::TurnCancelledBeforeStart)
        }
        bcode_session_models::TurnAdmission::Accepted(_)
        | bcode_session_models::TurnAdmission::Existing(_)
        | bcode_session_models::TurnAdmission::Deferred(_) => None,
    }
}

const fn provider_compaction_origin_label(
    origin: bcode_session_models::ProviderContextSnapshotOrigin,
) -> &'static str {
    match origin {
        bcode_session_models::ProviderContextSnapshotOrigin::Explicit => "explicit provider-native",
        bcode_session_models::ProviderContextSnapshotOrigin::ProviderManaged => "provider-managed",
    }
}

fn provider_compaction_description(
    snapshot: &bcode_session_models::ProviderContextSnapshot,
    compacted_through_sequence: u64,
) -> String {
    format!(
        "{} context compacted through #{compacted_through_sequence}: {} {}",
        provider_compaction_origin_label(snapshot.origin),
        snapshot.provider_plugin_id,
        snapshot.model_id
    )
}

/// Return a stable display label for persisted model-selection provenance.
const fn model_selection_source_label(
    source: bcode_session_models::ModelSelectionSource,
) -> &'static str {
    match source {
        bcode_session_models::ModelSelectionSource::ConfigDefault => "config default",
        bcode_session_models::ModelSelectionSource::UserExplicit => "user",
        bcode_session_models::ModelSelectionSource::SkillRequired => "skill required",
        bcode_session_models::ModelSelectionSource::SkillPreferred => "skill preferred",
        bcode_session_models::ModelSelectionSource::AgentProfile => "agent profile",
    }
}

fn print_session_event(event: &SessionEvent) {
    match &event.kind {
        SessionEventKind::TraceEvent { trace } => print_trace_session_event(event, trace),
        _ => print_non_trace_session_event(event),
    }
}

fn reasoning_activity_description(
    sequence: u64,
    turn_id: &str,
    activity: &bcode_session_models::ReasoningActivity,
) -> String {
    let mut parts = activity.parts.iter().collect::<Vec<_>>();
    parts.sort_by_key(|part| (part.order, part.kind, part.part_id.as_str()));
    let mut output = format!(
        "#{sequence} reasoning ({turn_id}) {:?}: opaque={} parts={}",
        activity.status,
        activity.opaque,
        parts.len()
    );
    for part in parts {
        let _ = write!(
            output,
            "\nreasoning {:?}/{:?} [{}]: {}",
            part.kind, part.role, part.part_id, part.text
        );
    }
    output
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
        SessionEventKind::UserMessage {
            client_id, text, ..
        } => {
            println!("#{} {client_id}: {text}", event.sequence);
        }
        SessionEventKind::AssistantReasoningDelta { text }
        | SessionEventKind::AssistantReasoningMessage { text } => {
            println!("thinking: {text}");
        }
        SessionEventKind::AssistantReasoningActivity { turn_id, activity } => {
            println!(
                "{}",
                reasoning_activity_description(event.sequence, turn_id, activity)
            );
        }
        SessionEventKind::PositionedAssistantReasoningActivity {
            turn_id,
            output_position,
            activity,
        } => {
            println!(
                "{}\noutput position: {}",
                reasoning_activity_description(event.sequence, turn_id, activity),
                output_position.get()
            );
        }
        SessionEventKind::AssistantDelta { text } => {
            println!("#{} assistant delta: {text}", event.sequence);
        }
        SessionEventKind::AssistantMessage { text } => {
            println!("#{} assistant: {text}", event.sequence);
        }
        SessionEventKind::AssistantResponseSegment {
            turn_id,
            segment_id,
            segment_order,
            text,
        } => {
            println!(
                "#{} assistant {turn_id}/{segment_id} order={segment_order}: {text}",
                event.sequence
            );
        }
        SessionEventKind::PositionedAssistantResponseSegment {
            turn_id,
            output_position,
            segment_id,
            segment_order,
            text,
        } => {
            println!(
                "#{} assistant {turn_id}/{segment_id} position={} order={segment_order}: {text}",
                event.sequence,
                output_position.get()
            );
        }
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            tool_name,
            arguments_json,
            ..
        } => {
            println!(
                "#{} tool call requested: {tool_name} ({tool_call_id}) {}",
                event.sequence, arguments_json
            );
        }
        SessionEventKind::PositionedToolCallRequested {
            turn_id,
            output_position,
            tool_call_id,
            tool_name,
            arguments_json,
            ..
        } => {
            println!(
                "#{} tool call requested: {tool_name} ({tool_call_id}) turn={turn_id} position={} {}",
                event.sequence,
                output_position.get(),
                arguments_json
            );
        }
        SessionEventKind::ToolInvocationResultRecorded { record } => {
            let status = if record.is_error { "error" } else { "ok" };
            println!(
                "#{} invocation result ({status}): {}: {}",
                event.sequence, record.invocation_id, record.model_output
            );
        }
        SessionEventKind::ToolExchangeRequested { request } => println!(
            "#{} exchange requested {}:{} schema={}@{} policy={:?} payload={}",
            event.sequence,
            request.invocation_id,
            request.exchange_id,
            request.schema,
            request.schema_version,
            request.response_policy,
            request.payload
        ),
        SessionEventKind::ToolExchangeResolved { event: resolution } => println!(
            "#{} exchange resolved {}:{} {:?}",
            event.sequence, resolution.invocation_id, resolution.exchange_id, resolution.resolution
        ),
        SessionEventKind::ToolContribution {
            event: contribution,
        } => println!(
            "#{} contribution {}:{} sequence={} schema={}@{} operation={:?} payload={}",
            event.sequence,
            contribution.invocation_id,
            contribution.contribution_id,
            contribution.sequence,
            contribution.schema,
            contribution.schema_version,
            contribution.operation,
            contribution.payload
        ),
        SessionEventKind::ToolContributionPlaced { envelope } => println!(
            "#{} contribution {:?} {}:{} sequence={} schema={}@{} operation={:?} payload={}",
            event.sequence,
            envelope.placement,
            envelope.contribution.invocation_id,
            envelope.contribution.contribution_id,
            envelope.contribution.sequence,
            envelope.contribution.schema,
            envelope.contribution.schema_version,
            envelope.contribution.operation,
            envelope.contribution.payload
        ),
        SessionEventKind::ToolInvocationLifecycle { event: lifecycle } => println!(
            "#{} invocation {}: {:?}{}",
            event.sequence,
            lifecycle.invocation_id,
            lifecycle.stage,
            lifecycle
                .message
                .as_deref()
                .map_or_else(String::new, |message| format!(" — {message}"))
        ),
        SessionEventKind::PermissionRequested {
            permission_id,
            tool_call_id,
            tool_name,
            arguments_json,
            ..
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
        SessionEventKind::ModelChanged {
            provider,
            model,
            selection_source,
        } => {
            println!(
                "#{} model changed: {provider}/{model} (source={})",
                event.sequence,
                model_selection_source_label(*selection_source)
            );
        }
        SessionEventKind::ReasoningChanged {
            effort,
            summary,
            model_scope,
        } => {
            println!(
                "#{} reasoning changed: effort={} summary={} scope={}",
                event.sequence,
                effort.as_deref().unwrap_or("provider default"),
                summary.as_deref().unwrap_or("provider default"),
                model_scope.as_ref().map_or_else(
                    || "session".to_owned(),
                    |scope| format!("{}/{}", scope.provider, scope.model)
                )
            );
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
                display(old_working_directory, old_working_directory),
                display(new_working_directory, old_working_directory)
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
        SessionEventKind::ProviderContextCompacted {
            snapshot,
            compacted_through_sequence,
        } => println!(
            "#{} {}",
            event.sequence,
            provider_compaction_description(snapshot, *compacted_through_sequence)
        ),
        SessionEventKind::RequestContextObserved { observation } => println!(
            "#{} context usage: {} tokens for {} through #{} ({:?})",
            event.sequence,
            observation.context_tokens.tokens(),
            observation.request.effective_model_id,
            observation.context_through_sequence,
            observation.context_tokens
        ),
        SessionEventKind::ModelTurnStarted { turn_id } => {
            println!("#{} model turn started: {turn_id}", event.sequence);
        }
        SessionEventKind::ModelFeatureFidelityNegotiated { turn_id, feature } => println!(
            "#{} model feature fidelity: {turn_id} {}:{} mechanism={} fidelity={}",
            event.sequence, feature.family, feature.feature, feature.mechanism, feature.fidelity
        ),
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
        SessionEventKind::SessionDerived {
            source_session_id,
            source_generation,
            source_cutoff_sequence,
            producer,
            operation_kind,
            selected_source_sequence,
            ..
        } => println!(
            "#{} session derived from {source_session_id} generation {source_generation} through {source_cutoff_sequence} by {producer} ({operation_kind}, selected={selected_source_sequence:?})",
            event.sequence
        ),
        SessionEventKind::ExecutionSessionCreated {
            provenance,
            visibility,
        } => println!(
            "#{} execution session: owner={} run={} node={} attempt={} visibility={visibility:?}",
            event.sequence,
            provenance.owner,
            provenance.run_id,
            provenance.node_id,
            provenance.attempt,
        ),
        SessionEventKind::RalphLifecycle {
            loop_name,
            kind,
            message,
            ..
        } => println!(
            "#{} Ralph {kind} for {loop_name}: {message}",
            event.sequence
        ),
        SessionEventKind::PluginStatusNote {
            plugin_id, text, ..
        } => println!("#{} plugin status {plugin_id}: {text}", event.sequence),
        SessionEventKind::InertHistory { event_type, .. } => {
            println!("#{} legacy event: {event_type}", event.sequence);
        }
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
        SessionEventKind::AssistantResponseSegment { text, .. } => {
            println!("{prefix} assistant segment: {}", one_line(text));
        }
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            tool_name,
            ..
        } => {
            println!("{prefix} tool requested: {tool_name} ({tool_call_id})");
        }
        SessionEventKind::ToolInvocationResultRecorded { record } => {
            let status = if record.is_error { "error" } else { "ok" };
            println!(
                "{prefix} invocation result: {} {status}",
                record.invocation_id
            );
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
        SessionEventKind::ToolInvocationLifecycle { event } => {
            print_timeline_invocation_lifecycle(&prefix, event);
        }
        SessionEventKind::ToolExchangeRequested { request } => {
            print_timeline_exchange_request(&prefix, request);
        }
        SessionEventKind::ToolExchangeResolved { event } => {
            print_timeline_exchange_resolution(&prefix, event);
        }
        SessionEventKind::ToolContribution {
            event: contribution,
        } => {
            println!(
                "{prefix} contribution {}:{} sequence={} schema={}@{} operation={:?} payload={}",
                contribution.invocation_id,
                contribution.contribution_id,
                contribution.sequence,
                contribution.schema,
                contribution.schema_version,
                contribution.operation,
                contribution.payload
            );
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

fn print_timeline_invocation_lifecycle(
    prefix: &str,
    event: &bcode_session_models::ToolInvocationLifecycleEvent,
) {
    println!(
        "{prefix} invocation {}: {:?}{}",
        event.invocation_id,
        event.stage,
        event
            .message
            .as_deref()
            .map_or_else(String::new, |message| format!(" — {message}"))
    );
}

fn print_timeline_exchange_request(
    prefix: &str,
    request: &bcode_session_models::ToolExchangeRequest,
) {
    println!(
        "{prefix} exchange requested {}:{} schema={}@{}",
        request.invocation_id, request.exchange_id, request.schema, request.schema_version
    );
}

fn print_timeline_exchange_resolution(
    prefix: &str,
    event: &bcode_session_models::ToolExchangeResolutionEvent,
) {
    println!(
        "{prefix} exchange resolved {}:{} {:?}",
        event.invocation_id, event.exchange_id, event.resolution
    );
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
        bcode_session_models::ProviderStreamEvent::RetryScheduled {
            message,
            retry_at_unix,
        } => format!("provider retry scheduled retry_at_unix={retry_at_unix} message={message}"),
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

fn format_session_compatibility_issue(issue: &SessionEventCompatibilityIssue) -> String {
    let classification = match issue.compatibility {
        SessionEventCompatibilityKind::UnknownEventKind => "unsupported event kind",
        SessionEventCompatibilityKind::FutureSchema => "future event schema",
    };
    format!(
        "warning: session opened with opaque history at event #{}: {classification} {} (schema {}); {}",
        issue.sequence, issue.event_kind, issue.schema_version, issue.remediation
    )
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

#[cfg(test)]
mod auth_cli_tests {
    #[test]
    fn credential_discovery_flag_is_global() {
        use clap::Parser as _;
        for args in [
            vec!["bcode", "--no-credential-discovery"],
            vec!["bcode", "onboard", "--no-credential-discovery"],
            vec!["bcode", "--no-credential-discovery", "onboard", "--dry-run"],
        ] {
            let cli = super::Cli::try_parse_from(args).expect("discovery opt-out parses");
            assert!(cli.no_credential_discovery);
        }
        assert!(
            !super::Cli::try_parse_from(["bcode"])
                .expect("default parses")
                .no_credential_discovery
        );
    }

    use super::*;

    fn parse_auth_command(arguments: &[&str]) -> AuthCommand {
        use clap::{CommandFactory as _, FromArgMatches as _};
        let matches = Cli::command()
            .try_get_matches_from(arguments)
            .expect("auth command parses");
        let cli = Cli::from_arg_matches(&matches).expect("auth command decodes");
        let Some(Commands::Auth { command }) = cli.command else {
            panic!("expected auth command");
        };
        command
    }

    #[test]
    fn auth_security_command_parses_backend_requirement() {
        let command = parse_auth_command(&[
            "bcode",
            "auth",
            "security",
            "--provider",
            "xai",
            "--profile",
            "windows-smoke",
            "--vault",
            "C:\\smoke\\vault",
            "--require-backend",
            "windows-dpapi-current-user",
        ]);
        let AuthCommand::Security {
            provider,
            profile,
            vault,
            require_backend,
        } = command
        else {
            panic!("expected auth security command");
        };
        assert_eq!(provider.as_deref(), Some("xai"));
        assert_eq!(profile, "windows-smoke");
        assert_eq!(vault.as_deref(), Some(Path::new("C:\\smoke\\vault")));
        assert_eq!(
            require_backend.as_deref(),
            Some("windows-dpapi-current-user")
        );
    }

    #[test]
    fn auth_security_status_reads_runtime_profile_without_exposing_credentials() {
        let temp = tempfile::tempdir().expect("temp dir");
        let state_dir = temp.path().join("state");
        let subscriptions = state_dir.join("auth").join("subscriptions.json");
        std::fs::create_dir_all(subscriptions.parent().expect("subscriptions parent"))
            .expect("state dir");
        std::fs::write(
            &subscriptions,
            serde_json::to_vec_pretty(&bcode_config::RuntimeAuthSubscriptions {
                profiles: BTreeMap::from([(
                    "windows-smoke".to_owned(),
                    bcode_config::RuntimeAuthProfile {
                        provider_id: "xai".to_owned(),
                        owner_plugin_id: "bcode.xai".to_owned(),
                        backend: "sshenv".to_owned(),
                        scheme: "api_key".to_owned(),
                        storage_profile: "windows-smoke".to_owned(),
                        vault: state_dir.join("vault"),
                        map: BTreeMap::new(),
                        device_seal: Some("required".to_owned()),
                    },
                )]),
                ..bcode_config::RuntimeAuthSubscriptions::default()
            })
            .expect("runtime auth JSON"),
        )
        .expect("runtime auth state");
        let previous_state = std::env::var_os("BCODE_STATE_DIR");
        // SAFETY: this test restores the process environment before returning and does not run
        // concurrently with another environment-mutating test in this module.
        unsafe {
            std::env::set_var("BCODE_STATE_DIR", &state_dir);
        }
        let status = auth_security_status(Some("xai"), "windows-smoke", None)
            .expect("runtime profile security status");
        match previous_state {
            Some(value) => unsafe { std::env::set_var("BCODE_STATE_DIR", value) },
            None => unsafe { std::env::remove_var("BCODE_STATE_DIR") },
        }
        assert_eq!(status.profile, "windows-smoke");
        assert_eq!(
            status.policy,
            bcode_provider_auth::security::AuthDeviceSealPolicy::Required
        );
        assert!(!status.vault_exists);
        let encoded = serde_json::to_string(&status).expect("security status JSON");
        assert!(!encoded.contains("api_key"));
        assert!(!encoded.contains("secret"));
    }

    fn registered_provider(
        methods: Vec<bcode_provider_auth_models::AuthMethodContribution>,
    ) -> bcode_plugin::RegisteredAuthProvider {
        registered_provider_owned_by("test", "bcode.test", methods)
    }

    fn registered_provider_owned_by(
        provider_id: &str,
        plugin_id: &str,
        methods: Vec<bcode_provider_auth_models::AuthMethodContribution>,
    ) -> bcode_plugin::RegisteredAuthProvider {
        bcode_plugin::RegisteredAuthProvider {
            plugin_id: plugin_id.to_owned(),
            contribution: bcode_provider_auth_models::AuthProviderContribution {
                schema_version:
                    bcode_provider_auth_models::AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
                provider_id: provider_id.to_owned(),
                display_name: provider_id.to_owned(),
                methods,
            },
        }
    }

    fn interactive(method_id: &str) -> bcode_provider_auth_models::AuthMethodContribution {
        bcode_provider_auth_models::AuthMethodContribution::Interactive {
            method_id: method_id.to_owned(),
            display_name: method_id.to_owned(),
            operation: "flow".to_owned(),
            credentials: Vec::new(),
            supports_revocation: false,
        }
    }

    fn parse_compatible_login_plan(arguments: &[&str]) -> Result<CompatibleLoginPlan, CliError> {
        use clap::{CommandFactory as _, FromArgMatches as _};
        let matches = Cli::command()
            .try_get_matches_from(arguments)
            .expect("compatibility command parses");
        let cli = Cli::from_arg_matches(&matches).expect("compatibility command decodes");
        let Some(Commands::Login { command }) = cli.command else {
            panic!("expected compatibility login command");
        };
        compatible_login_plan(command)
    }

    struct CompatibleLoginCase<'a> {
        arguments: &'a [&'a str],
        provider: LoginProvider,
        method: &'a str,
        profile: Option<&'a str>,
        pool: Option<&'a str>,
        add_subscription: bool,
        replace_owned: bool,
    }

    #[test]
    fn compatibility_login_command_matrix_translates_to_registered_enrollment() {
        let cases = [
            CompatibleLoginCase {
                arguments: &["bcode", "login", "openai", "--api-key", "openai-secret"],
                provider: LoginProvider::OpenAi,
                method: "api_key",
                profile: None,
                pool: None,
                add_subscription: false,
                replace_owned: true,
            },
            CompatibleLoginCase {
                arguments: &["bcode", "login", "openai", "--chatgpt", "--browser"],
                provider: LoginProvider::OpenAi,
                method: "chatgpt",
                profile: None,
                pool: None,
                add_subscription: false,
                replace_owned: false,
            },
            CompatibleLoginCase {
                arguments: &["bcode", "login", "openai", "--headless"],
                provider: LoginProvider::OpenAi,
                method: "device",
                profile: None,
                pool: None,
                add_subscription: false,
                replace_owned: false,
            },
            CompatibleLoginCase {
                arguments: &[
                    "bcode",
                    "login",
                    "openai",
                    "--add-subscription",
                    "--profile",
                    "openai-2",
                ],
                provider: LoginProvider::OpenAi,
                method: "chatgpt",
                profile: Some("openai-2"),
                pool: Some("openai"),
                add_subscription: true,
                replace_owned: false,
            },
            CompatibleLoginCase {
                arguments: &[
                    "bcode",
                    "login",
                    "openai",
                    "--add-subscription",
                    "--profile",
                    "openai-2",
                    "--headless",
                ],
                provider: LoginProvider::OpenAi,
                method: "device",
                profile: Some("openai-2"),
                pool: Some("openai"),
                add_subscription: true,
                replace_owned: false,
            },
            CompatibleLoginCase {
                arguments: &["bcode", "login", "xai", "--api-key", "xai-secret"],
                provider: LoginProvider::Xai,
                method: "api_key",
                profile: None,
                pool: None,
                add_subscription: false,
                replace_owned: true,
            },
        ];
        for case in cases {
            let plan =
                parse_compatible_login_plan(case.arguments).expect("valid compatibility plan");
            assert_eq!(plan.provider, case.provider, "{:?}", case.arguments);
            assert_eq!(plan.method_id, case.method, "{:?}", case.arguments);
            assert_eq!(
                plan.explicit_profile.as_deref(),
                case.profile,
                "{:?}",
                case.arguments
            );
            assert_eq!(plan.pool, case.pool, "{:?}", case.arguments);
            assert_eq!(
                plan.add_subscription, case.add_subscription,
                "{:?}",
                case.arguments
            );
            assert_eq!(
                plan.replace_owned, case.replace_owned,
                "{:?}",
                case.arguments
            );
        }
    }

    #[test]
    fn compatibility_login_preserves_profile_wrapper_model_url_and_device_seal_options() {
        let plan = parse_compatible_login_plan(&[
            "bcode",
            "login",
            "openai",
            "--api-key",
            "secret",
            "--profile",
            "work",
            "--vault",
            "/tmp/auth-vault",
            "--recipient-key",
            "recipient",
            "--no-device-seal",
            "--model",
            "gpt-5",
            "--base-url",
            "https://openai.example/v1",
        ])
        .expect("OpenAI compatibility plan");
        assert_eq!(plan.explicit_profile.as_deref(), Some("work"));
        assert_eq!(plan.vault.as_deref(), Some(Path::new("/tmp/auth-vault")));
        assert_eq!(plan.recipient_key.as_deref(), Some("recipient"));
        assert!(plan.no_device_seal);
        assert_eq!(plan.model.as_deref(), Some("gpt-5"));
        assert_eq!(plan.base_url.as_deref(), Some("https://openai.example/v1"));
        assert_eq!(
            plan.supplied.get("api_key").map(String::as_str),
            Some("secret")
        );

        let xai = parse_compatible_login_plan(&[
            "bcode",
            "login",
            "xai",
            "--api-key",
            "secret",
            "--model",
            "grok-4",
        ])
        .expect("xAI compatibility plan");
        assert_eq!(xai.model.as_deref(), Some("grok-4"));
        assert_eq!(xai.base_url.as_deref(), Some("https://api.x.ai/v1"));
    }

    #[test]
    fn compatibility_login_rejects_api_key_subscription_pool() {
        let error = parse_compatible_login_plan(&[
            "bcode",
            "login",
            "openai",
            "--add-subscription",
            "--api-key",
            "secret",
        ])
        .expect_err("API-key pool must remain unsupported");
        assert!(
            error
                .to_string()
                .contains("API-key pooled auth is not supported")
        );
    }

    #[test]
    fn compatibility_pool_profile_selection_preserves_refresh_and_allocates_next_name() {
        let config = bcode_config::BcodeConfig::default();
        let runtime = bcode_config::RuntimeAuthSubscriptions::default();
        assert_eq!(
            compatible_login_profile("openai", Some("openai-2"), true).expect("explicit profile"),
            "openai-2"
        );
        assert_eq!(
            next_compatible_pool_profile(&config, &runtime, "openai", None),
            "openai-2"
        );

        let mut runtime = runtime;
        runtime.profiles.insert(
            "openai-2".to_owned(),
            bcode_config::RuntimeAuthProfile::default(),
        );
        assert_eq!(
            next_compatible_pool_profile(&config, &runtime, "openai", None),
            "openai-3"
        );
    }

    #[test]
    fn runtime_pool_profile_preserves_registered_method_and_device_seal_policy() {
        let resolved = bcode_provider_auth::ResolvedAuthProfile {
            profile_name: "openai-2".to_owned(),
            provider_id: "openai".to_owned(),
            owner_plugin_id: "bcode.test-openai-compatible".to_owned(),
            profile: bcode_config::AuthProfileConfig {
                backend: "sshenv".to_owned(),
                provider_id: Some("openai".to_owned()),
                owner_plugin_id: Some("bcode.test-openai-compatible".to_owned()),
                scheme: Some("device".to_owned()),
                map: BTreeMap::from([(
                    "access_token".to_owned(),
                    bcode_config::AuthCredentialMapping {
                        env: None,
                        key: Some("TOKEN".to_owned()),
                    },
                )]),
                settings: BTreeMap::from([
                    ("profile".to_owned(), "vault-profile".to_owned()),
                    ("vault".to_owned(), "/vault".to_owned()),
                    ("device_seal".to_owned(), "off".to_owned()),
                ]),
            },
            source: bcode_provider_auth::AuthProfileSource::Runtime,
        };
        let profile = runtime_pool_profile(&resolved);
        assert_eq!(profile.auth_profile, "openai-2");
        assert_eq!(profile.storage_profile, "vault-profile");
        assert_eq!(profile.scheme, "device");
        assert_eq!(profile.device_seal.as_deref(), Some("off"));
        assert_eq!(
            profile
                .map
                .get("access_token")
                .and_then(|mapping| mapping.key.as_deref()),
            Some("TOKEN")
        );
    }

    fn secret_field(method_id: &str) -> bcode_provider_auth_models::AuthMethodContribution {
        bcode_provider_auth_models::AuthMethodContribution::SecretFields {
            method_id: method_id.to_owned(),
            display_name: method_id.to_owned(),
            fields: vec![bcode_provider_auth_models::AuthSecretField {
                discovery_sources: Vec::new(),
                credential_id: "api_key".to_owned(),
                storage_key: "TEST_PROVIDER_API_KEY".to_owned(),
                prompt: "API key".to_owned(),
                optional: false,
                validation: bcode_provider_auth_models::AuthSecretValidation::default(),
            }],
            supports_verification: false,
            supports_revocation: false,
        }
    }

    #[test]
    fn fresh_provider_status_lookup_is_non_mutating_and_login_blueprint_is_owned() {
        let provider =
            registered_provider_owned_by("exa", "bcode.web-search", vec![secret_field("api_key")]);
        let config = bcode_config::BcodeConfig::default();
        let runtime = bcode_config::RuntimeAuthSubscriptions::default();

        assert_eq!(
            lookup_registered_auth_profile_from(&config, &runtime, &provider, None)
                .expect("fresh status lookup"),
            bcode_provider_auth::AuthProviderProfileLookup::Unconfigured {
                profile_name: "exa".to_owned(),
            }
        );
        assert!(runtime.bindings.is_empty());
        assert!(runtime.profiles.is_empty());
        let status = unconfigured_auth_provider_status_lines(&provider, "exa").join("\n");
        assert!(status.contains("Configured: false"));
        assert!(status.contains("Available: false"));
        assert!(status.contains("Diagnostic [auth_profile_missing]"));
        assert!(status.contains("bcode auth login exa"));

        let (resolved, persist_runtime) = resolve_or_prepare_auth_profile_from(
            &config,
            &runtime,
            &provider,
            &secret_field("api_key"),
            None,
            Some(PathBuf::from("/tmp/exa-vault")),
            None,
        )
        .expect("fresh login blueprint");
        assert!(persist_runtime);
        assert_eq!(resolved.profile_name, "exa");
        assert_eq!(resolved.profile.provider_id.as_deref(), Some("exa"));
        assert_eq!(
            resolved.profile.owner_plugin_id.as_deref(),
            Some("bcode.web-search")
        );
        assert_eq!(resolved.profile.scheme.as_deref(), Some("api_key"));
        assert_eq!(
            resolved
                .profile
                .map
                .get("api_key")
                .and_then(|mapping| mapping.key.as_deref()),
            Some("TEST_PROVIDER_API_KEY")
        );
    }

    #[test]
    fn fresh_status_lookup_does_not_hide_explicit_or_dangling_missing_profile() {
        let provider =
            registered_provider_owned_by("exa", "bcode.web-search", vec![secret_field("api_key")]);
        let runtime = bcode_config::RuntimeAuthSubscriptions::default();
        assert!(
            lookup_registered_auth_profile_from(
                &bcode_config::BcodeConfig::default(),
                &runtime,
                &provider,
                Some("missing"),
            )
            .is_err()
        );

        let config = bcode_config::BcodeConfig {
            auth: bcode_config::AuthConfig {
                bindings: BTreeMap::from([(
                    "exa".to_owned(),
                    bcode_config::AuthBindingConfig {
                        profile: Some("missing".to_owned()),
                    },
                )]),
                ..bcode_config::AuthConfig::default()
            },
            ..bcode_config::BcodeConfig::default()
        };
        assert!(lookup_registered_auth_profile_from(&config, &runtime, &provider, None,).is_err());
    }

    #[test]
    fn ambient_unowned_wrapper_profile_does_not_hijack_registered_provider() {
        let provider =
            registered_provider_owned_by("exa", "bcode.web-search", vec![interactive("api_key")]);
        let config: bcode_config::BcodeConfig = toml::from_str(
            r#"
[auth.profiles.openai]
backend = "sshenv"

[auth.profiles.openai.settings]
mode = "chatgpt"
profile = "openai"
provider = "openai"

[model]
profile = "default"

[model.profiles.default]
provider_plugin_id = "bcode.wrapper"
auth_profile = "openai"
"#,
        )
        .expect("OpenAI wrapper config parses");
        let runtime = bcode_config::RuntimeAuthSubscriptions::default();

        assert_eq!(
            registered_auth_profile_hint_from(
                &config,
                &runtime,
                &provider,
                None,
                Some("openai".to_owned()),
            ),
            None
        );
        assert_eq!(
            registered_auth_profile_hint_from(&config, &runtime, &provider, None, None),
            None
        );
        let error = resolve_registered_auth_profile_from(&config, &runtime, &provider, None)
            .expect_err("missing Exa enrollment");
        assert!(error.to_string().contains("provider 'exa'"));
        assert!(
            !error
                .to_string()
                .contains("profile 'openai' has no authentication scheme")
        );

        let (resolved, persist_runtime) = resolve_or_prepare_auth_profile_from(
            &config,
            &runtime,
            &provider,
            &interactive("api_key"),
            None,
            Some(PathBuf::from("/tmp/exa-vault")),
            None,
        )
        .expect("prepare Exa profile independently of wrapper");
        assert_eq!(resolved.profile_name, "exa");
        assert_eq!(resolved.profile.provider_id.as_deref(), Some("exa"));
        assert_eq!(
            resolved.profile.owner_plugin_id.as_deref(),
            Some("bcode.web-search")
        );
        assert!(persist_runtime);
    }

    #[test]
    fn ambient_typed_other_provider_is_ignored_but_explicit_profile_fails_closed() {
        let provider =
            registered_provider_owned_by("exa", "bcode.web-search", vec![interactive("api_key")]);
        let config = bcode_config::BcodeConfig {
            model: bcode_config::ModelConfig {
                profile: Some("wrapper".to_owned()),
                profiles: BTreeMap::from([(
                    "wrapper".to_owned(),
                    bcode_config::ModelProfileConfig {
                        provider_plugin_id: "bcode.wrapper".to_owned(),
                        auth_profile: Some("openai".to_owned()),
                        ..bcode_config::ModelProfileConfig::default()
                    },
                )]),
                ..bcode_config::ModelConfig::default()
            },
            auth: bcode_config::AuthConfig {
                profiles: BTreeMap::from([(
                    "openai".to_owned(),
                    bcode_config::AuthProfileConfig {
                        backend: "sshenv".to_owned(),
                        provider_id: Some("openai".to_owned()),
                        owner_plugin_id: Some("bcode.wrapper".to_owned()),
                        scheme: Some("chatgpt".to_owned()),
                        ..bcode_config::AuthProfileConfig::default()
                    },
                )]),
                ..bcode_config::AuthConfig::default()
            },
            ..bcode_config::BcodeConfig::default()
        };
        let runtime = bcode_config::RuntimeAuthSubscriptions::default();

        assert_eq!(
            registered_auth_profile_hint_from(
                &config,
                &runtime,
                &provider,
                None,
                Some("openai".to_owned()),
            ),
            None
        );
        let error =
            resolve_registered_auth_profile_from(&config, &runtime, &provider, Some("openai"))
                .expect_err("explicit mismatched profile must fail closed");
        assert!(error.to_string().contains("belongs to provider 'openai'"));
    }

    #[test]
    fn runtime_provider_binding_resolves_under_unrelated_wrapper() {
        let provider =
            registered_provider_owned_by("exa", "bcode.web-search", vec![interactive("api_key")]);
        let config: bcode_config::BcodeConfig = toml::from_str(
            r#"
[auth.profiles.openai]
backend = "sshenv"

[model]
profile = "default"

[model.profiles.default]
provider_plugin_id = "bcode.wrapper"
auth_profile = "openai"
"#,
        )
        .expect("wrapper config parses");
        let runtime = bcode_config::RuntimeAuthSubscriptions {
            bindings: BTreeMap::from([(
                "exa".to_owned(),
                bcode_config::RuntimeAuthBinding {
                    profile: "exa".to_owned(),
                    owner_plugin_id: "bcode.web-search".to_owned(),
                },
            )]),
            profiles: BTreeMap::from([(
                "exa".to_owned(),
                bcode_config::RuntimeAuthProfile {
                    provider_id: "exa".to_owned(),
                    owner_plugin_id: "bcode.web-search".to_owned(),
                    backend: "sshenv".to_owned(),
                    scheme: "api_key".to_owned(),
                    storage_profile: "exa".to_owned(),
                    vault: PathBuf::from("/tmp/exa-vault"),
                    map: BTreeMap::new(),
                    device_seal: None,
                },
            )]),
            ..bcode_config::RuntimeAuthSubscriptions::default()
        };

        let resolved = resolve_registered_auth_profile_from(&config, &runtime, &provider, None)
            .expect("runtime Exa binding resolves");
        assert_eq!(resolved.profile_name, "exa");
        assert_eq!(
            resolved.source,
            bcode_provider_auth::AuthProfileSource::Runtime
        );
    }

    #[test]
    fn registered_auth_profile_hint_uses_owned_active_model_profile() {
        let provider = registered_provider(vec![interactive("browser")]);
        let config = bcode_config::BcodeConfig {
            model: bcode_config::ModelConfig {
                profile: Some("wrapper".to_owned()),
                profiles: BTreeMap::from([(
                    "wrapper".to_owned(),
                    bcode_config::ModelProfileConfig {
                        provider_plugin_id: "bcode.test".to_owned(),
                        auth_profile: Some("test-work".to_owned()),
                        ..bcode_config::ModelProfileConfig::default()
                    },
                )]),
                ..bcode_config::ModelConfig::default()
            },
            auth: bcode_config::AuthConfig {
                profiles: BTreeMap::from([(
                    "test-work".to_owned(),
                    bcode_config::AuthProfileConfig {
                        provider_id: Some("test".to_owned()),
                        owner_plugin_id: Some("bcode.test".to_owned()),
                        ..bcode_config::AuthProfileConfig::default()
                    },
                )]),
                ..bcode_config::AuthConfig::default()
            },
            ..bcode_config::BcodeConfig::default()
        };
        assert_eq!(
            registered_auth_profile_hint(
                &config,
                &bcode_config::RuntimeAuthSubscriptions::default(),
                &provider,
                None,
            )
            .as_deref(),
            Some("test-work")
        );
    }

    #[test]
    fn registered_auth_profile_hint_ignores_other_provider_model_profile() {
        let provider = registered_provider(vec![interactive("browser")]);
        let config = bcode_config::BcodeConfig {
            model: bcode_config::ModelConfig {
                profile: Some("wrapper".to_owned()),
                profiles: BTreeMap::from([(
                    "wrapper".to_owned(),
                    bcode_config::ModelProfileConfig {
                        provider_plugin_id: "bcode.other".to_owned(),
                        auth_profile: Some("other".to_owned()),
                        ..bcode_config::ModelProfileConfig::default()
                    },
                )]),
                ..bcode_config::ModelConfig::default()
            },
            auth: bcode_config::AuthConfig {
                profiles: BTreeMap::from([(
                    "other".to_owned(),
                    bcode_config::AuthProfileConfig {
                        provider_id: Some("other".to_owned()),
                        owner_plugin_id: Some("bcode.other".to_owned()),
                        ..bcode_config::AuthProfileConfig::default()
                    },
                )]),
                ..bcode_config::AuthConfig::default()
            },
            ..bcode_config::BcodeConfig::default()
        };
        assert_eq!(
            registered_auth_profile_hint(
                &config,
                &bcode_config::RuntimeAuthSubscriptions::default(),
                &provider,
                None,
            ),
            None
        );
    }

    #[test]
    fn resolved_method_uses_profile_scheme_for_multi_method_provider() {
        let provider = registered_provider(vec![interactive("browser"), interactive("device")]);
        let profile = bcode_config::AuthProfileConfig {
            scheme: Some("device".to_owned()),
            ..bcode_config::AuthProfileConfig::default()
        };
        let resolved = bcode_provider_auth::ResolvedAuthProfile {
            profile_name: "test".to_owned(),
            provider_id: "test".to_owned(),
            owner_plugin_id: "bcode.test".to_owned(),
            profile,
            source: bcode_provider_auth::AuthProfileSource::Runtime,
        };
        assert_eq!(
            resolved_auth_method(&provider, &resolved)
                .expect("resolved method")
                .method_id(),
            "device"
        );
    }

    #[test]
    fn method_selection_requires_explicit_choice_for_ambiguous_provider() {
        let provider = registered_provider(vec![interactive("browser"), interactive("device")]);
        assert!(selected_auth_method(&provider, None).is_err());
        assert_eq!(
            selected_auth_method(&provider, Some("device"))
                .expect("selected method")
                .method_id(),
            "device"
        );
        assert!(selected_auth_method(&provider, Some("missing")).is_err());
    }

    #[test]
    fn cli_shape_accepts_generic_device_seal_opt_out() {
        use clap::{CommandFactory as _, FromArgMatches as _};
        let matches = Cli::command()
            .try_get_matches_from([
                "bcode",
                "auth",
                "login",
                "openai",
                "--method",
                "chatgpt",
                "--no-device-seal",
            ])
            .expect("device seal opt-out parses");
        let cli = Cli::from_arg_matches(&matches).expect("device seal opt-out decodes");
        assert!(matches!(
            cli.command,
            Some(Commands::Auth {
                command: AuthCommand::Login {
                    provider: Some(provider),
                    no_device_seal: true,
                    ..
                }
            }) if provider == "openai"
        ));
    }

    #[test]
    fn cli_shape_accepts_generic_pool_enrollment() {
        use clap::{CommandFactory as _, FromArgMatches as _};
        let matches = Cli::command()
            .try_get_matches_from([
                "bcode",
                "auth",
                "login",
                "openai",
                "--method",
                "chatgpt",
                "--profile",
                "openai-2",
                "--pool",
                "openai",
            ])
            .expect("pool enrollment parses");
        let cli = Cli::from_arg_matches(&matches).expect("pool enrollment decodes");
        assert!(matches!(
            cli.command,
            Some(Commands::Auth {
                command: AuthCommand::Login {
                    provider: Some(provider),
                    profile: Some(profile),
                    pool: Some(pool),
                    method: Some(method),
                    ..
                }
            }) if provider == "openai"
                && profile == "openai-2"
                && pool == "openai"
                && method == "chatgpt"
        ));
    }

    #[test]
    fn cli_shape_keeps_provider_optional_for_login_and_status() {
        use clap::{CommandFactory as _, FromArgMatches as _};
        let matches = Cli::command()
            .try_get_matches_from(["bcode", "auth", "login", "--profile", "legacy"])
            .expect("provider-less login parses");
        let cli = Cli::from_arg_matches(&matches).expect("provider-less login decodes");
        assert!(matches!(
            cli.command,
            Some(Commands::Auth {
                command: AuthCommand::Login {
                    provider: None,
                    profile: Some(profile),
                    ..
                }
            }) if profile == "legacy"
        ));

        let matches = Cli::command()
            .try_get_matches_from(["bcode", "auth", "status", "test", "--profile", "work"])
            .expect("provider status parses");
        let cli = Cli::from_arg_matches(&matches).expect("provider status decodes");
        assert!(matches!(
            cli.command,
            Some(Commands::Auth {
                command: AuthCommand::Status {
                    provider: Some(provider),
                    profile: Some(profile),
                }
            }) if provider == "test" && profile == "work"
        ));
    }

    #[test]
    fn unknown_and_disabled_provider_lookup_fails_closed() {
        let host = bcode_plugin::PluginHost::default();
        let error = registered_auth_provider(&host, "missing").expect_err("missing provider");
        assert!(
            error
                .to_string()
                .contains("not registered by an enabled plugin")
        );
    }

    #[test]
    fn auth_effect_output_is_fallible_and_preserves_prompt_choices() {
        use bcode_provider_auth_models::AuthFlowEffect;
        let effects = [
            AuthFlowEffect::OpenBrowser {
                url: "https://example.com".to_owned(),
            },
            AuthFlowEffect::Prompt {
                prompt_id: "confirm".to_owned(),
                message: "Confirm".to_owned(),
                choices: vec!["yes".to_owned(), "no".to_owned()],
            },
            AuthFlowEffect::DisplayDeviceCode {
                verification_url: "https://example.com/device".to_owned(),
                user_code: "ABCD".to_owned(),
                expires_in_seconds: Some(60),
            },
            AuthFlowEffect::Message {
                message: "Done".to_owned(),
            },
        ];
        let mut output = Vec::new();
        for effect in &effects {
            write_auth_effect(&mut output, effect).unwrap();
            let mut empty = &mut [][..];
            assert!(write_auth_effect(&mut empty, effect).is_err());
        }
        assert_eq!(
            output,
            b"Open in browser: https://example.com\nConfirm\nChoices: yes, no\nOpen https://example.com/device and enter code ABCD\nDone\n"
        );
        let mut empty = &mut [][..];
        write_auth_effect(&mut empty, &AuthFlowEffect::Wait { millis: 1 }).unwrap();
    }

    #[test]
    fn auth_output_propagates_flush_failures() {
        struct FlushFailure;
        impl std::io::Write for FlushFailure {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        let effect = bcode_provider_auth_models::AuthFlowEffect::Message {
            message: "Ready".to_owned(),
        };
        for result in [
            write_auth_effect(&mut FlushFailure, &effect),
            write_auth_diagnostic(&mut FlushFailure, "ready", "Ready"),
        ] {
            assert!(
                matches!(result, Err(CliError::Signal(error)) if error.kind() == std::io::ErrorKind::BrokenPipe)
            );
        }
        write_auth_effect(
            &mut FlushFailure,
            &bcode_provider_auth_models::AuthFlowEffect::Wait { millis: 1 },
        )
        .unwrap();
    }

    #[test]
    fn auth_terminal_diagnostics_are_reported_only_after_validation() {
        use bcode_provider_auth_models::{
            AUTH_FLOW_SCHEMA_VERSION, AuthDiagnostic, AuthDiagnosticSeverity, AuthFlowResponse,
            AuthFlowStatus,
        };
        for status in [AuthFlowStatus::Failed, AuthFlowStatus::Cancelled] {
            let mut response = AuthFlowResponse {
                schema_version: AUTH_FLOW_SCHEMA_VERSION,
                status,
                state: None,
                effects: Vec::new(),
                credentials: BTreeMap::new(),
                diagnostics: vec![AuthDiagnostic {
                    code: "denied".to_owned(),
                    severity: AuthDiagnosticSeverity::Error,
                    message: "Access denied".to_owned(),
                    remediation: None,
                }],
            };
            let mut output = Vec::new();
            let error = prepare_auth_flow_response(&mut output, &response).unwrap_err();
            assert_eq!(
                error.exit_code(),
                if status == AuthFlowStatus::Cancelled {
                    4
                } else {
                    1
                }
            );
            assert_eq!(output, b"Diagnostic [denied]: Access denied\n");
            response.diagnostics[0].remediation = Some("Check provider access".to_owned());
            output.clear();
            assert!(prepare_auth_flow_response(&mut output, &response).is_err());
            assert_eq!(
                output,
                b"Diagnostic [denied]: Access denied\n  remediation: Check provider access\n"
            );
            // Permit the diagnostic line but fail on remediation output.
            let mut storage = [0_u8; 35];
            let mut limited = &mut storage[..];
            let error = prepare_auth_flow_response(&mut limited, &response).unwrap_err();
            assert!(
                matches!(error, CliError::Signal(error) if error.kind() == std::io::ErrorKind::WriteZero)
            );
            response.schema_version += 1;
            output.clear();
            assert!(prepare_auth_flow_response(&mut output, &response).is_err());
            assert!(output.is_empty());
        }
    }

    #[test]
    fn auth_diagnostic_output_is_fallible() {
        struct Broken;
        impl std::io::Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut bytes = Vec::new();
        write_auth_diagnostic(&mut bytes, "pending", "Waiting for approval").unwrap();
        assert_eq!(bytes, b"Diagnostic [pending]: Waiting for approval\n");
        assert!(write_auth_diagnostic(&mut Broken, "pending", "Waiting for approval").is_err());
    }

    #[test]
    fn auth_input_preserves_exact_choices_and_full_size_lines() {
        let prompt = bcode_provider_auth_models::AuthFlowEffect::Prompt {
            prompt_id: "choice".to_owned(),
            message: "Choose".to_owned(),
            choices: vec![" yes ".to_owned()],
        };
        let answer = read_auth_input_line(std::io::Cursor::new(b" yes \r\n")).unwrap();
        prompt.validate_answer(&answer).unwrap();
        for ending in ["", "\n", "\r\n"] {
            let answer = "é".repeat(MAX_CLI_AUTH_INPUT_BYTES / 2);
            let mut input = std::io::Cursor::new(format!("{answer}{ending}"));
            assert_eq!(read_auth_input_line(&mut input).unwrap(), answer);
            let oversized = format!("{answer}x{ending}");
            assert!(read_auth_input_line(std::io::Cursor::new(oversized)).is_err());
        }
        assert_eq!(
            read_auth_input_line(std::io::Cursor::new(b"value\r")).unwrap(),
            "value\r"
        );
    }

    #[test]
    fn auth_input_is_bounded_and_preserves_following_lines() {
        let mut input = std::io::Cursor::new(b" first \r\nsecond\n");
        assert_eq!(read_auth_input_line(&mut input).unwrap(), " first ");
        assert_eq!(read_auth_input_line(&mut input).unwrap(), "second");
        assert!(read_auth_input_line(&mut input).is_err());
        assert_eq!(
            read_auth_input_line(std::io::Cursor::new(b"\n")).unwrap(),
            ""
        );
        assert!(read_auth_input_line(std::io::Cursor::new([0xff])).is_err());
        let mut oversized = std::io::Cursor::new(vec![b'x'; MAX_CLI_AUTH_INPUT_BYTES + 20]);
        assert!(read_auth_input_line(&mut oversized).is_err());
        assert_eq!(oversized.position(), (MAX_CLI_AUTH_INPUT_BYTES + 3) as u64);
        assert_eq!(
            read_auth_input_line(std::io::Cursor::new(vec![b'x'; MAX_CLI_AUTH_INPUT_BYTES]))
                .unwrap()
                .len(),
            MAX_CLI_AUTH_INPUT_BYTES
        );
    }

    #[test]
    fn failed_and_cancelled_flow_statuses_are_terminal_errors() {
        let failed = auth_flow_terminal_result(bcode_provider_auth_models::AuthFlowStatus::Failed)
            .expect_err("failed flow");
        assert_eq!(failed.exit_code(), 1);
        let cancelled =
            auth_flow_terminal_result(bcode_provider_auth_models::AuthFlowStatus::Cancelled)
                .expect_err("cancelled flow");
        assert!(matches!(cancelled, CliError::AuthFlowCancelled));
        assert_eq!(cancelled.exit_code(), 4);
        assert_eq!(
            cancelled.to_string(),
            "Provider authentication flow was cancelled."
        );
        // Text resembling cancellation must not change generic failure semantics.
        assert_eq!(
            CliError::LoginProfile("cancelled".to_owned()).exit_code(),
            1
        );
        assert_eq!(
            auth_flow_terminal_result(bcode_provider_auth_models::AuthFlowStatus::Pending)
                .expect("pending result"),
            None
        );
        assert_eq!(
            auth_flow_terminal_result(bcode_provider_auth_models::AuthFlowStatus::Succeeded)
                .expect("success result"),
            Some(())
        );
    }

    #[test]
    fn malformed_and_unsupported_flow_responses_fail_validation() {
        assert!(
            serde_json::from_value::<bcode_provider_auth_models::AuthFlowResponse>(
                serde_json::json!({"schema_version": 1, "status": "not_a_status"})
            )
            .is_err()
        );

        let unsupported = bcode_provider_auth_models::AuthFlowResponse {
            schema_version: bcode_provider_auth_models::AUTH_FLOW_SCHEMA_VERSION + 1,
            status: bcode_provider_auth_models::AuthFlowStatus::Failed,
            state: None,
            effects: Vec::new(),
            credentials: BTreeMap::new(),
            diagnostics: Vec::new(),
        };
        assert!(matches!(
            unsupported.validate(),
            Err(bcode_provider_auth_models::AuthContractError::UnsupportedSchema { .. })
        ));
    }

    #[test]
    fn duplicate_provider_registration_is_rejected_before_cli_dispatch() {
        let contribution = bcode_provider_auth_models::AuthProviderContribution {
            schema_version: bcode_provider_auth_models::AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
            provider_id: "test".to_owned(),
            display_name: "Test".to_owned(),
            methods: vec![interactive("browser")],
        };
        let mut registry = bcode_plugin::AuthProviderRegistry::new();
        registry
            .register("bcode.first", contribution.clone())
            .expect("first registration");
        assert!(matches!(
            registry.register("bcode.second", contribution),
            Err(bcode_plugin::AuthProviderRegistryError::DuplicateProvider { .. })
        ));
    }

    #[test]
    fn bounded_flow_contract_rejects_timeout_and_terminal_reopen_shapes() {
        let pending = bcode_provider_auth_models::AuthFlowResponse {
            schema_version: bcode_provider_auth_models::AUTH_FLOW_SCHEMA_VERSION,
            status: bcode_provider_auth_models::AuthFlowStatus::Pending,
            state: Some("state".to_owned()),
            effects: vec![bcode_provider_auth_models::AuthFlowEffect::Wait {
                millis: bcode_provider_auth_models::MAX_AUTH_WAIT_MILLIS + 1,
            }],
            credentials: BTreeMap::new(),
            diagnostics: Vec::new(),
        };
        assert!(pending.validate().is_err());

        let cancelled = bcode_provider_auth_models::AuthFlowResponse {
            schema_version: bcode_provider_auth_models::AUTH_FLOW_SCHEMA_VERSION,
            status: bcode_provider_auth_models::AuthFlowStatus::Cancelled,
            state: Some("stale".to_owned()),
            effects: Vec::new(),
            credentials: BTreeMap::new(),
            diagnostics: Vec::new(),
        };
        assert!(cancelled.validate().is_err());
    }
}

#[cfg(test)]
mod session_diagnosis_tests {
    use super::*;
    use switchy::database::{DatabaseValue, query::FilterableQuery as _};

    fn collect_files(root: &Path, path: &Path, files: &mut Vec<(String, Vec<u8>)>) {
        for entry in fs::read_dir(path).expect("diagnosis fixture directory") {
            let entry = entry.expect("diagnosis fixture entry");
            let path = entry.path();
            if path.is_dir() {
                collect_files(root, &path, files);
            } else {
                files.push((
                    path.strip_prefix(root)
                        .expect("fixture-relative path")
                        .to_string_lossy()
                        .into_owned(),
                    fs::read(path).expect("fixture file bytes"),
                ));
            }
        }
    }

    fn session_store_files(root: &Path) -> Vec<(String, Vec<u8>)> {
        let mut files = Vec::new();
        collect_files(root, root, &mut files);
        files.sort_by(|left, right| left.0.cmp(&right.0));
        files
    }

    async fn current_diagnosis_fixture(root: &Path) -> SessionId {
        let session_id = SessionId::new();
        let db = bcode_session::db::SessionDb::initialize_turso_in_root(session_id, root)
            .await
            .expect("current diagnosis fixture");
        drop(db);
        session_id
    }

    async fn assert_repeated_diagnosis_preserves_bytes(
        root: &Path,
        session_id: SessionId,
        expected_classification: &str,
    ) {
        let before = session_store_files(root);
        let first = collect_session_diagnosis(session_id, root)
            .await
            .expect("first diagnosis");
        let after_first = session_store_files(root);
        let second = collect_session_diagnosis(session_id, root)
            .await
            .expect("second diagnosis");
        let after_second = session_store_files(root);

        assert_eq!(first.classification, expected_classification);
        assert_eq!(second.classification, expected_classification);
        assert_eq!(after_first, before, "first diagnosis changed storage bytes");
        assert_eq!(
            after_second, before,
            "repeated diagnosis changed storage bytes"
        );
    }

    #[tokio::test]
    async fn repeated_diagnosis_is_byte_preserving_for_current_legacy_damaged_and_future_stores() {
        let current_root = tempfile::tempdir().expect("current root");
        let current = current_diagnosis_fixture(current_root.path()).await;
        assert_repeated_diagnosis_preserves_bytes(current_root.path(), current, "current_ready")
            .await;

        let legacy_root = tempfile::tempdir().expect("legacy root");
        let legacy = current_diagnosis_fixture(legacy_root.path()).await;
        let legacy_db =
            bcode_session::db::SessionDb::open_existing_turso_in_root(legacy, legacy_root.path())
                .await
                .expect("legacy DB");
        legacy_db
            .database()
            .update("session_storage_contract")
            .value("writer_epoch", DatabaseValue::Int64(2))
            .where_eq("contract_id", 1)
            .execute(legacy_db.database())
            .await
            .expect("legacy writer epoch");
        drop(legacy_db);
        assert_repeated_diagnosis_preserves_bytes(legacy_root.path(), legacy, "migratable").await;

        let damaged_root = tempfile::tempdir().expect("damaged root");
        let damaged = current_diagnosis_fixture(damaged_root.path()).await;
        let damaged_db =
            bcode_session::db::SessionDb::open_existing_turso_in_root(damaged, damaged_root.path())
                .await
                .expect("damaged DB");
        damaged_db
            .database()
            .delete("session_storage_contract")
            .where_eq("contract_id", 1)
            .execute(damaged_db.database())
            .await
            .expect("remove writer contract");
        drop(damaged_db);
        assert_repeated_diagnosis_preserves_bytes(
            damaged_root.path(),
            damaged,
            "structurally_corrupt",
        )
        .await;

        let future_root = tempfile::tempdir().expect("future root");
        let future = current_diagnosis_fixture(future_root.path()).await;
        let future_db =
            bcode_session::db::SessionDb::open_existing_turso_in_root(future, future_root.path())
                .await
                .expect("future DB");
        future_db
            .database()
            .update("session_storage_contract")
            .value(
                "writer_epoch",
                DatabaseValue::Int64(i64::from(
                    bcode_session::lease::CURRENT_SESSION_STORAGE_WRITER_EPOCH + 1,
                )),
            )
            .where_eq("contract_id", 1)
            .execute(future_db.database())
            .await
            .expect("future writer epoch");
        drop(future_db);
        assert_repeated_diagnosis_preserves_bytes(future_root.path(), future, "unsupported_future")
            .await;
    }

    #[test]
    fn auth_reset_confirmation_output_failure_does_not_consume_input() {
        struct FailingOutput {
            fail_write: bool,
        }
        impl std::io::Write for FailingOutput {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.fail_write {
                    Err(std::io::ErrorKind::BrokenPipe.into())
                } else {
                    Ok(bytes.len())
                }
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        for fail_write in [true, false] {
            let mut input = std::io::Cursor::new(b"yes\n");
            let error = super::read_auth_reset_confirmation(
                &mut input,
                &mut FailingOutput { fail_write },
                "test",
                None,
            )
            .unwrap_err();
            assert!(
                matches!(error, CliError::Signal(error) if error.kind() == std::io::ErrorKind::BrokenPipe)
            );
            assert_eq!(input.position(), 0);
        }
    }

    #[tokio::test]
    async fn plugin_service_empty_routes_fail_before_host_work() {
        for daemon in [false, true] {
            for (plugin, interface, operation) in [
                ("", "interface", "operation"),
                ("plugin", " ", "operation"),
                ("plugin", "interface", ""),
            ] {
                assert!(matches!(
                    super::invoke_plugin_service(
                        &[],
                        plugin,
                        interface,
                        operation,
                        None,
                        daemon,
                        true
                    )
                    .await,
                    Err(CliError::InvalidArguments(_))
                ));
            }
            for (interface, operation) in [("", "operation"), ("interface", "\t")] {
                assert!(matches!(
                    super::call_plugin_service(&[], interface, operation, None, daemon, true).await,
                    Err(CliError::InvalidArguments(_))
                ));
            }
        }
    }

    #[test]
    fn auth_reset_report_preserves_output_and_propagates_failures() {
        struct FailedOutput(bool);
        impl std::io::Write for FailedOutput {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.0 {
                    Err(std::io::ErrorKind::BrokenPipe.into())
                } else {
                    Ok(bytes.len())
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        let report = super::AuthResetConsumeReport {
            pool: "pool".to_owned(),
            provider_plugin_id: "provider".to_owned(),
            profile: "test".to_owned(),
            dry_run: false,
            credit_id: None,
            redeem_request_id: "request".to_owned(),
            status: "reset".to_owned(),
            provider_code: None,
            windows_reset: Some(2),
            message: None,
            debug: BTreeMap::new(),
        };
        for json in [false, true] {
            let mut output = Vec::new();
            super::write_auth_reset_consume_report(&mut output, &report, json).unwrap();
            if json {
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&output).unwrap(),
                    serde_json::to_value(&report).unwrap()
                );
            } else {
                assert_eq!(
                    String::from_utf8(output).unwrap(),
                    "Used one banked Codex reset for test.\n\nWindows reset: 2\nProvider result: reset\n"
                );
            }
            for fail_write in [false, true] {
                assert!(matches!(
                    super::write_auth_reset_consume_report(&mut FailedOutput(fail_write), &report, json),
                    Err(CliError::Signal(error)) if error.kind() == std::io::ErrorKind::BrokenPipe
                ));
            }
        }
    }

    #[test]
    fn auth_reset_confirmation_rejects_read_failure_after_yes() {
        struct FailedInput;
        impl std::io::Read for FailedInput {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::ConnectionReset.into())
            }
        }
        let reader = std::io::Read::chain(std::io::Cursor::new(b"yes"), FailedInput);
        let mut input = std::io::BufReader::new(reader);
        let error = super::read_auth_reset_confirmation(&mut input, &mut Vec::new(), "test", None)
            .unwrap_err();
        assert!(matches!(
            error,
            CliError::Signal(error) if error.kind() == std::io::ErrorKind::ConnectionReset
        ));
    }

    #[test]
    fn auth_reset_confirmation_rejects_invalid_utf8() {
        let mut input = std::io::Cursor::new(b"yes\xff\nno\n");
        let error = super::read_auth_reset_confirmation(&mut input, &mut Vec::new(), "test", None)
            .unwrap_err();
        assert!(matches!(
            error,
            CliError::Signal(error) if error.kind() == std::io::ErrorKind::InvalidData
        ));
        assert_eq!(input.position(), 5);
    }

    #[test]
    fn auth_reset_confirmation_is_bounded_and_requires_yes() {
        for (line, accepted) in [("yes\n", true), (" no\n", false), ("", false)] {
            assert_eq!(
                super::read_auth_reset_confirmation(
                    &mut line.as_bytes(),
                    &mut Vec::new(),
                    "test",
                    None,
                )
                .unwrap(),
                accepted
            );
        }
        let mut input = std::io::Cursor::new(b"yes\nno\n");
        assert!(
            super::read_auth_reset_confirmation(&mut input, &mut Vec::new(), "test", None).unwrap()
        );
        assert_eq!(input.position(), 4);
        for (size, accepted) in [(64, true), (65, false), (128, false)] {
            let mut line = b"yes".to_vec();
            line.resize(size, b' ');
            let mut input = std::io::Cursor::new(line);
            assert_eq!(
                super::read_auth_reset_confirmation(&mut input, &mut Vec::new(), "test", None)
                    .unwrap(),
                accepted
            );
            assert_eq!(input.position(), u64::try_from(size.min(65)).unwrap());
        }
        let mut endless = std::io::BufReader::new(std::io::repeat(b' '));
        assert!(
            !super::read_auth_reset_confirmation(&mut endless, &mut Vec::new(), "test", None,)
                .unwrap()
        );
    }

    #[tokio::test]
    async fn reprice_exact_size_catalog_reaches_document_validation() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = directory.path().join("catalog.json");
        // A JSON string is not a catalog document. Padding to the exact limit
        // distinguishes document rejection from an off-by-one size rejection
        // without dispatching a request to the daemon.
        let mut contents = br#""private-catalog-value""#.to_vec();
        contents.resize(16 * 1024 * 1024, b' ');
        std::fs::write(&catalog, &contents).unwrap();
        let error = super::reprice_session_command(SessionId::new(), 0, 1, &catalog)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            CliError::InvalidArguments(message)
                if message == "catalog snapshot is not a valid catalog document"
        ));
        assert_eq!(std::fs::read(&catalog).unwrap(), contents);
    }

    #[tokio::test]
    async fn reprice_rejects_oversized_catalog_before_parsing() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = directory.path().join("catalog.json");
        let file = std::fs::File::create(&catalog).unwrap();
        let size = 16 * 1024 * 1024 + 1;
        file.set_len(size).unwrap();
        drop(file);
        let error = super::reprice_session_command(SessionId::new(), 0, 1, &catalog)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            CliError::InvalidArguments(message) if message == "catalog snapshot exceeds 16 MiB"
        ));
        assert_eq!(std::fs::metadata(&catalog).unwrap().len(), size);
    }

    #[tokio::test]
    async fn reprice_rejects_invalid_range_before_opening_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = directory.path().join("missing.json");
        let error = super::reprice_session_command(SessionId::new(), 2, 1, &catalog)
            .await
            .unwrap_err();
        assert!(matches!(error, CliError::InvalidArguments(_)));
        assert!(!catalog.exists());
    }

    #[tokio::test]
    async fn reprice_rejects_invalid_catalog_without_exposing_contents() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = directory.path().join("catalog.json");
        let contents = br#""private-catalog-value""#;
        std::fs::write(&catalog, contents).unwrap();
        let error = super::reprice_session_command(SessionId::new(), 0, 1, &catalog)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            CliError::InvalidArguments(message)
                if message == "catalog snapshot is not a valid catalog document"
        ));
        assert_eq!(std::fs::read(&catalog).unwrap(), contents);
    }

    #[test]
    fn reprice_datetimes_require_timezone_and_preserve_boundaries() {
        assert_eq!(
            super::parse_reprice_datetime("1970-01-01T00:00:01.250Z").unwrap(),
            1250
        );
        assert_eq!(
            super::parse_reprice_datetime("1970-01-01T01:00:01.250+01:00").unwrap(),
            1250
        );
        assert!(super::parse_reprice_datetime("2026-09-07").is_err());
        assert!(super::parse_reprice_datetime("1969-12-31T23:59:59Z").is_err());
    }

    #[test]
    fn diagnosis_classification_distinguishes_all_required_storage_states() {
        use bcode_session::db::{SessionDbError, SessionStorageCompatibility};

        let current = SessionStorageCompatibility::Current { writer_epoch: 5 };
        let legacy = SessionStorageCompatibility::KnownLegacy { writer_epoch: 2 };
        assert_eq!(
            session_diagnosis_classification(Ok(&current), "ready", None, false),
            SessionDiagnosisClassification::CurrentReady
        );
        assert_eq!(
            session_diagnosis_classification(Ok(&legacy), "not ready", None, false),
            SessionDiagnosisClassification::Migratable
        );
        assert_eq!(
            session_diagnosis_classification(Ok(&legacy), "not ready", None, true),
            SessionDiagnosisClassification::BlockedOwner
        );
        let future = SessionDbError::WriterIncompatible {
            actual: Some(6),
            expected: 5,
        };
        assert_eq!(
            session_diagnosis_classification(Err(&future), "not ready", None, false),
            SessionDiagnosisClassification::UnsupportedFuture
        );
        let corrupt = SessionDbError::InvalidCanonicalSequence {
            expected: 1,
            actual: 2,
        };
        assert_eq!(
            session_diagnosis_classification(Err(&corrupt), "not ready", None, false),
            SessionDiagnosisClassification::StructurallyCorrupt
        );
        assert_eq!(
            session_diagnosis_classification(
                Ok(&current),
                "not ready",
                Some("strict decode failed"),
                false,
            ),
            SessionDiagnosisClassification::StructurallyCorrupt
        );
        assert_eq!(
            session_diagnosis_classification(Ok(&current), "projection stale", None, false),
            SessionDiagnosisClassification::RepairRequired
        );
    }
}

#[cfg(test)]
mod theme_command_tests {
    use super::*;

    #[test]
    fn theme_subcommands_parse() {
        assert!(matches!(
            Cli::try_parse_from(["bcode", "theme", "list"])
                .expect("theme list parses")
                .command,
            Some(Commands::Theme {
                command: ThemeCommand::List { json: false }
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["bcode", "theme", "validate", "theme.toml"])
                .expect("theme validate parses")
                .command,
            Some(Commands::Theme {
                command: ThemeCommand::Validate { .. }
            })
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "bcode",
                "theme",
                "copy",
                "terminal-native",
                "theme.toml",
                "--force"
            ])
            .expect("theme copy parses")
            .command,
            Some(Commands::Theme {
                command: ThemeCommand::Copy { force: true, .. }
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn theme_copy_json_rejects_non_utf8_paths_before_mutation() {
        use std::os::unix::ffi::OsStringExt;

        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("not-created");
        let path = parent.join(std::ffi::OsString::from_vec(vec![0xff]));
        let mut output = Vec::new();
        let copy = |output: &mut Vec<u8>| {
            write_theme_command(
                ThemeCommand::Copy {
                    builtin: "terminal-native".to_owned(),
                    path: path.clone(),
                    force: true,
                    json: true,
                },
                output,
            )
        };
        assert!(matches!(copy(&mut output), Err(CliError::Json(_))));
        assert!(!parent.exists());
        assert!(output.is_empty());

        // Linux filesystems permit existing non-UTF-8 names; macOS may reject
        // creating them even though the preflight check above is portable Unix.
        #[cfg(target_os = "linux")]
        {
            std::fs::create_dir(&parent).unwrap();
            std::fs::write(&path, "original content").unwrap();
            assert!(matches!(copy(&mut output), Err(CliError::Json(_))));
            assert_eq!(std::fs::read(&path).unwrap(), b"original content");
            assert!(output.is_empty());
        }
    }

    #[test]
    fn theme_copy_json_reports_completed_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("copy.toml");
        let cli = Cli::try_parse_from([
            "bcode",
            "theme",
            "copy",
            "terminal-native",
            path.to_str().unwrap(),
            "--json",
        ])
        .unwrap();
        let Some(Commands::Theme { command }) = cli.command else {
            panic!("theme command")
        };
        let mut output = Vec::new();
        write_theme_command(command, &mut output).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "schema_version": 1, "builtin": "terminal-native", "path": path,
            })
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            bcode_tui::theme::definition::ThemeCatalog::bundled_source("terminal-native").unwrap()
        );
        output.clear();
        assert!(
            write_theme_command(
                ThemeCommand::Copy {
                    builtin: "terminal-native".to_owned(),
                    path,
                    force: false,
                    json: true,
                },
                &mut output
            )
            .is_err()
        );
        assert!(output.is_empty());
    }

    #[test]
    fn theme_output_propagates_flush_failures() {
        #[derive(Default)]
        struct FlushFailure {
            bytes: Vec<u8>,
        }
        impl std::io::Write for FlushFailure {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("theme.toml");
        std::fs::write(
            &path,
            bcode_tui::theme::definition::ThemeCatalog::bundled_source("terminal-native").unwrap(),
        )
        .unwrap();
        for json in [false, true] {
            for command in [
                ThemeCommand::List { json },
                ThemeCommand::Validate {
                    path: path.clone(),
                    json,
                },
            ] {
                let mut writer = FlushFailure::default();
                assert!(write_theme_command(command, &mut writer).is_err());
                assert!(!writer.bytes.is_empty());
            }
        }
        let copied = root.path().join("copy.toml");
        let mut writer = FlushFailure::default();
        assert!(
            write_theme_command(
                ThemeCommand::Copy {
                    builtin: "terminal-native".to_owned(),
                    path: copied.clone(),
                    force: false,
                    json: false,
                },
                &mut writer
            )
            .is_err()
        );
        // Output failure does not roll back the already completed file copy.
        assert_eq!(std::fs::read(copied).unwrap(), std::fs::read(path).unwrap());
    }

    #[test]
    fn theme_command_writes_json_and_propagates_output_failure() {
        struct Broken;
        impl std::io::Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut output = Vec::new();
        write_theme_command(ThemeCommand::List { json: true }, &mut output).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        let catalog = bcode_tui::theme::definition::ThemeCatalog::bundled().unwrap();
        assert_eq!(value, theme_catalog_json(&catalog));
        for json in [false, true] {
            assert!(write_theme_command(ThemeCommand::List { json }, &mut Broken).is_err());
        }

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("theme.toml");
        std::fs::write(
            &path,
            bcode_tui::theme::definition::ThemeCatalog::bundled_source("terminal-native").unwrap(),
        )
        .unwrap();
        output.clear();
        write_theme_command(ThemeCommand::Validate { path, json: true }, &mut output).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["valid"], true);
        assert_eq!(value["id"], "terminal-native");
        assert_eq!(
            value["fingerprint"],
            catalog
                .resolve(&bcode_tui::theme::definition::ThemeSelection::new(
                    "terminal-native"
                ))
                .unwrap()
                .fingerprint
        );
    }

    #[test]
    fn theme_list_json_matches_runtime_catalog() {
        assert!(matches!(
            Cli::try_parse_from(["bcode", "theme", "list", "--json"])
                .unwrap()
                .command,
            Some(Commands::Theme {
                command: ThemeCommand::List { json: true }
            })
        ));
        let catalog = bcode_tui::theme::definition::ThemeCatalog::bundled().unwrap();
        let value = theme_catalog_json(&catalog);
        assert_eq!(value["schema_version"], 1);
        let rows = value["themes"].as_array().unwrap();
        assert_eq!(rows.len(), catalog.definitions().count());
        assert!(!rows.is_empty());
        for (row, definition) in rows.iter().zip(catalog.definitions()) {
            assert_eq!(row["id"], definition.id());
            assert_eq!(row["display_name"], definition.display_name());
            assert_eq!(row["dark"], definition.has_dark_variant());
            assert_eq!(row["light"], definition.has_light_variant());
            assert_eq!(row["source"], "bundled");
        }
    }

    #[test]
    fn theme_validate_rejects_oversized_and_non_utf8_documents() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("theme.toml");
        std::fs::write(&path, vec![b' '; MAX_CLI_THEME_BYTES + 1]).unwrap();
        let validate = || {
            handle_theme_command(ThemeCommand::Validate {
                path: path.clone(),
                json: true,
            })
        };
        assert!(
            matches!(validate(), Err(CliError::InvalidArguments(message))
            if message == format!("theme definition exceeds {MAX_CLI_THEME_BYTES} bytes"))
        );
        std::fs::write(&path, [0xff]).unwrap();
        assert!(
            matches!(validate(), Err(CliError::InvalidArguments(message))
            if message == "theme definition must be valid UTF-8")
        );
    }

    #[test]
    fn theme_copy_requires_force_to_replace_existing_content() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("theme.toml");
        std::fs::write(&path, "user content").unwrap();
        let copy = |force| {
            handle_theme_command(ThemeCommand::Copy {
                builtin: "terminal-native".to_owned(),
                path: path.clone(),
                force,
                json: false,
            })
        };
        assert!(copy(false).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "user content");
        copy(true).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            bcode_tui::theme::definition::ThemeCatalog::bundled_source("terminal-native").unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn theme_copy_does_not_follow_dangling_symlink_without_force() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("missing.toml");
        let path = root.path().join("theme.toml");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(
            handle_theme_command(ThemeCommand::Copy {
                builtin: "terminal-native".to_owned(),
                path: path.clone(),
                force: false,
                json: false,
            })
            .is_err()
        );
        assert!(!target.exists());
        assert_eq!(std::fs::read_link(path).unwrap(), target);
    }

    #[test]
    fn theme_validate_and_copy_use_runtime_definitions() {
        let root = tempfile::tempdir().expect("theme tempdir");
        let copied = root.path().join("nested/theme.toml");
        handle_theme_command(ThemeCommand::Copy {
            builtin: "terminal-native".to_owned(),
            path: copied.clone(),
            force: false,
            json: false,
        })
        .expect("copy bundled theme");
        for json in [false, true] {
            handle_theme_command(ThemeCommand::Validate {
                path: copied.clone(),
                json,
            })
            .expect("validate copied theme");
        }
        assert_eq!(
            std::fs::read_to_string(copied).expect("copied theme"),
            bcode_tui::theme::definition::ThemeCatalog::bundled_source("terminal-native")
                .expect("bundled source")
        );
    }
}

#[cfg(test)]
mod plugin_surface_repo_path_tests {
    use super::*;

    #[test]
    fn missing_repo_path_resolves_to_caller_working_directory() {
        let expected = fs::canonicalize(std::env::current_dir().expect("current directory"))
            .expect("canonical current directory");

        let resolved = resolve_surface_repo_path(None).expect("current directory resolves");

        assert_eq!(resolved, Some(expected));
    }

    #[test]
    fn relative_repo_path_resolves_against_caller_working_directory() {
        let expected = fs::canonicalize(std::env::current_dir().expect("current directory"))
            .expect("canonical current directory");

        let resolved = resolve_surface_repo_path(Some(std::path::PathBuf::from(".")))
            .expect("relative path resolves");

        assert_eq!(
            resolved,
            Some(expected),
            "`.` must resolve in the client process, not the daemon"
        );
    }

    #[test]
    fn absolute_repo_path_is_preserved() {
        let temp = tempfile::tempdir().expect("tempdir");
        let expected = fs::canonicalize(temp.path()).expect("canonical tempdir");

        let resolved =
            resolve_surface_repo_path(Some(expected.clone())).expect("absolute path resolves");

        assert_eq!(resolved, Some(expected));
    }

    #[test]
    fn unavailable_repo_path_is_surfaced_rather_than_silently_defaulted() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing = temp.path().join("does-not-exist");

        let error =
            resolve_surface_repo_path(Some(missing)).expect_err("missing path should fail closed");

        assert!(
            matches!(error, CliError::SurfaceRepoPath(_)),
            "expected a surface repo path error, got {error:?}"
        );
    }
}

#[cfg(test)]
mod session_migration_cli_tests {
    use super::*;

    fn parse_session_command(arguments: &[&str]) -> SessionCommand {
        use clap::{CommandFactory as _, FromArgMatches as _};
        let matches = Cli::command()
            .try_get_matches_from(arguments)
            .expect("session command parses");
        let cli = Cli::from_arg_matches(&matches).expect("session command decodes");
        let Some(Commands::Session { command }) = cli.command else {
            panic!("expected session command");
        };
        command
    }

    #[test]
    fn bulk_migration_cli_exposes_inventory_confirmed_start_and_lifecycle_commands() {
        assert!(matches!(
            parse_session_command(&[
                "bcode",
                "session",
                "migrate-inventory",
                "--after-timestamp-ms",
                "10",
                "--json",
            ]),
            SessionCommand::MigrateInventory {
                after_timestamp_ms: Some(10),
                json: true,
                ..
            }
        ));
        assert!(matches!(
            parse_session_command(&[
                "bcode",
                "session",
                "migrate-start",
                "--confirm",
                "migrate-supported-sessions",
                "--foreground",
            ]),
            SessionCommand::MigrateStart {
                confirm,
                foreground: true,
                ..
            } if confirm == bcode_ipc::SESSION_BULK_MIGRATION_CONFIRMATION
        ));
        assert!(matches!(
            parse_session_command(&["bcode", "session", "migrate-status", "operation-1"]),
            SessionCommand::MigrateStatus { operation_id, .. } if operation_id == "operation-1"
        ));
        assert!(matches!(
            parse_session_command(&[
                "bcode",
                "session",
                "migrate-wait",
                "operation-1",
                "--after-revision",
                "4",
            ]),
            SessionCommand::MigrateWait {
                after_revision: 4,
                ..
            }
        ));
        assert!(matches!(
            parse_session_command(&["bcode", "session", "migrate-cancel", "operation-1"]),
            SessionCommand::MigrateCancel { operation_id, .. } if operation_id == "operation-1"
        ));
    }
}

#[cfg(test)]
mod web_command_tests {
    use super::*;

    #[test]
    fn cli_reasoning_description_preserves_structure_and_opaque_evidence() {
        let activity = bcode_session_models::ReasoningActivity {
            activity_id: "reasoning-1".to_owned(),
            order: 0,
            status: bcode_session_models::ReasoningActivityStatus::Interrupted,
            parts: vec![
                bcode_session_models::ReasoningPart {
                    part_id: "raw-0".to_owned(),
                    kind: bcode_session_models::ReasoningContentKind::Raw,
                    role: bcode_session_models::ReasoningContentRole::Detail,
                    order: 1,
                    text: "raw detail".to_owned(),
                },
                bcode_session_models::ReasoningPart {
                    part_id: "summary-0".to_owned(),
                    kind: bcode_session_models::ReasoningContentKind::Summary,
                    role: bcode_session_models::ReasoningContentRole::Milestone,
                    order: 0,
                    text: "summary".to_owned(),
                },
            ],
            opaque: true,
        };
        let rendered = reasoning_activity_description(7, "turn-1", &activity);
        assert!(rendered.contains("Interrupted: opaque=true parts=2"));
        assert!(rendered.contains("Summary/Milestone [summary-0]: summary"));
        assert!(rendered.contains("Raw/Detail [raw-0]: raw detail"));
        assert!(
            rendered.find("summary-0").expect("summary") < rendered.find("raw-0").expect("raw")
        );
    }

    #[tokio::test]
    async fn doctor_summary_categories_are_actionable_and_capped() {
        use bcode_session::repair::{RepairReport, RepairStatus, RepairTarget};

        let session_id = SessionId::new();
        let missing = RepairReport {
            target: RepairTarget::Session { session_id },
            db_path: std::path::PathBuf::from("/definitely/missing/session.db"),
            status: RepairStatus::ManualRequired,
            backup_path: None,
            initial_error: Some("session database does not exist".to_owned()),
            final_error: None,
            actions: Vec::new(),
            notes: Vec::new(),
        };
        let (category, action, retryable) =
            classify_session_doctor_report(std::path::Path::new("/definitely/missing"), &missing)
                .await;
        assert_eq!(category, SessionDoctorCategory::Missing);
        assert_eq!(action, SessionDoctorAction::Locate);
        assert!(!retryable);
    }

    #[tokio::test]
    async fn session_doctor_summaries_are_actionable_and_capped() {
        let session_ids = (0..5).map(|_| SessionId::new()).collect::<Vec<_>>();
        let reports = session_ids
            .iter()
            .map(|session_id| bcode_session::repair::RepairReport {
                target: bcode_session::repair::RepairTarget::Session {
                    session_id: *session_id,
                },
                db_path: std::path::PathBuf::from("/missing/session.db"),
                status: bcode_session::repair::RepairStatus::ManualRequired,
                backup_path: None,
                initial_error: Some("session database does not exist".to_owned()),
                final_error: None,
                actions: Vec::new(),
                notes: Vec::new(),
            })
            .collect::<Vec<_>>();
        let root = tempfile::tempdir().expect("doctor root");

        let (counts, summaries, findings) =
            summarize_session_doctor_reports(root.path(), &reports).await;

        assert_eq!(counts[&SessionDoctorCategory::Missing], 5);
        let missing = &summaries[&SessionDoctorCategory::Missing];
        assert_eq!(missing.count, 5);
        assert_eq!(missing.action, SessionDoctorAction::Locate);
        assert!(!missing.retryable);
        assert_eq!(
            missing.samples.len(),
            MAX_SESSION_DOCTOR_SAMPLES_PER_CATEGORY
        );
        assert_eq!(findings.len(), 5);
    }

    #[test]
    fn operation_payload_serialization_size_regression() {
        let outcomes = (0..bcode_ipc::MAX_SESSION_BULK_MIGRATION_OUTCOMES)
            .map(|_| bcode_ipc::SessionBulkMigrationOutcome {
                session_id: bcode_session_models::SessionId::new(),
                category: bcode_ipc::SessionCompatibilityCategory::MigrationRequired,
                action: bcode_ipc::SessionCompatibilityAction::Migrate,
                message: Some("x".repeat(bcode_ipc::MAX_SESSION_COMPATIBILITY_MESSAGE_BYTES)),
            })
            .collect::<Vec<_>>();
        let migration = bcode_ipc::SessionBulkMigrationOperationStatus {
            operation_id: "operation".to_owned(),
            revision: 1,
            state: bcode_ipc::SessionBulkMigrationState::NeedsAttention,
            mode: bcode_ipc::SessionBulkMigrationMode::Migrate,
            selected: outcomes.len() as u64,
            visited: outcomes.len() as u64,
            migrated: 0,
            blocked: outcomes.len() as u64,
            failed: 0,
            current_session_id: None,
            outcomes,
        };
        let migration_bytes = serde_json::to_vec(&migration)
            .expect("bounded migration payload")
            .len();
        assert!(migration_bytes < 512 * 1024, "{migration_bytes}");

        let providers = (0..bcode_session_search::MAX_COMPLETE_BACKFILL_PROVIDERS)
            .map(
                |index| bcode_session_search::CompleteSessionSearchBackfillProviderResult {
                    provider_id: format!("provider-{index}"),
                    selected_sessions: bcode_session_search::MAX_BACKFILL_SESSIONS,
                    completed_sessions: 0,
                    incomplete_sessions: 0,
                    failed_sessions: bcode_session_search::MAX_BACKFILL_SESSIONS,
                    catalog_pages: 1,
                    issues: (0..bcode_session_search::MAX_COMPLETE_BACKFILL_ISSUE_SAMPLES)
                        .map(
                            |_| bcode_session_search::CompleteSessionSearchBackfillIssueSummary {
                                code: bcode_session_search::SearchErrorCode::ProviderUnavailable,
                                count: 1,
                                retryable: true,
                                sample_session_ids: (0
                                    ..bcode_session_search::MAX_COMPLETE_BACKFILL_ISSUE_SAMPLES)
                                    .map(|_| bcode_session_models::SessionId::new())
                                    .collect(),
                                sample_message: Some(
                                    "x".repeat(bcode_session_search::MAX_HIT_PREVIEW_BYTES),
                                ),
                            },
                        )
                        .collect(),
                    error: None,
                },
            )
            .collect::<Vec<_>>();
        let backfill = bcode_session_search::CompleteSessionSearchBackfillResponse {
            provider_ids: providers
                .iter()
                .map(|provider| provider.provider_id.clone())
                .collect(),
            catalog_revision_started: 1,
            catalog_revision_completed: 1,
            convergence_passes: 1,
            cancelled: false,
            providers,
        };
        backfill.validate().expect("bounded backfill response");
        let backfill_bytes = serde_json::to_vec(&backfill)
            .expect("bounded backfill payload")
            .len();
        assert!(backfill_bytes < 5 * 1024 * 1024, "{backfill_bytes}");
        println!(
            "session_operation_payload_sizes migration_bytes={migration_bytes} backfill_bytes={backfill_bytes}"
        );
    }

    #[test]
    fn compatibility_issue_format_is_actionable_and_specific() {
        for (compatibility, expected_classification) in [
            (
                SessionEventCompatibilityKind::UnknownEventKind,
                "unsupported event kind",
            ),
            (
                SessionEventCompatibilityKind::FutureSchema,
                "future event schema",
            ),
        ] {
            let rendered = format_session_compatibility_issue(&SessionEventCompatibilityIssue {
                sequence: 1158,
                event_kind: "future_event_kind".to_owned(),
                schema_version: 39,
                compatibility,
                remediation: "upgrade Bcode".to_owned(),
            });
            assert!(rendered.contains("event #1158"));
            assert!(rendered.contains(expected_classification));
            assert!(rendered.contains("future_event_kind"));
            assert!(rendered.contains("schema 39"));
            assert!(rendered.contains("upgrade Bcode"));
        }
    }

    #[test]
    fn workflow_run_controls_preserve_run_identity() {
        for command in ["cancel-run", "pause-run", "resume-run", "run-status"] {
            let cli = Cli::try_parse_from([
                "bcode",
                "workflow",
                command,
                "--run-id",
                "run/exact-identity",
            ])
            .expect("workflow control");
            let Some(Commands::Workflow { command: parsed }) = cli.command else {
                panic!("expected workflow command");
            };
            let (("cancel-run", WorkflowCommand::CancelRun { run_id })
            | ("pause-run", WorkflowCommand::PauseRun { run_id })
            | ("resume-run", WorkflowCommand::ResumeRun { run_id })
            | ("run-status", WorkflowCommand::RunStatus { run_id })) = (command, parsed)
            else {
                panic!("incorrect workflow control variant");
            };
            assert_eq!(run_id, "run/exact-identity");
        }
    }

    #[test]
    fn workflow_run_and_wait_lists_preserve_limits() {
        for limit in [None, Some("7")] {
            let expected = if limit.is_some() { 7 } else { 100 };
            for command in ["runs", "waits"] {
                let mut arguments = vec!["bcode", "workflow", command];
                if command == "waits" {
                    arguments.extend(["--run-id", "run-1"]);
                }
                if let Some(limit) = limit {
                    arguments.extend(["--limit", limit]);
                }
                let cli = Cli::try_parse_from(arguments).expect("bounded workflow list");
                match cli.command {
                    Some(Commands::Workflow {
                        command: WorkflowCommand::Runs { limit },
                    }) => {
                        assert_eq!(command, "runs");
                        assert_eq!(limit, expected);
                    }
                    Some(Commands::Workflow {
                        command: WorkflowCommand::Waits { run_id, limit },
                    }) => {
                        assert_eq!(command, "waits");
                        assert_eq!(run_id, "run-1");
                        assert_eq!(limit, expected);
                    }
                    _ => panic!("incorrect workflow list variant"),
                }
            }
        }
    }

    #[test]
    fn workflow_doctor_preserves_bounds_and_rejects_repair_flags() {
        for limit in [None, Some("7")] {
            let mut arguments = vec!["bcode", "workflow", "doctor", "--run-id", "run-1"];
            if let Some(limit) = limit {
                arguments.extend(["--limit", limit]);
            }
            let cli = Cli::try_parse_from(&arguments).expect("workflow doctor");
            assert!(matches!(cli.command,
                Some(Commands::Workflow { command: WorkflowCommand::Doctor { run_id, limit: bound } })
                if run_id == "run-1" && bound == if limit.is_some() { 7 } else { 100 }
            ));
            arguments.push("--apply");
            assert!(Cli::try_parse_from(arguments).is_err());
        }
    }

    #[test]
    fn workflow_retry_node_requires_exact_attempt_identity() {
        let arguments = [
            "bcode",
            "workflow",
            "retry-node",
            "--run-id",
            "run-1",
            "--node-id",
            "node-2",
            "--activation-id",
            "activation-3",
            "--failed-attempt",
            "4",
        ];
        let cli = Cli::try_parse_from(arguments).expect("exact node retry");
        assert!(matches!(
            cli.command,
            Some(Commands::Workflow {
                command: WorkflowCommand::RetryNode {
                    run_id, node_id, activation_id, failed_attempt
                }
            }) if run_id == "run-1" && node_id == "node-2"
                && activation_id == "activation-3" && failed_attempt == 4
        ));
        for index in [3, 5, 7, 9] {
            let incomplete: Vec<_> = arguments
                .iter()
                .enumerate()
                .filter_map(|(position, value)| {
                    (!(index..index + 2).contains(&position)).then_some(*value)
                })
                .collect();
            assert!(Cli::try_parse_from(incomplete).is_err());
        }
        for invalid in ["0", "-1", "4294967296", "latest"] {
            let mut invalid_arguments = arguments;
            invalid_arguments[10] = invalid;
            assert!(Cli::try_parse_from(invalid_arguments).is_err());
        }
    }

    #[test]
    fn workflow_stage_run_edit_requires_candidate_file() {
        assert!(Cli::try_parse_from(["bcode", "workflow", "stage-run-edit"]).is_err());
        let cli =
            Cli::try_parse_from(["bcode", "workflow", "stage-run-edit", "--file", "edit.json"])
                .expect("stage edit command");
        assert!(matches!(cli.command,
            Some(Commands::Workflow { command: WorkflowCommand::StageRunEdit { file } })
            if file == Path::new("edit.json")));
    }

    #[test]
    fn workflow_run_output_command_parses_bounded_identity() {
        let cli = Cli::try_parse_from([
            "bcode",
            "workflow",
            "run-output",
            "--run-id",
            "run-1",
            "--limit",
            "25",
        ])
        .expect("run output command");
        assert!(matches!(
            cli.command,
            Some(Commands::Workflow {
                command: WorkflowCommand::RunOutput { run_id, limit }
            }) if run_id == "run-1" && limit == 25
        ));
    }

    #[test]
    fn workflow_store_reset_requires_explicit_confirmation_value() {
        assert!(Cli::try_parse_from(["bcode", "workflow", "reset-store"]).is_err());
        let cli = Cli::try_parse_from([
            "bcode",
            "workflow",
            "reset-store",
            "--confirm",
            "DELETE-INCOMPATIBLE-WORKFLOW-STATE",
        ])
        .expect("explicit reset command");
        assert!(matches!(
            cli.command,
            Some(Commands::Workflow {
                command: WorkflowCommand::ResetStore { confirm }
            }) if confirm == "DELETE-INCOMPATIBLE-WORKFLOW-STATE"
        ));
    }

    #[test]
    fn exact_revision_import_command_parses_machine_readable_controls() {
        let cli = Cli::try_parse_from([
            "bcode",
            "workflow",
            "author",
            "import-revision",
            "bundle.json",
            "--workflow-id",
            "workflow/example",
            "--revision",
            "2",
            "--activate",
            "--expected-active-revision",
            "1",
            "--operation-id",
            "import-revision-2",
            "--timeout-ms",
            "5000",
        ])
        .expect("revision import command should parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Workflow {
                command: WorkflowCommand::Author {
                    command
                }
            }) if matches!(
                *command,
                WorkflowAuthorCommand::ImportRevision {
                    revision: 2,
                    activate: true,
                    expected_active_revision: Some(1),
                    timeout_ms: 5000,
                    ..
                }
            )
        ));
    }

    #[test]
    fn doctor_scan_report_serializes_machine_clean_actionable_json() {
        let report = SessionDoctorScanReport {
            historical_storage: bcode_session_migration::HistoricalStorageDiagnosis {
                root: std::path::PathBuf::from("/state/sessions-v5"),
                status: bcode_session_migration::HistoricalStorageDiagnosisStatus::Ok,
                inspection: bcode_session_migration::HistoricalStorageInspectionReport::default(),
                notes: Vec::new(),
            },
            category_counts: BTreeMap::from([(SessionDoctorCategory::MigrationRequired, 1)]),
            category_summaries: BTreeMap::from([(
                SessionDoctorCategory::MigrationRequired,
                SessionDoctorCategorySummary {
                    count: 1,
                    action: SessionDoctorAction::Migrate,
                    retryable: false,
                    samples: Vec::new(),
                },
            )]),
            findings: Vec::new(),
            reports: Vec::new(),
        };
        let mut output = Vec::new();

        write_json_line(&mut output, &report).expect("doctor JSON");
        let value: serde_json::Value = serde_json::from_slice(&output).expect("machine-clean JSON");

        assert_eq!(value["category_counts"]["migration_required"], 1);
        assert_eq!(
            value["category_summaries"]["migration_required"]["action"],
            "migrate"
        );
    }

    #[test]
    fn json_writer_treats_broken_downstream_pipe_as_clean_termination_signal() {
        struct BrokenPipe;

        impl std::io::Write for BrokenPipe {
            fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let error = write_json_line(&mut BrokenPipe, &serde_json::json!({ "ok": true }))
            .expect_err("closed pipe");
        assert_eq!(error.io_error_kind(), Some(std::io::ErrorKind::BrokenPipe));
    }

    #[test]
    fn backfill_wait_recovery_reads_status_only_for_the_same_daemon() {
        let timeout = Err(ClientError::RequestTimeout {
            timeout: Duration::from_secs(35),
        });
        assert_eq!(
            classify_session_backfill_wait_recovery(&timeout, "daemon-a", Some("daemon-a")),
            SessionBackfillWaitRecovery::ReadStatus
        );
        assert_eq!(
            classify_session_backfill_wait_recovery(&timeout, "daemon-a", Some("daemon-b")),
            SessionBackfillWaitRecovery::ReinvokeAfterRestart
        );
        let completed = Ok(bcode_session_search::SessionSearchBackfillOperationStatus {
            operation_id: "operation".to_owned(),
            provider_id: "provider".to_owned(),
            revision: 1,
            state: bcode_session_search::SessionSearchBackfillOperationState::Completed,
            response: None,
            complete_progress: None,
            complete_response: None,
            error: None,
        });
        assert_eq!(
            classify_session_backfill_wait_recovery(&completed, "daemon-a", Some("daemon-b")),
            SessionBackfillWaitRecovery::ReturnWait
        );
    }

    #[test]
    fn unchanged_backfill_revision_is_not_printed_twice() {
        let mut last_printed_revision = None;
        let status = bcode_session_search::SessionSearchBackfillOperationStatus {
            operation_id: "operation".to_owned(),
            provider_id: "provider".to_owned(),
            revision: 3,
            state: bcode_session_search::SessionSearchBackfillOperationState::Running,
            response: None,
            complete_progress: None,
            complete_response: None,
            error: None,
        };
        print_session_search_backfill_operation_if_changed(
            &status,
            true,
            &mut last_printed_revision,
        )
        .expect("first revision prints");
        assert_eq!(last_printed_revision, Some(3));
        print_session_search_backfill_operation_if_changed(
            &status,
            true,
            &mut last_printed_revision,
        )
        .expect("duplicate revision is ignored");
        assert_eq!(last_printed_revision, Some(3));
    }

    #[test]
    fn session_search_maintenance_commands_require_provider_confirmation() {
        let purge = Cli::try_parse_from([
            "bcode",
            "session",
            "search-purge",
            "--provider",
            "bcode.tantivy-session-search",
            "--confirm",
            "purge-bcode.tantivy-session-search",
            "--json",
        ])
        .expect("purge command should parse");
        assert!(matches!(
            purge.command,
            Some(Commands::Session {
                command: SessionCommand::SearchPurge { provider, confirm, json }
            }) if provider == "bcode.tantivy-session-search"
                && confirm == "purge-bcode.tantivy-session-search"
                && json
        ));

        let rebuild = Cli::try_parse_from([
            "bcode",
            "session",
            "search-rebuild",
            "--provider",
            "bcode.tantivy-session-search",
            "--confirm",
            "rebuild-bcode.tantivy-session-search",
        ])
        .expect("rebuild command should parse");
        assert!(matches!(
            rebuild.command,
            Some(Commands::Session {
                command: SessionCommand::SearchRebuild { provider, confirm, json }
            }) if provider == "bcode.tantivy-session-search"
                && confirm == "rebuild-bcode.tantivy-session-search"
                && !json
        ));
    }

    #[test]
    fn complete_backfill_terminal_result_rejects_cancelled_partial_and_failed_statuses() {
        let status = |state, response| bcode_session_search::SessionSearchBackfillOperationStatus {
            operation_id: "operation".to_owned(),
            provider_id: "all-enabled-providers".to_owned(),
            revision: 2,
            state,
            response: None,
            complete_progress: None,
            complete_response: response,
            error: None,
        };
        assert!(
            complete_backfill_terminal_result(&status(
                bcode_session_search::SessionSearchBackfillOperationState::Completed,
                None,
            ))
            .is_ok()
        );
        assert!(
            complete_backfill_terminal_result(&status(
                bcode_session_search::SessionSearchBackfillOperationState::Cancelled,
                None,
            ))
            .is_err()
        );
        for (state, exit) in [
            (
                bcode_session_search::SessionSearchBackfillOperationState::Cancelled,
                4,
            ),
            (
                bcode_session_search::SessionSearchBackfillOperationState::Failed,
                1,
            ),
            (
                bcode_session_search::SessionSearchBackfillOperationState::NeedsAttention,
                1,
            ),
            (
                bcode_session_search::SessionSearchBackfillOperationState::Running,
                1,
            ),
        ] {
            assert_eq!(
                complete_backfill_terminal_result(&status(state, None))
                    .expect_err("not complete")
                    .exit_code(),
                exit
            );
        }
        let partial = bcode_session_search::CompleteSessionSearchBackfillResponse {
            provider_ids: vec!["provider".to_owned()],
            catalog_revision_started: 1,
            catalog_revision_completed: 1,
            convergence_passes: 1,
            cancelled: false,
            providers: vec![
                bcode_session_search::CompleteSessionSearchBackfillProviderResult {
                    provider_id: "provider".to_owned(),
                    selected_sessions: 1,
                    completed_sessions: 0,
                    incomplete_sessions: 1,
                    failed_sessions: 0,
                    catalog_pages: 1,
                    issues: Vec::new(),
                    error: None,
                },
            ],
        };
        assert!(
            complete_backfill_terminal_result(&status(
                bcode_session_search::SessionSearchBackfillOperationState::Failed,
                Some(partial),
            ))
            .is_err()
        );
        assert!(
            complete_backfill_terminal_result(&status(
                bcode_session_search::SessionSearchBackfillOperationState::Failed,
                None,
            ))
            .is_err()
        );
    }

    #[test]
    fn session_search_backfill_parses_all_enabled_provider_scope() {
        let cli = Cli::try_parse_from(["bcode", "session", "search-backfill", "--json"])
            .expect("unscoped backfill command should parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Session {
                command: SessionCommand::SearchBackfill {
                    provider: None,
                    sessions,
                    cursor: None,
                    json: true,
                    ..
                }
            }) if sessions.is_empty()
        ));
    }

    #[test]
    fn session_search_backfill_parses_selected_and_time_bounded_scope() {
        let cli = Cli::try_parse_from([
            "bcode",
            "session",
            "search-backfill",
            "--provider",
            "bcode.tantivy-session-search",
            "--session",
            "00000000-0000-0000-0000-000000000001",
            "--after-timestamp-ms",
            "100",
            "--before-timestamp-ms",
            "200",
            "--deadline-ms",
            "5000",
            "--json",
        ])
        .expect("backfill command should parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Session {
                command: SessionCommand::SearchBackfill {
                    provider,
                    sessions,
                    after_timestamp_ms: Some(100),
                    before_timestamp_ms: Some(200),
                    cursor: None,
                    deadline_ms: 5000,
                    json: true,
                }
            }) if provider.as_deref() == Some("bcode.tantivy-session-search") && sessions.len() == 1
        ));
    }

    #[test]
    fn session_search_backfill_wait_parses_revision_and_timeout() {
        let cli = Cli::try_parse_from([
            "bcode",
            "session",
            "search-backfill-wait",
            "session-search-backfill-7",
            "--after-revision",
            "2",
            "--timeout-ms",
            "5000",
            "--json",
        ])
        .expect("backfill wait command should parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Session {
                command: SessionCommand::SearchBackfillWait {
                    operation_id,
                    after_revision: 2,
                    timeout_ms: 5000,
                    json: true,
                }
            }) if operation_id == "session-search-backfill-7"
        ));
    }

    #[test]
    fn session_search_command_parses_bounded_content_and_hydration_options() {
        let cli = Cli::try_parse_from([
            "bcode",
            "session",
            "search",
            "database locked",
            "--match",
            "phrase",
            "--field",
            "text",
            "--content",
            "user-message",
            "--content",
            "shell-output",
            "--limit",
            "15",
            "--deadline-ms",
            "1200",
            "--hydrate",
            "--deep",
            "--session",
            "00000000-0000-0000-0000-000000000001",
            "--working-directory",
            "/tmp/project",
            "--after-timestamp-ms",
            "1000",
            "--before-timestamp-ms",
            "2000",
            "--tool",
            "shell",
            "--tool-status",
            "failed",
            "--provider",
            "provider.test",
            "--model",
            "gpt-test",
            "--agent",
            "build",
            "--import-source",
            "opencode",
            "--json",
        ])
        .expect("search command should parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Session {
                command: SessionCommand::Search {
                    match_mode: SessionSearchMatchArg::Phrase,
                    fields,
                    content,
                    limit: 15,
                    deadline_ms: 1200,
                    hydrate: true,
                    deep: true,
                    sessions,
                    working_directory: Some(working_directory),
                    after_timestamp_ms: Some(1000),
                    before_timestamp_ms: Some(2000),
                    tools,
                    tool_statuses,
                    providers,
                    models,
                    agents,
                    import_sources,
                    json: true,
                    ..
                }
            }) if fields.len() == 1
                && content.len() == 2
                && sessions.len() == 1
                && working_directory == Path::new("/tmp/project")
                && tools == ["shell"]
                && tool_statuses == ["failed"]
                && providers == ["provider.test"]
                && models == ["gpt-test"]
                && agents == ["build"]
                && import_sources == ["opencode"]
        ));
    }

    #[test]
    fn session_search_scope_builds_backend_neutral_filters_and_policy() {
        let session_id = SessionId::new();
        let scope = SessionSearchCliScope {
            deep: true,
            sessions: vec![session_id],
            working_directory: Some(PathBuf::from("/tmp/project")),
            after_timestamp_ms: Some(10),
            before_timestamp_ms: Some(20),
            tools: vec!["shell".to_owned()],
            tool_statuses: vec!["failed".to_owned()],
            providers: vec!["provider".to_owned()],
            models: vec!["model".to_owned()],
            agents: vec!["agent".to_owned()],
            import_sources: vec!["opencode".to_owned()],
        };
        let policy = scope.policy(1_500);
        assert_eq!(
            policy.execution_class,
            bcode_session_search::SessionSearchExecutionClass::Deep
        );
        assert_eq!(policy.per_provider_deadline_ms, 1_500);

        let filters = scope.filters(vec![SessionSearchContentArg::ToolError]);
        assert_eq!(filters.session_ids, BTreeSet::from([session_id]));
        assert_eq!(
            filters.working_directory.as_deref(),
            Some(Path::new("/tmp/project"))
        );
        assert_eq!(filters.after_timestamp_ms, Some(10));
        assert_eq!(filters.before_timestamp_ms, Some(20));
        assert_eq!(filters.tool_names, BTreeSet::from(["shell".to_owned()]));
        assert_eq!(filters.tool_statuses, BTreeSet::from(["failed".to_owned()]));
        assert_eq!(filters.providers, BTreeSet::from(["provider".to_owned()]));
        assert_eq!(filters.models, BTreeSet::from(["model".to_owned()]));
        assert_eq!(filters.agents, BTreeSet::from(["agent".to_owned()]));
        assert_eq!(filters.sources, BTreeSet::from(["opencode".to_owned()]));
        assert_eq!(
            filters.content_kinds,
            BTreeSet::from([bcode_session_search::SearchContentKind::ToolError])
        );
    }

    #[test]
    fn session_search_automation_outcomes_are_distinct() {
        use bcode_session_search::{
            FederatedProviderReport, FederatedSessionSearchResponse, ProviderSearchOutcome,
        };

        let no_provider = FederatedSessionSearchResponse {
            hits: Vec::new(),
            query_complete: false,
            coverage_complete: false,
            providers: Vec::new(),
            failures: Vec::new(),
        };
        assert_eq!(
            session_search_cli_outcome(&no_provider, &[]),
            "no_eligible_provider"
        );

        let no_results = FederatedSessionSearchResponse {
            hits: Vec::new(),
            query_complete: true,
            coverage_complete: true,
            providers: vec![FederatedProviderReport {
                provider_id: "provider".to_owned(),
                outcome: ProviderSearchOutcome::Complete,
                elapsed_ms: 1,
                query_complete: true,
                coverage_complete: true,
                next_cursor: None,
                searched_content: Vec::new(),
                excluded_content: Vec::new(),
            }],
            failures: Vec::new(),
        };
        assert_eq!(session_search_cli_outcome(&no_results, &[]), "no_results");

        let incomplete = FederatedSessionSearchResponse {
            coverage_complete: false,
            ..no_results
        };
        assert_eq!(
            session_search_cli_outcome(&incomplete, &[]),
            "incomplete_coverage"
        );
    }

    #[test]
    fn structured_session_inspection_command_parses() {
        let session_id = SessionId::new();
        let cli = Cli::try_parse_from([
            "bcode",
            "session",
            "inspect",
            &session_id.to_string(),
            "failed-tool-calls",
            "--after",
            "20",
            "--limit",
            "15",
            "--json",
        ])
        .expect("inspection command should parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Session {
                command: SessionCommand::Inspect {
                    category: SessionInspectionCategoryArg::FailedToolCalls,
                    after: Some(20),
                    before: None,
                    limit: 15,
                    json: true,
                    ..
                }
            })
        ));
    }

    #[test]
    fn bounded_session_history_command_parses() {
        let session_id = SessionId::new();
        let cli = Cli::try_parse_from([
            "bcode",
            "session",
            "history",
            &session_id.to_string(),
            "--after",
            "40",
            "--limit",
            "25",
            "--json",
        ])
        .expect("bounded history command should parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Session {
                command: SessionCommand::History {
                    after: Some(40),
                    before: None,
                    limit: 25,
                    json: true,
                    ..
                }
            })
        ));
    }

    #[test]
    fn session_around_command_parses() {
        let session_id = SessionId::new();
        let cli = Cli::try_parse_from([
            "bcode",
            "session",
            "around",
            &session_id.to_string(),
            "42",
            "--before",
            "5",
            "--after",
            "7",
            "--json",
        ])
        .expect("around command should parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Session {
                command: SessionCommand::Around {
                    sequence: 42,
                    before: 5,
                    after: 7,
                    json: true,
                    ..
                }
            })
        ));
    }

    #[test]
    fn session_read_owner_identity_requires_exact_daemon_match() {
        let record = bcode_daemon_lifecycle::DaemonRecord {
            schema_version: bcode_daemon_lifecycle::DAEMON_RECORD_SCHEMA_VERSION,
            namespace: "owner".to_owned(),
            protocol_version: 15,
            artifact_id: Some(bcode_ipc::ArtifactId::current()),
            build_fingerprint: "build".to_owned(),
            executable_digest: Some("digest".to_owned()),
            endpoint: bcode_daemon_lifecycle::DaemonEndpointRecord::UnixSocket {
                path: PathBuf::from("/tmp/owner.sock"),
            },
            storage_writer_epoch: Some(5),
            state_location_id: Some(bcode_ipc::state_location_id()),
            pid: Some(42),
            instance_id: "instance".to_owned(),
            log_path: PathBuf::from("/tmp/owner.log"),
            executable_path: Some(PathBuf::from("/tmp/bcode")),
            started_at_unix_ms: 1,
            last_seen_unix_ms: 1,
        };
        let matching = bcode_ipc::DaemonStatus {
            namespace: "owner".to_owned(),
            protocol_version: 15,
            artifact_id: Some(bcode_ipc::ArtifactId::current()),
            build_fingerprint: "build".to_owned(),
            executable_digest: Some("digest".to_owned()),
            storage_writer_epoch: Some(5),
            state_location_id: Some(bcode_ipc::state_location_id()),
            session_event_schema_version: Some(
                bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            ),
            pid: Some(42),
            instance_id: "instance".to_owned(),
            started_at_unix_ms: 1,
        };
        assert!(daemon_status_matches(&record, &matching));
        assert!(!daemon_status_matches(
            &record,
            &bcode_ipc::DaemonStatus {
                instance_id: "replacement".to_owned(),
                ..matching
            }
        ));
    }

    #[tokio::test]
    async fn destructive_server_commands_require_explicit_confirmation() {
        let unconfirmed = Cli::try_parse_from(["bcode", "server", "stop", "--force"])
            .expect("unconfirmed force stop parses before side-effect validation");
        let Some(Commands::Server { command }) = unconfirmed.command else {
            panic!("server command");
        };
        assert!(matches!(
            handle_server_command(command).await,
            Err(CliError::InvalidArguments(message))
                if message == "forced server stop requires --yes"
        ));

        let stop = Cli::try_parse_from(["bcode", "server", "stop", "--force", "--yes"])
            .expect("confirmed forced stop command should parse");
        assert!(matches!(
            stop.command,
            Some(Commands::Server {
                command: ServerCommand::Stop {
                    force: true,
                    yes: true,
                }
            })
        ));

        let stop_all = Cli::try_parse_from(["bcode", "server", "stop-all", "--yes"])
            .expect("confirmed stop-all command should parse");
        assert!(matches!(
            stop_all.command,
            Some(Commands::Server {
                command: ServerCommand::StopAll { yes: true }
            })
        ));
    }

    #[test]
    fn foreground_server_logging_requires_direct_server_run() {
        assert!(foreground_server_requested_from(
            ["bcode", "server", "run"],
            false
        ));
        assert!(!foreground_server_requested_from(
            ["bcode", "server", "run"],
            true
        ));
        assert!(!foreground_server_requested_from(
            ["bcode", "run", "server"],
            false
        ));
        assert!(!foreground_server_requested_from(
            ["bcode", "server", "start"],
            false
        ));
    }

    #[test]
    fn retire_incompatible_server_command_requires_confirmation() {
        let cli = Cli::try_parse_from(["bcode", "server", "retire-incompatible", "--yes"])
            .expect("confirmed retirement command should parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Server {
                command: ServerCommand::RetireIncompatible { yes: true }
            })
        ));
    }

    #[test]
    fn daemon_status_match_requires_full_storage_identity() {
        let record = bcode_daemon_lifecycle::DaemonRecord {
            schema_version: bcode_daemon_lifecycle::DAEMON_RECORD_SCHEMA_VERSION,
            namespace: "namespace".to_string(),
            protocol_version: 9,
            artifact_id: Some(bcode_ipc::ArtifactId::current()),
            build_fingerprint: "build".to_string(),
            storage_writer_epoch: Some(2),
            state_location_id: Some(bcode_ipc::state_location_id()),
            pid: Some(1),
            endpoint: bcode_daemon_lifecycle::DaemonEndpointRecord::Unknown {
                debug: "test".to_string(),
            },
            log_path: PathBuf::from("test.log"),
            executable_path: None,
            executable_digest: Some("digest".to_string()),
            started_at_unix_ms: 0,
            last_seen_unix_ms: 0,
            instance_id: "instance".to_string(),
        };
        let matching = bcode_ipc::DaemonStatus {
            namespace: "namespace".to_string(),
            protocol_version: 9,
            artifact_id: Some(bcode_ipc::ArtifactId::current()),
            build_fingerprint: "build".to_string(),
            executable_digest: Some("digest".to_string()),
            storage_writer_epoch: Some(2),
            state_location_id: Some(bcode_ipc::state_location_id()),
            session_event_schema_version: Some(38),
            pid: Some(1),
            instance_id: "instance".to_string(),
            started_at_unix_ms: 0,
        };
        assert!(daemon_status_matches(&record, &matching));
        assert!(!daemon_status_matches(
            &record,
            &bcode_ipc::DaemonStatus {
                instance_id: "reused-endpoint".to_string(),
                ..matching.clone()
            }
        ));
        assert!(!daemon_status_matches(
            &record,
            &bcode_ipc::DaemonStatus {
                storage_writer_epoch: Some(1),
                ..matching
            }
        ));
    }

    #[test]
    fn daemon_control_policy_never_spawns_protocol_unsupported_or_ambiguous_records() {
        use bcode_daemon_lifecycle::DaemonRecordClassification as Classification;

        assert_eq!(
            daemon_control_policy(Classification::CurrentHealthy),
            DaemonControlPolicy::GracefulIpc
        );
        assert_eq!(
            daemon_control_policy(Classification::HistoricalExactResponsive),
            DaemonControlPolicy::GracefulIpc
        );
        assert_eq!(
            daemon_control_policy(Classification::HistoricalProcessVerifiedProtocolUnsupported),
            DaemonControlPolicy::DelegatedGraceful
        );
        assert_eq!(
            daemon_control_policy(Classification::ResponsiveIdentityMismatch),
            DaemonControlPolicy::PreserveAndRefuse
        );
        assert_eq!(
            daemon_control_policy(Classification::Unverifiable),
            DaemonControlPolicy::PreserveAndRefuse
        );
        assert_eq!(
            daemon_control_policy(Classification::UnreachableStale),
            DaemonControlPolicy::PruneStale
        );
    }

    #[test]
    fn release_owner_command_parses_session_id() {
        let session_id = SessionId::new();
        let cli =
            Cli::try_parse_from(["bcode", "session", "release-owner", &session_id.to_string()])
                .expect("release-owner command should parse");
        let Some(Commands::Session {
            command: SessionCommand::ReleaseOwner { session_id: parsed },
        }) = cli.command
        else {
            panic!("expected release-owner command");
        };

        assert_eq!(parsed, session_id);
    }

    #[test]
    fn release_owner_messages_cover_success_and_all_blockers() {
        for outcome in [
            bcode_ipc::SessionOwnershipReleaseOutcome::Released,
            bcode_ipc::SessionOwnershipReleaseOutcome::AlreadyUnowned,
        ] {
            assert_eq!(
                session_ownership_release_message("daemon-1", outcome)
                    .expect("successful release message"),
                "released session ownership from daemon daemon-1"
            );
        }

        let error = session_ownership_release_message(
            "daemon-1",
            bcode_ipc::SessionOwnershipReleaseOutcome::Blocked {
                blockers: vec![
                    bcode_ipc::SessionOwnershipBlocker::AttachedClient,
                    bcode_ipc::SessionOwnershipBlocker::PendingAttach,
                    bcode_ipc::SessionOwnershipBlocker::QueuedCommand,
                    bcode_ipc::SessionOwnershipBlocker::ActiveRuntime,
                    bcode_ipc::SessionOwnershipBlocker::RuntimeWork,
                    bcode_ipc::SessionOwnershipBlocker::PluginInvocation,
                    bcode_ipc::SessionOwnershipBlocker::Migration,
                    bcode_ipc::SessionOwnershipBlocker::DatabaseHandleRetained,
                ],
            },
        )
        .expect_err("blocked release should be an error");
        assert_eq!(
            error.to_string(),
            "invalid arguments: daemon daemon-1 refused ownership release; blockers: attached client, pending attach, queued command, active runtime, runtime work, plugin invocation, migration, retained session database handle"
        );
    }

    #[tokio::test]
    async fn locked_session_diagnosis_reports_verified_holder_candidates_without_opening_database()
    {
        // A lock can outlive its lease record, so lease-based owner resolution can report no owner
        // while the database is still locked. Diagnosis must then surface verified live-daemon
        // evidence instead of dead-ending, and must not open or mutate canonical storage.
        let root = tempfile::tempdir().expect("diagnosis root");
        let session_id = SessionId::new();
        let session_dir = root.path().join(session_id.to_string());
        std::fs::create_dir_all(&session_dir).expect("session dir");
        let db_path = session_dir.join("session.db");
        std::fs::write(&db_path, b"canonical-bytes").expect("database fixture");
        let before = std::fs::read(&db_path).expect("database bytes before");

        let diagnosis = collect_session_locked_diagnosis(
            session_id,
            root.path(),
            "Locking error: Failed locking file 'session.db-wal'. File is locked by another process",
        )
        .await
        .expect("locked diagnosis should be produced");

        assert_eq!(diagnosis.session_id, session_id);
        assert_eq!(diagnosis.database_path, db_path);
        assert!(diagnosis.lock_error.contains("locked by another process"));
        // No lease records exist for this fixture, which is exactly the dead-end case.
        assert_eq!(diagnosis.lease_named_owners, 0);
        assert!(diagnosis.owner_observations.is_empty());
        // Guidance must name a non-destructive next step and must not advise stopping a daemon for
        // being old, because concurrent build-specific daemons are intended.
        assert!(diagnosis.recovery_guidance.contains("release-owner"));
        assert!(diagnosis.recovery_guidance.contains("not a stale owner"));
        // Every reported candidate must carry verified identity evidence.
        for candidate in &diagnosis.holder_candidates {
            assert!(
                matches!(
                    candidate.classification.as_str(),
                    "CurrentHealthy"
                        | "HistoricalExactResponsive"
                        | "HistoricalProcessVerifiedProtocolUnsupported"
                ),
                "unverified daemon evidence must not be reported as a holder: {}",
                candidate.classification
            );
        }

        assert_eq!(
            std::fs::read(&db_path).expect("database bytes after"),
            before,
            "locked diagnosis must not mutate canonical storage"
        );
    }

    #[cfg(not(feature = "web-renderer"))]
    #[test]
    fn web_command_is_absent_without_web_renderer_feature() {
        assert!(Cli::try_parse_from(["bcode", "web"]).is_err());
    }

    #[cfg(feature = "web-renderer")]
    #[test]
    fn web_command_defaults_to_loopback_without_external_opt_in() {
        let cli = Cli::try_parse_from(["bcode", "web"]).expect("web command should parse");
        let Some(Commands::Web {
            bind,
            port,
            allow_non_loopback,
        }) = cli.command
        else {
            panic!("expected web command");
        };

        assert_eq!(bind, bcode_hyperchad::DEFAULT_BIND_ADDRESS);
        assert_eq!(port, None);
        assert!(!allow_non_loopback);
    }

    #[cfg(feature = "web-renderer")]
    #[test]
    fn web_command_parses_explicit_external_bind_opt_in() {
        let cli = Cli::try_parse_from([
            "bcode",
            "web",
            "--bind",
            "0.0.0.0",
            "--port",
            "4321",
            "--allow-non-loopback",
        ])
        .expect("external web bind should parse with opt-in");
        let Some(Commands::Web {
            bind,
            port,
            allow_non_loopback,
        }) = cli.command
        else {
            panic!("expected web command");
        };

        assert_eq!(
            bind,
            "0.0.0.0"
                .parse::<std::net::IpAddr>()
                .expect("address should parse")
        );
        assert_eq!(port, Some(4321));
        assert!(allow_non_loopback);
    }
}

#[cfg(test)]
mod workflow_source_tests {
    use super::*;

    fn workflow_test_catalog(
        agent_profiles: std::collections::BTreeSet<String>,
    ) -> bcode_workflow::WorkflowAuthoringCatalogSnapshot {
        let shell_manifest: bcode_plugin::PluginManifest = toml::from_str(include_str!(
            "../../../plugins/shell-plugin/bcode-plugin.toml"
        ))
        .expect("shell plugin manifest");
        let shell = shell_manifest
            .services
            .iter()
            .find(|service| service.interface_id == bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID)
            .expect("shell workflow service");
        bcode_workflow::WorkflowAuthoringCatalogSnapshot {
            version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
                &bcode_workflow::WorkflowProductionCapabilities::current(),
            ),
            plugins: std::collections::BTreeSet::from(["bcode.shell".to_string()]),
            blocks: shell
                .workflow_blocks
                .iter()
                .cloned()
                .map(|block| (bcode_workflow::workflow_block_catalog_key(&block), block))
                .collect(),
            node_configuration_schemas: bcode_workflow::workflow_node_configuration_schemas(),
            workflow_definitions: std::collections::BTreeMap::new(),
            agent_profiles,
            authoring_actions: shell
                .workflow_authoring_actions
                .iter()
                .cloned()
                .map(|action| (action.catalog_key(), action))
                .collect(),
        }
    }

    #[test]
    fn primary_cli_apply_loader_preserves_source_v3_shorthand_and_infers_yaml() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let yaml = root.join("fixtures/workflows/concise-run.workflow.yaml");
        let loaded = read_workflow_source_file(&yaml, None).expect("source-v3 YAML source");
        assert_eq!(
            loaded.source_format,
            bcode_workflow::WorkflowSourceFormat::Yaml
        );
        assert!(loaded.source.contains("workflow_source_version: 3"));
        assert!(loaded.source.contains("run: printf 'first\\n'"));
        assert!(!loaded.source.contains("fixtures/workflows"));
        assert!(read_workflow_source_file(&yaml, Some("xml")).is_err());
    }

    #[test]
    fn package_manifest_parse_errors_do_not_echo_contents() {
        let temp = tempfile::tempdir().expect("fixture");
        for (extension, source) in [
            ("json", r#"{"version":"secret-marker-123"}"#),
            ("yaml", "version: secret-marker-123\n"),
            ("toml", "version = \"secret-marker-123\"\n"),
        ] {
            let path = temp.path().join(format!("package.{extension}"));
            std::fs::write(&path, source).unwrap();
            let error = read_workflow_package_manifest(&path).unwrap_err();
            assert!(error.to_string().contains("invalid workflow package"));
            assert!(!error.to_string().contains("secret-marker-123"));
            assert!(!format!("{error:?}").contains("secret-marker-123"));
            assert_eq!(std::fs::read_to_string(path).unwrap(), source);
        }
    }

    #[test]
    fn package_file_reads_enforce_manifest_and_combined_source_bounds() {
        let temp = tempfile::tempdir().expect("fixture");
        let manifest = temp.path().join("package.yaml");
        let limit = bcode_workflow::MAX_WORKFLOW_PACKAGE_SOURCE_BYTES;
        std::fs::write(&manifest, vec![b' '; limit + 1]).unwrap();
        let error = read_workflow_package_manifest(&manifest).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("workflow package manifest exceeds")
        );
        std::fs::write(&manifest, "version: 3\npackage_id: example/package\nexports:\n  main: first\nmembers:\n  - member_id: first\n    source_name: first.yaml\n  - member_id: second\n    source_name: second.yaml\n").unwrap();
        std::fs::write(temp.path().join("first.yaml"), vec![b' '; limit]).unwrap();
        std::fs::write(temp.path().join("second.yaml"), b"x").unwrap();
        let error = read_workflow_package_manifest(&manifest).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("workflow package sources exceeds 0 bytes")
        );
    }

    #[test]
    fn primary_cli_confines_and_loads_package_member_sources() {
        let temp = tempfile::tempdir().expect("tempdir");
        let member = temp.path().join("member.workflow.yaml");
        std::fs::write(
            &member,
            "workflow_source_version: 3\nworkflow_id: example/member\ntitle: Member\nsteps:\n  - id: input\n    input:\n      schema:\n        type_name: value/v1\n        schema:\n          type: string\n",
        )
        .expect("member");
        let manifest = temp.path().join("workflow-package.yaml");
        std::fs::write(
            &manifest,
            "version: 3\npackage_id: example/package\nexports:\n  main: member\nmembers:\n  - member_id: member\n    source_name: member.workflow.yaml\n",
        )
        .expect("manifest");
        let loaded = read_workflow_package_manifest(&manifest).expect("package");
        assert_eq!(loaded.members.len(), 1);
        assert_eq!(loaded.members[0].source_name, "member.workflow.yaml");
        assert_eq!(
            loaded.members[0].format,
            bcode_workflow::WorkflowSourceFormat::Yaml
        );
        assert!(loaded.members[0].source.contains("workflow_source_version"));

        std::fs::write(
            &manifest,
            "version: 3\npackage_id: example/package\nexports:\n  main: member\nmembers:\n  - member_id: member\n    source_name: ../outside.yaml\n",
        )
        .expect("escaping manifest");
        assert!(read_workflow_package_manifest(&manifest).is_err());
    }

    #[test]
    fn primary_cli_loads_generic_source_component_package() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest = root.join("fixtures/workflow-components/package.workflow-package.yaml");
        let loaded = read_workflow_package_manifest(&manifest).expect("component package");
        loaded.validate().expect("valid component package manifest");
        assert_eq!(loaded.package_id, "bcode/generic-source-components");
        assert_eq!(loaded.members.len(), 14);
        assert!(loaded.exports.contains_key("run-command-and-assert"));
        assert!(loaded.exports.contains_key("non-git-data-quality"));
        assert!(
            loaded
                .members
                .iter()
                .all(|member| !member.source.is_empty())
        );

        let shell_manifest: bcode_plugin::PluginManifest = toml::from_str(include_str!(
            "../../../plugins/shell-plugin/bcode-plugin.toml"
        ))
        .expect("shell plugin manifest");
        let service = shell_manifest
            .services
            .iter()
            .find(|service| service.interface_id == bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID)
            .expect("shell workflow service");
        let catalog = bcode_workflow::WorkflowAuthoringCatalogSnapshot {
            version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
                &bcode_workflow::WorkflowProductionCapabilities::current(),
            ),
            plugins: std::collections::BTreeSet::from(["bcode.shell".to_string()]),
            blocks: service
                .workflow_blocks
                .iter()
                .cloned()
                .map(|block| (bcode_workflow::workflow_block_catalog_key(&block), block))
                .collect(),
            node_configuration_schemas: bcode_workflow::workflow_node_configuration_schemas(),
            workflow_definitions: std::collections::BTreeMap::new(),
            agent_profiles: std::collections::BTreeSet::from([
                "build".to_string(),
                "review".to_string(),
            ]),
            authoring_actions: service
                .workflow_authoring_actions
                .iter()
                .cloned()
                .map(|action| (action.catalog_key(), action))
                .collect(),
        };
        for member_id in ["run-command-and-assert", "completion-evaluation"] {
            let member = loaded
                .members
                .iter()
                .find(|member| member.member_id == member_id)
                .expect("component member");
            bcode_workflow::lower_workflow_authoring_source(
                &member.source,
                member.format,
                &catalog,
            )
            .expect("component lowers through ordinary source contract");
        }
    }

    #[test]
    fn primary_cli_loads_and_plans_product_facing_typed_command_package() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest =
            root.join("examples/workflows/packages/command/package.workflow-package.yaml");
        let loaded = read_workflow_package_manifest(&manifest).expect("command package");
        loaded.validate().expect("valid command package manifest");
        assert_eq!(loaded.package_id, "bcode/examples-command");
        assert_eq!(loaded.exports["run-and-assert"], "run-and-assert");

        let shell_manifest: bcode_plugin::PluginManifest = toml::from_str(include_str!(
            "../../../plugins/shell-plugin/bcode-plugin.toml"
        ))
        .expect("shell plugin manifest");
        let service = shell_manifest
            .services
            .iter()
            .find(|service| service.interface_id == bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID)
            .expect("shell workflow service");
        let catalog = bcode_workflow::WorkflowAuthoringCatalogSnapshot {
            version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
                &bcode_workflow::WorkflowProductionCapabilities::current(),
            ),
            plugins: std::collections::BTreeSet::from(["bcode.shell".to_string()]),
            blocks: service
                .workflow_blocks
                .iter()
                .cloned()
                .map(|block| (bcode_workflow::workflow_block_catalog_key(&block), block))
                .collect(),
            node_configuration_schemas: bcode_workflow::workflow_node_configuration_schemas(),
            workflow_definitions: std::collections::BTreeMap::new(),
            agent_profiles: std::collections::BTreeSet::new(),
            authoring_actions: service
                .workflow_authoring_actions
                .iter()
                .cloned()
                .map(|action| (action.catalog_key(), action))
                .collect(),
        };
        let plan = bcode_workflow::plan_workflow_package(&loaded, &catalog)
            .expect("typed command package plans");
        assert_eq!(plan.members.len(), 1);
        let compiled = plan.members[0]
            .lowering
            .document
            .compilation_preview(&catalog, None)
            .compiled
            .expect("typed command package compiles");
        assert_eq!(compiled.definition.input.type_name, "bcode.shell.exec/v1");
        assert_eq!(
            compiled.definition.output.type_name,
            "bcode.shell.exec-result/v1"
        );
    }

    #[test]
    fn primary_cli_resolves_product_facing_recursive_package_import() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest = root.join("examples/workflows/packages/validation.workflow-package.yaml");
        let closure = read_workflow_package_closure(&manifest).expect("validation closure");
        assert_eq!(closure.entry_package_id, "bcode/examples-validation");
        assert_eq!(closure.packages.len(), 2);
        assert!(closure.packages.iter().any(|package| {
            package.manifest.package_id == "bcode/examples-command"
                && package.manifest.imports.is_empty()
        }));
        let validation = closure
            .packages
            .iter()
            .find(|package| package.manifest.package_id == "bcode/examples-validation")
            .expect("validation package");
        assert_eq!(validation.manifest.imports.len(), 1);
        assert_eq!(validation.manifest.imports[0].import_id, "command");
        assert_eq!(
            validation.manifest.members[0].external_dependencies,
            ["command"]
        );

        let shell_manifest: bcode_plugin::PluginManifest = toml::from_str(include_str!(
            "../../../plugins/shell-plugin/bcode-plugin.toml"
        ))
        .expect("shell plugin manifest");
        let service = shell_manifest
            .services
            .iter()
            .find(|service| service.interface_id == bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID)
            .expect("shell workflow service");
        let catalog = bcode_workflow::WorkflowAuthoringCatalogSnapshot {
            version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
                &bcode_workflow::WorkflowProductionCapabilities::current(),
            ),
            plugins: std::collections::BTreeSet::from(["bcode.shell".to_string()]),
            blocks: service
                .workflow_blocks
                .iter()
                .cloned()
                .map(|block| (bcode_workflow::workflow_block_catalog_key(&block), block))
                .collect(),
            node_configuration_schemas: bcode_workflow::workflow_node_configuration_schemas(),
            workflow_definitions: std::collections::BTreeMap::new(),
            agent_profiles: std::collections::BTreeSet::new(),
            authoring_actions: service
                .workflow_authoring_actions
                .iter()
                .cloned()
                .map(|action| (action.catalog_key(), action))
                .collect(),
        };
        let plan = bcode_workflow::plan_workflow_package_closure(&closure, &catalog)
            .expect("recursive product package plans");
        assert_eq!(plan.packages.len(), 2);
        let validation_plan = plan
            .packages
            .iter()
            .find(|package| package.package_id == "bcode/examples-validation")
            .expect("validation plan");
        let call: bcode_workflow::WorkflowCallConfiguration = serde_json::from_value(
            validation_plan.plan.members[0]
                .lowering
                .document
                .definition
                .nodes["commands"]
                .configuration
                .clone(),
        )
        .expect("resolved imported export call");
        let command_plan = plan
            .packages
            .iter()
            .find(|package| package.package_id == "bcode/examples-command")
            .expect("command plan");
        assert_eq!(
            call.target.definition_identity(),
            &command_plan.plan.members[0].definition_identity
        );
    }

    #[test]
    fn primary_cli_plans_product_facing_prompt_verification_package() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest =
            root.join("examples/workflows/packages/prompt-verification.workflow-package.yaml");
        let loaded = read_workflow_package_manifest(&manifest).expect("prompt package");
        assert_eq!(loaded.members.len(), 1);

        let shell_manifest: bcode_plugin::PluginManifest = toml::from_str(include_str!(
            "../../../plugins/shell-plugin/bcode-plugin.toml"
        ))
        .expect("shell plugin manifest");
        let shell = shell_manifest
            .services
            .iter()
            .find(|service| service.interface_id == bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID)
            .expect("shell workflow service");
        let catalog = bcode_workflow::WorkflowAuthoringCatalogSnapshot {
            version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
                &bcode_workflow::WorkflowProductionCapabilities::current(),
            ),
            plugins: std::collections::BTreeSet::from(["bcode.shell".to_string()]),
            blocks: shell
                .workflow_blocks
                .iter()
                .cloned()
                .map(|block| (bcode_workflow::workflow_block_catalog_key(&block), block))
                .collect(),
            node_configuration_schemas: bcode_workflow::workflow_node_configuration_schemas(),
            workflow_definitions: std::collections::BTreeMap::new(),
            agent_profiles: std::collections::BTreeSet::from(["build".to_string()]),
            authoring_actions: shell
                .workflow_authoring_actions
                .iter()
                .cloned()
                .map(|action| (action.catalog_key(), action))
                .collect(),
        };
        let plan = bcode_workflow::plan_workflow_package(&loaded, &catalog)
            .expect("prompt verification package plans");
        let member = &plan.members[0].lowering.document.definition;
        assert_eq!(
            member.input.type_name,
            "bcode.prompt-verification.request/v1"
        );
        assert_eq!(member.output.type_name, "bcode.shell.exec-result/v1");
        assert_eq!(
            member.nodes["implement"].kind,
            bcode_workflow::NodeKind::Agent
        );
        assert_eq!(
            member.nodes["verify"].kind,
            bcode_workflow::NodeKind::PluginBlock
        );
        assert!(
            member
                .edges
                .iter()
                .any(|edge| edge.from == "implement" && edge.to == "verify")
        );
    }

    #[test]
    fn primary_cli_plans_product_facing_isolated_review_package() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest = root.join("examples/workflows/packages/review.workflow-package.yaml");
        let loaded = read_workflow_package_manifest(&manifest).expect("review package");
        let catalog = bcode_workflow::WorkflowAuthoringCatalogSnapshot {
            version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
                &bcode_workflow::WorkflowProductionCapabilities::current(),
            ),
            plugins: std::collections::BTreeSet::new(),
            blocks: std::collections::BTreeMap::new(),
            node_configuration_schemas: bcode_workflow::workflow_node_configuration_schemas(),
            workflow_definitions: std::collections::BTreeMap::new(),
            agent_profiles: std::collections::BTreeSet::from([
                "plan".to_string(),
                "review".to_string(),
            ]),
            authoring_actions: std::collections::BTreeMap::new(),
        };
        let plan =
            bcode_workflow::plan_workflow_package(&loaded, &catalog).expect("review package plans");
        let definition = &plan.members[0].lowering.document.definition;
        assert_eq!(
            definition.nodes["correctness"].kind,
            bcode_workflow::NodeKind::Agent
        );
        assert_eq!(
            definition.nodes["security"].kind,
            bcode_workflow::NodeKind::Agent
        );
        assert_eq!(
            definition.nodes["aggregate"].kind,
            bcode_workflow::NodeKind::Parallel
        );
        for node_id in ["correctness", "security"] {
            let prompt: bcode_workflow::WorkflowPromptConfiguration =
                serde_json::from_value(definition.nodes[node_id].configuration.clone())
                    .expect("prompt configuration");
            assert_eq!(
                prompt.execution_target,
                bcode_workflow::PromptContextTarget::FreshIsolated
            );
            assert!(prompt.read_only);
        }
    }

    #[test]
    fn primary_cli_plans_product_facing_bounded_remediation_package() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest = root.join("examples/workflows/packages/remediation.workflow-package.yaml");
        let loaded = read_workflow_package_manifest(&manifest).expect("remediation package");
        let catalog = bcode_workflow::WorkflowAuthoringCatalogSnapshot {
            version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
                &bcode_workflow::WorkflowProductionCapabilities::current(),
            ),
            plugins: std::collections::BTreeSet::new(),
            blocks: std::collections::BTreeMap::new(),
            node_configuration_schemas: bcode_workflow::workflow_node_configuration_schemas(),
            workflow_definitions: std::collections::BTreeMap::new(),
            agent_profiles: std::collections::BTreeSet::from(["build".to_string()]),
            authoring_actions: std::collections::BTreeMap::new(),
        };
        let plan = bcode_workflow::plan_workflow_package(&loaded, &catalog)
            .expect("bounded remediation package plans");
        let definition = &plan.members[0].lowering.document.definition;
        assert_eq!(
            definition.nodes["remediate"].kind,
            bcode_workflow::NodeKind::Agent
        );
        assert_eq!(
            definition.nodes["remediate__repeat"].kind,
            bcode_workflow::NodeKind::Repeat
        );
        assert!(definition.edges.iter().any(|edge| {
            edge.from == "remediate__repeat"
                && edge.to == "remediate"
                && matches!(
                    edge.kind,
                    bcode_workflow::EdgeKind::Back {
                        max_iterations: 3,
                        ..
                    }
                )
        }));
        assert_eq!(definition.output.type_name, "bcode.remediation.state/v1");
    }

    #[test]
    fn primary_cli_plans_product_facing_repository_recovery_package() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest =
            root.join("examples/workflows/packages/repository-recovery.workflow-package.yaml");
        let loaded = read_workflow_package_manifest(&manifest).expect("conflict package");
        let shell_manifest: bcode_plugin::PluginManifest = toml::from_str(include_str!(
            "../../../plugins/shell-plugin/bcode-plugin.toml"
        ))
        .expect("shell plugin manifest");
        let shell = shell_manifest
            .services
            .iter()
            .find(|service| service.interface_id == bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID)
            .expect("shell workflow service");
        let catalog = bcode_workflow::WorkflowAuthoringCatalogSnapshot {
            version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
                &bcode_workflow::WorkflowProductionCapabilities::current(),
            ),
            plugins: std::collections::BTreeSet::from(["bcode.shell".to_string()]),
            blocks: shell
                .workflow_blocks
                .iter()
                .cloned()
                .map(|block| (bcode_workflow::workflow_block_catalog_key(&block), block))
                .collect(),
            node_configuration_schemas: bcode_workflow::workflow_node_configuration_schemas(),
            workflow_definitions: std::collections::BTreeMap::new(),
            agent_profiles: std::collections::BTreeSet::from(["build".to_string()]),
            authoring_actions: shell
                .workflow_authoring_actions
                .iter()
                .cloned()
                .map(|action| (action.catalog_key(), action))
                .collect(),
        };
        let plan = bcode_workflow::plan_workflow_package(&loaded, &catalog)
            .expect("conflict package plans");
        let definition = &plan.members[0].lowering.document.definition;
        let prompt: bcode_workflow::WorkflowPromptConfiguration =
            serde_json::from_value(definition.nodes["resolve"].configuration.clone())
                .expect("prompt");
        assert!(prompt.system_prompt.contains("resolve-conflicts"));
        assert_eq!(
            definition.nodes["verify"].kind,
            bcode_workflow::NodeKind::PluginBlock
        );
        assert!(loaded.members[0].source.contains("git ls-files --unmerged"));
    }

    #[test]
    fn primary_cli_plans_product_facing_planning_and_completion_exports() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest = root.join("examples/workflows/packages/planning.workflow-package.yaml");
        let loaded = read_workflow_package_manifest(&manifest).expect("planning package");
        let catalog = bcode_workflow::WorkflowAuthoringCatalogSnapshot {
            version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
                &bcode_workflow::WorkflowProductionCapabilities::current(),
            ),
            plugins: std::collections::BTreeSet::new(),
            blocks: std::collections::BTreeMap::new(),
            node_configuration_schemas: bcode_workflow::workflow_node_configuration_schemas(),
            workflow_definitions: std::collections::BTreeMap::new(),
            agent_profiles: std::collections::BTreeSet::from([
                "build".to_string(),
                "plan".to_string(),
                "review".to_string(),
            ]),
            authoring_actions: std::collections::BTreeMap::new(),
        };
        let plan = bcode_workflow::plan_workflow_package(&loaded, &catalog)
            .expect("planning package plans");
        assert_eq!(plan.members.len(), 2);
        let planning = plan
            .members
            .iter()
            .find(|member| member.member_id == "plan-or-refocus")
            .expect("planning member");
        let planning_definition = &planning.lowering.document.definition;
        assert_eq!(
            planning_definition.input.type_name,
            "bcode.planning.request/v1"
        );
        let planning_prompt: bcode_workflow::WorkflowPromptConfiguration =
            serde_json::from_value(planning_definition.nodes["plan"].configuration.clone())
                .expect("planning prompt");
        assert!(planning_prompt.system_prompt.contains("local-progress-doc"));
        assert!(
            planning_prompt
                .system_prompt
                .contains("refocus-progress-doc")
        );

        let completion = plan
            .members
            .iter()
            .find(|member| member.member_id == "evaluate-completion")
            .expect("completion member");
        let completion_definition = &completion.lowering.document.definition;
        assert_eq!(
            completion_definition.input.type_name,
            "bcode.completion.request/v1"
        );
        let completion_prompt: bcode_workflow::WorkflowPromptConfiguration =
            serde_json::from_value(
                completion_definition.nodes["evaluate"]
                    .configuration
                    .clone(),
            )
            .expect("completion prompt");
        assert_eq!(
            completion_prompt.execution_target,
            bcode_workflow::PromptContextTarget::FreshIsolated
        );
        assert!(completion_prompt.read_only);
    }

    #[test]
    fn primary_cli_plans_product_facing_configured_checkpoint_package() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest = root.join("examples/workflows/packages/checkpoint.workflow-package.yaml");
        let loaded = read_workflow_package_manifest(&manifest).expect("checkpoint package");
        let shell_manifest: bcode_plugin::PluginManifest = toml::from_str(include_str!(
            "../../../plugins/shell-plugin/bcode-plugin.toml"
        ))
        .expect("shell plugin manifest");
        let shell = shell_manifest
            .services
            .iter()
            .find(|service| service.interface_id == bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID)
            .expect("shell workflow service");
        let catalog = bcode_workflow::WorkflowAuthoringCatalogSnapshot {
            version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
                &bcode_workflow::WorkflowProductionCapabilities::current(),
            ),
            plugins: std::collections::BTreeSet::from(["bcode.shell".to_string()]),
            blocks: shell
                .workflow_blocks
                .iter()
                .cloned()
                .map(|block| (bcode_workflow::workflow_block_catalog_key(&block), block))
                .collect(),
            node_configuration_schemas: bcode_workflow::workflow_node_configuration_schemas(),
            workflow_definitions: std::collections::BTreeMap::new(),
            agent_profiles: std::collections::BTreeSet::new(),
            authoring_actions: shell
                .workflow_authoring_actions
                .iter()
                .cloned()
                .map(|action| (action.catalog_key(), action))
                .collect(),
        };
        let plan = bcode_workflow::plan_workflow_package(&loaded, &catalog)
            .expect("checkpoint package plans");
        assert_eq!(
            plan.members[0].lowering.document.definition.input.type_name,
            "bcode.shell.exec/v1"
        );
        assert!(
            loaded.members[0]
                .source
                .contains("example-message-replaced-by-typed-input")
        );
        assert!(
            loaded.members[0]
                .source
                .contains("example-pathspec-replaced-by-typed-input")
        );
        assert!(
            !loaded.members[0]
                .source
                .contains("local-composable-workflows-progress.md")
        );
    }

    #[test]
    fn primary_cli_plans_product_facing_normal_synchronization_package() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest =
            root.join("examples/workflows/packages/synchronization.workflow-package.yaml");
        let loaded = read_workflow_package_manifest(&manifest).expect("synchronization package");
        let shell_manifest: bcode_plugin::PluginManifest = toml::from_str(include_str!(
            "../../../plugins/shell-plugin/bcode-plugin.toml"
        ))
        .expect("shell plugin manifest");
        let shell = shell_manifest
            .services
            .iter()
            .find(|service| service.interface_id == bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID)
            .expect("shell workflow service");
        let catalog = bcode_workflow::WorkflowAuthoringCatalogSnapshot {
            version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
                &bcode_workflow::WorkflowProductionCapabilities::current(),
            ),
            plugins: std::collections::BTreeSet::from(["bcode.shell".to_string()]),
            blocks: shell
                .workflow_blocks
                .iter()
                .cloned()
                .map(|block| (bcode_workflow::workflow_block_catalog_key(&block), block))
                .collect(),
            node_configuration_schemas: bcode_workflow::workflow_node_configuration_schemas(),
            workflow_definitions: std::collections::BTreeMap::new(),
            agent_profiles: std::collections::BTreeSet::new(),
            authoring_actions: shell
                .workflow_authoring_actions
                .iter()
                .cloned()
                .map(|action| (action.catalog_key(), action))
                .collect(),
        };
        let plan = bcode_workflow::plan_workflow_package(&loaded, &catalog)
            .expect("synchronization package plans");
        assert_eq!(
            plan.members[0].lowering.document.definition.input.type_name,
            "bcode.shell.exec/v1"
        );
        assert!(loaded.members[0].source.contains("GIT_TERMINAL_PROMPT"));
        assert!(!loaded.members[0].source.contains("--force"));
        assert!(!loaded.members[0].source.contains("force-with-lease"));
    }

    #[test]
    fn primary_cli_plans_bounded_sync_recovery_from_narrow_imports() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest = root.join("examples/workflows/packages/sync-recovery.workflow-package.yaml");
        let closure = read_workflow_package_closure(&manifest).expect("recovery closure");
        let plan = bcode_workflow::plan_workflow_package_closure(
            &closure,
            &workflow_test_catalog(std::collections::BTreeSet::from(["build".to_string()])),
        )
        .expect("recovery closure plans");
        assert_eq!(plan.packages.len(), 3);
        let mut preview_catalog =
            workflow_test_catalog(std::collections::BTreeSet::from(["build".to_string()]));
        for package in &plan.packages {
            for member in &package.plan.members {
                preview_catalog.workflow_definitions.insert(
                    member.definition_identity.definition_id.clone(),
                    member.lowering.document.definition.clone(),
                );
            }
        }
        for package in &plan.packages {
            let preview = bcode_workflow::preview_workflow_package(
                &package.plan,
                &preview_catalog,
                &std::collections::BTreeMap::new(),
            )
            .expect("every recovery package independently previews");
            assert!(preview.is_compiled());
        }
        let entry = plan
            .packages
            .iter()
            .find(|package| package.package_id == "bcode/examples-sync-recovery")
            .expect("entry package");
        let definition = &entry.plan.members[0].lowering.document.definition;
        assert_eq!(
            definition.nodes["synchronize"].kind,
            bcode_workflow::NodeKind::WorkflowCall
        );
        assert_eq!(
            definition.nodes["recover"].kind,
            bcode_workflow::NodeKind::WorkflowCall
        );
        assert!(definition.edges.iter().any(|edge| {
            edge.from == "synchronize"
                && edge.to == "recover"
                && matches!(edge.kind, bcode_workflow::EdgeKind::Conditional { .. })
        }));
        let entry_source = closure
            .packages
            .iter()
            .find(|package| package.package_id == "bcode/examples-sync-recovery")
            .expect("entry source");
        assert_eq!(entry_source.manifest.imports.len(), 2);
        assert!(
            entry_source
                .manifest
                .imports
                .iter()
                .all(|import| import.export == "synchronize" || import.export == "resolve")
        );
    }

    #[test]
    fn primary_cli_plans_product_facing_delivery_import_closure() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest = root.join("examples/workflows/packages/delivery.workflow-package.yaml");
        let closure = read_workflow_package_closure(&manifest).expect("delivery closure");
        let plan = bcode_workflow::plan_workflow_package_closure(
            &closure,
            &workflow_test_catalog(std::collections::BTreeSet::from([
                "build".to_string(),
                "plan".to_string(),
                "review".to_string(),
            ])),
        )
        .expect("delivery closure plans");
        assert_eq!(plan.packages.len(), closure.packages.len());
        let entry = plan
            .packages
            .iter()
            .find(|package| package.package_id == "bcode/examples-delivery")
            .expect("delivery entry");
        let definition = &entry.plan.members[0].lowering.document.definition;
        for node_id in ["plan", "implement", "review", "completion"] {
            assert_eq!(
                definition.nodes[node_id].kind,
                bcode_workflow::NodeKind::WorkflowCall
            );
        }
        assert_eq!(
            definition.nodes["operator_decision"].kind,
            bcode_workflow::NodeKind::Approval
        );
    }

    #[test]
    fn primary_cli_plans_product_facing_non_repository_data_quality_closure() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest = root.join("examples/workflows/packages/data-quality.workflow-package.yaml");
        let closure = read_workflow_package_closure(&manifest).expect("data quality closure");
        let plan = bcode_workflow::plan_workflow_package_closure(
            &closure,
            &workflow_test_catalog(std::collections::BTreeSet::from([
                "build".to_string(),
                "plan".to_string(),
                "review".to_string(),
            ])),
        )
        .expect("data quality closure plans");
        assert_eq!(plan.packages.len(), 3);
        let entry = plan
            .packages
            .iter()
            .find(|package| package.package_id == "bcode/examples-data-quality")
            .expect("data quality entry");
        let definition = &entry.plan.members[0].lowering.document.definition;
        assert_eq!(
            definition.nodes["inspect"].kind,
            bcode_workflow::NodeKind::WorkflowCall
        );
        assert_eq!(
            definition.nodes["assess"].kind,
            bcode_workflow::NodeKind::Agent
        );
        assert_eq!(
            definition.nodes["operator_decision"].kind,
            bcode_workflow::NodeKind::Approval
        );
        assert_eq!(
            definition.nodes["remediate"].kind,
            bcode_workflow::NodeKind::WorkflowCall
        );
        let assessment: bcode_workflow::WorkflowPromptConfiguration =
            serde_json::from_value(definition.nodes["assess"].configuration.clone())
                .expect("assessment prompt");
        assert!(assessment.read_only);
        assert!(assessment.tool_allowlist.is_empty());
        assert!(
            !entry.plan.members[0]
                .lowering
                .document
                .definition
                .name
                .to_ascii_lowercase()
                .contains("git")
        );
    }

    fn assert_transitive_package_byte_boundary(root: &std::path::Path, child: &std::path::Path) {
        let child_source = std::fs::read(child.join("main.workflow.yaml")).expect("child source");
        let remaining = bcode_workflow::MAX_WORKFLOW_PACKAGE_SOURCE_BYTES - child_source.len();
        let manifest = root.join("package.workflow-package.yaml");
        std::fs::write(root.join("main.workflow.yaml"), vec![b' '; remaining]).unwrap();
        let exact = read_workflow_package_closure(&manifest).expect("exact transitive limit");
        assert_eq!(
            exact
                .packages
                .iter()
                .flat_map(|package| &package.manifest.members)
                .map(|member| member.source.len())
                .sum::<usize>(),
            bcode_workflow::MAX_WORKFLOW_PACKAGE_SOURCE_BYTES
        );
        std::fs::write(root.join("main.workflow.yaml"), vec![b' '; remaining + 1]).unwrap();
        let error = read_workflow_package_closure(&manifest).expect_err("one byte beyond limit");
        assert!(
            error
                .to_string()
                .contains("workflow package sources exceeds")
        );
        assert_eq!(
            std::fs::read(child.join("main.workflow.yaml")).unwrap(),
            child_source
        );
    }

    #[test]
    fn primary_cli_resolves_recursive_confined_package_manifests() {
        let temp = tempfile::tempdir().expect("tempdir");
        let write_package = |directory: &Path,
                             package_id: &str,
                             workflow_id: &str,
                             import: Option<(&str, &str)>| {
            std::fs::create_dir_all(directory).expect("package directory");
            let call = import.map_or_else(
                || {
                    "input:\n      schema:\n        type_name: value/v1\n        schema: {type: string}"
                        .to_string()
                },
                |(import_id, _)| format!("package_call: {{external: {import_id}}}"),
            );
            std::fs::write(
                directory.join("main.workflow.yaml"),
                format!(
                    "workflow_source_version: 3\nworkflow_id: {workflow_id}\ntitle: {package_id}\nsteps:\n  - id: main\n    {call}\n"
                ),
            )
            .expect("member");
            let import_yaml = import.map_or_else(String::new, |(import_id, manifest)| {
                format!(
                    "imports:\n  - import_id: {import_id}\n    package_id: child\n    export: main\n    manifest: {manifest}\n"
                )
            });
            let external = import.map_or_else(String::new, |(import_id, _)| {
                format!("    external_dependencies: [{import_id}]\n")
            });
            std::fs::write(
                directory.join("package.workflow-package.yaml"),
                format!(
                    "version: 3\npackage_id: {package_id}\nexports:\n  main: main\n{import_yaml}members:\n  - member_id: main\n    source_name: main.workflow.yaml\n{external}"
                ),
            )
            .expect("manifest");
        };
        let child = temp.path().join("child");
        write_package(&child, "child", "example/child", None);
        write_package(
            temp.path(),
            "root",
            "example/root",
            Some(("child", "child/package.workflow-package.yaml")),
        );
        let closure =
            read_workflow_package_closure(&temp.path().join("package.workflow-package.yaml"))
                .expect("recursive package closure");
        assert_eq!(closure.entry_package_id, "root");
        assert_eq!(closure.packages.len(), 2);
        assert!(
            closure
                .packages
                .iter()
                .any(|package| package.package_id == "child")
        );

        assert_transitive_package_byte_boundary(temp.path(), &child);

        std::fs::write(
            temp.path().join("main.workflow.yaml"),
            vec![b' '; bcode_workflow::MAX_WORKFLOW_PACKAGE_SOURCE_BYTES],
        )
        .expect("root consumes closure budget");
        let error =
            read_workflow_package_closure(&temp.path().join("package.workflow-package.yaml"))
                .expect_err("child must share the root source budget");
        assert!(
            error
                .to_string()
                .contains("workflow package sources exceeds 0 bytes")
        );
        write_package(
            temp.path(),
            "root",
            "example/root",
            Some(("child", "child/package.workflow-package.yaml")),
        );

        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(
            outside.path().join("package.workflow-package.yaml"),
            "version: 3\npackage_id: outside\nexports: {main: main}\nmembers: []\n",
        )
        .expect("outside manifest");
        std::fs::write(
            temp.path().join("package.workflow-package.yaml"),
            format!(
                "version: 3\npackage_id: root\nexports: {{main: main}}\nimports:\n  - import_id: outside\n    package_id: outside\n    export: main\n    manifest: {}\nmembers:\n  - member_id: main\n    source_name: main.workflow.yaml\n",
                outside.path().join("package.workflow-package.yaml").display()
            ),
        )
        .expect("escaping import");
        assert!(
            read_workflow_package_closure(&temp.path().join("package.workflow-package.yaml"))
                .is_err()
        );
    }

    #[test]
    fn primary_cli_loads_cross_format_external_package_closure() {
        let temp = tempfile::tempdir().expect("tempdir");
        let child = temp.path().join("child");
        std::fs::create_dir_all(&child).expect("child directory");
        std::fs::write(
            child.join("main.workflow.json"),
            serde_json::to_string(&serde_json::json!({
                "workflow_source_version": 3,
                "workflow_id": "example/child",
                "title": "Child",
                "steps": [{
                    "id": "main",
                    "input": {"schema": {"type_name": "value/v1", "schema": {"type": "string"}}}
                }]
            }))
            .expect("child source"),
        )
        .expect("child source file");
        std::fs::write(
            child.join("package.workflow-package.toml"),
            "version = 3\npackage_id = \"child\"\n[exports]\nmain = \"main\"\n[[members]]\nmember_id = \"main\"\nsource_name = \"main.workflow.json\"\n",
        )
        .expect("child manifest");
        std::fs::write(
            temp.path().join("main.workflow.yaml"),
            "workflow_source_version: 3\nworkflow_id: example/root\ntitle: Root\nsteps:\n  - id: main\n    package_call: {external: child}\n",
        )
        .expect("root source");
        std::fs::write(
            temp.path().join("package.workflow-package.yaml"),
            "version: 3\npackage_id: root\nexports: {main: main}\nimports:\n  - import_id: child\n    package_id: child\n    export: main\n    manifest: child/package.workflow-package.toml\nmembers:\n  - member_id: main\n    source_name: main.workflow.yaml\n    external_dependencies: [child]\n",
        )
        .expect("root manifest");
        let closure = read_workflow_package_closure_in_root(
            &temp.path().join("package.workflow-package.yaml"),
            Some(temp.path()),
        )
        .expect("cross-format closure");
        assert_eq!(closure.packages.len(), 2);
        let plan = bcode_workflow::plan_workflow_package_closure(
            &closure,
            &bcode_workflow::WorkflowAuthoringCatalogSnapshot {
                version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
                capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
                    &bcode_workflow::WorkflowProductionCapabilities::current(),
                ),
                plugins: std::collections::BTreeSet::new(),
                blocks: std::collections::BTreeMap::new(),
                authoring_actions: std::collections::BTreeMap::new(),
                node_configuration_schemas: std::collections::BTreeMap::new(),
                agent_profiles: std::collections::BTreeSet::new(),
                workflow_definitions: std::collections::BTreeMap::new(),
            },
        )
        .expect("cross-format plan");
        assert_eq!(plan.entry_package_id, "root");
    }

    #[test]
    #[cfg(unix)]
    fn primary_cli_rejects_symlink_escape_and_duplicate_package_identity() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(
            outside.path().join("outside.workflow.yaml"),
            "workflow_source_version: 3\nworkflow_id: outside\ntitle: Outside\nsteps:\n  - id: value\n    input:\n      schema: {type_name: value/v1, schema: {type: string}}\n",
        )
        .expect("outside source");
        symlink(
            outside.path().join("outside.workflow.yaml"),
            temp.path().join("escaped.workflow.yaml"),
        )
        .expect("source symlink");
        std::fs::write(
            temp.path().join("package.workflow-package.yaml"),
            "version: 3\npackage_id: root\nexports: {main: main}\nmembers:\n  - member_id: main\n    source_name: escaped.workflow.yaml\n",
        )
        .expect("manifest");
        assert!(
            read_workflow_package_closure_in_root(
                &temp.path().join("package.workflow-package.yaml"),
                Some(temp.path())
            )
            .is_err()
        );

        let source = "workflow_source_version: 3\nworkflow_id: duplicate\ntitle: Duplicate\nsteps:\n  - id: value\n    input:\n      schema: {type_name: value/v1, schema: {type: string}}\n";
        for directory in [temp.path().join("one"), temp.path().join("two")] {
            std::fs::create_dir_all(&directory).expect("directory");
            std::fs::write(directory.join("main.workflow.yaml"), source).expect("source");
            std::fs::write(
                directory.join("package.workflow-package.yaml"),
                "version: 3\npackage_id: duplicate\nexports: {main: main}\nmembers:\n  - member_id: main\n    source_name: main.workflow.yaml\n",
            )
            .expect("package");
        }
        std::fs::write(
            temp.path().join("main.workflow.yaml"),
            source.replace("workflow_id: duplicate", "workflow_id: root"),
        )
        .expect("root source");
        std::fs::write(
            temp.path().join("package.workflow-package.yaml"),
            "version: 3\npackage_id: root\nexports: {main: main}\nimports:\n  - {import_id: one, package_id: duplicate, export: main, manifest: one/package.workflow-package.yaml}\n  - {import_id: two, package_id: duplicate, export: main, manifest: two/package.workflow-package.yaml}\nmembers:\n  - member_id: main\n    source_name: main.workflow.yaml\n",
        )
        .expect("root manifest");
        assert!(
            read_workflow_package_closure_in_root(
                &temp.path().join("package.workflow-package.yaml"),
                Some(temp.path()),
            )
            .is_err()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn primary_cli_discovers_and_plans_hermetic_external_data_quality_package() {
        let repository = tempfile::tempdir().expect("external repository");
        let workflow_root = repository.path().join(".bcode/workflows");
        let command_root = workflow_root.join("command");
        let remediation_root = workflow_root.join("remediation");
        let data_root = workflow_root.clone();
        for root in [&command_root, &remediation_root, &data_root] {
            std::fs::create_dir_all(root).expect("package directory");
        }

        let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        std::fs::copy(
            source_root.join("examples/workflows/packages/command/run-and-assert.workflow.yaml"),
            command_root.join("run-and-assert.workflow.yaml"),
        )
        .expect("command source");
        std::fs::write(
            command_root.join("package.workflow-package.yaml"),
            "version: 3\npackage_id: external/command\nexports: {run: run}\nmembers:\n  - member_id: run\n    source_name: run-and-assert.workflow.yaml\n",
        )
        .expect("command manifest");
        std::fs::copy(
            source_root
                .join("examples/workflows/packages/remediation/bounded-remediation.workflow.yaml"),
            remediation_root.join("bounded-remediation.workflow.yaml"),
        )
        .expect("remediation source");
        std::fs::write(
            remediation_root.join("package.workflow-package.yaml"),
            "version: 3\npackage_id: external/remediation\nexports: {remediate: remediate}\nmembers:\n  - member_id: remediate\n    source_name: bounded-remediation.workflow.yaml\n",
        )
        .expect("remediation manifest");
        let data_source = std::fs::read_to_string(
            source_root.join("examples/workflows/packages/data-quality/data-quality.workflow.yaml"),
        )
        .expect("data source");
        std::fs::write(data_root.join("data-quality.workflow.yaml"), data_source)
            .expect("data source copy");
        std::fs::write(
            data_root.join("data-quality.workflow-package.yaml"),
            "version: 3\npackage_id: external/data-quality\nexports: {main: main}\nimports:\n  - {import_id: inspect, package_id: external/command, export: run, manifest: command/package.workflow-package.yaml}\n  - {import_id: remediate, package_id: external/remediation, export: remediate, manifest: remediation/package.workflow-package.yaml}\nmembers:\n  - member_id: main\n    source_name: data-quality.workflow.yaml\n    external_dependencies: [inspect, remediate]\n",
        )
        .expect("data manifest");

        let config = bcode_config::BcodeConfig {
            workflows: bcode_config::WorkflowsConfig {
                include_repo_workflows: true,
                include_user_workflows: false,
                paths: Vec::new(),
                ..bcode_config::WorkflowsConfig::default()
            },
            ..bcode_config::BcodeConfig::default()
        };
        let discovered =
            bcode_workflow_discovery::discover_workflows(repository.path(), &config.workflows, 10)
                .expect("external discovery");
        assert!(discovered.sources.iter().any(|source| matches!(
            source,
            bcode_workflow_discovery::DiscoveredWorkflowSource::Package { package_id, .. }
                if package_id == "external/data-quality"
        )));
        let closure = read_workflow_package_closure_in_root(
            &data_root.join("data-quality.workflow-package.yaml"),
            Some(&workflow_root),
        )
        .expect("external closure");
        let plan = bcode_workflow::plan_workflow_package_closure(
            &closure,
            &workflow_test_catalog(std::collections::BTreeSet::from([
                "build".to_string(),
                "plan".to_string(),
                "review".to_string(),
            ])),
        )
        .expect("external data quality plan");
        assert_eq!(plan.entry_package_id, "external/data-quality");
        assert_eq!(plan.packages.len(), 3);
    }

    #[test]
    fn primary_cli_builds_exact_portable_package_export_start() {
        let parent_session_id = SessionId::new();
        let request = workflow_package_start_request(
            bcode_workflow::WorkflowPackageExportIdentity {
                package_id: "example/package".to_string(),
                export: "main".to_string(),
                package_lock_digest_sha256: Some("a".repeat(64)),
            },
            Some("run-1".to_string()),
            parent_session_id,
            Some(7),
            Some("workspace".to_string()),
            Some(serde_json::json!({"mode": "safe"})),
            Some(serde_json::json!({"subject": "change"})),
        );
        assert_eq!(request.package_export.package_id, "example/package");
        assert_eq!(request.package_export.export, "main");
        assert_eq!(request.parent_session_id, parent_session_id);
        assert_eq!(request.parent_session_generation, Some(7));
        assert_eq!(request.input.expect("input")["subject"], "change");
    }

    #[test]
    fn primary_cli_parses_exact_package_generations() {
        let facts =
            parse_package_expected_generations(&["parent=4".to_string(), "child=2".to_string()])
                .expect("generation facts");
        assert_eq!(facts[0].member_id, "child");
        assert_eq!(facts[0].expected_generation, 2);
        assert_eq!(facts[1].member_id, "parent");
        assert!(parse_package_expected_generations(&["child=0".to_string()]).is_err());
        assert!(
            parse_package_expected_generations(&["child=1".to_string(), "child=2".to_string(),])
                .is_err()
        );
    }

    #[test]
    fn primary_cli_reads_json_and_toml_through_the_workflow_decoder() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let json = root.join("fixtures/workflows/source-defined-input.workflow.json");
        let toml = root.join("fixtures/workflows/source-defined-input.workflow.toml");
        let json_document = bcode_workflow::decode_workflow_authoring_source(
            &std::fs::read_to_string(&json).expect("JSON file"),
            bcode_workflow::WorkflowSourceFormat::Json,
        )
        .expect("primary CLI JSON source");
        let toml_document = bcode_workflow::decode_workflow_authoring_source(
            &std::fs::read_to_string(&toml).expect("TOML file"),
            bcode_workflow::WorkflowSourceFormat::Toml,
        )
        .expect("primary CLI TOML source");
        assert_eq!(json_document, toml_document);
        assert_eq!(
            bcode_workflow::WorkflowSourceFormat::from_file_name("workflow.yaml").expect("YAML"),
            bcode_workflow::WorkflowSourceFormat::Yaml
        );
    }
}

#[cfg(test)]
mod context_compaction_tests {
    use super::*;

    #[test]
    fn live_progress_descriptions_are_compact_and_omit_opaque_payloads() {
        let session_id = SessionId::new();
        let contribution = bcode_session_models::ToolContributionEvent {
            invocation_id: "call-1".to_owned(),
            contribution_id: "surface".to_owned(),
            sequence: 7,
            producer_id: "future.producer".to_owned(),
            schema: "future.unknown/schema".to_owned(),
            schema_version: 42,
            operation: bcode_session_models::ToolContributionOperation::Append,
            persistence: bcode_session_models::ToolContributionPersistence::Transient,
            artifact: None,
            payload: serde_json::json!({"opaque_cli": [1, 2, 3]}),
        };
        let descriptions = [
            session_live_event_description(&SessionLiveEvent {
                session_id,
                kind: SessionLiveEventKind::ToolContributionPlaced {
                    envelope: bcode_session_models::ToolContributionEnvelope::new(
                        bcode_session_models::ToolContributionPlacement::Progress,
                        contribution,
                    ),
                },
            }),
            session_live_event_description(&SessionLiveEvent {
                session_id,
                kind: SessionLiveEventKind::ToolRequestDraft {
                    event: bcode_session_models::ToolRequestDraftEvent {
                        output_position: None,
                        turn_id: "turn-1".to_owned(),
                        tool_call_id: "call-1".to_owned(),
                        tool_name: "filesystem.write".to_owned(),
                        producer_plugin_id: Some("bcode.filesystem".to_owned()),
                        schema: "bcode.filesystem.request-draft.write".to_owned(),
                        schema_version: 1,
                        placement: bcode_session_models::ToolContributionPlacement::Request,
                        generation: 1,
                        revision: 2,
                        operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                            start_offset: 0,
                            text: "private_draft_cli".to_owned(),
                        },
                        argument_bytes: 17,
                        truncated: false,
                    },
                },
            }),
        ];

        assert!(descriptions[0].contains("call-1:surface"));
        assert!(descriptions[0].contains("future.unknown/schema@42"));
        assert!(descriptions[0].contains("Progress"));
        assert!(descriptions[1].contains("generation=1"));
        for description in descriptions {
            assert!(!description.contains("opaque_cli"));
            assert!(!description.contains("private_draft_cli"));
            assert!(description.len() < 512);
        }
    }
}

#[cfg(test)]
mod cancellation_cli_tests {
    use super::{Cli, Commands, RuntimeWorkCommand};
    use clap::Parser as _;

    #[test]
    fn turn_and_runtime_work_commands_parse_structured_results() {
        let session_id = bcode_session_models::SessionId::new();
        let session = session_id.to_string();
        let turn = Cli::try_parse_from(["bcode", "cancel", &session, "--clear-queue", "--json"])
            .expect("turn cancellation parses");
        assert!(matches!(
            turn.command,
            Some(Commands::Cancel {
                clear_queue: true,
                json: true,
                ..
            })
        ));

        for arguments in [
            vec!["bcode", "runtime-work", "list", &session, "--json"],
            vec![
                "bcode",
                "runtime-work",
                "cancel",
                &session,
                "work-1",
                "--json",
            ],
            vec![
                "bcode",
                "runtime-work",
                "history",
                &session,
                "--limit",
                "10",
                "--json",
            ],
        ] {
            let parsed = Cli::try_parse_from(arguments).expect("runtime work command parses");
            assert!(matches!(parsed.command, Some(Commands::RuntimeWork { .. })));
        }
        let cancel = Cli::try_parse_from([
            "bcode",
            "runtime-work",
            "cancel",
            &session,
            "work-1",
            "--json",
        ])
        .expect("runtime cancellation parses");
        assert!(matches!(
            cancel.command,
            Some(Commands::RuntimeWork {
                command: RuntimeWorkCommand::Cancel { json: true, .. }
            })
        ));
    }
}

#[cfg(test)]
mod send_cli_tests {
    use super::{Cli, Commands};
    use clap::Parser as _;

    #[test]
    fn send_accepts_exact_prompt_sources_and_admission_controls() {
        let session_id = bcode_session_models::SessionId::new();
        let session = session_id.to_string();
        let direct = Cli::try_parse_from([
            "bcode",
            "send",
            &session,
            "hello",
            "--idempotency-key",
            "request-1",
            "--background",
            "--json",
        ])
        .expect("direct prompt parses");
        assert!(matches!(
            direct.command,
            Some(Commands::Send {
                message: Some(message),
                idempotency_key: Some(key),
                background: true,
                json: true,
                ..
            }) if message == "hello" && key == "request-1"
        ));

        let file = Cli::try_parse_from([
            "bcode",
            "send",
            &session,
            "--file",
            "prompt.txt",
            "--follow-up",
        ])
        .expect("file prompt parses");
        assert!(matches!(
            file.command,
            Some(Commands::Send {
                message: None,
                file: Some(_),
                follow_up: true,
                ..
            })
        ));
        assert!(Cli::try_parse_from(["bcode", "send", &session, "hello", "--stdin"]).is_err());
    }
}

#[cfg(test)]
mod watch_cli_tests {
    use super::{Cli, Commands, RuntimeWorkCommand, SessionCommand};
    use clap::Parser as _;

    #[test]
    fn session_and_runtime_watch_commands_parse_json_lines_mode() {
        let session_id = bcode_session_models::SessionId::new();
        let session = session_id.to_string();
        let watch = Cli::try_parse_from([
            "bcode", "session", "watch", &session, "--limit", "25", "--json",
        ])
        .expect("session watch parses");
        assert!(matches!(
            watch.command,
            Some(Commands::Session {
                command: SessionCommand::Watch {
                    session_id: parsed,
                    limit: 25,
                    json: true,
                }
            }) if parsed == session_id
        ));

        let runtime = Cli::try_parse_from(["bcode", "runtime-work", "watch", &session, "--json"])
            .expect("runtime watch parses");
        assert!(matches!(
            runtime.command,
            Some(Commands::RuntimeWork {
                command: RuntimeWorkCommand::Watch {
                    session_id: parsed,
                    json: true,
                }
            }) if parsed == session_id
        ));
    }
}

#[cfg(test)]
mod worktree_cli_tests {
    use super::{Cli, CliError, Commands, WorktreeCommand, handle_worktree_command};
    use clap::Parser as _;

    #[tokio::test]
    async fn worktree_remove_requires_confirmation_before_daemon_work() {
        let error = handle_worktree_command(WorktreeCommand::Remove {
            path: "../task".into(),
            cwd: None,
            force: false,
            yes: false,
            json: true,
        })
        .await
        .expect_err("unconfirmed removal must fail");
        assert!(matches!(
            error,
            CliError::InvalidArguments(message) if message == "worktree removal requires --yes"
        ));
    }

    #[test]
    fn worktree_commands_parse_bounded_daemon_operations() {
        let list = Cli::try_parse_from(["bcode", "worktree", "list", "--cwd", ".", "--json"])
            .expect("worktree list parses");
        assert!(matches!(
            list.command,
            Some(Commands::Worktree {
                command: WorktreeCommand::List { json: true, .. }
            })
        ));

        let session_id = bcode_session_models::SessionId::new();
        let create = Cli::try_parse_from([
            "bcode",
            "worktree",
            "create",
            "task",
            "--new-branch",
            "feature/task",
            "--attach-session-id",
            &session_id.to_string(),
            "--json",
        ])
        .expect("worktree create parses");
        assert!(matches!(
            create.command,
            Some(Commands::Worktree {
                command: WorktreeCommand::Create {
                    attach_session_id: Some(parsed),
                    new_session: false,
                    json: true,
                    ..
                }
            }) if parsed == session_id
        ));

        let remove =
            Cli::try_parse_from(["bcode", "worktree", "remove", "../task", "--yes", "--json"])
                .expect("worktree remove parses");
        assert!(matches!(
            remove.command,
            Some(Commands::Worktree {
                command: WorktreeCommand::Remove {
                    yes: true,
                    json: true,
                    ..
                }
            })
        ));
        assert!(
            Cli::try_parse_from([
                "bcode", "worktree", "create", "task", "--branch", "existing", "--detach",
            ])
            .is_err()
        );
    }
}

#[cfg(test)]
mod session_configuration_cli_tests {
    use super::{Cli, CliError, Commands, SessionCommand, delete_session};
    use clap::Parser as _;

    #[test]
    fn session_create_directory_and_receipt_preserve_existing_contract() {
        let parsed = Cli::try_parse_from([
            "bcode",
            "session",
            "create",
            "named",
            "--cwd",
            "workspace",
            "--json",
        ])
        .unwrap();
        assert!(matches!(parsed.command, Some(Commands::Session {
            command: SessionCommand::Create { name: Some(name), cwd: Some(cwd), json: true }
        }) if name == "named" && cwd == std::path::Path::new("workspace")));
        let session: bcode_session_models::SessionSummary =
            serde_json::from_value(serde_json::json!({
                "id": bcode_session_models::SessionId::new(), "name": "created",
                "client_count": 0, "created_at_ms": 1, "updated_at_ms": 1,
                "working_directory": "workspace"
            }))
            .unwrap();
        for json in [false, true] {
            let mut output = Vec::new();
            super::write_created_session(&mut output, &session, json).unwrap();
            if json {
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&output).unwrap(),
                    serde_json::to_value(&session).unwrap()
                );
                assert_eq!(
                    String::from_utf8(output).unwrap(),
                    format!("{}\n", serde_json::to_string(&session).unwrap())
                );
            } else {
                assert_eq!(
                    String::from_utf8(output).unwrap(),
                    format!("{}\n", session.id)
                );
            }
            assert!(
                super::write_created_session(
                    &mut std::io::Cursor::new(&mut [0_u8; 1][..]),
                    &session,
                    json
                )
                .is_err()
            );
        }
    }

    #[test]
    fn session_list_accepts_explicit_directory() {
        for (args, expected) in [
            (vec!["bcode", "session", "list", "--json"], None),
            (
                vec!["bcode", "session", "list", "--cwd", "other", "--json"],
                Some(std::path::PathBuf::from("other")),
            ),
        ] {
            let parsed = Cli::try_parse_from(args).expect("list parses");
            assert!(matches!(parsed.command, Some(Commands::Session {
                command: SessionCommand::List { cwd, json: true, with_status: false }
            }) if cwd == expected));
        }
    }

    #[test]
    fn session_refresh_parses_scope_and_preserves_loading_status() {
        let parsed = Cli::try_parse_from([
            "bcode",
            "session",
            "refresh",
            "--cwd",
            "workspace",
            "--source",
            "native",
            "--source",
            "imported",
            "--json",
        ])
        .unwrap();
        assert!(matches!(parsed.command, Some(Commands::Session {
            command: SessionCommand::Refresh { cwd: Some(cwd), sources, json: true }
        }) if cwd == std::path::Path::new("workspace") && sources == ["native", "imported"]));
        let catalog = bcode_client::SessionList {
            sessions: vec![],
            catalog_status: bcode_session_models::SessionCatalogStatus::Loading,
            catalog_sources: vec![],
            catalog_revision: 7,
        };
        let mut output = Vec::new();
        super::write_catalog_refresh(&mut output, &catalog, false).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "catalog revision 7: \"loading\"\n"
        );
        let mut output = Vec::new();
        super::write_catalog_refresh(&mut output, &catalog, true).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["catalog_status"], "loading");
        assert_eq!(value["catalog_revision"], 7);
        for json in [false, true] {
            assert!(
                super::write_catalog_refresh(
                    &mut std::io::Cursor::new(&mut [0_u8; 1][..]),
                    &catalog,
                    json
                )
                .is_err()
            );
        }
    }

    #[test]
    fn session_catalog_output_preserves_health_and_revision() {
        use bcode_session_models::{SessionCatalogSourceStatus, SessionCatalogStatus};
        assert!(Cli::try_parse_from(["bcode", "session", "list", "--with-status"]).is_err());
        let parsed =
            Cli::try_parse_from(["bcode", "session", "list", "--with-status", "--json"]).unwrap();
        assert!(matches!(
            parsed.command,
            Some(Commands::Session {
                command: SessionCommand::List {
                    with_status: true,
                    json: true,
                    ..
                }
            })
        ));
        for status in [
            SessionCatalogStatus::NotStarted,
            SessionCatalogStatus::Loading,
            SessionCatalogStatus::Loaded,
            SessionCatalogStatus::Degraded("partial".into()),
            SessionCatalogStatus::Failed("unavailable".into()),
        ] {
            let catalog = bcode_client::SessionList {
                sessions: vec![],
                catalog_status: status.clone(),
                catalog_revision: 42,
                catalog_sources: vec![SessionCatalogSourceStatus {
                    source_id: "native".into(),
                    display_name: "Native sessions".into(),
                    status: status.clone(),
                    updated_at_ms: 123,
                }],
            };
            let mut output = Vec::new();
            super::write_session_catalog(&mut output, &catalog).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(
                value,
                serde_json::json!({
                    "schema_version": 1, "sessions": [], "catalog_status": status,
                    "catalog_sources": catalog.catalog_sources, "catalog_revision": 42
                })
            );
            assert!(
                super::write_session_catalog(
                    &mut std::io::Cursor::new(&mut [0_u8; 1][..]),
                    &catalog
                )
                .is_err()
            );
        }
    }

    #[test]
    fn session_list_output_preserves_rows_and_reports_io_errors() {
        struct FailingOutput {
            flush_only: bool,
        }
        impl std::io::Write for FailingOutput {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.flush_only {
                    Ok(bytes.len())
                } else {
                    Err(std::io::ErrorKind::BrokenPipe.into())
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        let session: bcode_session_models::SessionSummary =
            serde_json::from_value(serde_json::json!({
                "id": bcode_session_models::SessionId::new(), "name": "listed session",
                "client_count": 2, "created_at_ms": 0, "updated_at_ms": 0
            }))
            .expect("summary");
        for sessions in [vec![], vec![session.clone()]] {
            for json in [false, true] {
                let mut output = Vec::new();
                super::write_session_list(&mut output, &sessions, json).expect("output");
                if json {
                    assert_eq!(
                        serde_json::from_slice::<serde_json::Value>(&output).unwrap(),
                        serde_json::to_value(&sessions).unwrap()
                    );
                } else {
                    let expected = if sessions.is_empty() {
                        "no sessions\n".to_owned()
                    } else {
                        format!("listed session\t{}\t2 clients\n", session.id)
                    };
                    assert_eq!(String::from_utf8(output).unwrap(), expected);
                }
                for flush_only in [false, true] {
                    assert!(
                        super::write_session_list(
                            &mut FailingOutput { flush_only },
                            &sessions,
                            json
                        )
                        .is_err()
                    );
                }
            }
        }
    }

    #[test]
    fn session_discovery_commands_parse_without_a_session_id() {
        let agents = Cli::try_parse_from(["bcode", "session", "agents", "--json"])
            .expect("agent discovery parses");
        assert!(matches!(
            agents.command,
            Some(Commands::Session {
                command: SessionCommand::Agents { json: true }
            })
        ));
        let skills = Cli::try_parse_from(["bcode", "session", "skills", "--json"])
            .expect("skill discovery parses");
        assert!(matches!(
            skills.command,
            Some(Commands::Session {
                command: SessionCommand::Skills { json: true }
            })
        ));
        let manifest = Cli::try_parse_from([
            "bcode",
            "session",
            "describe-skill",
            "repo:review",
            "--json",
        ])
        .expect("skill inspection parses");
        assert!(matches!(manifest.command, Some(Commands::Session {
            command: SessionCommand::DescribeSkill { skill_id, json: true }
        }) if skill_id == "repo:review"));
        assert!(Cli::try_parse_from(["bcode", "session", "describe-skill"]).is_err());
    }

    #[tokio::test]
    async fn session_delete_requires_confirmation_before_connecting() {
        let error = delete_session(bcode_session_models::SessionId::new(), false, true)
            .await
            .expect_err("unconfirmed deletion must fail");
        assert!(matches!(
            error,
            CliError::InvalidArguments(message) if message == "session deletion requires --yes"
        ));
    }

    #[test]
    fn session_invoke_skill_parses_machine_output() {
        let session = bcode_session_models::SessionId::new().to_string();
        let parsed = Cli::try_parse_from([
            "bcode",
            "session",
            "invoke-skill",
            &session,
            "skill-1",
            "argument text",
            "--json",
        ])
        .expect("skill invocation parses");
        assert!(matches!(
            parsed.command,
            Some(Commands::Session {
                command: SessionCommand::InvokeSkill {
                    skill_id,
                    arguments,
                    json: true,
                    ..
                }
            }) if skill_id == "skill-1" && arguments == "argument text"
        ));
    }

    #[test]
    fn session_configuration_commands_parse_machine_paths() {
        let session_id = bcode_session_models::SessionId::new();
        let session = session_id.to_string();
        let cases = [
            vec!["bcode", "session", "create", "named", "--json"],
            vec!["bcode", "session", "rename", &session, "renamed", "--json"],
            vec!["bcode", "session", "delete", &session, "--yes", "--json"],
            vec![
                "bcode",
                "session",
                "set-working-directory",
                &session,
                ".",
                "--json",
            ],
            vec!["bcode", "session", "set-agent", &session, "build", "--json"],
            vec![
                "bcode",
                "session",
                "set-model",
                &session,
                "model-1",
                "--provider",
                "provider",
                "--json",
            ],
            vec![
                "bcode",
                "session",
                "set-reasoning",
                &session,
                "--effort",
                "high",
                "--summary",
                "detailed",
                "--json",
            ],
            vec!["bcode", "session", "active-skills", &session, "--json"],
            vec![
                "bcode",
                "session",
                "activate-skill",
                &session,
                "skill-1",
                "--json",
            ],
            vec![
                "bcode",
                "session",
                "deactivate-skill",
                &session,
                "skill-1",
                "--json",
            ],
            vec!["bcode", "session", "compact", &session, "--json"],
        ];
        for arguments in cases {
            let parsed = Cli::try_parse_from(arguments).expect("session configuration parses");
            assert!(matches!(parsed.command, Some(Commands::Session { .. })));
        }

        let delete_without_confirmation =
            Cli::try_parse_from(["bcode", "session", "delete", &session, "--json"])
                .expect("delete confirmation is validated before side effects");
        assert!(matches!(
            delete_without_confirmation.command,
            Some(Commands::Session {
                command: SessionCommand::Delete {
                    yes: false,
                    json: true,
                    ..
                }
            })
        ));

        let pool = Cli::try_parse_from([
            "bcode",
            "session",
            "set-auth-pool",
            "openai",
            "--profile",
            "openai-2",
            "--json",
        ])
        .expect("auth pool preference parses");
        assert!(matches!(
            pool.command,
            Some(Commands::Session {
                command: SessionCommand::SetAuthPool {
                    pool,
                    profile: Some(profile),
                    clear: false,
                    json: true,
                }
            }) if pool == "openai" && profile == "openai-2"
        ));
    }
}

#[cfg(test)]
mod permission_cli_tests {
    use super::{Cli, Commands, PermissionCommand};
    use clap::Parser as _;
    use std::str::FromStr as _;

    #[test]
    fn permission_status_parses_human_and_machine_output() {
        for json in [false, true] {
            let mut arguments = vec!["bcode", "permission", "status"];
            if json {
                arguments.push("--json");
            }
            let parsed = Cli::try_parse_from(arguments).expect("policy status parses");
            assert!(matches!(parsed.command, Some(Commands::Permission {
                command: PermissionCommand::Status { json: actual }
            }) if actual == json));
        }
    }

    #[test]
    fn permission_commands_parse_scoped_json_remember_and_batch_paths() {
        let session_id = bcode_session_models::SessionId::new();
        let list = Cli::try_parse_from([
            "bcode",
            "permission",
            "list",
            "--session-id",
            &session_id.to_string(),
            "--json",
        ])
        .expect("scoped permission list parses");
        assert!(matches!(
            list.command,
            Some(Commands::Permission {
                command: PermissionCommand::List {
                    session_id: Some(parsed),
                    json: true,
                }
            }) if parsed == session_id
        ));

        let approve = Cli::try_parse_from([
            "bcode",
            "permission",
            "approve",
            "permission-1",
            "--remember",
            "--json",
        ])
        .expect("remembered approval parses");
        assert!(matches!(
            approve.command,
            Some(Commands::Permission {
                command: PermissionCommand::Approve {
                    permission_id,
                    remember: true,
                    json: true,
                }
            }) if permission_id == "permission-1"
        ));

        let add = Cli::try_parse_from([
            "bcode",
            "permission",
            "add",
            "--agent",
            "build",
            "--category",
            "read",
            "--pattern",
            "**/*.rs",
            "--action",
            "allow",
            "--json",
        ])
        .expect("JSON permission rule addition parses");
        assert!(matches!(
            add.command,
            Some(Commands::Permission {
                command: PermissionCommand::Add {
                    agent,
                    category,
                    pattern,
                    action,
                    json: true,
                }
            }) if agent == "build"
                && category == "read"
                && pattern == "**/*.rs"
                && action == "allow"
        ));

        let batch = Cli::try_parse_from([
            "bcode",
            "permission",
            "resolve-batch",
            "batch-1",
            "--deny",
            "--json",
        ])
        .expect("batch denial parses");
        assert!(matches!(
            batch.command,
            Some(Commands::Permission {
                command: PermissionCommand::ResolveBatch {
                    batch_id,
                    approve: false,
                    deny: true,
                    json: true,
                }
            }) if batch_id == "batch-1"
        ));
        assert!(Cli::try_parse_from(["bcode", "permission", "resolve-batch", "batch-1",]).is_err());
        assert!(
            Cli::try_parse_from([
                "bcode",
                "permission",
                "resolve-batch",
                "batch-1",
                "--approve",
                "--deny",
            ])
            .is_err()
        );
    }

    #[test]
    fn session_id_round_trip_used_by_parser_is_canonical() {
        let session_id = bcode_session_models::SessionId::new();
        assert_eq!(
            bcode_session_models::SessionId::from_str(&session_id.to_string())
                .expect("session id round trip"),
            session_id
        );
    }
}

#[cfg(test)]
mod turn_admission_cli_tests {
    use bcode_session_models::{SessionId, TurnAdmission, TurnReceipt, TurnRejectionReason};

    #[test]
    fn admission_classification_preserves_receipts_and_distinguishes_all_outcomes() {
        let receipt = TurnReceipt::from_accepted_event(SessionId::new(), 42);
        for (admission, expected_exit) in [
            (TurnAdmission::Accepted(receipt.clone()), None),
            (TurnAdmission::Existing(receipt.clone()), None),
            (TurnAdmission::Deferred(receipt.clone()), None),
            (TurnAdmission::CancelledBeforeStart(receipt), Some(4)),
            (
                TurnAdmission::Rejected(TurnRejectionReason::ExecutionPolicy),
                Some(3),
            ),
            (
                TurnAdmission::Rejected(TurnRejectionReason::SessionUnavailable),
                Some(1),
            ),
        ] {
            let original = serde_json::to_value(&admission).expect("admission JSON");
            assert_eq!(
                super::turn_admission_failure(&admission)
                    .as_ref()
                    .map(super::CliError::exit_code),
                expected_exit
            );
            assert_eq!(serde_json::to_value(&admission).unwrap(), original);
        }
    }
}

#[cfg(test)]
mod exit_code_tests {
    use super::{CliError, ClientError};

    #[test]
    fn invocation_input_exit_codes_use_explicit_operation_categories() {
        for (code, expected) in [
            ("invalid_invocation_input_producer", 2),
            ("invalid_invocation_input_schema", 2),
            ("invalid_invocation_input_id", 2),
            ("invocation_input_too_large", 2),
            ("plugin_invocation_not_active", 1),
            ("plugin_invocation_producer_mismatch", 1),
            ("invocation_input_queue_full", 1),
            ("invocation_input_route_closed", 1),
            ("invalid_invocation_input_future_variant", 1),
        ] {
            for message in ["invalid input", "cancelled", "authorization denied"] {
                let error = CliError::Client(ClientError::Server {
                    code: code.to_owned(),
                    message: message.to_owned(),
                });
                assert_eq!(error.exit_code(), expected, "{code}: {message}");
            }
        }
    }

    #[test]
    fn exit_codes_distinguish_usage_authorization_cancellation_and_runtime_failures() {
        assert_eq!(
            CliError::InvalidArguments("bad input".to_owned()).exit_code(),
            2
        );
        assert_eq!(
            CliError::Client(ClientError::Server {
                code: "authorization_denied".to_owned(),
                message: "denied".to_owned(),
            })
            .exit_code(),
            3
        );
        assert_eq!(
            CliError::Client(ClientError::Server {
                code: "cancelled".to_owned(),
                message: "cancelled".to_owned(),
            })
            .exit_code(),
            4
        );
        for code in [
            "ralph_run_cancel_failed",
            "workflow_cancellation_prevents_control",
            "future_cancel_state",
            "authorization_service_failed",
            "future_authorization_state",
        ] {
            assert_eq!(
                CliError::Client(ClientError::Server {
                    code: code.to_owned(),
                    message: "operation failed".to_owned(),
                })
                .exit_code(),
                1,
                "{code}"
            );
        }
        for code in [
            "workflow_computation_cancelled",
            "worktree_create_cancelled",
        ] {
            assert_eq!(
                CliError::Client(ClientError::Server {
                    code: code.to_owned(),
                    message: "operation cancelled".to_owned(),
                })
                .exit_code(),
                4,
                "{code}"
            );
        }
        assert_eq!(
            CliError::Client(ClientError::Server {
                code: "workflow_operation_unauthorized".to_owned(),
                message: "workflow operation is not authorized".to_owned(),
            })
            .exit_code(),
            3
        );
        assert_eq!(CliError::PluginCli("failed".to_owned()).exit_code(), 1);
        assert_eq!(
            CliError::TurnRejected(bcode_session_models::TurnRejectionReason::ExecutionPolicy)
                .exit_code(),
            3
        );
        assert_eq!(
            CliError::TurnRejected(bcode_session_models::TurnRejectionReason::SessionUnavailable)
                .exit_code(),
            1
        );
        assert_eq!(CliError::TurnCancelledBeforeStart.exit_code(), 4);
        assert_eq!(CliError::InvalidExchangeResolution.exit_code(), 2);
        assert_eq!(
            CliError::Client(ClientError::Server {
                code: "invalid_exchange_resolution".to_owned(),
                message: "tool exchange resolution is malformed or unsupported".to_owned(),
            })
            .exit_code(),
            2
        );
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::Interrupted,
        ] {
            let error = CliError::from(std::io::Error::from(kind));
            assert_eq!(error.exit_code(), 1);
            assert!(error.to_string().starts_with("I/O error:"));
        }
    }
}

#[cfg(test)]
mod plugin_cli_tests {
    use super::{Cli, Commands, PluginCommand};
    use clap::Parser as _;

    #[test]
    fn plugin_services_parse_daemon_json_output() {
        let cli = Cli::try_parse_from(["bcode", "plugin", "services", "--daemon", "--json"])
            .expect("daemon plugin service JSON listing parses");
        assert!(matches!(
            cli.command,
            Some(Commands::Plugin {
                command: PluginCommand::Services {
                    daemon: true,
                    json: true,
                    ..
                }
            })
        ));
    }

    #[test]
    fn plugin_inventory_and_checks_parse_machine_output() {
        let list = Cli::try_parse_from(["bcode", "plugin", "list", "--json"])
            .expect("plugin list JSON output parses");
        assert!(matches!(
            list.command,
            Some(Commands::Plugin {
                command: PluginCommand::List { json: true, .. }
            })
        ));
        let check = Cli::try_parse_from(["bcode", "plugin", "check", "--json"])
            .expect("plugin check JSON output parses");
        assert!(matches!(
            check.command,
            Some(Commands::Plugin {
                command: PluginCommand::Check { json: true, .. }
            })
        ));
    }

    #[test]
    fn plugin_actions_parse_machine_output() {
        let invoke = Cli::try_parse_from([
            "bcode",
            "plugin",
            "invoke",
            "example",
            "example/v1",
            "run",
            "--daemon",
            "--json",
        ])
        .expect("plugin invocation JSON output parses");
        assert!(matches!(
            invoke.command,
            Some(Commands::Plugin {
                command: PluginCommand::Invoke {
                    daemon: true,
                    json: true,
                    ..
                }
            })
        ));
        let call = Cli::try_parse_from([
            "bcode",
            "plugin",
            "call",
            "example/v1",
            "run",
            "--daemon",
            "--json",
        ])
        .expect("plugin call JSON output parses");
        assert!(matches!(
            call.command,
            Some(Commands::Plugin {
                command: PluginCommand::Call {
                    daemon: true,
                    json: true,
                    ..
                }
            })
        ));
        let publish = Cli::try_parse_from([
            "bcode",
            "plugin",
            "publish",
            "example.topic",
            "--daemon",
            "--json",
        ])
        .expect("plugin publish JSON output parses");
        assert!(matches!(
            publish.command,
            Some(Commands::Plugin {
                command: PluginCommand::Publish {
                    daemon: true,
                    json: true,
                    ..
                }
            })
        ));
    }
}

#[cfg(test)]
mod model_cli_tests {
    use super::{Cli, Commands, ModelCommand};
    use clap::Parser as _;

    #[test]
    fn image_verification_dry_run_needs_no_fixture_and_storage_is_opt_in() {
        let cli = Cli::try_parse_from(["bcode", "model", "verify-images", "--dry-run"])
            .expect("image verification parses");
        let Some(Commands::Model {
            command: ModelCommand::VerifyImages(args),
        }) = cli.command
        else {
            panic!("expected image verification");
        };
        assert!(!args.allow_conversation_storage);
        assert_eq!(args.max_models, 1);
        assert!(
            super::image_verification_fixture(&args)
                .expect("dry run")
                .is_none()
        );
        let cli = Cli::try_parse_from(["bcode", "model", "verify-images"])
            .expect("image verification parses");
        let Some(Commands::Model {
            command: ModelCommand::VerifyImages(args),
        }) = cli.command
        else {
            panic!("expected image verification");
        };
        assert!(super::image_verification_fixture(&args).is_err());
    }

    #[test]
    fn generated_image_probe_needs_no_local_files_or_answer_arguments() {
        let cli = Cli::try_parse_from([
            "bcode",
            "model",
            "verify-images",
            "--generated-seed",
            "726",
            "--tool-result",
        ])
        .expect("generated probe parses");
        let Some(Commands::Model {
            command: ModelCommand::VerifyImages(args),
        }) = cli.command
        else {
            panic!("probe")
        };
        let (images, question, answer) = super::prepare_image_probe(&args).expect("fixture");
        assert_eq!(images.expect("images").len(), 2);
        assert_eq!(answer, "red green blue yellow");
        assert!(!question.contains(&answer));
        assert!(args.tool_result);
        assert!(
            Cli::try_parse_from([
                "bcode",
                "model",
                "verify-images",
                "--generated-seed",
                "1",
                "--image",
                "private.png"
            ])
            .is_err()
        );
    }

    #[test]
    fn model_diagnostics_supports_machine_output() {
        let cli = Cli::try_parse_from(["bcode", "model", "diagnostics", "--json"])
            .expect("model diagnostics parses");
        assert!(matches!(
            cli.command,
            Some(Commands::Model {
                command: ModelCommand::Diagnostics { json: true }
            })
        ));
    }
}

#[cfg(test)]
mod json_stream_output_tests {
    use super::{CliError, write_json_stream_record};

    #[derive(Default)]
    struct Output {
        bytes: Vec<u8>,
        flushes: usize,
        fail_write: bool,
        fail_flush: bool,
    }

    impl std::io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.fail_write {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            if self.fail_flush {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            Ok(())
        }
    }

    #[cfg(unix)]
    fn parsed_artifact_range_args(id: &str, raw: bool) -> super::ArtifactRangeArgs {
        use clap::Parser as _;
        let mut arguments = vec![
            "bcode",
            "session",
            "artifact-range",
            id,
            "artifact",
            "reference",
            "--offset",
            "1",
            "--length",
            "3",
        ];
        if raw {
            arguments.push("--raw");
        }
        let cli = super::Cli::try_parse_from(arguments).unwrap();
        let super::Commands::Session { command } = cli.command.unwrap() else {
            panic!("session command")
        };
        let super::SessionCommand::ArtifactRange(args) = command else {
            panic!("artifact command")
        };
        args
    }

    #[cfg(unix)]
    fn assert_artifact_output(
        bytes: &[u8],
        expected: &bcode_session_models::SessionArtifactRange,
        raw: bool,
    ) {
        if raw {
            assert_eq!(bytes, expected.bytes);
        } else {
            assert_eq!(
                &serde_json::from_slice::<bcode_session_models::SessionArtifactRange>(bytes)
                    .unwrap(),
                expected
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn artifact_range_handler_preserves_ipc_success_and_failure_output() {
        for (raw, outcome) in [false, true]
            .into_iter()
            .flat_map(|raw| (0..3).map(move |outcome| (raw, outcome)))
        {
            let directory = tempfile::tempdir().unwrap();
            let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.path().join("a.sock"));
            let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
            let session_id = bcode_session_models::SessionId::new();
            let expected = bcode_session_models::SessionArtifactRange {
                artifact_id: "artifact".into(),
                reference_key: "reference".into(),
                content_type: Some("application/octet-stream".into()),
                offset: 1,
                total_bytes: 4,
                reference_bytes: Some(4),
                reference_revision: 7,
                finalized: true,
                finalized_event_seq: Some(9),
                availability: None,
                complete: Some(true),
                checksum_sha256: None,
                bytes: vec![0, 255, 10],
            };
            let artifact_response = match outcome {
                0 => bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::SessionArtifactRange {
                    range: expected.clone(),
                }),
                1 => {
                    let mut invalid = expected.clone();
                    invalid.reference_key = "unrequested-reference".into();
                    bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::SessionArtifactRange {
                        range: invalid,
                    })
                }
                _ => bcode_ipc::Response::Err(bcode_ipc::ErrorResponse {
                    code: "artifact_unavailable".into(),
                    message: "artifact unavailable".into(),
                }),
            };
            let server = tokio::spawn(async move {
                let mut stream = listener.accept().await.unwrap();
                let hello = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
                let daemon = bcode_ipc::DaemonStatus {
                    namespace: bcode_ipc::daemon_namespace(),
                    protocol_version: u32::from(bcode_ipc::CURRENT_PROTOCOL_VERSION),
                    artifact_id: Some(bcode_ipc::ArtifactId::current()),
                    build_fingerprint: bcode_ipc::BUILD_FINGERPRINT.into(),
                    storage_writer_epoch: Some(bcode_ipc::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
                    session_event_schema_version: Some(
                        bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                    ),
                    state_location_id: Some(bcode_ipc::state_location_id()),
                    ..Default::default()
                };
                let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
                    protocol_version: bcode_ipc::ProtocolVersion(
                        bcode_ipc::CURRENT_PROTOCOL_VERSION,
                    ),
                    client_id: bcode_session_models::ClientId::new(),
                    daemon,
                });
                bcode_ipc::send_envelope(
                    &mut stream,
                    &bcode_ipc::response_envelope(hello.request_id, &response).unwrap(),
                )
                .await
                .unwrap();
                let request = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
                assert!(
                    matches!(bcode_ipc::decode_request(&request.payload).unwrap(),
                    bcode_ipc::Request::ReadSessionArtifact { session_id: id, artifact_id, reference_key, offset: 1, length: 3 }
                    if id == session_id && artifact_id == "artifact" && reference_key == "reference")
                );
                bcode_ipc::send_envelope(
                    &mut stream,
                    &bcode_ipc::response_envelope(request.request_id, &artifact_response).unwrap(),
                )
                .await
                .unwrap();
            });
            let args = parsed_artifact_range_args(&session_id.to_string(), raw);
            let mut output = Output::default();
            let result = super::read_artifact_range_to(
                &bcode_client::BcodeClient::new(endpoint),
                args,
                &mut output,
            )
            .await;
            if outcome != 0 {
                assert!(result.is_err());
                assert!(output.bytes.is_empty());
                assert_eq!(output.flushes, 0);
            } else {
                result.unwrap();
                assert_eq!(output.flushes, 1);
                assert_artifact_output(&output.bytes, &expected, raw);
            }
            server.await.unwrap();
        }
    }

    #[test]
    fn artifact_range_output_preserves_binary_and_metadata() {
        let mut range = bcode_session_models::SessionArtifactRange {
            artifact_id: "artifact".into(),
            reference_key: "reference".into(),
            content_type: Some("application/octet-stream".into()),
            offset: 0,
            total_bytes: 3,
            reference_bytes: Some(3),
            reference_revision: 7,
            finalized: true,
            finalized_event_seq: Some(9),
            availability: None,
            complete: Some(true),
            checksum_sha256: None,
            bytes: vec![0, 255, 10],
        };
        for raw in [false, true] {
            let mut output = Output::default();
            super::write_artifact_range(&mut output, &range, raw).unwrap();
            assert_eq!(output.flushes, 1);
            if raw {
                assert_eq!(output.bytes, range.bytes);
            } else {
                assert_eq!(
                    serde_json::from_slice::<bcode_session_models::SessionArtifactRange>(
                        &output.bytes
                    )
                    .unwrap(),
                    range
                );
            }
            for fail_flush in [false, true] {
                let mut output = Output {
                    fail_write: !fail_flush,
                    fail_flush,
                    ..Output::default()
                };
                assert!(
                    matches!(super::write_artifact_range(&mut output, &range, raw),
                    Err(CliError::Signal(error)) if error.kind() == std::io::ErrorKind::BrokenPipe)
                );
            }
        }
        range.bytes.clear();
        range.offset = range.total_bytes;
        let mut output = Output::default();
        super::write_artifact_range(&mut output, &range, true).unwrap();
        assert!(output.bytes.is_empty());
        assert_eq!(output.flushes, 1);
    }

    #[test]
    fn invocation_input_receipt_flushes_and_propagates_output_failures() {
        for json in [false, true] {
            let mut output = Output::default();
            super::write_invocation_input_receipt(&mut output, json).unwrap();
            assert_eq!(output.flushes, 1);
            assert_eq!(
                output.bytes,
                if json {
                    b"{\"accepted\":true}\n".as_slice()
                } else {
                    b"Invocation input accepted\n".as_slice()
                }
            );
            for fail_flush in [false, true] {
                let mut output = Output {
                    fail_write: !fail_flush,
                    fail_flush,
                    ..Output::default()
                };
                assert!(matches!(
                    super::write_invocation_input_receipt(&mut output, json),
                    Err(CliError::Signal(error)) if error.kind() == std::io::ErrorKind::BrokenPipe
                ));
                assert_eq!(output.flushes, usize::from(fail_flush));
                assert_eq!(output.bytes.is_empty(), !fail_flush);
            }
        }
    }

    #[test]
    fn model_validation_exposes_provider_owned_remediation() {
        let failure = bcode_model::ProviderFailureContext {
            provider_id: "provider".to_owned(),
            source_kind: bcode_model::ProviderFailureSourceKind::AuthProfile,
            source: "work".to_owned(),
            capability: bcode_model::ProviderFailureCapability::Authentication,
            remediation: "Sign in to the work profile".to_owned(),
        };
        let validation = bcode_model::ValidateConfigResponse {
            valid: false,
            message: None,
            failures: vec![failure.clone()],
            metadata: std::collections::BTreeMap::new(),
        };
        for json in [false, true] {
            let mut output = Output::default();
            let response = bcode_ipc::PluginServiceResponse {
                payload: serde_json::to_vec(&validation).unwrap(),
                error: None,
            };
            let error = super::write_model_validation(&mut output, response, json).unwrap_err();
            assert_eq!(error.exit_code(), 1);
            if json {
                let parsed: bcode_model::ValidateConfigResponse =
                    serde_json::from_slice(&output.bytes).unwrap();
                assert_eq!(parsed.failures, vec![failure.clone()]);
            } else {
                assert_eq!(output.bytes, b"valid\tfalse\nfailure\tprovider\tAuthProfile\twork\tAuthentication\nremediation\tSign in to the work profile\n");
            }
            assert_eq!(output.flushes, 1);
        }
    }

    #[test]
    fn model_validation_preserves_results_but_rejects_invalid_configuration() {
        for json in [false, true] {
            for valid in [false, true] {
                let response = || {
                    bcode_ipc::PluginServiceResponse {
                    payload: serde_json::to_vec(&serde_json::json!({"valid": valid, "message": "details", "metadata": {"key": "value"}})).unwrap(),
                    error: None,
                }
                };
                let mut output = Output::default();
                let result = super::write_model_validation(&mut output, response(), json);
                if valid {
                    result.unwrap();
                } else {
                    assert!(
                        matches!(result, Err(CliError::PluginService { code, .. }) if code == "invalid_configuration")
                    );
                }
                if json {
                    let value: bcode_model::ValidateConfigResponse =
                        serde_json::from_slice(&output.bytes).unwrap();
                    assert_eq!(value.valid, valid);
                    assert_eq!(value.message.as_deref(), Some("details"));
                } else {
                    assert_eq!(
                        output.bytes,
                        format!("valid\t{valid}\nmessage\tdetails\nmetadata\tkey\tvalue\n")
                            .as_bytes()
                    );
                }
                assert_eq!(output.flushes, 1);
                for fail_flush in [false, true] {
                    let mut output = Output {
                        fail_write: !fail_flush,
                        fail_flush,
                        ..Output::default()
                    };
                    assert!(
                        matches!(super::write_model_validation(&mut output, response(), json),
                        Err(CliError::Signal(error)) if error.kind() == std::io::ErrorKind::BrokenPipe)
                    );
                }
            }
            let mut output = Output::default();
            let response = bcode_ipc::PluginServiceResponse {
                payload: vec![],
                error: Some(bcode_ipc::PluginServiceError {
                    code: "unavailable".to_owned(),
                    message: "unavailable".to_owned(),
                }),
            };
            assert!(matches!(
                super::write_model_validation(&mut output, response, json),
                Err(CliError::PluginService { .. })
            ));
            assert!(output.bytes.is_empty());
        }
    }

    #[test]
    fn model_capabilities_separate_failures_and_support_machine_results() {
        let payload = serde_json::to_vec(&serde_json::json!({
            "provider_id": "provider", "display_name": "Provider", "metadata": {"key": "value"}
        }))
        .unwrap();
        for json in [false, true] {
            let response = || bcode_ipc::PluginServiceResponse {
                payload: payload.clone(),
                error: None,
            };
            let mut output = Output::default();
            super::write_model_capabilities(&mut output, response(), json).unwrap();
            if json {
                let parsed: bcode_model::ProviderCapabilities =
                    serde_json::from_slice(&output.bytes).unwrap();
                assert_eq!(parsed.provider_id, "provider");
                assert_eq!(parsed.metadata.get("key").unwrap(), "value");
            } else {
                assert_eq!(output.bytes, b"provider\tProvider\nmetadata\tkey\tvalue\n");
            }
            assert_eq!(output.flushes, 1);
            for fail_flush in [false, true] {
                let mut output = Output {
                    fail_write: !fail_flush,
                    fail_flush,
                    ..Output::default()
                };
                assert!(
                    matches!(super::write_model_capabilities(&mut output, response(), json),
                    Err(CliError::Signal(error)) if error.kind() == std::io::ErrorKind::BrokenPipe)
                );
            }
            let mut output = Output::default();
            let failed = bcode_ipc::PluginServiceResponse {
                payload: vec![],
                error: Some(bcode_ipc::PluginServiceError {
                    code: "unavailable".to_owned(),
                    message: "provider unavailable".to_owned(),
                }),
            };
            assert!(matches!(
                super::write_model_capabilities(&mut output, failed, json),
                Err(CliError::PluginService { .. })
            ));
            assert!(output.bytes.is_empty());
            let malformed = bcode_ipc::PluginServiceResponse {
                payload: b"not json".to_vec(),
                error: None,
            };
            assert!(super::write_model_capabilities(&mut output, malformed, json).is_err());
            assert!(output.bytes.is_empty());
        }
    }

    #[test]
    fn catalog_diagnostics_preserve_degraded_state_and_output_errors() {
        for degraded in [false, true] {
            let diagnostics = bcode_ipc::ModelCatalogDiagnostics {
                embedded_revision: "embedded".to_owned(),
                remote_revision: degraded.then(|| "remote".to_owned()),
                remote_enabled: degraded,
                cache_state: if degraded { "stale" } else { "disabled" }.to_owned(),
                cache_age_seconds: degraded.then_some(123),
                refresh_in_progress: false,
                last_refresh_attempt_ms: None,
                last_refresh_success_ms: None,
                last_refresh_error: degraded.then(|| "refresh unavailable".to_owned()),
            };
            let mut output = Output::default();
            super::write_model_catalog_diagnostics(&mut output, &diagnostics).unwrap();
            let expected = if degraded {
                "embedded revision: embedded\nremote revision: remote\nremote enabled: true\ncache state: stale\ncache age seconds: 123\nrefresh in progress: false\nlast refresh error: refresh unavailable\n"
            } else {
                "embedded revision: embedded\nremote revision: -\nremote enabled: false\ncache state: disabled\ncache age seconds: -\nrefresh in progress: false\n"
            };
            assert_eq!(output.bytes, expected.as_bytes());
            assert_eq!(output.flushes, 1);
            for fail_flush in [false, true] {
                let mut output = Output {
                    fail_write: !fail_flush,
                    fail_flush,
                    ..Output::default()
                };
                assert!(
                    matches!(super::write_model_catalog_diagnostics(&mut output, &diagnostics),
                    Err(CliError::Signal(error)) if error.kind() == std::io::ErrorKind::BrokenPipe)
                );
            }
        }
    }

    #[test]
    fn model_lists_preserve_rows_and_propagate_output_failures() {
        let models: Vec<bcode_model::ModelInfo> = serde_json::from_value(serde_json::json!([
            {"model_id": "first", "display_name": "First", "is_default": true,
             "context_window": 32000, "max_output_tokens": 4096},
            {"model_id": "second", "display_name": "Second"}
        ]))
        .unwrap();
        for rows in [&[][..], models.as_slice()] {
            let mut output = Output::default();
            super::write_model_list(&mut output, rows).unwrap();
            let text = String::from_utf8(output.bytes).unwrap();
            let lines = text
                .lines()
                .map(|line| line.split_whitespace().collect::<Vec<_>>())
                .collect::<Vec<_>>();
            assert_eq!(
                lines[0],
                [
                    "MODEL", "DISPLAY", "NAME", "CTX", "MAX", "OUT", "METADATA", "DEFAULT"
                ]
            );
            assert_eq!(lines.len(), rows.len() + 1);
            if !rows.is_empty() {
                assert_eq!(lines[1], ["first", "First", "32000", "4096", "-", "yes"]);
                assert_eq!(lines[2], ["second", "Second", "-", "-", "-"]);
            }
            assert_eq!(output.flushes, 1);
            for fail_flush in [false, true] {
                let mut output = Output {
                    fail_write: !fail_flush,
                    fail_flush,
                    ..Output::default()
                };
                assert!(matches!(super::write_model_list(&mut output, rows),
                    Err(CliError::Signal(error)) if error.kind() == std::io::ErrorKind::BrokenPipe));
            }
        }
    }

    #[test]
    fn model_status_preserves_values_and_propagates_output_failures() {
        let empty: bcode_model::SessionModelStatus =
            serde_json::from_value(serde_json::json!({})).unwrap();
        let populated: bcode_model::SessionModelStatus =
            serde_json::from_value(serde_json::json!({
                "provider_plugin_id": "provider", "model_id": "model",
                "context_window": 32000, "max_output_tokens": 4096
            }))
            .unwrap();
        for (status, expected) in [
            (
                empty,
                "provider\t<auto>\nmodel\t<default>\ncontext_window\t<none>\nmax_output_tokens\t<none>\nmetadata_source\t<none>\n",
            ),
            (
                populated,
                "provider\tprovider\nmodel\tmodel\ncontext_window\t32000\nmax_output_tokens\t4096\nmetadata_source\t<none>\n",
            ),
        ] {
            let mut output = Output::default();
            super::write_model_status(&mut output, &status).unwrap();
            assert_eq!(output.bytes, expected.as_bytes());
            assert_eq!(output.flushes, 1);
            for fail_flush in [false, true] {
                let mut output = Output {
                    fail_write: !fail_flush,
                    fail_flush,
                    ..Output::default()
                };
                assert!(matches!(super::write_model_status(&mut output, &status),
                    Err(CliError::Signal(error)) if error.kind() == std::io::ErrorKind::BrokenPipe));
            }
        }
    }

    #[test]
    fn skill_lists_separate_diagnostics_and_propagate_output_failures() {
        let result = bcode_skill_models::SkillList {
            skills: vec![bcode_skill_models::SkillSummary {
                id: bcode_skill_models::SkillId::new("example"),
                name: "Example".to_owned(),
                description: Some("Instructions".to_owned()),
                version: None,
                source: bcode_skill_models::SkillSource {
                    kind: bcode_skill_models::SkillSourceKind::Bundled,
                    label: "bundled".to_owned(),
                    path: None,
                    precedence: 0,
                },
                activation: bcode_skill_models::SkillActivation::default(),
                diagnostics: vec![],
                disable_model_invocation: false,
            }],
            diagnostics: vec![bcode_skill_models::SkillDiagnostic {
                severity: bcode_skill_models::SkillDiagnosticSeverity::Warning,
                message: "another skill unavailable".to_owned(),
                path: None,
            }],
        };
        let mut output = Output::default();
        let mut diagnostics = Output::default();
        super::write_skill_list(&mut output, &mut diagnostics, &result).unwrap();
        assert_eq!(output.bytes, b"example\tExample\tInstructions\n");
        assert_eq!(diagnostics.bytes, b"Warning: another skill unavailable\n");
        assert_eq!((output.flushes, diagnostics.flushes), (1, 1));
        for fail_diagnostics in [false, true] {
            for fail_flush in [false, true] {
                let mut output = Output::default();
                let mut diagnostics = Output::default();
                let failing = if fail_diagnostics {
                    &mut diagnostics
                } else {
                    &mut output
                };
                failing.fail_write = !fail_flush;
                failing.fail_flush = fail_flush;
                assert!(matches!(
                    super::write_skill_list(&mut output, &mut diagnostics, &result),
                    Err(CliError::Signal(error)) if error.kind() == std::io::ErrorKind::BrokenPipe
                ));
                if !fail_diagnostics {
                    assert!(diagnostics.bytes.is_empty());
                }
            }
        }
    }

    #[test]
    fn worktree_lists_preserve_rows_and_return_output_errors() {
        let rows = [
            bcode_worktree_models::WorktreeInfo {
                path: "main tree".into(),
                is_main: true,
                branch: Some("master".to_owned()),
                commit: Some("abc123".to_owned()),
            },
            bcode_worktree_models::WorktreeInfo {
                path: "linked".into(),
                is_main: false,
                branch: None,
                commit: None,
            },
        ];
        let mut output = Output::default();
        super::write_worktree_list(&mut output, &rows).unwrap();
        assert_eq!(
            output.bytes,
            b"main tree\tmaster\tabc123\tmain\nlinked\t-\t-\tlinked\n"
        );
        assert_eq!(output.flushes, 1);
        let mut empty = Output::default();
        super::write_worktree_list(&mut empty, &[]).unwrap();
        assert!(empty.bytes.is_empty());
        assert_eq!(empty.flushes, 1);
        for fail_write in [true, false] {
            let mut output = Output {
                fail_write,
                fail_flush: !fail_write,
                ..Output::default()
            };
            assert!(matches!(
                super::write_worktree_list(&mut output, &rows),
                Err(CliError::Signal(error)) if error.kind() == std::io::ErrorKind::BrokenPipe
            ));
        }
    }

    #[test]
    fn worktree_path_receipts_preserve_text_and_return_output_errors() {
        let path = std::path::Path::new("workspace with spaces/λ");
        let mut output = Output::default();
        super::write_worktree_path(&mut output, path).expect("path receipt");
        assert_eq!(output.bytes, "workspace with spaces/λ\n".as_bytes());
        assert_eq!(output.flushes, 1);
        for fail_write in [true, false] {
            let mut output = Output {
                fail_write,
                fail_flush: !fail_write,
                ..Output::default()
            };
            assert!(matches!(
                super::write_worktree_path(&mut output, path),
                Err(CliError::Signal(error)) if error.kind() == std::io::ErrorKind::BrokenPipe
            ));
        }
    }

    #[test]
    fn json_writers_handle_short_interrupted_and_partial_writes() {
        struct ShortWriter {
            bytes: Vec<u8>,
            interrupt: bool,
            stop_at: usize,
            failure: Option<std::io::ErrorKind>,
            flushes: usize,
        }
        impl std::io::Write for ShortWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if std::mem::take(&mut self.interrupt) {
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                if self.bytes.len() == self.stop_at {
                    return self.failure.map_or(Ok(0), |kind| Err(kind.into()));
                }
                let count = bytes.len().min(2).min(self.stop_at - self.bytes.len());
                self.bytes.extend_from_slice(&bytes[..count]);
                Ok(count)
            }

            fn flush(&mut self) -> std::io::Result<()> {
                self.flushes += 1;
                Ok(())
            }
        }
        let value = serde_json::json!({ "message": "λ\nquoted\"" });
        for stream in [true, false] {
            let expected = format!(
                "{}\n",
                if stream {
                    serde_json::to_string(&value).unwrap()
                } else {
                    serde_json::to_string_pretty(&value).unwrap()
                }
            );
            for (stop_at, failure) in [
                (usize::MAX, None),
                (0, None),
                (5, None),
                (5, Some(std::io::ErrorKind::BrokenPipe)),
            ] {
                let mut output = ShortWriter {
                    bytes: Vec::new(),
                    interrupt: true,
                    stop_at,
                    failure,
                    flushes: 0,
                };
                let result = if stream {
                    write_json_stream_record(&mut output, &value)
                } else {
                    super::write_json_result(&mut output, &value)
                };
                if stop_at == usize::MAX {
                    result.expect("short and interrupted writes are retried");
                    assert_eq!(output.bytes, expected.as_bytes());
                    assert_eq!(output.flushes, 1);
                } else {
                    let expected_kind = failure.unwrap_or(std::io::ErrorKind::WriteZero);
                    assert!(matches!(result, Err(CliError::Signal(error))
                        if error.kind() == expected_kind));
                    assert_eq!(output.bytes, expected.as_bytes()[..stop_at]);
                    assert_eq!(output.flushes, 0);
                }
            }
        }
    }

    #[test]
    fn canonical_event_export_is_one_lossless_json_line() {
        let event = bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 42,
            timestamp_ms: 123,
            session_id: bcode_session_models::SessionId::new(),
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: "first\nsecond\r\n\"quoted\" λ".to_owned(),
            },
        };
        let mut output = Output::default();
        write_json_stream_record(&mut output, &event).expect("event output");
        let text = String::from_utf8(output.bytes).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert_eq!(output.flushes, 1);
        let decoded: bcode_session_models::SessionEvent = serde_json::from_str(&text).unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn interaction_inspect_selects_without_reinterpreting_schema() {
        use clap::Parser as _;
        let cli =
            super::Cli::try_parse_from(["bcode", "interaction", "inspect", "target"]).unwrap();
        assert!(matches!(cli.command, Some(super::Commands::Interaction {
            command: super::InteractionCommand::Inspect { exchange_id }
        }) if exchange_id == "target"));
    }

    #[test]
    fn interaction_lists_preserve_formats_and_propagate_output_failures() {
        let session_id = bcode_session_models::SessionId::new();
        let exchange = bcode_session_models::PendingToolExchangeSummary {
            session_id,
            request: bcode_session_models::ToolExchangeRequest {
                invocation_id: "invocation".to_owned(),
                exchange_id: "exchange".to_owned(),
                producer_id: "producer".to_owned(),
                schema: "schema".to_owned(),
                schema_version: 7,
                payload: serde_json::json!({ "opaque": [1, 2] }),
                response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
            },
        };
        for exchanges in [vec![], vec![exchange]] {
            for json in [true, false] {
                let mut output = Output::default();
                super::write_interaction_list(&mut output, &exchanges, json).unwrap();
                let expected = if json {
                    format!("{}\n", serde_json::to_string_pretty(&exchanges).unwrap())
                } else if exchanges.is_empty() {
                    "no pending interactions\n".to_owned()
                } else {
                    format!("exchange\t{session_id}\tproducer\tschema\t7\tRequired\n")
                };
                assert_eq!(output.bytes, expected.as_bytes());
                assert_eq!(output.flushes, 1);
                for fail_write in [true, false] {
                    let mut output = Output {
                        fail_write,
                        fail_flush: !fail_write,
                        ..Output::default()
                    };
                    assert!(matches!(
                        super::write_interaction_list(&mut output, &exchanges, json),
                        Err(CliError::Signal(error))
                            if error.kind() == std::io::ErrorKind::BrokenPipe
                    ));
                }
            }
        }
    }

    #[test]
    fn interaction_receipts_preserve_outcomes_and_propagate_output_failures() {
        for resolved in [true, false] {
            for json in [true, false] {
                let mut output = Output::default();
                super::write_interaction_resolution(&mut output, resolved, json)
                    .expect("interaction receipt");
                let expected = if json {
                    format!("{{\n  \"resolved\": {resolved}\n}}\n")
                } else {
                    format!("resolved: {resolved}\n")
                };
                assert_eq!(output.bytes, expected.as_bytes());
                assert_eq!(output.flushes, 1);
                for fail_write in [true, false] {
                    let mut output = Output {
                        fail_write,
                        fail_flush: !fail_write,
                        ..Output::default()
                    };
                    assert!(matches!(
                        super::write_interaction_resolution(&mut output, resolved, json),
                        Err(CliError::Signal(error))
                            if error.kind() == std::io::ErrorKind::BrokenPipe
                    ));
                }
            }
        }
    }

    #[test]
    fn plugin_service_lists_preserve_rows_and_propagate_failures() {
        for services in [
            vec![],
            vec![
                ("example/v1", "example", Some("Example")),
                ("other/v1", "other", None),
            ],
        ] {
            let mut output = Output::default();
            super::write_plugin_services(&mut output, services.iter().copied()).unwrap();
            assert_eq!(output.flushes, 1);
            assert_eq!(
                output.bytes,
                if services.is_empty() {
                    "no plugin services discovered\n"
                } else {
                    "example/v1\texample\tExample\nother/v1\tother\t<unnamed>\n"
                }
                .as_bytes()
            );
            for fail_write in [false, true] {
                let mut output = Output {
                    fail_write,
                    fail_flush: !fail_write,
                    ..Output::default()
                };
                assert!(matches!(
                    super::write_plugin_services(&mut output, services.iter().copied()),
                    Err(CliError::Signal(error)) if error.kind() == std::io::ErrorKind::BrokenPipe
                ));
            }
        }
    }

    #[test]
    fn plugin_delivery_output_preserves_counts_and_propagates_failures() {
        for delivered in [0, 1, usize::MAX] {
            for json in [false, true] {
                let mut output = Output::default();
                super::write_plugin_delivery(&mut output, delivered, json).unwrap();
                assert_eq!(output.flushes, 1);
                let expected = if json {
                    format!("{{\n  \"delivered\": {delivered}\n}}\n")
                } else {
                    format!("delivered\t{delivered}\n")
                };
                assert_eq!(output.bytes, expected.as_bytes());
                for fail_write in [false, true] {
                    let mut output = Output {
                        fail_write,
                        fail_flush: !fail_write,
                        ..Output::default()
                    };
                    assert!(matches!(
                        super::write_plugin_delivery(&mut output, delivered, json),
                        Err(CliError::Signal(error)) if error.kind() == std::io::ErrorKind::BrokenPipe
                    ));
                }
            }
        }
    }

    #[test]
    fn plugin_service_output_preserves_formats_and_propagates_failures() {
        for failed in [false, true] {
            let response = || super::PrintableServiceResponse {
                payload: vec![b'a', 0xff],
                error: failed.then(|| super::PrintableServiceError {
                    code: "test_error".to_owned(),
                    message: "operation failed".to_owned(),
                }),
            };
            for json in [false, true] {
                let mut output = Output::default();
                let result = super::write_service_response(&mut output, response(), json);
                if failed {
                    let error = result.unwrap_err();
                    assert_eq!(error.exit_code(), 1);
                    assert!(matches!(
                        error,
                        CliError::PluginService { code, message }
                            if code == "test_error" && message == "operation failed"
                    ));
                } else {
                    result.unwrap();
                }
                assert_eq!(output.flushes, 1);
                if json {
                    let value: serde_json::Value = serde_json::from_slice(&output.bytes).unwrap();
                    assert_eq!(value["payload"], serde_json::json!([97, 255]));
                    assert_eq!(
                        value["error"],
                        if failed {
                            serde_json::json!({"code": "test_error", "message": "operation failed"})
                        } else {
                            serde_json::Value::Null
                        }
                    );
                } else {
                    assert_eq!(
                        output.bytes,
                        if failed {
                            "ERROR\ttest_error\toperation failed\n"
                        } else {
                            "a\u{fffd}\n"
                        }
                        .as_bytes()
                    );
                }
                for fail_write in [false, true] {
                    let mut output = Output {
                        fail_write,
                        fail_flush: !fail_write,
                        ..Output::default()
                    };
                    assert!(matches!(
                        super::write_service_response(&mut output, response(), json),
                        Err(CliError::Signal(error)) if error.kind() == std::io::ErrorKind::BrokenPipe
                    ));
                }
            }
        }
    }

    #[test]
    fn permission_status_separates_diagnostics_and_propagates_stream_errors() {
        let build = vec!["filesystem.read".to_owned(), "shell.run".to_owned()];
        let messages = vec!["policy degraded".to_owned()];
        let mut output = Output::default();
        let mut diagnostics = Output::default();
        super::write_permission_status(
            &mut output,
            &mut diagnostics,
            "fallback",
            true,
            &build,
            &[],
            &messages,
        )
        .unwrap();
        assert_eq!(output.bytes, b"source: fallback\nusing default policy: true\nbuild enabled tools: filesystem.read, shell.run\nplan enabled tools: \n");
        assert_eq!(diagnostics.bytes, b"policy degraded\n");
        assert_eq!((output.flushes, diagnostics.flushes), (1, 1));
        for fail_stderr in [false, true] {
            for fail_write in [false, true] {
                let mut output = Output::default();
                let mut diagnostics = Output::default();
                let failed = if fail_stderr {
                    &mut diagnostics
                } else {
                    &mut output
                };
                failed.fail_write = fail_write;
                failed.fail_flush = !fail_write;
                assert!(
                    matches!(super::write_permission_status(&mut output, &mut diagnostics, "fallback", true, &build, &[], &messages), Err(CliError::Signal(error)) if error.kind() == std::io::ErrorKind::BrokenPipe)
                );
            }
        }
    }

    #[test]
    fn permission_receipts_preserve_formats_and_report_output_failures() {
        for (value, text) in [
            (serde_json::json!({"resolved": true}), "resolved: true"),
            (serde_json::json!({"resolved": false}), "resolved: false"),
            (serde_json::json!({"resolved_count": 0}), "resolved: 0"),
            (
                serde_json::json!({"config_path": "/config"}),
                "permission rule added: /config",
            ),
        ] {
            for json in [false, true] {
                let mut output = Output::default();
                super::write_permission_receipt(&mut output, &value, format_args!("{text}"), json)
                    .unwrap();
                assert_eq!(output.flushes, 1);
                if json {
                    assert_eq!(
                        serde_json::from_slice::<serde_json::Value>(&output.bytes).unwrap(),
                        value
                    );
                } else {
                    assert_eq!(output.bytes, format!("{text}\n").as_bytes());
                }
                for fail_write in [false, true] {
                    let mut output = Output {
                        fail_write,
                        fail_flush: !fail_write,
                        ..Output::default()
                    };
                    assert!(
                        super::write_permission_receipt(
                            &mut output,
                            &value,
                            format_args!("{text}"),
                            json
                        )
                        .is_err()
                    );
                }
            }
        }
    }

    #[test]
    fn populated_permission_lists_preserve_metadata_order_and_output_errors() {
        let session_id = bcode_session_models::SessionId::new();
        let permissions = ["second", "first"].map(|id| bcode_session_models::PermissionSummary {
            permission_id: id.to_owned(),
            session_id,
            tool_call_id: format!("call-{id}"),
            tool_name: "filesystem.read".to_owned(),
            arguments_json: "{\"path\":\"雪\"}".to_owned(),
            batch: Some(bcode_session_models::PermissionBatchCorrelation {
                batch_id: "batch".to_owned(),
                call_index: usize::from(id == "first"),
                call_count: 2,
            }),
            agent_id: "build".to_owned(),
            policy_source: Some("plugin.policy".to_owned()),
            policy_reason: Some("approval required".to_owned()),
            can_remember_policy: true,
        });
        for json in [false, true] {
            let mut output = Output::default();
            super::write_permission_list(&mut output, &permissions, json).unwrap();
            assert_eq!(output.flushes, 1);
            if json {
                let decoded: Vec<bcode_session_models::PermissionSummary> =
                    serde_json::from_slice(&output.bytes).unwrap();
                assert_eq!(decoded, permissions);
            } else {
                let expected = format!(
                    "second\t{session_id}\tcall-second\tfilesystem.read\tbuild\t{{\"path\":\"雪\"}}\nfirst\t{session_id}\tcall-first\tfilesystem.read\tbuild\t{{\"path\":\"雪\"}}\n"
                );
                assert_eq!(output.bytes, expected.as_bytes());
            }
            for fail_write in [false, true] {
                let mut output = Output {
                    fail_write,
                    fail_flush: !fail_write,
                    ..Output::default()
                };
                assert!(
                    matches!(super::write_permission_list(&mut output, &permissions, json), Err(CliError::Signal(error)) if error.kind() == std::io::ErrorKind::BrokenPipe)
                );
            }
        }
    }

    #[test]
    fn empty_permission_lists_flush_and_preserve_formats() {
        for json in [false, true] {
            let mut output = Output::default();
            super::write_permission_list(&mut output, &[], json).unwrap();
            assert_eq!(output.flushes, 1);
            assert_eq!(
                output.bytes,
                if json {
                    b"[]\n".as_slice()
                } else {
                    b"".as_slice()
                }
            );
            let mut output = Output {
                fail_flush: true,
                ..Output::default()
            };
            assert!(super::write_permission_list(&mut output, &[], json).is_err());
        }
    }

    #[test]
    fn follow_up_receipt_propagates_output_failures() {
        let mut output = Output::default();
        super::write_follow_up_disposition(&mut output, &"queued").unwrap();
        assert_eq!(output.bytes, b"\"queued\"\n");
        assert_eq!(output.flushes, 1);
        for fail_write in [false, true] {
            let mut output = Output {
                fail_write,
                fail_flush: !fail_write,
                ..Output::default()
            };
            assert!(matches!(
                super::write_follow_up_disposition(&mut output, &"queued"),
                Err(CliError::Signal(error)) if error.kind() == std::io::ErrorKind::BrokenPipe
            ));
        }
    }

    #[test]
    fn runtime_work_outputs_preserve_partial_spans_and_failures() {
        use bcode_session_models::{
            RuntimeWorkKind, RuntimeWorkSnapshot, RuntimeWorkStatus, WorkId,
        };
        let work = vec![RuntimeWorkSnapshot {
            work_id: WorkId::new("w"),
            kind: RuntimeWorkKind::Tool,
            label: "inspect λ".into(),
            tool_call_id: Some("call".into()),
            status: RuntimeWorkStatus::Cancelling,
            cancellable: true,
        }];
        let spans = vec![bcode_client::RuntimeWorkSpan {
            work_id: WorkId::new("w"),
            parent_work_id: None,
            label: String::new(),
            status: Some(RuntimeWorkStatus::Failed),
            started_at_ms: None,
            finished_at_ms: Some(40),
            cancelled: false,
            message: Some("failed λ".into()),
        }];
        for history in [false, true] {
            for empty in [false, true] {
                for json in [false, true] {
                    let write = |output: &mut Output| {
                        if history {
                            super::write_runtime_work_history(
                                output,
                                if empty { &[] } else { &spans },
                                json,
                            )
                        } else {
                            super::write_runtime_work_list(
                                output,
                                if empty { &[] } else { &work },
                                json,
                            )
                        }
                    };
                    let mut output = Output::default();
                    write(&mut output).unwrap();
                    assert_eq!(output.flushes, 1);
                    if json {
                        let expected = if empty {
                            serde_json::json!([])
                        } else if history {
                            serde_json::to_value(&spans).unwrap()
                        } else {
                            serde_json::to_value(&work).unwrap()
                        };
                        assert_eq!(
                            serde_json::from_slice::<serde_json::Value>(&output.bytes).unwrap(),
                            expected
                        );
                    } else {
                        let expected = if empty {
                            ""
                        } else if history {
                            "w status=Some(Failed) cancelled=false duration_ms=None parent=- label= message=failed λ\n"
                        } else {
                            "w Tool Cancelling inspect λ cancellable=true\n"
                        };
                        assert_eq!(output.bytes, expected.as_bytes());
                    }
                    for fail_write in [false, true] {
                        if fail_write && empty && !json {
                            continue;
                        }
                        let mut output = Output {
                            fail_write,
                            fail_flush: !fail_write,
                            ..Output::default()
                        };
                        let error = write(&mut output).unwrap_err();
                        assert_eq!(error.exit_code(), 1);
                        assert!(
                            matches!(error, CliError::Signal(error) if error.kind() == std::io::ErrorKind::BrokenPipe)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn cancellation_receipts_preserve_admission_and_propagate_output_failures() {
        for kind in ["turn", "runtime_work"] {
            for cancelled in [false, true] {
                for json in [false, true] {
                    let mut output = Output::default();
                    super::write_cancellation_result(&mut output, kind, cancelled, json).unwrap();
                    assert_eq!(output.flushes, 1);
                    if json {
                        let value: serde_json::Value =
                            serde_json::from_slice(&output.bytes).unwrap();
                        assert_eq!(
                            value,
                            serde_json::json!({
                                "kind": kind, "cancellation_requested": cancelled,
                            })
                        );
                    } else {
                        let expected = if cancelled {
                            format!("{kind} cancellation requested\n")
                        } else {
                            format!("no active {kind}\n")
                        };
                        assert_eq!(output.bytes, expected.as_bytes());
                    }
                    for fail_write in [false, true] {
                        let mut output = Output {
                            fail_write,
                            fail_flush: !fail_write,
                            ..Output::default()
                        };
                        let error =
                            super::write_cancellation_result(&mut output, kind, cancelled, json)
                                .unwrap_err();
                        assert_eq!(error.exit_code(), 1);
                        assert!(matches!(error, CliError::Signal(error)
                            if error.kind() == std::io::ErrorKind::BrokenPipe));
                    }
                }
            }
        }
    }

    #[test]
    fn turn_admission_output_preserves_outcomes_and_reports_io_failures() {
        use bcode_session_models::{SessionId, TurnAdmission, TurnReceipt, TurnRejectionReason};

        let session_id = SessionId::new();
        let receipt = TurnReceipt::from_accepted_event(session_id, 42);
        for (admission, expected_exit) in [
            (TurnAdmission::Accepted(receipt.clone()), None),
            (TurnAdmission::Existing(receipt.clone()), None),
            (TurnAdmission::Deferred(receipt.clone()), None),
            (TurnAdmission::CancelledBeforeStart(receipt), Some(4)),
            (
                TurnAdmission::Rejected(TurnRejectionReason::ExecutionPolicy),
                Some(3),
            ),
            (
                TurnAdmission::Rejected(TurnRejectionReason::SessionUnavailable),
                Some(1),
            ),
        ] {
            for json in [false, true] {
                let mut output = Output::default();
                let result = super::write_turn_admission(&mut output, session_id, &admission, json);
                assert_eq!(
                    result.err().as_ref().map(CliError::exit_code),
                    expected_exit
                );
                assert_eq!(output.flushes, 1);
                if json {
                    let value: serde_json::Value = serde_json::from_slice(&output.bytes).unwrap();
                    assert_eq!(
                        value,
                        serde_json::json!({
                            "session_id": session_id, "admission": admission,
                        })
                    );
                } else {
                    assert_eq!(output.bytes, format!("{admission:?}\n").as_bytes());
                }
                for fail_write in [false, true] {
                    let mut output = Output {
                        fail_write,
                        fail_flush: !fail_write,
                        ..Output::default()
                    };
                    let error =
                        super::write_turn_admission(&mut output, session_id, &admission, json)
                            .unwrap_err();
                    assert_eq!(error.exit_code(), 1);
                    assert!(matches!(error, CliError::Signal(error)
                        if error.kind() == std::io::ErrorKind::BrokenPipe));
                }
            }
        }
    }

    #[test]
    fn finite_result_preserves_pretty_json_and_returns_output_failures() {
        let value = serde_json::json!({ "result": [1, 2], "message": "hello\nworld" });
        let mut output = Output::default();
        super::write_json_result(&mut output, &value).expect("finite result");
        assert_eq!(output.flushes, 1);
        assert_eq!(
            String::from_utf8(output.bytes).expect("UTF-8"),
            format!("{}\n", serde_json::to_string_pretty(&value).unwrap())
        );
        for fail_write in [true, false] {
            let mut output = Output {
                fail_write,
                fail_flush: !fail_write,
                ..Output::default()
            };
            assert!(matches!(
                super::write_json_result(&mut output, &value),
                Err(CliError::Signal(error)) if error.kind() == std::io::ErrorKind::BrokenPipe
            ));
        }
    }

    #[test]
    fn stream_records_are_compact_delimited_and_flushed_individually() {
        let mut output = Output::default();
        let value = serde_json::json!({ "text": "first\nsecond" });
        for expected_flushes in 1..=2 {
            write_json_stream_record(&mut output, &value).expect("record written");
            assert_eq!(output.flushes, expected_flushes);
        }
        let text = String::from_utf8(output.bytes).expect("UTF-8 output");
        assert!(text.ends_with('\n'));
        assert_eq!(text.lines().count(), 2);
        for line in text.lines() {
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(line).unwrap(),
                value
            );
        }
    }

    #[test]
    fn stream_write_and_flush_failures_are_returned_without_panicking() {
        for fail_write in [true, false] {
            let mut output = Output {
                fail_write,
                fail_flush: !fail_write,
                ..Output::default()
            };
            let error = write_json_stream_record(&mut output, &true).expect_err("broken pipe");
            assert!(matches!(error, CliError::Signal(error)
                if error.kind() == std::io::ErrorKind::BrokenPipe));
        }
    }

    #[test]
    fn stream_serialization_failure_writes_nothing() {
        struct Invalid;
        impl serde::Serialize for Invalid {
            fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("test serialization failure"))
            }
        }
        let mut output = Output::default();
        let error = write_json_stream_record(&mut output, &Invalid).expect_err("invalid value");
        assert!(matches!(error, CliError::Json(_)));
        assert!(output.bytes.is_empty());
        assert_eq!(output.flushes, 0);
        let error =
            super::write_json_result(&mut output, &Invalid).expect_err("invalid finite value");
        assert!(matches!(error, CliError::Json(_)));
        assert!(output.bytes.is_empty());
        assert_eq!(output.flushes, 0);
    }
}

#[cfg(test)]
mod interaction_cli_tests {
    use super::{
        Cli, CliError, Commands, InteractionCommand, MAX_CLI_INTERACTION_JSON_BYTES,
        read_bounded_interaction_json, read_json_from_reader,
    };
    use clap::Parser as _;
    use std::io::Read as _;

    #[test]
    fn workflow_source_file_enforces_size_and_encoding() {
        let root = tempfile::tempdir().expect("source fixture");
        let path = root.path().join("source.yaml");
        let limit = bcode_workflow::MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES;
        let bytes = vec![b' '; limit];
        std::fs::write(&path, &bytes).unwrap();
        let source = super::read_workflow_source_file(&path, None).expect("exact limit");
        assert_eq!(source.source.len(), limit);
        assert_eq!(
            source.source_format,
            bcode_workflow::WorkflowSourceFormat::Yaml
        );
        std::fs::write(&path, vec![b' '; limit + 1]).unwrap();
        let error = super::read_workflow_source_file(&path, None)
            .err()
            .expect("oversized file");
        assert!(error.to_string().contains("workflow source exceeds"));
        std::fs::write(&path, [0xff]).unwrap();
        let error = super::read_workflow_source_file(&path, None)
            .err()
            .expect("invalid encoding");
        assert!(error.to_string().contains("not valid UTF-8"));
        assert_eq!(std::fs::read(&path).unwrap(), [0xff]);
    }

    #[test]
    fn workflow_input_value_accepts_inline_and_file_and_bounds_inline() {
        let value = serde_json::json!({"answer": 42});
        assert_eq!(
            super::read_workflow_input_value(&value.to_string()).unwrap(),
            value
        );
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("input.json");
        std::fs::write(&path, value.to_string()).unwrap();
        assert_eq!(
            super::read_workflow_input_value(path.to_str().unwrap()).unwrap(),
            value
        );
        let oversized = " ".repeat(bcode_workflow::MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES + 1);
        let error = super::read_workflow_input_value(&oversized).unwrap_err();
        assert!(error.to_string().contains("workflow JSON exceeds"));
    }

    #[test]
    fn workflow_source_reader_enforces_limit_during_read() {
        let bytes = b"name: example";
        assert_eq!(
            super::read_bytes_with_limit(bytes.as_slice(), bytes.len(), "workflow source")
                .expect("exact limit"),
            bytes
        );
        let mut reader = std::io::repeat(b'x').take(1_000_000);
        let error = super::read_bytes_with_limit(&mut reader, 32, "workflow source")
            .expect_err("oversized source");
        assert_eq!(reader.limit(), 1_000_000 - 33);
        assert!(
            error
                .to_string()
                .contains("workflow source exceeds 32 bytes")
        );
    }

    #[test]
    fn interaction_json_reader_consumes_only_limit_plus_sentinel() {
        let limit = 32;
        let mut reader = std::io::repeat(b' ').take(1_000_000);
        let error = read_json_from_reader(&mut reader, limit, "interaction JSON")
            .expect_err("oversized stream must be rejected");
        assert!(matches!(error, CliError::InvalidArguments(_)));
        assert_eq!(reader.limit(), 1_000_000 - 33);
    }

    #[test]
    fn interaction_json_reader_accepts_exact_limit_and_rejects_next_byte() {
        let bytes = br#"{"answer":true}"#;
        let parsed = read_json_from_reader(bytes.as_slice(), bytes.len(), "interaction JSON")
            .expect("exact-size valid JSON");
        assert_eq!(parsed, serde_json::json!({ "answer": true }));
        let error = read_json_from_reader(bytes.as_slice(), bytes.len() - 1, "interaction JSON")
            .expect_err("last byte crosses limit");
        assert!(matches!(error, CliError::InvalidArguments(_)));
    }

    #[test]
    fn interaction_json_reader_rejects_io_failure_after_valid_prefix() {
        struct FailedTail {
            prefix: std::io::Cursor<Vec<u8>>,
            interrupted: bool,
        }
        impl std::io::Read for FailedTail {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if std::mem::take(&mut self.interrupted) {
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                let count = self.prefix.read(buffer)?;
                if count == 0 {
                    Err(std::io::ErrorKind::ConnectionReset.into())
                } else {
                    Ok(count)
                }
            }
        }
        let error = read_json_from_reader(
            FailedTail {
                prefix: std::io::Cursor::new(br#"{"answer":true}"#.to_vec()),
                interrupted: true,
            },
            32,
            "interaction JSON",
        )
        .expect_err("valid prefix does not establish complete input");
        assert!(matches!(error, CliError::Signal(error)
            if error.kind() == std::io::ErrorKind::ConnectionReset));
    }

    #[test]
    fn interaction_json_reader_preserves_io_failure() {
        struct FailedReader;
        impl std::io::Read for FailedReader {
            fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("test read failure"))
            }
        }
        let error = read_json_from_reader(FailedReader, 32, "interaction JSON")
            .expect_err("read failure must not become a JSON result");
        assert!(matches!(error, CliError::Signal(_)));
    }

    #[test]
    fn interaction_list_session_scope_parses_and_preserves_exchange_metadata() {
        let selected = bcode_session_models::SessionId::new();
        let other = bcode_session_models::SessionId::new();
        let parsed = Cli::try_parse_from([
            "bcode",
            "interaction",
            "list",
            "--session-id",
            &selected.to_string(),
            "--json",
        ])
        .expect("scoped interaction list");
        assert!(matches!(parsed.command, Some(Commands::Interaction {
            command: InteractionCommand::List { session_id: Some(id), json: true }
        }) if id == selected));
        let make_exchange =
            |session_id, exchange_id: &str| bcode_session_models::PendingToolExchangeSummary {
                session_id,
                request: bcode_session_models::ToolExchangeRequest {
                    invocation_id: "invocation".to_owned(),
                    exchange_id: exchange_id.to_owned(),
                    producer_id: "test.plugin".to_owned(),
                    schema: "test.exchange".to_owned(),
                    schema_version: 7,
                    payload: serde_json::json!({ "opaque": [1, 2] }),
                    response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
                },
            };
        let original = vec![
            make_exchange(selected, "first"),
            make_exchange(other, "other"),
            make_exchange(selected, "last"),
        ];
        let mut exchanges = original.clone();
        super::filter_session_exchanges(&mut exchanges, None);
        assert_eq!(exchanges, original);
        super::filter_session_exchanges(&mut exchanges, Some(selected));
        assert_eq!(exchanges, vec![original[0].clone(), original[2].clone()]);
        super::filter_session_exchanges(&mut exchanges, Some(other));
        assert!(exchanges.is_empty());
    }

    #[test]
    fn interaction_json_is_validated_before_daemon_work() {
        let temp = tempfile::NamedTempFile::new().expect("temporary payload");
        std::fs::write(temp.path(), b"not-json").expect("write malformed payload");
        let error = read_bounded_interaction_json(temp.path()).expect_err("JSON must be valid");
        assert!(matches!(error, CliError::Json(_)));
    }

    #[test]
    fn interaction_json_has_a_domain_specific_bound() {
        let temp = tempfile::NamedTempFile::new().expect("temporary payload");
        std::fs::write(temp.path(), vec![b' '; MAX_CLI_INTERACTION_JSON_BYTES + 1])
            .expect("write oversized payload");
        let error =
            read_bounded_interaction_json(temp.path()).expect_err("payload must be bounded");
        assert!(matches!(
            error,
            CliError::InvalidArguments(message)
                if message == format!(
                    "interaction JSON exceeds {MAX_CLI_INTERACTION_JSON_BYTES} bytes"
                )
        ));
    }

    #[test]
    fn invocation_input_reads_typed_envelopes_and_reports_acceptance() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let input = serde_json::json!({
            "invocation_id": "call", "input_id": "input", "producer_id": "plugin",
            "schema": "plugin.input", "schema_version": 2, "payload": [true, "λ"]
        });
        std::fs::write(file.path(), serde_json::to_vec(&input).unwrap()).unwrap();
        let decoded = super::read_invocation_input(file.path()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), input);
        std::fs::write(file.path(), r#"{"schema_version":"private-sentinel"}"#).unwrap();
        let error = super::read_invocation_input(file.path()).unwrap_err();
        assert!(matches!(error, CliError::InvalidArguments(_)));
        assert!(!error.to_string().contains("private-sentinel"));
        for json in [false, true] {
            let mut output = Vec::new();
            super::write_invocation_input_receipt(&mut output, json).unwrap();
            if json {
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&output).unwrap(),
                    serde_json::json!({"accepted": true})
                );
            } else {
                assert_eq!(output, b"Invocation input accepted\n");
            }
        }
    }

    #[test]
    fn invocation_input_command_parses() {
        let session_id = bcode_session_models::SessionId::new();
        let parsed = Cli::try_parse_from([
            "bcode",
            "interaction",
            "input",
            &session_id.to_string(),
            "--payload",
            "-",
            "--json",
        ])
        .unwrap();
        assert!(matches!(parsed.command, Some(Commands::Interaction {
            command: InteractionCommand::Input { session_id: actual, payload, json: true }
        }) if actual == session_id && payload == std::path::Path::new("-")));
    }

    #[test]
    fn interaction_commands_parse_structured_list_respond_and_cancel_paths() {
        let list = Cli::try_parse_from(["bcode", "interaction", "list", "--json"])
            .expect("interaction list parses");
        assert!(matches!(
            list.command,
            Some(Commands::Interaction {
                command: InteractionCommand::List {
                    session_id: None,
                    json: true
                }
            })
        ));

        let respond = Cli::try_parse_from([
            "bcode",
            "interaction",
            "respond",
            "exchange-1",
            "--payload",
            "-",
            "--json",
        ])
        .expect("interaction response parses");
        assert!(matches!(
            respond.command,
            Some(Commands::Interaction {
                command: InteractionCommand::Respond {
                    exchange_id,
                    payload,
                    json: true,
                }
            }) if exchange_id == "exchange-1" && payload == std::path::Path::new("-")
        ));

        let cancel =
            Cli::try_parse_from(["bcode", "interaction", "cancel", "exchange-1", "--json"])
                .expect("interaction cancel parses");
        assert!(matches!(
            cancel.command,
            Some(Commands::Interaction {
                command: InteractionCommand::Cancel {
                    exchange_id,
                    json: true,
                }
            }) if exchange_id == "exchange-1"
        ));
    }
}

#[cfg(test)]
mod latency_diagnosis_tests {
    use super::{DiagnosticSeverity, diagnostic_observations};
    use bcode_ipc::ServerStatus;
    use bcode_metrics::{HistogramSnapshot, MetricsSnapshot};

    fn status_with_metrics(metrics: MetricsSnapshot) -> ServerStatus {
        ServerStatus {
            connected_client_count: 0,
            sessions: Vec::new(),
            session_catalog_loaded: false,
            session_catalog_status: bcode_ipc::SessionCatalogStatus::default(),
            session_catalog_sources: Vec::new(),
            session_catalog_revision: 0,
            selected_provider_plugin_id: None,
            selected_model_id: None,
            plugin_runtime: Vec::new(),
            daemon: bcode_ipc::DaemonStatus::default(),
            metrics,
            metrics_report: Box::default(),
            active_runtime_work: Vec::new(),
            idle_shutdown_blocker: None,
        }
    }

    fn status_with_histogram(key: &str, max_ms: u64) -> ServerStatus {
        let mut metrics = MetricsSnapshot::default();
        metrics.histograms.insert(
            key.to_owned(),
            HistogramSnapshot {
                count: 1,
                sum: max_ms,
                min: Some(max_ms),
                max: Some(max_ms),
                buckets: Vec::new(),
            },
        );
        status_with_metrics(metrics)
    }

    #[test]
    fn slow_time_to_first_output_is_reported() {
        let observations = diagnostic_observations(&status_with_histogram(
            "model.provider.first_output_latency_ms",
            12_000,
        ));

        let observation = observations
            .iter()
            .find(|observation| observation.code == "slow_time_to_first_output")
            .expect("slow first output should be diagnosed");
        assert!(matches!(observation.severity, DiagnosticSeverity::Warning));
        assert!(observation.message.contains("12000"));
    }

    #[test]
    fn fast_time_to_first_output_is_not_reported() {
        let observations = diagnostic_observations(&status_with_histogram(
            "model.provider.first_output_latency_ms",
            250,
        ));

        assert!(
            !observations
                .iter()
                .any(|observation| observation.code == "slow_time_to_first_output"),
            "fast first output must not be flagged"
        );
    }

    #[test]
    fn high_poll_idle_wait_is_reported_as_host_owned_latency() {
        let observations = diagnostic_observations(&status_with_histogram(
            "model.provider.poll_idle_wait_duration_ms",
            9_000,
        ));

        let observation = observations
            .iter()
            .find(|observation| observation.code == "high_poll_idle_wait")
            .expect("host-owned poll wait should be diagnosed");
        assert!(
            observation
                .message
                .contains("waiting between provider polls")
        );
    }

    #[test]
    fn absent_latency_metrics_produce_no_observations() {
        let observations =
            diagnostic_observations(&status_with_metrics(MetricsSnapshot::default()));

        assert!(
            observations.is_empty(),
            "a status without metrics must not invent latency findings"
        );
    }

    fn status_with_average(key: &str, count: u64, average_ms: u64) -> ServerStatus {
        let mut metrics = MetricsSnapshot::default();
        metrics.histograms.insert(
            key.to_owned(),
            HistogramSnapshot {
                count,
                sum: count * average_ms,
                min: Some(average_ms),
                max: Some(average_ms),
                buckets: Vec::new(),
            },
        );
        status_with_metrics(metrics)
    }

    #[test]
    fn high_average_session_append_latency_requires_enough_samples() {
        let few = diagnostic_observations(&status_with_average(
            "session.actor.append_event.db_append_duration_ms",
            10,
            40,
        ));
        assert!(
            !few.iter()
                .any(|observation| observation.code == "high_average_session_append_latency"),
            "a handful of slow appends is not a trend"
        );

        let sustained = diagnostic_observations(&status_with_average(
            "session.actor.append_event.db_append_duration_ms",
            500,
            19,
        ));
        let observation = sustained
            .iter()
            .find(|observation| observation.code == "high_average_session_append_latency")
            .expect("sustained slow appends should be diagnosed");
        assert!(observation.message.contains("average=19ms"));
        assert!(observation.message.contains("statement count"));
    }

    #[test]
    fn repeated_slow_provider_control_plane_calls_are_reported() {
        let observations = diagnostic_observations(&status_with_average(
            "model.provider.service.duration_ms",
            737,
            473,
        ));
        let observation = observations
            .iter()
            .find(|observation| observation.code == "slow_model_provider_service_calls")
            .expect("slow models/capabilities calls should be diagnosed");
        assert!(observation.message.contains("several times per turn"));
    }

    #[test]
    fn session_search_retry_storm_is_reported_from_failure_ratio() {
        let mut metrics = MetricsSnapshot::default();
        metrics.counters.insert(
            "server.session_search.ingestion_session_failed_total".to_owned(),
            173_050,
        );
        metrics.counters.insert(
            "server.session_search.ingestion_session_requeued_total".to_owned(),
            173_049,
        );
        metrics.counters.insert(
            "server.session_search.ingestion_session_completed_total".to_owned(),
            12,
        );
        let observations = diagnostic_observations(&status_with_metrics(metrics));
        let observation = observations
            .iter()
            .find(|observation| observation.code == "session_search_ingestion_retry_storm")
            .expect("retry storm should be diagnosed");
        assert!(matches!(observation.severity, DiagnosticSeverity::Warning));
        assert!(observation.message.contains("search-status"));

        // Ordinary transient failures alongside healthy completions are not a storm.
        let mut healthy = MetricsSnapshot::default();
        healthy.counters.insert(
            "server.session_search.ingestion_session_failed_total".to_owned(),
            150,
        );
        healthy.counters.insert(
            "server.session_search.ingestion_session_completed_total".to_owned(),
            1_000,
        );
        assert!(
            !diagnostic_observations(&status_with_metrics(healthy))
                .iter()
                .any(|observation| observation.code == "session_search_ingestion_retry_storm")
        );
    }
}

#[cfg(test)]
mod client_timeout_cli_tests {
    use super::{Cli, config_override_from_matches, execution_mode_launch_options};
    use clap::{CommandFactory as _, Parser as _};
    use std::sync::Mutex;

    static CONFIG_OVERRIDE_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn context_selection_is_global_and_accepts_user_defined_names() {
        for args in [
            vec!["bcode", "--context", "custom-team"],
            vec!["bcode", "session", "list", "--context", "custom-team"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert_eq!(cli.context.as_deref(), Some("custom-team"));
        }
    }

    #[test]
    fn request_timeout_override_is_visible_to_default_client() {
        let _lock = CONFIG_OVERRIDE_LOCK
            .lock()
            .expect("config override lock should not be poisoned");
        let matches = Cli::command()
            .try_get_matches_from(["bcode", "--request-timeout-secs", "60"])
            .expect("timeout override should parse");
        let guard = config_override_from_matches(&matches)
            .expect("timeout option should install a config override");

        let client = bcode_client::BcodeClient::default_endpoint();

        assert_eq!(client.request_timeout().as_secs(), 60);
        drop(guard);
    }

    #[test]
    fn request_timeout_override_accepts_positive_seconds() {
        let cli = Cli::try_parse_from(["bcode", "--request-timeout-secs", "60"])
            .expect("positive timeout should parse");

        assert_eq!(cli.request_timeout_secs, Some(60));
    }

    #[test]
    fn state_location_flags_parse_and_are_mutually_exclusive() {
        let root = Cli::try_parse_from(["bcode", "--state-root", "/volumes/big/bcode-state"])
            .expect("state root should parse");
        assert_eq!(
            root.state_root.as_deref(),
            Some(std::path::Path::new("/volumes/big/bcode-state"))
        );
        assert_eq!(root.state_profile, None);

        let profile = Cli::try_parse_from(["bcode", "--state-profile", "big"])
            .expect("state profile should parse");
        assert_eq!(profile.state_profile.as_deref(), Some("big"));
        assert_eq!(profile.state_root, None);

        Cli::try_parse_from([
            "bcode",
            "--state-root",
            "/volumes/big/bcode-state",
            "--state-profile",
            "big",
        ])
        .expect_err("state root and state profile must be mutually exclusive");
    }

    #[test]
    fn state_location_flags_are_global_for_subcommands() {
        let cli = Cli::try_parse_from([
            "bcode",
            "session",
            "list",
            "--state-root",
            "/volumes/big/bcode-state",
        ])
        .expect("global state root should parse after a subcommand");

        assert_eq!(
            cli.state_root.as_deref(),
            Some(std::path::Path::new("/volumes/big/bcode-state"))
        );
    }

    #[test]
    fn session_relocate_parses_destination_selection_and_options() {
        let session_id = bcode_session_models::SessionId::new();
        let cli = Cli::try_parse_from([
            "bcode",
            "session",
            "relocate",
            "--to",
            "/volumes/big/bcode-state",
            "--session",
            &session_id.to_string(),
            "--dry-run",
            "--json",
        ])
        .expect("relocate should parse");

        let Some(super::Commands::Session {
            command:
                super::SessionCommand::Relocate {
                    to,
                    to_profile,
                    session,
                    dry_run,
                    json,
                },
        }) = cli.command
        else {
            panic!("expected a session relocate command");
        };
        assert_eq!(
            to.as_deref(),
            Some(std::path::Path::new("/volumes/big/bcode-state"))
        );
        assert_eq!(to_profile, None);
        assert_eq!(session, vec![session_id]);
        assert!(dry_run);
        assert!(json);
    }

    #[test]
    fn session_relocate_destination_selectors_are_mutually_exclusive() {
        Cli::try_parse_from([
            "bcode",
            "session",
            "relocate",
            "--to",
            "/volumes/big/bcode-state",
            "--to-profile",
            "big",
        ])
        .expect_err("--to and --to-profile must be mutually exclusive");
    }

    #[test]
    fn state_prune_staging_parses_inventory_and_apply_modes() {
        let inventory = Cli::try_parse_from(["bcode", "state", "prune-staging"])
            .expect("prune-staging should parse");
        let Some(super::Commands::State {
            command: super::StateCommand::PruneStaging { root, apply, json },
        }) = inventory.command
        else {
            panic!("expected a state prune-staging command");
        };
        assert_eq!(root, None);
        assert!(!apply, "inventory must be the default, not deletion");
        assert!(!json);

        let applied = Cli::try_parse_from([
            "bcode",
            "state",
            "prune-staging",
            "--root",
            "/volumes/big/bcode-state",
            "--apply",
            "--json",
        ])
        .expect("prune-staging --apply should parse");
        let Some(super::Commands::State {
            command: super::StateCommand::PruneStaging { root, apply, json },
        }) = applied.command
        else {
            panic!("expected a state prune-staging command");
        };
        assert_eq!(
            root.as_deref(),
            Some(std::path::Path::new("/volumes/big/bcode-state"))
        );
        assert!(apply);
        assert!(json);
    }

    #[test]
    fn state_locations_inventory_parses() {
        let cli = Cli::try_parse_from(["bcode", "state", "locations", "--json"])
            .expect("state locations should parse");

        let Some(super::Commands::State {
            command: super::StateCommand::Locations { json },
        }) = cli.command
        else {
            panic!("expected a state locations command");
        };
        assert!(json);
    }

    #[test]
    fn execution_mode_flags_map_to_typed_launch_options() {
        let yolo =
            Cli::try_parse_from(["bcode", "tui", "--yolo"]).expect("yolo alias should parse");
        assert_eq!(
            yolo.launch_options(),
            bcode_tui::TuiLaunchOptions {
                permission_mode: bcode_session_models::TurnPermissionMode::Bypass,
                tool_policy: bcode_session_models::TurnToolPolicy::Enabled,
            }
        );

        let no_tools = Cli::try_parse_from(["bcode", "--disable-all-tools"])
            .expect("no-tools canonical flag should parse");
        assert_eq!(
            no_tools.launch_options(),
            bcode_tui::TuiLaunchOptions {
                permission_mode: bcode_session_models::TurnPermissionMode::Enforce,
                tool_policy: bcode_session_models::TurnToolPolicy::Disabled,
            }
        );
    }

    #[test]
    fn execution_mode_flags_conflict_and_are_scoped_to_turn_entry_points() {
        assert!(Cli::try_parse_from(["bcode", "--yolo", "--no-tools"]).is_err());
        let maintenance = Cli::try_parse_from(["bcode", "server", "status", "--yolo"])
            .expect("global flag parses before applicability validation");
        assert!(!maintenance.supports_execution_mode());
        let onboarding = Cli::try_parse_from(["bcode", "--onboard", "--yolo"])
            .expect("global onboarding flag should parse before applicability validation");
        assert!(!onboarding.supports_execution_mode());
    }

    #[test]
    fn direct_send_uses_the_same_exact_execution_options() {
        assert_eq!(
            execution_mode_launch_options(true, false).turn_execution_options(),
            bcode_session_models::TurnExecutionOptions {
                permission_mode: bcode_session_models::TurnPermissionMode::Bypass,
                tools: bcode_session_models::TurnToolPolicy::Enabled,
                ..bcode_session_models::TurnExecutionOptions::default()
            }
        );
        assert_eq!(
            execution_mode_launch_options(false, true).turn_execution_options(),
            bcode_session_models::TurnExecutionOptions {
                permission_mode: bcode_session_models::TurnPermissionMode::Enforce,
                tools: bcode_session_models::TurnToolPolicy::Disabled,
                ..bcode_session_models::TurnExecutionOptions::default()
            }
        );
    }

    #[test]
    fn request_timeout_override_rejects_zero() {
        assert!(Cli::try_parse_from(["bcode", "--request-timeout-secs", "0"]).is_err());
    }
}
