//! Portable visual contracts owned by the plugin model layer.

pub use bcode_plugin_models::tui_visual::{
    Disclosure, FactValue, LEGACY_TUI_VISUAL_CONTEXT_VERSION, MAX_SERIALIZED_TUI_VISUAL_ROWS,
    MAX_SERIALIZED_TUI_VISUAL_SPANS, MAX_SERIALIZED_TUI_VISUAL_TEXT_BYTES,
    MAX_TUI_VISUAL_PRESENTATION_ENTRIES, MAX_TUI_VISUAL_SECTION_LABEL_BYTES,
    MAX_TUI_VISUAL_SECTION_NAME_BYTES, MAX_TUI_VISUAL_SECTION_ROWS, MAX_TUI_VISUAL_SECTIONS,
    MAX_TUI_VISUAL_SUMMARY_BYTES, MAX_TUI_VISUAL_TARGET_BYTES,
    MIN_TUI_VISUAL_ADAPTER_CONTRACT_VERSION, OP_DELIVER_TUI_VISUAL_ARTIFACT, OP_RENDER_TUI_VISUAL,
    PLUGIN_TUI_VISUAL_CONTEXT_VERSION, Preview, PreviewKeep, RenderTuiVisualPresentedResponse,
    RenderTuiVisualRequest, RenderTuiVisualResponse, ResolvedSection, SerializedTuiArtifactChunk,
    SerializedTuiArtifactChunkRequest, SerializedTuiColor, SerializedTuiModifier, SerializedTuiRow,
    SerializedTuiSection, SerializedTuiSpan, SerializedTuiStyleRole, SerializedTuiToolPresentation,
    SerializedTuiVisualContext, TUI_VISUAL_ADAPTER_CONTRACT_VERSION,
    TUI_VISUAL_ADAPTER_INTERFACE_ID, TUI_VISUAL_TOOL_PRESENTATION_VERSION, TuiVisualAnchor,
    TuiVisualPresentation, TuiVisualSourceRange, bounded_line, bounded_optional_line,
    valid_section_name, validate_tool_sections, validate_visual_anchors,
};
