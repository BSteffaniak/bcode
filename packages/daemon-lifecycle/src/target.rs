//! Explicit, verified targeting for independently built daemon consumers.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use bcode_config::StateLocation;
use bcode_ipc::{ArtifactId, DaemonStatus, IpcEndpoint};
use tokio::io::AsyncReadExt as _;

/// Secret-safe failure to verify an explicit daemon target. No substitute is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DaemonTargetError {
    /// Paths or digest are invalid, inaccessible or inconsistent.
    #[error("explicit daemon executable, digest or configuration directory is invalid")]
    InvalidSelection,
    /// The selected executable did not provide a bounded supported identity response.
    #[error("selected daemon executable identity could not be verified")]
    IdentityUnavailable,
    /// Discovery is missing, ambiguous, incompatible or unavailable.
    #[error(
        "no uniquely verified running daemon matches the explicit target; start the selected Bcode artifact with the selected state and configuration"
    )]
    Unavailable,
    /// The serving daemon does not match the verified target and compatibility contract.
    #[error(
        "serving daemon identity, state or storage compatibility does not match the explicit target"
    )]
    Incompatible,
}

/// Verified running daemon artifact and state/configuration scope.
///
/// Construction performs bounded identity discovery, not startup, replacement, repair or
/// session access. Discovery records are evidence only: the live handshake must match too.
/// Keeping this value does not reserve the daemon or confer session authority.
#[derive(Debug, Clone)]
pub struct DaemonTarget {
    executable: PathBuf,
    digest: String,
    location: StateLocation,
    scope: String,
    endpoint: IpcEndpoint,
    artifact: ArtifactId,
    namespace: String,
    build: String,
}

impl DaemonTarget {
    /// Verify an absolute executable against a caller-selected SHA-256 and discover its
    /// exact already-running daemon in a resolved state and configuration-directory scope.
    ///
    /// `artifact-id` is the selected executable's supported identity interface. Neither the
    /// consumer executable nor a guessed digest-to-artifact conversion is used. The trusted
    /// executable is invoked with no credentials/configuration payload and bounded output/time.
    ///
    /// # Errors
    /// Rejects invalid paths/digests, changed executable bytes, unsupported identity output,
    /// incomplete/ambiguous discovery, unavailable daemons and incompatible live status.
    pub async fn verify(
        executable: PathBuf,
        expected_sha256: String,
        location: StateLocation,
        config_directory: PathBuf,
    ) -> Result<Self, DaemonTargetError> {
        if !executable.is_absolute()
            || !config_directory.is_absolute()
            || expected_sha256.len() != 64
            || !expected_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            || !config_directory.is_dir()
        {
            return Err(DaemonTargetError::InvalidSelection);
        }
        let executable = executable
            .canonicalize()
            .map_err(|_| DaemonTargetError::InvalidSelection)?;
        if super::executable_sha256(&executable).map_err(|_| DaemonTargetError::InvalidSelection)?
            != expected_sha256
        {
            return Err(DaemonTargetError::InvalidSelection);
        }
        let artifact = probe_artifact(&executable).await?;
        if super::executable_sha256(&executable)
            .map_err(|_| DaemonTargetError::IdentityUnavailable)?
            != expected_sha256
        {
            return Err(DaemonTargetError::IdentityUnavailable);
        }
        let namespace = bcode_ipc::daemon_namespace_for(&artifact);
        let scope = bcode_config::runtime_scope_id(location.root(), &config_directory);
        let records = super::read_records_strict(location.root(), 4096)
            .map_err(|_| DaemonTargetError::Unavailable)?;
        let mut matches = records.into_iter().filter(|record| {
            record.artifact_id.as_ref() == Some(&artifact)
                && record.namespace == namespace
                && record.state_location_id.as_deref() == Some(scope.as_str())
                && record.executable_digest.as_deref() == Some(expected_sha256.as_str())
        });
        let record = matches.next().ok_or(DaemonTargetError::Unavailable)?;
        if matches.next().is_some() || record.build_fingerprint.is_empty() {
            return Err(DaemonTargetError::Unavailable);
        }
        let endpoint = record
            .endpoint
            .to_ipc_endpoint()
            .ok_or(DaemonTargetError::Unavailable)?;
        let target = Self {
            executable,
            digest: expected_sha256,
            location,
            scope,
            endpoint,
            artifact,
            namespace,
            build: record.build_fingerprint.clone(),
        };
        let status = tokio::time::timeout(Duration::from_secs(5), target.probe_status())
            .await
            .map_err(|_| DaemonTargetError::Unavailable)??;
        target.verify_status(&status)?;
        if !super::daemon_status_matches_record(&record, &status) {
            return Err(DaemonTargetError::Incompatible);
        }
        Ok(target)
    }

    /// Verified executable selected by the caller; never the consumer executable.
    #[must_use]
    pub fn executable(&self) -> &std::path::Path {
        &self.executable
    }
    /// Resolved canonical durable location.
    #[must_use]
    pub const fn state_location(&self) -> &StateLocation {
        &self.location
    }
    /// Exact artifact-scoped transport endpoint, for the client adapter.
    #[must_use]
    pub const fn endpoint(&self) -> &IpcEndpoint {
        &self.endpoint
    }
    /// Verified artifact identity, for the client handshake.
    #[must_use]
    pub const fn artifact_id(&self) -> &ArtifactId {
        &self.artifact
    }
    /// Verified namespace, for the client handshake.
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    /// Verified build diagnostic, for the client handshake.
    #[must_use]
    pub fn build_fingerprint(&self) -> &str {
        &self.build
    }
    /// Exact state/configuration scope, for the client handshake.
    #[must_use]
    pub fn state_scope_id(&self) -> &str {
        &self.scope
    }

    /// Recheck selected executable bytes before a connection uses this target.
    /// # Errors
    /// Rejects unavailable or replaced executable bytes without selecting a substitute.
    pub fn verify_executable(&self) -> Result<(), DaemonTargetError> {
        if super::executable_sha256(&self.executable)
            .map_err(|_| DaemonTargetError::Incompatible)?
            != self.digest
        {
            return Err(DaemonTargetError::Incompatible);
        }
        Ok(())
    }

    /// Validate every live connection against the selected artifact and current portable
    /// protocol/storage contract. A new process instance is allowed only with identical facts.
    /// # Errors
    /// Rejects mismatched or missing identities, future/older protocol and storage contracts.
    pub fn verify_status(&self, status: &DaemonStatus) -> Result<(), DaemonTargetError> {
        self.verify_executable()?;
        if status.namespace != self.namespace
            || status.artifact_id.as_ref() != Some(&self.artifact)
            || status.executable_digest.as_deref() != Some(self.digest.as_str())
            || status.state_location_id.as_deref() != Some(self.scope.as_str())
            || status.build_fingerprint != self.build
            || status.protocol_version != u32::from(bcode_ipc::CURRENT_PROTOCOL_VERSION)
            || status.storage_writer_epoch != Some(bcode_ipc::CURRENT_SESSION_STORAGE_WRITER_EPOCH)
            || status.session_event_schema_version
                != Some(bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION)
        {
            return Err(DaemonTargetError::Incompatible);
        }
        Ok(())
    }

    async fn probe_status(&self) -> Result<DaemonStatus, DaemonTargetError> {
        let mut stream = bcode_ipc::LocalIpcStream::connect(&self.endpoint)
            .await
            .map_err(|_| DaemonTargetError::Unavailable)?;
        let request = bcode_ipc::Request::Hello {
            client_name: "bcode-explicit-target-verification".into(),
            runtime_context: None,
            daemon_namespace: self.namespace.clone(),
            artifact_id: Some(self.artifact.clone()),
            build_fingerprint: self.build.clone(),
            state_location_id: Some(self.scope.clone()),
        };
        let envelope =
            bcode_ipc::request_envelope(1, &request).map_err(|_| DaemonTargetError::Unavailable)?;
        bcode_ipc::send_envelope(&mut stream, &envelope)
            .await
            .map_err(|_| DaemonTargetError::Unavailable)?;
        let response = bcode_ipc::recv_envelope(&mut stream)
            .await
            .map_err(|_| DaemonTargetError::Unavailable)?;
        if response.request_id != 1 || response.kind != bcode_ipc::EnvelopeKind::Response {
            return Err(DaemonTargetError::Incompatible);
        }
        match bcode_ipc::decode_response(&response.payload)
            .map_err(|_| DaemonTargetError::Unavailable)?
        {
            bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello { daemon, .. }) => Ok(daemon),
            _ => Err(DaemonTargetError::Incompatible),
        }
    }
}

async fn probe_artifact(executable: &std::path::Path) -> Result<ArtifactId, DaemonTargetError> {
    let mut child = tokio::process::Command::new(executable)
        .arg("artifact-id")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| DaemonTargetError::IdentityUnavailable)?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or(DaemonTargetError::IdentityUnavailable)?;
    let mut bytes = Vec::new();
    let status = tokio::time::timeout(Duration::from_secs(5), async {
        (&mut stdout).take(257).read_to_end(&mut bytes).await?;
        if bytes.len() > 256 {
            return Err(std::io::Error::other("identity output exceeds limit"));
        }
        child.wait().await
    })
    .await
    .map_err(|_| DaemonTargetError::IdentityUnavailable)?
    .map_err(|_| DaemonTargetError::IdentityUnavailable)?;
    if !status.success() {
        return Err(DaemonTargetError::IdentityUnavailable);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| DaemonTargetError::IdentityUnavailable)?;
    let id = text
        .strip_suffix('\n')
        .ok_or(DaemonTargetError::IdentityUnavailable)?;
    if id.contains(['\r', '\n']) {
        return Err(DaemonTargetError::IdentityUnavailable);
    }
    ArtifactId::parse(id).map_err(|_| DaemonTargetError::IdentityUnavailable)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, DaemonTarget, DaemonStatus) {
        let directory = tempfile::tempdir().unwrap();
        let location = bcode_config::resolve_state_location_set(
            &bcode_config::StateConfig::default(),
            &bcode_config::StateLocationSelection {
                root: Some(directory.path().to_path_buf()),
                profile: None,
            },
        )
        .unwrap()
        .primary()
        .clone();
        let artifact = ArtifactId::parse("selected-daemon-not-consumer").unwrap();
        let executable = directory.path().join("bcode");
        std::fs::write(&executable, b"verified selected executable fixture").unwrap();
        let digest = super::super::executable_sha256(&executable).unwrap();
        let target = DaemonTarget {
            executable,
            digest,
            scope: "selected-state-config".into(),
            location,
            endpoint: IpcEndpoint::unix_socket(directory.path().join("target.sock")),
            namespace: bcode_ipc::daemon_namespace_for(&artifact),
            artifact,
            build: "selected-build".into(),
        };
        let status = DaemonStatus {
            namespace: target.namespace.clone(),
            artifact_id: Some(target.artifact.clone()),
            executable_digest: Some(target.digest.clone()),
            build_fingerprint: target.build.clone(),
            state_location_id: Some(target.scope.clone()),
            protocol_version: u32::from(bcode_ipc::CURRENT_PROTOCOL_VERSION),
            storage_writer_epoch: Some(bcode_ipc::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
            session_event_schema_version: Some(
                bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            ),
            pid: None,
            instance_id: "first".into(),
            started_at_unix_ms: 0,
        };
        (directory, target, status)
    }

    #[test]
    fn explicit_target_rejects_identity_and_compatibility_mismatches() {
        let (_directory, target, matching) = fixture();
        target.verify_status(&matching).unwrap();
        let mut cases = Vec::new();
        let mut status = matching.clone();
        status.namespace.push('x');
        cases.push(status);
        let mut status = matching.clone();
        status.artifact_id = Some(ArtifactId::current());
        cases.push(status);
        let mut status = matching.clone();
        status.executable_digest = None;
        cases.push(status);
        let mut status = matching.clone();
        status.state_location_id = Some("other-state".into());
        cases.push(status);
        let mut status = matching.clone();
        status.build_fingerprint.push('x');
        cases.push(status);
        let mut status = matching.clone();
        status.protocol_version += 1;
        cases.push(status);
        let mut status = matching.clone();
        status.storage_writer_epoch = None;
        cases.push(status);
        let mut status = matching;
        status.session_event_schema_version = None;
        cases.push(status);
        for status in cases {
            assert_eq!(
                target.verify_status(&status),
                Err(DaemonTargetError::Incompatible)
            );
        }
    }

    #[test]
    fn replacement_process_must_preserve_exact_target_not_consumer_identity() {
        let (_directory, target, mut status) = fixture();
        status.instance_id = "replacement".into();
        status.started_at_unix_ms = 42;
        target.verify_status(&status).unwrap();
        status.artifact_id = Some(ArtifactId::current());
        assert!(target.verify_status(&status).is_err());
    }

    #[test]
    fn replaced_executable_is_rejected_even_with_previously_matching_status() {
        let (_directory, target, status) = fixture();
        std::fs::write(target.executable(), b"substituted executable").unwrap();
        assert_eq!(
            target.verify_status(&status),
            Err(DaemonTargetError::Incompatible)
        );
    }

    #[test]
    fn state_and_configuration_scopes_are_independent() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let config_a = tempfile::tempdir().unwrap();
        let config_b = tempfile::tempdir().unwrap();
        let a = bcode_config::runtime_scope_id(first.path(), config_a.path());
        assert_ne!(
            a,
            bcode_config::runtime_scope_id(second.path(), config_a.path())
        );
        assert_ne!(
            a,
            bcode_config::runtime_scope_id(first.path(), config_b.path())
        );
    }

    #[tokio::test]
    async fn wrong_hash_is_rejected_before_identity_execution() {
        let (directory, target, _) = fixture();
        let result = DaemonTarget::verify(
            target.executable.clone(),
            "0".repeat(64),
            target.location,
            directory.path().to_path_buf(),
        )
        .await;
        assert!(matches!(result, Err(DaemonTargetError::InvalidSelection)));
    }

    #[tokio::test]
    async fn identity_probe_rejects_oversized_output_and_nonzero_exit() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("probe");
        for script in [
            "#!/bin/sh\nwhile :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done\n",
            "#!/bin/sh\nprintf 'valid-artifact\\n'; exit 1\n",
        ] {
            std::fs::write(&executable, script).unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(
                probe_artifact(&executable).await,
                Err(DaemonTargetError::IdentityUnavailable)
            );
        }
    }

    #[tokio::test]
    async fn invalid_selection_does_not_execute_or_substitute_a_target() {
        let (_directory, target, _) = fixture();
        assert!(matches!(
            DaemonTarget::verify(
                PathBuf::from("relative"),
                "a".repeat(64),
                target.location,
                PathBuf::from("/missing-config")
            )
            .await,
            Err(DaemonTargetError::InvalidSelection)
        ));
    }
}
