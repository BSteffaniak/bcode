//! Disposable session-local authentication health, independent of global pool preference.

use bcode_model::{ProviderAuthCandidate, ProviderRequestContext};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// A failed credential revision in exactly one session/pool/profile scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAuthFailure {
    /// Session-salted digest, never credential bytes.
    pub credential_revision: String,
    /// Observation time used only for bounded retention.
    pub observed_at_unix: u64,
}

fn scope(session: &str, pool: &str, profile: &str) -> String {
    let mut hash = Sha256::new();
    for value in [session, pool, profile] {
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value.as_bytes());
    }
    hex::encode(hash.finalize())
}

fn revision(session: &str, candidate: &ProviderAuthCandidate) -> Option<String> {
    // Hash normalized credential values rather than provider-specific credential names.
    // Scope is session-local; no credential bytes or cross-session identifiers are persisted.
    let values = candidate
        .auth
        .credentials
        .iter()
        .map(|(name, credential)| (name, &credential.value))
        .collect::<std::collections::BTreeMap<_, _>>();
    if values.is_empty() && candidate.env.is_empty() {
        return None;
    }
    let encoded = if values.is_empty() {
        serde_json::to_string(&candidate.env).ok()?
    } else {
        serde_json::to_string(&values).ok()?
    };
    Some(scope(
        session,
        candidate.auth.scheme.as_deref().unwrap_or(""),
        &encoded,
    ))
}

/// Remember an authentication rejection only for this session and exact request credential.
/// Does not change configured order, cooldowns, or interactive preferences.
pub fn record_auth_failure(session: &str, context: &ProviderRequestContext) {
    let (Some(pool), Some(profile)) = (
        context.auth_pool.as_deref(),
        context.auth_profile.as_deref(),
    ) else {
        return;
    };
    let candidate = ProviderAuthCandidate {
        profile: Some(profile.into()),
        auth: context.auth.clone().unwrap_or_default(),
        env: context.env.clone(),
    };
    let Some(credential_revision) = revision(session, &candidate) else {
        return;
    };
    let key = scope(session, pool, profile);
    let observed_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    crate::auth_pool_state::mutate_state(|state| {
        record_failure(state, key, credential_revision, observed_at_unix);
    });
}

fn record_failure(
    state: &mut crate::auth_pool_state::AuthPoolState,
    key: String,
    credential_revision: String,
    observed_at_unix: u64,
) {
    // Health is a disposable optimization, not an architectural limit on sessions.
    const MAX_OBSERVATIONS: usize = 4096;
    if state.session_auth_failures.len() >= MAX_OBSERVATIONS
        && !state.session_auth_failures.contains_key(&key)
        && let Some(oldest) = state
            .session_auth_failures
            .iter()
            .min_by_key(|(_, failure)| failure.observed_at_unix)
            .map(|(key, _)| key.clone())
    {
        state.session_auth_failures.remove(&oldest);
    }
    state.session_auth_failures.insert(
        key,
        SessionAuthFailure {
            credential_revision,
            observed_at_unix,
        },
    );
}

/// Exclude failed credentials before priming, selection and provider reuse/cache planning.
///
/// New sessions and changed credentials retain normal configured routing. Explicit standalone
/// profiles are unaffected. If every candidate failed, do not silently retry a rejected credential.
///
/// # Errors
/// Returns a reconnect-required explanation when no eligible member remains in this session.
pub fn prepare_session_auth_pool(
    session: &str,
    context: &mut ProviderRequestContext,
) -> Result<(), &'static str> {
    prepare_with_state(session, context, &crate::auth_pool_state::load_state())
}

fn prepare_with_state(
    session: &str,
    context: &mut ProviderRequestContext,
    state: &crate::auth_pool_state::AuthPoolState,
) -> Result<(), &'static str> {
    let Some(pool) = context.auth_pool.as_deref() else {
        return Ok(());
    };
    if context.auth_candidates.is_empty() {
        return Ok(());
    }
    let eligible: Vec<_> = context
        .auth_candidates
        .iter()
        .filter(|candidate| {
            let Some(profile) = candidate.profile.as_deref() else {
                return true;
            };
            let Some(revision) = revision(session, candidate) else {
                return true;
            };
            state
                .session_auth_failures
                .get(&scope(session, pool, profile))
                .is_none_or(|failure| failure.credential_revision != revision)
        })
        .cloned()
        .collect();
    if eligible.is_empty() {
        return Err(
            "All auth-pool profiles failed authentication in this session; reconnect a profile or start a new session to retry.",
        );
    }
    if context.auth_profile.is_some()
        && !eligible
            .iter()
            .any(|candidate| candidate.profile == context.auth_profile)
    {
        let selected = &eligible[0];
        context.auth_profile.clone_from(&selected.profile);
        context.auth = Some(selected.auth.clone());
        context.env.clone_from(&selected.env);
        context.auth_pool_selection_reason = None;
    }
    context.auth_candidates = eligible;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_pool_state::AuthPoolState;

    fn context() -> ProviderRequestContext {
        let candidates: Vec<_> = [
            ("primary", "synthetic-revoked"),
            ("secondary", "synthetic-healthy"),
        ]
        .into_iter()
        .map(|(profile, value)| ProviderAuthCandidate {
            profile: Some(profile.into()),
            auth: bcode_model::ProviderAuthContext {
                profile: Some(profile.into()),
                scheme: Some("chatgpt".into()),
                credentials: std::collections::BTreeMap::from([(
                    "access_token".into(),
                    bcode_model::ProviderAuthCredential {
                        value: value.into(),
                        source: None,
                    },
                )]),
                ..Default::default()
            },
            ..Default::default()
        })
        .collect();
        ProviderRequestContext {
            auth_pool: Some("pool".into()),
            auth_profile: Some("primary".into()),
            auth: Some(candidates[0].auth.clone()),
            auth_candidates: candidates,
            auth_pool_routing: bcode_model::ProviderAuthPoolRouting {
                priming_enabled: true,
                priming_include_primary: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn fail(
        state: &mut AuthPoolState,
        session: &str,
        context: &ProviderRequestContext,
        index: usize,
    ) {
        let candidate = &context.auth_candidates[index];
        record_failure(
            state,
            scope(
                session,
                context.auth_pool.as_deref().unwrap(),
                candidate.profile.as_deref().unwrap(),
            ),
            revision(session, candidate).unwrap(),
            100,
        );
    }

    #[test]
    fn session_failure_skips_priming_and_preserves_configured_order_for_other_sessions() {
        let original = context();
        let mut state = AuthPoolState::default();
        fail(&mut state, "session-a", &original, 0);
        // Round-trip proves this remains session-scoped across a disposable daemon restart.
        let state: AuthPoolState =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        let mut next = original.clone();
        prepare_with_state("session-a", &mut next, &state).unwrap();
        assert_eq!(next.auth_profile.as_deref(), Some("secondary"));
        assert_eq!(next.auth, Some(original.auth_candidates[1].auth.clone()));
        assert_eq!(next.auth_candidates.len(), 1);
        let selected = crate::auth_pool_routing::select_auth_pool_candidate_with_state(
            &crate::auth_pool_routing::AuthPoolSelectionInput {
                pool: next.auth_pool.as_deref(),
                primary_profile: Some("primary"),
                routing: &next.auth_pool_routing,
                candidates: &next.auth_candidates,
            },
            &state,
            101,
        )
        .unwrap();
        assert_eq!(selected.profile.as_deref(), Some("secondary"));
        let mut fresh_session = original.clone();
        prepare_with_state("session-b", &mut fresh_session, &state).unwrap();
        assert_eq!(fresh_session, original);
        assert!(state.entries.is_empty());
        assert!(state.pools.is_empty());
        let encoded = serde_json::to_string(&state).unwrap();
        assert!(!encoded.contains("synthetic-revoked"));
        assert!(!encoded.contains("synthetic-healthy"));
    }

    #[test]
    fn replaced_credentials_recover_but_diagnostic_changes_do_not_retry_rejected_credentials() {
        let original = context();
        let mut state = AuthPoolState::default();
        fail(&mut state, "session", &original, 0);
        let mut diagnostics_changed = original.clone();
        diagnostics_changed.auth_candidates[0]
            .auth
            .diagnostics
            .push(bcode_model::ProviderAuthDiagnostic {
                severity: "info".into(),
                code: "synthetic".into(),
                message: "changed diagnostic".into(),
                remediation: None,
            });
        prepare_with_state("session", &mut diagnostics_changed, &state).unwrap();
        assert_eq!(
            diagnostics_changed.auth_profile.as_deref(),
            Some("secondary")
        );
        let mut reconnected = original.clone();
        reconnected.auth_candidates[0]
            .auth
            .credentials
            .get_mut("access_token")
            .unwrap()
            .value = "new-access".into();
        reconnected.auth = Some(reconnected.auth_candidates[0].auth.clone());
        prepare_with_state("session", &mut reconnected, &state).unwrap();
        assert_eq!(reconnected.auth_profile.as_deref(), Some("primary"));
        assert_eq!(reconnected.auth_candidates.len(), 2);
    }

    #[test]
    fn all_failed_is_actionable_and_does_not_mutate_request_or_other_pools() {
        let original = context();
        let mut state = AuthPoolState::default();
        fail(&mut state, "session", &original, 0);
        fail(&mut state, "session", &original, 1);
        let mut exhausted = original.clone();
        assert!(prepare_with_state("session", &mut exhausted, &state).is_err());
        assert_eq!(exhausted, original);
        let mut other_pool = original.clone();
        other_pool.auth_pool = Some("other".into());
        prepare_with_state("session", &mut other_pool, &state).unwrap();
        assert_eq!(other_pool.auth_candidates, original.auth_candidates);
        let mut standalone = original;
        standalone.auth_pool = None;
        prepare_with_state("session", &mut standalone, &state).unwrap();
        assert_eq!(standalone.auth_profile.as_deref(), Some("primary"));
    }
}
