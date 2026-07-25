use bcode_markdown_render::{
    GitHubRepository, MarkdownContributionKind, MarkdownDestination, MarkdownDestinationRejection,
    MarkdownDocumentContext, MarkdownRenderOptions, MarkdownSemanticEventKind, MarkdownSemanticTag,
    MarkdownTableAlignment, MermaidContributionRendering, parse_markdown_document, render_markdown,
    render_markdown_lines, resolve_markdown_destination,
};
use bmux_tui::prelude::{Color, Line, Modifier, Style};
use unicode_width::UnicodeWidthStr;

const ALERTS: &str = include_str!("fixtures/alerts.md");
const ALERTS_STREAMING: &str = include_str!("fixtures/alerts_streaming.md");
const BLOCKQUOTES: &str = include_str!("fixtures/blockquotes.md");
const BLOCKS: &str = include_str!("fixtures/blocks.md");
const CODE_BLOCKS: &str = include_str!("fixtures/code_blocks.md");
const COMPOSITION: &str = include_str!("fixtures/composition.md");
const DETAILS: &str = include_str!("fixtures/details.md");
const DETAILS_NESTED: &str = include_str!("fixtures/details_nested.md");
const FOOTNOTES_BLOCKS: &str = include_str!("fixtures/footnotes_blocks.md");
const FOOTNOTES: &str = include_str!("fixtures/footnotes.md");
const GITHUB_MARKDOWN_EXAMPLE: &str = include_str!("../../../github-markdown-example.md");
const HEADINGS: &str = include_str!("fixtures/headings.md");
const HTML_XSS: &str = include_str!("fixtures/html_xss.md");
const IMAGES: &str = include_str!("fixtures/images.md");
const INLINE: &str = include_str!("fixtures/inline.md");
const INLINE_NESTED: &str = include_str!("fixtures/inline_nested.md");
const LINKS_IMAGES: &str = include_str!("fixtures/links_images.md");
const LISTS: &str = include_str!("fixtures/lists.md");
const LISTS_NUMBERING: &str = include_str!("fixtures/lists_numbering.md");
const LISTS_TIGHT_LOOSE: &str = include_str!("fixtures/lists_tight_loose.md");
const MATH: &str = include_str!("fixtures/math.md");
const PARAGRAPHS: &str = include_str!("fixtures/paragraphs.md");
const SEMANTIC_EXTENSIONS: &str = include_str!("fixtures/semantic_extensions.md");
const SEMANTIC_MALFORMED: &str = include_str!("fixtures/semantic_malformed.md");
const STREAMING_CODE: &str = include_str!("fixtures/streaming_code.md");
const STREAMING_INLINE: &str = include_str!("fixtures/streaming_inline.md");
const STREAMING_STRUCTURES: &str = include_str!("fixtures/streaming_structures.md");
const TABLE_ALIGNMENT: &str = include_str!("fixtures/table_alignment.md");
const TABLE_FORMATTED: &str = include_str!("fixtures/table_formatted.md");
const TABLE_MULTIPLE_ROWS: &str = include_str!("fixtures/table_multiple_rows.md");
const TABLE_SINGLE_ROW: &str = include_str!("fixtures/table_single_row.md");
const TABLE_UNEVEN: &str = include_str!("fixtures/table_uneven.md");
const TABLE_UNICODE: &str = include_str!("fixtures/table_unicode.md");
const UNICODE: &str = include_str!("fixtures/unicode.md");

fn render(markdown: &str, width: u16) -> Vec<Line> {
    render_markdown_lines(markdown, MarkdownRenderOptions::new(width))
}

fn visible_snapshot(markdown: &str, width: u16) -> String {
    visible_lines_snapshot(&render(markdown, width))
}

fn visible_lines_snapshot(lines: &[Line]) -> String {
    rendered_lines_text(lines)
        .lines()
        .enumerate()
        .map(|(index, line)| format!("{index:02} │ {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn rendered_lines_text(lines: &[Line]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn styled_snapshot(markdown: &str, width: u16) -> String {
    render(markdown, width)
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let spans = compact_spans(line)
                .into_iter()
                .map(|(content, style)| {
                    if style == Style::new() {
                        content
                    } else {
                        format!("[{}]{content}[/]", style_label(style))
                    }
                })
                .collect::<String>();
            format!("{index:02} │ {spans}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn compact_spans(line: &Line) -> Vec<(String, Style)> {
    let mut output: Vec<(String, Style)> = Vec::new();
    for span in &line.spans {
        if let Some((content, _)) = output.last_mut().filter(|(_, style)| *style == span.style) {
            content.push_str(&span.content);
        } else {
            output.push((span.content.clone(), span.style));
        }
    }
    output
}

fn style_label(style: Style) -> String {
    let mut attributes = Vec::new();
    if let Some(fg) = style.fg {
        attributes.push(format!("fg={}", color_label(fg)));
    }
    if let Some(bg) = style.bg {
        attributes.push(format!("bg={}", color_label(bg)));
    }
    for (modifier, label) in [
        (Modifier::BOLD, "bold"),
        (Modifier::DIM, "dim"),
        (Modifier::ITALIC, "italic"),
        (Modifier::UNDERLINE, "underline"),
        (Modifier::SLOW_BLINK, "slow-blink"),
        (Modifier::REVERSED, "reversed"),
        (Modifier::HIDDEN, "hidden"),
        (Modifier::CROSSED_OUT, "crossed-out"),
    ] {
        if style.modifiers.contains(modifier) {
            attributes.push(label.to_owned());
        }
    }
    attributes.join(",")
}

fn color_label(color: Color) -> String {
    match color {
        Color::Default => "default".to_owned(),
        Color::Black => "black".to_owned(),
        Color::Red => "red".to_owned(),
        Color::Green => "green".to_owned(),
        Color::Yellow => "yellow".to_owned(),
        Color::Blue => "blue".to_owned(),
        Color::Magenta => "magenta".to_owned(),
        Color::Cyan => "cyan".to_owned(),
        Color::White => "white".to_owned(),
        Color::BrightBlack => "bright-black".to_owned(),
        Color::BrightRed => "bright-red".to_owned(),
        Color::BrightGreen => "bright-green".to_owned(),
        Color::BrightYellow => "bright-yellow".to_owned(),
        Color::BrightBlue => "bright-blue".to_owned(),
        Color::BrightMagenta => "bright-magenta".to_owned(),
        Color::BrightCyan => "bright-cyan".to_owned(),
        Color::BrightWhite => "bright-white".to_owned(),
        Color::Indexed(index) => format!("indexed-{index}"),
        Color::Rgb(red, green, blue) => format!("rgb-{red}-{green}-{blue}"),
    }
}

fn semantic_snapshot(markdown: &str) -> String {
    parse_markdown_document(markdown)
        .events
        .into_iter()
        .map(|event| {
            format!(
                "{:04}..{:04} │ {:?}",
                event.source_range.start, event.source_range.end, event.kind
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn contribution_snapshot(markdown: &str) -> String {
    render_markdown(markdown, &MarkdownRenderOptions::new(80))
        .contributions
        .into_iter()
        .map(|contribution| {
            format!(
                "{} │ {:04}..{:04} │ {:?}",
                contribution.id,
                contribution.source_range.start,
                contribution.source_range.end,
                contribution.kind
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MarkdownCapabilities {
    color: bool,
    unicode: bool,
    interaction: bool,
    image_protocol: bool,
    network: bool,
    mermaid: bool,
}

fn capability_projection(markdown: &str, capabilities: MarkdownCapabilities) -> (String, usize) {
    let mut options = MarkdownRenderOptions::new(50);
    if !capabilities.color {
        options.theme = bcode_markdown_render::MarkdownTheme {
            text: Style::new(),
            heading: Style::new(),
            link: Style::new(),
            strong: Style::new(),
            emphasis: Style::new(),
            strikethrough: Style::new(),
            inline_code: Style::new(),
            code_block_text: Style::new(),
            code_block_border: Style::new(),
            blockquote_bar: Style::new(),
            alert_note: Style::new(),
            alert_tip: Style::new(),
            alert_important: Style::new(),
            alert_warning: Style::new(),
            alert_caution: Style::new(),
            list_marker: Style::new(),
            task_checked: Style::new(),
            task_unchecked: Style::new(),
            table_border: Style::new(),
            horizontal_rule: Style::new(),
        };
    }
    if capabilities.mermaid {
        options = options.with_mermaid(320, 200);
    }
    if capabilities.network {
        options = options.with_document_context(MarkdownDocumentContext {
            base_url: Some(url::Url::parse("https://example.com/base/").unwrap()),
            ..MarkdownDocumentContext::default()
        });
    }
    let result = render_markdown(markdown, &options);
    let actionable = if capabilities.interaction {
        result
            .contributions
            .iter()
            .filter(|item| match &item.kind {
                MarkdownContributionKind::Link { destination, .. }
                | MarkdownContributionKind::GitHubIssue { destination, .. } => matches!(
                    destination,
                    MarkdownDestination::Web(_) | MarkdownDestination::LocalPath(_)
                ),
                MarkdownContributionKind::Image { source, .. } => {
                    capabilities.image_protocol
                        && matches!(
                            source,
                            MarkdownDestination::Web(_) | MarkdownDestination::LocalPath(_)
                        )
                }
                MarkdownContributionKind::Details { .. }
                | MarkdownContributionKind::FootnoteReference { .. }
                | MarkdownContributionKind::FootnoteDefinition { .. } => true,
                MarkdownContributionKind::InlineMath { .. }
                | MarkdownContributionKind::DisplayMath { .. }
                | MarkdownContributionKind::Mermaid { .. } => false,
            })
            .count()
    } else {
        0
    };
    let mut visible = rendered_lines_text(&result.lines);
    if !capabilities.unicode {
        visible = visible
            .replace('ⓘ', "NOTE")
            .replace('◆', "TIP")
            .replace('❗', "IMPORTANT")
            .replace('⚠', "WARNING")
            .replace('⛔', "CAUTION");
    }
    (visible, actionable)
}

#[test]
fn capability_matrix_preserves_content_and_gates_interaction() {
    let markdown = concat!(
        "> [!NOTE]\n> Unicode 東京 and **important** content.\n\n",
        "[Guide](guide.md) ![Diagram](diagram.png)\n\n",
        "<details><summary>More</summary>Details body.</details>\n\n",
        "```mermaid\nflowchart LR\n  A --> B\n```\n",
    );
    let matrix = [
        MarkdownCapabilities {
            color: true,
            unicode: true,
            interaction: true,
            image_protocol: true,
            network: true,
            mermaid: true,
        },
        MarkdownCapabilities {
            color: false,
            unicode: false,
            interaction: true,
            image_protocol: false,
            network: false,
            mermaid: false,
        },
        MarkdownCapabilities {
            color: false,
            unicode: true,
            interaction: false,
            image_protocol: true,
            network: true,
            mermaid: false,
        },
    ];
    for capabilities in matrix {
        let (visible, actionable) = capability_projection(markdown, capabilities);
        for expected in [
            "NOTE",
            "Unicode",
            "important content",
            "Guide",
            "Diagram",
            "More",
            "Details body",
            "flowchart LR",
            "A --> B",
        ] {
            assert!(visible.contains(expected), "{capabilities:?}: {visible}");
        }
        if capabilities.interaction && capabilities.network {
            assert!(actionable >= 3);
        } else if !capabilities.interaction {
            assert_eq!(actionable, 0);
        }
    }
}

#[test]
fn rejected_destination_diagnostics_do_not_retain_sensitive_values() {
    let secret = "custom-secret://user:password@example.invalid/private?token=abc";
    let destination = resolve_markdown_destination(secret, None);
    let debug = format!("{destination:?}");

    assert_eq!(
        destination,
        MarkdownDestination::Inert {
            reason: MarkdownDestinationRejection::UnsupportedScheme,
        }
    );
    assert!(!debug.contains("password"));
    assert!(!debug.contains("token"));
    assert!(!debug.contains("private"));
}

#[test]
fn image_sources_use_the_same_safe_context_classification_as_links() {
    let options = MarkdownRenderOptions::new(80).with_document_context(MarkdownDocumentContext {
        base_url: None,
        base_directory: Some(std::path::PathBuf::from("/trusted/repository")),
        github_repository: None,
    });
    let result = render_markdown(LINKS_IMAGES, &options);
    assert!(result.contributions.iter().any(|item| matches!(
        &item.kind,
        MarkdownContributionKind::Image { alt, source, .. }
            if alt == "Diagram"
                && source == &MarkdownDestination::LocalPath(
                    std::path::PathBuf::from("/trusted/repository/diagram.png")
                )
    )));
    assert!(result.contributions.iter().any(|item| matches!(
        &item.kind,
        MarkdownContributionKind::Image { alt, source, .. }
            if alt == "Danger"
                && matches!(source, MarkdownDestination::Inert {
                    reason: MarkdownDestinationRejection::UnsupportedScheme,
                    ..
                })
    )));
    assert!(result.contributions.iter().any(|item| matches!(
        &item.kind,
        MarkdownContributionKind::Image { alt, source, .. }
            if alt == "Remote" && matches!(source, MarkdownDestination::Web(_))
    )));
}

#[test]
fn mermaid_contribution_renders_svg_or_keeps_source_with_diagnostic() {
    let enabled = MarkdownRenderOptions::new(80).with_mermaid(800, 600);
    let rendered = render_markdown("```mermaid\nflowchart LR\nA --> B\n```", &enabled);
    assert!(rendered.contributions.iter().any(|item| matches!(
        &item.kind,
        MarkdownContributionKind::Mermaid {
            source,
            rendering: MermaidContributionRendering::Svg(svg),
            ..
        } if source.contains("A --> B") && String::from_utf8_lossy(svg).contains("<svg")
    )));

    let rejected = render_markdown(
        "```mermaid\n%%{init: {}}%%\nflowchart LR\nA --> B\n```",
        &enabled,
    );
    assert!(rejected.contributions.iter().any(|item| matches!(
        &item.kind,
        MarkdownContributionKind::Mermaid {
            source,
            rendering: MermaidContributionRendering::Failed(message),
            ..
        } if source.contains("%%{init") && message.contains("directives are not allowed")
    )));
    let visible = rejected
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.content.as_str())
        .collect::<String>();
    assert!(visible.contains("%%{init"));
    assert!(visible.contains("flowchart LR"));
}

#[test]
fn snapshots_details_noninteractive_fallback_at_normal_and_narrow_widths() {
    insta::assert_snapshot!(
        "details_fallback_widths_60_24",
        [60_u16, 24]
            .into_iter()
            .map(|width| format!("== width {width} ==\n{}", styled_snapshot(DETAILS, width)))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn details_contributions_preserve_safe_summary_body_and_default_state() {
    let result = render_markdown(DETAILS, &MarkdownRenderOptions::new(60));
    let details = result
        .contributions
        .iter()
        .filter_map(|item| match &item.kind {
            MarkdownContributionKind::Details {
                summary,
                body,
                default_open,
            } => Some((summary.as_str(), body.as_str(), *default_open)),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(details.len(), 2);
    assert_eq!(details[0].0, "**Retry** `algorithm`");
    assert!(details[0].1.contains("Body with *Markdown*"));
    assert!(!details[0].2);
    assert_eq!(details[1].0, "Open by default");
    assert!(details[1].2);

    let visible = visible_snapshot(DETAILS, 60);
    assert!(visible.contains("Retry algorithm"));
    assert!(visible.contains("Body with Markdown and a list:"));
    assert!(visible.contains("<details data-unsupported=\"x\">"));
    assert!(visible.contains("<details><summary>Incomplete summary</summary>"));
    assert!(!visible.contains("<strong>"));
}

#[test]
fn nested_details_preserve_contributions_and_readable_recursive_fallback() {
    let result = render_markdown(DETAILS_NESTED, &MarkdownRenderOptions::new(60));
    let details = result
        .contributions
        .iter()
        .filter_map(|item| match &item.kind {
            MarkdownContributionKind::Details {
                summary,
                body,
                default_open,
            } => Some((summary.as_str(), body.as_str(), *default_open)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(details.len(), 2);
    assert_eq!(details[0].0, "Outer **summary**");
    assert!(details[0].1.contains("Inner `summary`"));
    assert!(!details[0].2);
    assert_eq!(details[1].0, "Inner `summary`");
    assert!(details[1].1.contains("Inner body with *Markdown*"));
    assert!(details[1].2);

    let visible = visible_snapshot(DETAILS_NESTED, 60);
    for content in [
        "Before.",
        "Outer summary",
        "Outer body.",
        "Inner summary",
        "Inner body with Markdown.",
        "After inner.",
        "After.",
    ] {
        assert!(visible.contains(content), "missing {content:?}: {visible}");
    }
    assert!(!visible.contains("<details"));
    assert!(!visible.contains("</details>"));
}

#[test]
fn streamed_incomplete_details_remain_visible_until_complete() {
    let partial = "Before.\n\n<details><summary>Retry</summary>Body with **important** content";
    let rendered = render_markdown(
        partial,
        &MarkdownRenderOptions::new(50).with_streaming(true),
    );
    let partial_visible = rendered_lines_text(&rendered.lines);
    assert!(!partial_visible.contains("<details>"));
    assert!(partial_visible.contains("Retry"));
    assert!(partial_visible.contains("Body with important content"));
    assert!(
        !rendered
            .contributions
            .iter()
            .any(|item| matches!(item.kind, MarkdownContributionKind::Details { .. }))
    );

    let complete = format!("{partial}</details>");
    let rendered = render_markdown(&complete, &MarkdownRenderOptions::new(50));
    let complete_visible = rendered_lines_text(&rendered.lines);
    assert!(complete_visible.contains("Retry"));
    assert!(complete_visible.contains("Body with important content"));
    assert!(!complete_visible.contains("<details>"));
    assert_eq!(
        rendered
            .contributions
            .iter()
            .filter(|item| matches!(item.kind, MarkdownContributionKind::Details { .. }))
            .count(),
        1
    );
}

#[test]
fn streamed_semantic_ids_shift_only_at_or_after_changed_source() {
    let before = render_markdown(
        "[first](a.md) and [second](b.md)",
        &MarkdownRenderOptions::new(80).with_document_id("stream:1"),
    );
    let after = render_markdown(
        "prefix [first](a.md) and [second](b.md)",
        &MarkdownRenderOptions::new(80).with_document_id("stream:1"),
    );
    assert_eq!(before.contributions.len(), after.contributions.len());
    assert_ne!(before.contributions[0].id, after.contributions[0].id);
    assert!(
        before
            .contributions
            .iter()
            .zip(&after.contributions)
            .all(|(before, after)| before.kind == after.kind && before.id != after.id)
    );

    let append_before = render_markdown(
        "[first](a.md)",
        &MarkdownRenderOptions::new(80).with_document_id("stream:1"),
    );
    let append_after = render_markdown(
        "[first](a.md) trailing stream text",
        &MarkdownRenderOptions::new(80).with_document_id("stream:1"),
    );
    assert_eq!(
        append_before.contributions[0].id,
        append_after.contributions[0].id
    );
}

#[test]
fn duplicate_semantic_nodes_have_unique_position_qualified_ids() {
    let rendered = render_markdown(
        "[same](target.md) [same](target.md)",
        &MarkdownRenderOptions::new(80).with_document_id("item:duplicates"),
    );
    assert_eq!(rendered.contributions.len(), 2);
    assert_ne!(rendered.contributions[0].id, rendered.contributions[1].id);
}

#[test]
fn markdown_layout_signature_tracks_every_renderer_owned_layout_input() {
    let base = MarkdownRenderOptions::new(80).with_document_id("item:1");
    let base_result = render_markdown("[link](relative.md)", &base);
    let variants = [
        MarkdownRenderOptions::new(40).with_document_id("item:1"),
        MarkdownRenderOptions::new(80).with_document_id("item:2"),
        MarkdownRenderOptions::new(80)
            .with_document_id("item:1")
            .with_streaming(true),
        MarkdownRenderOptions::new(80)
            .with_document_id("item:1")
            .with_mermaid(800, 600),
        MarkdownRenderOptions::new(80)
            .with_document_id("item:1")
            .with_document_context(MarkdownDocumentContext {
                base_url: Some(url::Url::parse("https://example.com/base/").unwrap()),
                ..MarkdownDocumentContext::default()
            }),
        MarkdownRenderOptions::new(80).with_theme(bcode_markdown_render::MarkdownTheme {
            heading: Style::new().fg(Color::Red),
            ..bcode_markdown_render::MarkdownTheme::default()
        }),
    ];
    for variant in variants {
        assert_ne!(
            base_result.layout_signature,
            render_markdown("[link](relative.md)", &variant).layout_signature
        );
    }
    assert_ne!(
        base_result.layout_signature,
        render_markdown("[different](relative.md)", &base).layout_signature
    );
}

#[test]
fn github_issue_references_require_explicit_repository_context() {
    let markdown = "Closes #42, see #7, but not #0 or #abc.";
    let without_context = render_markdown(markdown, &MarkdownRenderOptions::new(80));
    assert!(
        !without_context
            .contributions
            .iter()
            .any(|item| matches!(item.kind, MarkdownContributionKind::GitHubIssue { .. }))
    );

    let options = MarkdownRenderOptions::new(80).with_document_context(MarkdownDocumentContext {
        github_repository: Some(GitHubRepository {
            owner: "acme".to_owned(),
            name: "nebula".to_owned(),
        }),
        ..MarkdownDocumentContext::default()
    });
    let with_context = render_markdown(markdown, &options);
    let issues = with_context
        .contributions
        .iter()
        .filter_map(|item| match &item.kind {
            MarkdownContributionKind::GitHubIssue { number, closes, .. } => {
                Some((*number, *closes))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(issues, [(42, true), (7, false)]);
}

#[test]
fn contribution_ids_include_explicit_owner_and_remain_width_stable() {
    let wide_options = MarkdownRenderOptions::new(80).with_document_id("transcript:42");
    let narrow_options = MarkdownRenderOptions::new(20).with_document_id("transcript:42");
    let other_options = MarkdownRenderOptions::new(80).with_document_id("transcript:43");
    let wide = render_markdown(SEMANTIC_EXTENSIONS, &wide_options);
    let narrow = render_markdown(SEMANTIC_EXTENSIONS, &narrow_options);
    let other = render_markdown(SEMANTIC_EXTENSIONS, &other_options);
    let ids = |result: &bcode_markdown_render::MarkdownRenderResult| {
        result
            .contributions
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>()
    };

    assert_eq!(ids(&wide), ids(&narrow));
    assert!(ids(&wide).iter().all(|id| id.starts_with("transcript:42:")));
    assert_ne!(ids(&wide), ids(&other));
}

#[test]
fn link_destinations_require_explicit_safe_context() {
    let context = MarkdownDocumentContext {
        base_url: Some(url::Url::parse("https://github.com/acme/nebula/blob/main/").unwrap()),
        base_directory: Some(std::path::PathBuf::from("/trusted/repository")),
        github_repository: Some(GitHubRepository {
            owner: "acme".to_owned(),
            name: "nebula".to_owned(),
        }),
    };

    assert_eq!(
        resolve_markdown_destination("https://example.com/docs", None),
        MarkdownDestination::Web(url::Url::parse("https://example.com/docs").unwrap())
    );
    assert_eq!(
        resolve_markdown_destination("./docs/protocol.md", Some(&context)),
        MarkdownDestination::Web(
            url::Url::parse("https://github.com/acme/nebula/blob/main/docs/protocol.md").unwrap()
        )
    );
    assert_eq!(
        resolve_markdown_destination("./docs/protocol.md", None),
        MarkdownDestination::UnresolvedRelative("./docs/protocol.md".to_owned())
    );
    assert_eq!(
        resolve_markdown_destination("javascript:alert(1)", Some(&context)),
        MarkdownDestination::Inert {
            reason: MarkdownDestinationRejection::UnsupportedScheme,
        }
    );
    assert_eq!(
        resolve_markdown_destination("#section", Some(&context)),
        MarkdownDestination::Fragment("section".to_owned())
    );
}

#[test]
fn rendered_link_contributions_include_classified_destinations() {
    let context = MarkdownDocumentContext {
        base_url: None,
        base_directory: Some(std::path::PathBuf::from("/trusted/repository")),
        github_repository: None,
    };
    let options = MarkdownRenderOptions::new(80).with_document_context(context);
    let result = render_markdown("[Protocol](./docs/protocol.md)", &options);
    assert!(result.contributions.iter().any(|item| matches!(
        &item.kind,
        MarkdownContributionKind::Link { label, destination, .. }
            if label == "Protocol"
                && destination == &MarkdownDestination::LocalPath(
                    std::path::PathBuf::from("/trusted/repository/docs/protocol.md")
                )
    )));
}

#[test]
fn snapshots_math_at_normal_and_constrained_widths() {
    insta::assert_snapshot!(
        "math_widths_60_24",
        [60_u16, 24]
            .into_iter()
            .map(|width| format!("== width {width} ==\n{}", styled_snapshot(MATH, width)))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn math_uses_unicode_subset_and_preserves_unsupported_source() {
    let output = visible_snapshot(MATH, 60);

    assert!(output.contains("x² + α × y₁ ≤ ∞"));
    assert!(output.contains("n²"));
    assert!(output.contains("∑_{i=1}ⁿ i²"));
    assert!(output.contains(r"\frac{a}{b} + q^{word}"));
    assert!(output.contains("$unterminated math remains source text"));
}

#[test]
fn footnote_contributions_have_bidirectional_navigation_targets() {
    let options = MarkdownRenderOptions::new(80).with_document_id("message:7");
    let rendered = render_markdown(FOOTNOTES, &options);
    let references = rendered
        .contributions
        .iter()
        .filter_map(|item| match &item.kind {
            MarkdownContributionKind::FootnoteReference { label, target_id }
                if label == "alpha" =>
            {
                Some((item.id.clone(), target_id.clone()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let definition = rendered
        .contributions
        .iter()
        .find_map(|item| match &item.kind {
            MarkdownContributionKind::FootnoteDefinition {
                label,
                reference_ids,
            } if label == "alpha" => Some((item.id.clone(), reference_ids.clone())),
            _ => None,
        });

    assert_eq!(references.len(), 2);
    let (definition_id, reference_ids) = definition.expect("alpha definition");
    assert!(
        references
            .iter()
            .all(|(_, target_id)| target_id == &definition_id)
    );
    assert_eq!(
        reference_ids,
        references
            .iter()
            .map(|(reference_id, _)| reference_id.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn snapshots_footnote_block_content() {
    insta::assert_snapshot!(
        "footnotes_blocks_width_50",
        styled_snapshot(FOOTNOTES_BLOCKS, 50)
    );
}

#[test]
fn footnote_block_content_preserves_lists_quotes_and_code() {
    let output = visible_snapshot(FOOTNOTES_BLOCKS, 50);

    assert!(output.contains("Opening paragraph with strong text."));
    assert!(output.contains("•  first item"));
    assert!(output.contains("•  second item with code"));
    assert!(output.contains("│ quoted block"));
    assert!(output.contains("┌─ rust"));
    assert!(output.contains("fn main() {}"));
}

#[test]
fn snapshots_footnotes_at_normal_and_narrow_widths() {
    insta::assert_snapshot!(
        "footnotes_widths_60_24",
        [60_u16, 24]
            .into_iter()
            .map(|width| format!("== width {width} ==\n{}", styled_snapshot(FOOTNOTES, width)))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn footnotes_use_first_reference_numbers_and_preserve_all_content() {
    let output = visible_snapshot(FOOTNOTES, 60);

    assert!(output.contains("First[1], repeated[1], and second[2]. Missing[3]."));
    assert!(output.contains("1.  First definition over multiple lines. ↩×2"));
    assert!(output.contains("2.  Second definition with strong text, emphasis, code, and"));
    assert!(output.contains("Unicode 東京 🧪. ↩"));
    assert!(output.contains("3.  Unreferenced definition."));
    assert!(!output.contains("[^alpha]"));
}

#[test]
fn snapshots_parser_neutral_semantic_contributions() {
    insta::assert_snapshot!(
        "semantic_contributions",
        contribution_snapshot(SEMANTIC_EXTENSIONS)
    );
}

#[test]
fn semantic_contributions_have_stable_unique_ids_and_expected_payloads() {
    let first = render_markdown(SEMANTIC_EXTENSIONS, &MarkdownRenderOptions::new(80));
    let second = render_markdown(SEMANTIC_EXTENSIONS, &MarkdownRenderOptions::new(24));
    let first_ids = first
        .contributions
        .iter()
        .map(|item| item.id.as_str())
        .collect::<Vec<_>>();
    let second_ids = second
        .contributions
        .iter()
        .map(|item| item.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(first_ids, second_ids);
    let unique = first_ids
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(unique.len(), first_ids.len());
    assert!(first.contributions.iter().any(|item| matches!(
        &item.kind,
        MarkdownContributionKind::Link { label, .. } if label == "link"
    )));
    assert!(first.contributions.iter().any(|item| matches!(
        &item.kind,
        MarkdownContributionKind::Image { alt, .. } if alt == "diagram"
    )));
    assert!(first.contributions.iter().any(|item| matches!(
        &item.kind,
        MarkdownContributionKind::Mermaid { source, .. } if source.contains("A --> B")
    )));
}

#[test]
fn malformed_extensions_remain_visible_and_balanced() {
    let document = parse_markdown_document(SEMANTIC_MALFORMED);
    let semantic = semantic_snapshot(SEMANTIC_MALFORMED);
    let visible = visible_snapshot(SEMANTIC_MALFORMED, 60);

    assert!(!document.events.is_empty());
    assert!(semantic.contains("incomplete alert"));
    assert!(semantic.contains("CodeBlock(Some(\"mermaid\"))"));
    assert!(visible.contains("[!NOTE incomplete alert"));
    assert!(visible.contains("Unclosed [link]( destination"));
    assert!(visible.contains("Inline $unterminated"));
    assert!(visible.contains("Reference [1]"));
    assert!(visible.contains("<details open data-unsupported"));
    assert!(visible.contains("┌─ mermaid"));
}

#[test]
fn snapshots_parser_neutral_semantic_extensions() {
    insta::assert_snapshot!(
        "semantic_extensions",
        semantic_snapshot(SEMANTIC_EXTENSIONS)
    );
}

#[test]
fn semantic_extensions_preserve_required_metadata_and_source_order() {
    let document = parse_markdown_document(SEMANTIC_EXTENSIONS);
    for event in &document.events {
        assert!(event.source_range.start <= event.source_range.end);
        assert!(event.source_range.end <= SEMANTIC_EXTENSIONS.len());
    }

    assert!(document.events.iter().any(|event| matches!(
        &event.kind,
        MarkdownSemanticEventKind::Start(MarkdownSemanticTag::Link(link))
            if link.destination == "./docs/protocol.md" && link.title.as_deref() == Some("Protocol")
    )));
    assert!(document.events.iter().any(|event| matches!(
        &event.kind,
        MarkdownSemanticEventKind::Start(MarkdownSemanticTag::Image(image))
            if image.source == "diagram.png" && image.title.as_deref() == Some("Diagram")
    )));
    assert!(document.events.iter().any(|event| matches!(
        &event.kind,
        MarkdownSemanticEventKind::InlineMath(math) if math == "x^2"
    )));
    assert!(document.events.iter().any(|event| matches!(
        &event.kind,
        MarkdownSemanticEventKind::DisplayMath(math) if math == "x = y + 1"
    )));
    assert!(document.events.iter().any(|event| matches!(
        &event.kind,
        MarkdownSemanticEventKind::FootnoteReference(label) if label == "note"
    )));
    assert!(document.events.iter().any(|event| matches!(
        &event.kind,
        MarkdownSemanticEventKind::Start(MarkdownSemanticTag::FootnoteDefinition(label))
            if label == "note"
    )));
    assert!(document.events.iter().any(|event| matches!(
        &event.kind,
        MarkdownSemanticEventKind::Start(MarkdownSemanticTag::Table(alignments))
            if alignments == &[
                MarkdownTableAlignment::Left,
                MarkdownTableAlignment::Center,
                MarkdownTableAlignment::Right,
            ]
    )));
    assert!(document.events.iter().any(|event| matches!(
        &event.kind,
        MarkdownSemanticEventKind::Start(MarkdownSemanticTag::CodeBlock(Some(language)))
            if language == "mermaid"
    )));
    assert!(document.events.iter().any(|event| matches!(
        &event.kind,
        MarkdownSemanticEventKind::Html(html) if html.contains("<details>")
    )));
}

#[test]
fn snapshots_github_alerts_at_normal_and_narrow_widths() {
    insta::assert_snapshot!(
        "alerts_widths_60_24",
        [60_u16, 24]
            .into_iter()
            .map(|width| {
                format!(
                    "== complete width {width} ==\n{}\n== streaming width {width} ==\n{}",
                    styled_snapshot(ALERTS, width),
                    styled_snapshot(ALERTS_STREAMING, width)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn github_alert_labels_are_semantic_and_unknown_markers_remain_quotes() {
    let output = visible_snapshot(ALERTS, 60);

    for label in ["ⓘ NOTE", "◆ TIP", "❗ IMPORTANT", "⚠ WARNING", "⛔ CAUTION"] {
        assert!(output.contains(label), "missing alert label {label:?}");
    }
    assert_eq!(output.matches("ⓘ NOTE").count(), 1);
    assert_eq!(output.matches("◆ TIP").count(), 2);
    assert_eq!(output.matches("⚠ WARNING").count(), 2);
    assert!(output.contains("Nested warning."));
    assert!(output.contains("Alert inside a list item."));
    assert!(output.contains("東京"));
    assert!(output.contains("[!UNKNOWN]"));
    assert!(output.contains("[!NOTE] trailing text"));
    assert!(output.contains("[!NOTE Incomplete marker"));
}

#[test]
fn every_semantic_fallback_preserves_important_content() {
    let source = concat!(
        "[Guide](relative.md) and ![Architecture diagram](diagram.png).\n\n",
        "Reference.[^note]\n\n[^note]: Footnote **body**.\n\n",
        "Inline $x^2$ and display:\n\n$$y = 2$$\n\n",
        "<details><summary>Retry policy</summary>Retry **body**.</details>\n\n",
        "```mermaid\nflowchart LR\n  Client --> Server\n```\n\n",
        "| Check | State |\n|---|---|\n| integration | passing |\n",
    );
    for width in [80_u16, 20] {
        let rendered = render_markdown(source, &MarkdownRenderOptions::new(width));
        let visible = rendered_lines_text(&rendered.lines);
        let compact = visible.replace('\n', "");
        for expected in [
            "Guide",
            "Architecture diagram",
            "[1]",
            "Footnotes",
            "Footnote body",
            "x²",
            "y = 2",
            "Retry policy",
            "Retry body",
            "flowchart LR",
            "Client --> Serve",
            "integration",
            "passing",
        ] {
            assert!(
                compact.contains(expected),
                "width {width} lost fallback content {expected:?}: {visible}"
            );
        }
    }
}

#[test]
fn github_markdown_fixture_support_contract_is_fully_represented() {
    let options = MarkdownRenderOptions::new(100).with_document_context(MarkdownDocumentContext {
        base_url: Some(url::Url::parse("https://github.com/acme/nebula/blob/main/").unwrap()),
        base_directory: None,
        github_repository: Some(GitHubRepository {
            owner: "acme".to_owned(),
            name: "nebula".to_owned(),
        }),
    });
    let result = render_markdown(GITHUB_MARKDOWN_EXAMPLE, &options);

    assert!(result.contributions.iter().any(|item| matches!(
        &item.kind,
        MarkdownContributionKind::Mermaid { source, .. } if source.contains("flowchart LR")
    )));
    assert!(result.contributions.iter().any(|item| matches!(
        &item.kind,
        MarkdownContributionKind::Details { summary, body, .. }
            if summary.contains("retry algorithm") && body.contains("delay(n)")
    )));
    assert!(result.contributions.iter().any(|item| matches!(
        &item.kind,
        MarkdownContributionKind::FootnoteReference { label, .. } if label == "compat"
    )));
    assert!(result.contributions.iter().any(|item| matches!(
        &item.kind,
        MarkdownContributionKind::Image { alt, .. } if alt == "Build"
    )));
    assert!(result.contributions.iter().any(|item| matches!(
        &item.kind,
        MarkdownContributionKind::Link { label, destination, .. }
            if label == "docs/protocol.md"
                && matches!(destination, MarkdownDestination::Web(url) if url.path().ends_with("/docs/protocol.md"))
    )));
    assert!(
        !result.contributions.iter().any(|item| matches!(
            &item.kind,
            MarkdownContributionKind::GitHubIssue { number: 42, .. }
        )),
        "inline-code issue text must remain code, matching GitHub semantics"
    );
    let text = result
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.content.as_str())
        .collect::<String>();
    for expected in [
        "❗ IMPORTANT",
        "⚠ WARNING",
        "(n)",
        "Footnotes",
        "This enables older clients",
        "Gateway",
        "42k req/s",
    ] {
        assert!(
            text.contains(expected),
            "missing fixture contract text {expected:?}"
        );
    }
}

#[test]
fn snapshots_github_markdown_example_semantic_contributions() {
    insta::assert_snapshot!(
        "github_markdown_example_semantic_contributions",
        contribution_snapshot(GITHUB_MARKDOWN_EXAMPLE)
    );
}

#[test]
fn snapshots_github_markdown_example_at_representative_widths() {
    insta::assert_snapshot!(
        "github_markdown_example_widths_100_60_24",
        [100_u16, 60, 24]
            .into_iter()
            .map(|width| format!(
                "== width {width} ==\n{}",
                styled_snapshot(GITHUB_MARKDOWN_EXAMPLE, width)
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn github_markdown_example_documents_current_semantic_gaps() {
    let output = visible_snapshot(GITHUB_MARKDOWN_EXAMPLE, 100);

    assert!(
        output.contains("┌─ mermaid"),
        "Mermaid fence was not preserved"
    );
    assert!(
        output.contains("❗ IMPORTANT"),
        "IMPORTANT alert label changed"
    );
    assert!(output.contains("⚠ WARNING"), "WARNING alert label changed");
    assert!(
        output.contains("View the retry algorithm"),
        "details summary changed"
    );
    assert!(output.contains("delay(n) = min"), "details body changed");
    assert!(!output.contains("<details>"));
    assert!(!output.contains("<summary>"));
    assert!(output.contains("(n)"), "inline math degradation changed");
    assert!(output.contains("[1]"), "footnote reference changed");
    assert!(output.contains("Footnotes"), "footnote section changed");
    assert!(
        output.contains("This enables older clients"),
        "footnote definition changed"
    );
    assert!(output.contains("[image: Build]"), "badge fallback changed");
    assert!(output.contains("docs/protocol.md"), "link label changed");
    assert!(output.contains("Closes"), "issue-closing keyword changed");
    assert!(output.contains("#42"), "issue reference changed");
}

#[test]
fn snapshots_left_center_and_right_table_alignment() {
    insta::assert_snapshot!(
        "table_alignment_width_80",
        visible_snapshot(TABLE_ALIGNMENT, 80)
    );
}

#[test]
fn aligned_table_cells_have_expected_padding() {
    let output = visible_snapshot(TABLE_ALIGNMENT, 80);

    assert!(output.contains("│ x      │   y    │   zzz │"));
    assert!(output.contains("│ longer │   q    │     7 │"));
}

#[test]
fn snapshots_all_heading_levels_at_width_40() {
    insta::assert_snapshot!("headings_width_40", styled_snapshot(HEADINGS, 40));
}

#[test]
fn snapshots_paragraph_breaks_and_rules_at_width_40() {
    insta::assert_snapshot!("paragraphs_width_40", visible_snapshot(PARAGRAPHS, 40));
}

#[test]
fn snapshots_horizontal_rule_at_width_3() {
    insta::assert_snapshot!("horizontal_rule_width_3", visible_snapshot("---", 3));
}

#[test]
fn snapshots_nested_and_wrapped_blockquotes_at_width_24() {
    insta::assert_snapshot!("blockquotes_width_24", visible_snapshot(BLOCKQUOTES, 24));
}

#[test]
fn snapshots_fenced_and_indented_code_at_width_40() {
    insta::assert_snapshot!("code_blocks_width_40", visible_snapshot(CODE_BLOCKS, 40));
}

#[test]
fn snapshots_blocks_at_width_40() {
    insta::assert_snapshot!("blocks_width_40", visible_snapshot(BLOCKS, 40));
}

#[test]
fn snapshots_composition_at_width_40() {
    insta::assert_snapshot!("composition_width_40", styled_snapshot(COMPOSITION, 40));
}

#[test]
fn snapshots_nested_inline_styles_at_width_50() {
    insta::assert_snapshot!(
        "inline_nested_styles_width_50",
        styled_snapshot(INLINE_NESTED, 50)
    );
}

#[test]
fn snapshots_formatted_text_wrapping_at_width_16() {
    insta::assert_snapshot!(
        "inline_wrapped_styles_width_16",
        styled_snapshot(
            "**strong text wraps** and [linked text wraps](https://example.com)",
            16
        )
    );
}

#[test]
fn snapshots_images_and_fallbacks_at_width_40() {
    insta::assert_snapshot!("images_width_40", visible_snapshot(IMAGES, 40));
}

#[test]
fn snapshots_html_and_xss_output_at_width_80() {
    insta::assert_snapshot!("html_xss_width_80", visible_snapshot(HTML_XSS, 80));
}

#[test]
fn dangerous_html_is_escaped_and_not_executably_rendered() {
    let output = visible_snapshot(HTML_XSS, 80);
    assert!(!output.contains("<script>"));
    assert!(output.contains("&amp;lt;script&amp;gt;"));
}

#[test]
fn snapshots_inline_visible_text_at_width_80() {
    insta::assert_snapshot!("inline_visible_width_80", visible_snapshot(INLINE, 80));
}

#[test]
fn snapshots_inline_styles_at_width_80() {
    insta::assert_snapshot!("inline_styles_width_80", styled_snapshot(INLINE, 80));
}

#[test]
fn snapshots_links_images_escapes_and_entities_at_width_40() {
    insta::assert_snapshot!("links_images_width_40", styled_snapshot(LINKS_IMAGES, 40));
}

#[test]
fn snapshots_tight_and_loose_lists_at_width_34() {
    insta::assert_snapshot!(
        "lists_tight_loose_width_34",
        visible_snapshot(LISTS_TIGHT_LOOSE, 34)
    );
}

#[test]
fn snapshots_numbering_and_mixed_nested_lists_at_width_28() {
    insta::assert_snapshot!(
        "lists_numbering_width_28",
        visible_snapshot(LISTS_NUMBERING, 28)
    );
}

#[test]
fn snapshots_lists_at_width_30() {
    insta::assert_snapshot!("lists_width_30", styled_snapshot(LISTS, 30));
}

#[test]
fn snapshots_partial_streaming_inputs_at_width_40() {
    let snapshot = [
        "== code ==",
        &visible_snapshot(STREAMING_CODE, 40),
        "== inline ==",
        &visible_snapshot(STREAMING_INLINE, 40),
        "== structures ==",
        &visible_snapshot(STREAMING_STRUCTURES, 40),
    ]
    .join("\n");
    insta::assert_snapshot!("streaming_partial_width_40", snapshot);
}

#[test]
fn requested_widths_80_40_and_20_are_snapshot_covered() {
    insta::assert_snapshot!(
        "representative_widths_80_40_20",
        [
            "== width 80 ==",
            &visible_snapshot(COMPOSITION, 80),
            "== width 40 ==",
            &visible_snapshot(COMPOSITION, 40),
            "== width 20 ==",
            &visible_snapshot(COMPOSITION, 20),
        ]
        .join("\n")
    );
}

#[test]
fn snapshots_unicode_at_width_20() {
    insta::assert_snapshot!("unicode_width_20", visible_snapshot(UNICODE, 20));
}

#[test]
fn snapshots_table_with_one_body_row_at_width_80() {
    insta::assert_snapshot!(
        "table_single_row_width_80",
        styled_snapshot(TABLE_SINGLE_ROW, 80)
    );
}

#[test]
fn snapshots_table_that_exactly_fits_width_17() {
    insta::assert_snapshot!(
        "table_exact_fit_width_17",
        visible_snapshot(TABLE_MULTIPLE_ROWS, 17)
    );
}

#[test]
fn snapshots_table_with_multiple_body_rows_at_width_80() {
    insta::assert_snapshot!(
        "table_multiple_rows_width_80",
        visible_snapshot(TABLE_MULTIPLE_ROWS, 80)
    );
}

#[test]
fn snapshots_table_with_empty_and_uneven_cells_at_width_80() {
    insta::assert_snapshot!("table_uneven_width_80", visible_snapshot(TABLE_UNEVEN, 80));
}

#[test]
fn snapshots_formatted_table_styles_at_width_80() {
    insta::assert_snapshot!(
        "table_formatted_styles_width_80",
        styled_snapshot(TABLE_FORMATTED, 80)
    );
}

#[test]
fn snapshots_narrow_table_fallback_at_width_12() {
    insta::assert_snapshot!(
        "table_narrow_fallback_width_12",
        visible_snapshot(TABLE_MULTIPLE_ROWS, 12)
    );
}

#[test]
fn snapshots_unicode_table_at_width_80() {
    insta::assert_snapshot!(
        "table_unicode_width_80",
        visible_snapshot(TABLE_UNICODE, 80)
    );
}

#[test]
fn unicode_table_preserves_consistent_edges_and_display_width() {
    let lines = render(TABLE_UNICODE, 80);
    let visible = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    let widths = visible
        .iter()
        .map(|line| UnicodeWidthStr::width(line.as_str()))
        .collect::<Vec<_>>();

    assert!(!visible.is_empty());
    assert!(widths.iter().all(|width| *width == widths[0]));
    assert!(
        visible
            .iter()
            .filter(|line| line.starts_with('│'))
            .all(|line| line.ends_with('│'))
    );
    assert!(visible.iter().any(|line| line.contains("é")));
    assert!(visible.iter().any(|line| line.contains("東京")));
    assert!(visible.iter().any(|line| line.contains("🚀")));
}

#[test]
fn rendered_lines_respect_requested_width_for_wrappable_text() {
    for width in [2_u16, 20, 40, 80] {
        for line in render(UNICODE, width) {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>();
            assert!(
                UnicodeWidthStr::width(text.as_str()) <= usize::from(width.max(1)),
                "line exceeds width {width}: {text:?}"
            );
        }
    }
}

#[test]
fn defensive_widths_do_not_panic_for_supported_structures() {
    for width in [0_u16, 1] {
        for markdown in [BLOCKS, INLINE, LISTS, TABLE_MULTIPLE_ROWS, UNICODE] {
            let _ = render(markdown, width);
        }
    }
}

#[test]
fn zero_width_is_treated_as_one_without_panicking() {
    let lines = render("abc", 0);
    assert_eq!(lines.len(), 3);
}

#[test]
fn bordered_table_lines_have_consistent_display_width() {
    let lines = render(TABLE_MULTIPLE_ROWS, 80);
    let widths = lines
        .iter()
        .map(|line| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>();
            UnicodeWidthStr::width(text.as_str())
        })
        .collect::<Vec<_>>();

    assert!(!widths.is_empty());
    assert!(widths.iter().all(|width| *width == widths[0]));
}

#[test]
fn bordered_table_content_has_matching_vertical_edges() {
    for line in render(TABLE_MULTIPLE_ROWS, 80) {
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>();
        if text.starts_with('│') {
            assert!(text.ends_with('│'), "table row lacks right edge: {text:?}");
        }
    }
}
