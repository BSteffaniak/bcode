//! Agent profiles that a connected client application contributes.
//!
//! An application, such as a database client, may contribute profiles like `bdata.sql` with its
//! own instructions and a narrowed tool selection. A contributed profile can only restrict: it
//! names a base agent whose permission policy every call is still evaluated by, and it may only
//! remove tools from what that base agent exposes. It carries no permission rules, so an
//! application can never grant itself permissions. A profile exists only while a connection of
//! its application is registered; a session that selected it fails closed with
//! `agent_profile_unavailable` while the application is absent.

use serde::{Deserialize, Serialize};

/// Most profiles one application connection may contribute.
pub const MAX_CLIENT_AGENT_PROFILES: usize = 16;
/// Maximum UTF-8 bytes of a contributed profile id (`<provider_id>.<name>`).
pub const MAX_CLIENT_AGENT_PROFILE_ID_BYTES: usize = 128;
/// Maximum UTF-8 bytes of a contributed profile's display name.
pub const MAX_CLIENT_AGENT_PROFILE_NAME_BYTES: usize = 64;
/// Maximum UTF-8 bytes of a contributed profile's description.
pub const MAX_CLIENT_AGENT_PROFILE_DESCRIPTION_BYTES: usize = 1024;
/// Maximum UTF-8 bytes of a contributed profile's instructions.
pub const MAX_CLIENT_AGENT_PROFILE_INSTRUCTIONS_BYTES: usize = 32 * 1024;
/// Most tool names one contributed profile may select.
pub const MAX_CLIENT_AGENT_PROFILE_TOOLS: usize = 256;
/// Maximum UTF-8 bytes of a base agent id or one selected tool name.
pub const MAX_CLIENT_AGENT_PROFILE_NAME_FIELD_BYTES: usize = 256;

/// An agent profile a client application contributes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientAgentProfile {
    /// Stable id in the application's namespace: `<provider_id>.<name>`, such as `bdata.sql`.
    pub id: String,
    /// Display name for agent pickers.
    pub name: String,
    /// Short description for agent pickers.
    #[serde(default)]
    pub description: String,
    /// Agent whose tools and permission policy this profile narrows, such as `build`.
    pub base_agent: String,
    /// Instructions appended to the base agent's instructions.
    #[serde(default)]
    pub instructions: String,
    /// Exact names of the tools the profile may use: the application's own tools and named
    /// plugin tools. Tools the base agent does not expose stay unavailable.
    pub tools: Vec<String>,
}

/// Reason a profile registration is rejected as a whole.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientAgentProfileError {
    /// More than [`MAX_CLIENT_AGENT_PROFILES`] profiles.
    #[error("an application contributes at most 16 agent profiles")]
    TooManyProfiles,
    /// A profile id is outside the provider's namespace or out of bounds.
    #[error(
        "agent profile id {0:?} must be `<provider_id>.<name>` of [a-z0-9._-], at most 128 bytes"
    )]
    InvalidId(String),
    /// Two profiles share an id.
    #[error("agent profile id {0:?} is contributed twice")]
    DuplicateId(String),
    /// A display name, description, base agent or instructions are empty or out of bounds.
    #[error(
        "agent profile {0:?} has an empty or oversized name, description, base agent or instructions"
    )]
    InvalidField(String),
    /// The tool selection is out of bounds or names a tool twice.
    #[error("agent profile {0:?} selects too many, duplicate, empty or oversized tool names")]
    InvalidTools(String),
    /// A profile's base agent is itself a contributed profile.
    #[error("agent profile {0:?} must narrow a configured agent, not another contributed profile")]
    NestedBase(String),
}

/// Check a whole profile registration for `provider_id`.
///
/// # Errors
///
/// * [`ClientAgentProfileError::TooManyProfiles`] when more than 16 profiles are given.
/// * [`ClientAgentProfileError::InvalidId`] when an id is not `<provider_id>.<name>` of
///   `[a-z0-9._-]` or exceeds 128 bytes.
/// * [`ClientAgentProfileError::DuplicateId`] when two profiles share an id.
/// * [`ClientAgentProfileError::InvalidField`] when the name, base agent, description or
///   instructions are empty where required or exceed their bounds.
/// * [`ClientAgentProfileError::InvalidTools`] when the tool selection exceeds 256 names, or a
///   name is empty, oversized or repeated.
/// * [`ClientAgentProfileError::NestedBase`] when the base agent lies in the provider's own
///   namespace.
pub fn validate_client_agent_profiles(
    provider_id: &str,
    profiles: &[ClientAgentProfile],
) -> Result<(), ClientAgentProfileError> {
    if profiles.len() > MAX_CLIENT_AGENT_PROFILES {
        return Err(ClientAgentProfileError::TooManyProfiles);
    }
    let prefix = format!("{provider_id}.");
    let mut ids = std::collections::BTreeSet::new();
    for profile in profiles {
        let id = &profile.id;
        let name = id.strip_prefix(&prefix).unwrap_or_default();
        if name.is_empty()
            || id.len() > MAX_CLIENT_AGENT_PROFILE_ID_BYTES
            || !id.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
            })
        {
            return Err(ClientAgentProfileError::InvalidId(id.clone()));
        }
        if !ids.insert(id.as_str()) {
            return Err(ClientAgentProfileError::DuplicateId(id.clone()));
        }
        if profile.name.is_empty()
            || profile.name.len() > MAX_CLIENT_AGENT_PROFILE_NAME_BYTES
            || profile.description.len() > MAX_CLIENT_AGENT_PROFILE_DESCRIPTION_BYTES
            || profile.instructions.len() > MAX_CLIENT_AGENT_PROFILE_INSTRUCTIONS_BYTES
            || profile.base_agent.is_empty()
            || profile.base_agent.len() > MAX_CLIENT_AGENT_PROFILE_NAME_FIELD_BYTES
        {
            return Err(ClientAgentProfileError::InvalidField(id.clone()));
        }
        if profile.base_agent.starts_with(&prefix) {
            return Err(ClientAgentProfileError::NestedBase(id.clone()));
        }
        let mut tools = std::collections::BTreeSet::new();
        if profile.tools.len() > MAX_CLIENT_AGENT_PROFILE_TOOLS
            || profile.tools.iter().any(|tool| {
                tool.is_empty()
                    || tool.len() > MAX_CLIENT_AGENT_PROFILE_NAME_FIELD_BYTES
                    || !tools.insert(tool.as_str())
            })
        {
            return Err(ClientAgentProfileError::InvalidTools(id.clone()));
        }
    }
    Ok(())
}

/// Whether `agent_id` has the shape of a contributed profile id (`<provider_id>.<name>`).
///
/// A selected agent with this shape that neither a plugin nor a connected application provides
/// is an unavailable contributed profile, and turns that select it fail closed.
#[must_use]
pub fn is_client_agent_profile_id(agent_id: &str) -> bool {
    agent_id
        .split_once('.')
        .is_some_and(|(provider, name)| !provider.is_empty() && !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str) -> ClientAgentProfile {
        ClientAgentProfile {
            id: id.to_owned(),
            name: "SQL".to_owned(),
            description: "Answers questions about the open database".to_owned(),
            base_agent: "build".to_owned(),
            instructions: "Prefer read-only queries.".to_owned(),
            tools: vec!["bdata.run_query".to_owned(), "read".to_owned()],
        }
    }

    #[test]
    fn registrations_are_checked_whole() {
        assert_eq!(
            validate_client_agent_profiles("bdata", &[profile("bdata.sql")]),
            Ok(())
        );
        assert_eq!(
            validate_client_agent_profiles("bdata", &[profile("other.sql")]),
            Err(ClientAgentProfileError::InvalidId("other.sql".into()))
        );
        assert_eq!(
            validate_client_agent_profiles("bdata", &[profile("bdata.")]),
            Err(ClientAgentProfileError::InvalidId("bdata.".into()))
        );
        assert_eq!(
            validate_client_agent_profiles("bdata", &[profile("bdata.sql"), profile("bdata.sql")]),
            Err(ClientAgentProfileError::DuplicateId("bdata.sql".into()))
        );
        let mut nested = profile("bdata.sql");
        nested.base_agent = "bdata.other".into();
        assert_eq!(
            validate_client_agent_profiles("bdata", &[nested]),
            Err(ClientAgentProfileError::NestedBase("bdata.sql".into()))
        );
        let mut repeated = profile("bdata.sql");
        repeated.tools.push("read".into());
        assert_eq!(
            validate_client_agent_profiles("bdata", &[repeated]),
            Err(ClientAgentProfileError::InvalidTools("bdata.sql".into()))
        );
        let mut long = profile("bdata.sql");
        long.instructions = "x".repeat(MAX_CLIENT_AGENT_PROFILE_INSTRUCTIONS_BYTES + 1);
        assert_eq!(
            validate_client_agent_profiles("bdata", &[long]),
            Err(ClientAgentProfileError::InvalidField("bdata.sql".into()))
        );
    }

    #[test]
    fn profile_ids_are_recognized_by_shape() {
        assert!(is_client_agent_profile_id("bdata.sql"));
        assert!(!is_client_agent_profile_id("build"));
        assert!(!is_client_agent_profile_id(".sql"));
        assert!(!is_client_agent_profile_id("bdata."));
    }
}
