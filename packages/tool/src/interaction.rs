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
            Self::Null | Self::Bool(_) | Self::List(_) | Self::Object(_) => None,
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
    /// Interaction completed successfully.
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

/// Renderer-neutral interaction controller.
pub trait InteractionController {
    /// Snapshot type consumed by renderers.
    type Snapshot: Serialize;

    /// Stable interaction kind.
    fn kind(&self) -> &'static str;

    /// Return the current domain snapshot.
    fn snapshot(&self) -> Self::Snapshot;

    /// Handle semantic input from any renderer/client.
    fn handle_input(&mut self, input: InteractionInput) -> InteractionOutput;
}
