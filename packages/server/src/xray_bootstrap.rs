//! Matching-client bootstrap of a revocable, one-session local HTTP capability.
//! Bearer bytes exist only in a private credential file and the HTTP access constructor.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::Duration;

use bcode_ipc::XrayEndpointInfo;
use bcode_session_models::{ClientId, SessionId};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, interval, timeout};
use zeroize::Zeroizing;

use crate::ServerState;
use crate::xray_application::{XrayAccessGrant, XrayApplication};
use crate::xray_http::{self, XrayHttpAccess, XrayHttpConfig};

const MAX_ENDPOINTS: usize = 16;
const UNUSED_TIMEOUT: Duration = Duration::from_secs(60);

/// Secret-safe bootstrap failures; no paths, credentials or operating-system errors.
#[derive(Debug, thiserror::Error)]
pub enum XrayBootstrapError {
    #[error("X-ray bootstrap is not authorized")]
    Forbidden,
    #[error("X-ray bootstrap is unavailable")]
    Unavailable,
    #[error("X-ray endpoint capacity reached")]
    Capacity,
}

impl XrayBootstrapError {
    /// Stable public error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Forbidden => "xray_bootstrap_forbidden",
            Self::Unavailable => "xray_bootstrap_unavailable",
            Self::Capacity => "xray_bootstrap_capacity",
        }
    }
}

#[derive(Default)]
pub(crate) struct XrayEndpointRegistry {
    entries: Mutex<BTreeMap<String, Endpoint>>,
}

impl std::fmt::Debug for XrayEndpointRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("XrayEndpointRegistry")
            .finish_non_exhaustive()
    }
}

struct Endpoint {
    owner: ClientId,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        // Last-resort cancellation if the registry is dropped outside normal shutdown.
        self.task.abort();
    }
}

impl XrayEndpointRegistry {
    pub(crate) async fn active(&self) -> bool {
        let mut entries = self.entries.lock().await;
        entries.retain(|_, endpoint| !endpoint.task.is_finished());
        !entries.is_empty()
    }

    pub(crate) async fn stop_all(&self) {
        let entries = std::mem::take(&mut *self.entries.lock().await);
        for (_, mut endpoint) in entries {
            // Sending by replacement lets Endpoint retain its abort-on-drop guarantee.
            let (unused, _) = oneshot::channel();
            let _ = std::mem::replace(&mut endpoint.stop, unused).send(());
            if timeout(Duration::from_secs(4), &mut endpoint.task)
                .await
                .is_err()
            {
                endpoint.task.abort();
                let _ = (&mut endpoint.task).await;
            }
        }
    }
}

impl ServerState {
    /// Open an authenticated loopback endpoint for exactly one canonical session.
    ///
    /// The matching application caller explicitly authorizes sensitive capture with
    /// `deep_capture`; this is not inherited from ordinary session attachment. The
    /// returned private file is a capability: consumers must not log or publish it.
    /// No active subscription expires merely because its session is idle. An unused
    /// endpoint is reclaimed after a bounded grace interval, including after streams close.
    ///
    /// # Errors
    /// Fails closed for unverified callers, foreign/degraded sessions, shutdown,
    /// endpoint limits, unsafe credential storage or listener setup failures.
    pub async fn open_xray_endpoint(
        self: &Arc<Self>,
        client_id: ClientId,
        session_id: SessionId,
        deep_capture: bool,
    ) -> Result<XrayEndpointInfo, XrayBootstrapError> {
        self.require_xray_bootstrap_client(client_id).await?;
        crate::session_operations::require_selected_location(self, session_id)
            .await
            .map_err(|_| XrayBootstrapError::Unavailable)?;
        if self
            .active_session_namespace_mismatch(session_id, client_id)
            .await
            .is_some()
        {
            return Err(XrayBootstrapError::Forbidden);
        }
        let grant = XrayAccessGrant::authorized(BTreeSet::from([session_id]), deep_capture)
            .map_err(|_| XrayBootstrapError::Forbidden)?;
        let application = XrayApplication::new(Arc::clone(self), Arc::clone(&self.xray));
        // Reuse the application's bounded canonical health/read-admission policy. Drop
        // the probe immediately; it is not returned as a successful external observation.
        let probe = application
            .observe(
                &grant,
                bcode_xray_models::XrayObserveRequest {
                    schema_version: bcode_xray_models::XRAY_SCHEMA_VERSION,
                    session_id: session_id.to_string(),
                    deep_capture,
                },
            )
            .await
            .map_err(|_| XrayBootstrapError::Unavailable)?;
        drop(probe);
        drop(application);

        let mut entries = self.xray_endpoints.entries.lock().await;
        entries.retain(|_, endpoint| !endpoint.task.is_finished());
        if entries.len() >= MAX_ENDPOINTS {
            return Err(XrayBootstrapError::Capacity);
        }
        if self.shutdown_requested.load(Ordering::SeqCst) {
            return Err(XrayBootstrapError::Unavailable);
        }
        let endpoint_id = uuid::Uuid::new_v4().to_string();
        // Three UUID-v4 values contain 366 independent random bits (not just 244).
        let token = new_token();
        let credential = CredentialFile::create(&self.state_root, &endpoint_id, &token)?;
        let access =
            XrayHttpAccess::new(&token, grant).map_err(|_| XrayBootstrapError::Unavailable)?;
        drop(token);
        let config = XrayHttpConfig::default();
        let listener = xray_http::bind(&config)
            .await
            .map_err(|_| XrayBootstrapError::Unavailable)?;
        let address = listener
            .local_addr()
            .map_err(|_| XrayBootstrapError::Unavailable)?;
        let info = XrayEndpointInfo {
            endpoint_id: endpoint_id.clone(),
            session_id,
            endpoint: format!("http://{address}"),
            credential_file: credential.path.clone(),
            deep_capture,
            schema_version: bcode_xray_models::XRAY_SCHEMA_VERSION,
        };
        let (stop, stopped) = oneshot::channel();
        let activity = access.clone();
        let weak = Arc::downgrade(self);
        let shutdown = self.subscribe_shutdown();
        let application = XrayApplication::new(Arc::clone(self), Arc::clone(&self.xray));
        let task = tokio::spawn(async move {
            // Guard cleanup runs on normal completion, serve error, panic or abort.
            let _credential = credential;
            let inactivity = endpoint_shutdown(weak, activity, stopped, shutdown);
            if xray_http::serve(listener, application, access, config, inactivity)
                .await
                .is_err()
            {
                tracing::warn!("X-ray endpoint stopped with a transport failure");
            }
        });
        entries.insert(
            endpoint_id,
            Endpoint {
                owner: client_id,
                stop,
                task,
            },
        );
        drop(entries);
        Ok(info)
    }

    /// Revoke a capability created by this verified caller and wait for bounded teardown.
    /// This closes observations and deletes the credential, never cancels session work.
    ///
    /// # Errors
    /// Returns a fixed authorization failure for unknown/foreign endpoint identities.
    pub async fn close_xray_endpoint(
        &self,
        client_id: ClientId,
        endpoint_id: &str,
    ) -> Result<(), XrayBootstrapError> {
        self.require_xray_bootstrap_client(client_id).await?;
        let mut entries = self.xray_endpoints.entries.lock().await;
        if entries
            .get(endpoint_id)
            .is_none_or(|entry| entry.owner != client_id)
        {
            return Err(XrayBootstrapError::Forbidden);
        }
        let mut endpoint = entries
            .remove(endpoint_id)
            .ok_or(XrayBootstrapError::Forbidden)?;
        drop(entries);
        let (unused, _) = oneshot::channel();
        let _ = std::mem::replace(&mut endpoint.stop, unused).send(());
        if timeout(Duration::from_secs(4), &mut endpoint.task)
            .await
            .is_err()
        {
            endpoint.task.abort();
            let _ = (&mut endpoint.task).await;
        }
        Ok(())
    }

    async fn require_xray_bootstrap_client(
        &self,
        client_id: ClientId,
    ) -> Result<(), XrayBootstrapError> {
        if self.shutdown_requested.load(Ordering::SeqCst) {
            return Err(XrayBootstrapError::Unavailable);
        }
        // Namespace registration occurs only after successful artifact/build/state Hello.
        if !self.clients.lock().await.contains(&client_id)
            || !self
                .client_session_namespaces
                .lock()
                .await
                .contains_key(&client_id)
        {
            return Err(XrayBootstrapError::Forbidden);
        }
        Ok(())
    }
}

async fn endpoint_shutdown(
    state: Weak<ServerState>,
    activity: XrayHttpAccess,
    mut stopped: oneshot::Receiver<()>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let mut last_active = Instant::now();
    let mut tick = interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = &mut stopped => return,
            _ = shutdown.recv() => return,
            _ = tick.tick() => {
                if state.upgrade().is_none() { return; }
                // Endpoint-local registry includes idle SSE and finalized streams
                // draining payloads. Another endpoint's observers confer no lifetime.
                if unused_endpoint_expired(&mut last_active, activity.active_count(), Instant::now()) {
                    return;
                }
            }
        }
    }
}

fn unused_endpoint_expired(last_active: &mut Instant, active_count: usize, now: Instant) -> bool {
    if active_count > 0 {
        *last_active = now;
        false
    } else {
        now.saturating_duration_since(*last_active) >= UNUSED_TIMEOUT
    }
}

fn new_token() -> Zeroizing<String> {
    Zeroizing::new(format!(
        "{}{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    ))
}

// Never Debug: even the private locator should stay out of routine diagnostics.
struct CredentialFile {
    root: PathBuf,
    path: PathBuf,
}

impl Drop for CredentialFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.root);
    }
}

impl CredentialFile {
    #[cfg(unix)]
    fn create(
        state_root: &Path,
        endpoint_id: &str,
        token: &str,
    ) -> Result<Self, XrayBootstrapError> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
        if endpoint_id.is_empty()
            || endpoint_id.len() > 128
            || !endpoint_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(XrayBootstrapError::Unavailable);
        }
        let failed = |_| XrayBootstrapError::Unavailable;
        // Resolve only the trusted serving state root. No external path is accepted.
        let parent = state_root.canonicalize().map_err(failed)?;
        let root = parent.join(format!(".xray-{endpoint_id}"));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .map_err(failed)?;
        let credential = Self {
            path: root.join("credential"),
            root,
        };
        if credential.root.canonicalize().map_err(failed)? != credential.root
            || credential.root.parent() != Some(parent.as_path())
            || std::fs::symlink_metadata(&credential.root)
                .map_err(failed)?
                .permissions()
                .mode()
                & 0o777
                != 0o700
        {
            return Err(XrayBootstrapError::Unavailable);
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&credential.path)
            .map_err(failed)?;
        if !file.metadata().map_err(failed)?.is_file()
            || file.metadata().map_err(failed)?.permissions().mode() & 0o777 != 0o600
            || credential.path.canonicalize().map_err(failed)? != credential.path
        {
            return Err(XrayBootstrapError::Unavailable);
        }
        file.write_all(token.as_bytes()).map_err(failed)?;
        file.sync_all().map_err(failed)?;
        Ok(credential)
    }

    #[cfg(not(unix))]
    fn create(_: &Path, _: &str, _: &str) -> Result<Self, XrayBootstrapError> {
        Err(XrayBootstrapError::Unavailable)
    }
}

#[cfg(test)]
#[path = "xray_bootstrap_tests.rs"]
mod tests;
