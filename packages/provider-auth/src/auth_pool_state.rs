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
    /// Provider-selected applicable priming windows; absent means discovery is unknown.
    #[serde(default)]
    pub priming_windows: Option<BTreeMap<String, Vec<String>>>,
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
    /// Window length in seconds when known.
    #[serde(default)]
    pub window_duration_secs: Option<u64>,
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

/// Record provider-selected applicable priming windows after successful discovery.
pub fn record_profile_priming_windows(
    pool: Option<&str>,
    profile: Option<&str>,
    windows: Option<&BTreeMap<String, Vec<String>>>,
) {
    let Some(key) = state_key(pool, profile) else {
        return;
    };
    mutate_state(|state| {
        state.entries.entry(key).or_default().priming_windows = windows.cloned();
    });
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
                        window_duration_secs: window.window_duration_secs,
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

/// Clear the round-robin cursor so a newly preferred profile is selected next.
pub fn clear_pool_routing_cursor(pool: Option<&str>) {
    let Some(pool) = pool.filter(|value| !value.trim().is_empty()) else {
        return;
    };
    mutate_state(|state| {
        if let Some(entry) = state.pools.get_mut(pool) {
            entry.last_selected_profile = None;
        }
    });
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
    let contents = fs::read_to_string(path).or_else(|_| fs::read_to_string(legacy_state_path()));
    let Ok(contents) = contents else {
        return AuthPoolState::default();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

fn with_state<T>(f: impl FnOnce(&AuthPoolState) -> T) -> T {
    let _guard = STATE_LOCK.lock().ok();
    let state = load_state();
    f(&state)
}

fn observation_lock() -> std::io::Result<fs::File> {
    let path = state_path();
    fs::create_dir_all(
        path.parent()
            .ok_or_else(|| std::io::Error::other("missing parent"))?,
    )?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    file.try_lock().map_err(std::io::Error::other)?;
    Ok(file)
}

fn mutate_state<T>(f: impl FnOnce(&mut AuthPoolState) -> T) -> T {
    let _guard = STATE_LOCK.lock().ok();
    let ownership = observation_lock();
    let mut state = load_state();
    let result = f(&mut state);
    if ownership.is_ok() {
        save_state(&state);
    }
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
        .join("auth-pool-state.json")
}

fn legacy_state_path() -> PathBuf {
    bcode_config::default_state_dir()
        .join("provider")
        .join("openai-compatible-auth-pool-state.json")
}

pub(crate) struct ObservationOwnership {
    _guard: std::sync::MutexGuard<'static, ()>,
    _file: fs::File,
}

/// Copy observations without removing the source, making retries after interruption safe.
///
/// # Errors
/// Rejects unsupported/corrupt state, conflicting destination observations or failed atomic writes.
pub(crate) fn preserve_pool_observations(
    source: &str,
    destination: &str,
) -> Result<ObservationOwnership, bcode_config::ConfigError> {
    use std::io::{Read as _, Write as _};
    let invalid = || bcode_config::ConfigError::Composition {
        message: "Auth-pool observations are incompatible or unavailable; existing state preserved"
            .into(),
    };
    let guard = STATE_LOCK.lock().map_err(|_| invalid())?;
    let file = observation_lock().map_err(|_| invalid())?;
    let ownership = ObservationOwnership {
        _guard: guard,
        _file: file,
    };
    let path = state_path();
    let input = if path.exists() {
        path.clone()
    } else {
        legacy_state_path()
    };
    let mut bytes = Vec::new();
    match fs::File::open(&input) {
        Ok(file) => {
            file.take(8 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| invalid())?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(ownership),
        Err(_) => return Err(invalid()),
    }
    if bytes.len() > 8 * 1024 * 1024 {
        return Err(invalid());
    }
    let original: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    let mut state: AuthPoolState =
        serde_json::from_value(original.clone()).map_err(|_| invalid())?;
    if serde_json::to_value(&state).map_err(|_| invalid())? != original {
        return Err(invalid());
    }
    copy_pool_observations(&mut state, source, destination).map_err(|()| invalid())?;
    let parent = path.parent().ok_or_else(invalid)?;
    fs::create_dir_all(parent).map_err(|_| invalid())?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|_| invalid())?;
    serde_json::to_writer_pretty(&mut temporary, &state).map_err(|_| invalid())?;
    temporary.flush().map_err(|_| invalid())?;
    temporary.as_file().sync_all().map_err(|_| invalid())?;
    temporary.persist(&path).map_err(|_| invalid())?;
    Ok(ownership)
}

fn copy_pool_observations(
    state: &mut AuthPoolState,
    source: &str,
    destination: &str,
) -> Result<(), ()> {
    let mut next = state.clone();
    let prefix = format!("{source}/");
    for (key, value) in &state.entries {
        if let Some(profile) = key.strip_prefix(&prefix) {
            let target = format!("{destination}/{profile}");
            if next
                .entries
                .get(&target)
                .is_some_and(|existing| existing != value)
            {
                return Err(());
            }
            next.entries.insert(target, value.clone());
        }
    }
    if let Some(routing) = state.pools.get(source) {
        if next
            .pools
            .get(destination)
            .is_some_and(|existing| existing != routing)
        {
            return Err(());
        }
        next.pools.insert(destination.to_owned(), routing.clone());
    }
    *state = next;
    Ok(())
}

fn save_state(state: &AuthPoolState) {
    let path = state_path();
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    if let Ok(mut temporary) = tempfile::NamedTempFile::new_in(parent)
        && serde_json::to_writer_pretty(&mut temporary, state).is_ok()
        && temporary.as_file().sync_all().is_ok()
    {
        let _ = temporary.persist(path);
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
    window.used_percent.is_none_or(|percent| percent == 0)
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
    if let Some(windows) = &entry.priming_windows {
        for (meter_id, windows) in windows {
            for window_id in windows {
                targets.insert((meter_id.clone(), window_id.clone()));
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconciliation_observation_copy_is_retryable_and_rejects_conflicts_without_changes() {
        let mut state = AuthPoolState::default();
        state.entries.insert(
            "old/one".into(),
            AuthPoolProfileState {
                cooldown_until_unix: 100,
                ..Default::default()
            },
        );
        state.pools.insert(
            "old".into(),
            AuthPoolRoutingState {
                last_selected_profile: Some("one".into()),
            },
        );
        copy_pool_observations(&mut state, "old", "chosen").unwrap();
        assert_eq!(state.entries["old/one"], state.entries["chosen/one"]);
        let after = state.clone();
        copy_pool_observations(&mut state, "old", "chosen").unwrap();
        assert_eq!(state, after);
        state
            .entries
            .get_mut("chosen/one")
            .unwrap()
            .cooldown_until_unix = 200;
        let before = state.clone();
        assert!(copy_pool_observations(&mut state, "old", "chosen").is_err());
        assert_eq!(state, before);
    }

    #[test]
    fn provider_targets_exclude_missing_secondary_and_unrelated_cached_meters() {
        let mut state = AuthPoolState::default();
        let entry = state.entries.entry("pool/account".into()).or_default();
        entry.priming_windows = Some(BTreeMap::from([("weekly".into(), vec!["main".into()])]));
        entry.usage_windows.insert(
            "weekly".into(),
            BTreeMap::from([(
                "main".into(),
                AuthPoolUsageWindowState {
                    used_percent: Some(10),
                    resets_at_unix: Some(200),
                    ..Default::default()
                },
            )]),
        );
        entry.usage_windows.insert(
            "unrelated".into(),
            BTreeMap::from([("other".into(), AuthPoolUsageWindowState::default())]),
        );
        assert!(!profile_needs_priming_with_windows_in_state(
            &state,
            "pool/account",
            &BTreeMap::new(),
            None,
            100
        ));
        assert!(profile_needs_priming_with_windows_in_state(
            &state,
            "pool/account",
            &BTreeMap::from([("weekly".into(), vec!["missing".into()])]),
            None,
            100
        ));
    }

    #[test]
    fn provider_window_with_zero_usage_still_needs_priming_even_when_locally_marked() {
        let state = AuthPoolState {
            entries: BTreeMap::from([(
                "openai/openai-2".to_string(),
                AuthPoolProfileState {
                    primed_unix: Some(1_000),
                    usage_windows: BTreeMap::from([(
                        "codex".to_string(),
                        BTreeMap::from([(
                            "primary".to_string(),
                            AuthPoolUsageWindowState {
                                meter_id: "codex".to_string(),
                                window_id: "primary".to_string(),
                                resets_at_unix: Some(2_000),
                                used_percent: Some(0),
                                primed_at_unix: Some(1_000),
                                ..AuthPoolUsageWindowState::default()
                            },
                        )]),
                    )]),
                    ..AuthPoolProfileState::default()
                },
            )]),
            pools: BTreeMap::new(),
        };
        let required_windows = BTreeMap::from([("codex".to_string(), vec!["primary".to_string()])]);

        assert!(profile_needs_priming_with_windows_in_state(
            &state,
            "openai/openai-2",
            &required_windows,
            None,
            1_500,
        ));
    }

    #[test]
    fn provider_window_with_verified_usage_does_not_need_priming() {
        let state = AuthPoolState {
            entries: BTreeMap::from([(
                "openai/openai-2".to_string(),
                AuthPoolProfileState {
                    usage_windows: BTreeMap::from([(
                        "codex".to_string(),
                        BTreeMap::from([(
                            "primary".to_string(),
                            AuthPoolUsageWindowState {
                                meter_id: "codex".to_string(),
                                window_id: "primary".to_string(),
                                resets_at_unix: Some(2_000),
                                used_percent: Some(1),
                                ..AuthPoolUsageWindowState::default()
                            },
                        )]),
                    )]),
                    ..AuthPoolProfileState::default()
                },
            )]),
            pools: BTreeMap::new(),
        };
        let required_windows = BTreeMap::from([("codex".to_string(), vec!["primary".to_string()])]);

        assert!(!profile_needs_priming_with_windows_in_state(
            &state,
            "openai/openai-2",
            &required_windows,
            None,
            1_500,
        ));
    }
}
