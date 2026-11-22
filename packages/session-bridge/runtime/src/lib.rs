#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Choose and start the Bcode that serves an application's session bridge.
//!
//! Applications built outside Bcode (bdata, BMUX) reach Bcode only through the session
//! bridge. The bridge is served either by an **installed** Bcode (`bcode session-bridge
//! --stdio`, a child process) or by a Bcode **embedded** in the application's own process,
//! which an application offers through [`RuntimeOptions::embedded`] (the
//! `bcode_session_bridge_embedded` crate provides one). Everything above [`Attached`] is the
//! same either way, and an application that never embeds Bcode links none of it.
//!
//! Selection is deterministic and never falls back after a failure:
//!
//! * [`RuntimeMode::Installed`] and [`RuntimeMode::Embedded`] use exactly that runtime.
//! * [`RuntimeMode::Auto`] uses an installed Bcode that was named explicitly (a command-line
//!   flag, then `BCODE_BIN`, then configuration; see [`NamedExecutable`]), and that one is
//!   never replaced. Otherwise [`AutoPreference`] picks, and the other kind is used only when
//!   the preferred one is absent from this build or machine. A `bcode` found on `PATH` counts
//!   as installed but never outranks a preference for embedded.
//!
//! [`Attached::runtime`] reports which runtime serves, and why, for the application to show.

mod installed;
mod resolve;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use bcode_session_bridge::models::VersionRange;
use bcode_session_bridge::{BridgeClient, BridgeEvents};
use serde::{Deserialize, Serialize};

pub use resolve::{NamedExecutable, Selection, resolve};

/// Which Bcode serves the application (`runtime = "..."` in its configuration).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeMode {
    /// An explicitly named installed Bcode, else the [`AutoPreference`].
    #[default]
    Auto,
    /// Always an installed Bcode.
    Installed,
    /// Always the Bcode embedded in the application.
    Embedded,
}

/// What [`RuntimeMode::Auto`] uses when no installed Bcode is named. Each application picks
/// its default and lets users override it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AutoPreference {
    /// The embedded Bcode, when this build has it.
    Embedded,
    /// An installed Bcode, when one is found.
    #[default]
    Installed,
}

/// Which Bcode serves a connection, for the application to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActiveRuntime {
    /// An installed Bcode, through its session bridge process.
    Installed {
        /// Its executable.
        executable: PathBuf,
    },
    /// The Bcode embedded in the application.
    Embedded,
    /// A bridge the application connected itself ([`Attached::connect`]).
    External,
}

impl ActiveRuntime {
    /// A short label, such as "installed Bcode".
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Installed { .. } => "installed Bcode",
            Self::Embedded => "embedded Bcode",
            Self::External => "Bcode",
        }
    }
}

/// A Bcode an application hosts in its own process, serving the session bridge in-process.
pub trait EmbeddedBcode: Send + Sync {
    /// Start (or find) the embedded Bcode and connect to its bridge.
    ///
    /// # Errors
    /// [`AttachError::Failed`] when it cannot start or its bridge does not answer, or
    /// [`AttachError::VersionMismatch`] when the bridge shares no version with the application.
    fn attach<'a>(
        &'a self,
        options: &'a RuntimeOptions,
    ) -> Pin<Box<dyn Future<Output = Result<Attached, AttachError>> + Send + 'a>>;
}

/// How to start the session bridge.
#[derive(Clone)]
pub struct RuntimeOptions {
    /// Bridge client name and the application's name in messages, such as `bmux`.
    pub app: String,
    /// Which runtime to use.
    pub mode: RuntimeMode,
    /// What `auto` prefers when no installed Bcode is named.
    pub auto_preference: AutoPreference,
    /// The installed Bcode named by the application's flag or configuration.
    pub named: NamedExecutable,
    /// How users name the executable, for messages, such as "`--bcode`, `BCODE_BIN`, or
    /// `[feedback] bcode`".
    pub naming_hint: String,
    /// Environment variables withheld from the bridge, and from commands an embedded Bcode
    /// runs, such as credentials.
    pub private_env: BTreeSet<String>,
    /// Environment variables set for an installed bridge.
    pub env: BTreeMap<String, String>,
    /// Working directory of an installed bridge (Bcode reads project configuration there).
    pub directory: Option<PathBuf>,
    /// Bridge protocol versions to offer.
    pub versions: VersionRange,
    /// The Bcode this application can embed, if it links one.
    pub embedded: Option<Arc<dyn EmbeddedBcode>>,
}

impl std::fmt::Debug for RuntimeOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeOptions")
            .field("app", &self.app)
            .field("mode", &self.mode)
            .field("auto_preference", &self.auto_preference)
            .field("named", &self.named)
            .field("private_env", &self.private_env)
            .field("directory", &self.directory)
            .field("versions", &self.versions)
            .field("embedded", &self.embedded.is_some())
            .finish_non_exhaustive()
    }
}

impl RuntimeOptions {
    /// Options for `app` with every other setting at its default.
    #[must_use]
    pub fn new(app: impl Into<String>) -> Self {
        Self {
            app: app.into(),
            mode: RuntimeMode::Auto,
            auto_preference: AutoPreference::Installed,
            named: NamedExecutable::default(),
            naming_hint: "BCODE_BIN".to_owned(),
            private_env: BTreeSet::new(),
            env: BTreeMap::new(),
            directory: None,
            versions: VersionRange::supported(),
            embedded: None,
        }
    }
}

/// Why the application could not attach to Bcode. Each message says what to do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttachError {
    /// No usable Bcode: none installed, a named one is missing, or embedded is not built in.
    #[error("{0}")]
    Unavailable(String),
    /// The Bcode found speaks no bridge version the application speaks.
    #[error("{0}")]
    VersionMismatch(String),
    /// The bridge failed to start or answer.
    #[error("{0}")]
    Failed(String),
}

/// A negotiated session bridge and the runtime serving it.
#[derive(Debug)]
pub struct Attached {
    /// The bridge client; cloning shares the connection.
    pub client: BridgeClient,
    /// Unsolicited bridge events.
    pub events: BridgeEvents,
    runtime: ActiveRuntime,
    reason: String,
}

impl Attached {
    /// Speak the bridge protocol over `reader`/`writer`, for a bridge the application reached
    /// itself or a scripted one in tests.
    ///
    /// # Errors
    /// * [`AttachError::VersionMismatch`] when the bridge shares no version with `versions`.
    /// * [`AttachError::Failed`] when the transport fails.
    pub async fn connect<R, W>(
        reader: R,
        writer: W,
        app: &str,
        versions: VersionRange,
    ) -> Result<Self, AttachError>
    where
        R: tokio::io::AsyncRead + Send + Unpin + 'static,
        W: tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        let (client, events) = BridgeClient::connect(reader, writer, app, versions)
            .await
            .map_err(|error| installed::bridge_error(app, error))?;
        Ok(Self {
            client,
            events,
            runtime: ActiveRuntime::External,
            reason: "connected by the application".to_owned(),
        })
    }

    /// Which Bcode serves this connection.
    #[must_use]
    pub const fn runtime(&self) -> &ActiveRuntime {
        &self.runtime
    }

    /// Why this runtime was chosen, such as "`BCODE_BIN` names it".
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    fn served_by(mut self, runtime: ActiveRuntime, reason: String) -> Self {
        self.runtime = runtime;
        self.reason = reason;
        self
    }
}

/// Choose the runtime for `options` and start its session bridge.
///
/// # Errors
/// * [`AttachError::Unavailable`] when the chosen runtime does not exist: no installed Bcode,
///   a named executable that is not one, or no [`RuntimeOptions::embedded`].
/// * [`AttachError::VersionMismatch`] when the bridge shares no version with the application.
/// * [`AttachError::Failed`] when the bridge cannot start or answer.
pub async fn attach(options: &RuntimeOptions) -> Result<Attached, AttachError> {
    let selection = resolve(
        options.mode,
        options.auto_preference,
        &options.named,
        options.embedded.is_some(),
        &options.naming_hint,
    )?;
    match selection {
        Selection::Installed { executable, reason } => {
            let attached = installed::start(options, &executable).await?;
            Ok(attached.served_by(ActiveRuntime::Installed { executable }, reason))
        }
        Selection::Embedded { reason } => {
            let embedded = options.embedded.as_ref().ok_or_else(|| {
                AttachError::Unavailable(format!(
                    "This {} has no embedded Bcode: install Bcode, or use the installed runtime.",
                    options.app
                ))
            })?;
            let attached = embedded.attach(options).await?;
            Ok(attached.served_by(ActiveRuntime::Embedded, reason))
        }
    }
}
