//! Shared auth-pool runtime state.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static STATE_LOCK: Mutex<()> = Mutex::new(());

/// Auth-pool runtime state file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthPoolState {
    /// Per-pool/profile observed state keyed as `<pool>/<profile>`.
    #[serde(default)]
    pub entries: BTreeMap<String, AuthPoolProfileState>,
    /// Per-pool routing state.
    #[serde(default)]
    pub pools: BTreeMap<String, AuthPoolRoutingState>,
}

/// Per-pool routing cursor state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthPoolRoutingState {
    /// Last successfully selected profile.
    #[serde(default)]
    pub last_selected_profile: Option<String>,
}

/// Observed per-profile auth-pool state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthPoolProfileState {
    /// Unix timestamp until which this profile should be treated as quota-limited.
    #[serde(default)]
    pub cooldown_until_unix: u64,
    /// Human-readable cooldown reason.
    #[serde(default)]
    pub reason: String,
    /// Last provider error observed for this profile.
    #[serde(default)]
    pub last_error: Option<String>,
    /// Source of reset/cooldown timing.
    #[serde(default)]
    pub reset_source: Option<String>,
    /// Last successful use timestamp.
    #[serde(default)]
    pub last_success_unix: Option<u64>,
    /// Last priming success timestamp.
    #[serde(default)]
    pub primed_unix: Option<u64>,
    /// Provider-confirmed usage windows keyed by meter id, then window id.
    #[serde(default)]
    pub usage_windows: BTreeMap<String, BTreeMap<String, AuthPoolUsageWindowState>>,
}

/// Provider-confirmed usage window state for one profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthPoolUsageWindowState {
    /// Provider meter id.
    #[serde(default)]
    pub meter_id: String,
    /// Provider window id within the meter.
    #[serde(default)]
    pub window_id: String,
    /// Provider reset timestamp in Unix seconds when known.
    #[serde(default)]
    pub resets_at_unix: Option<u64>,
    /// Provider-rounded usage percentage when known.
    #[serde(default)]
    pub used_percent: Option<u32>,
    /// Local observation timestamp in Unix seconds.
    #[serde(default)]
    pub observed_at_unix: u64,
    /// Last successful Bcode priming/use timestamp for this provider window.
    #[serde(default)]
    pub primed_at_unix: Option<u64>,
    /// Provider/source identifier for this observation.
    #[serde(default)]
    pub source: Option<String>,
}

/// Whether an auth profile is outside local cooldown.
#[must_use]
pub fn is_profile_available(pool: Option<&str>, profile: Option<&str>) -> bool {
    let Some(key) = state_key(pool, profile) else {
        return true;
    };
    with_state(|state| {
        state
            .entries
            .get(&key)
            .is_none_or(|entry| entry.cooldown_until_unix <= now_unix())
    })
}

/// Mark a profile as quota-limited for a duration.
pub fn mark_profile_quota_limited(
    pool: Option<&str>,
    profile: Option<&str>,
    reason: &str,
    message: &str,
    cooldown: Duration,
) {
    mark_profile_quota_limited_until(
        pool,
        profile,
        reason,
        message,
        now_unix().saturating_add(cooldown.as_secs()),
        None,
    );
}

/// Mark a profile as quota-limited until an absolute Unix timestamp.
pub fn mark_profile_quota_limited_until(
    pool: Option<&str>,
    profile: Option<&str>,
    reason: &str,
    message: &str,
    cooldown_until_unix: u64,
    reset_source: Option<&str>,
) {
    let Some(key) = state_key(pool, profile) else {
        return;
    };
    mutate_state(|state| {
        let entry = state.entries.entry(key).or_default();
        entry.cooldown_until_unix = cooldown_until_unix;
        entry.reason = reason.to_string();
        entry.last_error = Some(message.to_string());
        entry.reset_source = reset_source.map(ToString::to_string);
    });
}

/// Return the current cooldown deadline for a profile, when active.
#[must_use]
pub fn profile_cooldown_until(pool: Option<&str>, profile: Option<&str>) -> Option<u64> {
    let key = state_key(pool, profile)?;
    with_state(|state| {
        state
            .entries
            .get(&key)
            .map(|entry| entry.cooldown_until_unix)
            .filter(|until| *until > now_unix())
    })
}

/// Clear local quota-limited state for a profile.
pub fn clear_profile_quota_limited(pool: Option<&str>, profile: Option<&str>) {
    let Some(key) = state_key(pool, profile) else {
        return;
    };
    mutate_state(|state| {
        if let Some(entry) = state.entries.get_mut(&key) {
            entry.cooldown_until_unix = 0;
            entry.reason.clear();
            entry.last_error = None;
            entry.reset_source = None;
        }
    });
}

/// Record a successful profile use.
pub fn mark_profile_success(pool: Option<&str>, profile: Option<&str>) {
    let Some(key) = state_key(pool, profile) else {
        return;
    };
    mutate_state(|state| {
        state.entries.entry(key).or_default().last_success_unix = Some(now_unix());
    });
}

/// Record a successful priming use.
pub fn mark_profile_primed(pool: Option<&str>, profile: Option<&str>) {
    let Some(key) = state_key(pool, profile) else {
        return;
    };
    mutate_state(|state| {
        let now = now_unix();
        let entry = state.entries.entry(key).or_default();
        entry.last_success_unix = Some(now);
        entry.primed_unix = Some(now);
    });
}

/// Whether a profile needs priming according to provider usage windows or fallback interval.
#[must_use]
pub fn profile_needs_priming_with_windows(
    pool: Option<&str>,
    profile: Option<&str>,
    required_windows: &BTreeMap<String, Vec<String>>,
    fallback_reprime_after: Option<Duration>,
) -> bool {
    let Some(key) = state_key(pool, profile) else {
        return false;
    };
    with_state(|state| {
        profile_needs_priming_with_windows_in_state(
            state,
            &key,
            required_windows,
            fallback_reprime_after,
            now_unix(),
        )
    })
}

/// Whether a profile needs priming according to optional reprime interval.
#[must_use]
pub fn profile_needs_priming(
    pool: Option<&str>,
    profile: Option<&str>,
    reprime_after: Option<Duration>,
) -> bool {
    let Some(key) = state_key(pool, profile) else {
        return false;
    };
    with_state(|state| profile_needs_priming_in_state(state, &key, reprime_after, now_unix()))
}

/// Record provider-confirmed usage windows for a profile.
pub fn record_profile_usage_windows(
    pool: Option<&str>,
    profile: Option<&str>,
    meters: &[bcode_model::AuthUsageMeterSnapshot],
) {
    let Some(key) = state_key(pool, profile) else {
        return;
    };
    mutate_state(|state| {
        let entry = state.entries.entry(key).or_default();
        for meter in meters {
            let windows = entry
                .usage_windows
                .entry(meter.meter_id.clone())
                .or_default();
            for window in &meter.windows {
                windows.insert(
                    window.window_id.clone(),
                    AuthPoolUsageWindowState {
                        meter_id: meter.meter_id.clone(),
                        window_id: window.window_id.clone(),
                        resets_at_unix: window.resets_at_unix,
                        used_percent: window.used_percent,
                        observed_at_unix: window.observed_at_unix,
                        primed_at_unix: None,
                        source: window.source.clone(),
                    },
                );
            }
        }
    });
}

/// Record that a successful request should count as touching current provider usage windows.
pub fn mark_profile_usage_windows_primed(
    pool: Option<&str>,
    profile: Option<&str>,
    required_windows: &BTreeMap<String, Vec<String>>,
) {
    let Some(key) = state_key(pool, profile) else {
        return;
    };
    mutate_state(|state| {
        let now = now_unix();
        let entry = state.entries.entry(key).or_default();
        entry.last_success_unix = Some(now);
        entry.primed_unix = Some(now);
        let targets = usage_window_targets(entry, required_windows);
        for (meter_id, window_id) in targets {
            let window = entry
                .usage_windows
                .entry(meter_id.clone())
                .or_default()
                .entry(window_id.clone())
                .or_insert_with(|| AuthPoolUsageWindowState {
                    meter_id: meter_id.clone(),
                    window_id: window_id.clone(),
                    observed_at_unix: now,
                    ..AuthPoolUsageWindowState::default()
                });
            window.primed_at_unix = Some(now);
            window.observed_at_unix = window.observed_at_unix.max(now);
        }
    });
}

/// Return last selected profile for a pool.
#[must_use]
pub fn last_selected_profile(pool: Option<&str>) -> Option<String> {
    let pool = pool.filter(|value| !value.trim().is_empty())?;
    with_state(|state| {
        state
            .pools
            .get(pool)
            .and_then(|entry| entry.last_selected_profile.clone())
    })
}

/// Mark the routing cursor for a pool.
pub fn mark_pool_selected(pool: Option<&str>, profile: Option<&str>) {
    let Some(pool) = pool.filter(|value| !value.trim().is_empty()) else {
        return;
    };
    let Some(profile) = profile.filter(|value| !value.trim().is_empty()) else {
        return;
    };
    mutate_state(|state| mark_pool_selected_in_state(state, pool, profile));
}

/// Remove cooldown entries for one profile or an entire pool.
#[must_use]
pub fn reset_cooldowns(pool: &str, profile: Option<&str>) -> usize {
    mutate_state(|state| reset_cooldowns_in_state(state, pool, profile))
}

/// Load the shared auth-pool state file.
#[must_use]
pub fn load_state() -> AuthPoolState {
    let path = state_path();
    let Ok(contents) = fs::read_to_string(path) else {
        return AuthPoolState::default();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

fn with_state<T>(f: impl FnOnce(&AuthPoolState) -> T) -> T {
    let _guard = STATE_LOCK.lock().ok();
    let state = load_state();
    f(&state)
}

fn mutate_state<T>(f: impl FnOnce(&mut AuthPoolState) -> T) -> T {
    let _guard = STATE_LOCK.lock().ok();
    let mut state = load_state();
    let result = f(&mut state);
    save_state(&state);
    result
}

fn state_key(pool: Option<&str>, profile: Option<&str>) -> Option<String> {
    let pool = pool.filter(|value| !value.trim().is_empty())?;
    let profile = profile.filter(|value| !value.trim().is_empty())?;
    Some(format!("{pool}/{profile}"))
}

fn state_path() -> PathBuf {
    bcode_config::default_state_dir()
        .join("provider")
        .join("openai-compatible-auth-pool-state.json")
}

fn save_state(state: &AuthPoolState) {
    let path = state_path();
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    if let Ok(contents) = serde_json::to_string_pretty(state) {
        let _ = fs::write(path, contents);
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

pub(crate) fn profile_needs_priming_in_state(
    state: &AuthPoolState,
    key: &str,
    reprime_after: Option<Duration>,
    now: u64,
) -> bool {
    let Some(primed_unix) = state.entries.get(key).and_then(|entry| entry.primed_unix) else {
        return true;
    };
    reprime_after.is_some_and(|duration| now.saturating_sub(primed_unix) >= duration.as_secs())
}

pub(crate) fn profile_needs_priming_with_windows_in_state(
    state: &AuthPoolState,
    key: &str,
    required_windows: &BTreeMap<String, Vec<String>>,
    fallback_reprime_after: Option<Duration>,
    now: u64,
) -> bool {
    let Some(entry) = state.entries.get(key) else {
        return true;
    };
    let targets = usage_window_targets(entry, required_windows);
    if targets.is_empty() {
        return profile_needs_priming_in_state(state, key, fallback_reprime_after, now);
    }
    targets
        .iter()
        .any(|(meter_id, window_id)| usage_window_needs_priming(entry, meter_id, window_id, now))
}

fn usage_window_needs_priming(
    entry: &AuthPoolProfileState,
    meter_id: &str,
    window_id: &str,
    now: u64,
) -> bool {
    let Some(window) = entry
        .usage_windows
        .get(meter_id)
        .and_then(|windows| windows.get(window_id))
    else {
        return true;
    };
    if window
        .resets_at_unix
        .is_some_and(|resets_at| resets_at <= now)
    {
        return true;
    }
    if window.used_percent.is_some_and(|percent| percent > 0) {
        return false;
    }
    window.primed_at_unix.is_none()
}

fn usage_window_targets(
    entry: &AuthPoolProfileState,
    required_windows: &BTreeMap<String, Vec<String>>,
) -> BTreeSet<(String, String)> {
    let mut targets = BTreeSet::new();
    for (meter_id, windows) in required_windows {
        for window_id in windows {
            targets.insert((meter_id.clone(), window_id.clone()));
        }
    }
    if !targets.is_empty() {
        return targets;
    }
    for (meter_id, windows) in &entry.usage_windows {
        for window_id in windows.keys() {
            targets.insert((meter_id.clone(), window_id.clone()));
        }
    }
    targets
}

pub(crate) fn mark_pool_selected_in_state(state: &mut AuthPoolState, pool: &str, profile: &str) {
    state
        .pools
        .entry(pool.to_string())
        .or_default()
        .last_selected_profile = Some(profile.to_string());
}

pub(crate) fn reset_cooldowns_in_state(
    state: &mut AuthPoolState,
    pool: &str,
    profile: Option<&str>,
) -> usize {
    if let Some(profile) = profile {
        let key = format!("{pool}/{profile}");
        return usize::from(state.entries.remove(&key).is_some());
    }
    let prefix = format!("{pool}/");
    let before = state.entries.len();
    state.entries.retain(|key, _| !key.starts_with(&prefix));
    before.saturating_sub(state.entries.len())
}
