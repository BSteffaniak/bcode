//! Bounded finite same-consumer semantic controller automation.
//!
//! The input is a sequence of semantic gestures, not raw exchange resolutions.
//! Expected revisions are selected from each successful snapshot, never retried.

use bcode_tool_models::interaction::InteractionInput;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControllerDriveRequest {
    pub version: u32,
    pub exchange_id: String,
    pub inputs: Vec<InteractionInput>,
}

impl ControllerDriveRequest {
    pub fn validate(&self) -> Result<(), crate::CliError> {
        if self.version != 1
            || self.exchange_id.is_empty()
            || self.exchange_id.len() > 256
            || self.inputs.is_empty()
            || self.inputs.len() > 128
        {
            return Err(crate::CliError::InvalidArguments(
                "controller drive requires version 1, a bounded exchange identity, and 1..=128 semantic inputs".to_owned(),
            ));
        }
        Ok(())
    }
}
