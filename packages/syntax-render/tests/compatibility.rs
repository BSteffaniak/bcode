use bcode_syntax_render::{SyntaxHighlighter, SyntaxSpan};

#[test]
fn facade_preserves_tokens_and_terminal_type_identity() {
    let highlighter: bmux_syntax_render::SyntaxHighlighter = SyntaxHighlighter::new();
    let source = "let 中文 = \"👩‍💻 e\u{301}\";";
    let tokens: Vec<bmux_syntax_render::SyntaxSpan> =
        highlighter.highlight_line_tokens("rust", source);
    let tokens: Vec<SyntaxSpan> = tokens;
    let reconstructed: String = tokens.iter().map(|span| span.content.as_str()).collect();
    assert_eq!(reconstructed, source);

    let spans: Vec<bmux_tui::prelude::Span> = highlighter.highlight_line("rust", source);
    let reconstructed: String = spans.iter().map(|span| span.content.as_str()).collect();
    assert_eq!(reconstructed, source);
}
