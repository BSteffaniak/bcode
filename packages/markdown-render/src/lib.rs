//! Markdown rendering for Bcode terminal surfaces.
//!
//! This crate uses `hyperchad_markdown` as the Markdown parser and semantic
//! conversion layer, then projects the generated `HyperChad` container tree into
//! `bmux_tui` terminal lines and spans.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_tui::prelude::{Color, Line, Modifier, Span, Style};
use hyperchad_color::Color as HyperChadColor;
use hyperchad_markdown::{MarkdownOptions, markdown_to_container_with_options};
use hyperchad_transformer::{Container, Element, Input};
use hyperchad_transformer_models::{FontWeight, TextDecorationLine};
use unicode_segmentation::UnicodeSegmentation;

/// Terminal styles used for Markdown rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkdownTheme {
    /// Base text style.
    pub text: Style,
    /// Heading style.
    pub heading: Style,
    /// Link style.
    pub link: Style,
    /// Strong emphasis style.
    pub strong: Style,
    /// Emphasis style.
    pub emphasis: Style,
    /// Strikethrough style.
    pub strikethrough: Style,
    /// Inline code style.
    pub inline_code: Style,
    /// Code block text fallback style.
    pub code_block_text: Style,
    /// Code block border style.
    pub code_block_border: Style,
    /// Blockquote bar style.
    pub blockquote_bar: Style,
    /// List marker style.
    pub list_marker: Style,
    /// Checked task marker style.
    pub task_checked: Style,
    /// Unchecked task marker style.
    pub task_unchecked: Style,
    /// Table border style.
    pub table_border: Style,
    /// Horizontal rule style.
    pub horizontal_rule: Style,
}

impl Default for MarkdownTheme {
    /// Create the default terminal Markdown theme.
    fn default() -> Self {
        let muted = Style::new().fg(Color::BrightBlack);
        Self {
            text: Style::new(),
            heading: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            link: Style::new()
                .fg(Color::Blue)
                .add_modifier(Modifier::UNDERLINE),
            strong: Style::new().add_modifier(Modifier::BOLD),
            emphasis: Style::new().add_modifier(Modifier::ITALIC),
            strikethrough: Style::new().add_modifier(Modifier::CROSSED_OUT),
            inline_code: Style::new().fg(Color::Yellow),
            code_block_text: Style::new().fg(Color::Yellow),
            code_block_border: muted,
            blockquote_bar: muted,
            list_marker: muted,
            task_checked: muted,
            task_unchecked: muted,
            table_border: muted,
            horizontal_rule: muted,
        }
    }
}

/// Options controlling terminal Markdown rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkdownRenderOptions {
    /// Available terminal width in cells.
    pub width: u16,
    /// Whether the source Markdown is still streaming.
    pub streaming: bool,
    /// Theme used for terminal Markdown styles.
    pub theme: MarkdownTheme,
}

impl Default for MarkdownRenderOptions {
    /// Create default Markdown render options.
    fn default() -> Self {
        Self {
            width: 80,
            streaming: false,
            theme: MarkdownTheme::default(),
        }
    }
}

impl MarkdownRenderOptions {
    /// Create render options for a terminal width.
    #[must_use]
    pub fn new(width: u16) -> Self {
        Self {
            width,
            ..Self::default()
        }
    }

    /// Return options with a custom theme.
    #[must_use]
    pub const fn with_theme(mut self, theme: MarkdownTheme) -> Self {
        self.theme = theme;
        self
    }

    /// Return options with streaming mode set.
    #[must_use]
    pub const fn streaming(mut self, streaming: bool) -> Self {
        self.streaming = streaming;
        self
    }
}

/// Render Markdown into terminal lines.
#[must_use]
pub fn render_markdown_lines(markdown: &str, options: MarkdownRenderOptions) -> Vec<Line> {
    let container = markdown_to_container_with_options(markdown, hyperchad_markdown_options());
    let mut renderer = TerminalMarkdownRenderer::new(options.width, options.theme);
    renderer.render_container_children(
        &container,
        TextStyle {
            style: options.theme.text,
            preserve_whitespace: false,
        },
    );
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
        syntax_highlighting: true,
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
    fn merge_container(self, container: &Container, theme: MarkdownTheme) -> Self {
        let mut output = self;
        if let Some(style) = semantic_markdown_style(container, theme) {
            output.style = style;
        } else if let Some(color) = container.color {
            output.style = output.style.fg(hyperchad_color_to_tui(color));
        }
        if container
            .classes
            .iter()
            .any(|class| class == "markdown-link")
        {
            output.style = theme.link;
        }
        if container
            .classes
            .iter()
            .any(|class| class == "markdown-strong")
        {
            output.style = theme.strong;
        }
        if container.classes.iter().any(|class| class == "markdown-em") {
            output.style = theme.emphasis;
        }
        if container.font_weight.is_some_and(is_bold_weight) {
            output.style = theme.strong;
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
            output.style = theme.strikethrough;
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
    in_table_collection: bool,
    theme: MarkdownTheme,
}

impl TerminalMarkdownRenderer {
    fn new(width: u16, theme: MarkdownTheme) -> Self {
        Self {
            width: usize::from(width.max(1)),
            rows: Vec::new(),
            current_spans: Vec::new(),
            current_width: 0,
            in_table_collection: false,
            theme,
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

        let style = style.merge_container(container, self.theme);
        match &container.element {
            Element::Text { value } | Element::Raw { value } => {
                self.push_text(value, style);
            }
            Element::Span | Element::Anchor { .. } | Element::THead | Element::TBody => {
                self.render_container_children(container, style);
            }
            Element::TH { .. } | Element::TD { .. } => {
                if !self.in_table_collection {
                    self.render_container_children(container, style);
                }
            }
            Element::Table => self.render_table(container, style),
            Element::Heading { .. } => {
                self.ensure_blank_line();
                self.render_container_children(
                    container,
                    TextStyle {
                        style: self.theme.heading,
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
            Element::TR => {
                if !self.in_table_collection {
                    self.render_table_row(container, style);
                }
            }
            Element::Input { input, .. } => {
                self.render_input(input, style);
            }
            Element::Image { alt, source, .. } => {
                let image = alt
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| source.as_deref().unwrap_or("image"));
                self.push_text(&format!("[image: {image}]"), style);
            }
            _ => {
                self.render_special_block_container(container, style);
            }
        }
    }

    fn render_input(&mut self, input: &Input, style: TextStyle) {
        match input {
            Input::Checkbox { checked } => {
                let marker = if checked.unwrap_or(false) {
                    "☑ "
                } else {
                    "☐ "
                };
                let marker_style = if checked.unwrap_or(false) {
                    self.theme.task_checked
                } else {
                    self.theme.task_unchecked
                };
                self.push_text(
                    marker,
                    TextStyle {
                        style: marker_style,
                        preserve_whitespace: false,
                    },
                );
            }
            Input::Text { value, .. } | Input::Password { value, .. } | Input::Hidden { value } => {
                if let Some(value) = value {
                    self.push_text(value, style);
                }
            }
        }
    }

    fn render_special_block_container(&mut self, container: &Container, style: TextStyle) {
        if container.classes.iter().any(|class| class == "markdown-hr") {
            self.render_horizontal_rule();
            return;
        }
        if container
            .classes
            .iter()
            .any(|class| class == "markdown-code-block")
        {
            self.render_code_block(container, style);
            return;
        }
        if container
            .classes
            .iter()
            .any(|class| class == "markdown-blockquote")
        {
            self.render_blockquote(container, style);
            return;
        }
        let is_block = is_block_container(container);
        if is_block {
            self.flush_line();
        }
        self.render_container_children(container, style);
        if is_block {
            self.flush_line();
        }
    }

    fn render_blockquote(&mut self, container: &Container, style: TextStyle) {
        self.flush_line();
        let mut nested = Self::new(
            u16::try_from(self.width.saturating_sub(2).max(1)).unwrap_or(u16::MAX),
            self.theme,
        );
        nested.render_container_children(container, style);
        nested.flush_line();
        let rows = nested.finish();
        let border_style = self.theme.blockquote_bar;
        for line in rows {
            let mut spans = vec![Span::styled("│ ", border_style)];
            spans.extend(line.spans);
            self.rows.push(Line::from_spans(spans));
        }
        self.ensure_blank_line();
    }

    fn render_code_block(&mut self, container: &Container, _style: TextStyle) {
        self.flush_line();
        self.ensure_blank_line();
        let border_style = self.theme.code_block_border;
        let language = container.data.get("language").map(String::as_str);
        let header = language.map_or_else(|| "╭─".to_owned(), |language| format!("╭─ {language}"));
        self.rows
            .push(Line::from_spans(vec![Span::styled(header, border_style)]));

        let nested_width = u16::try_from(self.width.saturating_sub(2).max(1)).unwrap_or(u16::MAX);
        let mut nested = Self::new(nested_width, self.theme);
        nested.render_container_children(
            container,
            TextStyle {
                style: self.theme.code_block_text,
                preserve_whitespace: true,
            },
        );
        nested.flush_line();
        let mut code_rows = nested.finish();
        if code_rows.is_empty() {
            code_rows.push(Line::default());
        }
        for line in code_rows {
            let mut spans = vec![Span::styled("│ ", border_style)];
            spans.extend(line.spans);
            self.rows.push(Line::from_spans(spans));
        }
        self.rows
            .push(Line::from_spans(vec![Span::styled("╰─", border_style)]));
        self.ensure_blank_line();
    }

    fn render_horizontal_rule(&mut self) {
        self.flush_line();
        self.ensure_blank_line();
        self.rows.push(Line::from_spans(vec![Span::styled(
            "─".repeat(self.width.max(1)),
            self.theme.horizontal_rule,
        )]));
        self.ensure_blank_line();
    }

    fn render_list(&mut self, container: &Container, ordered: bool, style: TextStyle) {
        self.flush_line();
        let list_items = container
            .children
            .iter()
            .filter(|child| matches!(child.element, Element::ListItem))
            .collect::<Vec<_>>();
        if list_items.is_empty() {
            self.ensure_blank_line();
            return;
        }

        let marker_digits = if ordered {
            decimal_digits(list_items.len())
        } else {
            0
        };
        for (index, child) in list_items.iter().enumerate() {
            let marker = if ordered {
                format!("{:>marker_digits$}.  ", index.saturating_add(1))
            } else {
                "•  ".to_owned()
            };
            self.render_prefixed_list_item(child, style, &marker);
        }
        self.ensure_blank_line();
    }

    fn render_prefixed_list_item(&mut self, item: &Container, style: TextStyle, marker: &str) {
        let marker_width = text_display_width(marker);
        let nested_width =
            u16::try_from(self.width.saturating_sub(marker_width).max(1)).unwrap_or(u16::MAX);
        let mut nested = Self::new(nested_width, self.theme);
        nested.render_container_children(item, style);
        nested.flush_line();
        let mut item_rows = nested.finish();
        if item_rows.is_empty() {
            item_rows.push(Line::default());
        }

        let muted = self.theme.list_marker;
        let continuation = " ".repeat(marker_width);
        for (row_index, row) in item_rows.into_iter().enumerate() {
            let prefix = if row_index == 0 {
                marker
            } else {
                &continuation
            };
            let mut spans = vec![Span::styled(prefix.to_owned(), muted)];
            spans.extend(row.spans);
            self.rows.push(Line::from_spans(spans));
        }
    }

    fn render_table(&mut self, container: &Container, style: TextStyle) {
        self.flush_line();
        let rows = table_rows(container, style, self.theme);
        if rows.is_empty() {
            self.ensure_blank_line();
            return;
        }

        let column_count = rows.iter().map(|row| row.cells.len()).max().unwrap_or(0);
        if column_count == 0 {
            self.ensure_blank_line();
            return;
        }

        let widths = table_column_widths(&rows, column_count);
        let total_width = widths.iter().sum::<usize>() + column_count.saturating_mul(3) + 1;
        if total_width > self.width {
            self.render_stacked_table_rows(&rows);
            return;
        }

        let border_style = self.theme.table_border;
        self.rows
            .push(table_border_line('┌', '┬', '┐', &widths, border_style));
        for (row_index, row) in rows.iter().enumerate() {
            self.rows
                .push(table_content_line(&row.cells, &widths, border_style));
            if (row.header || row_index == 0) && rows.len() > 1 {
                self.rows
                    .push(table_border_line('├', '┼', '┤', &widths, border_style));
            }
        }
        self.rows
            .push(table_border_line('└', '┴', '┘', &widths, border_style));
        self.ensure_blank_line();
    }

    fn render_stacked_table_rows(&mut self, rows: &[TableRow]) {
        let muted = self.theme.table_border;
        for (row_index, row) in rows.iter().enumerate() {
            if row_index > 0 {
                self.rows.push(Line::default());
            }
            for (column_index, cell) in row.cells.iter().enumerate() {
                let mut spans = vec![Span::styled(format!("{}: ", column_index + 1), muted)];
                spans.extend(cell.clone());
                self.rows.push(Line::from_spans(spans));
            }
        }
        self.ensure_blank_line();
    }

    fn render_table_row(&mut self, container: &Container, style: TextStyle) {
        self.flush_line();
        for (index, child) in container.children.iter().enumerate() {
            if index > 0 {
                self.push_text(
                    " │ ",
                    TextStyle {
                        style: self.theme.table_border,
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

fn decimal_digits(value: usize) -> usize {
    value.max(1).to_string().len()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TableRow {
    cells: Vec<Vec<Span>>,
    header: bool,
}

fn table_rows(container: &Container, style: TextStyle, theme: MarkdownTheme) -> Vec<TableRow> {
    let mut rows = Vec::new();
    collect_table_rows(container, style, theme, &mut rows);
    rows
}

fn collect_table_rows(
    container: &Container,
    style: TextStyle,
    theme: MarkdownTheme,
    rows: &mut Vec<TableRow>,
) {
    let style = style.merge_container(container, theme);
    if matches!(container.element, Element::TR) {
        let header = container
            .children
            .iter()
            .any(|child| matches!(child.element, Element::TH { .. }));
        rows.push(TableRow {
            cells: container
                .children
                .iter()
                .filter(|child| matches!(child.element, Element::TH { .. } | Element::TD { .. }))
                .map(|cell| inline_spans_for_container(cell, style, theme))
                .collect(),
            header,
        });
        return;
    }
    for child in &container.children {
        collect_table_rows(child, style, theme, rows);
    }
}

fn inline_spans_for_container(
    container: &Container,
    style: TextStyle,
    theme: MarkdownTheme,
) -> Vec<Span> {
    let mut renderer = TerminalMarkdownRenderer::new(u16::MAX, theme);
    renderer.in_table_collection = true;
    renderer.render_container_children(container, style.merge_container(container, theme));
    renderer.flush_line();
    renderer
        .finish()
        .into_iter()
        .flat_map(|line| line.spans)
        .collect()
}

fn table_column_widths(rows: &[TableRow], column_count: usize) -> Vec<usize> {
    let mut widths = vec![1; column_count];
    for row in rows {
        for (index, cell) in row.cells.iter().enumerate() {
            widths[index] = widths[index].max(spans_width(cell));
        }
    }
    widths
}

fn table_border_line(
    left: char,
    middle: char,
    right: char,
    widths: &[usize],
    style: Style,
) -> Line {
    let mut text = String::new();
    text.push(left);
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            text.push(middle);
        }
        text.push_str(&"─".repeat(width.saturating_add(2)));
    }
    text.push(right);
    Line::from_spans(vec![Span::styled(text, style)])
}

fn table_content_line(row: &[Vec<Span>], widths: &[usize], border_style: Style) -> Line {
    let mut spans = vec![Span::styled("│", border_style)];
    for (index, width) in widths.iter().enumerate() {
        spans.push(Span::raw(" "));
        if let Some(cell) = row.get(index) {
            spans.extend(cell.clone());
            let padding = width.saturating_sub(spans_width(cell));
            if padding > 0 {
                spans.push(Span::raw(" ".repeat(padding)));
            }
        } else {
            spans.push(Span::raw(" ".repeat(*width)));
        }
        spans.push(Span::raw(" "));
        spans.push(Span::styled("│", border_style));
    }
    Line::from_spans(spans)
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

fn semantic_markdown_style(container: &Container, theme: MarkdownTheme) -> Option<Style> {
    if container.classes.iter().any(|class| class == "inline-code") {
        Some(theme.inline_code)
    } else if container
        .classes
        .iter()
        .any(|class| class == "markdown-code-block")
    {
        Some(theme.code_block_text)
    } else if container
        .classes
        .iter()
        .any(|class| class == "markdown-link")
    {
        Some(theme.link)
    } else {
        None
    }
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

fn spans_width(spans: &[Span]) -> usize {
    spans
        .iter()
        .map(|span| text_display_width(&span.content))
        .sum()
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
    use super::{MarkdownRenderOptions, MarkdownTheme, render_markdown_lines};
    use bmux_tui::prelude::{Color, Style};

    fn rendered_text(markdown: &str) -> String {
        render_markdown_lines(markdown, MarkdownRenderOptions::new(80))
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

        assert!(output.contains("•  one"));
        assert!(output.contains("•  two"));
    }

    #[test]
    fn renders_code_blocks_preserving_lines() {
        let output = rendered_text("```rust\nfn main() {}\n```");

        assert!(output.contains("fn main() {}"));
    }

    #[test]
    fn renders_code_block_frame_with_language() {
        let output = rendered_text("```rust\nfn main() {}\n```");

        assert!(output.contains("╭─ rust"));
        assert!(output.contains("│ fn main() {}"));
        assert!(output.contains("╰─"));
    }

    #[test]
    fn renders_horizontal_rule() {
        let output = rendered_text("before\n\n---\n\nafter");

        assert!(output.contains("before"));
        assert!(output.contains("────"));
        assert!(output.contains("after"));
    }

    #[test]
    fn renders_table_with_borders() {
        let output = rendered_text("| A | B |\n|---|---|\n| 1 | 2 |");

        assert!(output.contains("┌"));
        assert!(output.contains("│ 1 │ 2 │"));
        assert!(output.contains("└"));
    }

    #[test]
    fn blockquote_wraps_with_bar_on_each_line() {
        let output = render_markdown_lines(
            "> a very long quoted line that should wrap when rendered with the test width",
            MarkdownRenderOptions::new(24),
        )
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

        assert!(output.lines().filter(|line| line.starts_with("│ ")).count() >= 2);
    }

    #[test]
    fn renders_task_list_markers() {
        let output = rendered_text("- [x] done\n- [ ] todo");

        assert!(output.contains("•  ☑ done"));
        assert!(output.contains("•  ☐ todo"));
    }

    #[test]
    fn ordered_list_wraps_with_hanging_indent() {
        let output = render_markdown_lines(
            "1. a very long item that should wrap under the text instead of under the marker",
            MarkdownRenderOptions::new(30),
        )
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

        assert!(
            output
                .lines()
                .next()
                .is_some_and(|line| line.starts_with("1.  "))
        );
        assert!(output.lines().skip(1).any(|line| line.starts_with("    ")));
    }

    #[test]
    fn nested_unordered_list_is_indented() {
        let output = rendered_text("1. parent\n   - child");

        assert!(output.contains("1.  parent"));
        assert!(output.contains("    •  child"));
    }

    #[test]
    fn list_item_code_block_is_indented() {
        let output = rendered_text("1. example\n\n   ```rust\n   fn main() {}\n   ```");

        assert!(output.contains("1.  example"));
        assert!(output.contains("    ╭─ rust"));
        assert!(output.contains("    │ fn main() {}"));
    }

    #[test]
    fn blockquote_inside_list_is_indented() {
        let output = rendered_text("1. note\n\n   > quoted");

        assert!(output.contains("1.  note"));
        assert!(output.contains("    │ quoted"));
    }

    #[test]
    fn custom_theme_styles_link_spans() {
        let theme = MarkdownTheme {
            link: Style::new().fg(Color::Red),
            ..MarkdownTheme::default()
        };
        let lines = render_markdown_lines(
            "[Bcode](https://example.com)",
            MarkdownRenderOptions::new(80).with_theme(theme),
        );

        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .filter(|span| span.style == Style::new().fg(Color::Red))
                .map(|span| span.content.as_str())
                .collect::<String>()
                .contains("Bcode")
        );
    }

    #[test]
    fn custom_theme_styles_code_block_border() {
        let theme = MarkdownTheme {
            code_block_border: Style::new().fg(Color::Magenta),
            ..MarkdownTheme::default()
        };
        let lines = render_markdown_lines(
            "```rust\nfn main() {}\n```",
            MarkdownRenderOptions::new(80).with_theme(theme),
        );

        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content.contains("╭─ rust") && span.style == Style::new().fg(Color::Magenta)
        }));
    }

    #[test]
    fn options_builder_sets_width_streaming_and_theme() {
        let theme = MarkdownTheme {
            horizontal_rule: Style::new().fg(Color::Green),
            ..MarkdownTheme::default()
        };
        let options = MarkdownRenderOptions::new(42)
            .streaming(true)
            .with_theme(theme);

        assert_eq!(options.width, 42);
        assert!(options.streaming);
        assert_eq!(options.theme.horizontal_rule, Style::new().fg(Color::Green));
    }

    #[test]
    fn streaming_partial_code_fence_renders() {
        let output = render_markdown_lines(
            "```rust\nfn main() {",
            MarkdownRenderOptions::new(80).streaming(true),
        )
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

        assert!(output.contains("fn main()"));
    }
}
