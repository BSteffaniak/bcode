//! Markdown rendering for Bcode terminal surfaces.
//!
//! This crate uses `hyperchad_markdown` as the Markdown parser and semantic
//! conversion layer, then projects the generated `HyperChad` container tree into
//! `bmux_tui` terminal lines and spans.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use hyperchad_color::Color as HyperChadColor;
use hyperchad_markdown::{MarkdownOptions, markdown_to_container_with_options};
use hyperchad_transformer::{Container, Element};
use hyperchad_transformer_models::{FontWeight, TextDecorationLine};
use unicode_segmentation::UnicodeSegmentation;

/// Options controlling terminal Markdown rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkdownRenderOptions {
    /// Available terminal width in cells.
    pub width: u16,
    /// Whether the source Markdown is still streaming.
    pub streaming: bool,
}

impl Default for MarkdownRenderOptions {
    fn default() -> Self {
        Self {
            width: 80,
            streaming: false,
        }
    }
}

/// Render Markdown into terminal lines.
#[must_use]
pub fn render_markdown_lines(markdown: &str, options: MarkdownRenderOptions) -> Vec<Line> {
    let container = markdown_to_container_with_options(markdown, hyperchad_markdown_options());
    let mut renderer = TerminalMarkdownRenderer::new(options.width);
    renderer.render_container_children(&container, TextStyle::default());
    renderer.finish()
}

fn hyperchad_markdown_options() -> MarkdownOptions {
    MarkdownOptions {
        enable_tables: true,
        enable_strikethrough: true,
        enable_tasklists: true,
        enable_footnotes: false,
        enable_smart_punctuation: true,
        emoji_enabled: false,
        xss_protection: true,
        syntax_highlighting: false,
        link_resolver: None,
    }
}

#[derive(Debug, Clone, Copy)]
struct TextStyle {
    style: Style,
    preserve_whitespace: bool,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            style: Style::new(),
            preserve_whitespace: false,
        }
    }
}

impl TextStyle {
    fn merge_container(self, container: &Container) -> Self {
        let mut output = self;
        if let Some(color) = container.color {
            output.style = output.style.fg(hyperchad_color_to_tui(color));
        }
        if let Some(background) = container.background {
            output.style = output.style.bg(hyperchad_color_to_tui(background));
        }
        if container.font_weight.is_some_and(is_bold_weight) {
            output.style = output.style.add_modifier(Modifier::BOLD);
        }
        if container
            .text_decoration
            .as_ref()
            .is_some_and(|decoration| decoration.line.contains(&TextDecorationLine::Underline))
        {
            output.style = output.style.add_modifier(Modifier::UNDERLINE);
        }
        if container
            .text_decoration
            .as_ref()
            .is_some_and(|decoration| decoration.line.contains(&TextDecorationLine::LineThrough))
        {
            output.style = output.style.add_modifier(Modifier::CROSSED_OUT);
        }
        if container
            .font_family
            .as_ref()
            .is_some_and(|families| families.iter().any(|family| family == "monospace"))
        {
            output.style = output.style.fg(Color::Yellow);
        }
        output
    }
}

#[derive(Debug)]
struct TerminalMarkdownRenderer {
    width: usize,
    rows: Vec<Line>,
    current_spans: Vec<Span>,
    current_width: usize,
}

impl TerminalMarkdownRenderer {
    fn new(width: u16) -> Self {
        Self {
            width: usize::from(width.max(1)),
            rows: Vec::new(),
            current_spans: Vec::new(),
            current_width: 0,
        }
    }

    fn finish(mut self) -> Vec<Line> {
        self.flush_line();
        trim_blank_edges(&mut self.rows);
        self.rows
    }

    fn render_container_children(&mut self, container: &Container, style: TextStyle) {
        for child in &container.children {
            self.render_container(child, style);
        }
    }

    fn render_container(&mut self, container: &Container, style: TextStyle) {
        if container.hidden == Some(true) {
            return;
        }

        let style = style.merge_container(container);
        match &container.element {
            Element::Text { value } | Element::Raw { value } => {
                self.push_text(value, style);
            }
            Element::Span
            | Element::Anchor { .. }
            | Element::THead
            | Element::TBody
            | Element::TH { .. }
            | Element::TD { .. } => {
                self.render_container_children(container, style);
            }
            Element::Heading { .. } => {
                self.ensure_blank_line();
                self.render_container_children(
                    container,
                    TextStyle {
                        style: style.style.fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        preserve_whitespace: style.preserve_whitespace,
                    },
                );
                self.ensure_blank_line();
            }
            Element::UnorderedList => self.render_list(container, false, style),
            Element::OrderedList => self.render_list(container, true, style),
            Element::ListItem => {
                self.render_container_children(container, style);
                self.flush_line();
            }
            Element::Table => self.render_table(container, style),
            Element::TR => {
                self.render_table_row(container, style);
            }
            Element::Input { .. } => {
                self.push_text("☐", style);
            }
            Element::Image { alt, source, .. } => {
                let image = alt
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| source.as_deref().unwrap_or("image"));
                self.push_text(&format!("[image: {image}]"), style);
            }
            _ => {
                let is_block = is_block_container(container);
                if is_block {
                    self.flush_line();
                }
                if is_block
                    && container
                        .classes
                        .iter()
                        .any(|class| class == "markdown-blockquote")
                {
                    self.push_text(
                        "│ ",
                        TextStyle {
                            style: Style::new().fg(Color::BrightBlack),
                            preserve_whitespace: false,
                        },
                    );
                }
                let child_style = if container
                    .classes
                    .iter()
                    .any(|class| class == "markdown-code-block")
                {
                    TextStyle {
                        style: style.style.fg(Color::Yellow),
                        preserve_whitespace: true,
                    }
                } else {
                    style
                };
                self.render_container_children(container, child_style);
                if is_block {
                    self.flush_line();
                    if container.classes.iter().any(|class| {
                        class == "markdown-code-block" || class == "markdown-blockquote"
                    }) {
                        self.ensure_blank_line();
                    }
                }
            }
        }
    }

    fn render_list(&mut self, container: &Container, ordered: bool, style: TextStyle) {
        self.flush_line();
        for (index, child) in container.children.iter().enumerate() {
            if matches!(child.element, Element::ListItem) {
                if matches!(child.element, Element::ListItem) {
                    let marker = if ordered {
                        format!("{}. ", index.saturating_add(1))
                    } else {
                        "• ".to_owned()
                    };
                    self.push_text(
                        &marker,
                        TextStyle {
                            style: Style::new().fg(Color::BrightBlack),
                            preserve_whitespace: false,
                        },
                    );
                }
                self.render_container(child, style);
            }
        }
        self.ensure_blank_line();
    }

    fn render_table(&mut self, container: &Container, style: TextStyle) {
        self.flush_line();
        self.render_container_children(container, style);
        self.ensure_blank_line();
    }

    fn render_table_row(&mut self, container: &Container, style: TextStyle) {
        self.flush_line();
        for (index, child) in container.children.iter().enumerate() {
            if index > 0 {
                self.push_text(
                    " │ ",
                    TextStyle {
                        style: Style::new().fg(Color::BrightBlack),
                        preserve_whitespace: false,
                    },
                );
            }
            self.render_container(child, style);
        }
        self.flush_line();
    }

    fn ensure_blank_line(&mut self) {
        self.flush_line();
        if self.rows.last().is_none_or(|line| !line.spans.is_empty()) {
            self.rows.push(Line::default());
        }
    }

    fn flush_line(&mut self) {
        if self.current_spans.is_empty() {
            return;
        }
        self.rows
            .push(Line::from_spans(std::mem::take(&mut self.current_spans)));
        self.current_width = 0;
    }

    fn push_text(&mut self, text: &str, style: TextStyle) {
        for segment in text.split_inclusive('\n') {
            let without_newline = segment.strip_suffix('\n').unwrap_or(segment);
            if style.preserve_whitespace {
                self.push_wrapped_text(without_newline, style.style);
            } else {
                self.push_wrapped_text(&normalize_inline_whitespace(without_newline), style.style);
            }
            if segment.ends_with('\n') {
                self.flush_line();
            }
        }
    }

    fn push_wrapped_text(&mut self, text: &str, style: Style) {
        for grapheme in text.graphemes(true) {
            let grapheme_width = text_display_width(grapheme);
            if self.current_width > 0
                && self.current_width.saturating_add(grapheme_width) > self.width
            {
                self.flush_line();
            }
            self.current_spans
                .push(Span::styled(grapheme.to_owned(), style));
            self.current_width = self.current_width.saturating_add(grapheme_width);
        }
    }
}

const fn is_block_container(container: &Container) -> bool {
    matches!(
        container.element,
        Element::Div
            | Element::Section
            | Element::Header
            | Element::Footer
            | Element::Aside
            | Element::Main
            | Element::Form { .. }
    )
}

fn normalize_inline_whitespace(text: &str) -> String {
    let mut output = String::new();
    let mut previous_whitespace = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !previous_whitespace {
                output.push(' ');
            }
            previous_whitespace = true;
        } else {
            output.push(ch);
            previous_whitespace = false;
        }
    }
    output
}

fn trim_blank_edges(rows: &mut Vec<Line>) {
    while rows.first().is_some_and(|line| line.spans.is_empty()) {
        rows.remove(0);
    }
    while rows.last().is_some_and(|line| line.spans.is_empty()) {
        rows.pop();
    }
}

const fn is_bold_weight(weight: FontWeight) -> bool {
    matches!(
        weight,
        FontWeight::Bold
            | FontWeight::ExtraBold
            | FontWeight::Black
            | FontWeight::Bolder
            | FontWeight::Weight600
            | FontWeight::Weight700
            | FontWeight::Weight800
            | FontWeight::Weight900
    )
}

const fn hyperchad_color_to_tui(color: HyperChadColor) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

fn text_display_width(text: &str) -> usize {
    text.chars().map(char_display_width).sum()
}

fn char_display_width(ch: char) -> usize {
    if ch == '\t' {
        4
    } else if ch.is_control() {
        0
    } else if ch.len_utf8() > 1 {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::{MarkdownRenderOptions, render_markdown_lines};

    fn rendered_text(markdown: &str) -> String {
        render_markdown_lines(
            markdown,
            MarkdownRenderOptions {
                width: 80,
                streaming: false,
            },
        )
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
    }

    #[test]
    fn renders_heading_and_paragraph_text() {
        let output = rendered_text("# Title\n\nHello **world**.");

        assert!(output.contains("Title"));
        assert!(output.contains("Hello world."));
    }

    #[test]
    fn renders_lists_with_markers() {
        let output = rendered_text("- one\n- two");

        assert!(output.contains("• one"));
        assert!(output.contains("• two"));
    }

    #[test]
    fn renders_code_blocks_preserving_lines() {
        let output = rendered_text("```rust\nfn main() {}\n```");

        assert!(output.contains("fn main() {}"));
    }
}
