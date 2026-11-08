//! Local installation probes. No prompts, login, installation or native conversation creation.
use crate::{AuthStatus, Error, LaunchOptions, NativeEnvironment};
use std::{path::Path, time::Duration};
use tokio::process::Command;

/// Native `--version` outputs whose stream-json, settlement and model-selection contracts
/// were behaviorally tested against this driver.
///
/// Membership gates verified features (exact execution tracking, advertised alias
/// resolution, setup attestations). Other parseable versions still launch, but only with
/// the conservative unverified contracts.
pub const TESTED_VERSIONS: &[&str] = &["2.1.308 (Claude Code)"];

/// Whether an exact native `--version` output is in [`TESTED_VERSIONS`].
#[must_use]
pub fn is_tested_version(version: &str) -> bool {
    TESTED_VERSIONS.contains(&version)
}

/// Inspect one explicitly resolved executable using only the caller's trusted environment.
/// This observation is not an exact-version capability attestation.
/// # Errors
/// Process failure, timeout, invalid version shape or bounded output overflow.
pub async fn installed_version(
    executable: &Path,
    cwd: &Path,
    environment: &NativeEnvironment,
) -> Result<String, Error> {
    if !executable.is_absolute() || !cwd.is_absolute() {
        return Err(Error::InvalidConfiguration("absolute probe paths required"));
    }
    let mut command = Command::new(executable);
    command
        .env_clear()
        .envs(&environment.0)
        .current_dir(cwd)
        .arg("--version")
        .kill_on_drop(true);
    let (status, output) =
        crate::auth::bounded_output(command, Duration::from_secs(3), 1024).await?;
    if !status.success() {
        return Err(Error::Protocol);
    }
    parse_version(&output)
}

/// Parse one trimmed native `--version` output of the exact form `X.Y.Z (Claude Code)`.
/// Wrappers, extra lines and other vendors are rejected rather than guessed.
/// # Errors
/// Returns `Protocol` for non-UTF-8, oversized, multi-line or differently shaped output.
pub fn parse_version(output: &[u8]) -> Result<String, Error> {
    let value = std::str::from_utf8(output)
        .map_err(|_| Error::Protocol)?
        .trim();
    if value.len() > 256 || value.chars().any(char::is_control) {
        return Err(Error::Protocol);
    }
    let version = value
        .strip_suffix(" (Claude Code)")
        .ok_or(Error::Protocol)?;
    let parts: Vec<_> = version.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(Error::Protocol);
    }
    Ok(value.into())
}

/// Inspect local native-account readiness, without granting runtime/login capability support.
/// Unknown protocol shapes and alternative billing routes are rejected rather than guessed.
/// # Errors
/// Exact version changed, native process failed, namespace/route mismatch or malformed status.
pub async fn inspect_installed_account(options: &LaunchOptions) -> Result<AuthStatus, Error> {
    if options.billing_route != crate::BillingRoute::NativeAccount {
        return Err(Error::InvalidConfiguration(
            "installation discovery requires native custody",
        ));
    }
    options.verify().await?;
    let mut command = options.command();
    command.args(["auth", "status"]);
    let (status, output) =
        crate::auth::bounded_output(command, Duration::from_secs(3), 16 * 1024).await?;
    if !matches!(status.code(), Some(0 | 1)) {
        return Err(Error::Protocol);
    }
    crate::auth::validated_status(options, status.success(), &output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn installation_version_is_exact_and_vendor_scoped() {
        assert_eq!(
            parse_version(b"2.1.308 (Claude Code)\n").unwrap(),
            "2.1.308 (Claude Code)"
        );
        // Untested but well-formed versions parse; tested membership is separate.
        assert_eq!(
            parse_version(b"2.2.0 (Claude Code)").unwrap(),
            "2.2.0 (Claude Code)"
        );
        assert!(!is_tested_version("2.2.0 (Claude Code)"));
        assert!(TESTED_VERSIONS.iter().all(|version| {
            is_tested_version(version) && parse_version(version.as_bytes()).is_ok()
        }));
        for output in [
            b"2.1.308".as_slice(),
            b"other tool",
            b"2.1.308 (Claude Code)\nsecret",
            b"x.y.z (Claude Code)",
            b"fake-claude 1",
            b"2.1 (Claude Code)",
            b"2.1.308.1 (Claude Code)",
            b"wrapper 2.1.308 (Claude Code)",
        ] {
            assert!(parse_version(output).is_err());
        }
    }
}
