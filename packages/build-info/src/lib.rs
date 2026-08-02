//! Build identity and display version semantics for Bcode artifacts.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use sha2::{Digest as _, Sha256};
use thiserror::Error;

/// Maximum supported display-version length.
pub const MAX_DISPLAY_VERSION_LEN: usize = 128;
/// Number of hexadecimal digits retained in a diagnostic build digest.
pub const SHORT_DIGEST_LEN: usize = 8;

/// Product build mode controlling user-visible version semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildMode {
    /// A local or otherwise non-distribution build with diagnostic identity.
    Developer,
    /// A canonical packaged distribution, shown as the product version only.
    Distribution,
}

/// Git metadata supplemental to the deterministic build digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitState {
    /// Git metadata was unavailable at build time.
    Unavailable,
    /// The build came from the given commit and normalized source state.
    Revision {
        /// Abbreviated hexadecimal commit ID.
        short_commit: String,
        /// Whether the source checkout differed from the recorded commit.
        dirty: bool,
    },
}

/// Validated, renderer-neutral information identifying a Bcode build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildInfo {
    version: String,
    mode: BuildMode,
    git: GitState,
    digest: String,
    target: String,
    profile: String,
    features: Vec<String>,
    compiler: String,
    release_channel: Option<String>,
    built_at_unix_seconds: Option<u64>,
    display_version: String,
}

impl BuildInfo {
    /// Construct validated build information.
    ///
    /// # Errors
    ///
    /// Returns an error when the version, Git commit, or digest is empty, too
    /// long, or contains characters outside its normalized representation.
    pub fn new(
        version: impl Into<String>,
        mode: BuildMode,
        git: GitState,
        digest: impl Into<String>,
    ) -> Result<Self, BuildInfoError> {
        let version = version.into();
        validate_version(&version)?;
        validate_git_state(&git)?;
        let digest = digest.into();
        validate_hex("build digest", &digest, SHORT_DIGEST_LEN)?;
        let display_version = format_display_version(&version, mode, &git, &digest);
        if display_version.len() > MAX_DISPLAY_VERSION_LEN {
            return Err(BuildInfoError::TooLong("display version"));
        }
        Ok(Self {
            version,
            mode,
            git,
            digest,
            target: "unknown".to_owned(),
            profile: "unknown".to_owned(),
            features: Vec::new(),
            compiler: "unknown".to_owned(),
            release_channel: None,
            built_at_unix_seconds: None,
            display_version,
        })
    }

    /// Return the product crate version without a leading `v`.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Return this artifact's explicit build mode.
    #[must_use]
    pub const fn mode(&self) -> BuildMode {
        self.mode
    }

    /// Return supplemental Git metadata.
    #[must_use]
    pub const fn git(&self) -> &GitState {
        &self.git
    }

    /// Return the deterministic diagnostic digest.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Attach normalized diagnostic facts to this build.
    ///
    /// # Errors
    ///
    /// Returns an error when a fact is empty, too long, or contains unsafe
    /// control characters, or when the release timestamp/channel is invalid.
    pub fn with_diagnostics(
        mut self,
        target: impl Into<String>,
        profile: impl Into<String>,
        features: Vec<String>,
        compiler: impl Into<String>,
        release_channel: Option<String>,
        built_at_unix_seconds: Option<u64>,
    ) -> Result<Self, BuildInfoError> {
        self.target = target.into();
        self.profile = profile.into();
        self.compiler = compiler.into();
        validate_diagnostic("target", &self.target, 128)?;
        validate_diagnostic("profile", &self.profile, 64)?;
        validate_diagnostic("compiler", &self.compiler, 512)?;
        let mut features = features;
        features.sort();
        features.dedup();
        for feature in &features {
            validate_diagnostic("feature", feature, 128)?;
        }
        if let Some(channel) = release_channel.as_deref() {
            validate_diagnostic("release channel", channel, 64)?;
        }
        if built_at_unix_seconds
            .is_some_and(|timestamp| !(1..=253_402_300_799).contains(&timestamp))
        {
            return Err(BuildInfoError::InvalidValue("build timestamp"));
        }
        self.features = features;
        self.release_channel = release_channel;
        self.built_at_unix_seconds = built_at_unix_seconds;
        Ok(self)
    }

    /// Return the compilation target triple.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Return the Cargo profile used for this artifact.
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Return sorted final-product feature names.
    #[must_use]
    pub fn features(&self) -> &[String] {
        &self.features
    }

    /// Return the compiler/toolchain identity.
    #[must_use]
    pub fn compiler(&self) -> &str {
        &self.compiler
    }

    /// Return the explicitly supplied release channel.
    #[must_use]
    pub fn release_channel(&self) -> Option<&str> {
        self.release_channel.as_deref()
    }

    /// Return the reproducible build/release timestamp, when supplied.
    #[must_use]
    pub const fn built_at_unix_seconds(&self) -> Option<u64> {
        self.built_at_unix_seconds
    }

    /// Return the canonical label shown by frontends.
    #[must_use]
    pub fn display_version(&self) -> &str {
        &self.display_version
    }
}

/// Normalized build facts included in a developer build digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildFacts {
    /// SHA-256 or equivalent normalized source-tree digest.
    pub source_digest: String,
    /// Rust compilation target triple.
    pub target: String,
    /// Cargo profile name.
    pub profile: String,
    /// Enabled final-product Cargo features.
    pub features: Vec<String>,
    /// Stable compiler/toolchain identity.
    pub compiler: String,
}

impl BuildFacts {
    /// Return a deterministic diagnostic digest for these build facts.
    #[must_use]
    pub fn diagnostic_digest(&self) -> String {
        let mut features = self.features.clone();
        features.sort();
        features.dedup();
        let features = features.join(",");
        diagnostic_digest([
            self.source_digest.as_bytes(),
            self.target.as_bytes(),
            self.profile.as_bytes(),
            features.as_bytes(),
            self.compiler.as_bytes(),
        ])
    }
}

/// Normalize a build-input path to a platform-independent relative form.
#[must_use]
pub fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

/// Build a deterministic short digest from normalized length-prefixed facts.
#[must_use]
pub fn diagnostic_digest<'a>(facts: impl IntoIterator<Item = &'a [u8]>) -> String {
    let mut digest = Sha256::new();
    for fact in facts {
        digest.update(u64::try_from(fact.len()).unwrap_or(u64::MAX).to_le_bytes());
        digest.update(fact);
    }
    let hex = format!("{:x}", digest.finalize());
    hex[..SHORT_DIGEST_LEN].to_owned()
}

fn format_display_version(version: &str, mode: BuildMode, git: &GitState, digest: &str) -> String {
    match mode {
        BuildMode::Distribution => format!("v{version}"),
        BuildMode::Developer => match git {
            GitState::Unavailable => format!("v{version}-dev.nogit.b{digest}"),
            GitState::Revision {
                short_commit,
                dirty: false,
            } => format!("v{version}-dev.g{short_commit}.b{digest}"),
            GitState::Revision {
                short_commit,
                dirty: true,
            } => format!("v{version}-dev.g{short_commit}.dirty.b{digest}"),
        },
    }
}

fn validate_version(version: &str) -> Result<(), BuildInfoError> {
    if version.is_empty() {
        return Err(BuildInfoError::Empty("version"));
    }
    if version.len() > 64 {
        return Err(BuildInfoError::TooLong("version"));
    }
    if !version
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
    {
        return Err(BuildInfoError::InvalidCharacter("version"));
    }
    Ok(())
}

fn validate_git_state(git: &GitState) -> Result<(), BuildInfoError> {
    if let GitState::Revision { short_commit, .. } = git {
        validate_hex("Git commit", short_commit, 40)?;
    }
    Ok(())
}

fn validate_diagnostic(
    field: &'static str,
    value: &str,
    maximum_len: usize,
) -> Result<(), BuildInfoError> {
    if value.is_empty() {
        return Err(BuildInfoError::Empty(field));
    }
    if value.len() > maximum_len {
        return Err(BuildInfoError::TooLong(field));
    }
    if value.chars().any(char::is_control) {
        return Err(BuildInfoError::InvalidCharacter(field));
    }
    Ok(())
}

fn validate_hex(
    field: &'static str,
    value: &str,
    maximum_len: usize,
) -> Result<(), BuildInfoError> {
    if value.is_empty() {
        return Err(BuildInfoError::Empty(field));
    }
    if value.len() > maximum_len {
        return Err(BuildInfoError::TooLong(field));
    }
    if !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(BuildInfoError::InvalidCharacter(field));
    }
    Ok(())
}

/// Invalid normalized build information.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BuildInfoError {
    /// A required field was empty.
    #[error("{0} must not be empty")]
    Empty(&'static str),
    /// A field exceeded its bounded representation.
    #[error("{0} is too long")]
    TooLong(&'static str),
    /// A field contained a semantically invalid value.
    #[error("{0} contains an invalid value")]
    InvalidValue(&'static str),
    /// A field contained a character outside its normalized representation.
    #[error("{0} contains an invalid character")]
    InvalidCharacter(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distribution_uses_only_product_version() {
        let info = BuildInfo::new(
            "1.2.3-alpha.1",
            BuildMode::Distribution,
            GitState::Revision {
                short_commit: "abcdef12".to_owned(),
                dirty: true,
            },
            "1234abcd",
        )
        .expect("build info");
        assert_eq!(info.display_version(), "v1.2.3-alpha.1");
    }

    #[test]
    fn developer_formats_clean_dirty_and_unavailable_git() {
        let clean = BuildInfo::new(
            "1.2.3",
            BuildMode::Developer,
            GitState::Revision {
                short_commit: "abcdef12".to_owned(),
                dirty: false,
            },
            "1234abcd",
        )
        .expect("clean");
        let dirty = BuildInfo::new(
            "1.2.3",
            BuildMode::Developer,
            GitState::Revision {
                short_commit: "abcdef12".to_owned(),
                dirty: true,
            },
            "1234abcd",
        )
        .expect("dirty");
        let unavailable = BuildInfo::new(
            "1.2.3",
            BuildMode::Developer,
            GitState::Unavailable,
            "1234abcd",
        )
        .expect("unavailable");
        assert_eq!(clean.display_version(), "v1.2.3-dev.gabcdef12.b1234abcd");
        assert_eq!(
            dirty.display_version(),
            "v1.2.3-dev.gabcdef12.dirty.b1234abcd"
        );
        assert_eq!(unavailable.display_version(), "v1.2.3-dev.nogit.b1234abcd");
    }

    #[test]
    fn digest_is_stable_ordered_and_length_prefixed() {
        let first = diagnostic_digest([b"ab".as_slice(), b"c".as_slice()]);
        let same = diagnostic_digest([b"ab".as_slice(), b"c".as_slice()]);
        let reordered = diagnostic_digest([b"c".as_slice(), b"ab".as_slice()]);
        let repartitioned = diagnostic_digest([b"a".as_slice(), b"bc".as_slice()]);
        assert_eq!(first, same);
        assert_ne!(first, reordered);
        assert_ne!(first, repartitioned);
    }

    #[test]
    fn build_facts_cover_source_target_profile_features_and_compiler() {
        let baseline = BuildFacts {
            source_digest: "source-a".to_owned(),
            target: "target-a".to_owned(),
            profile: "debug".to_owned(),
            features: vec!["z".to_owned(), "a".to_owned()],
            compiler: "rustc-a".to_owned(),
        };
        let baseline_digest = baseline.diagnostic_digest();
        let mut reordered = baseline.clone();
        reordered.features.reverse();
        assert_eq!(baseline_digest, reordered.diagnostic_digest());

        for changed in [
            BuildFacts {
                source_digest: "source-b".to_owned(),
                ..baseline.clone()
            },
            BuildFacts {
                target: "target-b".to_owned(),
                ..baseline.clone()
            },
            BuildFacts {
                profile: "release".to_owned(),
                ..baseline.clone()
            },
            BuildFacts {
                features: vec!["a".to_owned()],
                ..baseline.clone()
            },
            BuildFacts {
                compiler: "rustc-b".to_owned(),
                ..baseline
            },
        ] {
            assert_ne!(baseline_digest, changed.diagnostic_digest());
        }
    }

    #[test]
    fn path_normalization_is_platform_independent() {
        assert_eq!(
            normalize_path(r"packages\bcode\src\main.rs"),
            "packages/bcode/src/main.rs"
        );
    }

    #[test]
    fn diagnostics_are_sorted_and_validated() {
        let info = BuildInfo::new(
            "1.2.3",
            BuildMode::Developer,
            GitState::Unavailable,
            "1234abcd",
        )
        .and_then(|info| {
            info.with_diagnostics(
                "target",
                "release",
                vec!["z".to_owned(), "a".to_owned(), "a".to_owned()],
                "rustc 1.95.0",
                Some("stable".to_owned()),
                Some(1),
            )
        })
        .expect("diagnostics");
        assert_eq!(info.features(), &["a", "z"]);
        assert_eq!(info.built_at_unix_seconds(), Some(1));
        assert!(
            BuildInfo::new(
                "1.2.3",
                BuildMode::Developer,
                GitState::Unavailable,
                "1234abcd"
            )
            .and_then(|info| info.with_diagnostics(
                "target",
                "release",
                Vec::new(),
                "rustc",
                None,
                Some(0),
            ))
            .is_err()
        );
    }

    #[test]
    fn malformed_metadata_is_rejected() {
        assert_eq!(
            BuildInfo::new(
                "1/2",
                BuildMode::Developer,
                GitState::Unavailable,
                "1234abcd"
            ),
            Err(BuildInfoError::InvalidCharacter("version"))
        );
        assert_eq!(
            BuildInfo::new(
                "1.2.3",
                BuildMode::Developer,
                GitState::Revision {
                    short_commit: "not-git".to_owned(),
                    dirty: false,
                },
                "1234abcd"
            ),
            Err(BuildInfoError::InvalidCharacter("Git commit"))
        );
    }
}
