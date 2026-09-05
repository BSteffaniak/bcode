//! Ephemeral Codex response routing, scoped to one application turn and credential identity.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::header::HeaderValue;

const MAX_ROUTES: usize = 256;
const MAX_TOKEN_BYTES: usize = 16 * 1024;
const ROUTE_IDLE_TTL: Duration = Duration::from_hours(1);
pub const HEADER: &str = "x-codex-turn-state";

/// Never share a routing token across turns, accounts, endpoints, or models.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Scope {
    pub session: String,
    pub turn: String,
    pub endpoint: String,
    pub model: String,
    pub profile: Option<String>,
    pub account: Option<String>,
}

#[derive(Debug)]
struct Route {
    token: HeaderValue,
    used: Instant,
}

#[derive(Clone, Default)]
pub struct TurnRouting(Arc<Mutex<BTreeMap<Scope, Route>>>);

impl std::fmt::Debug for TurnRouting {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TurnRouting")
            .finish_non_exhaustive()
    }
}

impl TurnRouting {
    pub fn get(&self, scope: &Scope) -> Option<HeaderValue> {
        let mut routes = self.0.lock().expect("turn routing lock poisoned");
        let now = Instant::now();
        routes.retain(|_, route| now.duration_since(route.used) < ROUTE_IDLE_TTL);
        let route = routes.get_mut(scope)?;
        route.used = now;
        let token = route.token.clone();
        drop(routes);
        Some(token)
    }

    pub fn capture(&self, scope: Scope, token: &HeaderValue) {
        if token.is_empty() || token.as_bytes().len() > MAX_TOKEN_BYTES {
            return;
        }
        let mut routes = self.0.lock().expect("turn routing lock poisoned");
        // The first response wins, matching Codex's per-turn OnceLock semantics.
        if routes.contains_key(&scope) {
            return;
        }
        if routes.len() >= MAX_ROUTES {
            let oldest = routes
                .iter()
                .min_by_key(|(_, route)| route.used)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                routes.remove(&oldest);
            }
        }
        let mut token = token.clone();
        token.set_sensitive(true);
        routes.insert(
            scope,
            Route {
                token,
                used: Instant::now(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> Scope {
        Scope {
            session: "session".into(),
            turn: "turn".into(),
            endpoint: "endpoint".into(),
            model: "model".into(),
            profile: Some("profile".into()),
            account: Some("account".into()),
        }
    }

    #[test]
    fn first_token_is_reused_only_for_matching_scope_and_is_sensitive() {
        let routing = TurnRouting::default();
        let original = scope();
        routing.capture(original.clone(), &HeaderValue::from_static("first"));
        routing.capture(original.clone(), &HeaderValue::from_static("second"));
        let token = routing.get(&original).unwrap();
        assert_eq!(token, "first");
        assert!(token.is_sensitive());
        for field in 0..6 {
            let mut other = original.clone();
            match field {
                0 => other.session = "other".into(),
                1 => other.turn = "other".into(),
                2 => other.endpoint = "other".into(),
                3 => other.model = "other".into(),
                4 => other.profile = Some("other".into()),
                _ => other.account = Some("other".into()),
            }
            assert!(routing.get(&other).is_none());
        }
    }

    #[test]
    fn storage_and_token_sizes_are_bounded_and_idle_routes_expire() {
        let routing = TurnRouting::default();
        routing.capture(
            scope(),
            &HeaderValue::from_str(&"x".repeat(MAX_TOKEN_BYTES + 1)).unwrap(),
        );
        assert!(routing.get(&scope()).is_none());
        for turn in 0..=MAX_ROUTES {
            let mut key = scope();
            key.turn = turn.to_string();
            routing.capture(key, &HeaderValue::from_static("token"));
        }
        assert_eq!(routing.0.lock().unwrap().len(), MAX_ROUTES);
        let key = scope();
        routing.capture(key.clone(), &HeaderValue::from_static("token"));
        routing.0.lock().unwrap().get_mut(&key).unwrap().used =
            Instant::now().checked_sub(ROUTE_IDLE_TTL).unwrap();
        assert!(routing.get(&key).is_none());
    }
}
