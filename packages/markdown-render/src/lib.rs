//! Markdown rendering for Bcode terminal surfaces.
//!
//! This crate uses `hyperchad_markdown` as the Markdown parser and semantic
//! conversion layer, then projects the generated `HyperChad` container tree into
//! `bmux_tui` terminal lines and spans.
//!
//! # Supported Markdown
//!
//! The renderer supports paragraphs, soft and hard line breaks, headings,
//! horizontal rules, blockquotes, fenced and indented code blocks, ordered and
//! unordered lists, task lists, emphasis, strong emphasis, strikethrough,
//! inline code, links, autolinks, images, and GFM tables. Rendering is bounded
//! by terminal display width and preserves Unicode grapheme clusters.
//!
//! # Terminal-specific behavior
//!
//! * All heading levels intentionally share one terminal style.
//! * Tables use borders at widths where they fit and a header-labelled stacked
//!   layout at narrower widths.
//! * Ordered lists restart at one because the parser's container projection
//!   does not retain source start values.
//! * GFM table alignment is preserved for bordered tables; narrow stacked
//!   tables communicate columns through header labels.
//! * Images render as `[image: description]` using alt text, then source, then
//!   a generic fallback.
//! * Dangerous HTML is escaped by parser-level XSS protection; safe raw HTML is
//!   displayed as text.
//! * Footnotes and emoji aliases are intentionally disabled.
//! * Incomplete streaming input uses the same rendering path as finalized input;
//!   incomplete fences, inline syntax, tables, and lists remain best-effort.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::BTreeMap,
    fmt::Write as _,
    hash::{Hash, Hasher},
    ops::Range,
    rc::Rc,
};

use bcode_mermaid_render::{
    MermaidCancellationToken, MermaidRenderRequest, MermaidRenderedOutput, render_mermaid,
};
use bcode_syntax_render::{SyntaxHighlighter, SyntaxPalette};
use bmux_tui::prelude::{Color, Line, Modifier, Span, Style};
use hyperchad_color::Color as HyperChadColor;
use hyperchad_markdown::{MarkdownOptions, markdown_to_container_with_options};
use hyperchad_transformer::{Container, Element, Input};
use hyperchad_transformer_models::{FontWeight, TextDecorationLine};
use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, LinkType,
    Options as ParserOptions, Parser, Tag, TagEnd,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use url::Url;

#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
struct MarkdownRenderStageDiagnostics {
    semantic_parse: u64,
    fallback_projection: u64,
    hyperchad_conversion: u64,
    syntax_highlighting: u64,
    terminal_layout: u64,
    geometry: u64,
    anchors: u64,
    signature: u64,
}

#[cfg(test)]
thread_local! {
    static MARKDOWN_PARSE_COUNT: Cell<usize> = const { Cell::new(0) };
    static MARKDOWN_PROJECTION_COUNT: Cell<usize> = const { Cell::new(0) };
    static MARKDOWN_STAGE_DIAGNOSTICS: RefCell<MarkdownRenderStageDiagnostics> =
        const { RefCell::new(MarkdownRenderStageDiagnostics {
            semantic_parse: 0,
            fallback_projection: 0,
            hyperchad_conversion: 0,
            syntax_highlighting: 0,
            terminal_layout: 0,
            geometry: 0,
            anchors: 0,
            signature: 0,
        }) };
}

#[cfg(test)]
fn record_markdown_stage(
    update: impl FnOnce(&mut MarkdownRenderStageDiagnostics, u64),
    started: std::time::Instant,
) {
    let elapsed = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    MARKDOWN_STAGE_DIAGNOSTICS.with(|diagnostics| update(&mut diagnostics.borrow_mut(), elapsed));
}

#[cfg(test)]
fn reset_markdown_work_counts() {
    MARKDOWN_PARSE_COUNT.set(0);
    MARKDOWN_PROJECTION_COUNT.set(0);
    MARKDOWN_STAGE_DIAGNOSTICS.with(|diagnostics| {
        *diagnostics.borrow_mut() = MarkdownRenderStageDiagnostics::default();
    });
}

#[cfg(test)]
fn markdown_stage_diagnostics() -> MarkdownRenderStageDiagnostics {
    MARKDOWN_STAGE_DIAGNOSTICS.with(|diagnostics| *diagnostics.borrow())
}

#[cfg(test)]
fn markdown_work_counts() -> (usize, usize) {
    (MARKDOWN_PARSE_COUNT.get(), MARKDOWN_PROJECTION_COUNT.get())
}

/// Terminal styles used for Markdown rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// Note alert label and bar style.
    pub alert_note: Style,
    /// Tip alert label and bar style.
    pub alert_tip: Style,
    /// Important alert label and bar style.
    pub alert_important: Style,
    /// Warning alert label and bar style.
    pub alert_warning: Style,
    /// Caution alert label and bar style.
    pub alert_caution: Style,
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
            alert_note: Style::new().fg(Color::Blue),
            alert_tip: Style::new().fg(Color::Green),
            alert_important: Style::new().fg(Color::Magenta),
            alert_warning: Style::new().fg(Color::Yellow),
            alert_caution: Style::new().fg(Color::Red),
            list_marker: muted,
            task_checked: muted,
            task_unchecked: muted,
            table_border: muted,
            horizontal_rule: muted,
        }
    }
}

impl MarkdownTheme {
    const fn alert_style(self, kind: BlockQuoteKind) -> Style {
        match kind {
            BlockQuoteKind::Note => self.alert_note,
            BlockQuoteKind::Tip => self.alert_tip,
            BlockQuoteKind::Important => self.alert_important,
            BlockQuoteKind::Warning => self.alert_warning,
            BlockQuoteKind::Caution => self.alert_caution,
        }
    }
}

/// Trusted context for resolving Markdown destinations.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct MarkdownDocumentContext {
    /// Base URL for URL-relative destinations.
    pub base_url: Option<Url>,
    /// Base filesystem directory for path-relative destinations.
    pub base_directory: Option<std::path::PathBuf>,
    /// Explicit GitHub repository identity for issue references.
    pub github_repository: Option<GitHubRepository>,
}

/// Explicit GitHub repository identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GitHubRepository {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub name: String,
}

/// Classified destination suitable for explicit activation handling.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MarkdownDestination {
    /// Safe HTTP(S) URL.
    Web(Url),
    /// Trusted local filesystem path.
    LocalPath(std::path::PathBuf),
    /// In-document fragment.
    Fragment(String),
    /// Original destination is intentionally omitted to avoid leaking secrets.
    Inert {
        /// Stable reason suitable for UI diagnostics.
        reason: MarkdownDestinationRejection,
    },
    /// Relative destination without trusted context.
    UnresolvedRelative(String),
}

/// Reasons a destination is intentionally inert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarkdownDestinationRejection {
    /// URL scheme is not allowed for activation.
    UnsupportedScheme,
    /// Local path escapes the explicitly trusted base directory.
    OutsideTrustedRoot,
    /// File URL does not map to a local path.
    InvalidFileUrl,
}

/// Resolve a Markdown destination under explicit trusted context.
#[must_use]
pub fn resolve_markdown_destination(
    destination: &str,
    context: Option<&MarkdownDocumentContext>,
) -> MarkdownDestination {
    if let Some(fragment) = destination.strip_prefix('#') {
        return MarkdownDestination::Fragment(fragment.to_owned());
    }
    if let Ok(url) = Url::parse(destination) {
        return match url.scheme() {
            "http" | "https" => MarkdownDestination::Web(url),
            _ => MarkdownDestination::Inert {
                reason: MarkdownDestinationRejection::UnsupportedScheme,
            },
        };
    }
    if let Some(base_url) = context.and_then(|context| context.base_url.as_ref())
        && let Ok(url) = base_url.join(destination)
    {
        return MarkdownDestination::Web(url);
    }
    if let Some(base_directory) = context.and_then(|context| context.base_directory.as_ref()) {
        return resolve_local_destination(destination, base_directory);
    }
    MarkdownDestination::UnresolvedRelative(destination.to_owned())
}

fn resolve_local_destination(
    destination: &str,
    base_directory: &std::path::Path,
) -> MarkdownDestination {
    use std::path::Component;

    let mut relative = std::path::PathBuf::new();
    for component in std::path::Path::new(destination).components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir if relative.pop() => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return MarkdownDestination::Inert {
                    reason: MarkdownDestinationRejection::OutsideTrustedRoot,
                };
            }
        }
    }
    MarkdownDestination::LocalPath(base_directory.join(relative))
}

/// Terminal presentation policy for classified link destinations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MarkdownLinkDestinationPresentation {
    /// Render only the parser-semantic link label.
    #[default]
    LabelOnly,
    /// Append a readable destination while preserving safe redaction.
    ReadableFallback,
}

/// Mermaid contribution behavior for one render.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum MermaidRenderMode {
    /// Preserve highlighted source without a rich contribution request.
    #[default]
    Disabled,
    /// Emit a typed request for asynchronous presentation.
    Contribution,
    /// Render synchronously for explicit non-interactive callers and tests.
    Inline,
}

/// Options controlling terminal Markdown rendering.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarkdownRenderOptions {
    /// Available terminal width in cells.
    pub width: u16,
    /// Theme used for terminal Markdown styles.
    pub theme: MarkdownTheme,
    /// Semantic syntax palette for fenced code blocks.
    pub syntax: Option<SyntaxPalette>,
    /// Trusted context used only to resolve relative destinations.
    pub document_context: Option<MarkdownDocumentContext>,
    /// Mermaid contribution behavior.
    pub mermaid: MermaidRenderMode,
    /// Maximum Mermaid SVG width in pixels.
    pub mermaid_width: u32,
    /// Maximum Mermaid SVG height in pixels.
    pub mermaid_height: u32,
    /// Number of terminal rows reserved for each image contribution.
    pub image_reserved_rows: u16,
    /// Number of terminal rows reserved for each Mermaid contribution.
    pub mermaid_reserved_rows: u16,
    /// Stable identity of the owning document or transcript item.
    pub document_id: Option<String>,
    /// Whether the source is an incomplete streaming document.
    pub streaming: bool,
    /// Terminal projection policy for link destinations.
    pub link_destination_presentation: MarkdownLinkDestinationPresentation,
    /// Whether details blocks are interactive disclosure controls.
    pub details_interactive: bool,
    /// Explicit disclosure state keyed by stable details contribution identity.
    pub details_open: BTreeMap<String, bool>,
}

impl Default for MarkdownRenderOptions {
    /// Create default Markdown render options.
    fn default() -> Self {
        Self {
            width: 80,
            theme: MarkdownTheme::default(),
            syntax: None,
            document_context: None,
            mermaid: MermaidRenderMode::Disabled,
            mermaid_width: 1600,
            mermaid_height: 1200,
            image_reserved_rows: 1,
            mermaid_reserved_rows: 1,
            document_id: None,
            streaming: false,
            link_destination_presentation: MarkdownLinkDestinationPresentation::LabelOnly,
            details_interactive: false,
            details_open: BTreeMap::new(),
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

    /// Return options with a semantic syntax palette for fenced code blocks.
    #[must_use]
    pub const fn with_syntax_palette(mut self, palette: SyntaxPalette) -> Self {
        self.syntax = Some(palette);
        self
    }

    /// Return options with trusted link-resolution context.
    #[must_use]
    pub fn with_document_context(mut self, context: MarkdownDocumentContext) -> Self {
        self.document_context = Some(context);
        self
    }

    /// Return options with a stable owning document or transcript-item identity.
    #[must_use]
    pub fn with_document_id(mut self, document_id: impl Into<String>) -> Self {
        self.document_id = Some(document_id.into());
        self
    }

    /// Return options for an incomplete streaming document.
    #[must_use]
    pub const fn with_streaming(mut self, streaming: bool) -> Self {
        self.streaming = streaming;
        self
    }

    /// Return options with readable link destination fallbacks enabled.
    #[must_use]
    pub const fn with_link_destination_fallbacks(mut self, enabled: bool) -> Self {
        self.link_destination_presentation = if enabled {
            MarkdownLinkDestinationPresentation::ReadableFallback
        } else {
            MarkdownLinkDestinationPresentation::LabelOnly
        };
        self
    }

    /// Return options with explicit details disclosure state.
    #[must_use]
    pub fn with_details_open(mut self, details_open: BTreeMap<String, bool>) -> Self {
        self.details_interactive = true;
        self.details_open = details_open;
        self
    }

    /// Return options with Mermaid contribution requests enabled without rendering inline.
    #[must_use]
    pub const fn with_mermaid_contributions(mut self, width: u32, height: u32) -> Self {
        self.mermaid = MermaidRenderMode::Contribution;
        self.mermaid_width = width;
        self.mermaid_height = height;
        self
    }

    /// Return options with stable rich-presentation row reservations.
    #[must_use]
    pub fn with_rich_reserved_rows(mut self, image_rows: u16, mermaid_rows: u16) -> Self {
        self.image_reserved_rows = image_rows.max(1);
        self.mermaid_reserved_rows = mermaid_rows.max(1);
        self
    }

    /// Return options with bounded synchronous Mermaid rendering enabled.
    #[must_use]
    pub const fn with_mermaid(mut self, width: u32, height: u32) -> Self {
        self.mermaid = MermaidRenderMode::Inline;
        self.mermaid_width = width;
        self.mermaid_height = height;
        self
    }
}

/// Render Markdown into a compact plain-text projection.
///
/// This is intended for bounded one-line previews where full Markdown layout is inappropriate.
/// Semantic text is collected through the Markdown parser rather than by stripping punctuation.
#[must_use]
pub fn markdown_to_plain_text(markdown: &str) -> String {
    let document = parse_markdown_document(markdown);
    let mut output = String::new();
    for event in document.events {
        match event.kind {
            MarkdownSemanticEventKind::Start(_) => {}
            MarkdownSemanticEventKind::End(tag) => {
                if semantic_end_needs_separator(tag) {
                    output.push(' ');
                }
            }
            MarkdownSemanticEventKind::Text(text)
            | MarkdownSemanticEventKind::Code(text)
            | MarkdownSemanticEventKind::InlineMath(text)
            | MarkdownSemanticEventKind::DisplayMath(text) => output.push_str(&text),
            MarkdownSemanticEventKind::Html(html) => {
                collect_html_plain_text(&html, &mut output);
            }
            MarkdownSemanticEventKind::FootnoteReference(label) => {
                output.push_str(" [");
                output.push_str(&label);
                output.push(']');
            }
            MarkdownSemanticEventKind::SoftBreak | MarkdownSemanticEventKind::HardBreak => {
                output.push(' ');
            }
            MarkdownSemanticEventKind::Rule => output.push_str(" — "),
            MarkdownSemanticEventKind::TaskListMarker(checked) => {
                output.push_str(if checked { "checked " } else { "unchecked " });
            }
        }
    }
    normalize_inline_whitespace(&output).trim().to_owned()
}

const fn semantic_end_needs_separator(tag: MarkdownSemanticTagEnd) -> bool {
    matches!(
        tag,
        MarkdownSemanticTagEnd::Paragraph
            | MarkdownSemanticTagEnd::Heading
            | MarkdownSemanticTagEnd::BlockQuote
            | MarkdownSemanticTagEnd::CodeBlock
            | MarkdownSemanticTagEnd::List
            | MarkdownSemanticTagEnd::Item
            | MarkdownSemanticTagEnd::FootnoteDefinition
            | MarkdownSemanticTagEnd::Table
            | MarkdownSemanticTagEnd::TableHead
            | MarkdownSemanticTagEnd::TableRow
            | MarkdownSemanticTagEnd::TableCell
    )
}

fn collect_html_plain_text(html: &str, output: &mut String) {
    let mut in_tag = false;
    for character in html.chars() {
        match character {
            '<' => {
                in_tag = true;
                output.push(' ');
            }
            '>' if in_tag => {
                in_tag = false;
                output.push(' ');
            }
            _ if !in_tag => output.push(character),
            _ => {}
        }
    }
}

/// Semantic Markdown data preserved before terminal projection.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarkdownDocument {
    /// Source-order parser events carrying only Bcode-owned types.
    pub events: Vec<MarkdownSemanticEvent>,
}

/// A semantic Markdown event with its byte range in the original source.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarkdownSemanticEvent {
    /// Bcode-owned semantic event kind.
    pub kind: MarkdownSemanticEventKind,
    /// Byte range in the original Markdown source.
    pub source_range: Range<usize>,
}

/// Bcode-owned semantic events needed by terminal rendering and interaction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MarkdownSemanticEventKind {
    /// Start of a semantic container.
    Start(MarkdownSemanticTag),
    /// End of a semantic container.
    End(MarkdownSemanticTagEnd),
    /// Plain source text.
    Text(String),
    /// Inline code.
    Code(String),
    /// Inline math source without delimiters.
    InlineMath(String),
    /// Display math source without delimiters.
    DisplayMath(String),
    /// Raw HTML source.
    Html(String),
    /// Footnote reference label.
    FootnoteReference(String),
    /// Soft line break.
    SoftBreak,
    /// Hard line break.
    HardBreak,
    /// Thematic break.
    Rule,
    /// Task-list marker state.
    TaskListMarker(bool),
}

/// Semantic container tags independent of the parser implementation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MarkdownSemanticTag {
    /// Paragraph.
    Paragraph,
    /// Heading level from one through six.
    Heading(u8),
    /// Ordinary blockquote or typed GitHub alert.
    BlockQuote(Option<MarkdownAlertKind>),
    /// Fenced or indented code block and optional language identifier.
    CodeBlock(Option<String>),
    /// Raw HTML block.
    HtmlBlock,
    /// Ordered list with its source start, or unordered list when absent.
    List(Option<u64>),
    /// List item.
    Item,
    /// Footnote definition label.
    FootnoteDefinition(String),
    /// Table and per-column alignments.
    Table(Vec<MarkdownTableAlignment>),
    /// Table head.
    TableHead,
    /// Table row.
    TableRow,
    /// Table cell.
    TableCell,
    /// Emphasis.
    Emphasis,
    /// Strong emphasis.
    Strong,
    /// Strikethrough.
    Strikethrough,
    /// Link metadata.
    Link(MarkdownLink),
    /// Image metadata.
    Image(MarkdownImage),
}

/// End tags for semantic containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarkdownSemanticTagEnd {
    /// Paragraph.
    Paragraph,
    /// Heading.
    Heading,
    /// Blockquote or alert.
    BlockQuote,
    /// Code block.
    CodeBlock,
    /// Raw HTML block.
    HtmlBlock,
    /// List.
    List,
    /// List item.
    Item,
    /// Footnote definition.
    FootnoteDefinition,
    /// Table.
    Table,
    /// Table head.
    TableHead,
    /// Table row.
    TableRow,
    /// Table cell.
    TableCell,
    /// Emphasis.
    Emphasis,
    /// Strong emphasis.
    Strong,
    /// Strikethrough.
    Strikethrough,
    /// Link.
    Link,
    /// Image.
    Image,
}

/// GitHub alert kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarkdownAlertKind {
    /// Note.
    Note,
    /// Tip.
    Tip,
    /// Important information.
    Important,
    /// Warning.
    Warning,
    /// Caution.
    Caution,
}

/// GFM table cell alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarkdownTableAlignment {
    /// No explicit alignment.
    None,
    /// Left aligned.
    Left,
    /// Center aligned.
    Center,
    /// Right aligned.
    Right,
}

/// Link metadata preserved independently from its rendered label.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarkdownLink {
    /// Destination exactly as parsed.
    pub destination: String,
    /// Optional title.
    pub title: Option<String>,
    /// Reference identifier when present.
    pub reference_id: Option<String>,
    /// Link syntax kind.
    pub kind: MarkdownLinkKind,
}

/// Image metadata preserved independently from its alt-text events.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarkdownImage {
    /// Image source exactly as parsed.
    pub source: String,
    /// Optional title.
    pub title: Option<String>,
    /// Reference identifier when present.
    pub reference_id: Option<String>,
    /// Image link syntax kind.
    pub kind: MarkdownLinkKind,
}

/// Parser-neutral link syntax kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarkdownLinkKind {
    /// Inline destination.
    Inline,
    /// Reference destination.
    Reference,
    /// Collapsed reference.
    Collapsed,
    /// Shortcut reference.
    Shortcut,
    /// Automatic URL link.
    Autolink,
    /// Automatic email link.
    Email,
    /// Wiki-style link.
    WikiLink,
    /// Unresolved reference form.
    Unresolved,
}

/// Parse Markdown into Bcode-owned source-order semantic events.
#[must_use]
pub fn parse_markdown_document(markdown: &str) -> MarkdownDocument {
    #[cfg(test)]
    MARKDOWN_PARSE_COUNT.set(MARKDOWN_PARSE_COUNT.get().saturating_add(1));
    let mut options = ParserOptions::empty();
    options.insert(ParserOptions::ENABLE_TABLES);
    options.insert(ParserOptions::ENABLE_STRIKETHROUGH);
    options.insert(ParserOptions::ENABLE_TASKLISTS);
    options.insert(ParserOptions::ENABLE_FOOTNOTES);
    options.insert(ParserOptions::ENABLE_SMART_PUNCTUATION);
    options.insert(ParserOptions::ENABLE_HEADING_ATTRIBUTES);
    options.insert(ParserOptions::ENABLE_GFM);
    options.insert(ParserOptions::ENABLE_MATH);

    MarkdownDocument {
        events: Parser::new_ext(markdown, options)
            .into_offset_iter()
            .filter_map(|(event, source_range)| {
                semantic_event(event).map(|kind| MarkdownSemanticEvent { kind, source_range })
            })
            .collect(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarkdownSelectionProvenance {
    /// Original UTF-8 source ranges in canonical source order.
    pub source_ranges: Vec<Range<usize>>,
    /// Post-layout document-relative cells when this unit has truthful visible geometry.
    pub rects: Vec<MarkdownCellRect>,
    /// Optional semantic identity for whole-unit expansion.
    pub semantic_expansion: Option<String>,
    /// Parser semantic kind represented by the ranges.
    pub kind: MarkdownSelectionKind,
}

/// Exact canonical source ranges for one rendered code block.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarkdownCodeBlockSelection {
    /// Stable identity shared by code-body provenance entries.
    pub id: String,
    /// Entire original block, including indentation or fences.
    pub whole_range: Range<usize>,
    /// Original opening fence and info line, absent for indented blocks.
    pub header_range: Option<Range<usize>>,
    /// Ordered canonical code-body slices. Indented slices retain source indentation.
    pub body_ranges: Vec<Range<usize>>,
    /// Original closing fence, absent for indented or incomplete fenced blocks.
    pub footer_range: Option<Range<usize>>,
    /// Source representation used by the block.
    pub kind: MarkdownCodeBlockSelectionKind,
}

/// Canonical Markdown code-block representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarkdownCodeBlockSelectionKind {
    /// Backtick- or tilde-fenced block.
    Fenced,
    /// Four-space or tab-indented block.
    Indented,
}

/// Parser-neutral semantic kind for one selectable Markdown source unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarkdownSelectionKind {
    /// Opening semantic delimiter or container boundary.
    Start,
    /// Closing semantic delimiter or container boundary.
    End,
    /// Visible text.
    Text,
    /// Inline or block code content.
    Code,
    /// Soft source break.
    SoftBreak,
    /// Explicit hard break.
    HardBreak,
    /// Other truthful parser event with source bytes.
    Other,
}

#[must_use]
pub fn markdown_code_block_selections(source: &str) -> Vec<MarkdownCodeBlockSelection> {
    let lines = source_lines_with_offsets(source);
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let (line_start, line_end, content_end) = lines[index];
        let line = &source[line_start..content_end];
        if let Some((marker, marker_len)) = opening_fence(line) {
            let id = format!("code-block:{line_start}");
            let body_start = line_end;
            let mut footer = None;
            let mut cursor = index.saturating_add(1);
            while cursor < lines.len() {
                let (candidate_start, candidate_end, candidate_content_end) = lines[cursor];
                if closing_fence(
                    &source[candidate_start..candidate_content_end],
                    marker,
                    marker_len,
                ) {
                    footer = Some(candidate_start..candidate_end);
                    break;
                }
                cursor = cursor.saturating_add(1);
            }
            let body_end = footer.as_ref().map_or(source.len(), |range| range.start);
            let whole_end = footer.as_ref().map_or(source.len(), |range| range.end);
            blocks.push(MarkdownCodeBlockSelection {
                id,
                whole_range: line_start..whole_end,
                header_range: Some(line_start..line_end),
                body_ranges: (body_start < body_end)
                    .then_some(body_start..body_end)
                    .into_iter()
                    .collect(),
                footer_range: footer,
                kind: MarkdownCodeBlockSelectionKind::Fenced,
            });
            index = if cursor < lines.len() {
                cursor.saturating_add(1)
            } else {
                lines.len()
            };
            continue;
        }
        if is_indented_code_line(line) {
            let start_index = index;
            let mut cursor = index.saturating_add(1);
            while cursor < lines.len() {
                let (candidate_start, _, candidate_content_end) = lines[cursor];
                let candidate = &source[candidate_start..candidate_content_end];
                if !(candidate.is_empty() || is_indented_code_line(candidate)) {
                    break;
                }
                cursor = cursor.saturating_add(1);
            }
            let whole_start = lines[start_index].0;
            let whole_end = lines[cursor.saturating_sub(1)].1;
            blocks.push(MarkdownCodeBlockSelection {
                id: format!("code-block:{whole_start}"),
                whole_range: whole_start..whole_end,
                header_range: None,
                body_ranges: lines[start_index..cursor]
                    .iter()
                    .filter_map(|(start, end, content_end)| {
                        (*start < *content_end).then_some(*start..*end)
                    })
                    .collect(),
                footer_range: None,
                kind: MarkdownCodeBlockSelectionKind::Indented,
            });
            index = cursor;
            continue;
        }
        index = index.saturating_add(1);
    }
    blocks
}

fn source_lines_with_offsets(source: &str) -> Vec<(usize, usize, usize)> {
    let mut offset = 0_usize;
    let mut lines = source
        .split_inclusive('\n')
        .map(|line| {
            let start = offset;
            offset = offset.saturating_add(line.len());
            let content_end = offset
                .saturating_sub(usize::from(line.ends_with('\n')))
                .saturating_sub(usize::from(line.ends_with("\r\n")));
            (start, offset, content_end)
        })
        .collect::<Vec<_>>();
    if source.is_empty() || source.ends_with('\n') {
        lines.push((offset, offset, offset));
    }
    lines
}

fn opening_fence(line: &str) -> Option<(u8, usize)> {
    let bytes = line.as_bytes();
    let indent = bytes.iter().take_while(|byte| **byte == b' ').count();
    if indent > 3 {
        return None;
    }
    let marker = *bytes.get(indent)?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let count = bytes[indent..]
        .iter()
        .take_while(|byte| **byte == marker)
        .count();
    (count >= 3).then_some((marker, count))
}

fn closing_fence(line: &str, marker: u8, minimum_len: usize) -> bool {
    let bytes = line.as_bytes();
    let indent = bytes.iter().take_while(|byte| **byte == b' ').count();
    if indent > 3 {
        return false;
    }
    let count = bytes[indent..]
        .iter()
        .take_while(|byte| **byte == marker)
        .count();
    count >= minimum_len
        && bytes[indent.saturating_add(count)..]
            .iter()
            .all(u8::is_ascii_whitespace)
}

fn is_indented_code_line(line: &str) -> bool {
    line.starts_with("    ") || line.starts_with('\t')
}

fn semantic_expansion_id(event: &MarkdownSemanticEvent) -> Option<String> {
    match &event.kind {
        MarkdownSemanticEventKind::Start(MarkdownSemanticTag::CodeBlock(_)) => Some(format!(
            "code-block:{}:{}",
            event.source_range.start, event.source_range.end
        )),
        _ => None,
    }
}

fn markdown_selection_provenance(document: &MarkdownDocument) -> Vec<MarkdownSelectionProvenance> {
    document
        .events
        .iter()
        .filter(|event| event.source_range.start <= event.source_range.end)
        .map(|event| MarkdownSelectionProvenance {
            source_ranges: vec![event.source_range.clone()],
            rects: Vec::new(),
            semantic_expansion: semantic_expansion_id(event),
            kind: match &event.kind {
                MarkdownSemanticEventKind::Start(_) => MarkdownSelectionKind::Start,
                MarkdownSemanticEventKind::End(_) => MarkdownSelectionKind::End,
                MarkdownSemanticEventKind::Text(_) => MarkdownSelectionKind::Text,
                MarkdownSemanticEventKind::Code(_) => MarkdownSelectionKind::Code,
                MarkdownSemanticEventKind::SoftBreak => MarkdownSelectionKind::SoftBreak,
                MarkdownSemanticEventKind::HardBreak => MarkdownSelectionKind::HardBreak,
                _ => MarkdownSelectionKind::Other,
            },
        })
        .collect()
}

fn assign_selection_geometry(
    provenance: Vec<MarkdownSelectionProvenance>,
    document: &MarkdownDocument,
    source: &str,
    lines: &[Line],
) -> Vec<MarkdownSelectionProvenance> {
    let display = DisplayedMarkdown::new(lines);
    let mut search_from = 0;
    let mut projected = Vec::new();
    for (unit, event) in provenance.into_iter().zip(&document.events) {
        let (MarkdownSemanticEventKind::Text(visible) | MarkdownSemanticEventKind::Code(visible)) =
            &event.kind
        else {
            projected.push(unit);
            continue;
        };
        let Some(canonical) = source.get(event.source_range.clone()) else {
            projected.push(unit);
            continue;
        };
        if canonical != visible {
            projected.push(unit);
            continue;
        }
        let mut source_offset = event.source_range.start;
        for grapheme in visible.graphemes(true) {
            let source_range = source_offset..source_offset.saturating_add(grapheme.len());
            source_offset = source_range.end;
            if matches!(grapheme, "\n" | "\r\n") {
                projected.push(MarkdownSelectionProvenance {
                    source_ranges: vec![source_range],
                    rects: Vec::new(),
                    semantic_expansion: unit.semantic_expansion.clone(),
                    kind: unit.kind,
                });
                continue;
            }
            let Some((display_range, rects)) = display.find_geometry(grapheme, search_from) else {
                projected.push(MarkdownSelectionProvenance {
                    source_ranges: vec![source_range],
                    rects: Vec::new(),
                    semantic_expansion: unit.semantic_expansion.clone(),
                    kind: unit.kind,
                });
                continue;
            };
            search_from = display_range.end;
            projected.push(MarkdownSelectionProvenance {
                source_ranges: vec![source_range],
                rects,
                semantic_expansion: unit.semantic_expansion.clone(),
                kind: unit.kind,
            });
        }
    }
    projected
}

/// Rendered Markdown lines plus semantic contributions for richer consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownRenderResult {
    /// Terminal text projection.
    pub lines: Vec<Line>,
    /// Renderer-owned source provenance available to selection consumers.
    pub selection_provenance: Vec<MarkdownSelectionProvenance>,
    /// Exact canonical fenced and indented code-block ranges.
    pub code_block_selections: Vec<MarkdownCodeBlockSelection>,
    /// Source-order semantic contributions independent of TUI event-loop types.
    pub contributions: Vec<MarkdownContribution>,
    /// Post-layout document-relative cell rectangles for rich contributions.
    pub geometry: Vec<MarkdownContributionGeometry>,
    /// Post-layout document anchors for internal fragment navigation.
    pub anchors: Vec<MarkdownDocumentAnchor>,
    /// Layout signature including every renderer-owned layout-affecting option.
    pub layout_signature: String,
}

/// Stable internal document anchor and its rendered row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarkdownDocumentAnchor {
    /// GitHub-compatible fragment identifier without the leading `#`.
    pub fragment: String,
    /// Zero-based rendered document row.
    pub row: u16,
}

/// Document-relative terminal-cell rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MarkdownCellRect {
    /// Zero-based terminal column.
    pub x: u16,
    /// Zero-based rendered row.
    pub y: u16,
    /// Width in terminal cells.
    pub width: u16,
    /// Height in terminal rows.
    pub height: u16,
}

/// Stable contribution identity and its post-layout rectangles.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarkdownContributionGeometry {
    /// Stable contribution identity matching [`MarkdownContribution::id`].
    pub contribution_id: String,
    /// One rectangle per contiguous visual-row segment.
    pub rects: Vec<MarkdownCellRect>,
}

/// Stable semantic contribution emitted alongside terminal lines.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarkdownContribution {
    /// Stable identity derived from semantic kind and source byte range.
    pub id: String,
    /// Original source byte range.
    pub source_range: Range<usize>,
    /// Contribution payload.
    pub kind: MarkdownContributionKind,
}

/// Rich Markdown contribution payload.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MarkdownContributionKind {
    /// Link and its semantic label.
    Link {
        /// Link metadata.
        link: MarkdownLink,
        /// Parser-semantic visible label.
        label: String,
        /// Safe classified activation destination.
        destination: MarkdownDestination,
    },
    /// Image and its alt text.
    Image {
        /// Image metadata.
        image: MarkdownImage,
        /// Parser-semantic alt text.
        alt: String,
        /// Classified source; unsafe schemes are inert.
        source: MarkdownDestination,
        /// Safe classified destination when the image is nested in a link.
        linked_destination: Option<MarkdownDestination>,
    },
    /// Semantic target ID for the matching definition.
    FootnoteReference {
        /// Source footnote label.
        label: String,
        /// Stable target contribution ID.
        target_id: String,
    },
    /// Footnote definition.
    FootnoteDefinition {
        /// Source footnote label.
        label: String,
        /// Stable IDs of references targeting this definition.
        reference_ids: Vec<String>,
    },
    /// Safe details/disclosure block.
    Details {
        /// Summary Markdown after safe HTML-tag conversion.
        summary: String,
        /// Body Markdown.
        body: String,
        /// Whether the source includes the `open` attribute.
        default_open: bool,
    },
    /// GitHub issue reference resolved under explicit repository context.
    GitHubIssue {
        /// Repository identity.
        repository: GitHubRepository,
        /// Issue number.
        number: u64,
        /// Safe issue URL.
        destination: MarkdownDestination,
        /// Whether the reference follows a closing keyword.
        closes: bool,
    },
    /// Inline math source.
    InlineMath {
        /// Math source without delimiters.
        source: String,
    },
    /// Display math source.
    DisplayMath {
        /// Math source without delimiters.
        source: String,
    },
    /// Mermaid fenced source.
    Mermaid {
        /// Mermaid source without the fence.
        source: String,
        /// Versioned stable renderer cache key.
        cache_key: String,
        /// Backend outcome when Mermaid rendering was requested.
        rendering: MermaidContributionRendering,
    },
}

/// Backend-neutral Mermaid contribution rendering state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MermaidContributionRendering {
    /// Rendering is disabled; highlighted source remains the fallback.
    Disabled,
    /// Bcode-owned SVG bytes are available for image presentation.
    Svg(Vec<u8>),
    /// Rendering failed; source remains visible with a stable diagnostic.
    Failed(String),
}

/// Retained exact renderer state for append-only streaming Markdown.
///
/// The state incrementally reuses completed plain-paragraph blocks. Sources
/// containing Markdown constructs with retroactive or container semantics fall
/// back to a fresh full render. The retained state is presentation-only and is
/// bounded by the current source and rendered stable prefix.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarkdownStreamingRenderState {
    source: String,
    options: Option<MarkdownRenderOptions>,
    stable_source_len: usize,
    stable_lines: Vec<Line>,
    incremental_eligible: bool,
    processed_source_bytes: usize,
    result: Option<MarkdownRenderResult>,
}

impl MarkdownStreamingRenderState {
    /// Render the latest streaming source, reusing exact stable paragraph rows
    /// when the source is an eligible append and all render options match.
    #[must_use]
    pub fn render(
        &mut self,
        markdown: &str,
        options: &MarkdownRenderOptions,
    ) -> MarkdownRenderResult {
        let can_append = options.streaming
            && self.incremental_eligible
            && self.options.as_ref() == Some(options)
            && markdown.starts_with(&self.source)
            && plain_paragraph_append_is_eligible(&self.source, &markdown[self.source.len()..]);
        if !can_append {
            return self.replace_with_full_render(markdown, options);
        }

        let checkpoint = completed_plain_paragraph_boundary(&self.source)
            .unwrap_or(self.stable_source_len)
            .max(self.stable_source_len);
        if checkpoint > self.stable_source_len {
            let Some(stable_segment) = self.source.get(self.stable_source_len..checkpoint) else {
                return self.replace_with_full_render(markdown, options);
            };
            let segment = render_markdown(stable_segment, options);
            self.processed_source_bytes = self
                .processed_source_bytes
                .saturating_add(stable_segment.len());
            append_markdown_blocks(&mut self.stable_lines, &segment.lines);
            self.stable_source_len = checkpoint;
        }

        let Some(suffix) = markdown.get(self.stable_source_len..) else {
            return self.replace_with_full_render(markdown, options);
        };
        let suffix_result = render_markdown(suffix, options);
        self.processed_source_bytes = self.processed_source_bytes.saturating_add(suffix.len());
        if !suffix_result.contributions.is_empty() || !suffix_result.anchors.is_empty() {
            return self.replace_with_full_render(markdown, options);
        }
        let mut lines = self.stable_lines.clone();
        append_markdown_blocks(&mut lines, &suffix_result.lines);
        let document = parse_markdown_document(markdown);
        let selection_provenance = assign_selection_geometry(
            markdown_selection_provenance(&document),
            &document,
            markdown,
            &lines,
        );
        let result = MarkdownRenderResult {
            lines,
            selection_provenance,
            code_block_selections: markdown_code_block_selections(markdown),
            contributions: Vec::new(),
            geometry: Vec::new(),
            anchors: Vec::new(),
            layout_signature: markdown_layout_signature(markdown, options, &[], &[]),
        };
        self.source.clear();
        self.source.push_str(markdown);
        self.result = Some(result.clone());
        result
    }

    /// Return cumulative source bytes parsed by this retained state.
    #[cfg(test)]
    #[must_use]
    const fn processed_source_bytes(&self) -> usize {
        self.processed_source_bytes
    }

    fn replace_with_full_render(
        &mut self,
        markdown: &str,
        options: &MarkdownRenderOptions,
    ) -> MarkdownRenderResult {
        let result = render_markdown(markdown, options);
        self.source.clear();
        self.source.push_str(markdown);
        self.options = Some(options.clone());
        self.stable_source_len = 0;
        self.stable_lines.clear();
        self.incremental_eligible =
            options.streaming && plain_paragraph_append_is_eligible("", markdown);
        self.processed_source_bytes = self.processed_source_bytes.saturating_add(markdown.len());
        self.result = Some(result.clone());
        result
    }
}

fn append_markdown_blocks(lines: &mut Vec<Line>, appended: &[Line]) {
    if !lines.is_empty() && !appended.is_empty() {
        lines.push(Line::default());
    }
    lines.extend(appended.iter().cloned());
}

fn completed_plain_paragraph_boundary(source: &str) -> Option<usize> {
    source.rmatch_indices("\n\n").next().map(|(index, _)| index)
}

fn plain_paragraph_append_is_eligible(previous: &str, appended: &str) -> bool {
    let mut at_line_start = previous.is_empty() || previous.ends_with('\n');
    let mut ordered_prefix = true;
    for character in appended.chars() {
        if matches!(
            character,
            '#' | '*' | '_' | '[' | ']' | '<' | '>' | '|' | '`' | '\\'
        ) {
            return false;
        }
        if at_line_start {
            if matches!(character, '-' | '+' | '>') {
                return false;
            }
            ordered_prefix = character.is_ascii_digit();
            at_line_start = character == '\n';
            continue;
        }
        if ordered_prefix {
            if character == '.' {
                return false;
            }
            ordered_prefix = character.is_ascii_digit();
        }
        at_line_start = character == '\n';
    }
    true
}

/// Render Markdown into terminal lines and semantic contributions.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn render_markdown(markdown: &str, options: &MarkdownRenderOptions) -> MarkdownRenderResult {
    #[cfg(test)]
    let semantic_started = std::time::Instant::now();
    let document = parse_markdown_document(markdown);
    let mut selection_provenance = markdown_selection_provenance(&document);
    let details = collect_details_projections(markdown);
    let contributions = markdown_contributions(
        &document,
        options.document_context.as_ref(),
        options.document_id.as_deref(),
        &details,
        options,
    );
    #[cfg(test)]
    record_markdown_stage(
        |diagnostics, elapsed| diagnostics.semantic_parse = elapsed,
        semantic_started,
    );
    #[cfg(test)]
    let fallback_started = std::time::Instant::now();
    let destination_projection =
        project_link_destination_fallbacks(markdown, &contributions, options);
    let destination_unchanged = matches!(destination_projection, Cow::Borrowed(_));
    let projected_source = destination_projection.as_ref();
    let projected_document = if destination_unchanged {
        document.clone()
    } else {
        parse_markdown_document(projected_source)
    };
    let projected_details_storage;
    let projected_details = if destination_unchanged {
        details.as_slice()
    } else {
        projected_details_storage = collect_details_projections(projected_source);
        projected_details_storage.as_slice()
    };
    let mut projection_options = options.clone();
    if !destination_unchanged {
        for (original, projected) in details.iter().zip(projected_details) {
            let original_id = details_contribution_id(original, options.document_id.as_deref());
            if let Some(open) = options.details_open.get(&original_id) {
                let projected_id =
                    details_contribution_id(projected, options.document_id.as_deref());
                projection_options.details_open.insert(projected_id, *open);
            }
        }
    }
    let projection = project_semantic_fallbacks(
        projected_source,
        &projected_document,
        projected_details,
        &projection_options,
    );
    let mut projection = projection;
    assign_projected_contribution_ids(&mut projection.contributions, &contributions);
    let projected_markdown = if options.streaming {
        let details_projection = project_incomplete_details_fallback(&projection.markdown);
        Cow::Owned(project_incomplete_unsafe_destination_fallback(&details_projection).into_owned())
    } else {
        Cow::Borrowed(projection.markdown.as_ref())
    };
    #[cfg(test)]
    record_markdown_stage(
        |diagnostics, elapsed| diagnostics.fallback_projection = elapsed,
        fallback_started,
    );
    #[cfg(test)]
    let projection_started = std::time::Instant::now();
    let final_document = if projected_markdown.as_ref() == projected_source {
        projected_document.clone()
    } else {
        parse_markdown_document(&projected_markdown)
    };
    let (lines, mut geometry, heading_rows) =
        render_markdown_projection(&projected_markdown, options);
    if projected_markdown.as_ref() == markdown {
        selection_provenance =
            assign_selection_geometry(selection_provenance, &document, markdown, &lines);
    }
    #[cfg(test)]
    record_markdown_stage(
        |diagnostics, elapsed| {
            diagnostics.hyperchad_conversion = elapsed;
            diagnostics.terminal_layout = elapsed;
            diagnostics.syntax_highlighting = elapsed;
        },
        projection_started,
    );
    #[cfg(test)]
    let geometry_started = std::time::Instant::now();
    geometry.extend(projected_contribution_geometry(
        &projected_markdown,
        &lines,
        &projection.contributions,
    ));
    let geometry = assign_contribution_geometry(geometry, &contributions);
    #[cfg(test)]
    record_markdown_stage(
        |diagnostics, elapsed| diagnostics.geometry = elapsed,
        geometry_started,
    );
    #[cfg(test)]
    let anchors_started = std::time::Instant::now();
    let anchors = markdown_document_anchors(&projected_markdown, &final_document, &heading_rows);
    #[cfg(test)]
    record_markdown_stage(
        |diagnostics, elapsed| diagnostics.anchors = elapsed,
        anchors_started,
    );
    #[cfg(test)]
    let signature_started = std::time::Instant::now();
    let layout_signature = markdown_layout_signature(markdown, options, &contributions, &geometry);
    #[cfg(test)]
    record_markdown_stage(
        |diagnostics, elapsed| diagnostics.signature = elapsed,
        signature_started,
    );
    MarkdownRenderResult {
        lines,
        selection_provenance,
        code_block_selections: markdown_code_block_selections(markdown),
        contributions,
        geometry,
        anchors,
        layout_signature,
    }
}

fn markdown_document_anchors(
    source: &str,
    document: &MarkdownDocument,
    heading_rows: &[u16],
) -> Vec<MarkdownDocumentAnchor> {
    let mut headings = Vec::new();
    let mut active: Option<(String, Option<String>)> = None;
    for event in &document.events {
        match &event.kind {
            MarkdownSemanticEventKind::Start(MarkdownSemanticTag::Heading(_)) => {
                active = Some((
                    String::new(),
                    heading_explicit_id(source, event.source_range.start),
                ));
            }
            MarkdownSemanticEventKind::Text(text) | MarkdownSemanticEventKind::Code(text) => {
                if let Some((label, _)) = &mut active {
                    label.push_str(text);
                }
            }
            MarkdownSemanticEventKind::End(MarkdownSemanticTagEnd::Heading) => {
                if let Some((label, explicit)) = active.take() {
                    let fragment = explicit.unwrap_or_else(|| github_heading_slug(&label));
                    if !fragment.is_empty() {
                        headings.push(fragment);
                    }
                }
            }
            _ => {}
        }
    }
    let mut duplicate_counts = BTreeMap::<String, usize>::new();
    headings
        .into_iter()
        .zip(heading_rows.iter().copied())
        .map(|(fragment, row)| {
            let count = duplicate_counts.entry(fragment.clone()).or_default();
            let fragment = if *count == 0 {
                fragment
            } else {
                format!("{fragment}-{count}")
            };
            *count = count.saturating_add(1);
            MarkdownDocumentAnchor { fragment, row }
        })
        .collect()
}

fn heading_explicit_id(source: &str, start: usize) -> Option<String> {
    let line = source.get(start..)?.lines().next()?;
    let attributes = line.rsplit_once(" {#")?.1.strip_suffix('}')?;
    (!attributes.is_empty()).then(|| attributes.to_owned())
}

fn github_heading_slug(label: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for character in label.chars().flat_map(char::to_lowercase) {
        if character.is_alphanumeric() || character == '_' || character == '-' {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(character);
        } else if character.is_whitespace() {
            pending_dash = true;
        }
    }
    slug
}

fn project_link_destination_fallbacks<'a>(
    markdown: &'a str,
    contributions: &[MarkdownContribution],
    options: &MarkdownRenderOptions,
) -> Cow<'a, str> {
    if options.link_destination_presentation
        != MarkdownLinkDestinationPresentation::ReadableFallback
    {
        return Cow::Borrowed(markdown);
    }
    let replacements = contributions
        .iter()
        .filter_map(|contribution| match &contribution.kind {
            MarkdownContributionKind::Link {
                link,
                label,
                destination,
            } => readable_destination_fallback(link, label, destination).and_then(|fallback| {
                markdown
                    .get(contribution.source_range.clone())
                    .map(|source| {
                        (
                            contribution.source_range.clone(),
                            format!("{source} ({fallback})"),
                        )
                    })
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    if replacements.is_empty() {
        Cow::Borrowed(markdown)
    } else {
        Cow::Owned(replace_source_ranges(markdown, replacements))
    }
}

fn readable_destination_fallback(
    link: &MarkdownLink,
    label: &str,
    destination: &MarkdownDestination,
) -> Option<String> {
    let fallback = match destination {
        MarkdownDestination::Web(url) => url.as_str(),
        MarkdownDestination::LocalPath(path) => return Some(path.to_string_lossy().into_owned()),
        MarkdownDestination::Fragment(fragment) => return Some(format!("#{fragment}")),
        MarkdownDestination::UnresolvedRelative(value) => value,
        MarkdownDestination::Inert { .. } => return None,
    };
    (fallback != label && fallback != link.destination.trim())
        .then(|| fallback.to_owned())
        .or_else(|| (fallback != label).then(|| fallback.to_owned()))
}

fn assign_projected_contribution_ids(
    projected: &mut [ProjectedContributionRange],
    contributions: &[MarkdownContribution],
) {
    let mut details = contributions.iter().filter(|contribution| {
        matches!(contribution.kind, MarkdownContributionKind::Details { .. })
    });
    let mut references = contributions.iter().filter(|contribution| {
        matches!(
            contribution.kind,
            MarkdownContributionKind::FootnoteReference { .. }
        )
    });
    let mut definitions = contributions.iter().filter(|contribution| {
        matches!(
            contribution.kind,
            MarkdownContributionKind::FootnoteDefinition { .. }
        )
    });
    for item in projected {
        item.contribution_id = match item.kind {
            ProjectedContributionKind::Details => details.next(),
            ProjectedContributionKind::FootnoteReference => references.next(),
            ProjectedContributionKind::FootnoteDefinition => definitions.next(),
        }
        .map(|contribution| contribution.id.clone());
    }
}

fn projected_contribution_geometry(
    markdown: &str,
    lines: &[Line],
    projected: &[ProjectedContributionRange],
) -> Vec<MarkdownContributionGeometry> {
    let display = DisplayedMarkdown::new(lines);
    let mut search_from = 0;
    let mut details_from = 0;
    projected
        .iter()
        .filter_map(|item| {
            let visible = match item.kind {
                ProjectedContributionKind::Details => {
                    let target = markdown.get(item.range.clone())?;
                    if !target.contains("▶ **") && !target.contains("▼ **") {
                        target.replace("**", "")
                    } else {
                        let suffix = markdown.get(details_from..)?;
                        let (marker_offset, marker) = [
                            suffix.find("▶ **").map(|offset| (offset, "▶ ")),
                            suffix.find("▼ **").map(|offset| (offset, "▼ ")),
                        ]
                        .into_iter()
                        .flatten()
                        .min_by_key(|(offset, _)| *offset)?;
                        let summary_start = marker_offset.saturating_add("▶ **".len());
                        let summary_len = suffix.get(summary_start..)?.find("**")?;
                        details_from = details_from
                            .saturating_add(summary_start)
                            .saturating_add(summary_len)
                            .saturating_add(2);
                        format!(
                            "{marker}{}",
                            suffix.get(summary_start..summary_start.saturating_add(summary_len))?
                        )
                    }
                }
                ProjectedContributionKind::FootnoteReference
                | ProjectedContributionKind::FootnoteDefinition => markdown
                    .get(item.range.clone())?
                    .replace("**", "")
                    .replace('`', ""),
            };
            let (range, rects) = display.find_geometry(&visible, search_from)?;
            search_from = range.end;
            Some(MarkdownContributionGeometry {
                contribution_id: item.contribution_id.clone().unwrap_or_else(|| {
                    match item.kind {
                        ProjectedContributionKind::Details => "details",
                        ProjectedContributionKind::FootnoteReference => "footnote-reference",
                        ProjectedContributionKind::FootnoteDefinition => "footnote-definition",
                    }
                    .to_owned()
                }),
                rects,
            })
        })
        .collect()
}

#[derive(Debug)]
struct DisplayedMarkdown {
    text: String,
    cells: Vec<DisplayedMarkdownCell>,
}

#[derive(Debug, Clone, Copy)]
struct DisplayedMarkdownCell {
    byte_start: usize,
    byte_end: usize,
    row: usize,
    column: usize,
    width: usize,
}

impl DisplayedMarkdown {
    fn new(lines: &[Line]) -> Self {
        let mut text = String::new();
        let mut cells = Vec::new();
        for (row, line) in lines.iter().enumerate() {
            let mut column = 0;
            for span in &line.spans {
                for grapheme in span.content.graphemes(true) {
                    let byte_start = text.len();
                    text.push_str(grapheme);
                    let byte_end = text.len();
                    let width = text_display_width(grapheme);
                    cells.push(DisplayedMarkdownCell {
                        byte_start,
                        byte_end,
                        row,
                        column,
                        width,
                    });
                    column = column.saturating_add(width);
                }
            }
        }
        Self { text, cells }
    }

    fn find_geometry(
        &self,
        target: &str,
        from: usize,
    ) -> Option<(Range<usize>, Vec<MarkdownCellRect>)> {
        if target.is_empty() {
            return None;
        }
        let start = self
            .text
            .get(from..)
            .and_then(|suffix| {
                suffix
                    .find(target)
                    .map(|offset| from.saturating_add(offset))
            })
            .or_else(|| self.text.find(target))?;
        let range = start..start.saturating_add(target.len());
        let mut rects: Vec<MarkdownCellRect> = Vec::new();
        for cell in self.cells.iter().filter(|cell| {
            cell.byte_start < range.end && range.start < cell.byte_end && cell.width > 0
        }) {
            if let Some(last) = rects.last_mut()
                && usize::from(last.y) == cell.row
                && usize::from(last.x.saturating_add(last.width)) == cell.column
            {
                last.width = last
                    .width
                    .saturating_add(u16::try_from(cell.width).unwrap_or(u16::MAX));
                continue;
            }
            rects.push(markdown_cell_rect(cell.column, cell.row, cell.width));
        }
        (!rects.is_empty()).then_some((range, rects))
    }
}

fn assign_contribution_geometry(
    mut geometry: Vec<MarkdownContributionGeometry>,
    contributions: &[MarkdownContribution],
) -> Vec<MarkdownContributionGeometry> {
    let contribution_ids = contributions
        .iter()
        .map(|contribution| contribution.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let has_projected_details = geometry
        .iter()
        .any(|item| item.contribution_id == "details");
    let mut links = contributions.iter().filter_map(|contribution| {
        matches!(contribution.kind, MarkdownContributionKind::Link { .. })
            .then_some(contribution.id.as_str())
    });
    let mut images = contributions.iter().filter_map(|contribution| {
        matches!(contribution.kind, MarkdownContributionKind::Image { .. })
            .then_some(contribution.id.as_str())
    });
    let mut details = contributions.iter().filter_map(|contribution| {
        matches!(contribution.kind, MarkdownContributionKind::Details { .. })
            .then_some(contribution.id.as_str())
    });
    let mut footnote_references = contributions.iter().filter_map(|contribution| {
        matches!(
            contribution.kind,
            MarkdownContributionKind::FootnoteReference { .. }
        )
        .then_some(contribution.id.as_str())
    });
    let mut footnote_definitions = contributions.iter().filter_map(|contribution| {
        matches!(
            contribution.kind,
            MarkdownContributionKind::FootnoteDefinition { .. }
        )
        .then_some(contribution.id.as_str())
    });
    let mut mermaid = contributions.iter().filter_map(|contribution| {
        matches!(contribution.kind, MarkdownContributionKind::Mermaid { .. })
            .then_some(contribution.id.as_str())
    });
    geometry.retain_mut(|item| {
        if contribution_ids.contains(item.contribution_id.as_str()) {
            return true;
        }
        let id = match item.contribution_id.as_str() {
            "link" if has_projected_details => links.next(),
            "link" => links.next().or_else(|| details.next()),
            "image" => images.next(),
            "details" => details.next(),
            "footnote-reference" => footnote_references.next(),
            "footnote-definition" => footnote_definitions.next(),
            "mermaid" => mermaid.next(),
            _ => None,
        };
        if let Some(id) = id {
            id.clone_into(&mut item.contribution_id);
            true
        } else {
            false
        }
    });
    geometry
}

fn markdown_layout_signature(
    markdown: &str,
    options: &MarkdownRenderOptions,
    contributions: &[MarkdownContribution],
    geometry: &[MarkdownContributionGeometry],
) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    markdown.hash(&mut hasher);
    options.hash(&mut hasher);
    contributions.hash(&mut hasher);
    geometry.hash(&mut hasher);
    format!("markdown-layout-v4:{:016x}", hasher.finish())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ProjectedContributionKind {
    Details,
    FootnoteReference,
    FootnoteDefinition,
}

#[derive(Debug, Clone)]
struct ProjectedContributionRange {
    kind: ProjectedContributionKind,
    range: Range<usize>,
    contribution_id: Option<String>,
}

struct ProjectedMarkdown<'a> {
    markdown: Cow<'a, str>,
    contributions: Vec<ProjectedContributionRange>,
}

type ProjectionReplacement = (
    Range<usize>,
    String,
    Vec<(ProjectedContributionKind, Range<usize>)>,
);

fn details_contribution_id(details: &DetailsProjection, document_id: Option<&str>) -> String {
    let id = format!(
        "details:{}:{}",
        details.source_range.start, details.source_range.end
    );
    document_id.map_or_else(|| id.clone(), |document_id| format!("{document_id}:{id}"))
}

fn projected_details(
    details: &DetailsProjection,
    all_details: &[DetailsProjection],
    options: &MarkdownRenderOptions,
) -> (String, Vec<(ProjectedContributionKind, Range<usize>)>) {
    let summary = details.summary_markdown.trim();
    let contribution_id = details_contribution_id(details, options.document_id.as_deref());
    let open = options
        .details_open
        .get(&contribution_id)
        .copied()
        .unwrap_or(details.default_open);
    let marker = if open { "▼" } else { "▶" };
    let summary_end = marker.len().saturating_add(3).saturating_add(summary.len());
    let mut contributions = vec![(ProjectedContributionKind::Details, 0..summary_end)];
    if !open {
        return (format!("{marker} **{summary}**"), contributions);
    }

    let mut body = details.body_markdown.clone();
    let body_start = details.body_range.start;
    let body_end = details.body_range.end;
    let mut children = all_details
        .iter()
        .filter(|candidate| {
            body_start <= candidate.source_range.start
                && candidate.source_range.end <= body_end
                && !all_details.iter().any(|parent| {
                    let parent_start = parent.source_range.start;
                    let parent_end = parent.source_range.end;
                    body_start <= parent_start
                        && parent_start < candidate.source_range.start
                        && candidate.source_range.end <= parent_end
                        && parent_end <= body_end
                })
        })
        .collect::<Vec<_>>();
    children.sort_by_key(|child| child.source_range.start);
    let mut nested_contributions: Vec<(ProjectedContributionKind, Range<usize>)> = Vec::new();
    for child in children.into_iter().rev() {
        let local = child
            .source_range
            .start
            .saturating_sub(details.body_range.start)
            ..child
                .source_range
                .end
                .saturating_sub(details.body_range.start);
        let (replacement, child_contributions) = projected_details(child, all_details, options);
        body.replace_range(local.clone(), &replacement);
        let removed = local.end.saturating_sub(local.start);
        let delta = replacement
            .len()
            .cast_signed()
            .saturating_sub(removed.cast_signed());
        for (_, range) in &mut nested_contributions {
            range.start = range.start.saturating_add_signed(delta);
            range.end = range.end.saturating_add_signed(delta);
        }
        nested_contributions.extend(child_contributions.into_iter().map(|(kind, range)| {
            (
                kind,
                local.start.saturating_add(range.start)..local.start.saturating_add(range.end),
            )
        }));
    }
    let body_start = summary_end.saturating_add(2);
    contributions.extend(nested_contributions.into_iter().map(|(kind, range)| {
        (
            kind,
            body_start.saturating_add(range.start)..body_start.saturating_add(range.end),
        )
    }));
    (format!("{marker} **{summary}**\n\n{body}"), contributions)
}

fn semantic_replacements(
    all_details: &[DetailsProjection],
    math: Vec<MathProjection>,
    footnotes: &FootnoteProjection,
    options: &MarkdownRenderOptions,
) -> Vec<ProjectionReplacement> {
    let mut replacements = Vec::new();
    for details in all_details.iter().filter(|candidate| {
        !all_details.iter().any(|parent| {
            parent.source_range.start < candidate.source_range.start
                && parent.source_range.end >= candidate.source_range.end
        })
    }) {
        let summary = details.summary_markdown.trim();
        if !options.details_interactive {
            let replacement = format!(
                "**{summary}**\n\n{}",
                project_details_markup(&details.body_markdown).trim()
            );
            replacements.push((
                details.source_range.clone(),
                replacement,
                vec![(ProjectedContributionKind::Details, 2..2 + summary.len())],
            ));
            continue;
        }
        let (replacement, contributions) = projected_details(details, all_details, options);
        replacements.push((details.source_range.clone(), replacement, contributions));
    }
    replacements.extend(math.into_iter().map(|projection| {
        let replacement = match projection.kind {
            MathKind::Inline => format!("`{}`", projection.rendered),
            MathKind::Display => format!("\n```text\n{}\n```\n", projection.rendered),
        };
        (projection.source_range, replacement, Vec::new())
    }));
    for reference in &footnotes.references {
        let replacement = format!("[{}]", reference.number);
        replacements.push((
            reference.source_range.clone(),
            replacement.clone(),
            vec![(
                ProjectedContributionKind::FootnoteReference,
                0..replacement.len(),
            )],
        ));
    }
    for definition in footnotes.definitions.values() {
        replacements.push((definition.source_range.clone(), String::new(), Vec::new()));
    }
    replacements
}

fn project_semantic_fallbacks<'a>(
    markdown: &'a str,
    document: &MarkdownDocument,
    details: &[DetailsProjection],
    options: &MarkdownRenderOptions,
) -> ProjectedMarkdown<'a> {
    let footnotes = collect_footnotes(markdown, document);
    let math = collect_math_projections(markdown, document);
    if footnotes.references.is_empty()
        && footnotes.definitions.is_empty()
        && math.is_empty()
        && details.is_empty()
    {
        return ProjectedMarkdown {
            markdown: Cow::Borrowed(markdown),
            contributions: Vec::new(),
        };
    }

    let mut replacements = semantic_replacements(details, math, &footnotes, options);
    replacements.sort_by_key(|(range, _, _)| (range.start, range.end));
    let mut projected = markdown.to_owned();
    let mut projected_contributions: Vec<ProjectedContributionRange> = Vec::new();
    for (range, replacement, contribution) in replacements.into_iter().rev() {
        projected.replace_range(range.clone(), &replacement);
        let removed = range.end.saturating_sub(range.start);
        let delta = replacement
            .len()
            .cast_signed()
            .saturating_sub(removed.cast_signed());
        for existing in &mut projected_contributions {
            existing.range.start = existing.range.start.saturating_add_signed(delta);
            existing.range.end = existing.range.end.saturating_add_signed(delta);
        }
        for (kind, local) in contribution {
            projected_contributions.push(ProjectedContributionRange {
                kind,
                range: range.start.saturating_add(local.start)
                    ..range.start.saturating_add(local.end),
                contribution_id: None,
            });
        }
    }
    projected_contributions.sort_by_key(|item| item.range.start);
    if !footnotes.definitions.is_empty() {
        projected.push_str("\n\n---\n\nFootnotes\n\n");
        for definition in footnotes.definitions.values() {
            let start = projected.len();
            let _ = write!(
                projected,
                "{}.  {}",
                definition.number,
                definition.body.trim()
            );
            let number_end = start.saturating_add(definition.number.to_string().len());
            projected_contributions.push(ProjectedContributionRange {
                kind: ProjectedContributionKind::FootnoteDefinition,
                range: start..number_end,
                contribution_id: None,
            });
            if !definition.reference_numbers.is_empty() {
                projected.push_str(" ↩");
                if definition.reference_numbers.len() > 1 {
                    let _ = write!(projected, "×{}", definition.reference_numbers.len());
                }
            }
            projected.push_str("\n\n");
        }
    }
    ProjectedMarkdown {
        markdown: Cow::Owned(projected),
        contributions: projected_contributions,
    }
}

fn project_incomplete_unsafe_destination_fallback(markdown: &str) -> Cow<'_, str> {
    let mut output = markdown.to_owned();
    let mut search_end = output.len();
    let mut changed = false;
    while let Some(separator) = output[..search_end].rfind("](") {
        let destination_start = separator + 2;
        let destination_end = output[destination_start..]
            .find([')', '\n', '\r'])
            .map_or(output.len(), |offset| destination_start + offset);
        if output[destination_start..destination_end].contains(')') {
            search_end = separator;
            continue;
        }
        let destination = output[destination_start..destination_end].trim();
        if destination.is_empty()
            || !matches!(
                resolve_markdown_destination(destination, None),
                MarkdownDestination::Inert { .. }
            )
        {
            search_end = separator;
            continue;
        }
        let Some(label_start) = output[..separator].rfind('[') else {
            search_end = separator;
            continue;
        };
        let replacement_start = label_start
            .checked_sub(1)
            .filter(|index| output.as_bytes()[*index] == b'!')
            .unwrap_or(label_start);
        let label = output[label_start + 1..separator].to_owned();
        output.replace_range(replacement_start..destination_end, &label);
        search_end = replacement_start;
        changed = true;
    }
    if changed {
        Cow::Owned(output)
    } else {
        Cow::Borrowed(markdown)
    }
}

fn project_incomplete_details_fallback(markdown: &str) -> Cow<'_, str> {
    let lower = markdown.to_ascii_lowercase();
    let Some(start) = lower.rfind("<details") else {
        return Cow::Borrowed(markdown);
    };
    if matching_details_close(
        &lower,
        lower[start..]
            .find('>')
            .map_or(start, |end| start + end + 1),
    )
    .is_some()
    {
        return Cow::Borrowed(markdown);
    }
    let suffix = &markdown[start..];
    let suffix_lower = &lower[start..];
    let Some(open_end) = suffix.find('>') else {
        return Cow::Borrowed(markdown);
    };
    let content_start = open_end + 1;
    let Some(summary_start_relative) = suffix_lower[content_start..].find("<summary") else {
        return Cow::Borrowed(markdown);
    };
    let summary_start = content_start + summary_start_relative;
    let Some(summary_open_end_relative) = suffix[summary_start..].find('>') else {
        return Cow::Borrowed(markdown);
    };
    let summary_open_end = summary_start + summary_open_end_relative + 1;
    let Some(summary_close_relative) = suffix_lower[summary_open_end..].find("</summary>") else {
        return Cow::Borrowed(markdown);
    };
    let summary_close = summary_open_end + summary_close_relative;
    let body_start = summary_close + "</summary>".len();
    let mut output = markdown[..start].to_owned();
    let _ = write!(
        output,
        "**{}**\n\n{}",
        safe_summary_markdown(&suffix[summary_open_end..summary_close]).trim(),
        suffix[body_start..].trim()
    );
    Cow::Owned(output)
}

fn project_details_markup(markdown: &str) -> Cow<'_, str> {
    let details = collect_details_projections(markdown);
    if details.is_empty() {
        return Cow::Borrowed(markdown);
    }
    let replacements = details
        .iter()
        .filter(|candidate| {
            !details.iter().any(|parent| {
                parent.source_range.start < candidate.source_range.start
                    && parent.source_range.end >= candidate.source_range.end
            })
        })
        .map(|details| {
            (
                details.source_range.clone(),
                format!(
                    "**{}**\n\n{}",
                    details.summary_markdown.trim(),
                    project_details_markup(&details.body_markdown).trim()
                ),
            )
        })
        .collect();
    Cow::Owned(replace_source_ranges(markdown, replacements))
}

fn collect_details_projections(markdown: &str) -> Vec<DetailsProjection> {
    let lower = markdown.to_ascii_lowercase();
    let mut output = Vec::new();
    let mut offset = 0;
    while let Some(relative_start) = lower[offset..].find("<details") {
        let start = offset + relative_start;
        let Some(open_end_relative) = lower[start..].find('>') else {
            break;
        };
        let open_end = start + open_end_relative + 1;
        let Some(close_start) = matching_details_close(&lower, open_end) else {
            break;
        };
        let end = close_start + "</details>".len();
        let inner_lower = &lower[open_end..close_start];
        let Some(summary_relative) = inner_lower.find("<summary") else {
            offset = end;
            continue;
        };
        let summary_start = open_end + summary_relative;
        let Some(summary_open_end_relative) = lower[summary_start..].find('>') else {
            offset = end;
            continue;
        };
        let summary_open_end = summary_start + summary_open_end_relative + 1;
        let Some(summary_close_relative) = lower[summary_open_end..close_start].find("</summary>")
        else {
            offset = end;
            continue;
        };
        let summary_close = summary_open_end + summary_close_relative;
        let open_tag = &lower[start..open_end];
        output.push(DetailsProjection {
            source_range: start..end,
            body_range: summary_close + "</summary>".len()..close_start,
            summary_markdown: safe_summary_markdown(&markdown[summary_open_end..summary_close]),
            body_markdown: markdown[summary_close + "</summary>".len()..close_start].to_owned(),
            default_open: details_has_open_attribute(open_tag),
        });
        let body_start = summary_close + "</summary>".len();
        output.extend(
            collect_details_projections(&markdown[body_start..close_start])
                .into_iter()
                .map(|mut nested| {
                    nested.source_range = nested.source_range.start.saturating_add(body_start)
                        ..nested.source_range.end.saturating_add(body_start);
                    nested.body_range = nested.body_range.start.saturating_add(body_start)
                        ..nested.body_range.end.saturating_add(body_start);
                    nested
                }),
        );
        offset = end;
    }
    output
}

fn matching_details_close(lower: &str, content_start: usize) -> Option<usize> {
    let mut depth = 1_usize;
    let mut offset = content_start;
    while offset < lower.len() {
        let next_open = lower[offset..]
            .find("<details")
            .map(|relative| offset + relative);
        let next_close = lower[offset..]
            .find("</details>")
            .map(|relative| offset + relative);
        match (next_open, next_close) {
            (Some(open), Some(close)) if open < close => {
                let tag_end = lower[open..].find('>')? + open + 1;
                depth = depth.saturating_add(1);
                offset = tag_end;
            }
            (_, Some(close)) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(close);
                }
                offset = close.saturating_add("</details>".len());
            }
            _ => return None,
        }
    }
    None
}

fn details_has_open_attribute(open_tag: &str) -> bool {
    let mut attributes = open_tag
        .trim_matches(|character| character == '<' || character == '>')
        .split_ascii_whitespace();
    let _ = attributes.next();
    attributes.any(|attribute| {
        attribute
            .split_once('=')
            .map_or(attribute, |(name, _)| name)
            .eq_ignore_ascii_case("open")
    })
}

fn safe_summary_markdown(summary: &str) -> String {
    summary
        .replace("<strong>", "**")
        .replace("</strong>", "**")
        .replace("<em>", "*")
        .replace("</em>", "*")
        .replace("<code>", "`")
        .replace("</code>", "`")
}

#[derive(Debug, Clone)]
struct DetailsProjection {
    source_range: Range<usize>,
    body_range: Range<usize>,
    summary_markdown: String,
    body_markdown: String,
    default_open: bool,
}

#[derive(Debug)]
struct MathProjection {
    source_range: Range<usize>,
    kind: MathKind,
    rendered: String,
}

#[derive(Debug, Clone, Copy)]
enum MathKind {
    Inline,
    Display,
}

fn collect_math_projections(markdown: &str, document: &MarkdownDocument) -> Vec<MathProjection> {
    document
        .events
        .iter()
        .filter_map(|event| match &event.kind {
            MarkdownSemanticEventKind::InlineMath(source) => Some(MathProjection {
                source_range: event.source_range.clone(),
                kind: MathKind::Inline,
                rendered: render_terminal_math(source),
            }),
            MarkdownSemanticEventKind::DisplayMath(source) => Some(MathProjection {
                source_range: event.source_range.clone(),
                kind: MathKind::Display,
                rendered: render_terminal_math(source),
            }),
            _ => None,
        })
        .chain(escaped_inline_math(markdown, document))
        .collect()
}

fn escaped_inline_math(
    markdown: &str,
    document: &MarkdownDocument,
) -> impl Iterator<Item = MathProjection> + use<> {
    let parsed_ranges = document
        .events
        .iter()
        .filter_map(|event| {
            matches!(
                event.kind,
                MarkdownSemanticEventKind::InlineMath(_)
                    | MarkdownSemanticEventKind::DisplayMath(_)
            )
            .then_some(event.source_range.clone())
        })
        .collect::<Vec<_>>();
    let mut output = Vec::new();
    let mut offset = 0;
    while let Some(relative_start) = markdown[offset..].find("\\(") {
        let start = offset + relative_start;
        let content_start = start + 2;
        let Some(relative_end) = markdown[content_start..].find("\\)") else {
            break;
        };
        let end = content_start + relative_end + 2;
        if !parsed_ranges
            .iter()
            .any(|range| range.start <= start && range.end >= end)
        {
            output.push(MathProjection {
                source_range: start..end,
                kind: MathKind::Inline,
                rendered: render_terminal_math(&markdown[content_start..end - 2]),
            });
        }
        offset = end;
    }
    output.into_iter()
}

fn render_terminal_math(source: &str) -> String {
    const MAX_SOURCE_BYTES: usize = 4096;
    if source.len() > MAX_SOURCE_BYTES {
        return source.to_owned();
    }
    let mut output = source.to_owned();
    for (command, replacement) in [
        ("\\times", "×"),
        ("\\cdot", "·"),
        ("\\leq", "≤"),
        ("\\geq", "≥"),
        ("\\neq", "≠"),
        ("\\to", "→"),
        ("\\infty", "∞"),
        ("\\alpha", "α"),
        ("\\beta", "β"),
        ("\\gamma", "γ"),
        ("\\Delta", "Δ"),
        ("\\sum", "∑"),
        ("\\sqrt", "√"),
    ] {
        output = output.replace(command, replacement);
    }
    output = replace_math_scripts(&output, '^', true);
    replace_math_scripts(&output, '_', false)
}

fn replace_math_scripts(source: &str, marker: char, superscript: bool) -> String {
    let characters = source.chars().collect::<Vec<_>>();
    let mut output = String::new();
    let mut index = 0;
    while index < characters.len() {
        if characters[index] == marker && index + 1 < characters.len() {
            let (script, consumed) = if characters[index + 1] == '{' {
                let mut end = index + 2;
                while end < characters.len() && characters[end] != '}' {
                    end += 1;
                }
                if end == characters.len() {
                    output.push(marker);
                    index += 1;
                    continue;
                }
                (
                    characters[index + 2..end].iter().collect::<String>(),
                    end - index,
                )
            } else {
                (characters[index + 1].to_string(), 1)
            };
            if let Some(converted) = unicode_script(&script, superscript) {
                output.push_str(&converted);
            } else {
                output.push(marker);
                if consumed > 1 {
                    output.push('{');
                    output.push_str(&script);
                    output.push('}');
                } else {
                    output.push_str(&script);
                }
            }
            index += consumed + 1;
        } else {
            output.push(characters[index]);
            index += 1;
        }
    }
    output
}

fn unicode_script(source: &str, superscript: bool) -> Option<String> {
    source
        .chars()
        .map(|character| match (superscript, character) {
            (true, '0') => Some('⁰'),
            (true, '1') => Some('¹'),
            (true, '2') => Some('²'),
            (true, '3') => Some('³'),
            (true, '4') => Some('⁴'),
            (true, '5') => Some('⁵'),
            (true, '6') => Some('⁶'),
            (true, '7') => Some('⁷'),
            (true, '8') => Some('⁸'),
            (true, '9') => Some('⁹'),
            (true, '+') => Some('⁺'),
            (true, '-') => Some('⁻'),
            (true, 'n') => Some('ⁿ'),
            (true, 'i') => Some('ⁱ'),
            (false, '0') => Some('₀'),
            (false, '1') => Some('₁'),
            (false, '2') => Some('₂'),
            (false, '3') => Some('₃'),
            (false, '4') => Some('₄'),
            (false, '5') => Some('₅'),
            (false, '6') => Some('₆'),
            (false, '7') => Some('₇'),
            (false, '8') => Some('₈'),
            (false, '9') => Some('₉'),
            (false, '+') => Some('₊'),
            (false, '-') => Some('₋'),
            _ => None,
        })
        .collect()
}

#[derive(Debug)]
struct FootnoteProjection {
    references: Vec<FootnoteReferenceProjection>,
    definitions: BTreeMap<String, FootnoteDefinitionProjection>,
}

#[derive(Debug)]
struct FootnoteReferenceProjection {
    source_range: Range<usize>,
    number: usize,
}

#[derive(Debug)]
struct FootnoteDefinitionProjection {
    source_range: Range<usize>,
    number: usize,
    body: String,
    reference_numbers: Vec<usize>,
}

fn collect_footnotes(markdown: &str, document: &MarkdownDocument) -> FootnoteProjection {
    let mut label_numbers = BTreeMap::<String, usize>::new();
    let mut references = Vec::new();
    let mut definition_events = BTreeMap::<String, (Range<usize>, String)>::new();
    let mut active_definition: Option<(String, Range<usize>, String)> = None;

    for event in &document.events {
        match &event.kind {
            MarkdownSemanticEventKind::FootnoteReference(label) => {
                let next_number = label_numbers.len().saturating_add(1);
                let number = *label_numbers.entry(label.clone()).or_insert(next_number);
                references.push((
                    label.clone(),
                    FootnoteReferenceProjection {
                        source_range: event.source_range.clone(),
                        number,
                    },
                ));
            }
            MarkdownSemanticEventKind::Start(MarkdownSemanticTag::FootnoteDefinition(label)) => {
                active_definition =
                    Some((label.clone(), event.source_range.clone(), String::new()));
            }
            MarkdownSemanticEventKind::End(MarkdownSemanticTagEnd::FootnoteDefinition) => {
                if let Some((label, range, body)) = active_definition.take() {
                    let source_body = footnote_definition_markdown(
                        markdown.get(range.clone()).unwrap_or_default(),
                        &label,
                    );
                    definition_events.insert(
                        label,
                        (
                            range,
                            if source_body.is_empty() {
                                body
                            } else {
                                source_body
                            },
                        ),
                    );
                }
            }
            MarkdownSemanticEventKind::Text(text) | MarkdownSemanticEventKind::Code(text) => {
                if let Some((_, _, body)) = &mut active_definition {
                    body.push_str(text);
                }
            }
            MarkdownSemanticEventKind::SoftBreak | MarkdownSemanticEventKind::HardBreak => {
                if let Some((_, _, body)) = &mut active_definition {
                    body.push(' ');
                }
            }
            _ => {}
        }
    }

    for (label, range) in unresolved_footnote_references(markdown, document) {
        let next_number = label_numbers.len().saturating_add(1);
        let number = *label_numbers.entry(label.clone()).or_insert(next_number);
        references.push((
            label,
            FootnoteReferenceProjection {
                source_range: range,
                number,
            },
        ));
    }
    references.sort_by_key(|(_, reference)| reference.source_range.start);
    for label in definition_events.keys() {
        let next_number = label_numbers.len().saturating_add(1);
        label_numbers.entry(label.clone()).or_insert(next_number);
    }
    let mut reference_counts = BTreeMap::<String, Vec<usize>>::new();
    for (ordinal, (label, _)) in references.iter().enumerate() {
        reference_counts
            .entry(label.clone())
            .or_default()
            .push(ordinal.saturating_add(1));
    }
    let definitions = definition_events
        .into_iter()
        .map(|(label, (source_range, body))| {
            let number = label_numbers[&label];
            (
                label.clone(),
                FootnoteDefinitionProjection {
                    source_range,
                    number,
                    body,
                    reference_numbers: reference_counts.remove(&label).unwrap_or_default(),
                },
            )
        })
        .collect();
    FootnoteProjection {
        references: references
            .into_iter()
            .map(|(_, reference)| reference)
            .collect(),
        definitions,
    }
}

fn footnote_definition_markdown(source: &str, label: &str) -> String {
    let prefix = format!("[^{label}]:");
    let mut lines = source.lines();
    let Some(first) = lines.next() else {
        return String::new();
    };
    let Some(first_body) = first.strip_prefix(&prefix) else {
        return String::new();
    };
    let continuation = lines
        .map(|line| {
            line.strip_prefix("    ")
                .or_else(|| line.strip_prefix('\t'))
                .unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n");
    if continuation.is_empty() {
        first_body.trim_start().to_owned()
    } else {
        format!("{}\n{}", first_body.trim_start(), continuation)
    }
}

fn unresolved_footnote_references(
    markdown: &str,
    document: &MarkdownDocument,
) -> Vec<(String, Range<usize>)> {
    let known_ranges = document
        .events
        .iter()
        .filter_map(|event| {
            matches!(
                event.kind,
                MarkdownSemanticEventKind::FootnoteReference(_)
                    | MarkdownSemanticEventKind::Start(MarkdownSemanticTag::FootnoteDefinition(_))
            )
            .then_some(event.source_range.clone())
        })
        .collect::<Vec<_>>();
    let mut output = Vec::new();
    let bytes = markdown.as_bytes();
    let mut index = 0;
    while index + 3 < bytes.len() {
        if bytes[index] == b'['
            && bytes[index + 1] == b'^'
            && let Some(relative_end) = markdown[index + 2..].find(']')
        {
            let end = index + 2 + relative_end + 1;
            let range = index..end;
            let is_definition = bytes.get(end) == Some(&b':');
            if !is_definition
                && !known_ranges
                    .iter()
                    .any(|known| known.start <= index && known.end >= end)
            {
                let label = &markdown[index + 2..end - 1];
                if !label.is_empty()
                    && label
                        .chars()
                        .all(|character| character.is_alphanumeric() || "-_".contains(character))
                {
                    output.push((label.to_owned(), range));
                }
            }
            index = end;
        } else {
            index += 1;
        }
    }
    output
}

fn replace_source_ranges(markdown: &str, mut replacements: Vec<(Range<usize>, String)>) -> String {
    replacements.sort_by_key(|(range, _)| (range.start, range.end));
    let mut output = markdown.to_owned();
    for (range, replacement) in replacements.into_iter().rev() {
        output.replace_range(range, &replacement);
    }
    output
}

/// Render Markdown into terminal lines.
#[must_use]
pub fn render_markdown_lines(markdown: &str, options: MarkdownRenderOptions) -> Vec<Line> {
    let result = render_markdown(markdown, &options);
    drop(options);
    result.lines
}

fn render_markdown_projection(
    markdown: &str,
    options: &MarkdownRenderOptions,
) -> (Vec<Line>, Vec<MarkdownContributionGeometry>, Vec<u16>) {
    #[cfg(test)]
    MARKDOWN_PROJECTION_COUNT.set(MARKDOWN_PROJECTION_COUNT.get().saturating_add(1));
    let table_alignments = table_alignments(markdown);
    let alert_kinds = alert_kinds(markdown);
    let container = markdown_to_container_with_options(markdown, hyperchad_markdown_options());
    let mut renderer = TerminalMarkdownRenderer::new(
        options.width,
        options.theme,
        options.syntax,
        table_alignments,
        alert_kinds,
        options.image_reserved_rows,
        options.mermaid_reserved_rows,
    );
    renderer.render_container_children(
        &container,
        TextStyle {
            style: options.theme.text,
            preserve_whitespace: false,
        },
    );
    renderer.finish_projection()
}

fn markdown_contributions(
    document: &MarkdownDocument,
    context: Option<&MarkdownDocumentContext>,
    document_id: Option<&str>,
    details: &[DetailsProjection],
    options: &MarkdownRenderOptions,
) -> Vec<MarkdownContribution> {
    let mut contributions = Vec::new();
    let mut containers: Vec<ContributionContainer> = Vec::new();
    for event in &document.events {
        collect_markdown_contribution(event, context, options, &mut containers, &mut contributions);
    }
    if let Some(repository) = context.and_then(|context| context.github_repository.as_ref()) {
        contributions.extend(github_issue_contributions(document, repository));
    }
    contributions.extend(details.iter().map(|details| {
        contribution(
            "details",
            details.source_range.clone(),
            MarkdownContributionKind::Details {
                summary: details.summary_markdown.clone(),
                body: details.body_markdown.clone(),
                default_open: details.default_open,
            },
        )
    }));
    contributions.sort_by_key(|item| (item.source_range.start, item.source_range.end));
    qualify_contribution_ids(&mut contributions, document_id);
    link_footnote_contributions(&mut contributions);
    contributions
}

fn qualify_contribution_ids(contributions: &mut [MarkdownContribution], document_id: Option<&str>) {
    let Some(document_id) = document_id else {
        return;
    };
    for item in contributions {
        item.id = format!("{document_id}:{}", item.id);
    }
}

fn link_footnote_contributions(contributions: &mut [MarkdownContribution]) {
    let definitions = contributions
        .iter()
        .filter_map(|item| match &item.kind {
            MarkdownContributionKind::FootnoteDefinition { label, .. } => {
                Some((label.clone(), item.id.clone()))
            }
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let mut references = BTreeMap::<String, Vec<String>>::new();
    for item in contributions.iter_mut() {
        if let MarkdownContributionKind::FootnoteReference { label, target_id } = &mut item.kind {
            *target_id = definitions.get(label).cloned().unwrap_or_default();
            references
                .entry(label.clone())
                .or_default()
                .push(item.id.clone());
        }
    }
    for item in contributions {
        if let MarkdownContributionKind::FootnoteDefinition {
            label,
            reference_ids,
        } = &mut item.kind
        {
            *reference_ids = references.remove(label).unwrap_or_default();
        }
    }
}

fn github_issue_contributions(
    document: &MarkdownDocument,
    repository: &GitHubRepository,
) -> Vec<MarkdownContribution> {
    let mut output = Vec::new();
    for event in &document.events {
        let MarkdownSemanticEventKind::Text(text) = &event.kind else {
            continue;
        };
        for (offset, number, closes) in github_issue_references(text) {
            let start = event.source_range.start.saturating_add(offset);
            let end = start
                .saturating_add(number.to_string().len())
                .saturating_add(1);
            let url = format!(
                "https://github.com/{}/{}/issues/{number}",
                repository.owner, repository.name
            );
            let destination = Url::parse(&url).map_or_else(
                |_| MarkdownDestination::Inert {
                    reason: MarkdownDestinationRejection::UnsupportedScheme,
                },
                MarkdownDestination::Web,
            );
            output.push(contribution(
                "github-issue",
                start..end,
                MarkdownContributionKind::GitHubIssue {
                    repository: repository.clone(),
                    number,
                    destination,
                    closes,
                },
            ));
        }
    }
    output
}

fn github_issue_references(text: &str) -> Vec<(usize, u64, bool)> {
    let mut output = Vec::new();
    for (index, _) in text.match_indices('#') {
        let digits = text[index + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        if digits.is_empty() {
            continue;
        }
        let boundary = text[index + 1 + digits.len()..].chars().next();
        if boundary.is_some_and(|character| character.is_alphanumeric() || character == '_') {
            continue;
        }
        if let Ok(number) = digits.parse::<u64>()
            && number > 0
        {
            let prefix = text[..index].trim_end();
            let keyword = prefix
                .split_whitespace()
                .next_back()
                .unwrap_or_default()
                .trim_matches(|character: char| !character.is_alphabetic());
            output.push((
                index,
                number,
                matches!(
                    keyword.to_ascii_lowercase().as_str(),
                    "close"
                        | "closes"
                        | "closed"
                        | "fix"
                        | "fixes"
                        | "fixed"
                        | "resolve"
                        | "resolves"
                        | "resolved"
                ),
            ));
        }
    }
    output
}

fn collect_markdown_contribution(
    event: &MarkdownSemanticEvent,
    context: Option<&MarkdownDocumentContext>,
    options: &MarkdownRenderOptions,
    containers: &mut Vec<ContributionContainer>,
    contributions: &mut Vec<MarkdownContribution>,
) {
    match &event.kind {
        MarkdownSemanticEventKind::Start(MarkdownSemanticTag::Link(link)) => {
            containers.push(ContributionContainer::Link {
                metadata: link.clone(),
                source_range: event.source_range.clone(),
                text: String::new(),
            });
        }
        MarkdownSemanticEventKind::Start(MarkdownSemanticTag::Image(image)) => {
            containers.push(ContributionContainer::Image {
                metadata: image.clone(),
                source_range: event.source_range.clone(),
                text: String::new(),
                linked_destination: containers.iter().rev().find_map(|container| {
                    if let ContributionContainer::Link { metadata, .. } = container {
                        Some(resolve_markdown_destination(&metadata.destination, context))
                    } else {
                        None
                    }
                }),
            });
        }
        MarkdownSemanticEventKind::Start(MarkdownSemanticTag::FootnoteDefinition(label)) => {
            contributions.push(contribution(
                "footnote-definition",
                event.source_range.clone(),
                MarkdownContributionKind::FootnoteDefinition {
                    label: label.clone(),
                    reference_ids: Vec::new(),
                },
            ));
        }
        MarkdownSemanticEventKind::Start(MarkdownSemanticTag::CodeBlock(Some(language)))
            if language.eq_ignore_ascii_case("mermaid") =>
        {
            containers.push(ContributionContainer::Mermaid {
                source_range: event.source_range.clone(),
                text: String::new(),
            });
        }
        MarkdownSemanticEventKind::End(MarkdownSemanticTagEnd::Link) => {
            finish_link_contribution(containers, contributions, context);
        }
        MarkdownSemanticEventKind::End(MarkdownSemanticTagEnd::Image) => {
            finish_image_contribution(containers, contributions, context);
        }
        MarkdownSemanticEventKind::End(MarkdownSemanticTagEnd::CodeBlock) => {
            finish_mermaid_contribution(containers, contributions, options);
        }
        MarkdownSemanticEventKind::Text(text) | MarkdownSemanticEventKind::Code(text) => {
            if let Some(container) = containers.last_mut() {
                container.push_text(text);
            }
        }
        MarkdownSemanticEventKind::FootnoteReference(label) => contributions.push(contribution(
            "footnote-reference",
            event.source_range.clone(),
            MarkdownContributionKind::FootnoteReference {
                label: label.clone(),
                target_id: String::new(),
            },
        )),
        MarkdownSemanticEventKind::InlineMath(source) => contributions.push(contribution(
            "inline-math",
            event.source_range.clone(),
            MarkdownContributionKind::InlineMath {
                source: source.clone(),
            },
        )),
        MarkdownSemanticEventKind::DisplayMath(source) => contributions.push(contribution(
            "display-math",
            event.source_range.clone(),
            MarkdownContributionKind::DisplayMath {
                source: source.clone(),
            },
        )),
        _ => {}
    }
}

fn finish_link_contribution(
    containers: &mut Vec<ContributionContainer>,
    contributions: &mut Vec<MarkdownContribution>,
    context: Option<&MarkdownDocumentContext>,
) {
    if let Some(ContributionContainer::Link {
        metadata,
        source_range,
        text,
    }) = containers.pop()
    {
        contributions.push(contribution(
            "link",
            source_range,
            MarkdownContributionKind::Link {
                destination: resolve_markdown_destination(&metadata.destination, context),
                link: metadata,
                label: normalize_inline_whitespace(&text).trim().to_owned(),
            },
        ));
    }
}

fn finish_image_contribution(
    containers: &mut Vec<ContributionContainer>,
    contributions: &mut Vec<MarkdownContribution>,
    context: Option<&MarkdownDocumentContext>,
) {
    if let Some(ContributionContainer::Image {
        metadata,
        source_range,
        text,
        linked_destination,
    }) = containers.pop()
    {
        contributions.push(contribution(
            "image",
            source_range,
            MarkdownContributionKind::Image {
                source: resolve_markdown_destination(&metadata.source, context),
                image: metadata,
                alt: normalize_inline_whitespace(&text).trim().to_owned(),
                linked_destination,
            },
        ));
    }
}

fn finish_mermaid_contribution(
    containers: &mut Vec<ContributionContainer>,
    contributions: &mut Vec<MarkdownContribution>,
    options: &MarkdownRenderOptions,
) {
    if matches!(
        containers.last(),
        Some(ContributionContainer::Mermaid { .. })
    ) && let Some(ContributionContainer::Mermaid { source_range, text }) = containers.pop()
    {
        let request =
            MermaidRenderRequest::svg(text.clone(), options.mermaid_width, options.mermaid_height);
        let cache_key = request.cache_key();
        let rendering = if matches!(options.mermaid, MermaidRenderMode::Inline) {
            match render_mermaid(&request, &MermaidCancellationToken::default()) {
                Ok(rendered) => match rendered.output {
                    MermaidRenderedOutput::Svg(svg) => MermaidContributionRendering::Svg(svg),
                },
                Err(error) => MermaidContributionRendering::Failed(error.to_string()),
            }
        } else {
            MermaidContributionRendering::Disabled
        };
        contributions.push(contribution(
            "mermaid",
            source_range,
            MarkdownContributionKind::Mermaid {
                source: text,
                cache_key,
                rendering,
            },
        ));
    }
}

#[derive(Debug)]
enum ContributionContainer {
    Link {
        metadata: MarkdownLink,
        source_range: Range<usize>,
        text: String,
    },
    Image {
        metadata: MarkdownImage,
        source_range: Range<usize>,
        text: String,
        linked_destination: Option<MarkdownDestination>,
    },
    Mermaid {
        source_range: Range<usize>,
        text: String,
    },
}

impl ContributionContainer {
    fn push_text(&mut self, value: &str) {
        match self {
            Self::Link { text, .. } | Self::Image { text, .. } | Self::Mermaid { text, .. } => {
                text.push_str(value);
            }
        }
    }
}

fn contribution(
    prefix: &str,
    source_range: Range<usize>,
    kind: MarkdownContributionKind,
) -> MarkdownContribution {
    MarkdownContribution {
        id: format!("{prefix}:{}:{}", source_range.start, source_range.end),
        source_range,
        kind,
    }
}

fn semantic_event(event: Event<'_>) -> Option<MarkdownSemanticEventKind> {
    match event {
        Event::Start(tag) => semantic_start_tag(tag).map(MarkdownSemanticEventKind::Start),
        Event::End(tag) => semantic_end_tag(tag).map(MarkdownSemanticEventKind::End),
        Event::Text(text) => Some(MarkdownSemanticEventKind::Text(text.into_string())),
        Event::Code(code) => Some(MarkdownSemanticEventKind::Code(code.into_string())),
        Event::InlineMath(math) => Some(MarkdownSemanticEventKind::InlineMath(math.into_string())),
        Event::DisplayMath(math) => {
            Some(MarkdownSemanticEventKind::DisplayMath(math.into_string()))
        }
        Event::Html(html) | Event::InlineHtml(html) => {
            Some(MarkdownSemanticEventKind::Html(html.into_string()))
        }
        Event::FootnoteReference(label) => Some(MarkdownSemanticEventKind::FootnoteReference(
            label.into_string(),
        )),
        Event::SoftBreak => Some(MarkdownSemanticEventKind::SoftBreak),
        Event::HardBreak => Some(MarkdownSemanticEventKind::HardBreak),
        Event::Rule => Some(MarkdownSemanticEventKind::Rule),
        Event::TaskListMarker(checked) => Some(MarkdownSemanticEventKind::TaskListMarker(checked)),
    }
}

fn semantic_start_tag(tag: Tag<'_>) -> Option<MarkdownSemanticTag> {
    match tag {
        Tag::Paragraph => Some(MarkdownSemanticTag::Paragraph),
        Tag::Heading { level, .. } => Some(MarkdownSemanticTag::Heading(heading_level(level))),
        Tag::BlockQuote(kind) => Some(MarkdownSemanticTag::BlockQuote(kind.map(alert_kind))),
        Tag::CodeBlock(kind) => Some(MarkdownSemanticTag::CodeBlock(match kind {
            CodeBlockKind::Indented => None,
            CodeBlockKind::Fenced(language) => language
                .split_whitespace()
                .next()
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
        })),
        Tag::HtmlBlock => Some(MarkdownSemanticTag::HtmlBlock),
        Tag::List(start) => Some(MarkdownSemanticTag::List(start)),
        Tag::Item => Some(MarkdownSemanticTag::Item),
        Tag::FootnoteDefinition(label) => {
            Some(MarkdownSemanticTag::FootnoteDefinition(label.into_string()))
        }
        Tag::Table(alignments) => Some(MarkdownSemanticTag::Table(
            alignments.into_iter().map(table_alignment).collect(),
        )),
        Tag::TableHead => Some(MarkdownSemanticTag::TableHead),
        Tag::TableRow => Some(MarkdownSemanticTag::TableRow),
        Tag::TableCell => Some(MarkdownSemanticTag::TableCell),
        Tag::Emphasis => Some(MarkdownSemanticTag::Emphasis),
        Tag::Strong => Some(MarkdownSemanticTag::Strong),
        Tag::Strikethrough => Some(MarkdownSemanticTag::Strikethrough),
        Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        } => Some(MarkdownSemanticTag::Link(MarkdownLink {
            destination: dest_url.into_string(),
            title: optional_parser_string(title.as_ref()),
            reference_id: optional_parser_string(id.as_ref()),
            kind: link_kind(link_type),
        })),
        Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        } => Some(MarkdownSemanticTag::Image(MarkdownImage {
            source: dest_url.into_string(),
            title: optional_parser_string(title.as_ref()),
            reference_id: optional_parser_string(id.as_ref()),
            kind: link_kind(link_type),
        })),
        Tag::DefinitionList
        | Tag::DefinitionListTitle
        | Tag::DefinitionListDefinition
        | Tag::Superscript
        | Tag::Subscript
        | Tag::MetadataBlock(_) => None,
    }
}

const fn semantic_end_tag(tag: TagEnd) -> Option<MarkdownSemanticTagEnd> {
    match tag {
        TagEnd::Paragraph => Some(MarkdownSemanticTagEnd::Paragraph),
        TagEnd::Heading(_) => Some(MarkdownSemanticTagEnd::Heading),
        TagEnd::BlockQuote(_) => Some(MarkdownSemanticTagEnd::BlockQuote),
        TagEnd::CodeBlock => Some(MarkdownSemanticTagEnd::CodeBlock),
        TagEnd::HtmlBlock => Some(MarkdownSemanticTagEnd::HtmlBlock),
        TagEnd::List(_) => Some(MarkdownSemanticTagEnd::List),
        TagEnd::Item => Some(MarkdownSemanticTagEnd::Item),
        TagEnd::FootnoteDefinition => Some(MarkdownSemanticTagEnd::FootnoteDefinition),
        TagEnd::Table => Some(MarkdownSemanticTagEnd::Table),
        TagEnd::TableHead => Some(MarkdownSemanticTagEnd::TableHead),
        TagEnd::TableRow => Some(MarkdownSemanticTagEnd::TableRow),
        TagEnd::TableCell => Some(MarkdownSemanticTagEnd::TableCell),
        TagEnd::Emphasis => Some(MarkdownSemanticTagEnd::Emphasis),
        TagEnd::Strong => Some(MarkdownSemanticTagEnd::Strong),
        TagEnd::Strikethrough => Some(MarkdownSemanticTagEnd::Strikethrough),
        TagEnd::Link => Some(MarkdownSemanticTagEnd::Link),
        TagEnd::Image => Some(MarkdownSemanticTagEnd::Image),
        TagEnd::DefinitionList
        | TagEnd::DefinitionListTitle
        | TagEnd::DefinitionListDefinition
        | TagEnd::Superscript
        | TagEnd::Subscript
        | TagEnd::MetadataBlock(_) => None,
    }
}

const fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

const fn alert_kind(kind: BlockQuoteKind) -> MarkdownAlertKind {
    match kind {
        BlockQuoteKind::Note => MarkdownAlertKind::Note,
        BlockQuoteKind::Tip => MarkdownAlertKind::Tip,
        BlockQuoteKind::Important => MarkdownAlertKind::Important,
        BlockQuoteKind::Warning => MarkdownAlertKind::Warning,
        BlockQuoteKind::Caution => MarkdownAlertKind::Caution,
    }
}

const fn table_alignment(alignment: Alignment) -> MarkdownTableAlignment {
    match alignment {
        Alignment::None => MarkdownTableAlignment::None,
        Alignment::Left => MarkdownTableAlignment::Left,
        Alignment::Center => MarkdownTableAlignment::Center,
        Alignment::Right => MarkdownTableAlignment::Right,
    }
}

const fn link_kind(kind: LinkType) -> MarkdownLinkKind {
    match kind {
        LinkType::Inline => MarkdownLinkKind::Inline,
        LinkType::Reference => MarkdownLinkKind::Reference,
        LinkType::Collapsed => MarkdownLinkKind::Collapsed,
        LinkType::Shortcut => MarkdownLinkKind::Shortcut,
        LinkType::Autolink => MarkdownLinkKind::Autolink,
        LinkType::Email => MarkdownLinkKind::Email,
        LinkType::WikiLink { .. } => MarkdownLinkKind::WikiLink,
        LinkType::ReferenceUnknown | LinkType::CollapsedUnknown | LinkType::ShortcutUnknown => {
            MarkdownLinkKind::Unresolved
        }
    }
}

fn optional_parser_string(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

fn table_alignments(markdown: &str) -> Vec<Vec<Alignment>> {
    let mut options = ParserOptions::empty();
    options.insert(ParserOptions::ENABLE_TABLES);
    Parser::new_ext(markdown, options)
        .filter_map(|event| match event {
            Event::Start(Tag::Table(alignments)) => Some(alignments),
            _ => None,
        })
        .collect()
}

fn alert_kinds(markdown: &str) -> Vec<Option<BlockQuoteKind>> {
    let mut options = ParserOptions::empty();
    options.insert(ParserOptions::ENABLE_GFM);
    Parser::new_ext(markdown, options)
        .filter_map(|event| match event {
            Event::Start(Tag::BlockQuote(kind)) => Some(kind),
            _ => None,
        })
        .collect()
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
    fn merge_container(self, container: &Container, theme: MarkdownTheme) -> Self {
        let mut output = self;
        if let Some(style) = semantic_markdown_style(container, theme) {
            output.style = output.style.patch(style);
        } else if let Some(color) = container.color {
            output.style = output.style.fg(hyperchad_color_to_tui(color));
        }
        if container
            .classes
            .iter()
            .any(|class| class == "markdown-link")
            && !matches!(
                &container.element,
                Element::Anchor { href: Some(href), .. }
                    if href.starts_with("bcode-contribution:")
            )
        {
            output.style = output.style.patch(theme.link);
        }
        if container
            .classes
            .iter()
            .any(|class| class == "markdown-strong")
        {
            output.style = output.style.patch(theme.strong);
        }
        if container.classes.iter().any(|class| class == "markdown-em") {
            output.style = output.style.patch(theme.emphasis);
        }
        if container.font_weight.is_some_and(is_bold_weight) {
            output.style = output.style.patch(theme.strong);
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
            output.style = output.style.patch(theme.strikethrough);
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
    geometry: Rc<RefCell<Vec<MarkdownContributionGeometry>>>,
    heading_rows: Rc<RefCell<Vec<u16>>>,
    origin_x: usize,
    origin_y: usize,
    in_table_collection: bool,
    table_alignments: Vec<Vec<Alignment>>,
    next_table_index: usize,
    alert_kinds: Rc<Vec<Option<BlockQuoteKind>>>,
    next_blockquote_index: Rc<Cell<usize>>,
    image_reserved_rows: usize,
    mermaid_reserved_rows: usize,
    theme: MarkdownTheme,
    syntax: Option<SyntaxPalette>,
}

impl TerminalMarkdownRenderer {
    fn new(
        width: u16,
        theme: MarkdownTheme,
        syntax: Option<SyntaxPalette>,
        table_alignments: Vec<Vec<Alignment>>,
        alert_kinds: Vec<Option<BlockQuoteKind>>,
        image_reserved_rows: u16,
        mermaid_reserved_rows: u16,
    ) -> Self {
        Self {
            width: usize::from(width.max(1)),
            rows: Vec::new(),
            current_spans: Vec::new(),
            current_width: 0,
            geometry: Rc::new(RefCell::new(Vec::new())),
            heading_rows: Rc::new(RefCell::new(Vec::new())),
            origin_x: 0,
            origin_y: 0,
            in_table_collection: false,
            table_alignments,
            next_table_index: 0,
            alert_kinds: Rc::new(alert_kinds),
            next_blockquote_index: Rc::new(Cell::new(0)),
            image_reserved_rows: usize::from(image_reserved_rows.max(1)),
            mermaid_reserved_rows: usize::from(mermaid_reserved_rows.max(1)),
            theme,
            syntax,
        }
    }

    fn nested(&self, width: usize, origin_x: usize, origin_y: usize) -> Self {
        Self {
            width: width.max(1),
            rows: Vec::new(),
            current_spans: Vec::new(),
            current_width: 0,
            geometry: Rc::clone(&self.geometry),
            heading_rows: Rc::clone(&self.heading_rows),
            origin_x,
            origin_y,
            in_table_collection: false,
            table_alignments: Vec::new(),
            next_table_index: 0,
            alert_kinds: Rc::clone(&self.alert_kinds),
            next_blockquote_index: Rc::clone(&self.next_blockquote_index),
            image_reserved_rows: self.image_reserved_rows,
            mermaid_reserved_rows: self.mermaid_reserved_rows,
            theme: self.theme,
            syntax: self.syntax,
        }
    }

    fn finish(mut self) -> Vec<Line> {
        self.flush_line();
        trim_blank_edges(&mut self.rows);
        self.rows
    }

    fn finish_projection(mut self) -> (Vec<Line>, Vec<MarkdownContributionGeometry>, Vec<u16>) {
        self.flush_line();
        let trimmed_top = self
            .rows
            .iter()
            .take_while(|line| line.spans.is_empty())
            .count();
        trim_blank_edges(&mut self.rows);
        let trimmed_top = u16::try_from(trimmed_top).unwrap_or(u16::MAX);
        let mut geometry = std::mem::take(&mut *self.geometry.borrow_mut());
        for item in &mut geometry {
            for rect in &mut item.rects {
                rect.y = rect.y.saturating_sub(trimmed_top);
            }
        }
        let mut heading_rows = std::mem::take(&mut *self.heading_rows.borrow_mut());
        for row in &mut heading_rows {
            *row = row.saturating_sub(trimmed_top);
        }
        (self.rows, geometry, heading_rows)
    }

    fn render_container_children(&mut self, container: &Container, style: TextStyle) {
        let mut children = container.children.iter().peekable();
        while let Some(child) = children.next() {
            if let Element::Image { alt, source, .. } = &child.element {
                let parsed_alt = children.peek().and_then(|next| match &next.element {
                    Element::Text { value } if !value.is_empty() => Some(value.as_str()),
                    _ => None,
                });
                if parsed_alt.is_some() {
                    children.next();
                }
                let image = parsed_alt
                    .or_else(|| alt.as_deref().filter(|value| !value.is_empty()))
                    .or_else(|| source.as_deref().filter(|value| !value.is_empty()))
                    .unwrap_or("image");
                self.render_marked_reserved_text(
                    &format!("[image: {image}]"),
                    style,
                    "image",
                    self.image_reserved_rows,
                );
            } else {
                self.render_container(child, style);
            }
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
            Element::Span | Element::THead | Element::TBody => {
                self.render_container_children(container, style);
            }
            Element::Anchor { href, .. } => {
                let kind = match href.as_deref() {
                    Some("bcode-contribution:details") => "details",
                    Some("bcode-contribution:footnote-reference") => "footnote-reference",
                    Some("bcode-contribution:footnote-definition") => "footnote-definition",
                    _ => "link",
                };
                self.render_marked_children(container, style, kind);
            }
            Element::TH { .. } | Element::TD { .. } => {
                if !self.in_table_collection {
                    self.render_container_children(container, style);
                }
            }
            Element::Table => self.render_table(container, style),
            Element::Heading { .. } => {
                self.ensure_blank_line();
                self.heading_rows.borrow_mut().push(
                    u16::try_from(self.origin_y.saturating_add(self.rows.len()))
                        .unwrap_or(u16::MAX),
                );
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
                    .or_else(|| source.as_deref().filter(|value| !value.is_empty()))
                    .unwrap_or("image");
                self.render_marked_reserved_text(
                    &format!("[image: {image}]"),
                    style,
                    "image",
                    self.image_reserved_rows,
                );
            }
            _ => {
                self.render_special_block_container(container, style);
            }
        }
    }

    fn render_marked_children(&mut self, container: &Container, style: TextStyle, kind: &str) {
        let start_row = self.rows.len();
        let start_column = self.current_width;
        self.render_container_children(container, style);
        self.record_geometry(kind, start_row, start_column);
    }

    fn render_marked_reserved_text(
        &mut self,
        text: &str,
        style: TextStyle,
        kind: &str,
        reserved_rows: usize,
    ) {
        let start_row = self.rows.len();
        let start_column = self.current_width;
        self.push_text(text, style);
        if reserved_rows <= 1 {
            self.record_geometry(kind, start_row, start_column);
            return;
        }
        self.flush_line();
        while self.rows.len().saturating_sub(start_row) < reserved_rows {
            self.rows.push(Line::from_spans(vec![Span::raw(" ")]));
        }
        self.record_geometry(kind, start_row, start_column);
    }

    fn record_geometry(&self, kind: &str, start_row: usize, start_column: usize) {
        let end_row = self.rows.len();
        let mut rects = Vec::new();
        if start_row == end_row {
            let width = self.current_width.saturating_sub(start_column);
            if width > 0 {
                rects.push(markdown_cell_rect(start_column, start_row, width));
            }
        } else {
            if let Some(first) = self.rows.get(start_row) {
                let width = spans_width(&first.spans).saturating_sub(start_column);
                if width > 0 {
                    rects.push(markdown_cell_rect(start_column, start_row, width));
                }
            }
            for row in start_row.saturating_add(1)..end_row {
                if let Some(line) = self.rows.get(row) {
                    let width = spans_width(&line.spans);
                    if width > 0 {
                        rects.push(markdown_cell_rect(0, row, width));
                    }
                }
            }
            if self.current_width > 0 {
                rects.push(markdown_cell_rect(0, end_row, self.current_width));
            }
        }
        if !rects.is_empty() {
            for rect in &mut rects {
                rect.x = rect
                    .x
                    .saturating_add(u16::try_from(self.origin_x).unwrap_or(u16::MAX));
                rect.y = rect
                    .y
                    .saturating_add(u16::try_from(self.origin_y).unwrap_or(u16::MAX));
            }
            debug_assert!(rects.iter().all(|rect| rect.width > 0 && rect.height == 1));
            self.geometry
                .borrow_mut()
                .push(MarkdownContributionGeometry {
                    contribution_id: kind.to_owned(),
                    rects,
                });
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
            Input::File => {}
        }
    }

    fn render_special_block_container(&mut self, container: &Container, style: TextStyle) {
        if container.classes.iter().any(|class| class == "markdown-p") {
            self.ensure_blank_line();
            self.render_container_children(container, style);
            self.ensure_blank_line();
            return;
        }
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
        let blockquote_index = self.next_blockquote_index.get();
        let alert_kind = self.alert_kinds.get(blockquote_index).copied().flatten();
        self.next_blockquote_index
            .set(blockquote_index.saturating_add(1));
        let mut nested = self.nested(
            self.width.saturating_sub(2),
            self.origin_x.saturating_add(2),
            self.origin_y.saturating_add(self.rows.len()),
        );
        nested.render_container_children(container, style);
        nested.flush_line();
        let mut rows = nested.finish();
        if let Some(kind) = alert_kind {
            strip_alert_marker(&mut rows, kind);
            let alert_style = self.theme.alert_style(kind);
            rows.insert(
                0,
                Line::from_spans(vec![Span::styled(
                    alert_label(kind),
                    alert_style.patch(self.theme.strong),
                )]),
            );
        }
        let border_style = alert_kind.map_or(self.theme.blockquote_bar, |kind| {
            self.theme.alert_style(kind)
        });
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
        let geometry_start = (language == Some("mermaid")).then(|| {
            (
                self.rows.len(),
                self.current_width,
                self.geometry.borrow().len(),
            )
        });
        let header = language.map_or_else(|| "┌─".to_owned(), |language| format!("┌─ {language}"));
        self.rows
            .push(Line::from_spans(vec![Span::styled(header, border_style)]));

        let nested_width = self.width.saturating_sub(2);
        let mut nested = self.nested(
            nested_width,
            self.origin_x.saturating_add(2),
            self.origin_y.saturating_add(self.rows.len()),
        );
        nested.render_container_children(
            container,
            TextStyle {
                style: self.theme.code_block_text,
                preserve_whitespace: true,
            },
        );
        nested.flush_line();
        let mut code_rows = nested.finish();
        if let Some(language) = language {
            apply_code_block_syntax_highlighting(language, &mut code_rows, self.theme, self.syntax);
        }
        if code_rows.is_empty() {
            code_rows.push(Line::default());
        }
        for line in code_rows {
            let mut spans = vec![Span::styled("│ ", border_style)];
            spans.extend(line.spans);
            self.rows.push(Line::from_spans(spans));
        }
        self.rows
            .push(Line::from_spans(vec![Span::styled("└─", border_style)]));
        self.ensure_blank_line();
        if language == Some("mermaid") {
            while self
                .rows
                .len()
                .saturating_sub(geometry_start.map_or(0, |value| value.0))
                < self.mermaid_reserved_rows
            {
                self.rows.push(Line::from_spans(vec![Span::raw(" ")]));
            }
        }
        if let Some((start_row, start_column, geometry_start)) = geometry_start {
            self.geometry.borrow_mut().truncate(geometry_start);
            self.record_geometry("mermaid", start_row, start_column);
        }
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
        let nested_width = self.width.saturating_sub(marker_width);
        let mut nested = self.nested(
            nested_width,
            self.origin_x.saturating_add(marker_width),
            self.origin_y.saturating_add(self.rows.len()),
        );
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

        let alignments = self
            .table_alignments
            .get(self.next_table_index)
            .cloned()
            .unwrap_or_default();
        self.next_table_index = self.next_table_index.saturating_add(1);
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
            let visual_height = row.cells.iter().map(Vec::len).max().unwrap_or(1);
            for visual_row in 0..visual_height {
                self.rows.push(table_content_line(
                    &row.cells,
                    visual_row,
                    &widths,
                    &alignments,
                    border_style,
                    row.header.then_some(self.theme.strong),
                ));
            }
            if row_index + 1 < rows.len() {
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
        let (headers, body_rows) = rows
            .split_first()
            .filter(|(first, _)| first.header)
            .map_or((None, rows), |(header, body)| (Some(&header.cells), body));
        for (row_index, row) in body_rows.iter().enumerate() {
            if row_index > 0 {
                self.rows.push(Line::default());
            }
            for (column_index, cell) in row.cells.iter().enumerate() {
                let label = headers
                    .and_then(|cells| cells.get(column_index))
                    .map(|lines| {
                        lines
                            .first()
                            .into_iter()
                            .flatten()
                            .map(|span| span.content.as_str())
                            .collect::<String>()
                    })
                    .filter(|label| !label.is_empty())
                    .unwrap_or_else(|| column_index.saturating_add(1).to_string());
                for (visual_row, line) in cell.iter().enumerate() {
                    let prefix = if visual_row == 0 {
                        format!("{label}: ")
                    } else {
                        " ".repeat(text_display_width(&label).saturating_add(2))
                    };
                    let mut spans = vec![Span::styled(prefix, muted)];
                    spans.extend(line.clone());
                    self.rows.push(Line::from_spans(spans));
                }
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

fn markdown_cell_rect(column: usize, row: usize, width: usize) -> MarkdownCellRect {
    MarkdownCellRect {
        x: u16::try_from(column).unwrap_or(u16::MAX),
        y: u16::try_from(row).unwrap_or(u16::MAX),
        width: u16::try_from(width).unwrap_or(u16::MAX),
        height: 1,
    }
}

fn decimal_digits(value: usize) -> usize {
    value.max(1).to_string().len()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TableRow {
    cells: Vec<Vec<Vec<Span>>>,
    header: bool,
}

fn table_rows(container: &Container, style: TextStyle, theme: MarkdownTheme) -> Vec<TableRow> {
    let mut rows = Vec::new();
    collect_table_rows(container, style, theme, false, &mut rows);
    rows
}

fn collect_table_rows(
    container: &Container,
    style: TextStyle,
    theme: MarkdownTheme,
    in_header: bool,
    rows: &mut Vec<TableRow>,
) {
    let style = style.merge_container(container, theme);
    let in_header = in_header || matches!(container.element, Element::THead);
    let cells = container
        .children
        .iter()
        .filter(|child| matches!(child.element, Element::TH { .. } | Element::TD { .. }))
        .map(|cell| lines_for_table_cell(cell, style, theme))
        .collect::<Vec<_>>();
    if matches!(container.element, Element::TR) || in_header && !cells.is_empty() {
        rows.push(TableRow {
            cells,
            header: in_header,
        });
        return;
    }
    for child in &container.children {
        collect_table_rows(child, style, theme, in_header, rows);
    }
}

fn lines_for_table_cell(
    container: &Container,
    style: TextStyle,
    theme: MarkdownTheme,
) -> Vec<Vec<Span>> {
    let mut renderer =
        TerminalMarkdownRenderer::new(u16::MAX, theme, None, Vec::new(), Vec::new(), 1, 1);
    renderer.in_table_collection = true;
    renderer.render_container_children(container, style.merge_container(container, theme));
    renderer.flush_line();
    let lines = renderer
        .finish()
        .into_iter()
        .map(|line| line.spans)
        .collect::<Vec<_>>();
    if lines.is_empty() {
        vec![Vec::new()]
    } else {
        lines
    }
}

fn table_column_widths(rows: &[TableRow], column_count: usize) -> Vec<usize> {
    let mut widths = vec![1; column_count];
    for row in rows {
        for (index, cell) in row.cells.iter().enumerate() {
            for line in cell {
                widths[index] = widths[index].max(spans_width(line));
            }
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

fn table_content_line(
    row: &[Vec<Vec<Span>>],
    visual_row: usize,
    widths: &[usize],
    alignments: &[Alignment],
    border_style: Style,
    cell_style: Option<Style>,
) -> Line {
    let mut spans = vec![Span::styled("│", border_style)];
    for (index, width) in widths.iter().enumerate() {
        spans.push(Span::raw(" "));
        if let Some(cell) = row.get(index).and_then(|lines| lines.get(visual_row)) {
            let padding = width.saturating_sub(spans_width(cell));
            let (left_padding, right_padding) = aligned_padding(
                padding,
                alignments.get(index).copied().unwrap_or(Alignment::None),
            );
            if left_padding > 0 {
                spans.push(Span::raw(" ".repeat(left_padding)));
            }
            spans.extend(cell.iter().cloned().map(|mut span| {
                if let Some(style) = cell_style {
                    span.style = span.style.patch(style);
                }
                span
            }));
            if right_padding > 0 {
                spans.push(Span::raw(" ".repeat(right_padding)));
            }
        } else {
            spans.push(Span::raw(" ".repeat(*width)));
        }
        spans.push(Span::raw(" "));
        spans.push(Span::styled("│", border_style));
    }
    Line::from_spans(spans)
}

const fn aligned_padding(padding: usize, alignment: Alignment) -> (usize, usize) {
    match alignment {
        Alignment::Center => {
            let left = padding / 2;
            (left, padding - left)
        }
        Alignment::Right => (padding, 0),
        Alignment::None | Alignment::Left => (0, padding),
    }
}

const fn alert_label(kind: BlockQuoteKind) -> &'static str {
    match kind {
        BlockQuoteKind::Note => "ⓘ NOTE",
        BlockQuoteKind::Tip => "◆ TIP",
        BlockQuoteKind::Important => "❗ IMPORTANT",
        BlockQuoteKind::Warning => "⚠ WARNING",
        BlockQuoteKind::Caution => "⛔ CAUTION",
    }
}

fn strip_alert_marker(rows: &mut Vec<Line>, kind: BlockQuoteKind) {
    let marker = match kind {
        BlockQuoteKind::Note => "[!NOTE]",
        BlockQuoteKind::Tip => "[!TIP]",
        BlockQuoteKind::Important => "[!IMPORTANT]",
        BlockQuoteKind::Warning => "[!WARNING]",
        BlockQuoteKind::Caution => "[!CAUTION]",
    };
    let Some(first) = rows.first_mut() else {
        return;
    };
    let mut remaining = marker.len();
    while remaining > 0 && !first.spans.is_empty() {
        let span = &mut first.spans[0];
        let consumed = remaining.min(span.content.len());
        span.content.replace_range(..consumed, "");
        remaining -= consumed;
        if span.content.is_empty() {
            first.spans.remove(0);
        }
    }
    if first.spans.is_empty() {
        rows.remove(0);
    }
}

fn apply_code_block_syntax_highlighting(
    language: &str,
    code_rows: &mut [Line],
    theme: MarkdownTheme,
    syntax: Option<SyntaxPalette>,
) {
    let highlighter = syntax.map_or_else(SyntaxHighlighter::new, SyntaxHighlighter::with_palette);
    if !highlighter.can_highlight(language) {
        return;
    }
    let text = code_rows
        .iter()
        .map(|row| {
            row.spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    let lines = text.iter().map(String::as_str).collect::<Vec<_>>();

    for (row, highlighted) in code_rows
        .iter_mut()
        .zip(highlighter.highlight_lines(language, &lines))
    {
        row.spans = highlighted
            .into_iter()
            .map(|span| Span::styled(span.content, theme.code_block_text.patch(span.style)))
            .collect();
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
    text.split('\t')
        .map(UnicodeWidthStr::width)
        .sum::<usize>()
        .saturating_add(text.matches('\t').count().saturating_mul(4))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        MarkdownContribution, MarkdownContributionKind, MarkdownDestination, MarkdownRenderOptions,
        MarkdownStreamingRenderState, MarkdownTheme, MermaidContributionRendering, TableRow,
        aligned_padding, markdown_stage_diagnostics, markdown_to_plain_text, markdown_work_counts,
        render_markdown, render_markdown_lines, reset_markdown_work_counts, table_column_widths,
        table_content_line, text_display_width,
    };
    use bmux_tui::prelude::{Color, Modifier, Span, Style};
    use pulldown_cmark::Alignment;
    use unicode_segmentation::UnicodeSegmentation;

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
    fn multiline_table_cells_align_each_visual_row_independently() {
        let rows = [TableRow {
            cells: vec![
                vec![vec![Span::raw("a")], vec![Span::raw("long")]],
                vec![vec![Span::raw("x")], vec![Span::raw("yy")]],
                vec![vec![Span::raw("7")], vec![Span::raw("42")]],
            ],
            header: false,
        }];
        let widths = table_column_widths(&rows, 3);
        assert_eq!(widths, [4, 2, 2]);
        let alignments = [Alignment::Left, Alignment::Center, Alignment::Right];
        let line = |visual_row| {
            table_content_line(
                &rows[0].cells,
                visual_row,
                &widths,
                &alignments,
                Style::new(),
                None,
            )
            .spans
            .into_iter()
            .map(|span| span.content)
            .collect::<String>()
        };
        assert_eq!(line(0), "│ a    │ x  │  7 │");
        assert_eq!(line(1), "│ long │ yy │ 42 │");
        assert_eq!(aligned_padding(3, alignments[1]), (1, 2));
    }

    #[test]
    fn details_disclosure_defaults_and_explicit_state_control_visible_body() {
        let source = "<details><summary>Retry</summary>Run it again.</details>";
        let closed = render_markdown(
            source,
            &MarkdownRenderOptions::new(80).with_details_open(BTreeMap::new()),
        );
        let closed_text = closed
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(closed_text.contains("▶ Retry"));
        assert!(!closed_text.contains("Run it again."));

        let details_id = closed
            .contributions
            .iter()
            .find(|item| matches!(item.kind, MarkdownContributionKind::Details { .. }))
            .expect("details contribution")
            .id
            .clone();
        let open = render_markdown(
            source,
            &MarkdownRenderOptions::new(80).with_details_open(BTreeMap::from([(details_id, true)])),
        );
        let open_text = open
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(open_text.contains("▼ Retry"));
        assert!(open_text.contains("Run it again."));
        assert_ne!(closed.layout_signature, open.layout_signature);
    }

    #[test]
    fn details_open_attribute_initializes_expanded_state_and_can_be_overridden() {
        let source = "<details open><summary>More</summary>Visible body.</details>";
        let default = render_markdown(
            source,
            &MarkdownRenderOptions::new(80).with_details_open(BTreeMap::new()),
        );
        let default_text = default
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(default_text.contains("▼ More"));
        assert!(default_text.contains("Visible body."));

        let details_id = default.contributions[0].id.clone();
        let collapsed = render_markdown(
            source,
            &MarkdownRenderOptions::new(80)
                .with_details_open(BTreeMap::from([(details_id, false)])),
        );
        let collapsed_text = collapsed
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(collapsed_text.contains("▶ More"));
        assert!(!collapsed_text.contains("Visible body."));
    }

    #[test]
    fn nested_details_have_independent_disclosure_state_and_geometry() {
        let source = "<details><summary>Outer</summary>Before <details><summary>Inner</summary>Secret</details> After</details>";
        let initial = render_markdown(
            source,
            &MarkdownRenderOptions::new(80).with_details_open(BTreeMap::new()),
        );
        let detail_ids = initial
            .contributions
            .iter()
            .filter(|item| matches!(item.kind, MarkdownContributionKind::Details { .. }))
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(detail_ids.len(), 2);
        let initial_text = initial
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(initial_text.contains("▶ Outer"));
        assert!(!initial_text.contains("Inner"));

        let outer_open = render_markdown(
            source,
            &MarkdownRenderOptions::new(80)
                .with_details_open(BTreeMap::from([(detail_ids[0].clone(), true)])),
        );
        let outer_text = outer_open
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(outer_text.contains("▼ Outer"));
        assert!(outer_text.contains("▶ Inner"));
        assert!(!outer_text.contains("Secret"));
        assert_eq!(
            outer_open
                .geometry
                .iter()
                .filter(|geometry| detail_ids.contains(&geometry.contribution_id))
                .count(),
            2
        );

        let both_open = render_markdown(
            source,
            &MarkdownRenderOptions::new(80).with_details_open(BTreeMap::from([
                (detail_ids[0].clone(), true),
                (detail_ids[1].clone(), true),
            ])),
        );
        let both_text = both_open
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(both_text.contains("▼ Outer"));
        assert!(both_text.contains("▼ Inner"));
        assert!(both_text.contains("Secret"));
        assert_ne!(outer_open.layout_signature, both_open.layout_signature);
    }

    #[test]
    fn escaped_and_nested_link_labels_preserve_typed_destination_and_readable_fallback() {
        let rendered = render_markdown(
            r"[Escaped \[label\] with **strong** and `code`](docs/guide.md)",
            &MarkdownRenderOptions::new(32).with_link_destination_fallbacks(true),
        );
        let link = rendered
            .contributions
            .iter()
            .find_map(|item| match &item.kind {
                MarkdownContributionKind::Link {
                    label, destination, ..
                } => Some((label, destination)),
                _ => None,
            })
            .expect("link contribution");
        assert_eq!(link.0, "Escaped [label] with strong and code");
        assert_eq!(
            link.1,
            &MarkdownDestination::UnresolvedRelative("docs/guide.md".to_owned())
        );
        let visible = rendered
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(visible.contains("Escaped [label] with strong and code"));
        assert!(visible.contains("docs/guide.md"));
        assert!(rendered.geometry[0].rects.len() >= 2);
    }

    #[test]
    fn internal_heading_anchorse_github_slugs_duplicates_and_explicit_ids() {
        let source = "# Hello, World!\n\n## Hello World\n\n### Custom {#chosen}";
        let rendered = render_markdown(source, &MarkdownRenderOptions::new(80));
        assert_eq!(
            rendered
                .anchors
                .iter()
                .map(|anchor| (anchor.fragment.as_str(), anchor.row))
                .collect::<Vec<_>>(),
            [("hello-world", 0), ("hello-world-1", 2), ("chosen", 4)]
        );
    }

    #[test]
    fn links_append_readable_safe_or_unresolved_destination_fallbacks() {
        let visible = |source: &str, options: MarkdownRenderOptions| {
            render_markdown(source, &options)
                .lines
                .iter()
                .flat_map(|line| &line.spans)
                .map(|span| span.content.as_str())
                .collect::<String>()
        };
        assert!(
            visible(
                "[Guide](https://example.com/docs)",
                MarkdownRenderOptions::new(80).with_link_destination_fallbacks(true),
            )
            .contains("Guide (https://example.com/docs)")
        );
        assert!(
            visible(
                "[Guide](docs.md)",
                MarkdownRenderOptions::new(80).with_link_destination_fallbacks(true),
            )
            .contains("Guide (docs.md)")
        );
        assert!(
            visible(
                "[Section](#target)",
                MarkdownRenderOptions::new(80).with_link_destination_fallbacks(true),
            )
            .contains("Section (#target)")
        );
        let unsafe_visible = visible(
            "[Secret](javascript:alert(1))",
            MarkdownRenderOptions::new(80).with_link_destination_fallbacks(true),
        );
        assert!(unsafe_visible.contains("Secret"));
        assert!(!unsafe_visible.contains("javascript"));
    }

    #[test]
    fn plain_text_projection_uses_markdown_semantics() {
        assert_eq!(
            markdown_to_plain_text(
                "# Request\n\n- Use **care**\n- Run `cargo test`\n\n[Guide](https://example.com)"
            ),
            "Request Use care Run cargo test Guide"
        );
    }

    #[test]
    fn plain_text_projection_preserves_extension_semantics() {
        assert_eq!(
            markdown_to_plain_text(
                "For $x^2$.[^note]\n\n[^note]: Footnote.\n\n<details><summary>More</summary>Body</details>"
            ),
            "For x^2. [note] Footnote. More Body"
        );
    }

    #[test]
    fn selection_provenance_tracks_wrapped_text_after_visual_prefixes() {
        let rendered = render_markdown(
            "> 1. a long **emphasized value** that wraps across terminal rows",
            &MarkdownRenderOptions::new(18),
        );
        let emphasized = rendered
            .selection_provenance
            .iter()
            .filter(|unit| unit.kind == super::MarkdownSelectionKind::Text)
            .filter(|unit| {
                unit.source_ranges.iter().any(|range| {
                    "> 1. a long **emphasized value** that wraps across terminal rows"
                        [range.clone()]
                    .chars()
                    .all(|character| "emphasized value".contains(character))
                })
            })
            .collect::<Vec<_>>();

        assert!(!emphasized.is_empty());
        assert!(
            emphasized
                .iter()
                .flat_map(|unit| &unit.rects)
                .next()
                .is_some()
        );
        assert!(
            emphasized
                .iter()
                .flat_map(|unit| &unit.rects)
                .all(|rect| rect.x >= 2)
        );
    }

    #[test]
    fn selection_provenance_tracks_code_through_syntax_highlighting() {
        let markdown = "```rust\nfn main() {\n\tprintln!(\"hi\");\n}\n```";
        let rendered = render_markdown(markdown, &MarkdownRenderOptions::new(80));
        let code = rendered
            .selection_provenance
            .iter()
            .filter(|unit| {
                matches!(
                    unit.kind,
                    super::MarkdownSelectionKind::Text | super::MarkdownSelectionKind::Code
                ) && unit
                    .source_ranges
                    .iter()
                    .any(|range| range.start >= 8 && range.end <= markdown.len().saturating_sub(3))
            })
            .collect::<Vec<_>>();

        assert!(code.iter().flat_map(|unit| &unit.rects).next().is_some());
        assert_eq!(
            code.iter()
                .flat_map(|unit| &unit.source_ranges)
                .map(|range| &markdown[range.clone()])
                .collect::<String>(),
            "fn main() {\n\tprintln!(\"hi\");\n}\n"
        );
    }

    #[test]
    fn renders_code_block_with_generic_syntax_highlighting() {
        let rows =
            render_markdown_lines("```rust\nfn main() {}\n```", MarkdownRenderOptions::new(80));

        assert!(
            rows.iter()
                .flat_map(|line| &line.spans)
                .any(|span| { !span.content.trim().is_empty() && span.style.fg.is_some() })
        );
    }

    #[test]
    fn highlights_toml_and_nix_code_blocks_without_changing_text() {
        for (language, source) in [
            ("toml", "description = \"\"\"first\nsecond\"\"\""),
            ("nix", "let value = ''first\nsecond''; in value"),
        ] {
            let markdown = format!("```{language}\n{source}\n```");
            let rows = render_markdown_lines(&markdown, MarkdownRenderOptions::new(80));
            let output = rows
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_str())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");

            for source_line in source.lines() {
                assert!(
                    output.contains(source_line),
                    "missing {source_line:?} in {output:?}"
                );
            }
            assert!(
                rows.iter().flat_map(|line| &line.spans).any(|span| !span
                    .content
                    .trim()
                    .is_empty()
                    && span.style.fg.is_some()),
                "expected highlighted {language} spans"
            );
        }
    }

    #[test]
    fn code_block_selection_distinguishes_exact_fence_body_and_footer() {
        let markdown = "```rust\r\nfn main() {\r\n\tprintln!(\"hi\");\r\n}\r\n```\r\n";
        let rendered = render_markdown(markdown, &MarkdownRenderOptions::new(80));
        let block = rendered.code_block_selections.first().expect("code block");

        assert_eq!(block.kind, super::MarkdownCodeBlockSelectionKind::Fenced);
        assert_eq!(
            &markdown[block.header_range.clone().expect("header")],
            "```rust\r\n"
        );
        assert_eq!(
            block
                .body_ranges
                .iter()
                .map(|range| &markdown[range.clone()])
                .collect::<String>(),
            "fn main() {\r\n\tprintln!(\"hi\");\r\n}\r\n"
        );
        assert_eq!(
            &markdown[block.footer_range.clone().expect("footer")],
            "```\r\n"
        );
        assert_eq!(&markdown[block.whole_range.clone()], markdown);
    }

    #[test]
    fn code_block_selection_preserves_indentation_and_incomplete_fences() {
        let indented = "    first\n\tsecond\n\nnext";
        let rendered = render_markdown(indented, &MarkdownRenderOptions::new(80));
        let block = rendered
            .code_block_selections
            .first()
            .expect("indented block");
        assert_eq!(block.kind, super::MarkdownCodeBlockSelectionKind::Indented);
        assert!(block.header_range.is_none());
        assert!(block.footer_range.is_none());
        assert_eq!(
            &indented[block.whole_range.clone()],
            "    first\n\tsecond\n\n"
        );

        let incomplete = "~~~rust\nlet value = 1;";
        let rendered = render_markdown(incomplete, &MarkdownRenderOptions::new(80));
        let block = rendered
            .code_block_selections
            .first()
            .expect("incomplete block");
        assert!(block.footer_range.is_none());
        assert_eq!(&incomplete[block.whole_range.clone()], incomplete);
        assert_eq!(
            block
                .body_ranges
                .iter()
                .map(|range| &incomplete[range.clone()])
                .collect::<String>(),
            "let value = 1;"
        );
    }

    #[test]
    fn selection_provenance_preserves_parser_source_ranges_and_semantic_kinds() {
        let markdown = "# *hello* [world](https://example.com)\n\n`code`  \nnext";
        let result = render_markdown(markdown, &MarkdownRenderOptions::new(40));

        assert!(!result.selection_provenance.is_empty());
        assert!(result.selection_provenance.iter().all(|entry| {
            entry
                .rects
                .iter()
                .all(|rect| rect.width > 0 && rect.height > 0)
                && entry.source_ranges.iter().all(|range| {
                    range.start <= range.end
                        && markdown.is_char_boundary(range.start)
                        && markdown.is_char_boundary(range.end)
                })
        }));
        for kind in [
            super::MarkdownSelectionKind::Start,
            super::MarkdownSelectionKind::End,
            super::MarkdownSelectionKind::Text,
            super::MarkdownSelectionKind::Code,
            super::MarkdownSelectionKind::HardBreak,
        ] {
            assert!(
                result
                    .selection_provenance
                    .iter()
                    .any(|entry| entry.kind == kind),
                "missing {kind:?}"
            );
        }
        assert!(result.selection_provenance.iter().any(|entry| {
            entry.source_ranges.iter().any(|range| {
                markdown
                    .get(range.clone())
                    .is_some_and(|source| source.contains("https://example.com"))
            })
        }));
    }

    #[test]
    fn block_code_provenance_never_invents_source_bytes() {
        for markdown in [
            "```rust\nfn main() {}\n```",
            "    indented\tcode\n",
            "```\npartial",
        ] {
            let result = render_markdown(markdown, &MarkdownRenderOptions::new(20));
            assert!(result.selection_provenance.iter().all(|entry| {
                entry.source_ranges.iter().all(|range| {
                    range.end <= markdown.len()
                        && markdown.is_char_boundary(range.start)
                        && markdown.is_char_boundary(range.end)
                })
            }));
            if markdown.starts_with("```") {
                assert!(result.selection_provenance.iter().any(|entry| {
                    entry
                        .semantic_expansion
                        .as_deref()
                        .is_some_and(|id| id.starts_with("code-block:"))
                }));
            }
        }
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
        assert!(output.contains("    ┌─ rust"));
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
    fn append_state_matches_full_render_for_plain_paragraph_prefixes() {
        let source = "First paragraph grows here.\n\nSecond paragraph streams progressively.";
        let options = MarkdownRenderOptions::new(24).with_streaming(true);
        let mut state = MarkdownStreamingRenderState::default();
        for end in source
            .char_indices()
            .map(|(index, character)| index.saturating_add(character.len_utf8()))
        {
            let prefix = &source[..end];
            assert_eq!(
                state.render(prefix, &options),
                render_markdown(prefix, &options)
            );
        }
    }

    #[test]
    fn append_state_falls_back_exactly_for_retroactive_or_incompatible_changes() {
        let cases = [
            "# heading\n\nparagraph",
            "paragraph\n\n- list item",
            "paragraph\n\n1. ordered item",
            "paragraph\n\n| a | b |\n| - | - |",
            "paragraph\n\n```rust\nfn main() {}\n```",
            "paragraph [link](https://example.com)",
            "paragraph\n\n<details><summary>More</summary>Body</details>",
            "paragraph[^note]\n\n[^note]: Definition",
            "paragraph with $x^2$ math",
            "```mermaid\nflowchart LR\nA --> B\n```",
            "Unicode 東京 🧪 and é.\n\nNext paragraph.",
            "hard break  \nnext line",
        ];
        for source in cases {
            let options = MarkdownRenderOptions::new(32).with_streaming(true);
            let mut state = MarkdownStreamingRenderState::default();
            for end in source
                .char_indices()
                .map(|(index, character)| index.saturating_add(character.len_utf8()))
            {
                let prefix = &source[..end];
                assert_eq!(
                    state.render(prefix, &options),
                    render_markdown(prefix, &options)
                );
            }
            let finalized = MarkdownRenderOptions::new(32);
            assert_eq!(
                state.render(source, &finalized),
                render_markdown(source, &finalized)
            );
        }
    }

    #[test]
    fn append_state_work_scales_with_unstable_suffix_not_total_history() {
        let paragraph =
            "a stable paragraph with enough text to represent a realistic streamed block.";
        let source = (0..64)
            .map(|index| format!("{paragraph} {index}."))
            .collect::<Vec<_>>()
            .join("\n\n");
        let options = MarkdownRenderOptions::new(80).with_streaming(true);
        let mut state = MarkdownStreamingRenderState::default();
        let mut current = String::new();
        for block in source.split_inclusive("\n\n") {
            current.push_str(block);
            let _ = state.render(&current, &options);
        }
        let processed = state.processed_source_bytes();
        assert!(
            processed <= source.len().saturating_mul(3),
            "incremental renderer processed {processed} bytes for {} accepted bytes",
            source.len()
        );
    }

    #[test]
    fn append_state_falls_back_for_replacement_and_width_changes() {
        let mut state = MarkdownStreamingRenderState::default();
        let streaming = MarkdownRenderOptions::new(20).with_streaming(true);
        let _ = state.render("one paragraph", &streaming);
        let replaced = state.render("replacement", &streaming);
        assert_eq!(replaced, render_markdown("replacement", &streaming));

        let resized = MarkdownRenderOptions::new(10).with_streaming(true);
        let result = state.render("replacement grows", &resized);
        assert_eq!(result, render_markdown("replacement grows", &resized));
        assert!(state.stable_source_len <= state.source.len());
    }

    #[test]
    fn stage_diagnostics_cover_every_full_render_stage() {
        reset_markdown_work_counts();
        let _ = render_markdown(
            "# Heading\n\n[link](https://example.com)\n\n```rust\nfn main() {}\n```",
            &MarkdownRenderOptions::new(80),
        );
        let diagnostics = markdown_stage_diagnostics();
        assert_eq!(
            [
                diagnostics.semantic_parse,
                diagnostics.fallback_projection,
                diagnostics.hyperchad_conversion,
                diagnostics.syntax_highlighting,
                diagnostics.terminal_layout,
                diagnostics.geometry,
                diagnostics.anchors,
                diagnostics.signature,
            ]
            .len(),
            8
        );
        assert_eq!(markdown_work_counts(), (1, 1));
    }

    #[test]
    fn plain_document_uses_one_semantic_parse_and_one_terminal_projection() {
        reset_markdown_work_counts();
        let result = render_markdown(
            "# Heading\n\nPlain paragraph with **strong** text.",
            &MarkdownRenderOptions::new(80),
        );

        assert!(!result.lines.is_empty());
        assert_eq!(markdown_work_counts(), (1, 1));
    }

    #[test]
    fn heading_and_projected_contribution_density_do_not_add_projection_passes() {
        for count in [1, 8, 64] {
            let headings = (0..count)
                .map(|index| format!("## Heading {index}"))
                .collect::<Vec<_>>()
                .join("\n\n");
            reset_markdown_work_counts();
            let result = render_markdown(&headings, &MarkdownRenderOptions::new(80));
            assert_eq!(result.anchors.len(), count);
            assert_eq!(markdown_work_counts(), (1, 1));

            let footnotes = (0..count)
                .map(|index| {
                    format!("reference [^note-{index}]\n\n[^note-{index}]: definition {index}")
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            reset_markdown_work_counts();
            let result = render_markdown(&footnotes, &MarkdownRenderOptions::new(80));
            assert!(result.geometry.len() >= count.saturating_mul(2));
            let (parses, projections) = markdown_work_counts();
            assert!(parses <= 2, "unexpected parse amplification: {parses}");
            assert_eq!(projections, 1);
        }
    }

    #[test]
    fn representative_geometry_matrix_is_bounded_at_narrow_and_wide_widths() {
        let source = concat!(
            "# Heading\n\n",
            "> [!NOTE]\n> [quoted link](https://example.com).\n\n",
            "<details><summary>More</summary>Body</details>\n\n",
            "Text[^note] and ![image](https://example.com/image.png).\n\n",
            "[^note]: Definition.\n\n",
            "| left | right |\n| --- | ---: |\n| one | two |\n\n",
            "Inline $x^2$.\n\n```mermaid\nflowchart LR\nA --> B\n```",
        );
        for width in [12, 80] {
            let result = render_markdown(
                source,
                &MarkdownRenderOptions::new(width)
                    .with_document_id("matrix")
                    .with_mermaid_contributions(1600, 1200),
            );
            assert!(!result.lines.is_empty());
            assert!(result.geometry.iter().all(|geometry| {
                !geometry.rects.is_empty()
                    && geometry.rects.iter().all(|rect| {
                        rect.width > 0
                            && rect.height == 1
                            && usize::from(rect.y) < result.lines.len()
                    })
            }));
            for kind in [
                "link",
                "details",
                "footnote-reference",
                "footnote-definition",
                "image",
                "mermaid",
            ] {
                assert!(
                    result
                        .geometry
                        .iter()
                        .any(|geometry| geometry.contribution_id.contains(&format!(":{kind}:"))),
                    "missing {kind} geometry at width {width}: {:?}",
                    result.geometry
                );
            }
        }
    }

    #[test]
    fn custom_theme_styles_alert_label_and_bar() {
        let alert_style = Style::new().fg(Color::BrightCyan);
        let theme = MarkdownTheme {
            alert_note: alert_style,
            ..MarkdownTheme::default()
        };
        let lines = render_markdown_lines(
            "> [!NOTE]\n> themed",
            MarkdownRenderOptions::new(80).with_theme(theme),
        );

        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| { span.content == "│ " && span.style == alert_style })
        );
        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content == "ⓘ NOTE"
                && span.style == alert_style.patch(Style::new().add_modifier(Modifier::BOLD))
        }));
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
            span.content.contains("┌─ rust") && span.style == Style::new().fg(Color::Magenta)
        }));
    }

    fn geometry_text(result: &super::MarkdownRenderResult, index: usize) -> String {
        let geometry = &result.geometry[index];
        geometry
            .rects
            .iter()
            .filter_map(|rect| {
                result
                    .lines
                    .get(usize::from(rect.y))
                    .map(|line| (line, rect))
            })
            .map(|(line, rect)| {
                let text = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>();
                let start = usize::from(rect.x);
                let end = start.saturating_add(usize::from(rect.width));
                let mut column = 0_usize;
                text.graphemes(true)
                    .filter(|grapheme| {
                        let grapheme_start = column;
                        column = column.saturating_add(text_display_width(grapheme));
                        grapheme_start >= start && column <= end
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn contribution_geometry_tracks_wrapped_unicode_link_cells() {
        let result = render_markdown(
            "[a界b](https://example.com)",
            &MarkdownRenderOptions::new(3).with_document_id("item-1"),
        );
        assert_eq!(result.geometry.len(), 1);
        assert_eq!(result.geometry[0].contribution_id, "item-1:link:0:28");
        assert_eq!(
            result.geometry[0]
                .rects
                .iter()
                .map(|rect| (rect.x, rect.y, rect.width, rect.height))
                .collect::<Vec<_>>(),
            [(0, 0, 3, 1), (0, 1, 1, 1)]
        );
    }

    #[test]
    fn contribution_geometry_disambiguates_duplicate_link_labels() {
        let result = render_markdown(
            "[same](https://one.example) and [same](https://two.example)",
            &MarkdownRenderOptions::new(80),
        );
        assert_eq!(result.geometry.len(), 2);
        assert_eq!(result.geometry[0].rects[0].x, 0);
        assert!(result.geometry[1].rects[0].x > result.geometry[0].rects[0].x);
        assert_ne!(
            result.geometry[0].contribution_id,
            result.geometry[1].contribution_id
        );
    }

    #[test]
    fn rich_reservations_are_stable_and_geometry_affecting() {
        let image = render_markdown(
            "![alt](image.png)",
            &MarkdownRenderOptions::new(40).with_rich_reserved_rows(4, 8),
        );
        assert_eq!(image.lines.len(), 4);
        assert_eq!(image.geometry[0].rects[0].height, 1);
        let image_bottom = image.geometry[0]
            .rects
            .iter()
            .map(|rect| rect.y.saturating_add(rect.height))
            .max()
            .unwrap();
        assert_eq!(image_bottom, 4);

        let mermaid = render_markdown(
            "```mermaid\nflowchart LR\nA --> B\n```",
            &MarkdownRenderOptions::new(40)
                .with_mermaid_contributions(1600, 1200)
                .with_rich_reserved_rows(4, 8),
        );
        let mermaid_geometry = &mermaid.geometry[0];
        let mermaid_bottom = mermaid_geometry
            .rects
            .iter()
            .map(|rect| rect.y.saturating_add(rect.height))
            .max()
            .unwrap();
        assert!(mermaid_bottom >= 8);

        let compact = render_markdown(
            "![alt](image.png)",
            &MarkdownRenderOptions::new(40).with_rich_reserved_rows(1, 1),
        );
        assert_ne!(image.layout_signature, compact.layout_signature);
    }

    #[test]
    fn asynchronous_mermaid_mode_emits_typed_request_without_inline_rendering() {
        let source = "```mermaid\nflowchart LR\nA --> B\n```";
        let disabled = render_markdown(source, &MarkdownRenderOptions::new(40));
        assert!(matches!(
            disabled.contributions.as_slice(),
            [MarkdownContribution {
                kind: MarkdownContributionKind::Mermaid {
                    rendering: MermaidContributionRendering::Disabled,
                    ..
                },
                ..
            }]
        ));

        let asynchronous = render_markdown(
            source,
            &MarkdownRenderOptions::new(40).with_mermaid_contributions(1600, 1200),
        );
        assert!(matches!(
            asynchronous.contributions.as_slice(),
            [MarkdownContribution {
                kind: MarkdownContributionKind::Mermaid {
                    rendering: MermaidContributionRendering::Disabled,
                    ..
                },
                ..
            }]
        ));
        assert_eq!(asynchronous.geometry.len(), 1);
    }

    #[test]
    fn contribution_geometry_tracks_details_footnotes_and_mermaid() {
        let result = render_markdown(
            "<details><summary>More</summary>Body</details>\n\nText[^note].\n\n[^note]: Definition.\n\n```mermaid\nflowchart LR\nA --> B\n```",
            &MarkdownRenderOptions::new(40)
                .with_document_id("item")
                .with_mermaid_contributions(1600, 1200),
        );
        let ids = result
            .geometry
            .iter()
            .map(|geometry| geometry.contribution_id.as_str())
            .collect::<Vec<_>>();
        for kind in [
            "details",
            "footnote-reference",
            "footnote-definition",
            "mermaid",
        ] {
            assert!(
                ids.iter()
                    .any(|id| id.starts_with(&format!("item:{kind}:"))),
                "missing {kind}: {ids:?}"
            );
        }
        assert!(result.geometry.iter().all(|item| !item.rects.is_empty()));
    }

    #[test]
    fn geometry_is_deterministic_across_widths_and_stream_replacement() {
        let markdown = "Prefix [nested **label**](https://example.com) suffix";
        let wide = render_markdown(
            markdown,
            &MarkdownRenderOptions::new(80).with_document_id("stream"),
        );
        let narrow = render_markdown(
            markdown,
            &MarkdownRenderOptions::new(12).with_document_id("stream"),
        );
        assert_eq!(
            wide.geometry[0].contribution_id,
            narrow.geometry[0].contribution_id
        );
        assert_eq!(geometry_text(&wide, 0), "nested label");
        assert_eq!(geometry_text(&narrow, 0).replace('\n', ""), "nested label");
        assert_ne!(wide.geometry[0].rects, narrow.geometry[0].rects);

        let appended = render_markdown(
            &format!("{markdown} trailing stream text"),
            &MarkdownRenderOptions::new(80)
                .with_document_id("stream")
                .with_streaming(true),
        );
        assert_eq!(wide.geometry[0], appended.geometry[0]);

        let replaced = render_markdown(
            "Prefix [changed](https://example.com) suffix",
            &MarkdownRenderOptions::new(80)
                .with_document_id("stream")
                .with_streaming(true),
        );
        assert_ne!(
            wide.geometry[0].contribution_id,
            replaced.geometry[0].contribution_id
        );
        assert_eq!(geometry_text(&replaced, 0), "changed");
    }

    #[test]
    fn details_and_footnote_geometry_tracks_projected_visible_cells() {
        let markdown = "<details><summary>Retry **algorithm**</summary>Body</details>\n\nText[^note].\n\n[^note]: Definition body.";
        for width in [80, 12] {
            let result = render_markdown(
                markdown,
                &MarkdownRenderOptions::new(width).with_document_id("item"),
            );
            let details_index = result
                .geometry
                .iter()
                .position(|item| item.contribution_id.starts_with("item:details:"))
                .unwrap();
            let reference_index = result
                .geometry
                .iter()
                .position(|item| item.contribution_id.starts_with("item:footnote-reference:"))
                .unwrap();
            let definition_index = result
                .geometry
                .iter()
                .position(|item| {
                    item.contribution_id
                        .starts_with("item:footnote-definition:")
                })
                .unwrap();
            let details_text = geometry_text(&result, details_index);
            assert!(
                details_text.replace('\n', "").ends_with("try algorithm"),
                "{details_text:?}"
            );
            assert_eq!(geometry_text(&result, reference_index), "[1]");
            assert_eq!(geometry_text(&result, definition_index), "1");
        }
    }

    #[test]
    fn nested_link_geometryes_document_relative_list_and_blockquote_offsets() {
        let result = render_markdown(
            "* before [list](https://example.com/list)\n\n  > [quote](https://example.com/quote)",
            &MarkdownRenderOptions::new(40),
        );
        assert_eq!(result.geometry.len(), 2);
        assert!(result.geometry[0].rects[0].x >= 3);
        assert!(result.geometry[1].rects[0].x >= 5);
        assert!(result.geometry[1].rects[0].y > result.geometry[0].rects[0].y);
    }

    #[test]
    fn contribution_geometry_tracks_complete_image_fallback() {
        let result = render_markdown(
            "![diagram](https://example.com/image.png)",
            &MarkdownRenderOptions::new(80),
        );
        assert_eq!(result.geometry.len(), 1);
        assert_eq!(
            result.geometry[0]
                .rects
                .iter()
                .map(|rect| (rect.x, rect.y, rect.width))
                .collect::<Vec<_>>(),
            [(0, 0, 16)]
        );
    }

    #[test]
    fn options_builder_sets_width_and_theme() {
        let theme = MarkdownTheme {
            horizontal_rule: Style::new().fg(Color::Green),
            ..MarkdownTheme::default()
        };
        let options = MarkdownRenderOptions::new(42).with_theme(theme);

        assert_eq!(options.width, 42);
        assert_eq!(options.theme.horizontal_rule, Style::new().fg(Color::Green));
        assert!(options.document_context.is_none());
        assert!(options.document_id.is_none());
    }

    #[test]
    fn partial_code_fence_renders() {
        let output = render_markdown_lines("```rust\nfn main() {", MarkdownRenderOptions::new(80))
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
