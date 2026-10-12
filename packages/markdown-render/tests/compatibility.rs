use bcode_markdown_render::{MarkdownDocument, MarkdownRenderOptions, parse_markdown_document};

#[test]
fn facade_preserves_shared_type_identity() {
    let document: bmux_markdown::MarkdownDocument = parse_markdown_document("# 中文 👩‍💻\n");
    let document: MarkdownDocument = document;
    assert!(!document.events.is_empty());

    let palette: bcode_syntax_render::SyntaxPalette = Default::default();
    let options: bmux_markdown::MarkdownRenderOptions = MarkdownRenderOptions {
        syntax: Some(palette),
        ..MarkdownRenderOptions::new(12)
    };
    let lines: Vec<bmux_tui::prelude::Line> =
        bcode_markdown_render::render_markdown_lines("```rust\nlet x = 1;\n```", options);
    assert!(!lines.is_empty());

    let mode: bcode_markdown_render::MermaidRenderMode =
        bcode_markdown_render::MermaidRenderMode::Disabled;
    let _: bmux_markdown::MermaidRenderMode = mode;
}
