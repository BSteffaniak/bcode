//! Portable visual contracts owned by the plugin model layer.

pub use bcode_plugin_models::tui_visual::{
    MAX_SERIALIZED_TUI_VISUAL_ROWS, MAX_SERIALIZED_TUI_VISUAL_SPANS,
    MAX_SERIALIZED_TUI_VISUAL_TEXT_BYTES, MIN_TUI_VISUAL_ADAPTER_CONTRACT_VERSION,
    OP_DELIVER_TUI_VISUAL_ARTIFACT, OP_RENDER_TUI_VISUAL, RenderTuiVisualRequest,
    RenderTuiVisualResponse, SerializedTuiArtifactChunk, SerializedTuiArtifactChunkRequest,
    SerializedTuiColor, SerializedTuiModifier, SerializedTuiRow, SerializedTuiSpan,
    SerializedTuiStyleRole, SerializedTuiVisualContext, TUI_VISUAL_ADAPTER_CONTRACT_VERSION,
    TUI_VISUAL_ADAPTER_INTERFACE_ID, TuiVisualAnchor, TuiVisualSourceRange,
    validate_visual_anchors,
};
