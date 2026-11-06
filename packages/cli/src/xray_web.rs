//! CLI adaptation of the shared observation launcher.
use crate::CliError;
use bcode_session_models::SessionId;
use clap::Args;
use std::path::PathBuf;

/// Open a one-way browser inspector for one selected session.
#[derive(Debug, Args)]
pub struct XrayWebArgs {
    /// Exact session to observe.
    pub session_id: SessionId,
    /// Developer-only built asset override.
    #[arg(long)]
    pub asset_dir: Option<PathBuf>,
    /// Print URL and pairing instructions without opening a browser.
    #[arg(long)]
    pub no_open: bool,
    /// Open on serving host even through SSH.
    #[arg(long)]
    pub local_browser: bool,
    /// Require a one-time browser pairing ticket or code.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub require_pairing: Option<bool>,
    /// Explicitly authorize sensitive payload capture.
    #[arg(long)]
    pub deep: bool,
    /// Maximum observation lifetime; never limits session execution.
    #[arg(long, default_value_t = 3600, value_parser = clap::value_parser!(u64).range(1..=86400))]
    pub lifetime_seconds: u64,
}

pub async fn run(args: &XrayWebArgs) -> Result<(), CliError> {
    let client = crate::session_read_client(args.session_id).await?;
    let handle = bcode_xray_inspector::start(client, args.session_id, options(args))
        .await
        .map_err(|error| failure(&error))?;
    wait(handle).await
}
pub async fn run_demo(args: &XrayWebArgs) -> Result<(), CliError> {
    let handle = bcode_xray_inspector::start_demo(options(args))
        .await
        .map_err(|error| failure(&error))?;
    wait(handle).await
}
fn options(args: &XrayWebArgs) -> bcode_xray_inspector::LaunchOptions {
    bcode_xray_inspector::LaunchOptions {
        asset_dir: args.asset_dir.clone(),
        no_open: args.no_open,
        deep: args.deep,
        lifetime_seconds: args.lifetime_seconds,
        local_browser: args.local_browser,
        pairing: if args.require_pairing.unwrap_or(false) {
            bcode_xray_inspector::PairingMode::Required
        } else {
            bcode_xray_inspector::PairingMode::Open
        },
    }
}
async fn wait(handle: bcode_xray_inspector::InspectorHandle) -> Result<(), CliError> {
    // Dropping wait's owned handle on Ctrl-C requests shared host cleanup; wait explicitly
    // through stop instead so ordinary exit confirms revocation.
    let mut handle = handle;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        result = handle.settled() => return result.map_err(|error| failure(&error)),
    }
    handle.stop().await.map_err(|error| failure(&error))
}
fn failure(error: &bcode_xray_inspector::InspectorError) -> CliError {
    CliError::InvalidArguments(error.to_string())
}
