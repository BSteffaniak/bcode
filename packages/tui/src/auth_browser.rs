//! Native browser adaptation for normalized authentication effects.

use std::collections::BTreeSet;

/// Per-flow automatic-open policy; failed attempts are not repeated on every poll.
pub struct AuthBrowser {
    enabled: bool,
    attempted: BTreeSet<String>,
}

impl AuthBrowser {
    pub const fn new(enabled: bool) -> Self {
        Self {
            enabled,
            attempted: BTreeSet::new(),
        }
    }

    pub fn open(&mut self, url: &str) -> bool {
        self.open_with(url, |url| webbrowser::open(url).is_ok())
    }

    fn open_with(&mut self, value: &str, opener: impl FnOnce(&str) -> bool) -> bool {
        if !self.enabled || self.attempted.contains(value) {
            return true;
        }
        let Ok(url) = url::Url::parse(value) else {
            return false;
        };
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return false;
        }
        self.attempted.insert(value.to_owned());
        opener(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_never_invokes_opener() {
        assert!(
            AuthBrowser::new(false).open_with("https://example.com", |_| panic!("must not open"))
        );
    }
    #[test]
    fn repeated_effects_do_not_spawn_tabs_even_after_failure() {
        let mut browser = AuthBrowser::new(true);
        assert!(!browser.open_with("https://example.com", |_| false));
        assert!(browser.open_with("https://example.com", |_| panic!("duplicate open")));
    }
    #[test]
    fn rejects_non_web_and_credential_bearing_urls() {
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://user:secret@example.com",
            "not-a-url",
        ] {
            assert!(!AuthBrowser::new(true).open_with(url, |_| panic!("unsafe open")));
        }
    }
}
