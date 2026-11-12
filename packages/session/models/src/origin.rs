//! Descriptive record of the application that created a session.
//!
//! Origin is display and filter data. It never selects storage, routes execution, grants
//! permissions or restricts which clients may open a session. It is recorded once, inside the
//! canonical `SessionCreated` event, and is never changed afterwards.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::SessionVisibility;

/// Current session-origin contract version.
pub const SESSION_ORIGIN_VERSION: u32 = 1;
/// Maximum UTF-8 bytes in an origin application id.
pub const MAX_SESSION_ORIGIN_APP_ID_BYTES: usize = 64;
/// Maximum UTF-8 bytes in an origin display name.
pub const MAX_SESSION_ORIGIN_DISPLAY_NAME_BYTES: usize = 64;
/// Maximum number of origin context entries.
pub const MAX_SESSION_ORIGIN_CONTEXT_ENTRIES: usize = 16;
/// Maximum UTF-8 bytes in one origin context key.
pub const MAX_SESSION_ORIGIN_CONTEXT_KEY_BYTES: usize = 64;
/// Maximum UTF-8 bytes in one origin context value.
pub const MAX_SESSION_ORIGIN_CONTEXT_VALUE_BYTES: usize = 256;

/// Application that created a session, with bounded secret-free context.
///
/// The context must not hold credentials, connection strings or file contents. Bcode cannot
/// detect secrets in general, so the creating application is responsible for that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOrigin {
    /// Origin contract version.
    pub version: u32,
    /// Stable producer namespace, such as `bdata` or `bcode.code-review`.
    pub app_id: String,
    /// Label shown in pickers, such as `bdata`.
    #[serde(default)]
    pub app_display_name: Option<String>,
    /// Application-owned display and filter values, such as a connection label.
    #[serde(default)]
    pub context: BTreeMap<String, String>,
    /// Whether normal pickers list the session.
    #[serde(default)]
    pub visibility: SessionVisibility,
}

/// Reason a requested origin is rejected. Out-of-bounds values are never truncated.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionOriginError {
    /// The contract version is not supported by this build.
    #[error("unsupported session origin version {0}")]
    UnsupportedVersion(u32),
    /// The application id is empty, too long, or uses characters outside `[a-z0-9._-]`.
    #[error("session origin app id must be 1-64 bytes of [a-z0-9._-]")]
    InvalidAppId,
    /// The display name is empty or longer than its bound.
    #[error("session origin display name must be 1-64 bytes")]
    InvalidDisplayName,
    /// The context has too many entries.
    #[error("session origin context holds at most 16 entries")]
    TooManyContextEntries,
    /// A context key is empty, too long, or contains control characters.
    #[error("session origin context key {0:?} must be 1-64 bytes without control characters")]
    InvalidContextKey(String),
    /// A context value is too long or contains control characters.
    #[error(
        "session origin context value for {0:?} must be at most 256 bytes without control characters"
    )]
    InvalidContextValue(String),
}

impl SessionOrigin {
    /// Create a visible origin for an application id with no display name or context.
    #[must_use]
    pub fn new(app_id: impl Into<String>) -> Self {
        Self {
            version: SESSION_ORIGIN_VERSION,
            app_id: app_id.into(),
            app_display_name: None,
            context: BTreeMap::new(),
            visibility: SessionVisibility::Visible,
        }
    }

    /// Set the picker label.
    #[must_use]
    pub fn with_display_name(mut self, display_name: impl Into<String>) -> Self {
        self.app_display_name = Some(display_name.into());
        self
    }

    /// Add one context entry, replacing an existing value for the same key.
    #[must_use]
    pub fn with_context(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.context.insert(key.into(), value.into());
        self
    }

    /// Set picker visibility.
    #[must_use]
    pub const fn with_visibility(mut self, visibility: SessionVisibility) -> Self {
        self.visibility = visibility;
        self
    }

    /// Label for pickers: the display name, or the application id when none is set.
    #[must_use]
    pub fn label(&self) -> &str {
        self.app_display_name.as_deref().unwrap_or(&self.app_id)
    }

    /// Check every bound of the origin contract.
    ///
    /// # Errors
    ///
    /// * [`SessionOriginError::UnsupportedVersion`] when `version` is not
    ///   [`SESSION_ORIGIN_VERSION`].
    /// * [`SessionOriginError::InvalidAppId`] when the app id is empty, longer than 64 bytes, or
    ///   contains characters outside `[a-z0-9._-]`.
    /// * [`SessionOriginError::InvalidDisplayName`] when the display name is empty or longer than
    ///   64 bytes.
    /// * [`SessionOriginError::TooManyContextEntries`] when there are more than 16 entries.
    /// * [`SessionOriginError::InvalidContextKey`] or [`SessionOriginError::InvalidContextValue`]
    ///   when an entry is out of bounds or contains control characters.
    pub fn validate(&self) -> Result<(), SessionOriginError> {
        if self.version != SESSION_ORIGIN_VERSION {
            return Err(SessionOriginError::UnsupportedVersion(self.version));
        }
        if !is_valid_app_id(&self.app_id) {
            return Err(SessionOriginError::InvalidAppId);
        }
        if self.app_display_name.as_ref().is_some_and(|name| {
            name.is_empty()
                || name.len() > MAX_SESSION_ORIGIN_DISPLAY_NAME_BYTES
                || name.chars().any(char::is_control)
        }) {
            return Err(SessionOriginError::InvalidDisplayName);
        }
        if self.context.len() > MAX_SESSION_ORIGIN_CONTEXT_ENTRIES {
            return Err(SessionOriginError::TooManyContextEntries);
        }
        for (key, value) in &self.context {
            if key.is_empty()
                || key.len() > MAX_SESSION_ORIGIN_CONTEXT_KEY_BYTES
                || key.chars().any(char::is_control)
            {
                return Err(SessionOriginError::InvalidContextKey(key.clone()));
            }
            if value.len() > MAX_SESSION_ORIGIN_CONTEXT_VALUE_BYTES
                || value.chars().any(char::is_control)
            {
                return Err(SessionOriginError::InvalidContextValue(key.clone()));
            }
        }
        Ok(())
    }
}

/// Return whether `app_id` is 1-64 bytes of `[a-z0-9._-]`.
#[must_use]
pub fn is_valid_app_id(app_id: &str) -> bool {
    !app_id.is_empty()
        && app_id.len() <= MAX_SESSION_ORIGIN_APP_ID_BYTES
        && app_id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

/// Catalog filter on session origin.
///
/// Sessions without a readable origin never match an origin filter; catalog pages report how
/// many examined candidates could not be classified.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOriginFilter {
    /// Required application id. `None` matches any session that has an origin.
    #[serde(default)]
    pub app_id: Option<String>,
    /// Context entries that must all be present with equal values.
    #[serde(default)]
    pub context: BTreeMap<String, String>,
}

impl SessionOriginFilter {
    /// Filter on one application id.
    #[must_use]
    pub fn app(app_id: impl Into<String>) -> Self {
        Self {
            app_id: Some(app_id.into()),
            context: BTreeMap::new(),
        }
    }

    /// Require one context entry.
    #[must_use]
    pub fn with_context(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.context.insert(key.into(), value.into());
        self
    }

    /// Return whether `origin` satisfies this filter.
    #[must_use]
    pub fn matches(&self, origin: Option<&SessionOrigin>) -> bool {
        let Some(origin) = origin else {
            return false;
        };
        self.app_id
            .as_ref()
            .is_none_or(|app_id| *app_id == origin.app_id)
            && self
                .context
                .iter()
                .all(|(key, value)| origin.context.get(key) == Some(value))
    }

    /// Check the filter's bounds, which match the origin contract.
    ///
    /// # Errors
    ///
    /// * [`SessionOriginError::InvalidAppId`] when the app id is malformed.
    /// * [`SessionOriginError::TooManyContextEntries`] when more than 16 entries are required.
    /// * [`SessionOriginError::InvalidContextKey`] or [`SessionOriginError::InvalidContextValue`]
    ///   when an entry is out of bounds.
    pub fn validate(&self) -> Result<(), SessionOriginError> {
        if self
            .app_id
            .as_deref()
            .is_some_and(|app_id| !is_valid_app_id(app_id))
        {
            return Err(SessionOriginError::InvalidAppId);
        }
        if self.context.len() > MAX_SESSION_ORIGIN_CONTEXT_ENTRIES {
            return Err(SessionOriginError::TooManyContextEntries);
        }
        for (key, value) in &self.context {
            if key.is_empty() || key.len() > MAX_SESSION_ORIGIN_CONTEXT_KEY_BYTES {
                return Err(SessionOriginError::InvalidContextKey(key.clone()));
            }
            if value.len() > MAX_SESSION_ORIGIN_CONTEXT_VALUE_BYTES {
                return Err(SessionOriginError::InvalidContextValue(key.clone()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_bounds_reject_instead_of_truncating() {
        let valid = SessionOrigin::new("bdata")
            .with_display_name("bdata")
            .with_context("connection", "prod-replica");
        assert_eq!(valid.validate(), Ok(()));
        assert_eq!(valid.label(), "bdata");

        for app_id in ["", "BData", "has space", &"a".repeat(65)] {
            assert_eq!(
                SessionOrigin::new(app_id).validate(),
                Err(SessionOriginError::InvalidAppId),
                "{app_id:?}"
            );
        }
        assert_eq!(
            SessionOrigin::new("x")
                .with_display_name("n".repeat(65))
                .validate(),
            Err(SessionOriginError::InvalidDisplayName)
        );
        let mut crowded = SessionOrigin::new("x");
        for index in 0..17 {
            crowded = crowded.with_context(format!("k{index}"), "v");
        }
        assert_eq!(
            crowded.validate(),
            Err(SessionOriginError::TooManyContextEntries)
        );
        assert_eq!(
            SessionOrigin::new("x")
                .with_context("k", "v".repeat(257))
                .validate(),
            Err(SessionOriginError::InvalidContextValue("k".into()))
        );
        assert_eq!(
            SessionOrigin::new("x").with_context("", "v").validate(),
            Err(SessionOriginError::InvalidContextKey(String::new()))
        );
        let mut future = SessionOrigin::new("x");
        future.version = 2;
        assert_eq!(
            future.validate(),
            Err(SessionOriginError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn filters_require_an_origin_and_every_context_entry() {
        let origin = SessionOrigin::new("bdata").with_context("connection", "prod");
        assert!(SessionOriginFilter::app("bdata").matches(Some(&origin)));
        assert!(SessionOriginFilter::default().matches(Some(&origin)));
        assert!(!SessionOriginFilter::default().matches(None));
        assert!(!SessionOriginFilter::app("other").matches(Some(&origin)));
        assert!(
            SessionOriginFilter::app("bdata")
                .with_context("connection", "prod")
                .matches(Some(&origin))
        );
        assert!(
            !SessionOriginFilter::app("bdata")
                .with_context("connection", "dev")
                .matches(Some(&origin))
        );
    }
}
