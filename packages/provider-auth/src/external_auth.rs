//! Explicit, in-place reuse of credential documents owned by other local applications.
//!
//! Providers declare [`AuthExternalSource`]s. Discovery inspects document presence only.
//! Approval and dispatch read the document after consent, deliver its values to the owning
//! provider for that request, and never copy, refresh, rewrite, or import it. The caller
//! supplies a fresh enabled-plugin snapshot from its authenticated registry on every call.
use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use bcode_provider_auth_models::AuthProviderContribution;
pub use bcode_provider_auth_models::external::*;
use sha2::{Digest as _, Sha256};
use zeroize::Zeroizing;

#[cfg(test)]
mod tests;

/// Enabled declarations supplied by the authenticated application plugin registry.
pub type EnabledDeclarations<'a> = &'a [(&'a str, &'a AuthProviderContribution)];

const MAX_STATE_BYTES: u64 = 1_048_576;
const MAX_GRANTS: usize = 1024;

/// Directories external documents resolve against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRoots {
    /// The user's home directory.
    pub home: PathBuf,
    /// Declared root-override environment variables that hold absolute directories.
    pub overrides: BTreeMap<String, PathBuf>,
}

impl SourceRoots {
    /// Capture the current process's home and the declared root-override variables.
    #[must_use]
    pub fn from_process(home: PathBuf, enabled: EnabledDeclarations<'_>) -> Self {
        let overrides = enabled
            .iter()
            .flat_map(|(_, provider)| &provider.external_sources)
            .filter_map(|source| source.location.root_override.as_ref())
            .filter_map(|root| {
                let value = PathBuf::from(std::env::var_os(&root.env)?);
                value.is_absolute().then(|| (root.env.clone(), value))
            })
            .collect();
        Self { home, overrides }
    }

    fn document(&self, source: &AuthExternalSource) -> (PathBuf, String) {
        if let Some(root) = &source.location.root_override
            && let Some(directory) = self.overrides.get(&root.env)
        {
            return (directory.clone(), root.relative_path.clone());
        }
        (self.home.clone(), source.location.relative_path.clone())
    }
}

struct Binding<'a> {
    candidate: ExternalAuthCandidate,
    source: &'a AuthExternalSource,
    root: PathBuf,
    relative_path: String,
}

fn bindings<'a>(
    roots: &SourceRoots,
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
        for source in &provider.external_sources {
            let target = ExternalAuthTarget {
                plugin_id: (*plugin).to_owned(),
                provider_id: provider.provider_id.clone(),
                method_id: source.method_id.clone(),
                source_id: source.source_id.clone(),
            };
            let (root, relative_path) = roots.document(source);
            // Stable across cosmetic declaration edits and credential rotation; changes when
            // the target or the resolved document location changes.
            let bytes = serde_json::to_vec(&(&target, root.join(&relative_path)))
                .map_err(|_| ExternalAuthError::DeclarationChanged)?;
            let available = crate::discovery::open_confined(&root, &relative_path)
                .ok()
                .flatten()
                .is_some_and(|file| {
                    file.metadata().is_ok_and(|meta| {
                        meta.is_file() && meta.len() <= crate::discovery::MAX_SOURCE_BYTES
                    })
                });
            result.push(Binding {
                candidate: ExternalAuthCandidate {
                    binding_id: hex::encode(Sha256::digest(bytes)),
                    target,
                    application: source.application.clone(),
                    available,
                },
                source,
                root,
                relative_path,
            });
        }
    }
    Ok(result)
}

fn find_binding<'a>(
    roots: &SourceRoots,
    enabled: EnabledDeclarations<'a>,
    binding_id: &str,
    target: &ExternalAuthTarget,
) -> Result<Binding<'a>, ExternalAuthError> {
    bindings(roots, enabled)?
        .into_iter()
        .find(|binding| {
            binding.candidate.binding_id == binding_id && binding.candidate.target == *target
        })
        .ok_or(ExternalAuthError::DeclarationChanged)
}

/// Discover declared documents by presence, without reading their contents.
///
/// # Errors
/// Rejects invalid or duplicate enabled provider declarations.
pub fn discover(
    roots: &SourceRoots,
    enabled: EnabledDeclarations<'_>,
) -> Result<Vec<ExternalAuthCandidate>, ExternalAuthError> {
    Ok(bindings(roots, enabled)?
        .into_iter()
        .map(|binding| binding.candidate)
        .collect())
}

/// Credentials read from one document, plus its account fingerprint.
struct Acquired {
    context: bcode_model::ProviderRequestContext,
    account_fingerprint: Option<String>,
    expires_at_unix: Option<u64>,
}

impl Acquired {
    fn expired(&self, now_unix: u64) -> bool {
        self.expires_at_unix
            .is_some_and(|expiry| expiry <= now_unix)
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// Read a declared expiry; an unreadable declared expiry is an unsupported document.
fn expiry(
    document: &serde_json::Value,
    expiry: &AuthExternalExpiry,
) -> Result<u64, ExternalAuthError> {
    use base64::Engine as _;
    let value = document
        .pointer(&expiry.pointer)
        .ok_or(ExternalAuthError::SourceUnavailable)?;
    match expiry.format {
        AuthExternalExpiryFormat::UnixSeconds => value.as_u64(),
        AuthExternalExpiryFormat::JwtExp => value
            .as_str()
            .and_then(|token| token.split('.').nth(1))
            .and_then(|payload| {
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(payload.trim_end_matches('='))
                    .ok()
            })
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|claims| claims.get("exp")?.as_u64()),
    }
    .ok_or(ExternalAuthError::SourceUnavailable)
}

fn acquire(binding: &Binding<'_>) -> Result<Acquired, ExternalAuthError> {
    let unavailable = |_: std::io::Error| ExternalAuthError::SourceUnavailable;
    let file = crate::discovery::open_confined(&binding.root, &binding.relative_path)
        .map_err(|_| ExternalAuthError::SourceUnavailable)?
        .ok_or(ExternalAuthError::SourceUnavailable)?;
    let metadata = file.metadata().map_err(unavailable)?;
    if !metadata.is_file() || metadata.len() > crate::discovery::MAX_SOURCE_BYTES {
        return Err(ExternalAuthError::SourceUnavailable);
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(crate::discovery::MAX_SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(unavailable)?;
    let mut document: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| ExternalAuthError::SourceUnavailable)?;
    let result = interpret(binding, &document);
    crate::discovery::clear_json_strings(&mut document);
    result
}

fn interpret(
    binding: &Binding<'_>,
    document: &serde_json::Value,
) -> Result<Acquired, ExternalAuthError> {
    let source = binding.source;
    let present = |pointer: &str| {
        document
            .pointer(pointer)
            .is_some_and(|value| !value.is_null())
    };
    if source
        .required_absent_pointers
        .iter()
        .any(|pointer| present(pointer))
        || source
            .discriminator
            .as_ref()
            .is_some_and(|(pointer, expected)| {
                document
                    .pointer(pointer)
                    .and_then(serde_json::Value::as_str)
                    != Some(expected)
            })
    {
        return Err(ExternalAuthError::SourceUnavailable);
    }
    let mut credentials = BTreeMap::new();
    for credential in &source.credentials {
        match document
            .pointer(&credential.pointer)
            .and_then(serde_json::Value::as_str)
        {
            Some(value) if source.literal_policy.permits(value) => {
                credentials.insert(
                    credential.credential_id.clone(),
                    bcode_model::ProviderAuthCredential {
                        value: value.to_owned(),
                        source: Some(format!("external:{}", source.source_id)),
                    },
                );
            }
            None if credential.optional && !present(&credential.pointer) => {}
            _ => return Err(ExternalAuthError::SourceUnavailable),
        }
    }
    let account_fingerprint = match &source.account_pointer {
        None => None,
        Some(pointer) => {
            let account = document
                .pointer(pointer)
                .and_then(serde_json::Value::as_str)
                .filter(|account| !account.trim().is_empty())
                .ok_or(ExternalAuthError::SourceUnavailable)?;
            Some(hex::encode(&Sha256::digest(account.as_bytes())[..8]))
        }
    };
    let expires_at_unix = source
        .expiry
        .as_ref()
        .map(|declared| expiry(document, declared))
        .transpose()?;
    Ok(Acquired {
        context: bcode_model::ProviderRequestContext {
            auth: Some(bcode_model::ProviderAuthContext {
                backend: Some("external".to_owned()),
                scheme: Some(source.method_id.clone()),
                credentials,
                ..Default::default()
            }),
            ..Default::default()
        },
        account_fingerprint,
        expires_at_unix,
    })
}

/// Selection recipe carried until dispatch: routing only, never credentials.
#[must_use]
pub fn recipe(
    selection: &bcode_config::ResolvedModelSelection,
) -> bcode_model::ProviderRequestContext {
    bcode_model::ProviderRequestContext {
        model_profile: selection.model_profile.clone(),
        auth_profile: selection.auth_profile.clone(),
        auth_pool: selection.auth_pool.clone(),
        settings: selection.settings.clone(),
        request: selection.request.clone(),
        external_auth_binding: selection.external_auth_binding.clone(),
        ..Default::default()
    }
}

/// Inputs for acquiring an external connection at the authoritative dispatch boundary.
#[derive(Clone, Copy)]
pub struct DispatchResolution<'a> {
    /// The unmaterialized recipe selected for this request.
    pub recipe: &'a bcode_model::ProviderRequestContext,
    /// Provider plugin the request dispatches to.
    pub provider_plugin_id: &'a str,
    /// Provider-native model the request dispatches to.
    pub model_id: &'a str,
    /// Document roots of the serving process.
    pub roots: &'a SourceRoots,
    /// Current authenticated enabled-plugin declarations.
    pub enabled: EnabledDeclarations<'a>,
    /// Fresh offline destinations the provider recommends for the approved method. The
    /// request must match one on its routing settings, so configuration cannot redirect an
    /// external credential to another endpoint.
    pub provider_choices: &'a [bcode_model::StartupRecommendation],
    /// Setting keys the provider declares as deciding routing; empty requires an exact match.
    pub routing_settings: &'a [String],
}

/// Whether request `settings` use the destination a provider `choice` recommends.
///
/// Every declared routing key, and every key the choice sets, must have the same value (absent
/// counts as a value). Other settings are free. Without a declaration the settings must match the
/// choice exactly.
#[must_use]
pub fn destination_matches(
    choice: &BTreeMap<String, String>,
    settings: &BTreeMap<String, String>,
    routing_settings: &[String],
) -> bool {
    if routing_settings.is_empty() {
        return choice == settings;
    }
    routing_settings
        .iter()
        .chain(choice.keys())
        .all(|key| choice.get(key) == settings.get(key))
}

/// Durable non-secret approvals, guarded by an OS file lock with atomic publication.
#[derive(Debug, Clone)]
pub struct ExternalAuthStore {
    path: PathBuf,
}

impl ExternalAuthStore {
    /// Select an explicit Bcode state location; never consult ambient credential state.
    #[must_use]
    pub fn new(state_root: &Path) -> Self {
        Self {
            path: state_root.join("auth/external-connections.json"),
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
            || state.grants.len() > MAX_GRANTS
            || state.grants.iter().any(|grant| !valid_grant(grant))
        {
            return Err(ExternalAuthError::UnsupportedState);
        }
        Ok(state)
    }

    /// Block until this process holds the consent lock. Readers share it so concurrent
    /// dispatches proceed together; approval and revocation exclude them.
    fn lock(&self, exclusive: bool) -> Result<std::fs::File, ExternalAuthError> {
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
        if exclusive {
            file.lock()
        } else {
            file.lock_shared()
        }
        .map_err(|_| ExternalAuthError::StateUnavailable)?;
        Ok(file)
    }

    fn write(&self, state: &ExternalAuthState) -> Result<(), ExternalAuthError> {
        let parent = self
            .path
            .parent()
            .ok_or(ExternalAuthError::StateUnavailable)?;
        let bytes = serde_json::to_vec(state).map_err(|_| ExternalAuthError::StateUnavailable)?;
        if bytes.len() as u64 > MAX_STATE_BYTES || state.grants.len() > MAX_GRANTS {
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

    /// List approvals without reading any source. Stale entries remain visible for revocation.
    ///
    /// # Errors
    /// Fails closed on unsupported, damaged, or unreadable state.
    pub fn list(&self) -> Result<Vec<ExternalAuthGrant>, ExternalAuthError> {
        let _lock = self.lock(false)?;
        Ok(self.read()?.grants)
    }

    /// Persist an approval. Call only after the user confirmed this exact candidate.
    ///
    /// Approval reads the document once, so an unsupported format is reported now instead
    /// of on the first request, and records the declared account fingerprint.
    ///
    /// # Errors
    /// Rejects stale candidates, unreadable or unsupported documents, and unavailable state.
    pub fn approve(
        &self,
        roots: &SourceRoots,
        enabled: EnabledDeclarations<'_>,
        candidate: &ExternalAuthCandidate,
        approved_at_unix: u64,
    ) -> Result<ExternalAuthGrant, ExternalAuthError> {
        let binding = find_binding(roots, enabled, &candidate.binding_id, &candidate.target)?;
        let acquired = acquire(&binding)?;
        let grant = ExternalAuthGrant {
            binding_id: binding.candidate.binding_id,
            target: binding.candidate.target,
            application: binding.candidate.application,
            account_fingerprint: acquired.account_fingerprint,
            approved_at_unix,
        };
        let _lock = self.lock(true)?;
        let mut state = self.read()?;
        state
            .grants
            .retain(|existing| existing.binding_id != grant.binding_id);
        state.grants.push(grant.clone());
        self.write(&state)?;
        Ok(grant)
    }

    /// Revoke an approval, including stale declarations, without reading any source.
    ///
    /// # Errors
    /// Fails closed on unavailable or damaged state.
    pub fn revoke(&self, binding_id: &str) -> Result<bool, ExternalAuthError> {
        let _lock = self.lock(true)?;
        let mut state = self.read()?;
        let previous = state.grants.len();
        state.grants.retain(|grant| grant.binding_id != binding_id);
        let removed = previous != state.grants.len();
        if removed {
            self.write(&state)?;
        }
        Ok(removed)
    }

    /// Acquire an approved connection for one request at the dispatch boundary.
    ///
    /// The shared consent lock is held through acquisition, so a concurrent revocation
    /// either completes first or waits. Rotation inside the same account is picked up; an
    /// account change or a non-recommended destination is rejected. Never falls back.
    ///
    /// # Errors
    /// Rejects conflicting profile/pool/request selections, missing approval, declaration
    /// drift, rejected destinations, account changes, and missing or invalid documents.
    pub fn resolve_dispatch(
        &self,
        request: DispatchResolution<'_>,
    ) -> Result<bcode_model::ProviderRequestContext, ExternalAuthError> {
        let recipe = request.recipe;
        let binding_id = recipe
            .external_auth_binding
            .as_deref()
            .ok_or(ExternalAuthError::InvalidRequest)?;
        if recipe.auth.is_some()
            || !recipe.auth_candidates.is_empty()
            || !recipe.env.is_empty()
            || recipe.auth_profile.is_some()
            || recipe.auth_pool.is_some()
            || !recipe.request.is_empty()
        {
            return Err(ExternalAuthError::InvalidRequest);
        }
        if !request.provider_choices.iter().any(|choice| {
            choice.provider_plugin_id == request.provider_plugin_id
                && choice.model_id == request.model_id
                && destination_matches(&choice.settings, &recipe.settings, request.routing_settings)
        }) {
            return Err(ExternalAuthError::DestinationRejected);
        }
        let _lock = self.lock(false)?;
        let grant = self
            .read()?
            .grants
            .into_iter()
            .find(|grant| grant.binding_id == binding_id)
            .ok_or(ExternalAuthError::NotApproved)?;
        if grant.target.plugin_id != request.provider_plugin_id {
            return Err(ExternalAuthError::DeclarationChanged);
        }
        let binding = find_binding(request.roots, request.enabled, binding_id, &grant.target)?;
        let acquired = acquire(&binding)?;
        if acquired.account_fingerprint != grant.account_fingerprint {
            return Err(ExternalAuthError::AccountChanged);
        }
        if acquired.expired(now_unix()) {
            return Err(ExternalAuthError::Expired);
        }
        let mut context = acquired.context;
        context.settings.clone_from(&recipe.settings);
        context.model_profile.clone_from(&recipe.model_profile);
        context.api_surface = recipe.api_surface;
        context.external_auth_binding = Some(grant.binding_id);
        Ok(context)
    }

    /// Local status of every approval, reading each approved source once.
    ///
    /// # Errors
    /// Fails only when consent state is unavailable; per-connection problems are statuses.
    pub fn status(
        &self,
        roots: &SourceRoots,
        enabled: EnabledDeclarations<'_>,
    ) -> Result<Vec<ExternalAuthConnection>, ExternalAuthError> {
        let now = now_unix();
        Ok(self
            .list()?
            .into_iter()
            .map(|grant| {
                let status = match find_binding(roots, enabled, &grant.binding_id, &grant.target)
                    .and_then(|binding| acquire(&binding))
                {
                    Err(ExternalAuthError::DeclarationChanged) => {
                        ExternalConnectionStatus::DeclarationChanged
                    }
                    Err(_) => ExternalConnectionStatus::SourceUnavailable,
                    Ok(acquired) if acquired.account_fingerprint != grant.account_fingerprint => {
                        ExternalConnectionStatus::AccountChanged
                    }
                    Ok(acquired) if acquired.expired(now) => ExternalConnectionStatus::Expired,
                    Ok(acquired) => ExternalConnectionStatus::Ready {
                        expires_at_unix: acquired.expires_at_unix,
                    },
                };
                ExternalAuthConnection {
                    grant,
                    status,
                    recommended_models: Vec::new(),
                }
            })
            .collect())
    }

    /// The approved method for a binding, used to fetch the provider's destinations.
    ///
    /// # Errors
    /// Fails when the binding is not approved or state is unavailable.
    pub fn approved_target(
        &self,
        binding_id: &str,
    ) -> Result<ExternalAuthTarget, ExternalAuthError> {
        self.list()?
            .into_iter()
            .find(|grant| grant.binding_id == binding_id)
            .map(|grant| grant.target)
            .ok_or(ExternalAuthError::NotApproved)
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
            &grant.target.source_id,
            &grant.application,
        ]
        .into_iter()
        .all(|id| valid_id(id))
}
