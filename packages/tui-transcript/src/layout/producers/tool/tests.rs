//! Tool producer behavior: disclosure levels, section disclosure and previews, the visibility
//! floor, lifecycle states and their containers, chrome tool borders, diff and shell bodies,
//! source mapping, activity slots, renderer-neutral fallbacks, and the B8 matrix (zero and
//! tiny widths, CJK, emoji, combining marks, control characters, resize, and agreement between
//! painted rows and derived geometry).

use std::sync::Arc;

use bcode_session_models::{
    ToolContributionEvent, ToolContributionOperation, ToolContributionPersistence,
};
use bcode_session_view_models::{InteractionViewState, InteractionViewSummary};
use bcode_tui_theme::definition::{
    ContainerBorder, ContainerLayout, ContainerRecipe, ContainerWidth,
};
use bmux_tui::component::LogicalRect;
use bmux_tui::text_width::display_width;
use serde_json::json;

use super::*;
use crate::layout::SourcePosition;
use crate::layout::geometry::PaintSlotKind;
use crate::layout::producers::group::harness::{
    Fixture, Rendered, assert_geometry_inside, config, id, invocation, item, permission, tool_item,
    user,
};
use crate::units::UnitKey;
use crate::units::structure::StructureConfig;

const WIDTHS: [u16; 9] = [0, 1, 2, 3, 4, 6, 9, 16, 80];

fn lines(count: usize) -> String {
    (1..=count)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn read_with(output: &str) -> TranscriptViewItem {
    tool_item(
        "t",
        invocation(
            "read",
            ToolInvocationViewStatus::Finished,
            Some(r#"{"path":"a.rs"}"#),
            Some(output),
        ),
    )
}

fn single(config: StructureConfig, item: TranscriptViewItem) -> Fixture {
    Fixture::new(config, vec![user("u"), item])
}

fn trimmed(rendered: &Rendered) -> Vec<String> {
    rendered
        .rows
        .iter()
        .map(|row| row.trim_end().to_owned())
        .collect()
}

fn unit_target(name: &str) -> DisclosureTarget {
    DisclosureTarget::Unit(id(name))
}

fn section_target(name: &str, section: &str) -> DisclosureTarget {
    DisclosureTarget::Section(id(name), section.to_owned())
}

fn theme_with(edit: impl FnOnce(&mut ResolvedThemeDefinition)) -> Arc<ResolvedThemeDefinition> {
    let mut theme = ResolvedThemeDefinition::empty("t", "T");
    edit(&mut theme);
    Arc::new(theme)
}

#[test]
fn expanded_tools_render_header_then_labelled_sections() {
    let fixture = single(StructureConfig::default(), read_with("first\nsecond"));
    let rendered = fixture.render_item("t", 40);
    assert_eq!(
        trimmed(&rendered),
        vec![
            "▾ Tool result · read · finished",
            "  ▾ path",
            "    a.rs",
            "  ▾ output",
            "    first",
            "    second",
            "",
        ]
    );
    // B12: the unit header is target 0; section labels nest under it.
    let targets = &rendered.geometry.focus_targets;
    assert_eq!(targets[0].disclosure, Some(unit_target("t")));
    assert_eq!(targets[0].parent, None);
    assert_eq!(
        targets[1].disclosure,
        Some(section_target("t", "arguments"))
    );
    assert_eq!(targets[1].parent, Some(0));
    assert_eq!(rendered.focus_row(&section_target("t", "output")), Some(3));
    assert_eq!(
        rendered.hit(&section_target("t", "output")),
        Some(LogicalRect::new(2, 3, 38, 1)),
        "the label row's retained cells"
    );
    assert_geometry_inside(&rendered, 40);
}

#[test]
fn disclosure_levels_expand_summarize_and_collapse() {
    for (disclosure, expected) in [
        (
            "summary",
            vec!["▸ Tool result · read · finished", "  first", ""],
        ),
        ("collapsed", vec!["▸ Tool result · read · finished", ""]),
    ] {
        let rules = json!([{ "match": { "kind": "tool" }, "set": { "disclosure": disclosure } }]);
        let fixture = single(config(rules), read_with("first\nsecond"));
        let rendered = fixture.render_item("t", 40);
        assert_eq!(trimmed(&rendered), expected, "{disclosure}");
        assert_eq!(rendered.focus_row(&unit_target("t")), Some(0));
        assert_eq!(
            rendered.geometry.focus_targets.len(),
            1,
            "no section targets"
        );
    }
}

#[test]
fn summary_templates_substitute_header_fields() {
    let rules = json!([{
        "match": { "kind": "tool" },
        "set": { "disclosure": "summary",
                 "summary": { "source": ["template"], "template": "{tool} → {status}" } }
    }]);
    let fixture = single(config(rules), read_with("first"));
    let rendered = fixture.render_item("t", 40);
    assert_eq!(trimmed(&rendered)[1], "  read → finished");
}

#[test]
fn x1_section_toggles_collapse_and_summarize_one_section() {
    let mut fixture = single(StructureConfig::default(), read_with(&lines(14)));
    fixture.toggle(&section_target("t", "output"));
    let rendered = fixture.render_item("t", 40);
    assert_eq!(
        trimmed(&rendered),
        vec![
            "▾ Tool result · read · finished",
            "  ▾ path",
            "    a.rs",
            "  ▸ output · 14 rows",
            "",
        ]
    );
    assert_eq!(
        rendered
            .hit(&section_target("t", "output"))
            .map(|rect| rect.y),
        Some(3)
    );

    let rules = json!([{
        "match": { "kind": "tool" },
        "set": { "sections": { "output": { "disclosure": "summary" },
                               "arguments": { "visible": false } } }
    }]);
    let fixture = single(config(rules), read_with("\n  first\nsecond"));
    let rendered = fixture.render_item("t", 40);
    assert_eq!(
        trimmed(&rendered),
        vec![
            "▾ Tool result · read · finished",
            "  ▸ output · 3 rows",
            "    first",
            ""
        ]
    );
}

#[test]
fn previews_keep_head_tail_or_both_and_report_hidden_rows() {
    for (keep, expected) in [
        (
            "tail",
            ["    line 4", "    line 5", "    … 3 output rows hidden …"],
        ),
        (
            "head",
            ["    line 1", "    line 2", "    … 3 output rows hidden …"],
        ),
        (
            "both",
            ["    line 1", "    line 5", "    … 3 output rows hidden …"],
        ),
    ] {
        let rules = json!([{
            "match": { "kind": "tool" },
            "set": { "sections": { "arguments": { "visible": false },
                                   "output": { "preview": { "lines": 2, "keep": keep } } } }
        }]);
        let fixture = single(config(rules), read_with(&lines(5)));
        let rendered = fixture.render_item("t", 40);
        assert_eq!(trimmed(&rendered)[2..5], expected, "{keep}");
    }
}

#[test]
fn c24_output_rows_map_to_source_bytes_through_previews() {
    let rules = json!([{
        "match": { "kind": "tool" },
        "set": { "sections": { "arguments": { "visible": false },
                               "output": { "preview": { "lines": 2, "keep": "tail" } } } }
    }]);
    // "aa\nbbb\ncccc\n": the kept tail starts at byte 3.
    let fixture = single(config(rules), read_with("aa\nbbb\ncccc\n"));
    let rendered = fixture.render_item("t", 40);
    let map = &rendered.geometry.source_map;
    assert_eq!(
        map.source_for_row(2),
        Some((SourcePosition::ShellByte(3), 2))
    );
    assert_eq!(
        map.source_for_row(3),
        Some((SourcePosition::ShellByte(7), 3))
    );
    assert_eq!(map.row_for_source(&SourcePosition::ShellByte(8)), Some(3));
    let fragment = rendered
        .geometry
        .selection_fragments
        .iter()
        .find(|fragment| fragment.source.is_some())
        .expect("source-backed output fragment");
    assert_eq!(fragment.rows, 2..4);
    assert_eq!(fragment.source, Some(SourcePosition::ShellByte(3)));
}

#[test]
fn a16_floor_keeps_failed_tools_at_summary_and_decisions_expanded() {
    let rules = json!([
        { "match": { "kind": "tool" }, "set": { "disclosure": "collapsed" } },
        { "match": { "kind": "permission" }, "set": { "disclosure": "collapsed" } }
    ]);
    let failed = tool_item(
        "t",
        invocation(
            "read",
            ToolInvocationViewStatus::Failed,
            Some(r#"{"path":"a.rs"}"#),
            Some("no such file"),
        ),
    );
    let fixture = Fixture::new(config(rules), vec![user("u"), failed, permission("p")]);
    let rendered = fixture.render_item("t", 40);
    assert_eq!(
        trimmed(&rendered)[..2],
        ["▸ Tool result · read · failed", "  no such file"],
        "failed units never drop below summary"
    );

    let rendered = fixture.render_item("p", 60);
    let text = rendered.text();
    assert!(text.contains("Permission required · shell"), "{text}");
    assert!(text.contains("rm -rf build"), "arguments shown: {text}");
    assert!(
        text.contains("awaiting decision"),
        "confirmation shown: {text}"
    );
    assert_eq!(rendered.focus_row(&unit_target("p")), Some(0));
    assert_eq!(
        rendered.hit(&unit_target("p")),
        None,
        "a pending decision cannot be collapsed by clicking"
    );
    assert_geometry_inside(&rendered, 60);
}

fn status_item(status: ToolInvocationViewStatus, timed_out: bool) -> TranscriptViewItem {
    let mut tool = invocation("grep", status, None, None);
    tool.timing.timed_out = timed_out.then_some(true);
    tool.timing.started_at_ms = Some(1_000);
    tool.timing.timeout_ms = Some(30_000);
    if !matches!(
        status,
        ToolInvocationViewStatus::Running
            | ToolInvocationViewStatus::Waiting
            | ToolInvocationViewStatus::Requested
    ) {
        tool.timing.duration_ms = Some(1_500);
    }
    tool_item("t", tool)
}

#[test]
fn lifecycle_states_select_status_text_containers_and_timing() {
    use ToolInvocationViewStatus as S;
    let cases = [
        (
            S::Requested,
            false,
            "requested",
            "tool.requested",
            "timeout 30s",
        ),
        (S::Running, false, "running", "tool.running", "timeout 30s"),
        (S::Waiting, false, "waiting", "tool.waiting", "timeout 30s"),
        (
            S::Finished,
            false,
            "finished",
            "tool.succeeded",
            "duration 1s",
        ),
        (S::Failed, false, "failed", "tool.failed", "duration 1s"),
        (
            S::Cancelled,
            false,
            "cancelled",
            "tool.cancelled",
            "duration 1s",
        ),
        (S::Failed, true, "timed out", "tool.timed_out", "timed out"),
    ];
    for (status, timed_out, text, role, timing) in cases {
        let mut fixture = single(StructureConfig::default(), status_item(status, timed_out));
        // Only this status's container frames the unit, so the frame shows which applied.
        fixture.render.theme = theme_with(|theme| {
            theme.containers.insert(
                role.to_owned(),
                ContainerRecipe {
                    layout: ContainerLayout::Panel,
                    width: ContainerWidth::Full,
                    border: ContainerBorder::All,
                    padding_x: 0,
                    padding_y: 0,
                },
            );
        });
        let rendered = fixture.render_item("t", 60);
        let rows = trimmed(&rendered);
        assert!(rows[0].starts_with('┌'), "{role} frames the unit: {rows:?}");
        assert!(rows[1].contains(&format!("grep · {text}")), "{rows:?}");
        assert!(rows[1].contains(timing), "{timing}: {rows:?}");
        let failed = matches!(status, S::Failed);
        assert_eq!(
            rendered.text().contains("tool failed") || rendered.text().contains("tool timed out"),
            failed,
            "{text}: error section {rows:?}"
        );
        assert_geometry_inside(&rendered, 60);
    }
}

#[test]
fn x4_tool_borders_follow_chrome() {
    use bcode_tui_theme::chrome::BorderStyle;
    let cases = [
        (None, "▾ Tool result · read · finished"),
        (Some(BorderStyle::None), "▾ Tool result · read · finished"),
        (Some(BorderStyle::Left), "│▾ Tool result · read · finished"),
        (
            Some(BorderStyle::Rounded),
            "╭──────────────────────────────╮",
        ),
        (Some(BorderStyle::Ascii), "+------------------------------+"),
        (
            Some(BorderStyle::Double),
            "╔══════════════════════════════╗",
        ),
    ];
    for (border, first) in cases {
        let mut fixture = single(StructureConfig::default(), read_with("ok"));
        fixture.render.theme = theme_with(|theme| theme.chrome.borders.tool = border);
        let rendered = fixture.render_item("t", 32);
        let rows = trimmed(&rendered);
        assert_eq!(rows[0], first, "{border:?}: {rows:?}");
        let header = rendered
            .row_of("read · finished")
            .unwrap_or_else(|| panic!("{border:?}: header row in {rows:?}"));
        assert_eq!(
            rendered.focus_row(&unit_target("t")),
            Some(header),
            "{border:?}: focus target on the painted header row"
        );
        assert_eq!(
            rendered.hit(&unit_target("t")).map(|rect| rect.y),
            Some(header)
        );
        assert_geometry_inside(&rendered, 32);
        // Frames drop before they leave no content column (B4).
        for width in WIDTHS {
            assert_geometry_inside(&fixture.render_item("t", width), width);
        }
    }
}

#[test]
fn running_tools_reserve_spinner_and_elapsed_slots_without_reading_a_clock() {
    let fixture = single(
        StructureConfig::default(),
        status_item(ToolInvocationViewStatus::Running, false),
    );
    let rendered = fixture.render_item("t", 60);
    let slots: Vec<_> = rendered
        .geometry
        .paint_slots
        .iter()
        .map(|slot| (slot.kind, slot.rect))
        .collect();
    assert_eq!(
        slots,
        vec![
            (PaintSlotKind::Spinner, LogicalRect::new(51, 0, 1, 1)),
            (
                PaintSlotKind::Elapsed {
                    started_at_ms: 1_000
                },
                LogicalRect::new(53, 0, 7, 1)
            ),
        ]
    );
    let reserved: String = rendered.rows[0].chars().skip(50).collect();
    assert_eq!(reserved.trim(), "", "slots are blank");
    let finished = single(
        StructureConfig::default(),
        status_item(ToolInvocationViewStatus::Finished, false),
    );
    assert_eq!(
        finished.render_item("t", 60).geometry.paint_slots,
        Vec::new()
    );
}

#[test]
fn unified_diff_output_renders_as_a_diff_section_mapped_by_diff_line() {
    let diff = "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n";
    let fixture = single(StructureConfig::default(), read_with(diff));
    let rendered = fixture.render_item("t", 40);
    let rows = trimmed(&rendered);
    assert_eq!(rows[3], "  ▾ diff");
    assert_eq!(rows[8], "    +new");
    let map = &rendered.geometry.source_map;
    assert_eq!(
        map.source_for_row(4),
        Some((SourcePosition::DiffLine(0), 4))
    );
    assert_eq!(
        map.source_for_row(8),
        Some((SourcePosition::DiffLine(4), 8))
    );
    assert!(looks_like_diff(diff));
    assert!(!looks_like_diff("+ not a diff\n- list"));
}

#[test]
fn c39_shell_output_is_a_keyed_inline_screen_bounded_to_128_rows() {
    let rules = json!([{
        "match": { "kind": "tool" },
        "set": { "sections": { "output": { "preview": { "lines": 500, "keep": "tail" } } } }
    }]);
    let shell = tool_item(
        "t",
        invocation(
            "shell",
            ToolInvocationViewStatus::Finished,
            Some(r#"{"command":"seq 300"}"#),
            Some(&lines(300)),
        ),
    );
    let fixture = single(config(rules), shell);
    let rendered = fixture.render_item("t", 40);
    let key = rendered
        .geometry
        .content_keys
        .iter()
        .find(|key| key.key == "shell:call-shell")
        .expect("shell content key");
    assert_eq!(key.rows.end - key.rows.start, u64::from(SHELL_MAX_ROWS));
    assert!(rendered.text().contains("… 172 output rows hidden …"));
    // Lines clip rather than wrap: horizontal scrolling is host-local paint state.
    let wide = single(
        StructureConfig::default(),
        tool_item(
            "t",
            invocation(
                "shell",
                ToolInvocationViewStatus::Finished,
                Some(r#"{"command":"cat"}"#),
                Some(&"x".repeat(200)),
            ),
        ),
    );
    assert_eq!(
        wide.render_item("t", 20).height(),
        wide.render_item("t", 200).height()
    );
}

#[test]
fn b8_untrusted_output_is_bounded_and_cell_measured() {
    let combining = "e\u{301}".repeat(12);
    let cases = [
        "漢字漢字漢字漢字",
        "😀😀😀😀😀😀",
        combining.as_str(),
        "\u{1b}[31mred\u{7}\tbell",
    ];
    for output in cases {
        let fixture = single(StructureConfig::default(), read_with(output));
        for width in WIDTHS {
            let rendered = fixture.render_item("t", width);
            assert!(
                rendered
                    .rows
                    .iter()
                    .all(|row| display_width(row) <= usize::from(width)),
                "{output:?} at {width}: {:?}",
                rendered.rows
            );
            assert_geometry_inside(&rendered, width);
        }
        let rendered = fixture.render_item("t", 80);
        assert!(
            !rendered.text().contains('\u{1b}'),
            "control characters replaced"
        );
        assert_eq!(
            rendered.height(),
            fixture.render_item("t", 12).height(),
            "clipped rows keep their count across resize"
        );
    }
    let source = "a\u{1b}b\u{85}c";
    assert_eq!(
        sanitize(source).len(),
        source.len(),
        "byte offsets preserved"
    );
}

#[test]
fn b8_oversized_headers_drop_optional_fields_then_truncate() {
    let long = "a".repeat(100);
    let fixture = single(
        StructureConfig::default(),
        tool_item(
            "t",
            invocation(&long, ToolInvocationViewStatus::Finished, None, None),
        ),
    );
    for width in WIDTHS {
        let rendered = fixture.render_item("t", width);
        let first = rendered.rows.first().cloned().unwrap_or_default();
        assert!(display_width(&first) <= usize::from(width));
        assert_geometry_inside(&rendered, width);
    }
    let rendered = fixture.render_item("t", 12);
    assert!(
        !rendered.rows[0].contains("finished"),
        "{:?}",
        rendered.rows
    );
}

#[test]
fn e5_unknown_contributions_get_compact_fallbacks_never_raw_json() {
    let contribution = ToolContributionEvent {
        invocation_id: "call-x".to_owned(),
        contribution_id: "c".to_owned(),
        sequence: 1,
        producer_id: "example.plugin".to_owned(),
        schema: "example.card".to_owned(),
        schema_version: 2,
        operation: ToolContributionOperation::Upsert,
        persistence: ToolContributionPersistence::Durable,
        artifact: None,
        payload: json!({ "secret": "raw-value", "rows": [1, 2] }),
    };
    let fixture = single(
        StructureConfig::default(),
        item(
            "t",
            TranscriptViewItemKind::ToolContribution {
                contribution,
                placement: ToolContributionPlacement::Result,
                invocation: None,
            },
        ),
    );
    let rendered = fixture.render_item("t", 60);
    let text = rendered.text();
    assert!(text.contains("Tool result"), "{text}");
    assert!(text.contains("schema example.card v2"), "{text}");
    assert!(text.contains("payload 2 fields"), "{text}");
    assert!(!text.contains("raw-value") && !text.contains('{'), "{text}");
}

#[test]
fn interactions_summarize_state_and_snapshot() {
    let interaction = InteractionViewSummary {
        interaction_id: "i".to_owned(),
        producer_id: None,
        exchange_schema: None,
        exchange_schema_version: None,
        kind: "choice".to_owned(),
        tool_call_id: None,
        title: Some("Pick one".to_owned()),
        required: true,
        snapshot: Some(json!({ "options": ["a", "b"] })),
        state: InteractionViewState::Pending,
        status_detail: None,
        resolved: false,
        resolution: None,
    };
    let fixture = single(
        StructureConfig::default(),
        item("t", TranscriptViewItemKind::Interaction { interaction }),
    );
    let rendered = fixture.render_item("t", 60);
    let text = rendered.text();
    assert!(text.contains("Interaction"), "{text}");
    assert!(text.contains("awaiting input"), "{text}");
    assert!(text.contains("snapshot 1 fields"), "{text}");
    assert!(!text.contains("options"), "{text}");
}

#[test]
fn argument_fields_are_bounded() {
    let long = "x".repeat(500);
    let arguments = json!({
        "a": long, "b": 1, "c": true, "d": null, "e": [1, 2], "f": { "g": 1 }, "h": "dropped"
    })
    .to_string();
    let fields = argument_fields(&arguments);
    assert_eq!(fields.len(), MAX_ARGUMENT_FIELDS);
    assert_eq!(fields[0].1.chars().count(), MAX_ARGUMENT_VALUE_CHARS + 1);
    assert_eq!(fields[4].1, "2 values");
    assert_eq!(argument_fields("not json"), Vec::<(String, String)>::new());
}

#[test]
fn producers_are_pure_in_unit_and_context() {
    let fixture = single(StructureConfig::default(), read_with(&lines(3)));
    let first = fixture.render_item("t", 30);
    let second = fixture.render_item("t", 30);
    assert_eq!(first.rows, second.rows);
    assert_eq!(first.geometry, second.geometry);
    assert_eq!(first.key, UnitKey::Item(id("t")));
}
