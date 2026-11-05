use crate::{BillingRoute, Error, LaunchOptions};
use serde::Deserialize;
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
};

/// Exact-version native auth handoff support must be tested by the integrating plugin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeAuthSupport {
    Unknown,
    VerifiedStatusAndLogin,
}
/// Sanitized route classification; no email, account ID, token, URL or login code.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub enum AuthMethod {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "claude.ai")]
    ClaudeAccount,
    #[serde(rename = "oauth_token")]
    OAuthToken,
    #[serde(rename = "api_key")]
    ApiKey,
    #[serde(rename = "api_key_helper")]
    ApiKeyHelper,
    #[serde(rename = "third_party")]
    ThirdParty,
}
/// Local native readiness only; this is not a remote credential verification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthStatus {
    pub logged_in: bool,
    pub method: AuthMethod,
    pub config_directory: PathBuf,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeStatus {
    logged_in: bool,
    auth_method: AuthMethod,
    config_directory: PathBuf,
}
/// An explicit native terminal handoff.
///
/// Browser URLs/codes go only to the caller's
/// inherited terminal, not through protocol observations or Bcode history.
/// Drop kills the login process; no login material is persisted by this library.
pub struct NativeLoginHandoff {
    child: Child,
}
impl NativeLoginHandoff {
    #[must_use]
    pub fn process_id(&self) -> Option<u32> {
        self.child.id()
    }
    /// Wait for native login exit. Reinspect status afterwards; exit is not verified readiness.
    /// # Errors
    /// Process waiting failure.
    pub async fn wait(&mut self) -> Result<std::process::ExitStatus, Error> {
        Ok(self.child.wait().await?)
    }
    /// Cancel and reap this login only, without logging out other shared waiters.
    /// # Errors
    /// Native kill/wait error.
    pub async fn cancel(&mut self) -> Result<(), Error> {
        if self.child.try_wait()?.is_none() {
            self.child.kill().await?;
        }
        let _ = self.child.wait().await?;
        Ok(())
    }
}
/// Query native local status in the exact launch namespace, never scrape credential stores.
/// Requires a tested version whose status returns `configDirectory` (v2.1.268+).
/// # Errors
/// Unsupported auth mode, version mismatch, malformed/mismatched namespace or billing route,
/// process failure, timeout or bounded output overflow. Native payloads are not included in errors.
pub async fn auth_status(options: &LaunchOptions) -> Result<AuthStatus, Error> {
    if options.auth_support != NativeAuthSupport::VerifiedStatusAndLogin {
        return Err(Error::InvalidConfiguration(
            "native auth status support unverified",
        ));
    }
    options.verify().await?;
    let mut command = options.command();
    command.args(["auth", "status"]);
    let (status, output) = bounded_output(
        command,
        options.limits.request_timeout,
        options.limits.frame_bytes,
    )
    .await?;
    if !matches!(status.code(), Some(0 | 1)) {
        return Err(Error::Protocol);
    }
    let native: NativeStatus = serde_json::from_slice(&output).map_err(|_| Error::Protocol)?;
    if native.config_directory != options.config_directory || native.logged_in != status.success() {
        return Err(Error::Protocol);
    }
    let acceptable = match options.billing_route {
        BillingRoute::NativeAccount => matches!(
            native.auth_method,
            AuthMethod::None | AuthMethod::ClaudeAccount
        ),
        BillingRoute::ApiKey => matches!(native.auth_method, AuthMethod::None | AuthMethod::ApiKey),
    };
    if !acceptable {
        return Err(Error::InvalidConfiguration(
            "native auth billing route mismatch",
        ));
    }
    Ok(AuthStatus {
        logged_in: native.logged_in,
        method: native.auth_method,
        config_directory: native.config_directory,
    })
}
/// Explicitly begin native interactive login in the same credential namespace.
/// This is a terminal handoff, not structured auth continuation. Caller must authorize it.
/// # Errors
/// Unverified handoff support, API-billing selection, invalid configuration or process failure.
pub async fn login_handoff(options: &LaunchOptions) -> Result<NativeLoginHandoff, Error> {
    if options.auth_support != NativeAuthSupport::VerifiedStatusAndLogin
        || options.billing_route != BillingRoute::NativeAccount
    {
        return Err(Error::InvalidConfiguration(
            "native account login handoff unavailable",
        ));
    }
    options.verify().await?;
    let mut command = options.command();
    command
        .args(["auth", "login"])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    Ok(NativeLoginHandoff {
        child: command.spawn()?,
    })
}

pub async fn bounded_output(
    mut command: Command,
    timeout: Duration,
    limit: usize,
) -> Result<(std::process::ExitStatus, Vec<u8>), Error> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().ok_or(Error::Closed)?;
    let stderr = child.stderr.take().ok_or(Error::Closed)?;
    let work = async {
        let (status, output, ()) =
            tokio::try_join!(child.wait(), capture(stdout, limit), discard(stderr))?;
        Ok::<_, Error>((status, output))
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(error)) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(error)
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(Error::Uncertain)
        }
    }
}
async fn capture(mut reader: impl AsyncRead + Unpin, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            return Ok(output);
        }
        if count > limit.saturating_sub(output.len()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "native output exceeded limit",
            ));
        }
        output.extend_from_slice(&chunk[..count]);
    }
}
async fn discard(mut reader: impl AsyncRead + Unpin) -> std::io::Result<()> {
    let mut chunk = [0_u8; 4096];
    while reader.read(&mut chunk).await? != 0 {}
    Ok(())
}
