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

use std::{borrow::Cow, cell::Cell, collections::BTreeMap, fmt::Write as _, ops::Range, rc::Rc};

use bcode_mermaid_render::{
    MermaidCancellationToken, MermaidRenderRequest, MermaidRenderedOutput, render_mermaid,
};
use bcode_syntax_render::SyntaxHighlighter;
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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MarkdownDocumentContext {
    /// Base URL for URL-relative destinations.
    pub base_url: Option<Url>,
    /// Base filesystem directory for path-relative destinations.
    pub base_directory: Option<std::path::PathBuf>,
    /// Explicit GitHub repository identity for issue references.
    pub github_repository: Option<GitHubRepository>,
}

/// Explicit GitHub repository identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubRepository {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub name: String,
}

/// Classified destination suitable for explicit activation handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownDestination {
    /// Safe HTTP(S) URL.
    Web(Url),
    /// Trusted local filesystem path.
    LocalPath(std::path::PathBuf),
    /// In-document fragment.
    Fragment(String),
    /// Unsupported or dangerous scheme; remains visible but inert.
    Inert {
        /// Original destination.
        original: String,
        /// Stable reason suitable for UI diagnostics.
        reason: MarkdownDestinationRejection,
    },
    /// Relative destination without trusted context.
    UnresolvedRelative(String),
}

/// Reasons a destination is intentionally inert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownDestinationRejection {
    /// URL scheme is not allowed for activation.
    UnsupportedScheme,
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
            "file" => url.to_file_path().map_or_else(
                |()| MarkdownDestination::Inert {
                    original: destination.to_owned(),
                    reason: MarkdownDestinationRejection::InvalidFileUrl,
                },
                MarkdownDestination::LocalPath,
            ),
            _ => MarkdownDestination::Inert {
                original: destination.to_owned(),
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
        return MarkdownDestination::LocalPath(base_directory.join(destination));
    }
    MarkdownDestination::UnresolvedRelative(destination.to_owned())
}

/// Options controlling terminal Markdown rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownRenderOptions {
    /// Available terminal width in cells.
    pub width: u16,
    /// Theme used for terminal Markdown styles.
    pub theme: MarkdownTheme,
    /// Trusted context used only to resolve relative destinations.
    pub document_context: Option<MarkdownDocumentContext>,
    /// Whether the Mermaid backend should be used for Mermaid fences.
    pub mermaid_enabled: bool,
    /// Maximum Mermaid SVG width in pixels.
    pub mermaid_width: u32,
    /// Maximum Mermaid SVG height in pixels.
    pub mermaid_height: u32,
    /// Stable identity of the owning document or transcript item.
    pub document_id: Option<String>,
}

impl Default for MarkdownRenderOptions {
    /// Create default Markdown render options.
    fn default() -> Self {
        Self {
            width: 80,
            theme: MarkdownTheme::default(),
            document_context: None,
            mermaid_enabled: false,
            mermaid_width: 1600,
            mermaid_height: 1200,
            document_id: None,
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

    /// Return options with bounded Mermaid rendering enabled.
    #[must_use]
    pub const fn with_mermaid(mut self, width: u32, height: u32) -> Self {
        self.mermaid_enabled = true;
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownDocument {
    /// Source-order parser events carrying only Bcode-owned types.
    pub events: Vec<MarkdownSemanticEvent>,
}

/// A semantic Markdown event with its byte range in the original source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownSemanticEvent {
    /// Bcode-owned semantic event kind.
    pub kind: MarkdownSemanticEventKind,
    /// Byte range in the original Markdown source.
    pub source_range: Range<usize>,
}

/// Bcode-owned semantic events needed by terminal rendering and interaction.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Rendered Markdown lines plus semantic contributions for richer consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownRenderResult {
    /// Terminal text projection.
    pub lines: Vec<Line>,
    /// Source-order semantic contributions independent of TUI event-loop types.
    pub contributions: Vec<MarkdownContribution>,
    /// Layout signature including every renderer-owned layout-affecting option.
    pub layout_signature: String,
}

/// Stable semantic contribution emitted alongside terminal lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownContribution {
    /// Stable identity derived from semantic kind and source byte range.
    pub id: String,
    /// Original source byte range.
    pub source_range: Range<usize>,
    /// Contribution payload.
    pub kind: MarkdownContributionKind,
}

/// Rich Markdown contribution payload.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MermaidContributionRendering {
    /// Rendering is disabled; highlighted source remains the fallback.
    Disabled,
    /// Bcode-owned SVG bytes are available for image presentation.
    Svg(Vec<u8>),
    /// Rendering failed; source remains visible with a stable diagnostic.
    Failed(String),
}

/// Render Markdown into terminal lines and semantic contributions.
#[must_use]
pub fn render_markdown(markdown: &str, options: &MarkdownRenderOptions) -> MarkdownRenderResult {
    let document = parse_markdown_document(markdown);
    let details = collect_details_projections(markdown);
    let contributions = markdown_contributions(
        &document,
        options.document_context.as_ref(),
        options.document_id.as_deref(),
        &details,
        options,
    );
    let projected_markdown = project_semantic_fallbacks(markdown, &document, &details);
    let layout_signature = markdown_layout_signature(markdown, options, &contributions);
    MarkdownRenderResult {
        lines: render_markdown_lines_internal(&projected_markdown, options),
        contributions,
        layout_signature,
    }
}

fn markdown_layout_signature(
    markdown: &str,
    options: &MarkdownRenderOptions,
    contributions: &[MarkdownContribution],
) -> String {
    format!(
        "markdown-layout-v1:{}:{}:{}:{}:{}:{}:{}",
        options.width,
        stable_text_hash(markdown),
        stable_text_hash(&format!("{:?}", options.theme)),
        stable_text_hash(&format!("{:?}", options.document_context)),
        options.mermaid_enabled,
        options.mermaid_width,
        stable_text_hash(&format!("{}:{:?}", options.mermaid_height, contributions))
    )
}

fn stable_text_hash(source: &str) -> u64 {
    const OFFSET: u64 = 14_695_981_039_346_656_037;
    const PRIME: u64 = 1_099_511_628_211;
    source.as_bytes().iter().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

fn project_semantic_fallbacks<'a>(
    markdown: &'a str,
    document: &MarkdownDocument,
    details: &[DetailsProjection],
) -> Cow<'a, str> {
    let footnotes = collect_footnotes(markdown, document);
    let math = collect_math_projections(markdown, document);
    if footnotes.references.is_empty()
        && footnotes.definitions.is_empty()
        && math.is_empty()
        && details.is_empty()
    {
        return Cow::Borrowed(markdown);
    }

    let mut replacements = details
        .iter()
        .cloned()
        .map(|details| {
            (
                details.source_range,
                format!(
                    "**{}**\n\n{}",
                    details.summary_markdown.trim(),
                    details.body_markdown.trim()
                ),
            )
        })
        .collect::<Vec<_>>();
    replacements.extend(math.into_iter().map(|projection| {
        let replacement = match projection.kind {
            MathKind::Inline => format!("`{}`", projection.rendered),
            MathKind::Display => format!("\n```text\n{}\n```\n", projection.rendered),
        };
        (projection.source_range, replacement)
    }));
    for reference in &footnotes.references {
        replacements.push((
            reference.source_range.clone(),
            format!("[{}]", reference.number),
        ));
    }
    for definition in footnotes.definitions.values() {
        replacements.push((definition.source_range.clone(), String::new()));
    }
    let mut projected = replace_source_ranges(markdown, replacements);
    if !footnotes.definitions.is_empty() {
        projected.push_str("\n\n---\n\nFootnotes\n\n");
        for definition in footnotes.definitions.values() {
            let _ = write!(
                projected,
                "{}. {}",
                definition.number,
                definition.body.trim()
            );
            if !definition.reference_numbers.is_empty() {
                projected.push_str(" ↩");
                if definition.reference_numbers.len() > 1 {
                    let _ = write!(projected, "×{}", definition.reference_numbers.len());
                }
            }
            projected.push_str("\n\n");
        }
    }
    Cow::Owned(projected)
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
        let Some(close_relative) = lower[open_end..].find("</details>") else {
            break;
        };
        let close_start = open_end + close_relative;
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
            summary_markdown: safe_summary_markdown(&markdown[summary_open_end..summary_close]),
            body_markdown: markdown[summary_close + "</summary>".len()..close_start].to_owned(),
            default_open: details_has_open_attribute(open_tag),
        });
        offset = end;
    }
    output
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

fn render_markdown_lines_internal(markdown: &str, options: &MarkdownRenderOptions) -> Vec<Line> {
    let table_alignments = table_alignments(markdown);
    let alert_kinds = alert_kinds(markdown);
    let container = markdown_to_container_with_options(markdown, hyperchad_markdown_options());
    let mut renderer =
        TerminalMarkdownRenderer::new(options.width, options.theme, table_alignments, alert_kinds);
    renderer.render_container_children(
        &container,
        TextStyle {
            style: options.theme.text,
            preserve_whitespace: false,
        },
    );
    renderer.finish()
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
                    original: url,
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
    }) = containers.pop()
    {
        contributions.push(contribution(
            "image",
            source_range,
            MarkdownContributionKind::Image {
                source: resolve_markdown_destination(&metadata.source, context),
                image: metadata,
                alt: normalize_inline_whitespace(&text).trim().to_owned(),
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
        let rendering = if options.mermaid_enabled {
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
    in_table_collection: bool,
    table_alignments: Vec<Vec<Alignment>>,
    next_table_index: usize,
    alert_kinds: Rc<Vec<Option<BlockQuoteKind>>>,
    next_blockquote_index: Rc<Cell<usize>>,
    theme: MarkdownTheme,
}

impl TerminalMarkdownRenderer {
    fn new(
        width: u16,
        theme: MarkdownTheme,
        table_alignments: Vec<Vec<Alignment>>,
        alert_kinds: Vec<Option<BlockQuoteKind>>,
    ) -> Self {
        Self {
            width: usize::from(width.max(1)),
            rows: Vec::new(),
            current_spans: Vec::new(),
            current_width: 0,
            in_table_collection: false,
            table_alignments,
            next_table_index: 0,
            alert_kinds: Rc::new(alert_kinds),
            next_blockquote_index: Rc::new(Cell::new(0)),
            theme,
        }
    }

    fn nested(&self, width: usize) -> Self {
        Self {
            width: width.max(1),
            rows: Vec::new(),
            current_spans: Vec::new(),
            current_width: 0,
            in_table_collection: false,
            table_alignments: Vec::new(),
            next_table_index: 0,
            alert_kinds: Rc::clone(&self.alert_kinds),
            next_blockquote_index: Rc::clone(&self.next_blockquote_index),
            theme: self.theme,
        }
    }

    fn finish(mut self) -> Vec<Line> {
        self.flush_line();
        trim_blank_edges(&mut self.rows);
        self.rows
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
                self.push_text(&format!("[image: {image}]"), style);
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
                    .or_else(|| source.as_deref().filter(|value| !value.is_empty()))
                    .unwrap_or("image");
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
        let blockquote_index = self.next_blockquote_index.get();
        let alert_kind = self.alert_kinds.get(blockquote_index).copied().flatten();
        self.next_blockquote_index
            .set(blockquote_index.saturating_add(1));
        let mut nested = self.nested(self.width.saturating_sub(2));
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
        let header = language.map_or_else(|| "┌─".to_owned(), |language| format!("┌─ {language}"));
        self.rows
            .push(Line::from_spans(vec![Span::styled(header, border_style)]));

        let nested_width = self.width.saturating_sub(2);
        let mut nested = self.nested(nested_width);
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
            apply_code_block_syntax_highlighting(language, &mut code_rows, self.theme);
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
        let mut nested = self.nested(nested_width);
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
            self.rows.push(table_content_line(
                &row.cells,
                &widths,
                &alignments,
                border_style,
                row.header.then_some(self.theme.strong),
            ));
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
                    .map(|spans| {
                        spans
                            .iter()
                            .map(|span| span.content.as_str())
                            .collect::<String>()
                    })
                    .filter(|label| !label.is_empty())
                    .unwrap_or_else(|| column_index.saturating_add(1).to_string());
                let mut spans = vec![Span::styled(format!("{label}: "), muted)];
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
        .map(|cell| inline_spans_for_container(cell, style, theme))
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

fn inline_spans_for_container(
    container: &Container,
    style: TextStyle,
    theme: MarkdownTheme,
) -> Vec<Span> {
    let mut renderer = TerminalMarkdownRenderer::new(u16::MAX, theme, Vec::new(), Vec::new());
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

fn table_content_line(
    row: &[Vec<Span>],
    widths: &[usize],
    alignments: &[Alignment],
    border_style: Style,
    cell_style: Option<Style>,
) -> Line {
    let mut spans = vec![Span::styled("│", border_style)];
    for (index, width) in widths.iter().enumerate() {
        spans.push(Span::raw(" "));
        if let Some(cell) = row.get(index) {
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
) {
    let highlighter = SyntaxHighlighter::new();
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
    use super::{
        MarkdownRenderOptions, MarkdownTheme, markdown_to_plain_text, render_markdown_lines,
    };
    use bmux_tui::prelude::{Color, Modifier, Style};

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
