//! Tools that a connected client application implements in its own process.
//!
//! A client registers namespaced tools on its daemon connection and claims the sessions in which
//! they are offered. The daemon authorizes every call with the same permission policy as plugin
//! tools before it sends the invocation to the client, and the client answers with a result.
//! Registrations and claims live only as long as the connection; nothing here is durable.

use serde::{Deserialize, Serialize};

/// Maximum tools one provider may register.
pub const MAX_CLIENT_TOOLS_PER_PROVIDER: usize = 128;
/// Maximum UTF-8 bytes of a provider id.
pub const MAX_CLIENT_TOOL_PROVIDER_ID_BYTES: usize = 64;
/// Maximum UTF-8 bytes of one tool name, including the provider namespace.
pub const MAX_CLIENT_TOOL_NAME_BYTES: usize = 128;
/// Maximum UTF-8 bytes of one tool description.
pub const MAX_CLIENT_TOOL_DESCRIPTION_BYTES: usize = 4 * 1024;
/// Maximum bytes of one tool's JSON input schema.
pub const MAX_CLIENT_TOOL_SCHEMA_BYTES: usize = 64 * 1024;
/// Longest deadline a client tool may declare, in milliseconds.
pub const MAX_CLIENT_TOOL_DURATION_MS: u32 = 10 * 60 * 1000;
/// Maximum UTF-8 bytes of a completed result's text.
pub const MAX_CLIENT_TOOL_RESULT_BYTES: usize = 1024 * 1024;

/// Tool error code when no live connection of the tool's provider has claimed the session.
pub const CLIENT_TOOL_UNAVAILABLE: &str = "client_tool_unavailable";
/// Tool error code when the provider's connection closed while the call was running.
pub const CLIENT_TOOL_DISCONNECTED: &str = "client_tool_disconnected";
/// Tool error code when the provider did not answer before the declared deadline.
pub const CLIENT_TOOL_TIMEOUT: &str = "client_tool_timeout";

/// Permission class of a client tool, evaluated by the daemon's shared tool policy.
///
/// The user's exact-name rules (for example `"bdata.run_query" = "ask"`) apply on top of the
/// declared class, so a provider that declares too weak a class cannot bypass them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientToolPolicy {
    /// Reads application state without changing it; needs no permission unless a rule asks.
    ReadOnly,
    /// Changes state or has effects outside the conversation; asks unless a rule allows it.
    #[default]
    Mutating,
}

/// One tool a client application implements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientToolDescriptor {
    /// Model-visible name in the provider's namespace: `<provider_id>.<tool>`.
    pub name: String,
    /// Model-visible description.
    pub description: String,
    /// JSON Schema of the tool's arguments.
    pub input_schema: serde_json::Value,
    /// Permission class.
    #[serde(default)]
    pub policy: ClientToolPolicy,
    /// How long the daemon waits for a result before failing the call with
    /// [`CLIENT_TOOL_TIMEOUT`].
    pub max_duration_ms: u32,
}

/// Reason a client tool registration is rejected. Registrations are accepted or rejected whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientToolRegistrationError {
    /// The provider id is not 1-64 bytes of `[a-z0-9._-]`.
    InvalidProviderId,
    /// More than [`MAX_CLIENT_TOOLS_PER_PROVIDER`] tools were registered.
    TooManyTools,
    /// A tool name is outside the provider namespace or uses characters outside
    /// `[a-z0-9._-]`.
    InvalidToolName(String),
    /// Two tools in the registration have the same name.
    DuplicateToolName(String),
    /// A description, schema or deadline exceeds its bound, or the schema is not an object.
    InvalidDescriptor(String),
}

impl std::fmt::Display for ClientToolRegistrationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidProviderId => {
                formatter.write_str("client tool provider id must be 1-64 bytes of [a-z0-9._-]")
            }
            Self::TooManyTools => write!(
                formatter,
                "a provider may register at most {MAX_CLIENT_TOOLS_PER_PROVIDER} tools"
            ),
            Self::InvalidToolName(name) => write!(
                formatter,
                "client tool name {name:?} must be <provider_id>.<tool> using [a-z0-9._-]"
            ),
            Self::DuplicateToolName(name) => {
                write!(formatter, "client tool {name:?} is registered twice")
            }
            Self::InvalidDescriptor(name) => write!(
                formatter,
                "client tool {name:?} exceeds the description, schema or deadline bounds"
            ),
        }
    }
}

impl std::error::Error for ClientToolRegistrationError {}

fn is_namespace_text(text: &str) -> bool {
    !text.is_empty()
        && text.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

/// Check a whole registration against the client tool bounds.
///
/// # Errors
///
/// * [`ClientToolRegistrationError::InvalidProviderId`] for a malformed provider id.
/// * [`ClientToolRegistrationError::TooManyTools`] for more than
///   [`MAX_CLIENT_TOOLS_PER_PROVIDER`] tools.
/// * [`ClientToolRegistrationError::InvalidToolName`] for a name outside `<provider_id>.`.
/// * [`ClientToolRegistrationError::DuplicateToolName`] for a repeated name.
/// * [`ClientToolRegistrationError::InvalidDescriptor`] for an oversized description or schema,
///   a schema that is not a JSON object, or a deadline of zero or above
///   [`MAX_CLIENT_TOOL_DURATION_MS`].
pub fn validate_client_tool_registration(
    provider_id: &str,
    tools: &[ClientToolDescriptor],
) -> Result<(), ClientToolRegistrationError> {
    if provider_id.len() > MAX_CLIENT_TOOL_PROVIDER_ID_BYTES || !is_namespace_text(provider_id) {
        return Err(ClientToolRegistrationError::InvalidProviderId);
    }
    if tools.len() > MAX_CLIENT_TOOLS_PER_PROVIDER {
        return Err(ClientToolRegistrationError::TooManyTools);
    }
    let prefix = format!("{provider_id}.");
    let mut names = std::collections::BTreeSet::new();
    for tool in tools {
        let local = tool.name.strip_prefix(&prefix).unwrap_or_default();
        if tool.name.len() > MAX_CLIENT_TOOL_NAME_BYTES || !is_namespace_text(local) {
            return Err(ClientToolRegistrationError::InvalidToolName(
                tool.name.clone(),
            ));
        }
        if !names.insert(tool.name.as_str()) {
            return Err(ClientToolRegistrationError::DuplicateToolName(
                tool.name.clone(),
            ));
        }
        let schema_bytes = serde_json::to_vec(&tool.input_schema).map_or(usize::MAX, |s| s.len());
        if tool.description.len() > MAX_CLIENT_TOOL_DESCRIPTION_BYTES
            || schema_bytes > MAX_CLIENT_TOOL_SCHEMA_BYTES
            || !tool.input_schema.is_object()
            || tool.max_duration_ms == 0
            || tool.max_duration_ms > MAX_CLIENT_TOOL_DURATION_MS
        {
            return Err(ClientToolRegistrationError::InvalidDescriptor(
                tool.name.clone(),
            ));
        }
    }
    Ok(())
}

/// Invocation the daemon sends to the claiming client after the call was authorized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientToolInvocation {
    /// Identity the client echoes in its result.
    pub invocation_id: String,
    /// Session whose turn made the call.
    pub session_id: String,
    /// Model tool-call id.
    pub tool_call_id: String,
    /// Registered tool name.
    pub tool_name: String,
    /// JSON arguments chosen by the model. Treat them as untrusted input.
    pub arguments_json: String,
    /// Milliseconds the client has to answer before the call fails.
    pub deadline_ms: u32,
}

/// A client's answer to one invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientToolOutcome {
    /// The tool ran; `content` is the model-visible result text.
    Completed {
        /// Result text. The daemon treats it as untrusted and applies tool-result bounds.
        content: String,
    },
    /// The tool ran and failed, or refused the call.
    Failed {
        /// Application-defined error code.
        code: String,
        /// Model-visible error message.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> ClientToolDescriptor {
        ClientToolDescriptor {
            name: name.to_owned(),
            description: "Run a query".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
            policy: ClientToolPolicy::ReadOnly,
            max_duration_ms: 1000,
        }
    }

    #[test]
    fn registrations_are_namespaced_bounded_and_unique() {
        assert_eq!(
            validate_client_tool_registration("bdata", &[tool("bdata.run_query")]),
            Ok(())
        );
        assert_eq!(
            validate_client_tool_registration("BData", &[]),
            Err(ClientToolRegistrationError::InvalidProviderId)
        );
        for name in ["run_query", "other.run_query", "bdata.", "bdata.Run"] {
            assert_eq!(
                validate_client_tool_registration("bdata", &[tool(name)]),
                Err(ClientToolRegistrationError::InvalidToolName(name.into())),
                "{name}"
            );
        }
        assert_eq!(
            validate_client_tool_registration(
                "bdata",
                &[tool("bdata.run_query"), tool("bdata.run_query")]
            ),
            Err(ClientToolRegistrationError::DuplicateToolName(
                "bdata.run_query".into()
            ))
        );
        let mut slow = tool("bdata.slow");
        slow.max_duration_ms = MAX_CLIENT_TOOL_DURATION_MS + 1;
        let mut schemaless = tool("bdata.schemaless");
        schemaless.input_schema = serde_json::json!("string");
        for invalid in [slow, schemaless] {
            assert!(matches!(
                validate_client_tool_registration("bdata", &[invalid]),
                Err(ClientToolRegistrationError::InvalidDescriptor(_))
            ));
        }
        let crowded = (0..=MAX_CLIENT_TOOLS_PER_PROVIDER)
            .map(|index| tool(&format!("bdata.t{index}")))
            .collect::<Vec<_>>();
        assert_eq!(
            validate_client_tool_registration("bdata", &crowded),
            Err(ClientToolRegistrationError::TooManyTools)
        );
    }
}
