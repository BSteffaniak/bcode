//! Renderer-neutral interactive tool session primitives.

pub use bcode_tool_models::interaction::{
    InteractionControlId, InteractionInput, InteractionNavigation, InteractionOutput,
    InteractionValue,
};
use serde::Serialize;

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
