//! Exact launch-configured browser origins; never an authentication mechanism.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Default)]
pub struct BrowserOrigins(BTreeSet<String>);

impl BrowserOrigins {
    pub(super) fn new(origins: &[String]) -> Result<Self, &'static str> {
        let mut allowed = BTreeSet::new();
        for origin in origins {
            if !valid_origin(origin) {
                return Err("invalid browser origin");
            }
            allowed.insert(origin.clone());
        }
        Ok(Self(allowed))
    }

    pub(super) fn allows(&self, headers: &BTreeMap<String, String>) -> bool {
        let mut origins = headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("origin"));
        let Some((_, origin)) = origins.next() else {
            // Capability-authenticated non-browser clients need not send Origin.
            return true;
        };
        origins.next().is_none() && valid_origin(origin) && self.0.contains(origin)
    }
}

fn valid_origin(origin: &str) -> bool {
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    !origin.contains('*')
        && matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.origin().ascii_serialization() == origin
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HyperChadAppState;
    use bcode_client::BcodeClient;
    use hyperchad::router::{RequestInfo, RouteRequest};

    #[test]
    fn configured_origins_are_exact_and_never_replace_the_token() {
        let state = HyperChadAppState::new(BcodeClient::default_endpoint(), "secret");
        let mut request = RouteRequest::from_path("/?token=secret", RequestInfo::default());
        assert!(state.authorizes(&request));
        request
            .headers
            .insert("Origin".into(), "http://127.0.0.1:8765".into());
        assert!(!state.authorizes(&request));
        let routed_clone = state.clone();
        let state = state
            .with_browser_origins(&[
                "http://127.0.0.1:8765".into(),
                "https://embed.example".into(),
            ])
            .unwrap();
        assert!(state.authorizes(&request));
        assert!(routed_clone.authorizes(&request));
        for origin in ["https://embed.example", "http://127.0.0.1:8765"] {
            request.headers.insert("Origin".into(), origin.into());
            assert!(state.authorizes(&request));
            request.query.clear();
            assert!(!state.authorizes(&request));
            request.query.insert("token".into(), "wrong".into());
            assert!(!state.authorizes(&request));
            request.query.insert("token".into(), "secret".into());
        }
        for origin in [
            "null",
            "",
            "https://evil.example",
            "http://127.0.0.1:8766",
            "http://127.0.0.1:8765.evil.example",
            "http://127.0.0.1:8765/path",
            "https://embed.example https://evil.example",
            "https://user@embed.example",
        ] {
            request.headers.insert("Origin".into(), origin.into());
            request
                .headers
                .insert("Host".into(), "embed.example".into());
            request
                .headers
                .insert("Forwarded".into(), "host=embed.example;proto=https".into());
            assert!(!state.authorizes(&request), "{origin}");
        }
        request
            .headers
            .insert("Origin".into(), "https://embed.example".into());
        request
            .headers
            .insert("origin".into(), "https://embed.example".into());
        assert!(!state.authorizes(&request));
        request.headers.remove("Origin");
        assert!(state.authorizes(&request));
    }

    #[test]
    fn invalid_configuration_does_not_replace_the_active_policy() {
        let state = HyperChadAppState::new(BcodeClient::default_endpoint(), "secret")
            .with_browser_origins(&["https://embed.example".into()])
            .unwrap();
        for origin in [
            "null",
            "*",
            "file:///tmp",
            "https://embed.example/",
            "https://embed.example?x=1",
            "https://embed.example#x",
            "https://user:password@embed.example",
            "https://*.example",
            "https://embed.example\n",
            "http://[::1]:bad",
        ] {
            assert!(
                state
                    .clone()
                    .with_browser_origins(&[origin.into()])
                    .is_err(),
                "{origin}"
            );
        }
        let mut request = RouteRequest::from_path("/?token=secret", RequestInfo::default());
        request
            .headers
            .insert("origin".into(), "https://embed.example".into());
        assert!(state.authorizes(&request));
        assert!(BrowserOrigins::new(&["http://[::1]:8765".into()]).is_ok());
    }
}
