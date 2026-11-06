#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Frontend-neutral, one-way browser inspector hosting using public application clients.

mod assets;
mod host;

use bcode_client::BcodeClient;
use bcode_session_models::SessionId;
use std::path::PathBuf;
use tokio::sync::{oneshot, watch};

/// Secret-safe launch and cleanup failure.
#[derive(Debug, thiserror::Error)]
pub enum InspectorError {
    /// Invalid launch inputs or failed scoped host operation.
    #[error("{0}")]
    InvalidArguments(String),
    /// Public application boundary did not complete successfully.
    #[error("inspector application operation unavailable")]
    Application,
    /// Local asset or host operation failed.
    #[error("inspector local resource unavailable")]
    Local,
}
impl From<bcode_client::ClientError> for InspectorError {
    fn from(_: bcode_client::ClientError) -> Self {
        Self::Application
    }
}
impl From<std::io::Error> for InspectorError {
    fn from(_: std::io::Error) -> Self {
        Self::Local
    }
}

/// Explicit observation options, never execution or model selection.
#[derive(Clone, Debug)]
pub struct LaunchOptions {
    /// Developer-only external built asset override.
    pub asset_dir: Option<PathBuf>,
    /// Leave browser opening to the caller; pairing remains available.
    pub no_open: bool,
    /// Explicit permission to request sensitive capture.
    pub deep: bool,
    /// Finite host lifetime; expiry only stops observation.
    pub lifetime_seconds: u64,
    /// Explicitly open browser on serving host even over SSH.
    pub local_browser: bool,
}
impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            asset_dir: None,
            no_open: false,
            deep: false,
            lifetime_seconds: 3600,
            local_browser: false,
        }
    }
}

/// Owned observation lifetime. Dropping requests cleanup, never session cancellation.
#[must_use]
pub struct InspectorHandle {
    shutdown: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<Result<(), InspectorError>>>,
}
impl InspectorHandle {
    /// Stop observation and await owned resource cleanup.
    /// # Errors
    /// Returns failure when host settlement or endpoint revocation cannot be confirmed.
    pub async fn stop(mut self) -> Result<(), InspectorError> {
        self.shutdown.send_replace(true);
        self.wait_task().await
    }
    /// Wait until explicit stop, expiry, or host failure.
    /// # Errors
    /// Returns secret-safe host or cleanup failure.
    pub async fn wait(mut self) -> Result<(), InspectorError> {
        self.wait_task().await
    }
    /// Observe settlement without consuming the handle; cancellation retains cleanup ownership.
    /// # Errors
    /// Reports host or join failure without exposing credentials.
    pub async fn settled(&mut self) -> Result<(), InspectorError> {
        let result = self.task.as_mut().ok_or(InspectorError::Local)?.await;
        let _settled = self.task.take();
        result.map_err(|_| InspectorError::Local)?
    }
    async fn wait_task(&mut self) -> Result<(), InspectorError> {
        self.task
            .take()
            .ok_or(InspectorError::Local)?
            .await
            .map_err(|_| InspectorError::Local)?
    }
}
impl Drop for InspectorHandle {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
    }
}

/// Launch using the caller's exact public client; never select an alternate daemon.
/// # Errors
/// Returns invalid options, rejected authorization, unavailable assets, or host setup failure.
pub async fn start(
    client: BcodeClient,
    session_id: SessionId,
    options: LaunchOptions,
) -> Result<InspectorHandle, InspectorError> {
    launch(Some(client), session_id, options).await
}
/// Launch explicitly synthetic in-memory evidence with no daemon or provider access.
/// # Errors
/// Returns invalid options or unavailable local assets/hosting.
pub async fn start_demo(options: LaunchOptions) -> Result<InspectorHandle, InspectorError> {
    launch(None, SessionId::default(), options).await
}
async fn launch(
    client: Option<BcodeClient>,
    session_id: SessionId,
    options: LaunchOptions,
) -> Result<InspectorHandle, InspectorError> {
    if !(1..=86400).contains(&options.lifetime_seconds) {
        return Err(InspectorError::InvalidArguments(
            "inspector lifetime must be 1..86400 seconds".into(),
        ));
    }
    let args = host::XrayWebArgs {
        session_id,
        asset_dir: options.asset_dir,
        no_open: options.no_open,
        deep: options.deep,
        lifetime_seconds: options.lifetime_seconds,
        local_browser: options.local_browser,
    };
    let (shutdown, stopped) = watch::channel(false);
    let (ready, started) = oneshot::channel();
    let task = tokio::spawn(async move {
        match client {
            Some(client) => host::run(client, &args, stopped, ready).await,
            None => host::run_demo(&args, stopped, ready).await,
        }
    });
    let mut handle = InspectorHandle {
        shutdown,
        task: Some(task),
    };
    if started.await.is_err() {
        handle.wait_task().await?;
        return Err(InspectorError::Local);
    }
    Ok(handle)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn settled_handle_is_consumed_without_repolling_completed_task() {
        let (shutdown, _receiver) = watch::channel(false);
        let mut handle = InspectorHandle {
            shutdown,
            task: Some(tokio::spawn(async { Ok(()) })),
        };
        handle.settled().await.unwrap();
        assert!(matches!(handle.settled().await, Err(InspectorError::Local)));
    }

    #[tokio::test]
    async fn stop_notifies_before_waiting_for_cleanup() {
        let (shutdown, mut receiver) = watch::channel(false);
        let handle = InspectorHandle {
            shutdown,
            task: Some(tokio::spawn(async move {
                receiver
                    .wait_for(|value| *value)
                    .await
                    .map_err(|_| InspectorError::Local)?;
                Ok(())
            })),
        };
        handle.stop().await.unwrap();
    }
}
