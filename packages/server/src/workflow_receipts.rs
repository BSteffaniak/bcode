//! Server-owned compatibility contract for durable agent-turn recovery receipts.
//!
//! The owner identifier is the compatibility version. Unknown owners are not decoded
//! as this contract. Optional acceptance fields support previously emitted v1 receipts;
//! when supplied, they must agree with session-owned admission identity.

use bcode_session_models::{SessionId, TurnReceipt};
use serde::Deserialize;

pub const AGENT_TURN_OWNER: &str = "bcode.server.agent-turn/v1";

#[derive(Debug, Deserialize)]
pub struct AgentTurnReceipt {
    pub session_id: SessionId,
    pub turn_id: String,
    pub output_schema_id: String,
    pub owner_artifact_id: Option<String>,
    pub owner_daemon_instance_id: Option<String>,
    work_id: Option<String>,
    accepted_event_sequence: Option<u64>,
}

impl AgentTurnReceipt {
    pub fn decode(value: &serde_json::Value) -> Result<Self, &'static str> {
        if value
            .get("owner")
            .is_some_and(|owner| owner.as_str() != Some(AGENT_TURN_OWNER))
        {
            return Err("unsupported agent-turn recovery contract");
        }
        let receipt: Self = serde_json::from_value(value.clone())
            .map_err(|_| "malformed agent-turn recovery receipt")?;
        if receipt.turn_id.trim().is_empty() || receipt.output_schema_id.trim().is_empty() {
            return Err("agent-turn receipt has empty turn or output schema identity");
        }
        for identity in [
            &receipt.owner_artifact_id,
            &receipt.owner_daemon_instance_id,
            &receipt.work_id,
        ] {
            if identity.as_ref().is_some_and(|id| id.trim().is_empty()) {
                return Err("agent-turn receipt has empty ownership or work identity");
            }
        }
        if let Some(sequence) = receipt.accepted_event_sequence {
            let expected = TurnReceipt::from_accepted_event(receipt.session_id, sequence);
            if receipt.turn_id != expected.turn_id.0
                || receipt
                    .work_id
                    .as_ref()
                    .is_some_and(|id| id != &expected.work_id.0)
            {
                return Err("agent-turn receipt disagrees with session admission identity");
            }
        }
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_turn_contract_validates_admission_and_preserves_legacy_v1() {
        let session_id = SessionId::new();
        let accepted = TurnReceipt::from_accepted_event(session_id, 42);
        let value = serde_json::json!({
            "owner": AGENT_TURN_OWNER, "session_id": session_id,
            "turn_id": accepted.turn_id, "work_id": accepted.work_id,
            "accepted_event_sequence": 42, "output_schema_id": "output",
            "owner_artifact_id": "artifact", "owner_daemon_instance_id": "instance"
        });
        assert!(AgentTurnReceipt::decode(&value).is_ok());
        for (field, invalid) in [
            ("owner", serde_json::json!("bcode.server.agent-turn/v2")),
            ("accepted_event_sequence", serde_json::json!(43)),
            ("work_id", serde_json::json!("different-work")),
            ("owner_artifact_id", serde_json::json!(" ")),
            ("owner_daemon_instance_id", serde_json::json!(7)),
            ("output_schema_id", serde_json::json!("")),
        ] {
            let mut malformed = value.clone();
            malformed[field] = invalid;
            assert!(AgentTurnReceipt::decode(&malformed).is_err(), "{field}");
        }
        let legacy = serde_json::json!({"session_id": session_id, "turn_id": "legacy-turn", "output_schema_id": "output"});
        assert!(AgentTurnReceipt::decode(&legacy).is_ok());
    }
}
