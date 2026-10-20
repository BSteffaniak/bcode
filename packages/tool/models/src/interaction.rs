//! Renderer-neutral interactive tool session primitives.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Stable identifier for a renderer-visible interaction control.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InteractionControlId(pub String);

impl InteractionControlId {
    /// Create a control id.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Return this control id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Renderer-neutral value submitted by an interaction client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InteractionValue {
    /// Empty value.
    Null,
    /// Boolean value.
    Bool(bool),
    /// Numeric value.
    Number(i64),
    /// String value.
    String(String),
    /// List value.
    List(Vec<Self>),
    /// Object value.
    Object(BTreeMap<String, Self>),
}

impl InteractionValue {
    /// Return the contained string, if this is a string value.
    #[must_use]
    pub const fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value.as_str()),
            Self::Null | Self::Bool(_) | Self::Number(_) | Self::List(_) | Self::Object(_) => None,
        }
    }
}

/// Focus traversal direction requested by a renderer/client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionNavigation {
    /// Move to the next focusable control.
    Next,
    /// Move to the previous focusable control.
    Previous,
}

/// Semantic input from any interaction renderer/client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InteractionInput {
    /// Activate a control, such as a button or option.
    Activate {
        /// Control to activate.
        control_id: InteractionControlId,
    },
    /// Replace a control value.
    Change {
        /// Control whose value changed.
        control_id: InteractionControlId,
        /// New value.
        value: InteractionValue,
    },
    /// Focus a control.
    Focus {
        /// Control to focus.
        control_id: InteractionControlId,
    },
    /// Remove focus from a control.
    Blur {
        /// Control losing focus.
        control_id: InteractionControlId,
    },
    /// Navigate focus semantically.
    Navigate {
        /// Direction to move focus.
        direction: InteractionNavigation,
    },
    /// Timer tick for interactions with time-based state.
    Tick,
    /// Submit the interaction.
    Submit,
    /// Cancel/dismiss the interaction.
    Cancel,
}

/// Semantic output from an interaction controller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InteractionOutput {
    /// No renderer/host action is needed.
    None,
    /// Redraw or refresh the interaction snapshot.
    Redraw,
    /// Plugin proposes successful completion; the application must commit the resolution.
    Submitted {
        /// Domain-specific completion payload.
        payload: serde_json::Value,
    },
    /// Interaction was cancelled.
    Cancelled,
}

impl InteractionOutput {
    /// Return whether this output requests a redraw.
    #[must_use]
    pub const fn requests_redraw(&self) -> bool {
        matches!(self, Self::Redraw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn semantic_inputs_retain_existing_renderer_neutral_wire_representation() {
        let fixtures = [
            (
                InteractionInput::Activate {
                    control_id: InteractionControlId::new("choice"),
                },
                json!({"type":"activate","control_id":"choice"}),
            ),
            (
                InteractionInput::Change {
                    control_id: InteractionControlId::new("answer"),
                    value: InteractionValue::String("yes".into()),
                },
                json!({"type":"change","control_id":"answer","value":"yes"}),
            ),
            (
                InteractionInput::Navigate {
                    direction: InteractionNavigation::Previous,
                },
                json!({"type":"navigate","direction":"previous"}),
            ),
            (InteractionInput::Submit, json!({"type":"submit"})),
            (InteractionInput::Cancel, json!({"type":"cancel"})),
        ];
        for (input, fixture) in fixtures {
            assert_eq!(serde_json::to_value(&input).unwrap(), fixture);
            assert_eq!(
                serde_json::from_value::<InteractionInput>(fixture).unwrap(),
                input
            );
        }
    }

    #[test]
    fn proposed_effects_retain_wire_shape_and_unknown_future_variants_fail_closed() {
        let payload = json!({"domain":"opaque"});
        let submitted = InteractionOutput::Submitted {
            payload: payload.clone(),
        };
        assert_eq!(
            serde_json::to_value(&submitted).unwrap(),
            json!({"type":"submitted","payload":payload})
        );
        assert_eq!(
            serde_json::from_value::<InteractionOutput>(json!({"type":"cancelled"})).unwrap(),
            InteractionOutput::Cancelled
        );
        assert!(
            serde_json::from_value::<InteractionInput>(json!({"type":"future_input"})).is_err()
        );
        assert!(
            serde_json::from_value::<InteractionOutput>(json!({"type":"future_effect"})).is_err()
        );
    }
}
