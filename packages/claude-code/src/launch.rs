use crate::{Error, NativeAuthSupport, SettlementSupport};
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};
use tokio::process::Command;

/// Explicit billing selection; no fallback between native subscription and API credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BillingRoute {
    NativeAccount,
    ApiKey,
}
/// Whether native user/project settings remain active. Managed policy is never bypassed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsInheritance {
    Native,
    ExplicitSources,
}
/// Complete child environment. No ambient variables are inherited by the process.
/// Do not log this value: it can contain credentials.
#[derive(Clone, Default)]
pub struct NativeEnvironment(pub BTreeMap<String, String>);
/// Memory and concurrency bounds, not completion timers.
#[derive(Clone, Debug)]
pub struct Limits {
    pub frame_bytes: usize,
    pub queued_events: usize,
    pub queued_writes: usize,
    pub pending_requests: usize,
    pub active_callbacks: usize,
    pub active_tasks: usize,
    pub request_timeout: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            frame_bytes: 1024 * 1024,
            queued_events: 128,
            queued_writes: 32,
            pending_requests: 32,
            active_callbacks: 32,
            active_tasks: 256,
            request_timeout: Duration::from_secs(60),
        }
    }
}
/// Explicit executable/configuration provenance for one native process.
#[derive(Clone)]
pub struct LaunchOptions {
    pub executable: PathBuf,
    /// Exact trimmed `--version` output, checked before launch.
    pub expected_version: String,
    pub cwd: PathBuf,
    pub config_directory: PathBuf,
    pub environment: NativeEnvironment,
    pub billing_route: BillingRoute,
    pub settings: SettingsInheritance,
    /// Used only with `ExplicitSources`; empty disables optional settings sources.
    pub setting_sources: Vec<String>,
    /// A specific native identity, never `--continue` or most-recent selection.
    pub resume_session_id: Option<String>,
    pub settlement_support: SettlementSupport,
    /// Explicit exact-version native auth handoff attestation, unknown by default.
    pub auth_support: NativeAuthSupport,
    pub limits: Limits,
}
impl LaunchOptions {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        if !self.executable.is_absolute()
            || !self.cwd.is_absolute()
            || !self.config_directory.is_absolute()
            || self.expected_version.is_empty()
            || self.expected_version.len() > 256
        {
            return Err(Error::InvalidConfiguration(
                "absolute paths and exact version required",
            ));
        }
        if self
            .executable
            .extension()
            .and_then(|v| v.to_str())
            .is_some_and(|v| v.eq_ignore_ascii_case("cmd") || v.eq_ignore_ascii_case("bat"))
        {
            return Err(Error::InvalidConfiguration(
                "shell wrappers are unsupported",
            ));
        }
        let l = &self.limits;
        if l.frame_bytes == 0
            || l.frame_bytes > 16 * 1024 * 1024
            || l.queued_events == 0
            || l.queued_events > 4096
            || l.queued_writes == 0
            || l.queued_writes > 1024
            || l.pending_requests == 0
            || l.pending_requests > 1024
            || l.active_callbacks == 0
            || l.active_callbacks > 1024
            || l.active_tasks == 0
            || l.active_tasks > 4096
            || l.request_timeout.is_zero()
        {
            return Err(Error::InvalidConfiguration("invalid transport limits"));
        }
        let env = &self.environment.0;
        if env.contains_key("CLAUDE_CONFIG_DIR")
            || env.contains_key("CLAUDE_CODE_SDK_READS_SESSION_STATE")
        {
            return Err(Error::InvalidConfiguration("reserved environment override"));
        }
        // Alternate cloud endpoints/profile credentials must not silently change billing.
        for key in [
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_PROFILE",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ] {
            if env.contains_key(key) {
                return Err(Error::InvalidConfiguration(
                    "unsupported ambient authentication route",
                ));
            }
        }
        let route_matches = match (self.billing_route, env.get("ANTHROPIC_API_KEY")) {
            (BillingRoute::NativeAccount, None) => true,
            (BillingRoute::ApiKey, Some(key)) => !key.trim().is_empty(),
            _ => false,
        };
        if !route_matches {
            return Err(Error::InvalidConfiguration(
                "explicit billing route does not match credentials",
            ));
        }
        if let Some(id) = &self.resume_session_id {
            validate_id(id)?;
        }
        if self
            .setting_sources
            .iter()
            .any(|v| !matches!(v.as_str(), "user" | "project" | "local"))
        {
            return Err(Error::InvalidConfiguration(
                "unknown native settings source",
            ));
        }
        if self.settings == SettingsInheritance::Native && !self.setting_sources.is_empty() {
            return Err(Error::InvalidConfiguration(
                "ambiguous settings inheritance",
            ));
        }
        Ok(())
    }
    pub(crate) fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command
            .env_clear()
            .envs(&self.environment.0)
            .env("CLAUDE_CONFIG_DIR", &self.config_directory)
            .current_dir(&self.cwd)
            .kill_on_drop(true);
        command
    }
    pub(crate) async fn verify(&self) -> Result<(), Error> {
        self.validate()?;
        let mut command = self.command();
        command.arg("--version");
        let (status, output) =
            crate::auth::bounded_output(command, self.limits.request_timeout, 4096).await?;
        if !status.success() {
            return Err(Error::VersionMismatch);
        }
        if String::from_utf8_lossy(&output).trim() != self.expected_version {
            return Err(Error::VersionMismatch);
        }
        Ok(())
    }
    pub(crate) fn stream_command(&self) -> Command {
        let mut command = self.command();
        command
            .args([
                "--print",
                "--include-partial-messages",
                "--output-format",
                "stream-json",
                "--verbose",
                "--input-format",
                "stream-json",
                "--permission-prompt-tool",
                "stdio",
            ])
            .env("CLAUDE_CODE_SDK_READS_SESSION_STATE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(id) = &self.resume_session_id {
            command.arg(format!("--resume={id}"));
        }
        if self.settings == SettingsInheritance::ExplicitSources {
            command.arg(format!(
                "--setting-sources={}",
                self.setting_sources.join(",")
            ));
        }
        command
    }
}

/// Validate the UUID required by the native `--session-id` create option.
/// # Errors
/// Returns `Protocol` for anything other than a canonical hyphenated UUID.
pub fn validate_session_uuid(id: &str) -> Result<(), Error> {
    if id.len() != 36
        || !id.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()
            }
        })
    {
        return Err(Error::Protocol);
    }
    Ok(())
}
pub fn validate_id(id: &str) -> Result<(), Error> {
    if id.is_empty()
        || id.len() > 256
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(Error::Protocol);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> LaunchOptions {
        LaunchOptions {
            executable: PathBuf::from("/isolated/claude"),
            expected_version: "fixture".into(),
            cwd: PathBuf::from("/isolated/work"),
            config_directory: PathBuf::from("/isolated/config"),
            environment: NativeEnvironment::default(),
            billing_route: BillingRoute::NativeAccount,
            settings: SettingsInheritance::Native,
            setting_sources: Vec::new(),
            resume_session_id: None,
            settlement_support: SettlementSupport::Unknown,
            auth_support: NativeAuthSupport::Unknown,
            limits: Limits::default(),
        }
    }

    #[test]
    fn billing_route_requires_explicit_nonblank_api_credentials() {
        let mut options = options();
        assert!(options.validate().is_ok());
        for key in ["", " ", "\t\n", "fixture-key"] {
            options
                .environment
                .0
                .insert("ANTHROPIC_API_KEY".into(), key.into());
            assert!(options.validate().is_err());
        }
        options.billing_route = BillingRoute::ApiKey;
        options.environment.0.remove("ANTHROPIC_API_KEY");
        assert!(options.validate().is_err());
        for key in ["", " ", "\t\n"] {
            options
                .environment
                .0
                .insert("ANTHROPIC_API_KEY".into(), key.into());
            assert!(options.validate().is_err());
        }
        options
            .environment
            .0
            .insert("ANTHROPIC_API_KEY".into(), "fixture-key".into());
        assert!(options.validate().is_ok());
        options
            .environment
            .0
            .insert("ANTHROPIC_AUTH_TOKEN".into(), "alternate-route".into());
        assert!(options.validate().is_err());
    }
}
