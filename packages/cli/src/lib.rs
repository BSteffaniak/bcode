#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Command-line interface for Bcode.

mod plugin_cli;
mod session_cleanup;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bcode_client::{BcodeClient, ClientError, DaemonAvailability};
use bcode_config::AuthMode;
use bcode_ipc::{Event, PermissionSummary, ServerStatus, default_endpoint};
use bcode_model_provider_runtime::{
    BlockingModelProviderInvoker, SingleTurnRequest, SingleTurnStatus, run_single_turn_blocking,
};
use bcode_plugin_sdk::path::{display, display_from_current_dir};
use bcode_session_import::{
    DiscoverImportableSessionsRequest, DiscoverImportableSessionsResponse,
    ListImportSourcesResponse, OP_DISCOVER_IMPORTABLE_SESSIONS, OP_LIST_IMPORT_SOURCES,
    SESSION_IMPORT_INTERFACE_ID,
};
use bcode_session_models::{
    SessionEvent, SessionEventKind, SessionHistoryCursor, SessionHistoryDirection,
    SessionHistoryQuery, SessionId,
};
use bcode_worktree_models::WorktreeCreateRequest;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use rand::TryRngCore as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
    #[error("daemon lifecycle error: {0}")]
    DaemonLifecycle(#[from] bcode_daemon_lifecycle::DaemonLifecycleError),
    #[error("daemon start error: {0}")]
    DaemonStart(#[from] bcode_daemon_lifecycle::DaemonStartError),
    #[error("config error: {0}")]
    Config(#[from] bcode_config::ConfigError),
    #[error("server error: {0}")]
    Server(#[from] bcode_server::ServerError),
    #[error("session database error: {0}")]
    SessionDb(#[from] bcode_session::db::SessionDbError),
    #[error("session lease error: {0}")]
    SessionLease(#[from] bcode_session::lease::SessionLeaseError),
    #[error("session store error: {0}")]
    SessionStore(#[from] bcode_session::SessionStoreError),
    #[error("session repair error: {0}")]
    SessionRepair(#[from] bcode_session::repair::SessionRepairError),
    #[error("semantic migration audit error: {0}")]
    SemanticMigrationAudit(#[from] bcode_session::semantic_migration::SemanticMigrationAuditError),
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
    #[error("Web renderer error: {0}")]
    WebRender(String),
    #[error("sshenv error: {0}")]
    Sshenv(String),
    #[error("interrupted: {0}")]
    Signal(#[from] std::io::Error),
    #[error("--new cannot be combined with a subcommand")]
    NewSessionWithCommand,
    #[error("{0}")]
    LoginProfile(String),
    #[error("bundled plugin install failed: {0}")]
    BundledPluginInstallFailed(String),
    #[error("plugin service error {code}: {message}")]
    PluginService { code: String, message: String },
    #[error("session maintenance blocked by incompatible live daemon: {0}")]
    IncompatibleDaemonStorage(String),
    #[error("session repair usage error: {0}")]
    SessionRepairUsage(String),
    #[error("legacy stream cleanup usage error: {0}")]
    LegacyStreamCleanupUsage(String),
    #[error(transparent)]
    LegacyStreamCleanup(#[from] bcode_session::legacy_stream_cleanup::LegacyStreamCleanupError),
    #[error(transparent)]
    PluginCliComposition(#[from] plugin_cli::CompositionError),
    #[error("plugin CLI command failed: {0}")]
    PluginCli(String),
    #[error("{0}")]
    AuthPrimeFailed(String),
}

use std::sync::OnceLock;

static STATIC_BUNDLED_PLUGINS: OnceLock<Vec<bcode_plugin::StaticBundledPlugin>> = OnceLock::new();
static STATIC_BUNDLED_PLUGIN_IDS: OnceLock<Vec<String>> = OnceLock::new();

/// Parse CLI arguments and run the requested command.
///
/// # Errors
///
/// Returns an error when the requested command fails.
pub async fn run() -> Result<(), CliError> {
    run_with_static_bundled(Vec::new()).await
}

/// Parse CLI arguments and run with caller-provided static bundled plugins.
///
/// # Errors
///
/// Returns an error when the requested command fails.
pub async fn run_with_static_bundled(
    static_plugins: Vec<bcode_plugin::StaticBundledPlugin>,
) -> Result<(), CliError> {
    let static_plugin_ids = bcode_plugin::static_bundled_plugin_ids(&static_plugins)?;
    if let Err(error) = bcode_daemon_lifecycle::ensure_current_executable_cached() {
        tracing::warn!(%error, "failed to cache current executable for detached daemon startup");
    }
    let _ = STATIC_BUNDLED_PLUGINS.set(static_plugins);
    let _ = STATIC_BUNDLED_PLUGIN_IDS.set(static_plugin_ids);
    init_tracing();
    let registrations = plugin_cli::registrations(
        STATIC_BUNDLED_PLUGINS.get().map_or(&[][..], Vec::as_slice),
        STATIC_BUNDLED_PLUGIN_IDS
            .get()
            .map_or(&[][..], Vec::as_slice),
    )?;
    let command = plugin_cli::compose(Cli::command(), &registrations);
    let matches = command.get_matches();
    let _config_override = config_override_from_matches(&matches);
    if let Some(plugin) = plugin_cli::matched(&matches, &registrations)
        && let Some((_, subcommand_matches)) = matches.subcommand()
    {
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

fn config_override_from_matches(
    matches: &clap::ArgMatches,
) -> Option<bcode_config::ConfigOverrideGuard> {
    let profile = matches.get_one::<String>("profile");
    let request_timeout_secs = matches.get_one::<u64>("request_timeout_secs");
    if profile.is_none() && request_timeout_secs.is_none() {
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
    Some(bcode_config::push_process_config_overrides(
        bcode_config::ConfigLoadOverrides::from_env_with_cli(None, Some(override_toml)),
    ))
}

async fn handle_cli(cli: Cli) -> Result<(), CliError> {
    let _ = (&cli.profile, cli.request_timeout_secs);
    if cli.new {
        if cli.command.is_some() {
            return Err(CliError::NewSessionWithCommand);
        }
        Box::pin(run_new_session_tui(cli.worktree)).await?;
        return Ok(());
    }
    if cli.onboard {
        handle_onboard_command(&OnboardOptions::default())?;
        return Ok(());
    }
    if cli.command.is_none() && should_auto_start_onboarding()? {
        handle_onboard_command(&OnboardOptions::default())?;
        return Ok(());
    }
    match cli.command.unwrap_or_default() {
        Commands::Onboard {
            reset,
            dry_run,
            non_interactive,
            provider,
            skip_launch,
            control_center,
            secure_import_env,
        } => handle_onboard_flags(
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
        )?,
        Commands::Server { command } => handle_server_command(command).await?,
        Commands::Session { command } => handle_session_command(command).await?,
        Commands::Web {
            bind,
            port,
            allow_non_loopback,
        } => handle_web_command(bind, port, allow_non_loopback).await?,
        Commands::Plugin { command } => handle_plugin_command(command).await?,
        Commands::Model { command } => handle_model_command(command).await?,
        Commands::Auth { command } => handle_auth_command(command)?,
        Commands::Login { command } => handle_login_command(command).await?,
        Commands::Permission { command } => handle_permission_command(command).await?,
        Commands::RuntimeWork { command } => handle_runtime_work_command(command).await?,
        command => Box::pin(handle_session_io_command(command)).await?,
    }
    Ok(())
}

async fn handle_plugin_command(command: PluginCommand) -> Result<(), CliError> {
    match command {
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
    }
    Ok(())
}

async fn handle_web_command(
    bind: std::net::IpAddr,
    requested_port: Option<u16>,
    allow_non_loopback: bool,
) -> Result<(), CliError> {
    let bind = bcode_web_render::validate_bind_address(bind, allow_non_loopback)
        .map_err(|error| CliError::WebRender(error.to_owned()))?;
    let port = requested_port.unwrap_or(0);
    let access_token = random_web_access_token()?;
    let state = bcode_web_render::WebRenderState::new(
        BcodeClient::default_endpoint(),
        access_token.clone(),
    );
    let builder = bcode_web_render::init(&state).await?;
    let initial_session_id = initial_web_session_id(&state).await;
    let launch_query = initial_session_id.map_or_else(
        || format!("token={access_token}"),
        |session_id| {
            format!("token={access_token}&hyperchad-event-scope={access_token}:{session_id}")
        },
    );
    let builder = builder
        .with_actix_bind_address(bind.to_string())
        .with_actix_port(port)
        .with_actix_on_bound(move |address| {
            eprintln!("Bcode web renderer: http://{address}/?{launch_query}");
        })
        .with_viewport(bcode_web_render::VIEWPORT.clone());
    let mut app = bcode_web_render::build_app(builder)
        .map_err(|error| CliError::WebRender(error.to_string()))?;
    bcode_web_render::configure_live_updates(&mut app, &state);
    tokio::task::spawn_blocking(move || app.handle_serve())
        .await
        .map_err(|error| CliError::WebRender(format!("web renderer task failed: {error}")))?
        .map_err(|error| CliError::WebRender(error.to_string()))?;
    Ok(())
}

async fn initial_web_session_id(
    state: &bcode_web_render::WebRenderState,
) -> Option<bcode_session_models::SessionId> {
    const ATTEMPTS: usize = 5;
    for attempt in 0..ATTEMPTS {
        match state.initial_state().await {
            Ok((snapshot, sessions)) => {
                if let Some(session_id) = snapshot.session_id {
                    return Some(session_id);
                }
                if sessions.is_empty() && attempt + 1 == ATTEMPTS {
                    return None;
                }
            }
            Err(_) if attempt + 1 == ATTEMPTS => return None,
            Err(_) => {}
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    None
}

fn handle_onboard_flags(
    reset: bool,
    output_mode: OnboardOutputMode,
    provider: Option<String>,
    launch_mode: OnboardLaunchMode,
    experience_mode: OnboardExperienceMode,
    secure_import_env: Option<String>,
) -> Result<(), CliError> {
    handle_onboard_command(&OnboardOptions {
        reset,
        output_mode,
        provider,
        launch_mode,
        experience_mode,
        secure_import_env,
    })
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

fn handle_onboard_command(options: &OnboardOptions) -> Result<(), CliError> {
    let store = bcode_settings::SettingsStore::default();
    if options.reset {
        store.reset_database()?;
    }
    let now_ms = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX);
    let detection = bcode_settings::detect_setup_environment(now_ms);
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
        import_onboarding_env_credential(env_var, &secure_import_plans, now_ms)?;
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
        input
            .configured_sections
            .insert(bcode_settings::SetupSectionId::Providers);
        println!("onboarding provider hint: {provider}");
    }
    let progress = store.onboarding_progress()?;
    input.current_section = progress
        .and_then(|progress| progress.last_section)
        .as_deref()
        .and_then(onboard_section_from_str);
    let persisted_sections = store.onboarding_sections()?;
    let recommendations = store.setup_recommendations()?;
    let shell =
        bcode_tui::onboarding::OnboardingShell::from_reconciliation(&persisted_sections, &input);
    let readiness_report =
        bcode_settings::setup_readiness_report(shell.sections(), &recommendations);
    store.save_readiness_report(&readiness_report, now_ms)?;
    let render = shell.render_model(&store.health(), Some(readiness_report));
    if options.output_mode != OnboardOutputMode::Preview {
        println!("Bcode onboarding setup map\n");
        println!("{}", render.snapshot_text());
        if options.launch_mode == OnboardLaunchMode::SkipLaunch {
            println!("\nlaunch will be skipped after onboarding");
        }
        return Ok(());
    }
    bcode_tui::run_onboarding()?;
    Ok(())
}

fn onboard_section_from_str(value: &str) -> Option<bcode_settings::SetupSectionId> {
    bcode_settings::SetupSectionId::all()
        .into_iter()
        .find(|section| section.as_str() == value)
}

async fn handle_session_io_command(command: Commands) -> Result<(), CliError> {
    match command {
        Commands::Cancel {
            session_id,
            clear_queue,
        } => cancel_session_turn(session_id, clear_queue).await?,
        Commands::Attach { session_id } => attach_session(session_id).await?,
        Commands::Tui { session_id } => {
            bcode_tui::run_with_static_bundled(session_id, &static_bundled_plugins()).await?;
        }
        Commands::Send {
            session_id,
            message,
        } => send_message(session_id, message).await?,
        Commands::Onboard { .. }
        | Commands::Server { .. }
        | Commands::Session { .. }
        | Commands::Web { .. }
        | Commands::Plugin { .. }
        | Commands::Model { .. }
        | Commands::Auth { .. }
        | Commands::Login { .. }
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

/// Return the root `bcode` CLI command definition.
///
/// This keeps generated documentation, completions, and help snapshots in sync
/// with the actual parser without exposing parser internals as public API.
#[must_use]
pub fn root_command() -> clap::Command {
    Cli::command()
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
    /// Override the local client/daemon IPC request timeout in seconds.
    #[arg(
        long,
        global = true,
        value_name = "SECONDS",
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    request_timeout_secs: Option<u64>,
    /// Force the onboarding/setup-map flow.
    #[arg(long = "onboard", global = true)]
    onboard: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
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
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    Web {
        /// Address to bind. Defaults to IPv4 loopback.
        #[arg(long, default_value_t = bcode_web_render::DEFAULT_BIND_ADDRESS)]
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
    },
    Diagnose {
        #[arg(long)]
        json: bool,
    },
    Stop,
    Cleanup,
    StopAll,
    /// Gracefully stop every live daemon whose storage writer epoch is incompatible.
    RetireIncompatible,
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
        catalog: bool,
        #[arg(long)]
        scan: bool,
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
    Reindex {
        session_id: SessionId,
    },
    /// Remove historically persisted live tool-stream payloads.
    PruneLiveEvents {
        session_id: Option<SessionId>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        json: bool,
    },
    /// Audit local sessions for semantic-result migration readiness without writing changes.
    MigrateSemanticResults {
        /// Session store root to audit. Defaults to Bcode's local session store.
        #[arg(long)]
        root: Option<PathBuf>,
        /// Emit the full JSON audit report.
        #[arg(long)]
        json: bool,
    },
    Import {
        #[command(subcommand)]
        command: SessionImportCommand,
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
    Capabilities,
    Validate,
    Ignore {
        model_id: String,
        #[arg(long)]
        provider: Option<String>,
    },
    Unignore {
        model_id: String,
        #[arg(long)]
        provider: Option<String>,
    },
    Ignored {
        #[arg(long)]
        provider: Option<String>,
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
        /// Do not bind saved credentials to this device.
        #[arg(long)]
        no_device_seal: bool,
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
        /// Permission category: `command`, `read`, `write`, `edit`, or `web`.
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
        ServerCommand::Metrics { json, report } => server_metrics(json, report).await?,
        ServerCommand::Diagnose { json } => server_diagnose(json).await?,
        ServerCommand::Stop => server_stop().await?,
        ServerCommand::Cleanup => server_cleanup(false).await?,
        ServerCommand::StopAll => server_cleanup(true).await?,
        ServerCommand::RetireIncompatible => retire_incompatible_daemons().await?,
    }
    Ok(())
}

async fn handle_session_command(command: SessionCommand) -> Result<(), CliError> {
    match command {
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
            catalog,
            scan,
            json,
        } => {
            run_session_repair_command(SessionRepairCliOptions {
                target: repair_cli_target(session_id, catalog, scan),
                mode: SessionRepairCliMode::DryRun,
                output: repair_cli_output(json),
            })
            .await?;
        }
        SessionCommand::Repair {
            session_id,
            catalog,
            scan,
            dry_run,
            json,
        } => {
            run_session_repair_command(SessionRepairCliOptions {
                target: repair_cli_target(session_id, catalog, scan),
                mode: repair_cli_mode(dry_run),
                output: repair_cli_output(json),
            })
            .await?;
        }
        SessionCommand::Reindex { session_id } => {
            reindex_session_model_context(session_id).await?;
        }
        SessionCommand::PruneLiveEvents {
            session_id,
            all,
            dry_run,
            apply,
            json,
        } => {
            if dry_run == apply {
                return Err(CliError::LegacyStreamCleanupUsage(
                    "choose exactly one of --dry-run or --apply".to_owned(),
                ));
            }
            if all == session_id.is_some() {
                return Err(CliError::LegacyStreamCleanupUsage(
                    "provide exactly one session id or --all".to_owned(),
                ));
            }
            let target = session_id.map_or(
                session_cleanup::RunTarget::All,
                session_cleanup::RunTarget::Session,
            );
            let mode = if apply {
                session_cleanup::RunMode::Apply
            } else {
                session_cleanup::RunMode::DryRun
            };
            session_cleanup::run(session_cleanup::RunOptions { target, mode, json }).await?;
        }
        SessionCommand::MigrateSemanticResults { root, json } => {
            audit_semantic_result_migration(root, json).await?;
        }
        SessionCommand::Import { command } => handle_session_import_command(command).await?,
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

async fn handle_model_command(command: ModelCommand) -> Result<(), CliError> {
    match command {
        ModelCommand::Ignore { model_id, provider } => {
            let provider = provider.unwrap_or(default_model_provider_id()?);
            let path = bcode_config::ignore_model_in_state(&provider, model_id.clone())?;
            println!(
                "Ignored model '{model_id}' for provider '{provider}' in {}",
                display_from_current_dir(&path)
            );
        }
        ModelCommand::Unignore { model_id, provider } => {
            let provider = provider.unwrap_or(default_model_provider_id()?);
            let path = bcode_config::unignore_model_in_state(&provider, &model_id)?;
            println!(
                "Removed state ignore for model '{model_id}' and provider '{provider}' in {}",
                display_from_current_dir(&path)
            );
        }
        ModelCommand::Ignored { provider } => {
            let state = bcode_config::load_model_ignores_state()?;
            for (provider_id, rules) in state {
                if provider
                    .as_deref()
                    .is_some_and(|filter| filter != provider_id)
                {
                    continue;
                }
                println!("{provider_id}");
                for model in rules.models {
                    println!("  model {model}");
                }
                for pattern in rules.patterns {
                    println!("  pattern {pattern}");
                }
            }
        }
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
        other => {
            ensure_server_running().await?;
            match other {
                ModelCommand::List { json, provider } => list_models(json, provider).await?,
                ModelCommand::Status { session_id, json } => {
                    model_status(session_id, json).await?;
                }
                ModelCommand::Capabilities => model_capabilities().await?,
                ModelCommand::Validate => model_validate_config().await?,
                ModelCommand::Set {
                    session_id,
                    provider,
                    model_id,
                } => set_session_model(session_id, provider, model_id).await?,
                ModelCommand::Verify { .. }
                | ModelCommand::Ignore { .. }
                | ModelCommand::Unignore { .. }
                | ModelCommand::Ignored { .. } => unreachable!("handled above"),
            }
        }
    }
    Ok(())
}

fn handle_auth_command(command: AuthCommand) -> Result<(), CliError> {
    match command {
        AuthCommand::Status => auth_status(),
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
        },
        AuthCommand::Prime { command } => handle_auth_prime_command(command),
        AuthCommand::Resets { command } => handle_auth_resets_command(command),
        AuthCommand::Usage { command } => handle_auth_usage_command(command),
        AuthCommand::Login {
            profile,
            vault,
            recipient_key,
        } => auth_login(profile, vault, recipient_key),
    }
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
    let mut response = host.invoke_service_json::<_, bcode_model::AuthResetCreditConsumeResponse>(
        &plan.provider_plugin_id,
        bcode_model::MODEL_PROVIDER_INTERFACE_ID,
        bcode_model::OP_AUTH_RESET_CREDIT_CONSUME,
        &request,
    )?;
    host.deactivate_all()?;
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
    let credit = credit_id.unwrap_or("provider-selected credit");
    print!(
        "Consume one banked rate-limit reset for auth profile '{profile}' ({credit})? Type 'yes' to continue: "
    );
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim() == "yes")
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
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
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
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }
    if report.dry_run {
        println!("Dry run: no reset was consumed.");
        println!();
        println!("Would use:");
        println!("  Profile: {}", report.profile);
        println!(
            "  Credit: {}",
            report.credit_id.as_deref().unwrap_or("provider-selected")
        );
        return Ok(());
    }

    match report.status.as_str() {
        "reset" => {
            println!("Used one banked Codex reset for {}.", report.profile);
            println!();
            if let Some(windows_reset) = report.windows_reset {
                println!("Windows reset: {windows_reset}");
            }
            println!("Provider result: reset");
        }
        "nothing_to_reset" => {
            println!("No reset was used.");
            println!();
            println!("Reason: no current rate-limit window is eligible for reset.");
            println!("Your banked reset should still be available.");
        }
        "no_credit" => {
            println!("No banked reset is available for {}.", report.profile);
        }
        "already_redeemed" => {
            println!(
                "This reset request already completed successfully for {}.",
                report.profile
            );
        }
        _ => {
            println!("Reset consume status: {}", report.status);
            if let Some(message) = &report.message {
                println!("Detail: {message}");
            }
        }
    }
    Ok(())
}

fn print_auth_usage_report(report: &AuthUsageReport, json: bool) -> Result<(), CliError> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
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
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
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
    let profiles = declared_pool
        .map(|pool| pool.profiles.clone())
        .unwrap_or_default();
    let runtime_profiles = runtime_pool
        .map(|pool| {
            pool.profiles
                .iter()
                .map(|profile| profile.auth_profile.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
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
        auth_profile: auth_profile_name.clone(),
        storage_profile: storage_profile.clone(),
        vault_path: vault_path.clone(),
        api_key_env: Some(api_key_env.clone()),
        config_update: LoginConfigUpdate::Declarative,
        device_seal_policy,
        recipient_key: recipient_key_hint.clone(),
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
        } => {
            login_openai(OpenAiLoginOptions {
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
            })
            .await?;
        }
        LoginCommand::Xai {
            api_key,
            base_url,
            profile,
            vault,
            recipient_key,
            no_device_seal,
            model,
        } => {
            login_xai(XaiLoginOptions {
                api_key,
                base_url,
                profile,
                vault,
                recipient_key,
                no_device_seal,
                model,
            })?;
        }
    }
    Ok(())
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

async fn login_openai(options: OpenAiLoginOptions) -> Result<(), CliError> {
    if options.mode.auth.is_add_subscription()
        && (options.api_key.is_some() || options.base_url.is_some())
    {
        return Err(CliError::LoginProfile(
            "`bcode login openai --add-subscription` adds ChatGPT subscription OAuth accounts; API-key pooled auth is not supported yet. Remove --api-key/--base-url or omit --add-subscription.".to_string(),
        ));
    }
    let mut target = if options.mode.auth.is_add_subscription() {
        resolve_add_subscription_login_target(options.profile.clone(), options.vault.clone())
    } else {
        resolve_login_target(
            LoginProvider::OpenAi,
            options.profile,
            options.vault,
            options.recipient_key.as_deref(),
        )?
    };
    if options.no_device_seal {
        target.device_seal_policy = bcode_provider_auth::security::AuthDeviceSealPolicy::Off;
    }
    let store = open_auth_store(&target.vault_path)?;
    if options.api_key.is_some() || (options.base_url.is_some() && !options.mode.auth.is_chatgpt())
    {
        login_openai_api_key(
            &store,
            &target,
            options.api_key,
            options.base_url,
            options.model,
        )
    } else {
        login_openai_chatgpt(
            &store,
            target,
            options.model,
            options.mode.flow,
            options.mode.auth.is_add_subscription(),
        )
        .await
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
    device_seal_policy: bcode_provider_auth::security::AuthDeviceSealPolicy,
    recipient_key: Option<String>,
}

fn resolve_login_target(
    provider: LoginProvider,
    explicit_profile: Option<String>,
    explicit_vault: Option<PathBuf>,
    explicit_recipient_key: Option<&str>,
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
                explicit_recipient_key,
            );
        }
        let vault_path = explicit_vault.unwrap_or_else(bcode_config::default_auth_vault_path);
        return Ok(LoginTarget {
            auth_profile: profile.clone(),
            storage_profile: profile,
            vault_path,
            api_key_env: None,
            config_update: LoginConfigUpdate::Writable,
            device_seal_policy: bcode_provider_auth::security::AuthDeviceSealPolicy::Preferred,
            recipient_key: explicit_recipient_key.map(ToString::to_string),
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
        explicit_recipient_key,
    )
}

fn resolve_add_subscription_login_target(
    explicit_profile: Option<String>,
    explicit_vault: Option<PathBuf>,
) -> LoginTarget {
    let config = bcode_config::load_config().unwrap_or_default();
    let registry = bcode_config::load_runtime_auth_subscriptions();
    let profile = explicit_profile.map_or_else(
        || next_subscription_profile_name(&config, &registry),
        |profile| {
            if runtime_subscription_profile_exists(&registry, "openai", &profile) {
                println!(
                    "Refreshing existing OpenAI subscription auth profile '{profile}' in runtime auth state."
                );
            }
            profile
        },
    );
    let vault_path = explicit_vault.unwrap_or_else(|| {
        runtime_subscription_vault(&registry, "openai", &profile)
            .unwrap_or_else(bcode_config::default_auth_vault_path)
    });
    LoginTarget {
        auth_profile: profile.clone(),
        storage_profile: profile,
        vault_path,
        api_key_env: None,
        config_update: LoginConfigUpdate::Writable,
        device_seal_policy: bcode_provider_auth::security::AuthDeviceSealPolicy::Preferred,
        recipient_key: None,
    }
}

fn runtime_subscription_profile_exists(
    registry: &bcode_config::RuntimeAuthSubscriptions,
    pool: &str,
    profile: &str,
) -> bool {
    registry.pools.get(pool).is_some_and(|pool| {
        pool.profiles
            .iter()
            .any(|candidate| candidate.auth_profile == profile)
    })
}

fn runtime_subscription_vault(
    registry: &bcode_config::RuntimeAuthSubscriptions,
    pool: &str,
    profile: &str,
) -> Option<PathBuf> {
    registry
        .pools
        .get(pool)?
        .profiles
        .iter()
        .find(|candidate| candidate.auth_profile == profile)
        .map(|candidate| candidate.vault.clone())
}

fn next_subscription_profile_name(
    config: &bcode_config::BcodeConfig,
    registry: &bcode_config::RuntimeAuthSubscriptions,
) -> String {
    if !config.auth.profiles.contains_key("openai")
        && !runtime_subscription_profile_exists(registry, "openai", "openai")
    {
        return "openai".to_string();
    }
    for index in 2.. {
        let candidate = format!("openai-{index}");
        if !config.auth.profiles.contains_key(&candidate)
            && !runtime_subscription_profile_exists(registry, "openai", &candidate)
        {
            if index > 2 {
                println!(
                    "Adding new OpenAI subscription auth profile '{candidate}'. To refresh an existing subscription instead, pass `--profile openai-2` (or the profile shown by `bcode auth pool status openai`)."
                );
            }
            return candidate;
        }
    }
    unreachable!("unbounded subscription profile search should return")
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
    explicit_recipient_key: Option<&str>,
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
    let recipient_key = explicit_recipient_key
        .map(ToString::to_string)
        .or_else(|| auth_profile.settings.get("recipient_key").cloned());
    Ok(LoginTarget {
        auth_profile: auth_profile_name.to_string(),
        storage_profile,
        vault_path,
        api_key_env,
        config_update: LoginConfigUpdate::Declarative,
        device_seal_policy: bcode_provider_auth::security::device_seal_policy_for_auth_profile(
            auth_profile,
        ),
        recipient_key,
    })
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

    let config_base_url = base_url.clone();
    let mut values = BTreeMap::from([
        (auth_mode_key, "api_key".to_string()),
        (api_key_key, api_key),
    ]);
    let mut remove_keys = Vec::new();
    if let Some(base_url) = base_url {
        values.insert(base_url_key, base_url);
    } else {
        remove_keys.push(base_url_key);
    }
    remove_keys.extend([
        format!("BCODE_{prefix}_CODEX_ACCESS_TOKEN"),
        format!("BCODE_{prefix}_CODEX_ID_TOKEN"),
        format!("BCODE_{prefix}_CODEX_REFRESH_TOKEN"),
        format!("BCODE_{prefix}_CODEX_EXPIRES_AT"),
        format!("BCODE_{prefix}_CODEX_ACCOUNT_ID"),
    ]);
    upsert_auth_profile_secrets(store, target, values, &remove_keys)?;
    apply_auth_device_seal_policy(
        &target.vault_path,
        &target.storage_profile,
        target.device_seal_policy,
        target.recipient_key.as_deref(),
    )?;

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
    let mut target = resolve_login_target(
        LoginProvider::Xai,
        options.profile,
        options.vault,
        options.recipient_key.as_deref(),
    )?;
    if options.no_device_seal {
        target.device_seal_policy = bcode_provider_auth::security::AuthDeviceSealPolicy::Off;
    }
    let store = open_auth_store(&target.vault_path)?;
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
    add_subscription: bool,
) -> Result<(), CliError> {
    let oauth = run_openai_codex_oauth(flow).await?;
    let expires_at = unix_timestamp() + oauth.expires_in.unwrap_or(3600).saturating_sub(60);
    let account_id = oauth
        .id_token
        .as_deref()
        .and_then(chatgpt_account_id_from_access_token)
        .or_else(|| chatgpt_account_id_from_access_token(&oauth.access_token));
    let mut values = BTreeMap::from([
        ("BCODE_OPENAI_AUTH_MODE".to_string(), "chatgpt".to_string()),
        (
            "BCODE_OPENAI_CODEX_ACCESS_TOKEN".to_string(),
            oauth.access_token,
        ),
        (
            "BCODE_OPENAI_CODEX_EXPIRES_AT".to_string(),
            expires_at.to_string(),
        ),
    ]);
    let mut remove_keys = vec![
        target
            .api_key_env
            .clone()
            .unwrap_or_else(|| "BCODE_OPENAI_API_KEY".to_string()),
        "BCODE_OPENAI_BASE_URL".to_string(),
        "BCODE_OPENAI_CODEX_ID_TOKEN".to_string(),
        "BCODE_OPENAI_CODEX_REFRESH_TOKEN".to_string(),
        "BCODE_OPENAI_CODEX_ACCOUNT_ID".to_string(),
    ];
    if let Some(id_token) = oauth.id_token {
        values.insert("BCODE_OPENAI_CODEX_ID_TOKEN".to_string(), id_token);
        remove_keys.retain(|key| key != "BCODE_OPENAI_CODEX_ID_TOKEN");
    }
    if let Some(refresh_token) = oauth.refresh_token {
        values.insert(
            "BCODE_OPENAI_CODEX_REFRESH_TOKEN".to_string(),
            refresh_token,
        );
        remove_keys.retain(|key| key != "BCODE_OPENAI_CODEX_REFRESH_TOKEN");
    }
    if let Some(account_id) = account_id {
        values.insert("BCODE_OPENAI_CODEX_ACCOUNT_ID".to_string(), account_id);
        remove_keys.retain(|key| key != "BCODE_OPENAI_CODEX_ACCOUNT_ID");
    }
    upsert_auth_profile_secrets(store, &target, values, &remove_keys)?;
    apply_auth_device_seal_policy(
        &target.vault_path,
        &target.storage_profile,
        target.device_seal_policy,
        target.recipient_key.as_deref(),
    )?;

    report_login_completion(
        "OpenAI ChatGPT subscription login saved",
        &target,
        "OPENAI",
        || {
            if add_subscription {
                let registry_path = bcode_config::register_runtime_auth_subscription(
                    "openai",
                    bcode_config::RuntimeAuthSubscriptionProfile {
                        auth_profile: target.auth_profile.clone(),
                        storage_profile: target.storage_profile.clone(),
                        vault: target.vault_path.clone(),
                        provider: "openai".to_string(),
                        scheme: "chatgpt".to_string(),
                    },
                )?;
                Ok(registry_path)
            } else {
                bcode_config::set_openai_sshenv_auth_mode(
                    target.auth_profile.clone(),
                    target.vault_path.clone(),
                    model,
                    AuthMode::ChatGpt,
                )
            }
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
            Ok(config_path) => {
                println!("Config updated: {}", display_from_current_dir(&config_path));
            }
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

fn random_web_access_token() -> Result<String, CliError> {
    let mut data = [0_u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut data)
        .map_err(|error| CliError::WebRender(error.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(data))
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

fn plugin_selection_for_config(
    config: &bcode_config::BcodeConfig,
) -> bcode_plugin::PluginSelection {
    let static_plugin_ids = STATIC_BUNDLED_PLUGIN_IDS
        .get()
        .map_or_else(Vec::new, Clone::clone);
    bcode_config::plugin_selection_with_default_plugin_ids(config, &static_plugin_ids)
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

fn list_plugins(roots: &[std::path::PathBuf]) -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    let selection = plugin_selection_for_config(&config);
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
            display_from_current_dir(&plugin.manifest_path)
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
    let selection = plugin_selection_for_config(&config);
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
    let selection = plugin_selection_for_config(&config);
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
    let selection = plugin_selection_for_config(&config);
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
    let selection = plugin_selection_for_config(&config);
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
    let selection = plugin_selection_for_config(&config);
    let plugins =
        bcode_plugin::filter_selected_plugins(discover_plugins_for_cli(roots)?, &selection);
    let mut host = bcode_plugin::PluginHost::load_registered_plugins(&plugins)?;
    let delivered = host.publish_event(topic, &payload)?;
    host.deactivate_all()?;
    println!("delivered\t{delivered}");
    Ok(())
}

async fn list_models(json: bool, provider: Option<String>) -> Result<(), CliError> {
    let models = BcodeClient::default_endpoint()
        .session_model_list(provider)
        .await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&models)?);
    } else {
        print_model_list(&models.models);
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
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print_model_status(&status);
    }
    Ok(())
}

fn print_model_status(status: &bcode_ipc::SessionModelStatus) {
    println!(
        "provider\t{}",
        status.provider_plugin_id.as_deref().unwrap_or("<auto>")
    );
    println!(
        "model\t{}",
        status.model_id.as_deref().unwrap_or("<default>")
    );
    println!(
        "context_window\t{}",
        status
            .context_window
            .map_or_else(|| "<none>".to_string(), |value| value.to_string())
    );
    println!(
        "max_output_tokens\t{}",
        status
            .max_output_tokens
            .map_or_else(|| "<none>".to_string(), |value| value.to_string())
    );
    println!(
        "metadata_source\t{}",
        status
            .metadata_source
            .map_or_else(|| "<none>".to_string(), |source| format!("{source:?}"))
    );
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
        "{:<model_width$}  {:<display_name_width$}  {:>10}  {:>10}  {:<16}  DEFAULT",
        "MODEL", "DISPLAY NAME", "CTX", "MAX OUT", "METADATA"
    );
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
            println!(
                "{:<model_width$}  {:<display_name_width$}  {:>10}  {:>10}  {:<16}  yes",
                model.model_id, model.display_name, context, max_output, metadata
            );
        } else {
            println!(
                "{:<model_width$}  {:<display_name_width$}  {:>10}  {:>10}  {:<16}",
                model.model_id, model.display_name, context, max_output, metadata
            );
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
    let body = serde_json::to_string_pretty(&report)?;
    if let Some(output) = output {
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&output, body)?;
        println!("wrote {}", display_from_current_dir(&output));
    } else {
        println!("{body}");
    }
    host.deactivate_all()?;
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

struct CliPluginTurnInvoker<'a> {
    host: &'a mut bcode_plugin::PluginHost,
}

impl BlockingModelProviderInvoker for CliPluginTurnInvoker<'_> {
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

async fn call_model_provider_service(
    operation: &str,
) -> Result<bcode_ipc::PluginServiceResponse, CliError> {
    call_model_provider_service_payload(operation, Vec::new()).await
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
        println!("{}", serde_json::to_string_pretty(&value)?);
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
        let client =
            BcodeClient::new(endpoint).with_daemon_availability(DaemonAvailability::RequireRunning);
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
    status.namespace == record.namespace
        && status.instance_id == record.instance_id
        && status.build_fingerprint == record.build_fingerprint
        && status.storage_writer_epoch == record.storage_writer_epoch
}

fn remove_stale_socket(record: &bcode_daemon_lifecycle::DaemonRecord) {
    #[cfg(unix)]
    if let bcode_daemon_lifecycle::DaemonEndpointRecord::UnixSocket { path } = &record.endpoint
        && is_bcode_socket_path(path)
        && !unix_socket_has_listener(path)
    {
        let _ = std::fs::remove_file(path);
    }
}

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

async fn server_stop() -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint()
        .with_daemon_availability(DaemonAvailability::RequireRunning);
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
    bcode_tui::run_with_static_bundled(Some(session.id), &static_bundled_plugins()).await?;
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
        println!(
            "{}\t{}\t{} clients",
            session.display_title(),
            session.id,
            session.client_count
        );
    }
    Ok(())
}

async fn rename_session(session_id: SessionId, name: String) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let session = client.rename_session(session_id, Some(name)).await?;
    println!("renamed {} to {}", session.id, session.display_title());
    Ok(())
}

async fn delete_session(session_id: SessionId) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let session = client.delete_session(session_id).await?;
    println!("deleted {} ({})", session.display_title(), session.id);
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

async fn audit_semantic_result_migration(
    root: Option<PathBuf>,
    json: bool,
) -> Result<(), CliError> {
    let root = root.unwrap_or_else(|| bcode_config::default_state_dir().join("sessions"));
    let report = bcode_session::semantic_migration::audit_semantic_result_migration(&root).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_semantic_migration_audit(&report);
    }
    Ok(())
}

fn print_semantic_migration_audit(
    report: &bcode_session::semantic_migration::SemanticMigrationAuditReport,
) {
    println!("semantic migration audit");
    println!("root: {}", display_from_current_dir(&report.root));
    println!("sessions scanned: {}", report.sessions_scanned);
    println!("sessions decoded: {}", report.sessions_decoded);
    println!("events scanned: {}", report.events_scanned);
    println!("tool completions: {}", report.tool_call_finished.total);
    println!(
        "  with semantic_result: {}",
        report.tool_call_finished.with_semantic_result
    );
    println!(
        "  without semantic_result: {}",
        report.tool_call_finished.without_semantic_result
    );
    println!(
        "  legacy terminal JSON: {}",
        report.tool_call_finished.legacy_terminal_json
    );
    println!(
        "  non-terminal JSON: {}",
        report.tool_call_finished.non_terminal_json
    );
    println!("  plain text: {}", report.tool_call_finished.plain_text);
    println!("presentations: {}", report.presentations.total);
    println!("  terminal: {}", report.presentations.terminal);
    println!("  file_change: {}", report.presentations.file_change);
    println!(
        "  matched to completion: {}",
        report.presentations.matched_to_completion
    );
    println!("  orphan: {}", report.presentations.orphan);
    println!("  duplicate: {}", report.presentations.duplicate);
    println!("  conflict: {}", report.presentations.conflict);
    println!("migration readiness:");
    println!(
        "  removable presentations: {}",
        report.readiness.removable_presentations
    );
    println!(
        "  addable terminal results: {}",
        report.readiness.addable_terminal_results
    );
    println!(
        "  addable file-change results: {}",
        report.readiness.addable_file_change_results
    );
    println!(
        "  sessions requiring review: {}",
        report.readiness.sessions_requiring_review
    );
    if !report.issues.is_empty() {
        println!("issues:");
        for issue in &report.issues {
            println!(
                "  {:?}: {} ({})",
                issue.issue,
                issue.detail,
                display_from_current_dir(&issue.path)
            );
        }
    }
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

async fn ensure_session_maintenance_daemon_compatibility() -> Result<(), CliError> {
    if let Some((_, record)) = bcode_daemon_lifecycle::incompatible_storage_writer_records(
        &bcode_config::default_state_dir(),
        bcode_session::lease::CURRENT_SESSION_STORAGE_WRITER_EPOCH,
    )
    .await
    .into_iter()
    .next()
    {
        return Err(CliError::IncompatibleDaemonStorage(format!(
            "namespace={}, pid={:?}, build={}, writer_epoch={:?}, endpoint={:?}; stop this daemon before retrying maintenance",
            record.namespace,
            record.pid,
            record.build_fingerprint,
            record.storage_writer_epoch,
            record.endpoint
        )));
    }
    Ok(())
}

async fn reindex_session_model_context(session_id: SessionId) -> Result<(), CliError> {
    ensure_session_maintenance_daemon_compatibility().await?;
    let root = bcode_config::default_state_dir().join("sessions");
    let maintenance = bcode_session::lease::acquire_session_maintenance_guard(&root, session_id)?;
    let write = bcode_session::lease::acquire_maintenance_session_write_lock(
        &maintenance,
        &root,
        session_id,
    )?;
    let db = bcode_session::db::SessionDb::migrate_turso_in_root(
        session_id,
        &root,
        &maintenance,
        &write,
    )
    .await?;
    let event_count = db.reindex_model_context(&maintenance, &write).await?;
    println!(
        "Reindexed model context for session {session_id} from {event_count} canonical events"
    );
    Ok(())
}

async fn run_session_repair_command(options: SessionRepairCliOptions) -> Result<(), CliError> {
    let root = bcode_config::default_state_dir().join("sessions");
    let dry_run = matches!(options.mode, SessionRepairCliMode::DryRun);
    let mut reports = Vec::new();
    match options.target {
        SessionRepairCliTarget::Scan => {
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
    if matches!(options.output, SessionRepairCliOutput::Json) {
        println!("{}", serde_json::to_string_pretty(&reports)?);
    } else {
        for report in &reports {
            print_repair_report(report);
        }
    }
    Ok(())
}

async fn repair_session_report(
    root: &Path,
    session_id: SessionId,
    dry_run: bool,
) -> Result<bcode_session::repair::RepairReport, CliError> {
    if dry_run {
        Ok(bcode_session::repair::doctor_session(root, session_id).await?)
    } else {
        ensure_session_maintenance_daemon_compatibility().await?;
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
        SessionEventKind::InteractiveToolRequestCreated { .. } => {
            "interactive_tool_request_created"
        }
        SessionEventKind::InteractiveToolRequestResolved { .. } => {
            "interactive_tool_request_resolved"
        }
        SessionEventKind::PermissionRequested { .. } => "permission_requested",
        SessionEventKind::PermissionResolved { .. } => "permission_resolved",
        SessionEventKind::ModelChanged { .. } => "model_changed",
        SessionEventKind::ReasoningChanged { .. } => "reasoning_changed",
        SessionEventKind::SystemMessage { .. } => "system_message",
        SessionEventKind::AgentChanged { .. } => "agent_changed",
        SessionEventKind::ModelTurnStarted { .. } => "model_turn_started",
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
        SessionEventKind::ToolInvocationStream { .. } => "tool_invocation_stream",
        SessionEventKind::WorkingDirectoryChanged { .. } => "working_directory_changed",
        SessionEventKind::SessionImported { .. } => "session_imported",
        SessionEventKind::SessionForked { .. } => "session_forked",
        SessionEventKind::RalphLifecycle { .. } => "ralph_lifecycle",
        SessionEventKind::PluginStatusNote { .. } => "plugin_status_note",
        SessionEventKind::PluginAutomationTurnStarted { .. } => "plugin_automation_turn_started",
        SessionEventKind::PluginAutomationTurnFinished { .. } => "plugin_automation_turn_finished",
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
                eprintln!("imported [{source}] with {} warnings", warnings.len());
                for warning in warnings {
                    eprintln!("{}: {}", warning.code, warning.message);
                }
            }
        }
    }
    Ok(())
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
                .cancel_runtime_work(session_id, bcode_session_models::WorkId::new(work_id))
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
                    Event::SessionLive(_)
                    | Event::SessionViewResyncRequired { .. }
                    | Event::SessionCatalogUpdated { .. } => {}
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
    client
        .send_user_message(session_id, message, bcode_ipc::PromptPlacement::FollowUp)
        .await?;
    Ok(())
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
        SessionEventKind::UserMessage {
            client_id, text, ..
        } => {
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
            ..
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
            ..
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
        SessionEventKind::InteractiveToolRequestCreated {
            interaction_id,
            tool_call_id,
            interaction_kind,
            surface_kind,
            ..
        } => println!(
            "#{} interactive tool request: {interaction_id} {} via {surface_kind} ({tool_call_id})",
            event.sequence,
            interaction_kind.as_deref().unwrap_or("<unknown>")
        ),
        SessionEventKind::InteractiveToolRequestResolved {
            interaction_id,
            tool_call_id,
            ..
        } => println!(
            "#{} interactive tool resolved: {interaction_id} ({tool_call_id})",
            event.sequence
        ),
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
        SessionEventKind::ModelChanged { provider, model } => {
            println!("#{} model changed: {provider}/{model}", event.sequence);
        }
        SessionEventKind::ReasoningChanged { effort, summary } => {
            println!(
                "#{} reasoning changed: effort={} summary={}",
                event.sequence,
                effort.as_deref().unwrap_or("provider default"),
                summary.as_deref().unwrap_or("provider default")
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
        SessionEventKind::SessionForked {
            source_session_id,
            kind,
            ..
        } => {
            println!(
                "#{} session {:?} from {source_session_id}",
                event.sequence, kind
            );
        }
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
        SessionEventKind::PluginAutomationTurnStarted {
            plugin_id,
            operation_id,
            display_label,
            turn_id,
            ..
        } => println!(
            "#{} plugin automation started: {plugin_id} {operation_id} {turn_id} {display_label}",
            event.sequence
        ),
        SessionEventKind::PluginAutomationTurnFinished {
            plugin_id,
            operation_id,
            turn_id,
            outcome,
            message,
        } => println!(
            "#{} plugin automation finished: {plugin_id} {operation_id} {turn_id} {outcome:?} {}",
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

#[cfg(test)]
mod web_command_tests {
    use super::*;

    #[test]
    fn retire_incompatible_server_command_parses() {
        let cli = Cli::try_parse_from(["bcode", "server", "retire-incompatible"])
            .expect("retirement command should parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Server {
                command: ServerCommand::RetireIncompatible
            })
        ));
    }

    #[test]
    fn daemon_status_match_requires_full_storage_identity() {
        let record = bcode_daemon_lifecycle::DaemonRecord {
            schema_version: bcode_daemon_lifecycle::DAEMON_RECORD_SCHEMA_VERSION,
            namespace: "namespace".to_string(),
            protocol_version: 9,
            build_fingerprint: "build".to_string(),
            storage_writer_epoch: Some(2),
            pid: Some(1),
            endpoint: bcode_daemon_lifecycle::DaemonEndpointRecord::Unknown {
                debug: "test".to_string(),
            },
            log_path: PathBuf::from("test.log"),
            executable_path: None,
            started_at_unix_ms: 0,
            last_seen_unix_ms: 0,
            instance_id: "instance".to_string(),
        };
        let matching = bcode_ipc::DaemonStatus {
            namespace: "namespace".to_string(),
            protocol_version: 9,
            build_fingerprint: "build".to_string(),
            storage_writer_epoch: Some(2),
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

        assert_eq!(bind, bcode_web_render::DEFAULT_BIND_ADDRESS);
        assert_eq!(port, None);
        assert!(!allow_non_loopback);
    }

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
mod context_compaction_tests {
    use super::*;

    fn snapshot(
        origin: bcode_session_models::ProviderContextSnapshotOrigin,
    ) -> bcode_session_models::ProviderContextSnapshot {
        bcode_session_models::ProviderContextSnapshot {
            format_version: 1,
            request_fingerprint: None,
            request_id: None,
            provider_plugin_id: "provider".to_string(),
            model_id: "model".to_string(),
            compatibility_key: "surface".to_string(),
            auth_profile: None,
            origin,
            messages_json: "opaque-secret".to_string(),
            portable_summary: "portable-secret".to_string(),
        }
    }

    #[test]
    fn cli_origin_labels_are_distinct_and_opaque_data_is_not_disclosed() {
        let explicit = provider_compaction_description(
            &snapshot(bcode_session_models::ProviderContextSnapshotOrigin::Explicit),
            7,
        );
        let managed = provider_compaction_description(
            &snapshot(bcode_session_models::ProviderContextSnapshotOrigin::ProviderManaged),
            7,
        );
        assert!(explicit.contains("explicit provider-native"));
        assert!(managed.contains("provider-managed"));
        for output in [explicit, managed] {
            assert!(!output.contains("opaque-secret"));
            assert!(!output.contains("portable-secret"));
        }
    }
}

#[cfg(test)]
mod client_timeout_cli_tests {
    use super::{Cli, config_override_from_matches};
    use clap::{CommandFactory as _, Parser as _};
    use std::sync::Mutex;

    static CONFIG_OVERRIDE_LOCK: Mutex<()> = Mutex::new(());

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
    fn request_timeout_override_rejects_zero() {
        assert!(Cli::try_parse_from(["bcode", "--request-timeout-secs", "0"]).is_err());
    }
}
