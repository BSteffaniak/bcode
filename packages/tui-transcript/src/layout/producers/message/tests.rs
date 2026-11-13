//! Message producer behavior: speaker variants, disclosure levels, note kinds, the B8 matrix
//! (zero and tiny widths, CJK, emoji, combining marks, styled spans, resize), source anchors
//! across width changes, streaming convergence and painted/geometry agreement.

use std::sync::Arc;

use bcode_presentation_policy::models::{HeaderSpec, SummarySpec};
use bcode_session_models::ReasoningActivityStatus;
use bcode_session_view_models::{
    CompactionViewStatus, SkillView, TranscriptChanges, TranscriptViewItemId,
};
use bcode_tui_theme::definition::{
    ContainerBorder, ContainerLayout, ContainerRecipe, ContainerWidth,
};
use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;

use super::*;
use crate::layout::SourcePosition;
use crate::layout::context::RenderConfig;
use crate::layout::geometry::ItemAction;
use crate::layout::producers::ProducerRegistry;
use crate::units::structure::{FlatStructure, ItemStructure, StructureResolver};
use crate::units::{UnitKey, Units};
use crate::window::WindowMode;

fn view_item(kind: TranscriptViewItemKind, streaming: bool) -> TranscriptViewItem {
    TranscriptViewItem {
        id: TranscriptViewItemId::new("m"),
        revision: 1,
        sequence: Some(1),
        timestamp_ms: None,
        output_location: None,
        streaming,
        kind,
    }
}

fn user(text: &str) -> TranscriptViewItem {
    view_item(
        TranscriptViewItemKind::UserMessage {
            message: ChatMessageView::markdown(text),
        },
        false,
    )
}

fn assistant(text: &str, streaming: bool) -> TranscriptViewItem {
    view_item(
        TranscriptViewItemKind::AssistantMessage {
            message: ChatMessageView::markdown(text),
        },
        streaming,
    )
}

fn key() -> UnitKey {
    UnitKey::Item(TranscriptViewItemId::new("m"))
}

fn default_structure() -> ResolvedUnitPresentation {
    StructureResolver::default()
        .unit_presentation(&key())
        .clone()
}

fn with_presentation(edit: impl FnOnce(&mut ResolvedPresentation)) -> ResolvedUnitPresentation {
    let mut structure = default_structure();
    if let ResolvedUnitPresentation::Item(ItemStructure { presentation, .. }) = &mut structure {
        edit(presentation);
    }
    structure
}

fn config_with(edit: impl FnOnce(&mut ResolvedThemeDefinition)) -> RenderConfig {
    let mut config = RenderConfig::default();
    let mut theme = (*config.theme).clone();
    edit(&mut theme);
    config.theme = Arc::new(theme);
    config
}

fn speaker(style: SpeakerStyle) -> RenderConfig {
    config_with(|theme| theme.chrome.speaker = style)
}

/// One produced, laid-out and painted unit.
struct Rendered {
    node: LayoutNode,
    rows: Vec<String>,
    geometry: ItemGeometry,
    work: ProducerWork,
}

fn render_with(
    item: &TranscriptViewItem,
    width: u16,
    config: &RenderConfig,
    structure: &ResolvedUnitPresentation,
) -> Rendered {
    let items = vec![item.clone()];
    let mut units = Units::new();
    let _ = units.apply(
        &TranscriptChanges::default(),
        &items,
        WindowMode::Live,
        &FlatStructure,
    );
    let entry = units.entry(&key()).expect("item unit");
    let cx = config.context(&key(), u64::from(width), structure);
    let produced = ProducerRegistry::standard().produce(
        UnitView {
            unit: entry,
            item: Some(&items[0]),
        },
        &cx,
    );
    let node = produced.content.layout(
        Constraints::for_width(u64::from(width)),
        &mut LayoutCx::new(),
    );
    let height = u16::try_from(node.size.height).expect("height fits");
    let rows = if width == 0 || height == 0 {
        Vec::new()
    } else {
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        let mut frame = Frame::new(&mut buffer);
        produced.content.paint(&node, &mut PaintCx::new(&mut frame));
        (0..height)
            .map(|row| frame.buffer().row_symbols(row).unwrap_or_default())
            .collect()
    };
    let geometry = produced.content.geometry(&node);
    Rendered {
        node,
        rows,
        geometry,
        work: produced.work,
    }
}

fn render(item: &TranscriptViewItem, width: u16) -> Rendered {
    render_with(item, width, &RenderConfig::default(), &default_structure())
}

fn trimmed(rows: &[String]) -> Vec<String> {
    rows.iter().map(|row| row.trim_end().to_owned()).collect()
}

/// Geometry stays inside the measured rectangle (B3, B7).
fn assert_geometry_inside(rendered: &Rendered) {
    let size = rendered.node.size;
    for region in &rendered.geometry.hit_regions {
        assert!(
            region.rect.x + region.rect.width <= size.width,
            "{region:?} {size:?}"
        );
        assert!(
            region.rect.y + region.rect.height <= size.height,
            "{region:?} {size:?}"
        );
    }
    for target in &rendered.geometry.focus_targets {
        assert!(target.rows.end <= size.height, "{target:?}");
    }
    for row in rendered.geometry.source_map.rows() {
        assert!(row.row < size.height.max(1), "{row:?}");
    }
}

#[test]
fn prefix_speaker_renders_heading_indented_body_and_trailing_row() {
    let rendered = render(&user("hello world"), 40);
    assert_eq!(trimmed(&rendered.rows), vec!["You", "  hello world", ""]);
    // The heading row is the disclosure focus target and toggle hit region.
    let target = default_structure().target().cloned().expect("item target");
    assert_eq!(rendered.geometry.focus_targets[0].rows, 0..1);
    assert_eq!(
        rendered.geometry.focus_targets[0].disclosure,
        Some(target.clone())
    );
    assert_eq!(
        rendered.geometry.hit_regions[0].action,
        ItemAction::ToggleDisclosure(target)
    );
    // The body row maps to the first source byte (C24).
    assert_eq!(
        rendered.geometry.source_map.source_for_row(1),
        Some((SourcePosition::MarkdownByte(0), 1))
    );
    assert_eq!(rendered.work.markdown_parses, 1);
    assert_eq!(rendered.work.projections, 1);
}

#[test]
fn x4_speaker_variants_share_one_layout_definition() {
    let item = user("hello world");

    let badge = render_with(
        &item,
        40,
        &speaker(SpeakerStyle::Badge),
        &default_structure(),
    );
    assert_eq!(trimmed(&badge.rows), vec![" You", "  hello world", ""]);

    let bar = render_with(&item, 40, &speaker(SpeakerStyle::Bar), &default_structure());
    let bar_rows = trimmed(&bar.rows);
    assert_eq!(bar_rows[0], "▎ You");
    assert_eq!(bar_rows[1], "▎   hello world");
    // The heading hit region starts after the bar prefix: paint and hits agree (B3).
    assert_eq!(
        bar.geometry.hit_regions[0].rect.x,
        SpeakerLayout::bar_prefix()
    );

    let gutter = render_with(
        &item,
        40,
        &speaker(SpeakerStyle::Gutter),
        &default_structure(),
    );
    let gutter_rows = trimmed(&gutter.rows);
    assert_eq!(gutter_rows[0], "You       hello world");
    assert_eq!(
        gutter.geometry.hit_regions[0].rect.width,
        GUTTER_LABEL_CELLS
    );
    // The body source row sits beside the label.
    assert_eq!(
        gutter.geometry.source_map.source_for_row(0),
        Some((SourcePosition::MarkdownByte(0), 0))
    );
    for rendered in [&badge, &bar, &gutter] {
        assert_geometry_inside(rendered);
    }
}

fn markers(user: &str, assistant: &str) -> RenderConfig {
    config_with(|theme| {
        theme.chrome.speaker = SpeakerStyle::Marker;
        theme.chrome.markers.user = bcode_tui_theme::chrome::ChromeText::new(user);
        theme.chrome.markers.assistant = bcode_tui_theme::chrome::ChromeText::new(assistant);
    })
}

#[test]
fn marker_speaker_replaces_the_heading_and_hangs_the_body_beside_the_marker() {
    let config = markers("❯", "⏺");
    let rendered = render_with(&user("hello world"), 40, &config, &default_structure());
    assert_eq!(trimmed(&rendered.rows), vec!["❯ hello world", ""]);
    // The marker is the disclosure target; the body source row starts beside it.
    assert_eq!(rendered.geometry.hit_regions[0].rect.x, 0);
    assert_eq!(rendered.geometry.hit_regions[0].rect.width, 2);
    assert_eq!(
        rendered.geometry.source_map.source_for_row(0),
        Some((SourcePosition::MarkdownByte(0), 0))
    );
    // Wrapped rows hang under the text, not under the marker.
    let wrapped = render_with(
        &assistant("one two three", false),
        9,
        &config,
        &default_structure(),
    );
    assert_eq!(
        trimmed(&wrapped.rows)[..2],
        ["⏺ one two".to_owned(), "  three".to_owned()]
    );
    // A two-cell glyph widens the marker column by its measured width.
    let wide = render_with(&user("hi"), 20, &markers("中", "⏺"), &default_structure());
    assert_eq!(trimmed(&wide.rows)[0], "中 hi");
    for rendered in [&rendered, &wrapped, &wide] {
        assert_geometry_inside(rendered);
    }
}

#[test]
fn marker_speaker_degrades_to_the_heading_when_narrow_or_not_expanded() {
    let config = markers("❯", "⏺");
    assert_eq!(
        SpeakerLayout::resolve(SpeakerStyle::Marker, true, 2 + MIN_MARKER_BODY_CELLS - 1, 2),
        SpeakerLayout::PREFIX
    );
    let narrow = render_with(&user("hi"), 4, &config, &default_structure());
    assert_eq!(trimmed(&narrow.rows)[0], "You");
    let collapsed = render_with(
        &user("hello"),
        40,
        &config,
        &with_presentation(|presentation| presentation.disclosure = Disclosure::Collapsed),
    );
    assert!(
        trimmed(&collapsed.rows)[0].contains("You"),
        "{:?}",
        collapsed.rows
    );
}

#[test]
fn b4_speaker_chrome_degrades_explicitly_at_narrow_widths() {
    let item = user("hi");
    assert_eq!(
        SpeakerLayout::resolve(SpeakerStyle::Gutter, true, MIN_GUTTER_WIDTH - 1, 0),
        SpeakerLayout::PREFIX
    );
    assert_eq!(
        SpeakerLayout::resolve(SpeakerStyle::Bar, true, 3, 0),
        SpeakerLayout::PREFIX
    );
    assert_eq!(
        SpeakerLayout::resolve(SpeakerStyle::Gutter, false, 80, 0),
        SpeakerLayout::PREFIX,
        "only user and assistant messages carry a speaker"
    );
    let gutter = render_with(
        &item,
        12,
        &speaker(SpeakerStyle::Gutter),
        &default_structure(),
    );
    assert_eq!(trimmed(&gutter.rows)[0], "You");
    // A badge that does not fit falls back to the plain heading.
    let badge = render_with(
        &item,
        4,
        &speaker(SpeakerStyle::Badge),
        &default_structure(),
    );
    assert_eq!(trimmed(&badge.rows)[0], "You");
}

#[test]
fn b8_matrix_zero_tiny_widths_wide_graphemes_and_styled_spans_stay_contained() {
    let combining = "e\u{301}".repeat(12);
    let sources = [
        "漢字漢字漢字 漢字",
        "😀😀 👩‍💻 😀😀😀",
        combining.as_str(),
        "**bold** _emphasis_ `code` plain",
        "x",
    ];
    for style in [
        SpeakerStyle::Prefix,
        SpeakerStyle::Badge,
        SpeakerStyle::Bar,
        SpeakerStyle::Gutter,
        SpeakerStyle::Marker,
    ] {
        let config = speaker(style);
        for source in sources {
            for width in [0u16, 1, 2, 3, 5, 8, 21, 80] {
                let rendered = render_with(&user(source), width, &config, &default_structure());
                assert!(rendered.node.size.height > 0, "{style:?} {width} {source}");
                for row in &rendered.rows {
                    assert!(
                        display_width(row) <= usize::from(width),
                        "{style:?} {width} {source}: {row:?}"
                    );
                }
                assert_geometry_inside(&rendered);
            }
        }
    }
}

#[test]
fn b8_resize_reflows_and_keeps_every_body_grapheme() {
    let source = "漢字 😀 e\u{301} word another longer sentence";
    let wide = render(&user(source), 60);
    let narrow = render(&user(source), 10);
    assert!(narrow.node.size.height > wide.node.size.height);
    let joined = |rows: &[String]| rows.concat().replace(' ', "");
    for needle in ["漢字", "😀", "e\u{301}", "longer", "sentence"] {
        assert!(joined(&wide.rows).contains(needle), "{needle}");
        assert!(joined(&narrow.rows).contains(needle), "{needle}");
    }
}

#[test]
fn c24_markdown_source_anchors_resolve_after_width_change() {
    let first = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
    let second = "lambda mu nu xi omicron pi rho sigma tau upsilon";
    let source = format!("{first}\n\n{second}");
    let anchor = SourcePosition::MarkdownByte((first.len() + 2) as u64);
    for width in [80u16, 24, 12] {
        let rendered = render(&assistant(&source, false), width);
        let row = rendered
            .geometry
            .source_map
            .row_for_source(&anchor)
            .expect("mapped anchor");
        let row = usize::try_from(row).expect("row");
        assert!(
            rendered.rows[row].contains("lambda"),
            "width {width}: row {row} {:?}",
            rendered.rows
        );
    }
}

#[test]
fn c38_streaming_markdown_keeps_accepted_source_redacts_and_converges() {
    let partial = "Done with **bold** and [docs](javascript:alert(1";
    let streaming = render(&assistant(partial, true), 60);
    let text = streaming.rows.concat();
    assert!(text.contains("Done with"), "{:?}", streaming.rows);
    assert!(!text.contains("javascript"), "{:?}", streaming.rows);
    assert!(
        trimmed(&streaming.rows)[0].ends_with('…'),
        "streaming title"
    );
    // Streaming revisions skip provenance; rows fall back to item-row anchors.
    assert!(streaming.geometry.source_map.is_empty());

    let complete = "Done with **bold** and [docs](https://example.com).";
    let finished = render(&assistant(complete, false), 60);
    let again = render(&assistant(complete, false), 60);
    assert_eq!(
        finished.rows, again.rows,
        "the final projection is deterministic"
    );
    assert!(!finished.geometry.source_map.is_empty());
    assert!(!trimmed(&finished.rows)[0].ends_with('…'));
    // The link is an interactive, namespaced content region (E3).
    assert!(finished.geometry.hit_regions.iter().any(|region| matches!(
        &region.action,
        ItemAction::Activate(key) if key.starts_with(markdown::CONTENT_KEY_PREFIX)
    )));
}

#[test]
fn disclosure_levels_select_heading_summary_and_body() {
    let item = user("first line of the message\nsecond line");
    let rows = |disclosure: Disclosure| {
        trimmed(
            &render_with(
                &item,
                40,
                &RenderConfig::default(),
                &with_presentation(|presentation| presentation.disclosure = disclosure),
            )
            .rows,
        )
    };
    let expanded = rows(Disclosure::Expanded);
    assert_eq!(expanded[0], "You");
    assert!(expanded.iter().any(|row| row.contains("second line")));
    assert_eq!(
        rows(Disclosure::Summary),
        vec!["▸ You", "  first line of the message", ""]
    );
    assert_eq!(rows(Disclosure::Collapsed), vec!["▸ You", ""]);
    assert_eq!(rows(Disclosure::Hidden), vec!["▸ 1 message hidden", ""]);

    let hidden = render_with(
        &item,
        40,
        &RenderConfig::default(),
        &with_presentation(|presentation| presentation.disclosure = Disclosure::Hidden),
    );
    assert!(matches!(
        hidden.geometry.hit_regions[0].action,
        ItemAction::ToggleDisclosure(_)
    ));
    assert_eq!(
        hidden.work,
        ProducerWork::default(),
        "no body work when hidden"
    );
}

#[test]
fn header_fields_summary_templates_spacing_and_indent_apply() {
    let item = user("body text");
    let structure = with_presentation(|presentation| {
        presentation.disclosure = Disclosure::Summary;
        presentation.header = Some(HeaderSpec {
            fields: vec!["marker".to_owned(), "label".to_owned(), "status".to_owned()],
            template: None,
        });
        presentation.summary = SummarySpec {
            source: vec![SummarySource::Template],
            template: Some("said {title}".to_owned()),
        };
        presentation.spacing = Spacing {
            before: 1,
            after: 0,
        };
        presentation.indent = 2;
    });
    let rendered = render_with(&item, 40, &RenderConfig::default(), &structure);
    assert_eq!(trimmed(&rendered.rows), vec!["", "  ▸ You", "    said You"]);
    assert_eq!(rendered.geometry.focus_targets[0].rows, 1..2);
    assert_eq!(rendered.geometry.hit_regions[0].rect.x, 2);
}

#[test]
fn x4_theme_containers_frame_messages_and_drop_when_narrow() {
    let config = config_with(|theme| {
        theme.containers.insert(
            "transcript.user".to_owned(),
            ContainerRecipe {
                layout: ContainerLayout::Panel,
                width: ContainerWidth::Full,
                border: ContainerBorder::All,
                padding_x: 1,
                padding_y: 0,
            },
        );
    });
    let framed = render_with(&user("hi"), 20, &config, &default_structure());
    let rows = trimmed(&framed.rows);
    assert!(rows[0].starts_with('┌'), "{rows:?}");
    assert!(rows[1].starts_with("│ You"), "{rows:?}");
    assert_eq!(framed.geometry.hit_regions[0].rect.x, 2);
    assert_geometry_inside(&framed);
    // A frame that would leave no content column is dropped (B4).
    let narrow = render_with(&user("hi"), 4, &config, &default_structure());
    assert_eq!(trimmed(&narrow.rows)[0], "You");
}

#[test]
fn every_non_tool_kind_renders_with_its_heading() {
    let compaction = view_item(
        TranscriptViewItemKind::Compaction {
            compaction: CompactionView {
                status: CompactionViewStatus::Local,
                text: "Context compacted".to_owned(),
                provider_plugin_id: None,
                model_id: None,
            },
        },
        false,
    );
    assert_eq!(
        trimmed(&render(&compaction, 40).rows),
        vec!["· Context compacted"]
    );

    let skill = |status| {
        view_item(
            TranscriptViewItemKind::Skill {
                skill: SkillView {
                    skill_id: "s".to_owned(),
                    status,
                    text: "details".to_owned(),
                },
            },
            false,
        )
    };
    for (status, title) in [
        (SkillViewStatus::Invoked, "Skill"),
        (SkillViewStatus::Suggested, "Skill"),
        (SkillViewStatus::ContextLoaded, "Skill context"),
        (SkillViewStatus::Failed, "Skill error"),
    ] {
        let rows = trimmed(&render(&skill(status), 40).rows);
        assert_eq!(rows, vec![title, "  details", ""], "{status:?}");
    }

    let mut message = ChatMessageView::plain("status text");
    message.display_label = Some("Loop".to_owned());
    let plugin = view_item(TranscriptViewItemKind::SystemMessage { message }, false);
    assert_eq!(trimmed(&render(&plugin, 40).rows)[0], "Plugin · Loop");
    let system = view_item(
        TranscriptViewItemKind::SystemMessage {
            message: ChatMessageView::plain("notice"),
        },
        false,
    );
    assert_eq!(
        trimmed(&render(&system, 40).rows),
        vec!["System", "  notice", ""]
    );
}

#[test]
fn reasoning_assistant_and_json_variants_render() {
    let legacy = view_item(
        TranscriptViewItemKind::ReasoningMessage {
            message: ChatMessageView::markdown("- step"),
        },
        true,
    );
    assert_eq!(trimmed(&render(&legacy, 40).rows)[0], "Reasoning …");

    let activity = |status, finished, opaque| {
        view_item(
            TranscriptViewItemKind::ReasoningActivity {
                activity: ReasoningActivityView {
                    turn_id: "t".to_owned(),
                    activity_id: "a".to_owned(),
                    order: 0,
                    status,
                    parts: Vec::new(),
                    opaque,
                    readable_parts_filtered: false,
                    finished,
                },
            },
            false,
        )
    };
    let withheld = trimmed(
        &render(
            &activity(ReasoningActivityStatus::Completed, true, true),
            100,
        )
        .rows,
    );
    assert_eq!(withheld[0], "Reasoning");
    assert!(
        withheld[1].contains("did not return readable reasoning"),
        "{withheld:?}"
    );
    let failed =
        trimmed(&render(&activity(ReasoningActivityStatus::Failed, false, false), 80).rows);
    assert_eq!(failed, vec!["Reasoning failed", "  ·", ""]);

    let mut labeled = ChatMessageView::markdown("ok");
    labeled.display_label = Some("Claude Code".to_owned());
    let external = view_item(
        TranscriptViewItemKind::AssistantMessage { message: labeled },
        false,
    );
    assert_eq!(trimmed(&render(&external, 40).rows)[0], "Claude Code");
    assert_eq!(
        trimmed(&render(&assistant("ok", false), 40).rows)[0],
        "Bcode"
    );

    let json = view_item(
        TranscriptViewItemKind::UserMessage {
            message: ChatMessageView {
                format: TextFormat::Json,
                ..ChatMessageView::plain("{\"a\":1}")
            },
        },
        false,
    );
    let rows = trimmed(&render(&json, 40).rows);
    assert!(rows.iter().any(|row| row.trim() == "\"a\": 1"), "{rows:?}");
}

#[test]
fn l8_mermaid_placeholders_reserve_rows_as_rendered_fragments() {
    let source = "before\n\n```mermaid\ngraph TD; A-->B;\n```\n\nafter";
    let mut config = RenderConfig::default();
    config.markdown.mermaid = true;
    let rendered = render_with(&assistant(source, false), 60, &config, &default_structure());
    let placeholder = rendered
        .geometry
        .selection_fragments
        .iter()
        .find(|fragment| fragment.rendered)
        .expect("rendered placeholder fragment");
    assert!(
        placeholder.rows.end - placeholder.rows.start >= u64::from(markdown::MERMAID_RESERVED_ROWS),
        "{placeholder:?}"
    );
    assert!(
        rendered
            .geometry
            .content_keys
            .iter()
            .any(|key| key.key.starts_with(markdown::CONTENT_KEY_PREFIX))
    );
    let disabled = render(&assistant(source, false), 60);
    assert!(
        disabled
            .geometry
            .selection_fragments
            .iter()
            .all(|fragment| !fragment.rendered),
        "Mermaid stays highlighted source without the option"
    );
}
