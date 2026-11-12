//! Explicit in-place reuse of provider-declared static external credentials.
//!
//! No vault, native process, provider request, refresh, or import is performed. The caller
//! supplies a fresh enabled-plugin snapshot from its authenticated registry on every operation.
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

pub use bcode_provider_auth_models::external::*;
use bcode_provider_auth_models::{
    AuthCredentialSource, AuthMethodContribution, AuthProviderContribution, AuthSecretField,
};
use sha2::{Digest as _, Sha256};

#[cfg(test)]
mod tests;

/// Enabled declarations supplied by the authenticated application plugin registry.
pub type EnabledDeclarations<'a> = &'a [(&'a str, &'a AuthProviderContribution)];
const MAX_STATE_BYTES: u64 = 1_048_576;

struct Binding<'a> {
    candidate: ExternalAuthCandidate,
    field: &'a AuthSecretField,
    index: usize,
}

fn bindings<'a>(
    home: &Path,
    enabled: EnabledDeclarations<'a>,
) -> Result<Vec<Binding<'a>>, ExternalAuthError> {
    let mut providers = std::collections::BTreeSet::new();
    let mut result = Vec::new();
    for (plugin, provider) in enabled {
        if plugin.is_empty()
            || provider.validate().is_err()
            || !providers.insert(&provider.provider_id)
        {
            return Err(ExternalAuthError::DeclarationChanged);
        }
        for method in &provider.methods {
            let AuthMethodContribution::SecretFields { fields, .. } = method else {
                continue;
            };
            for field in fields {
                // A single static field must be sufficient; never guess missing enrollment data.
                if fields
                    .iter()
                    .any(|other| !other.optional && other.credential_id != field.credential_id)
                {
                    continue;
                }
                for (index, source) in field.discovery_sources.iter().enumerate() {
                    let (AuthCredentialSource::JsonFile {
                        application,
                        relative_path,
                        ..
                    }
                    | AuthCredentialSource::JsonFileValidated {
                        application,
                        relative_path,
                        ..
                    }) = source
                    else {
                        continue;
                    };
                    let kind = ExternalAuthSourceKind::Declared {
                        application: application.clone(),
                    };
                    let target = ExternalAuthTarget {
                        plugin_id: (*plugin).to_owned(),
                        provider_id: provider.provider_id.clone(),
                        method_id: method.method_id().to_owned(),
                        credential_id: field.credential_id.clone(),
                    };
                    // No indices are persisted. Pin full declaration, exact source and home boundary.
                    let bytes = serde_json::to_vec(&(home, &target, provider, source))
                        .map_err(|_| ExternalAuthError::DeclarationChanged)?;
                    let binding_id = hex::encode(Sha256::digest(bytes));
                    let available = crate::discovery::open_confined(home, relative_path)
                        .ok()
                        .flatten()
                        .is_some_and(|file| {
                            file.metadata()
                                .is_ok_and(|meta| meta.is_file() && meta.len() <= MAX_STATE_BYTES)
                        });
                    result.push(Binding {
                        candidate: ExternalAuthCandidate {
                            source: kind,
                            binding_id,
                            target,
                            label: application.clone(),
                            available,
                        },
                        field,
                        index,
                    });
                }
            }
        }
    }
    Ok(result)
}

/// Discover source metadata without reading any credential bytes or environment values.
///
/// # Errors
/// Rejects invalid or duplicate enabled provider declarations.
pub fn discover(
    home: &Path,
    enabled: EnabledDeclarations<'_>,
) -> Result<Vec<ExternalAuthCandidate>, ExternalAuthError> {
    Ok(bindings(home, enabled)?
        .into_iter()
        .map(|binding| binding.candidate)
        .collect())
}

/// Resolve a single user-approved use without remembering consent.
/// The caller must obtain explicit consent for this exact candidate before calling.
/// No durable approval or credential is written.
///
/// # Errors
/// Rejects changed declarations or unsafe/invalid source material; never falls back.
pub fn resolve_once(
    home: &Path,
    enabled: EnabledDeclarations<'_>,
    candidate: &ExternalAuthCandidate,
) -> Result<bcode_model::ProviderRequestContext, ExternalAuthError> {
    materialize(home, &find_binding(home, enabled, candidate)?)
}

/// Durable non-secret approvals. All updates use an OS lock and atomic publication.
#[derive(Debug, Clone)]
pub struct ExternalAuthStore {
    path: PathBuf,
}

impl ExternalAuthStore {
    /// Select an explicit Bcode state location; never consult ambient credential state.
    #[must_use]
    pub fn new(state_root: &Path) -> Self {
        Self {
            path: state_root.join("auth/external-auth.json"),
        }
    }

    fn read(&self) -> Result<ExternalAuthState, ExternalAuthError> {
        let file = match bcode_confined_fs::open_file_path(
            &self.path,
            bcode_confined_fs::OpenOptions::new().read(true),
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ExternalAuthState::default());
            }
            Err(_) => return Err(ExternalAuthError::StateUnavailable),
        };
        let mut bytes = Vec::new();
        file.take(MAX_STATE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ExternalAuthError::StateUnavailable)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(ExternalAuthError::UnsupportedState);
        }
        let state: ExternalAuthState =
            serde_json::from_slice(&bytes).map_err(|_| ExternalAuthError::UnsupportedState)?;
        if state.schema_version != EXTERNAL_AUTH_SCHEMA_VERSION
            || state.grants.len() > 1024
            || state.grants.iter().any(|grant| !valid_grant(grant))
        {
            return Err(ExternalAuthError::UnsupportedState);
        }
        Ok(state)
    }

    fn lock(&self) -> Result<std::fs::File, ExternalAuthError> {
        let parent = self
            .path
            .parent()
            .ok_or(ExternalAuthError::StateUnavailable)?;
        match bcode_confined_fs::create_private_dir(parent) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                bcode_confined_fs::Dir::open(parent)
                    .map_err(|_| ExternalAuthError::StateUnavailable)?;
            }
            Err(_) => return Err(ExternalAuthError::StateUnavailable),
        }
        let file = bcode_confined_fs::open_file_path(
            &self.path.with_extension("lock"),
            bcode_confined_fs::OpenOptions::new()
                .read(true)
                .write(true)
                .private(true)
                .creation(bcode_confined_fs::Creation::IfMissing),
        )
        .map_err(|_| ExternalAuthError::StateUnavailable)?;
        file.try_lock()
            .map_err(|_| ExternalAuthError::StateUnavailable)?;
        Ok(file)
    }

    fn write(&self, state: &ExternalAuthState) -> Result<(), ExternalAuthError> {
        let parent = self
            .path
            .parent()
            .ok_or(ExternalAuthError::StateUnavailable)?;
        let bytes = serde_json::to_vec(state).map_err(|_| ExternalAuthError::StateUnavailable)?;
        if bytes.len() as u64 > MAX_STATE_BYTES || state.grants.len() > 1024 {
            return Err(ExternalAuthError::StateUnavailable);
        }
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .map_err(|_| ExternalAuthError::StateUnavailable)?;
        temporary
            .write_all(&bytes)
            .map_err(|_| ExternalAuthError::StateUnavailable)?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|_| ExternalAuthError::StateUnavailable)?;
        bcode_confined_fs::persist(temporary, &self.path)
            .map_err(|_| ExternalAuthError::StateUnavailable)?;
        bcode_confined_fs::sync_directory_path(parent)
            .map_err(|_| ExternalAuthError::StateUnavailable)
    }

    /// List explicit approvals without reading any source. Stale entries remain visible for revocation.
    ///
    /// # Errors
    /// Fails closed on unsupported, damaged, or unreadable state.
    pub fn list(&self) -> Result<Vec<ExternalAuthGrant>, ExternalAuthError> {
        Ok(self.read()?.grants)
    }

    /// Persist an explicit first-use approval. Call only after user confirmation of candidate and scope.
    /// `None` scope means all local Bcode applications. No credential is read by approval.
    ///
    /// # Errors
    /// Rejects stale candidates, invalid scope, unavailable locking or unsupported state.
    pub fn approve(
        &self,
        home: &Path,
        enabled: EnabledDeclarations<'_>,
        candidate: &ExternalAuthCandidate,
        scope: Option<ExternalAuthScope>,
        approved_at_unix: u64,
    ) -> Result<ExternalAuthGrant, ExternalAuthError> {
        let binding = find_binding(home, enabled, candidate)?;
        let grant = ExternalAuthGrant {
            source: binding.candidate.source,
            binding_id: binding.candidate.binding_id,
            target: binding.candidate.target,
            scope: scope.unwrap_or(ExternalAuthScope::AllLocalApplications),
            approved_at_unix,
        };
        if !valid_grant(&grant) {
            return Err(ExternalAuthError::InvalidRequest);
        }
        let _lock = self.lock()?;
        let mut state = self.read()?;
        state.grants.retain(|existing| {
            existing.binding_id != grant.binding_id || existing.scope != grant.scope
        });
        state.grants.push(grant.clone());
        self.write(&state)?;
        Ok(grant)
    }

    /// Revoke an exact approval, including stale declarations, without reading any source.
    ///
    /// # Errors
    /// Fails closed on unavailable ownership or invalid durable state.
    pub fn revoke(&self, grant: &ExternalAuthGrant) -> Result<bool, ExternalAuthError> {
        let _lock = self.lock()?;
        let mut state = self.read()?;
        let previous = state.grants.len();
        state.grants.retain(|entry| entry != grant);
        let removed = previous != state.grants.len();
        if removed {
            self.write(&state)?;
        }
        Ok(removed)
    }

    /// Resolve one explicitly approved source in place, with fresh enabled declarations.
    /// No fallback, refresh, native launch, environment import or vault access is attempted.
    /// The consent lock is held through acquisition, serializing revocation with reads.
    /// `application_id` must be verified by the caller. `None` authorizes broad grants
    /// only; an application-scoped grant never authorizes an unidentified caller.
    ///
    /// # Errors
    /// Rejects missing consent, scope mismatch, declaration drift, or unsafe/invalid source data.
    pub fn resolve(
        &self,
        home: &Path,
        enabled: EnabledDeclarations<'_>,
        candidate: &ExternalAuthCandidate,
        application_id: Option<&str>,
    ) -> Result<bcode_model::ProviderRequestContext, ExternalAuthError> {
        if application_id.is_some_and(|identity| !valid_id(identity)) {
            return Err(ExternalAuthError::InvalidRequest);
        }
        // Read existing consent before any source access, including discovery metadata.
        let _lock = self.lock()?;
        let state = self.read()?;
        let grants = state
            .grants
            .iter()
            .filter(|grant| {
                grant.binding_id == candidate.binding_id
                    && grant.target == candidate.target
                    && grant.source == candidate.source
            })
            .collect::<Vec<_>>();
        if grants.is_empty() {
            return Err(ExternalAuthError::NotApproved);
        }
        if !grants.iter().any(|grant| match &grant.scope {
            ExternalAuthScope::AllLocalApplications => true,
            ExternalAuthScope::Application {
                application_id: approved,
            } => Some(approved.as_str()) == application_id,
        }) {
            return Err(ExternalAuthError::ScopeDenied);
        }
        let binding = find_binding(home, enabled, candidate)?;
        materialize(home, &binding)
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}
fn valid_grant(grant: &ExternalAuthGrant) -> bool {
    grant.binding_id.len() == 64
        && grant
            .binding_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        && [
            &grant.target.plugin_id,
            &grant.target.provider_id,
            &grant.target.method_id,
            &grant.target.credential_id,
        ]
        .into_iter()
        .all(|id| valid_id(id))
        && match &grant.scope {
            ExternalAuthScope::AllLocalApplications => true,
            ExternalAuthScope::Application { application_id } => valid_id(application_id),
        }
}
fn find_binding<'a>(
    home: &Path,
    enabled: EnabledDeclarations<'a>,
    candidate: &ExternalAuthCandidate,
) -> Result<Binding<'a>, ExternalAuthError> {
    bindings(home, enabled)?
        .into_iter()
        .find(|binding| {
            binding.candidate.binding_id == candidate.binding_id
                && binding.candidate.target == candidate.target
                && binding.candidate.source == candidate.source
        })
        .ok_or(ExternalAuthError::DeclarationChanged)
}
fn materialize(
    home: &Path,
    binding: &Binding<'_>,
) -> Result<bcode_model::ProviderRequestContext, ExternalAuthError> {
    let value = crate::discovery::read_selected(home, binding.field, binding.index)
        .map_err(|_| ExternalAuthError::SourceUnavailable)?
        .ok_or(ExternalAuthError::SourceUnavailable)?;
    if value.trim().is_empty() {
        return Err(ExternalAuthError::SourceUnavailable);
    }
    let source = format!("external:{}", binding.candidate.source.as_str());
    Ok(bcode_model::ProviderRequestContext {
        auth: Some(bcode_model::ProviderAuthContext {
            backend: Some("external".to_owned()),
            scheme: Some(binding.candidate.target.method_id.clone()),
            credentials: std::collections::BTreeMap::from([(
                binding.candidate.target.credential_id.clone(),
                bcode_model::ProviderAuthCredential {
                    value: value.to_string(),
                    source: Some(source),
                },
            )]),
            ..Default::default()
        }),
        ..Default::default()
    })
}
