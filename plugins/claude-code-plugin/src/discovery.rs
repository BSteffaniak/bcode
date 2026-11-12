//! Passive native installation discovery, separate from consent and compatibility admission.
use crate::config::{ApiBilling, InstanceConfig};
use bcode_harness_models::{HarnessAuthBinding, HarnessAuthCustody, HarnessInstance};
use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

/// Explicit discovery inputs; no settings files, credential stores or sshenv are consulted.
#[derive(Clone, Debug)]
pub struct ClaudeCodeDiscovery {
    /// Absolute native home used only to locate the default `.claude` directory.
    pub home: PathBuf,
    /// PATH-style executable search list. Relative/empty entries are ignored.
    pub search_path: OsString,
    /// Explicit `CLAUDE_CONFIG_DIR` override; must be an existing absolute directory.
    pub config_directory: Option<PathBuf>,
}

/// Passive candidate, not an authenticated account or supported execution route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeCodeCandidate {
    executable: PathBuf,
    config_directory: PathBuf,
    identity: InstallationIdentity,
}

// A filesystem change stamp, not a cryptographic artifact digest. Executable/script
// dependencies are outside this stamp; compatibility evidence must cover them too.
#[derive(Clone, Debug, PartialEq, Eq)]
struct InstallationIdentity {
    length: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed: (i64, i64),
}

/// Operator/release evidence for precisely this candidate, never inferred from `--version`.
#[derive(Clone, Debug)]
pub struct ClaudeCodeCompatibility {
    /// Snapshot of the installation against which compatibility was established.
    pub candidate: ClaudeCodeCandidate,
    /// Exact trimmed version output established by the compatibility test.
    pub exact_version: String,
    /// Exact-version native local-status behavior and namespace/billing validation were tested.
    pub verified_native_auth_status: bool,
    /// Empty `--setting-sources` support and managed-policy retention were tested.
    pub verified_minimal_settings: bool,
}

/// Secret-free admission failures; no native output is included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeCodePreparationError {
    /// First-use approval has not been obtained by the application.
    ConsentRequired,
    /// Missing or mismatched exact-installation compatibility evidence.
    CompatibilityRequired,
    /// Executable or namespace changed/disappeared after discovery or evidence collection.
    InstallationChanged,
}

/// Discover at most 128 PATH entries without executing anything or opening config files.
/// Relative paths are ignored; canonical executable/namespace paths are deduplicated.
#[must_use]
pub fn discover_candidates_in(input: &ClaudeCodeDiscovery) -> Vec<ClaudeCodeCandidate> {
    if !input.home.is_absolute() {
        return Vec::new();
    }
    let namespace = input
        .config_directory
        .clone()
        .unwrap_or_else(|| input.home.join(".claude"));
    if !namespace.is_absolute() || !namespace.is_dir() {
        return Vec::new();
    }
    let Ok(namespace) = namespace.canonicalize() else {
        return Vec::new();
    };
    // Public harness contracts require lossless UTF-8 path identities.
    if namespace.to_str().is_none() {
        return Vec::new();
    }
    let mut seen = BTreeSet::new();
    env::split_paths(&input.search_path)
        .take(128)
        .filter(|directory| directory.is_absolute())
        .filter_map(|directory| directory.join("claude").canonicalize().ok())
        .filter(|path| path.to_str().is_some() && seen.insert(path.clone()))
        .filter_map(|executable| {
            let identity = installation_identity(&executable)?;
            Some(ClaudeCodeCandidate {
                executable,
                config_directory: namespace.clone(),
                identity,
            })
        })
        .collect()
}

/// Passive discovery using only HOME, PATH and explicit `CLAUDE_CONFIG_DIR`, never sshenv.
#[must_use]
pub fn discover_candidates() -> Vec<ClaudeCodeCandidate> {
    let Some(home) = env::var_os("HOME") else {
        return Vec::new();
    };
    let Some(search_path) = env::var_os("PATH") else {
        return Vec::new();
    };
    discover_candidates_in(&ClaudeCodeDiscovery {
        home: home.into(),
        search_path,
        config_directory: env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from),
    })
}

fn installation_identity(path: &Path) -> Option<InstallationIdentity> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o111 == 0 {
            return None;
        }
        Some(InstallationIdentity {
            length: metadata.len(),
            modified: metadata.modified().ok()?,
            device: metadata.dev(),
            inode: metadata.ino(),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
    #[cfg(not(unix))]
    {
        Some(InstallationIdentity {
            length: metadata.len(),
            modified: metadata.modified().ok()?,
        })
    }
}

impl ClaudeCodeCandidate {
    /// Canonical path; discovery has not executed it.
    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Canonical native credential/config namespace; contents have not been read.
    #[must_use]
    pub fn config_directory(&self) -> &Path {
        &self.config_directory
    }

    /// Prepare existing native-managed harness configuration without running a process.
    /// The application must obtain/validate explicit consent (including its configured scope)
    /// before passing `approved = true`, and separately authorize later status or launch.
    /// Compatibility is operator/release evidence, not a version-number assertion. Revalidate
    /// immediately before use; the filesystem stamp detects ordinary updates, not hostile
    /// modification or updates of a script's interpreter/wrapped artifact. Existing launch
    /// code additionally checks exact version before status/launch.
    ///
    /// # Errors
    /// Missing consent, missing/mismatched evidence, or changed executable/namespace.
    pub fn prepare(
        &self,
        approved: bool,
        evidence: &ClaudeCodeCompatibility,
    ) -> Result<InstanceConfig, ClaudeCodePreparationError> {
        if !approved {
            return Err(ClaudeCodePreparationError::ConsentRequired);
        }
        if evidence.candidate != *self
            || evidence.exact_version.is_empty()
            || evidence.exact_version.len() > 256
            || evidence.exact_version.trim() != evidence.exact_version
            || evidence.exact_version.chars().any(char::is_control)
            || !evidence.verified_native_auth_status
            || !evidence.verified_minimal_settings
        {
            return Err(ClaudeCodePreparationError::CompatibilityRequired);
        }
        if installation_identity(&self.executable).as_ref() != Some(&self.identity)
            || self.executable.canonicalize().ok().as_ref() != Some(&self.executable)
            || !self.config_directory.is_dir()
            || self.config_directory.canonicalize().ok().as_ref() != Some(&self.config_directory)
        {
            return Err(ClaudeCodePreparationError::InstallationChanged);
        }
        Ok(InstanceConfig {
            billing: ApiBilling::Native,
            executable: self.executable.clone(),
            config_directory: self.config_directory.clone(),
            exact_version: evidence.exact_version.clone(),
            verified_session_state: false,
            verified_native_auth_status: true,
            verified_native_auth: false,
            setting_sources: Some(Vec::new()),
        })
    }

    /// Build the matching non-secret native-custody binding for coordinator admission.
    /// This grants no consent, readiness, credential access, or process authority.
    #[must_use]
    pub fn harness_instance(&self, instance_id: String, profile: String) -> HarnessInstance {
        let namespace = self.config_directory.to_string_lossy().into_owned();
        HarnessInstance {
            instance_id,
            plugin_id: crate::PLUGIN_ID.into(),
            executable: self.executable.to_string_lossy().into_owned(),
            config_namespace: namespace.clone(),
            auth: HarnessAuthBinding {
                provider_id: "claude-code".into(),
                profile,
                credential_namespace: namespace,
                custody: HarnessAuthCustody::NativeManaged,
            },
        }
    }
}

#[cfg(test)]
#[path = "discovery_tests.rs"]
mod tests;
