//! Context blocks an application supplies with one submission.
//!
//! A block is labelled text the model reads together with the submitted message, such as the
//! application's current screen or cursor place. Blocks are durable by default: they are stored
//! with the user message, appear in later turns' model context the same way the message does, and
//! are not part of the message text that transcripts show. A block the application marks
//! request-only is never written to session history: the daemon keeps it in memory for the one
//! turn it was submitted with, and that turn fails closed when the daemon no longer holds it.

use serde::{Deserialize, Serialize};

/// Maximum number of context blocks on one submission, durable and request-only together.
pub const MAX_TURN_CONTEXT_BLOCKS: usize = 16;
/// Maximum UTF-8 bytes of all context block labels and texts on one submission.
pub const MAX_TURN_CONTEXT_BYTES: usize = 16 * 1024;
/// Maximum UTF-8 bytes in one context block label.
pub const MAX_TURN_CONTEXT_LABEL_BYTES: usize = 64;
/// Maximum UTF-8 bytes in a daemon-assigned request-only context handle.
pub const MAX_TURN_CONTEXT_HANDLE_BYTES: usize = 128;

/// One labelled context block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnContextBlock {
    /// Short label naming what the block describes, such as `screen` or `cursor`.
    pub label: String,
    /// Context text the model reads.
    pub text: String,
}

impl TurnContextBlock {
    /// A block labelled `label` holding `text`.
    #[must_use]
    pub fn new(label: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            text: text.into(),
        }
    }

    const fn byte_len(&self) -> usize {
        self.label.len() + self.text.len()
    }
}

/// Context supplied with one submission.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnContext {
    /// Durable blocks, stored with the user message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<TurnContextBlock>,
    /// Request-only blocks as the application submits them. The daemon moves them into memory
    /// before anything is persisted; history never holds them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub request_only: Vec<TurnContextBlock>,
    /// Daemon-assigned handle of the request-only blocks it holds for this turn. Applications
    /// never set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_only_handle: Option<String>,
}

/// Reason supplied turn context is rejected. Out-of-bounds context is never truncated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TurnContextError {
    /// More than [`MAX_TURN_CONTEXT_BLOCKS`] blocks.
    #[error("turn context holds at most 16 blocks")]
    TooManyBlocks,
    /// The labels and texts together exceed [`MAX_TURN_CONTEXT_BYTES`].
    #[error("turn context holds at most 16 KiB")]
    TooLarge,
    /// A label is empty or longer than [`MAX_TURN_CONTEXT_LABEL_BYTES`].
    #[error("turn context labels must be 1-64 bytes")]
    InvalidLabel,
    /// A block has no text.
    #[error("turn context blocks must not be empty")]
    EmptyBlock,
    /// The handle is empty, too long, or set together with request-only blocks.
    #[error("turn context request-only handle is invalid")]
    InvalidHandle,
}

impl TurnContext {
    /// Context with the given durable blocks.
    #[must_use]
    pub const fn durable(blocks: Vec<TurnContextBlock>) -> Self {
        Self {
            blocks,
            request_only: Vec::new(),
            request_only_handle: None,
        }
    }

    /// Context with the given request-only blocks.
    #[must_use]
    pub const fn request_only(blocks: Vec<TurnContextBlock>) -> Self {
        Self {
            blocks: Vec::new(),
            request_only: blocks,
            request_only_handle: None,
        }
    }

    /// Whether no context was supplied or retained.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.request_only.is_empty() && self.request_only_handle.is_none()
    }

    /// Check the bounds of the supplied blocks.
    ///
    /// # Errors
    ///
    /// * [`TurnContextError::TooManyBlocks`] when more than 16 blocks are supplied.
    /// * [`TurnContextError::TooLarge`] when the labels and texts exceed 16 KiB.
    /// * [`TurnContextError::InvalidLabel`] when a label is empty or longer than 64 bytes.
    /// * [`TurnContextError::EmptyBlock`] when a block has no text.
    /// * [`TurnContextError::InvalidHandle`] when the handle is empty, too long, or present
    ///   together with request-only blocks.
    pub fn validate(&self) -> Result<(), TurnContextError> {
        if self.blocks.len() + self.request_only.len() > MAX_TURN_CONTEXT_BLOCKS {
            return Err(TurnContextError::TooManyBlocks);
        }
        let mut bytes = 0_usize;
        for block in self.blocks.iter().chain(&self.request_only) {
            if block.label.is_empty() || block.label.len() > MAX_TURN_CONTEXT_LABEL_BYTES {
                return Err(TurnContextError::InvalidLabel);
            }
            if block.text.is_empty() {
                return Err(TurnContextError::EmptyBlock);
            }
            bytes = bytes.saturating_add(block.byte_len());
        }
        if bytes > MAX_TURN_CONTEXT_BYTES {
            return Err(TurnContextError::TooLarge);
        }
        if let Some(handle) = &self.request_only_handle
            && (handle.is_empty()
                || handle.len() > MAX_TURN_CONTEXT_HANDLE_BYTES
                || !self.request_only.is_empty())
        {
            return Err(TurnContextError::InvalidHandle);
        }
        Ok(())
    }
}

/// Render context blocks as the text the model reads after a message.
///
/// Each block becomes a `<context label="...">` element; quotes and angle brackets in labels are
/// replaced so a label cannot close the element early.
#[must_use]
pub fn render_turn_context_blocks(blocks: &[TurnContextBlock]) -> Option<String> {
    if blocks.is_empty() {
        return None;
    }
    let mut rendered = String::new();
    for block in blocks {
        if !rendered.is_empty() {
            rendered.push('\n');
        }
        let label = block
            .label
            .chars()
            .map(|character| match character {
                '"' | '<' | '>' => '_',
                other => other,
            })
            .collect::<String>();
        rendered.push_str("<context label=\"");
        rendered.push_str(&label);
        rendered.push_str("\">\n");
        rendered.push_str(&block.text);
        rendered.push_str("\n</context>");
    }
    Some(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_are_enforced_without_truncation() {
        let ok = TurnContext::durable(vec![TurnContextBlock::new("screen", "rows 1-20")]);
        assert_eq!(ok.validate(), Ok(()));
        let many = TurnContext::durable(vec![TurnContextBlock::new("a", "b"); 17]);
        assert_eq!(many.validate(), Err(TurnContextError::TooManyBlocks));
        let large = TurnContext::request_only(vec![TurnContextBlock::new(
            "screen",
            "x".repeat(MAX_TURN_CONTEXT_BYTES),
        )]);
        assert_eq!(large.validate(), Err(TurnContextError::TooLarge));
        let unlabelled = TurnContext::durable(vec![TurnContextBlock::new("", "text")]);
        assert_eq!(unlabelled.validate(), Err(TurnContextError::InvalidLabel));
        let empty = TurnContext::durable(vec![TurnContextBlock::new("screen", "")]);
        assert_eq!(empty.validate(), Err(TurnContextError::EmptyBlock));
        let mixed = TurnContext {
            request_only_handle: Some("handle".into()),
            ..TurnContext::request_only(vec![TurnContextBlock::new("screen", "rows")])
        };
        assert_eq!(mixed.validate(), Err(TurnContextError::InvalidHandle));
    }

    #[test]
    fn rendering_keeps_labels_inside_their_element() {
        let rendered = render_turn_context_blocks(&[
            TurnContextBlock::new("screen", "rows 1-20"),
            TurnContextBlock::new("a\"><b", "text"),
        ])
        .expect("rendered");
        assert_eq!(
            rendered,
            "<context label=\"screen\">\nrows 1-20\n</context>\n<context label=\"a___b\">\ntext\n</context>"
        );
        assert_eq!(render_turn_context_blocks(&[]), None);
    }

    #[test]
    fn empty_context_is_omitted_from_durable_json() {
        let json = serde_json::to_value(TurnContext::default()).expect("encode");
        assert_eq!(json, serde_json::json!({}));
    }
}
