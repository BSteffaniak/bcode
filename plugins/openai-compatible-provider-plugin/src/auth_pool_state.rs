use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct AuthPoolState {
    #[serde(default)]
    entries: BTreeMap<String, AuthPoolProfileState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AuthPoolProfileState {
    cooldown_until_unix: u64,
    reason: String,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    reset_source: Option<String>,
}

pub fn is_profile_available(pool: Option<&str>, profile: Option<&str>) -> bool {
    let Some(key) = state_key(pool, profile) else {
        return true;
    };
    let state = load_state();
    state
        .entries
        .get(&key)
        .is_none_or(|entry| entry.cooldown_until_unix <= now_unix())
}

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
    let mut state = load_state();
    state.entries.insert(
        key,
        AuthPoolProfileState {
            cooldown_until_unix,
            reason: reason.to_string(),
            last_error: Some(message.to_string()),
            reset_source: reset_source.map(ToString::to_string),
        },
    );
    save_state(&state);
}

pub fn profile_cooldown_until(pool: Option<&str>, profile: Option<&str>) -> Option<u64> {
    let key = state_key(pool, profile)?;
    let state = load_state();
    state
        .entries
        .get(&key)
        .map(|entry| entry.cooldown_until_unix)
        .filter(|until| *until > now_unix())
}

pub fn clear_profile_quota_limited(pool: Option<&str>, profile: Option<&str>) {
    let Some(key) = state_key(pool, profile) else {
        return;
    };
    let mut state = load_state();
    if state.entries.remove(&key).is_some() {
        save_state(&state);
    }
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

fn load_state() -> AuthPoolState {
    let path = state_path();
    let Ok(contents) = fs::read_to_string(path) else {
        return AuthPoolState::default();
    };
    serde_json::from_str(&contents).unwrap_or_default()
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
