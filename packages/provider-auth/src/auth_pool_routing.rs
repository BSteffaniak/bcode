//! Auth-pool routing selection.

use crate::auth_pool_state::{self, AuthPoolState};
use bcode_model::{ProviderAuthCandidate, ProviderAuthPoolRouting};
use std::collections::BTreeSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Prepare pool ordering and return best-effort usage probes needed before selection.
///
/// Callers invoke these provider-owned operations, record successful responses with
/// [`record_auth_pool_usage`], then call [`apply_auth_pool_selection`] before starting a turn.
#[must_use]
pub fn prepare_auth_pool_usage(
    context: &mut bcode_model::ProviderRequestContext,
) -> Vec<bcode_model::AuthUsageRequest> {
    if let Some(pool) = context.auth_pool.as_deref()
        && let Some(preferred) = bcode_config::load_runtime_auth_subscriptions()
            .pools
            .get(pool)
            .and_then(|entry| entry.preferred_profile.as_deref())
    {
        prefer_candidate(context, preferred);
    }
    if !context.auth_pool_routing.priming_enabled
        || !context.auth_pool_routing.priming_provider_windows
    {
        return Vec::new();
    }
    let fallback_reprime_after = context
        .auth_pool_routing
        .priming_reprime_after
        .as_deref()
        .or(context
            .auth_pool_routing
            .priming_fallback_reprime_after
            .as_deref())
        .and_then(parse_duration);
    context
        .auth_candidates
        .iter()
        .filter_map(|candidate| {
            if !auth_pool_state::profile_needs_priming_with_windows(
                context.auth_pool.as_deref(),
                candidate.profile.as_deref(),
                &context.auth_pool_routing.priming_required_windows,
                fallback_reprime_after,
            ) {
                return None;
            }
            let mut candidate_context = context.clone();
            candidate_context
                .auth_profile
                .clone_from(&candidate.profile);
            candidate_context.auth = Some(candidate.auth.clone());
            candidate_context.env.clone_from(&candidate.env);
            Some(bcode_model::AuthUsageRequest {
                provider_context: candidate_context,
                meter_ids: context
                    .auth_pool_routing
                    .priming_required_windows
                    .keys()
                    .cloned()
                    .collect(),
            })
        })
        .collect()
}

fn prefer_candidate(context: &mut bcode_model::ProviderRequestContext, preferred: &str) {
    if let Some(position) = context
        .auth_candidates
        .iter()
        .position(|candidate| candidate.profile.as_deref() == Some(preferred))
    {
        let candidate = context.auth_candidates.remove(position);
        context.auth_candidates.insert(0, candidate);
    }
}

/// Record a supported usage probe without treating unsupported discovery as an auth failure.
pub fn record_auth_pool_usage(
    request: &bcode_model::AuthUsageRequest,
    response: &bcode_model::AuthUsageResponse,
) {
    if response.supported {
        auth_pool_state::record_profile_usage_windows(
            request.provider_context.auth_pool.as_deref(),
            request.provider_context.auth_profile.as_deref(),
            &response.meters,
        );
    }
}

/// Apply current pool routing to a request, retaining all candidates for provider-owned fallback.
pub fn apply_auth_pool_selection(context: &mut bcode_model::ProviderRequestContext) {
    let state = auth_pool_state::load_state();
    apply_auth_pool_selection_with_state(context, &state, unix_now());
}

fn apply_auth_pool_selection_with_state(
    context: &mut bcode_model::ProviderRequestContext,
    state: &AuthPoolState,
    now: u64,
) {
    let Some(selection) = select_auth_pool_candidate_with_state(
        &AuthPoolSelectionInput {
            pool: context.auth_pool.as_deref(),
            primary_profile: context
                .auth_candidates
                .first()
                .and_then(|candidate| candidate.profile.as_deref()),
            routing: &context.auth_pool_routing,
            candidates: &context.auth_candidates,
        },
        state,
        now,
    ) else {
        return;
    };
    let Some(candidate) = context
        .auth_candidates
        .iter()
        .find(|candidate| candidate.profile == selection.profile)
    else {
        return;
    };
    context.auth_profile.clone_from(&candidate.profile);
    context.auth = Some(candidate.auth.clone());
    context.env.clone_from(&candidate.env);
    context.auth_pool_selection_reason = Some(
        match selection.reason {
            AuthPoolSelectionReason::Priming => "priming",
            AuthPoolSelectionReason::Strategy => "strategy",
        }
        .to_string(),
    );
}

/// Reason a candidate was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthPoolSelectionReason {
    /// Selected to satisfy priming before normal strategy routing.
    Priming,
    /// Selected by the configured routing strategy.
    Strategy,
}

/// Host-selected auth-pool route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthPoolSelection {
    /// Selected auth profile.
    pub profile: Option<String>,
    /// Selection reason.
    pub reason: AuthPoolSelectionReason,
}

/// Input for selecting an auth-pool candidate.
pub struct AuthPoolSelectionInput<'a> {
    /// Auth pool name.
    pub pool: Option<&'a str>,
    /// Primary/profile already selected by model configuration.
    pub primary_profile: Option<&'a str>,
    /// Declarative routing config.
    pub routing: &'a ProviderAuthPoolRouting,
    /// Candidate profiles in config order.
    pub candidates: &'a [ProviderAuthCandidate],
}

/// Select the next auth candidate before provider reuse planning.
#[must_use]
pub fn select_auth_pool_candidate(input: &AuthPoolSelectionInput<'_>) -> Option<AuthPoolSelection> {
    let state = auth_pool_state::load_state();
    select_auth_pool_candidate_with_state(input, &state, unix_now())
}

/// Select the next auth candidate using a provided state snapshot.
#[must_use]
pub fn select_auth_pool_candidate_with_state(
    input: &AuthPoolSelectionInput<'_>,
    state: &AuthPoolState,
    now: u64,
) -> Option<AuthPoolSelection> {
    let available = available_candidates(input, state, now);
    let (candidate, reason) = priming_candidate(input, state, &available, now)
        .map(|candidate| (candidate, AuthPoolSelectionReason::Priming))
        .or_else(|| {
            strategy_ordered_candidates(input, state, &available)
                .into_iter()
                .next()
                .map(|candidate| (candidate, AuthPoolSelectionReason::Strategy))
        })?;
    Some(AuthPoolSelection {
        profile: candidate.profile.clone(),
        reason,
    })
}

fn available_candidates<'a>(
    input: &AuthPoolSelectionInput<'a>,
    state: &AuthPoolState,
    now: u64,
) -> Vec<&'a ProviderAuthCandidate> {
    input
        .candidates
        .iter()
        .filter(|candidate| is_available(input.pool, candidate.profile.as_deref(), state, now))
        .collect()
}

fn is_available(
    pool: Option<&str>,
    profile: Option<&str>,
    state: &AuthPoolState,
    now: u64,
) -> bool {
    let Some(pool) = pool.filter(|value| !value.trim().is_empty()) else {
        return true;
    };
    let Some(profile) = profile.filter(|value| !value.trim().is_empty()) else {
        return true;
    };
    state
        .entries
        .get(&format!("{pool}/{profile}"))
        .is_none_or(|entry| entry.cooldown_until_unix <= now)
}

fn priming_candidate<'a>(
    input: &AuthPoolSelectionInput<'a>,
    state: &AuthPoolState,
    available: &[&'a ProviderAuthCandidate],
    now: u64,
) -> Option<&'a ProviderAuthCandidate> {
    available
        .iter()
        .copied()
        .find(|candidate| candidate_needs_priming(input, state, candidate, now))
}

fn candidate_needs_priming(
    input: &AuthPoolSelectionInput<'_>,
    state: &AuthPoolState,
    candidate: &ProviderAuthCandidate,
    now: u64,
) -> bool {
    if !input.routing.priming_enabled {
        return false;
    }
    if !input.routing.priming_include_primary
        && candidate.profile.as_deref() == input.primary_profile
    {
        return false;
    }
    let reprime_after = input
        .routing
        .priming_reprime_after
        .as_deref()
        .or(input.routing.priming_fallback_reprime_after.as_deref())
        .and_then(parse_duration);
    let Some(pool) = input.pool else {
        return false;
    };
    let Some(profile) = candidate.profile.as_deref() else {
        return false;
    };
    if input.routing.priming_provider_windows {
        return auth_pool_state::profile_needs_priming_with_windows_in_state(
            state,
            &format!("{pool}/{profile}"),
            &input.routing.priming_required_windows,
            reprime_after,
            now,
        );
    }
    auth_pool_state::profile_needs_priming_in_state(
        state,
        &format!("{pool}/{profile}"),
        reprime_after,
        now,
    )
}

fn strategy_ordered_candidates<'a>(
    input: &AuthPoolSelectionInput<'a>,
    state: &AuthPoolState,
    available: &[&'a ProviderAuthCandidate],
) -> Vec<&'a ProviderAuthCandidate> {
    match input.routing.strategy.as_deref() {
        Some("round_robin") => round_robin_ordered_candidates(input, state, available),
        _ => available.to_vec(),
    }
}

fn round_robin_ordered_candidates<'a>(
    input: &AuthPoolSelectionInput<'a>,
    state: &AuthPoolState,
    available: &[&'a ProviderAuthCandidate],
) -> Vec<&'a ProviderAuthCandidate> {
    if available.len() < 2 {
        return available.to_vec();
    }
    let Some(pool) = input.pool else {
        return available.to_vec();
    };
    let Some(last_selected) = state
        .pools
        .get(pool)
        .and_then(|pool| pool.last_selected_profile.as_deref())
    else {
        return available.to_vec();
    };
    let Some(position) = available
        .iter()
        .position(|candidate| candidate.profile.as_deref() == Some(last_selected))
    else {
        return available.to_vec();
    };
    available[position.saturating_add(1)..]
        .iter()
        .chain(&available[..=position])
        .copied()
        .collect()
}

/// Return all non-selected candidate profiles in their fallback order.
#[must_use]
pub fn remaining_candidate_profiles(
    selected_profile: Option<&str>,
    candidates: &[ProviderAuthCandidate],
) -> BTreeSet<String> {
    candidates
        .iter()
        .filter_map(|candidate| candidate.profile.as_ref())
        .filter(|profile| Some(profile.as_str()) != selected_profile)
        .cloned()
        .collect()
}

fn parse_duration(value: &str) -> Option<Duration> {
    let trimmed = value.trim();
    let (number, multiplier) = trimmed
        .strip_suffix('d')
        .map_or_else(|| (trimmed, 1), |number| (number, 86_400));
    let (number, multiplier) = number
        .strip_suffix('h')
        .map_or((number, multiplier), |number| (number, 3_600));
    let (number, multiplier) = number
        .strip_suffix('m')
        .map_or((number, multiplier), |number| (number, 60));
    let (number, multiplier) = number
        .strip_suffix('s')
        .map_or((number, multiplier), |number| (number, 1));
    number
        .parse::<u64>()
        .ok()
        .map(|seconds| Duration::from_secs(seconds.saturating_mul(multiplier)))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_pool_state::{
        AuthPoolProfileState, AuthPoolRoutingState, AuthPoolUsageWindowState,
    };

    use std::collections::BTreeMap;

    fn candidate(profile: &str) -> ProviderAuthCandidate {
        ProviderAuthCandidate {
            profile: Some(profile.to_string()),
            ..ProviderAuthCandidate::default()
        }
    }

    fn input<'a>(
        routing: &'a ProviderAuthPoolRouting,
        candidates: &'a [ProviderAuthCandidate],
    ) -> AuthPoolSelectionInput<'a> {
        AuthPoolSelectionInput {
            pool: Some("openai"),
            primary_profile: Some("openai"),
            routing,
            candidates,
        }
    }

    #[test]
    fn preparation_selects_preferred_material_and_preserves_fallbacks() {
        let mut preferred = candidate("preferred");
        preferred
            .env
            .insert("credential-source".into(), "preferred".into());
        let mut context = bcode_model::ProviderRequestContext {
            auth_profile: Some("configured".into()),
            auth_pool: Some("pool".into()),
            auth_candidates: vec![candidate("configured"), preferred.clone()],
            ..Default::default()
        };
        prefer_candidate(&mut context, "preferred");
        let candidates = context.auth_candidates.clone();
        apply_auth_pool_selection_with_state(&mut context, &AuthPoolState::default(), 100);
        assert_eq!(context.auth_profile, preferred.profile);
        assert_eq!(context.auth, Some(preferred.auth));
        assert_eq!(context.env, preferred.env);
        assert_eq!(
            context.auth_pool_selection_reason.as_deref(),
            Some("strategy")
        );
        assert_eq!(context.auth_candidates, candidates);
    }

    #[test]
    fn preparation_without_candidates_preserves_explicit_auth() {
        let mut context = bcode_model::ProviderRequestContext {
            auth_profile: Some("explicit".into()),
            env: BTreeMap::from([("credential-source".into(), "explicit".into())]),
            ..Default::default()
        };
        let original = context.clone();
        apply_auth_pool_selection_with_state(&mut context, &AuthPoolState::default(), 100);
        assert_eq!(context, original);
    }

    #[test]
    fn round_robin_selects_profile_after_last_selected() {
        let candidates = vec![
            candidate("openai"),
            candidate("openai-2"),
            candidate("openai-3"),
        ];
        let routing = ProviderAuthPoolRouting {
            strategy: Some("round_robin".to_string()),
            ..ProviderAuthPoolRouting::default()
        };
        let state = AuthPoolState {
            pools: BTreeMap::from([(
                "openai".to_string(),
                AuthPoolRoutingState {
                    last_selected_profile: Some("openai-2".to_string()),
                },
            )]),
            ..AuthPoolState::default()
        };

        let selection =
            select_auth_pool_candidate_with_state(&input(&routing, &candidates), &state, 100)
                .expect("candidate should select");

        assert_eq!(selection.profile.as_deref(), Some("openai-3"));
        assert_eq!(selection.reason, AuthPoolSelectionReason::Strategy);
    }

    #[test]
    fn priming_preempts_failover_strategy_for_unprimed_secondary() {
        let candidates = vec![candidate("openai"), candidate("openai-2")];
        let routing = ProviderAuthPoolRouting {
            strategy: Some("failover".to_string()),
            priming_enabled: true,
            ..ProviderAuthPoolRouting::default()
        };

        let selection = select_auth_pool_candidate_with_state(
            &input(&routing, &candidates),
            &AuthPoolState::default(),
            100,
        )
        .expect("candidate should select");

        assert_eq!(selection.profile.as_deref(), Some("openai-2"));
        assert_eq!(selection.reason, AuthPoolSelectionReason::Priming);
    }

    #[test]
    fn failover_returns_to_primary_after_secondary_is_primed() {
        let candidates = vec![candidate("openai"), candidate("openai-2")];
        let routing = ProviderAuthPoolRouting {
            strategy: Some("failover".to_string()),
            priming_enabled: true,
            ..ProviderAuthPoolRouting::default()
        };
        let state = AuthPoolState {
            entries: BTreeMap::from([(
                "openai/openai-2".to_string(),
                AuthPoolProfileState {
                    primed_unix: Some(50),
                    ..AuthPoolProfileState::default()
                },
            )]),
            ..AuthPoolState::default()
        };

        let selection =
            select_auth_pool_candidate_with_state(&input(&routing, &candidates), &state, 100)
                .expect("candidate should select");

        assert_eq!(selection.profile.as_deref(), Some("openai"));
        assert_eq!(selection.reason, AuthPoolSelectionReason::Strategy);
    }

    #[test]
    fn priming_include_primary_allows_primary_selection() {
        let candidates = vec![candidate("openai"), candidate("openai-2")];
        let routing = ProviderAuthPoolRouting {
            priming_enabled: true,
            priming_include_primary: true,
            ..ProviderAuthPoolRouting::default()
        };

        let selection = select_auth_pool_candidate_with_state(
            &input(&routing, &candidates),
            &AuthPoolState::default(),
            100,
        )
        .expect("candidate should select");

        assert_eq!(selection.profile.as_deref(), Some("openai"));
        assert_eq!(selection.reason, AuthPoolSelectionReason::Priming);
    }

    #[test]
    fn priming_marks_strategy_selected_candidate_when_required_windows_are_inactive() {
        let candidates = vec![candidate("openai"), candidate("openai-2")];
        let routing = ProviderAuthPoolRouting {
            strategy: Some("round_robin".to_string()),
            priming_enabled: true,
            priming_provider_windows: true,
            priming_required_windows: BTreeMap::from([(
                "codex".to_string(),
                vec!["primary".to_string(), "secondary".to_string()],
            )]),
            ..ProviderAuthPoolRouting::default()
        };
        let state = AuthPoolState {
            entries: BTreeMap::from([(
                "openai/openai-2".to_string(),
                AuthPoolProfileState {
                    usage_windows: BTreeMap::from([(
                        "codex".to_string(),
                        BTreeMap::from([
                            (
                                "primary".to_string(),
                                AuthPoolUsageWindowState {
                                    meter_id: "codex".to_string(),
                                    window_id: "primary".to_string(),
                                    resets_at_unix: Some(200),
                                    used_percent: Some(1),
                                    ..AuthPoolUsageWindowState::default()
                                },
                            ),
                            (
                                "secondary".to_string(),
                                AuthPoolUsageWindowState {
                                    meter_id: "codex".to_string(),
                                    window_id: "secondary".to_string(),
                                    resets_at_unix: Some(200),
                                    used_percent: Some(0),
                                    ..AuthPoolUsageWindowState::default()
                                },
                            ),
                        ]),
                    )]),
                    ..AuthPoolProfileState::default()
                },
            )]),
            pools: BTreeMap::from([(
                "openai".to_string(),
                AuthPoolRoutingState {
                    last_selected_profile: Some("openai".to_string()),
                },
            )]),
        };

        let selection =
            select_auth_pool_candidate_with_state(&input(&routing, &candidates), &state, 100)
                .expect("candidate should select");

        assert_eq!(selection.profile.as_deref(), Some("openai-2"));
        assert_eq!(selection.reason, AuthPoolSelectionReason::Priming);
    }

    #[test]
    fn reprime_after_marks_old_priming_stale() {
        let candidates = vec![candidate("openai"), candidate("openai-2")];
        let routing = ProviderAuthPoolRouting {
            strategy: Some("round_robin".to_string()),
            priming_enabled: true,
            priming_reprime_after: Some("10s".to_string()),
            ..ProviderAuthPoolRouting::default()
        };
        let state = AuthPoolState {
            entries: BTreeMap::from([(
                "openai/openai-2".to_string(),
                AuthPoolProfileState {
                    primed_unix: Some(80),
                    ..AuthPoolProfileState::default()
                },
            )]),
            pools: BTreeMap::from([(
                "openai".to_string(),
                AuthPoolRoutingState {
                    last_selected_profile: Some("openai".to_string()),
                },
            )]),
        };

        let selection =
            select_auth_pool_candidate_with_state(&input(&routing, &candidates), &state, 100)
                .expect("candidate should select");

        assert_eq!(selection.profile.as_deref(), Some("openai-2"));
        assert_eq!(selection.reason, AuthPoolSelectionReason::Priming);
    }
}
