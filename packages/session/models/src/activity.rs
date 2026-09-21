//! Portable activity presentation. Display data never authorizes execution.

use serde::{Deserialize, Serialize};

/// Service for producer-owned activity projection; hosts route to an explicitly selected plugin.
pub const ACTIVITY_PRESENTATION_INTERFACE_ID: &str = "bcode.activity-presentation/v1";
/// Project one explicitly associated activity boundary.
pub const OP_PROJECT_ACTIVITY: &str = "project";

/// Maximum encoded projection request, checked before decoding at the producer boundary.
pub const MAX_ACTIVITY_PROJECTION_REQUEST_BYTES: usize = 1_048_576;

/// Read-only projection input. No field grants execution or document access authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityProjectionRequest {
    /// Producer-defined stage, not inferred from user text.
    pub stage: String,
    /// Producer activity revision supplied by the lifecycle owner.
    pub revision: u64,
    /// Already admitted structured input supplied by the host.
    pub input: serde_json::Value,
}

/// Current generic activity envelope version; producer payload versions evolve separately.
pub const ACTIVITY_PRESENTATION_VERSION: u32 = 1;
/// Maximum encoded envelope size, including the fallback and producer payload.
pub const MAX_ACTIVITY_PRESENTATION_BYTES: usize = 65_536;

/// Producer-owned display content for a host-associated activity.
///
/// The host supplies session/turn association outside this envelope. These fields must never
/// select a turn, grant authority, or determine a workflow outcome. Unknown envelope versions
/// are preserved on decode but must not be interpreted. Unknown payload schemas use `fallback`.
/// This type alone does not establish persistence, delivery ordering, or durable resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityPresentation {
    /// Generic envelope compatibility version.
    pub version: u32,
    /// Producer namespace verified by the publishing host.
    pub producer: String,
    /// Producer-local activity identity, scoped by the host's execution identity.
    pub activity_id: String,
    /// Monotonic replacement revision within that scope, starting at one.
    pub revision: u64,
    /// Producer-owned payload schema identifier.
    pub schema: String,
    /// Independent payload schema version.
    pub schema_version: u32,
    /// Human-readable fallback for clients without the payload adapter.
    pub fallback: String,
    /// Bounded renderer-neutral semantic payload, not execution input.
    pub payload: serde_json::Value,
}

impl ActivityPresentation {
    /// Validate a supported envelope against the authenticated producer namespace.
    ///
    /// # Errors
    /// * Unsupported envelope version or mismatched producer.
    /// * Empty, oversized, or control-containing identities; zero revisions/schema versions.
    /// * Empty/oversized fallback or an encoded envelope exceeding its byte budget.
    ///
    /// Validation does not authenticate the caller or associate the payload with a turn.
    pub fn validate(&self, authenticated_producer: &str) -> Result<(), &'static str> {
        if self.version != ACTIVITY_PRESENTATION_VERSION {
            return Err("unsupported activity envelope version");
        }
        if self.producer != authenticated_producer {
            return Err("activity producer does not match authenticated publisher");
        }
        for identity in [&self.producer, &self.activity_id, &self.schema] {
            if identity.trim().is_empty()
                || identity.len() > 256
                || identity.chars().any(char::is_control)
            {
                return Err("invalid activity identity");
            }
        }
        if self.revision == 0 || self.schema_version == 0 {
            return Err("activity revision and schema version must be nonzero");
        }
        if self.fallback.trim().is_empty() || self.fallback.len() > 4_096 {
            return Err("invalid activity fallback");
        }
        serde_json::to_writer(EncodingBudget(MAX_ACTIVITY_PRESENTATION_BYTES), self)
            .map_err(|_| "activity envelope exceeds byte limit or cannot be encoded")?;
        Ok(())
    }
}

// Count encoded bytes without allocating a second copy of a potentially oversized payload.
struct EncodingBudget(usize);

impl std::io::Write for EncodingBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("activity encoding budget exhausted"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presentation() -> ActivityPresentation {
        ActivityPresentation {
            version: ACTIVITY_PRESENTATION_VERSION,
            producer: "example.workflow".into(),
            activity_id: "iteration:4".into(),
            revision: 1,
            schema: "example.iteration".into(),
            schema_version: 1,
            fallback: "Iteration 4 · implementing".into(),
            payload: serde_json::json!({"iteration": 4}),
        }
    }

    #[test]
    fn round_trip_and_unknown_schema_preserve_fallback() {
        let mut value = presentation();
        value.schema_version = 99;
        assert!(value.validate("example.workflow").is_ok());
        let decoded: ActivityPresentation =
            serde_json::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(decoded, value);
        value.version = 99;
        let decoded: ActivityPresentation =
            serde_json::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(decoded, value);
        assert!(decoded.validate("example.workflow").is_err());
    }

    #[test]
    fn encoded_limit_accounts_for_envelope_and_json_escaping() {
        let mut value = presentation();
        value.payload = serde_json::json!("");
        let overhead = serde_json::to_vec(&value).unwrap().len();
        value.payload = serde_json::json!("x".repeat(MAX_ACTIVITY_PRESENTATION_BYTES - overhead));
        assert_eq!(
            serde_json::to_vec(&value).unwrap().len(),
            MAX_ACTIVITY_PRESENTATION_BYTES
        );
        assert!(value.validate("example.workflow").is_ok());
        value.payload =
            serde_json::json!("x".repeat(MAX_ACTIVITY_PRESENTATION_BYTES - overhead + 1));
        assert!(value.validate("example.workflow").is_err());
        value.payload = serde_json::json!("\n".repeat(MAX_ACTIVITY_PRESENTATION_BYTES / 2));
        assert!(value.validate("example.workflow").is_err());
    }

    #[test]
    fn rejects_spoofing_invalid_identity_and_excessive_payload() {
        let mut value = presentation();
        assert!(value.validate("different.plugin").is_err());
        value.activity_id = "bad\nidentity".into();
        assert!(value.validate("example.workflow").is_err());
        value = presentation();
        value.payload = serde_json::json!("x".repeat(MAX_ACTIVITY_PRESENTATION_BYTES));
        assert!(value.validate("example.workflow").is_err());
        value = presentation();
        value.revision = 0;
        assert!(value.validate("example.workflow").is_err());
    }
}
