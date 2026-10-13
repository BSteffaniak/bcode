//! Owner-fenced provider enrollment into retained custody, never a legacy-vault import.

use super::{AuthCustodyKeySource, RetainedAuthRequestCustody};
use crate::{
    ResolvedAuthProfile,
    custody_storage::{AuthRefreshIntent, CredentialCustodyStorage},
    interactive::{FlowProgress, InteractiveEnrollment},
    lifecycle::{AuthVaultLifecycle, AuthVaultLifecycleError},
};
use bcode_provider_auth_models::{
    AuthFlowEffect, AuthFlowRequest, AuthFlowResponse, AuthFlowStatus, AuthMethodContribution,
};
use std::{collections::BTreeMap, path::Path};
use zeroize::Zeroizing;

/// Explicit authorization selected by the application, not inferred from damaged state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustodyEnrollmentMode {
    /// Enroll only into empty, unfenced custody.
    Initialize,
    /// A new provider enrollment after cancelling/settling all previous provider work.
    /// Never resumes or replays the exchange associated with an existing durable fence.
    Reconnect,
}

/// Local readiness only: no remote verification or automatic repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustodyEnrollmentReadiness {
    /// Empty custody, suitable for an initial connection.
    Empty,
    /// Locally readable credentials. Provider verification remains a separate operation.
    Ready,
    /// An unresolved remote exchange requires explicit reconnect authorization.
    ReconnectRequired,
}

/// One owner-bound interactive attempt. Opaque state, answers, and credentials are not printable.
/// Dropping this value intentionally preserves its durable fence.
pub struct CustodyEnrollment {
    attempt: String,
    fence: AuthRefreshIntent,
    flow: InteractiveEnrollment,
    dispatched: bool,
    succeeded: bool,
}

impl std::fmt::Debug for CustodyEnrollment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CustodyEnrollment([REDACTED])")
    }
}

impl CustodyEnrollment {
    /// Admit the next invocation exactly once. An uncertain invocation cannot be reissued.
    ///
    /// # Errors
    /// Rejects pending/terminal invocations. A missing response requires explicit reconnect.
    pub fn next_request(&mut self) -> Result<&AuthFlowRequest, &'static str> {
        if self.dispatched {
            return Err("Authentication invocation outcome is unresolved; reconnect required");
        }
        self.flow
            .request()?
            .validate()
            .map_err(|_| "Invalid authentication request")?;
        self.dispatched = true;
        self.flow.request()
    }

    /// Accept a response from this invocation before presenting effects.
    ///
    /// # Errors
    /// Invalid responses remain unresolved and cannot authorize publication or another invocation.
    pub fn accept(&mut self, response: AuthFlowResponse) -> Result<FlowProgress, &'static str> {
        if !self.dispatched {
            return Err("Authentication invocation was not admitted");
        }
        let progress = self.flow.accept(response)?;
        self.dispatched = false;
        self.succeeded = progress.status == AuthFlowStatus::Succeeded;
        Ok(progress)
    }

    /// Answer a validated provider prompt without exposing opaque state.
    ///
    /// # Errors
    /// Rejects answers while an invocation is outstanding, terminal flows, or invalid prompts.
    pub fn answer(&mut self, prompt: &AuthFlowEffect, value: String) -> Result<(), &'static str> {
        if self.dispatched {
            return Err("Authentication invocation outcome is unresolved; reconnect required");
        }
        self.flow.answer(prompt, value)
    }
}

impl RetainedAuthRequestCustody {
    /// Initialize a fresh age-only custody directory through the typed vault lifecycle.
    /// No provider effects or legacy ciphertext reads occur. The caller explicitly authorizes
    /// creation and supplies a recipient matching the retained identities. Ancestors must be trusted.
    ///
    /// # Errors
    /// Rejects ownership/method mismatch, implicit/device policy, existing destinations, unlock
    /// failure, or uncertain publication. Interrupted initialization is preserved for maintenance.
    #[allow(clippy::too_many_arguments)]
    pub fn initialize_enrollment(
        directory: &Path,
        resolved: ResolvedAuthProfile,
        provider_id: &str,
        plugin_id: &str,
        method: AuthMethodContribution,
        recipient: &str,
        identities: Vec<Zeroizing<String>>,
        keys: &dyn AuthCustodyKeySource,
    ) -> Result<Self, AuthVaultLifecycleError> {
        validate_service_policy(&resolved, &method)?;
        if !recipient.starts_with("age1")
            || identities.len() != 1
            || !identities[0].trim().starts_with("AGE-SECRET-KEY-")
        {
            return Err(AuthVaultLifecycleError::InvalidCredential);
        }
        let lifecycle = AuthVaultLifecycle::new(&resolved, provider_id, plugin_id, &method)?;
        let bytes = lifecycle.prepare_custody(recipient, keys)?;
        let storage =
            CredentialCustodyStorage::create(directory, &bytes).map_err(|_| unavailable())?;
        let retained = Self::new(
            storage,
            resolved,
            provider_id,
            plugin_id,
            method,
            identities,
            None,
        )?;
        if retained.enrollment_readiness()? != CustodyEnrollmentReadiness::Empty {
            return Err(unavailable());
        }
        Ok(retained)
    }

    /// Inspect the retained owner without probing, mutating, or falling back to a legacy vault.
    ///
    /// # Errors
    /// Invalid policy, damaged state, unresolved provisioning, or unavailable unlock requires
    /// maintenance rather than silently reporting empty custody.
    pub fn enrollment_readiness(
        &self,
    ) -> Result<CustodyEnrollmentReadiness, AuthVaultLifecycleError> {
        validate_service_policy(&self.resolved, &self.method)?;
        let lifecycle = self.refresh_lifecycle(&self.resolved)?;
        let storage = self.storage.lock().map_err(|_| unavailable())?;
        self.enrollment_readiness_with(storage.as_ref(), &lifecycle)
    }

    fn enrollment_readiness_with(
        &self,
        storage: &dyn crate::custody_storage::AuthCustodyStorage,
        lifecycle: &AuthVaultLifecycle<'_>,
    ) -> Result<CustodyEnrollmentReadiness, AuthVaultLifecycleError> {
        storage
            .ensure_no_provisioning()
            .map_err(|_| unavailable())?;
        if self.passphrase.is_some()
            || self.identities.len() != 1
            || !self.identities[0].trim().starts_with("AGE-SECRET-KEY-")
        {
            return Err(AuthVaultLifecycleError::InvalidCredential);
        }
        let identities = self
            .identities
            .iter()
            .map(|value| value.as_str())
            .collect::<Vec<_>>();
        lifecycle.validate_enrollment_custody(storage, &identities)?;
        if storage.ensure_no_refresh().is_err() {
            let intent = storage.refresh_intent().map_err(|_| unavailable())?;
            if intent.profile_binding != super::provisioning_binding(&self.resolved)? {
                return Err(unavailable());
            }
            return Ok(CustodyEnrollmentReadiness::ReconnectRequired);
        }
        let values = lifecycle.read_from_custody(storage, &identities, None)?;
        let values = values
            .into_iter()
            .map(|(id, value)| (id, Zeroizing::new(value)))
            .collect::<BTreeMap<_, _>>();
        Ok(if values.is_empty() {
            CustodyEnrollmentReadiness::Empty
        } else {
            CustodyEnrollmentReadiness::Ready
        })
    }

    /// Durably fence before the existing registered provider's first interactive invocation.
    /// Reconnect requires application authorization and settled/cancelled prior invocation work.
    /// The returned flow supplies the same provider/method/profile binding used for publication.
    ///
    /// # Errors
    /// Rejects competing attempts, nonempty initial custody, foreign fences, and uncertain writes.
    pub fn begin_enrollment(
        &self,
        mode: CustodyEnrollmentMode,
    ) -> Result<CustodyEnrollment, AuthVaultLifecycleError> {
        validate_service_policy(&self.resolved, &self.method)?;
        let lifecycle = self.refresh_lifecycle(&self.resolved)?;
        let storage = self.storage.lock().map_err(|_| unavailable())?;
        let readiness = self.enrollment_readiness_with(storage.as_ref(), &lifecycle)?;
        let mut active = self.active_enrollment.lock().map_err(|_| unavailable())?;
        if mode == CustodyEnrollmentMode::Initialize
            && (readiness != CustodyEnrollmentReadiness::Empty || active.is_some())
        {
            return Err(unavailable());
        }
        let attempt = hex::encode(&sshenv_vault::crypto::generate_data_key()[..]);
        let binding = super::provisioning_binding(&self.resolved)?;
        let fence = if readiness == CustodyEnrollmentReadiness::ReconnectRequired {
            let fence = storage.refresh_intent().map_err(|_| unavailable())?;
            if fence.profile_binding != binding {
                return Err(unavailable());
            }
            fence
        } else {
            let fence = AuthRefreshIntent {
                version: 1,
                attempt: attempt.clone(),
                profile_binding: binding,
            };
            storage
                .ensure_no_provisioning()
                .map_err(|_| unavailable())?;
            storage.begin_refresh(&fence).map_err(|_| unavailable())?;
            fence
        };
        // Enrollment and refresh share the durable exclusion slot. Reconnect invalidates all
        // process-local commit authority; the durable identity alone never revives an old attempt.
        *self.active_refresh.lock().map_err(|_| unavailable())? = None;
        *active = Some(attempt.clone());
        drop(active);
        drop(storage);
        Ok(CustodyEnrollment {
            attempt,
            fence,
            flow: InteractiveEnrollment::new(
                self.resolved.provider_id.clone(),
                self.method.method_id().to_owned(),
                self.resolved.profile_name.clone(),
            ),
            dispatched: false,
            succeeded: false,
        })
    }

    /// Consume successful provider output directly into a fenced durable custody commit.
    /// Only after success may the application publish metadata and probe the provider.
    /// Absent registered optional values are removed, never inherited from a prior subscription.
    ///
    /// # Errors
    /// Rejects unfinished, stale, foreign, or invalid output. Any publication ambiguity preserves
    /// the fence and consumes commit authority; reconnect must perform a new provider enrollment.
    pub fn complete_enrollment(
        &self,
        mut enrollment: CustodyEnrollment,
    ) -> Result<(), AuthVaultLifecycleError> {
        if enrollment.dispatched || !enrollment.succeeded {
            return Err(unavailable());
        }
        let lifecycle = self.refresh_lifecycle(&self.resolved)?;
        let mut storage = self.storage.lock().map_err(|_| unavailable())?;
        let mut active = self.active_enrollment.lock().map_err(|_| unavailable())?;
        if active.as_deref() != Some(enrollment.attempt.as_str())
            || storage.refresh_intent().map_err(|_| unavailable())? != enrollment.fence
            || enrollment.fence.profile_binding != super::provisioning_binding(&self.resolved)?
        {
            return Err(unavailable());
        }
        *active = None;
        drop(active);
        let credentials = enrollment.flow.take_credentials();
        if credentials.is_empty() {
            return Err(AuthVaultLifecycleError::InvalidCredential);
        }
        // The lifecycle validates registered IDs while mapping to storage keys. The flow already
        // validated secret bounds; provider-specific completeness remains provider-owned.
        let mut record = lifecycle
            .credential_storage_keys()?
            .into_keys()
            .map(|id| (id, None))
            .collect::<BTreeMap<_, _>>();
        for (id, value) in credentials {
            record.insert(id, Some(value));
        }
        self.publish_refresh_record(&lifecycle, storage.as_mut(), &enrollment.fence, record)?;
        drop(storage);
        Ok(())
    }
}

/// Read one explicitly selected service identity without ambient key discovery or fallback.
///
/// The caller must authorize local secret acquisition. Only an absolute, owner-only regular file
/// is accepted; final symlinks and oversized/multiple identities fail closed.
///
/// # Errors
/// Returns a secret-safe failure for unsupported permissions, unavailable files, or invalid input.
pub fn read_custody_identity(path: &Path) -> Result<Zeroizing<String>, AuthVaultLifecycleError> {
    use std::{
        io::Read as _,
        os::unix::fs::{MetadataExt as _, OpenOptionsExt as _},
    };
    if !path.is_absolute() {
        return Err(AuthVaultLifecycleError::InvalidCredential);
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| unavailable())?;
    let metadata = file.metadata().map_err(|_| unavailable())?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > 4096
    {
        return Err(AuthVaultLifecycleError::InvalidCredential);
    }
    let mut identity = Zeroizing::new(String::new());
    file.by_ref()
        .take(4097)
        .read_to_string(&mut identity)
        .map_err(|_| unavailable())?;
    if identity.len() > 4096 {
        return Err(AuthVaultLifecycleError::InvalidCredential);
    }
    let trimmed = Zeroizing::new(identity.trim().to_owned());
    if !trimmed.starts_with("AGE-SECRET-KEY-1") || trimmed.contains(['\n', '\r']) {
        return Err(AuthVaultLifecycleError::InvalidCredential);
    }
    Ok(trimmed)
}

fn validate_service_policy(
    resolved: &ResolvedAuthProfile,
    method: &AuthMethodContribution,
) -> Result<(), AuthVaultLifecycleError> {
    if resolved
        .profile
        .settings
        .get("device_seal")
        .map(String::as_str)
        != Some("off")
        || !matches!(method, AuthMethodContribution::Interactive { .. })
        || resolved.profile.backend != "sshenv"
        || method.validate().is_err()
    {
        return Err(AuthVaultLifecycleError::InvalidCredential);
    }
    Ok(())
}

const fn unavailable() -> AuthVaultLifecycleError {
    AuthVaultLifecycleError::RefreshBlocked
}

#[cfg(all(test, not(feature = "custody-simulation")))]
mod tests {
    use super::*;
    use bcode_provider_auth_models::AuthCredentialStorage;

    // Public age crate test vector, never an operator identity or live account.
    const IDENTITY: &str =
        "AGE-SECRET-KEY-1GQ9778VQXMMJVE8SK7J6VT8UJ4HDQAJUVSFCWCM02D8GEWQ72PVQ2Y5J33";
    const RECIPIENT: &str = "age1t7rxyev2z3rw82stdlrrepyc39nvn86l5078zqkf5uasdy86jp6svpy7pa";

    struct Keys;
    impl AuthCustodyKeySource for Keys {
        fn generate(&self) -> Result<Zeroizing<[u8; 32]>, AuthVaultLifecycleError> {
            Ok(Zeroizing::new(*sshenv_vault::crypto::generate_data_key()))
        }
    }

    fn selection() -> (ResolvedAuthProfile, AuthMethodContribution) {
        let resolved = ResolvedAuthProfile {
            profile_name: "account".into(),
            provider_id: "test-provider".into(),
            owner_plugin_id: "test-plugin".into(),
            profile: bcode_config::AuthProfileConfig {
                backend: "sshenv".into(),
                provider_id: Some("test-provider".into()),
                owner_plugin_id: Some("test-plugin".into()),
                scheme: Some("subscription".into()),
                settings: BTreeMap::from([("device_seal".into(), "off".into())]),
                ..Default::default()
            },
            source: crate::AuthProfileSource::Declarative,
        };
        let method = AuthMethodContribution::Interactive {
            method_id: "subscription".into(),
            display_name: "Subscription".into(),
            operation: "enroll".into(),
            supports_revocation: false,
            credentials: ["access", "refresh", "optional"]
                .into_iter()
                .map(|id| AuthCredentialStorage {
                    credential_id: id.into(),
                    storage_key: id.to_uppercase(),
                })
                .collect(),
        };
        (resolved, method)
    }

    #[test]
    fn service_identity_reader_enforces_ownership_bounds_and_redaction() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("identity");
        std::fs::write(&path, format!("{IDENTITY}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_custody_identity(&path).unwrap().as_str(), IDENTITY);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = read_custody_identity(&path).unwrap_err();
        assert!(!format!("{error:?}").contains(IDENTITY));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = root.path().join("identity-link");
        symlink(&path, &link).unwrap();
        assert!(read_custody_identity(&link).is_err());
        assert!(read_custody_identity(root.path()).is_err());
        assert!(read_custody_identity(Path::new("relative-identity")).is_err());
        std::fs::write(&path, format!("{IDENTITY}\n{IDENTITY}")).unwrap();
        assert!(read_custody_identity(&path).is_err());
        std::fs::write(&path, "x".repeat(4097)).unwrap();
        assert!(read_custody_identity(&path).is_err());
    }

    #[test]
    fn plain_age_initialization_and_enrollment_commit_before_readiness() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("custody");
        let (resolved, method) = selection();
        let retained = RetainedAuthRequestCustody::initialize_enrollment(
            &path,
            resolved,
            "test-provider",
            "test-plugin",
            method,
            RECIPIENT,
            vec![Zeroizing::new(IDENTITY.into())],
            &Keys,
        )
        .unwrap();
        assert_eq!(
            retained.enrollment_readiness().unwrap(),
            CustodyEnrollmentReadiness::Empty
        );
        let mut flow = retained
            .begin_enrollment(CustodyEnrollmentMode::Initialize)
            .unwrap();
        assert!(
            retained
                .begin_enrollment(CustodyEnrollmentMode::Initialize)
                .is_err()
        );
        assert_eq!(
            retained.enrollment_readiness().unwrap(),
            CustodyEnrollmentReadiness::ReconnectRequired
        );
        flow.next_request().unwrap();
        flow.accept(AuthFlowResponse {
            schema_version: bcode_provider_auth_models::AUTH_FLOW_SCHEMA_VERSION,
            status: AuthFlowStatus::Succeeded,
            state: None,
            effects: Vec::new(),
            diagnostics: Vec::new(),
            credentials: BTreeMap::from([
                ("access".into(), "test-access".into()),
                ("refresh".into(), "test-refresh".into()),
            ]),
        })
        .unwrap();
        retained.complete_enrollment(flow).unwrap();
        assert_eq!(
            retained.enrollment_readiness().unwrap(),
            CustodyEnrollmentReadiness::Ready
        );
        drop(retained);
        let (resolved, method) = selection();
        let reopened = RetainedAuthRequestCustody::new(
            CredentialCustodyStorage::open(&path).unwrap(),
            resolved,
            "test-provider",
            "test-plugin",
            method,
            vec![Zeroizing::new(IDENTITY.into())],
            None,
        )
        .unwrap();
        assert_eq!(
            reopened.enrollment_readiness().unwrap(),
            CustodyEnrollmentReadiness::Ready
        );
        assert!(
            reopened
                .begin_enrollment(CustodyEnrollmentMode::Initialize)
                .is_err()
        );
    }

    #[test]
    fn failed_plain_age_initialization_preserves_existing_destination() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("custody");
        std::fs::create_dir(&path).unwrap();
        let marker = path.join("operator-state");
        std::fs::write(&marker, b"preserve-existing-state").unwrap();
        let (resolved, method) = selection();
        assert!(
            RetainedAuthRequestCustody::initialize_enrollment(
                &path,
                resolved,
                "test-provider",
                "test-plugin",
                method,
                RECIPIENT,
                vec![Zeroizing::new(IDENTITY.into())],
                &Keys,
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&marker).unwrap(), b"preserve-existing-state");
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 1);
        assert!(CredentialCustodyStorage::open(&path).is_err());
    }

    #[test]
    fn unresolved_interactive_invocation_cannot_be_reissued_and_debug_is_redacted() {
        let mut flow = CustodyEnrollment {
            attempt: "synthetic-private-attempt".into(),
            fence: AuthRefreshIntent {
                version: 1,
                attempt: "synthetic-private-fence".into(),
                profile_binding: "synthetic-private-binding".into(),
            },
            flow: InteractiveEnrollment::new(
                "test-provider".into(),
                "subscription".into(),
                "account".into(),
            ),
            dispatched: false,
            succeeded: false,
        };
        assert!(flow.next_request().is_ok());
        assert!(flow.next_request().is_err());
        assert_eq!(format!("{flow:?}"), "CustodyEnrollment([REDACTED])");
        assert_eq!(format!("{flow:#?}"), "CustodyEnrollment([REDACTED])");
    }

    #[test]
    fn initialization_rejects_wrong_owner_and_implicit_factors_without_creating_directory() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("custody");
        let (mut resolved, method) = selection();
        resolved.profile.settings.clear();
        assert!(
            RetainedAuthRequestCustody::initialize_enrollment(
                &path,
                resolved,
                "test-provider",
                "test-plugin",
                method,
                RECIPIENT,
                vec![Zeroizing::new(IDENTITY.into())],
                &Keys,
            )
            .is_err()
        );
        let (resolved, method) = selection();
        assert!(
            RetainedAuthRequestCustody::initialize_enrollment(
                &path,
                resolved,
                "test-provider",
                "foreign-plugin",
                method,
                RECIPIENT,
                vec![Zeroizing::new(IDENTITY.into())],
                &Keys,
            )
            .is_err()
        );
        assert!(!path.exists());
    }
}
