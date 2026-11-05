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
    /// Neither local status nor login has been verified.
    Unknown,
    /// Exact-version local status only; does not authorize login handoff.
    VerifiedStatus,
    /// Exact-version local status and native login handoff have both been verified.
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
    if options.billing_route == BillingRoute::ApiKey {
        // Explicit credential presence is local readiness only, never remote verification.
        // Do not ask native account status to reinterpret an admitted API billing route.
        options.verify().await?;
        return Ok(AuthStatus {
            logged_in: true,
            method: AuthMethod::ApiKey,
            config_directory: options.config_directory.clone(),
        });
    }
    if options.auth_support == NativeAuthSupport::Unknown {
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
    validated_status(options, status.success(), &output)
}

fn validated_status(
    options: &LaunchOptions,
    success: bool,
    output: &[u8],
) -> Result<AuthStatus, Error> {
    let native: NativeStatus = serde_json::from_slice(output).map_err(|_| Error::Protocol)?;
    if native.config_directory != options.config_directory || native.logged_in != success {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Limits, NativeEnvironment, SettingsInheritance, SettlementSupport};

    fn options(support: NativeAuthSupport) -> LaunchOptions {
        LaunchOptions {
            executable: PathBuf::from("/nonexistent/bcode-auth-test/claude"),
            expected_version: "test-only".into(),
            cwd: PathBuf::from("/nonexistent/bcode-auth-test"),
            config_directory: PathBuf::from("/isolated/native-config"),
            environment: NativeEnvironment::default(),
            billing_route: BillingRoute::NativeAccount,
            settings: SettingsInheritance::ExplicitSources,
            setting_sources: Vec::new(),
            resume_session_id: None,
            settlement_support: SettlementSupport::Unknown,
            auth_support: support,
            limits: Limits::default(),
        }
    }

    #[tokio::test]
    async fn auth_support_matrix_fences_before_child_spawn() {
        for support in [
            NativeAuthSupport::Unknown,
            NativeAuthSupport::VerifiedStatus,
            NativeAuthSupport::VerifiedStatusAndLogin,
        ] {
            let options = options(support);
            let status = auth_status(&options).await;
            if support == NativeAuthSupport::Unknown {
                assert!(matches!(
                    status,
                    Err(Error::InvalidConfiguration(
                        "native auth status support unverified"
                    ))
                ));
            } else {
                // Missing executable proves verified status reached version preflight.
                assert!(matches!(status, Err(Error::Io(_))));
            }
            let login = login_handoff(&options).await;
            if support == NativeAuthSupport::VerifiedStatusAndLogin {
                assert!(matches!(login, Err(Error::Io(_))));
            } else {
                // This exact rejection precedes even executable verification.
                assert!(matches!(
                    login,
                    Err(Error::InvalidConfiguration(
                        "native account login handoff unavailable"
                    ))
                ));
            }
        }
    }

    #[test]
    fn verified_status_preserves_namespace_exit_and_billing_checks() {
        for support in [
            NativeAuthSupport::VerifiedStatus,
            NativeAuthSupport::VerifiedStatusAndLogin,
        ] {
            let options = options(support);
            let native = br#"{"loggedIn":true,"authMethod":"claude.ai","configDirectory":"/isolated/native-config"}"#;
            let status = validated_status(&options, true, native).unwrap();
            assert_eq!(status.method, AuthMethod::ClaudeAccount);
            assert_eq!(status.config_directory, options.config_directory);
            assert!(matches!(
                validated_status(&options, false, native),
                Err(Error::Protocol)
            ));
            let wrong_namespace =
                br#"{"loggedIn":true,"authMethod":"claude.ai","configDirectory":"/other"}"#;
            assert!(matches!(
                validated_status(&options, true, wrong_namespace),
                Err(Error::Protocol)
            ));
            let api = br#"{"loggedIn":true,"authMethod":"api_key","configDirectory":"/isolated/native-config"}"#;
            assert!(matches!(
                validated_status(&options, true, api),
                Err(Error::InvalidConfiguration(_))
            ));
            let mut api_options = options;
            api_options.billing_route = BillingRoute::ApiKey;
            assert!(validated_status(&api_options, true, api).is_ok());
            assert!(matches!(
                validated_status(&api_options, true, native),
                Err(Error::InvalidConfiguration(_))
            ));
        }
    }
}
