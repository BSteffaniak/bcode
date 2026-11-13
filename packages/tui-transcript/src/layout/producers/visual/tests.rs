//! Plugin visual producer behavior (L8, A8, E3, E5, C36, C39): accepted adapter rows render
//! inside the host tool chrome as a bounded, clipped, keyed `visual` section with the adapter's
//! header facts, named sections, anchors and selection rows; without an accepted visual the
//! unit renders the renderer-neutral canonical fallback; acceptance changes the layout
//! fingerprint; canonical shell screens scroll horizontally.

use std::sync::Arc;

use bcode_session_view_models::ToolInvocationViewStatus;
use bmux_tui::text::Line;
use bmux_tui::text_width::display_width;
use serde_json::json;

use super::*;
use crate::layout::context::AcceptedPluginVisual;
use crate::layout::producers::group::harness::{
    Fixture, assert_geometry_inside, config, id, invocation, tool_item, user,
};
use crate::layout::producers::{ProducerKind, producer_kind};
use crate::units::UnitKey;
use crate::units::structure::{DisclosureTarget, StructureConfig};

fn shell_fixture(config: StructureConfig, status: ToolInvocationViewStatus) -> Fixture {
    Fixture::new(
        config,
        vec![
            user("u"),
            tool_item(
                "t",
                invocation("shell", status, Some(r#"{"command":"ls"}"#), Some("a\nb")),
            ),
        ],
    )
}

fn accept(fixture: &mut Fixture, rows: &[&str]) {
    fixture.render.plugin_visuals.accept(
        id("t"),
        AcceptedPluginVisual::rows(
            "example.shell/card",
            1,
            rows.iter()
                .map(|row| Line::raw((*row).to_owned()))
                .collect::<Vec<_>>()
                .into(),
        ),
    );
}

#[test]
fn a8_without_an_accepted_visual_the_canonical_fallback_renders() {
    let fixture = shell_fixture(
        StructureConfig::default(),
        ToolInvocationViewStatus::Finished,
    );
    let key = UnitKey::Item(id("t"));
    let unit = fixture.units.entry(&key).expect("unit");
    let structure = fixture.resolver.unit_presentation(&key);
    let cx = fixture.render.context(&key, 40, structure);
    let item = fixture.units.item(&fixture.items, &id("t"));
    assert_eq!(
        producer_kind(UnitView { unit, item }, &cx),
        ProducerKind::Tool
    );
    let rendered = fixture.render_item("t", 40);
    assert!(rendered.text().contains("command"), "{:?}", rendered.rows);
    assert!(
        rendered
            .geometry
            .content_keys
            .iter()
            .all(|key| !key.key.ends_with(":visual"))
    );
}

#[test]
fn l8_e3_accepted_rows_render_as_a_keyed_rendered_visual_section() {
    let mut fixture = shell_fixture(
        StructureConfig::default(),
        ToolInvocationViewStatus::Finished,
    );
    let before = {
        let key = UnitKey::Item(id("t"));
        let structure = fixture.resolver.unit_presentation(&key);
        fixture.render.context(&key, 40, structure).fingerprint()
    };
    accept(&mut fixture, &["┌ card ┐", "│ ls   │", "└──────┘"]);
    let key = UnitKey::Item(id("t"));
    let structure = fixture.resolver.unit_presentation(&key);
    assert_ne!(
        fixture.render.context(&key, 40, structure).fingerprint(),
        before,
        "acceptance is a layout revision"
    );
    let rendered = fixture.render_item("t", 40);
    let rows: Vec<_> = rendered.rows.iter().map(|row| row.trim_end()).collect();
    assert_eq!(
        rows,
        vec![
            "▾ Tool result · shell · finished",
            "┌ card ┐",
            "│ ls   │",
            "└──────┘",
            ""
        ],
        "the adapter replaces arguments and output; the host keeps the header"
    );
    let key = rendered
        .geometry
        .content_keys
        .iter()
        .find(|key| key.key == "example.shell/card:visual")
        .expect("namespaced content key");
    assert_eq!(key.rows, 1..4);
    let fragment = rendered
        .geometry
        .selection_fragments
        .iter()
        .find(|fragment| fragment.rows == (1..4))
        .expect("visual fragment");
    assert!(fragment.rendered && fragment.source.is_none());
    assert_eq!(
        rendered.focus_row(&DisclosureTarget::Unit(id("t"))),
        Some(0)
    );
    assert_geometry_inside(&rendered, 40);
}

#[test]
fn visual_rows_clip_bound_and_keep_the_failure_notice() {
    let rules = json!([{
        "match": { "kind": "tool" },
        "set": { "sections": { "visual": { "preview": { "lines": 2, "keep": "head" } } } }
    }]);
    let mut fixture = shell_fixture(config(rules), ToolInvocationViewStatus::Failed);
    accept(
        &mut fixture,
        &["漢字漢字漢字漢字漢字", "😀😀😀😀😀😀", "third", "fourth"],
    );
    for width in [0u16, 1, 3, 7, 12, 40] {
        let rendered = fixture.render_item("t", width);
        assert!(
            rendered
                .rows
                .iter()
                .all(|row| display_width(row) <= usize::from(width)),
            "{width}: {:?}",
            rendered.rows
        );
        assert_geometry_inside(&rendered, width);
    }
    let rendered = fixture.render_item("t", 40);
    let text = rendered.text();
    assert!(text.contains("… 2 visual rows hidden …"), "{text}");
    assert!(text.contains("tool failed"), "{text}");
    assert!(!text.contains("third"), "{text}");
}

#[test]
fn collapsed_visual_sections_count_rows_and_toggle() {
    let mut fixture = shell_fixture(
        StructureConfig::default(),
        ToolInvocationViewStatus::Finished,
    );
    accept(&mut fixture, &["one", "two"]);
    let target = DisclosureTarget::Section(id("t"), "visual".to_owned());
    fixture.toggle(&target);
    let rendered = fixture.render_item("t", 40);
    assert_eq!(rendered.rows[1].trim_end(), "  ▸ visual · 2 rows");
    assert_eq!(rendered.hit(&target).map(|rect| rect.y), Some(1));
}

// ---- full plugin visual presentation ------------------------------------------------------

use crate::layout::SourcePosition;
use crate::layout::geometry::SelectableRunKind;

fn lines(rows: &[&str]) -> Arc<[Line]> {
    rows.iter()
        .map(|row| Line::raw((*row).to_owned()))
        .collect::<Vec<_>>()
        .into()
}

fn accept_visual(fixture: &mut Fixture, visual: AcceptedPluginVisual) {
    fixture.render.plugin_visuals.accept(id("t"), visual);
}

fn source(identity: &str, start: u64, end: u64) -> VisualSourceRange {
    VisualSourceRange {
        identity: identity.to_owned(),
        start,
        end,
    }
}

fn anchor(row: usize, key: &str, source: Option<VisualSourceRange>) -> VisualAnchor {
    VisualAnchor {
        row,
        key: key.to_owned(),
        source,
    }
}

/// Selection row showing `text` from `byte_start` with one one-cell cell per byte, starting at
/// `column`.
fn ascii_row(identity: &str, byte_start: usize, text: &str, column: u16) -> VisualSelectionRow {
    VisualSelectionRow {
        identity: identity.to_owned(),
        byte_start,
        text: text.to_owned(),
        cells: (0..text.len())
            .map(|index| VisualSelectionCell {
                column: column + u16::try_from(index).expect("column"),
                width: 1,
                bytes: byte_start + index..byte_start + index + 1,
            })
            .collect(),
    }
}

/// Content targets and selectable runs agree with the measured node (B3, B7).
fn assert_plugin_geometry_inside(rendered: &crate::layout::producers::group::harness::Rendered) {
    let (width, height) = (rendered.node.size.width, rendered.height());
    for target in &rendered.geometry.content_targets {
        let rect = target.rect;
        assert!(
            rect.width > 0 && rect.x + rect.width <= width && rect.y + rect.height <= height,
            "{target:?} outside {width}x{height}"
        );
    }
    for text in &rendered.geometry.selectable {
        for run in &text.runs {
            assert!(run.x + run.width <= width && run.row < height, "{run:?}");
            assert!(text.text.get(run.source.clone()).is_some(), "{run:?}");
        }
    }
}

/// E3: version 4 source ranges map their own row, version 3 keys map their row and the rows
/// below it with the row offset; positions resolve only inside their own identity.
#[test]
fn e3_source_ranges_and_content_keys_anchor_rows_inside_their_identity() {
    let mut fixture = shell_fixture(
        StructureConfig::default(),
        ToolInvocationViewStatus::Finished,
    );
    let rows = lines(&["card", "screen 0", "screen 1", "line a", "after a"]);
    accept_visual(
        &mut fixture,
        AcceptedPluginVisual {
            correspondence: Arc::new(VisualCorrespondence::new(
                vec![
                    anchor(3, "p:row-a", Some(source("p:line:a", 10, 16))),
                    anchor(1, "p:screen", None),
                ],
                [],
                rows.len(),
            )),
            ..AcceptedPluginVisual::rows("p:card", 1, rows)
        },
    );
    let rendered = fixture.render_item("t", 40);
    assert_plugin_geometry_inside(&rendered);
    let row = |needle: &str| rendered.row_of(needle).expect(needle);
    let map = &rendered.geometry.source_map;
    let plugin = |identity: &str, offset| SourcePosition::PluginSource {
        identity: identity.to_owned(),
        offset,
    };
    assert_eq!(
        map.row_for_source(&plugin("p:screen", 1)),
        Some(row("screen 1"))
    );
    assert_eq!(
        map.row_for_source(&plugin("p:screen", 0)),
        Some(row("screen 0"))
    );
    // Containment inside the identity: an offset within the range resolves to its row.
    assert_eq!(
        map.row_for_source(&plugin("p:line:a", 12)),
        Some(row("line a"))
    );
    // Another identity never resolves to these rows, even when it orders between them.
    assert_eq!(map.row_for_source(&plugin("p:line:0", 0)), None);
    assert_eq!(map.row_for_source(&plugin("p:other", 0)), None);
    assert_eq!(
        map.source_for_row(row("screen 1")),
        Some((plugin("p:screen", 1), row("screen 1")))
    );
    // Rows of a key region and of a source row are routed to the adapter under their identity;
    // the unmapped row below a source range is not a target.
    let targets: Vec<_> = rendered
        .geometry
        .content_targets
        .iter()
        .map(|target| {
            (
                target.rect.y..target.rect.y + target.rect.height,
                &target.kind,
            )
        })
        .collect();
    assert_eq!(
        targets,
        vec![
            (
                row("screen 0")..row("screen 1") + 1,
                &ContentTargetKind::PluginVisual {
                    identity: "p:screen".to_owned()
                }
            ),
            (
                row("line a")..row("line a") + 1,
                &ContentTargetKind::PluginVisual {
                    identity: "p:line:a".to_owned()
                }
            ),
        ]
    );
}

/// C36: rows with adapter selection metadata copy the adapter's source (formatter output copies
/// the original command); contiguous rows of one identity join without a newline, an identity
/// change starts a new part, rows without metadata copy their painted text, and cells clipped
/// at the body width are not selectable.
#[test]
fn c36_selection_rows_copy_the_adapter_source_and_clip_at_the_body() {
    let mut fixture = shell_fixture(
        StructureConfig::default(),
        ToolInvocationViewStatus::Finished,
    );
    // Painted: a formatted command over two soft-wrapped rows, then output.
    let rows = lines(&["$ ls -la", "  /tmp", "plain output"]);
    let selection = vec![
        (0, ascii_row("p:cmd", 0, "ls  -la ", 2)),
        (1, ascii_row("p:cmd", 8, "/tmp", 2)),
    ];
    accept_visual(
        &mut fixture,
        AcceptedPluginVisual {
            correspondence: Arc::new(VisualCorrespondence::new(Vec::new(), selection, rows.len())),
            ..AcceptedPluginVisual::rows("p:card", 1, rows)
        },
    );
    let rendered = fixture.render_item("t", 40);
    assert_plugin_geometry_inside(&rendered);
    let texts: Vec<&str> = rendered
        .geometry
        .selectable
        .iter()
        .map(|text| &*text.text)
        .collect();
    assert_eq!(texts, vec!["ls  -la /tmp", "plain output"]);
    let command = &rendered.geometry.selectable[0];
    assert!(
        command
            .runs
            .iter()
            .all(|run| run.kind == SelectableRunKind::Unit && run.width == 1)
    );
    assert_eq!(command.runs.len(), 12, "one fragment per adapter cell");
    assert_eq!(
        command.runs[0].row,
        rendered.row_of("$ ls").expect("command row")
    );

    // At a narrow width, cells beyond the body are not selectable (never mapped untruthfully).
    let narrow = fixture.render_item("t", 6);
    assert_plugin_geometry_inside(&narrow);
    let narrow_command = narrow
        .geometry
        .selectable
        .iter()
        .find(|text| text.text.starts_with("ls"))
        .expect("command part");
    assert!(narrow_command.runs.len() < command.runs.len());
}

/// Header facts follow the plugin contract as in V1: transcript-block visuals show the adapter title instead of label, tool and status,
/// and full-block visuals draw their own header while expanded (the first adapter row is the
/// unit's focus target); collapsed, the host header returns.
#[test]
fn render_modes_header_facts_and_full_block_focus() {
    let mut fixture = shell_fixture(
        StructureConfig::default(),
        ToolInvocationViewStatus::Finished,
    );
    accept_visual(
        &mut fixture,
        AcceptedPluginVisual {
            mode: VisualRenderMode::TranscriptBlock,
            title: Some("Shell card".to_owned()),
            target: Some("~/work".to_owned()),
            ..AcceptedPluginVisual::rows("p:card", 1, lines(&["body"]))
        },
    );
    let block = fixture.render_item("t", 60);
    let header = block.rows[0].trim_end();
    assert!(header.contains("Shell card"), "{header}");
    assert!(
        !header.contains("Tool result") && !header.contains("finished"),
        "{header}"
    );

    accept_visual(
        &mut fixture,
        AcceptedPluginVisual {
            mode: VisualRenderMode::FullBlock,
            ..AcceptedPluginVisual::rows("p:card", 2, lines(&["┌ own header ┐", "body"]))
        },
    );
    let full = fixture.render_item("t", 60);
    assert!(full.rows[0].contains("own header"), "{:?}", full.rows);
    assert!(!full.text().contains("Tool result"), "{:?}", full.rows);
    assert_eq!(
        full.focus_row(&DisclosureTarget::Unit(id("t"))),
        Some(0),
        "the first adapter row stands in for the omitted header"
    );
    assert_geometry_inside(&full, 60);
    fixture.toggle(&DisclosureTarget::Unit(id("t")));
    let collapsed = fixture.render_item("t", 60);
    assert!(
        collapsed.rows[0].contains("Tool result"),
        "{:?}",
        collapsed.rows
    );
    assert!(!collapsed.text().contains("own header"));
}

/// Named sections (plugin contract v3) follow the adapter rows under their labels (or leaf
/// names), each bounded by its own preview; an adapter with only named sections renders no
/// `visual` section; the producer summary is the `plugin` summary source.
#[test]
fn named_sections_and_summary_render_like_v1() {
    let rules = json!([{
        "match": { "kind": "tool" },
        "set": { "sections": { "bcode.git.files": { "preview": { "lines": 1, "keep": "head" } } } }
    }]);
    let mut fixture = shell_fixture(config(rules), ToolInvocationViewStatus::Finished);
    accept_visual(
        &mut fixture,
        AcceptedPluginVisual {
            summary: Some("2 files changed".to_owned()),
            sections: Arc::from(vec![
                VisualSection {
                    name: "bcode.git.files".to_owned(),
                    label: None,
                    rows: lines(&["a.rs", "b.rs"]),
                },
                VisualSection {
                    name: "bcode.git.stats".to_owned(),
                    label: Some("Stats".to_owned()),
                    rows: lines(&["+3 -1"]),
                },
            ]),
            ..AcceptedPluginVisual::rows("p:card", 1, lines(&[]))
        },
    );
    let rendered = fixture.render_item("t", 40);
    let text = rendered.text();
    assert!(text.contains("files") && text.contains("Stats"), "{text}");
    assert!(text.contains("a.rs") && !text.contains("b.rs"), "{text}");
    assert!(text.contains("… 1 files rows hidden …"), "{text}");
    assert!(text.contains("+3 -1"), "{text}");
    assert!(
        rendered
            .geometry
            .content_keys
            .iter()
            .all(|key| !key.key.ends_with(":visual")),
        "no visual section without adapter rows"
    );

    let summary_rules =
        json!([{ "match": { "kind": "tool" }, "set": { "disclosure": "summary" } }]);
    let mut summarized = shell_fixture(config(summary_rules), ToolInvocationViewStatus::Finished);
    accept_visual(
        &mut summarized,
        AcceptedPluginVisual {
            summary: Some("2 files changed".to_owned()),
            ..AcceptedPluginVisual::rows("p:card", 1, lines(&["row"]))
        },
    );
    let rendered = summarized.render_item("t", 40);
    assert!(
        rendered.text().contains("2 files changed"),
        "{:?}",
        rendered.rows
    );
}

/// C39: a canonical shell screen scrolled horizontally shows the end of a line wider than the
/// body without wrapping it, stays source-mapped by byte, reports its clamped offset and
/// maximum, and an offset past the widest line is clamped.
#[test]
fn c39_shell_screens_scroll_horizontally_and_clamp() {
    let wide = format!("HEAD{}TAIL", "-".repeat(60));
    let fixture_for = |columns: u64| {
        let mut fixture = Fixture::new(
            StructureConfig::default(),
            vec![
                user("u"),
                tool_item(
                    "t",
                    invocation(
                        "bash",
                        ToolInvocationViewStatus::Finished,
                        Some(r#"{"command":"echo"}"#),
                        Some(&format!("short\n{wide}")),
                    ),
                ),
            ],
        );
        fixture.render.shell_scroll.set(id("t"), columns);
        fixture
    };
    let unscrolled = fixture_for(0).render_item("t", 40);
    assert!(unscrolled.text().contains("HEAD") && !unscrolled.text().contains("TAIL"));
    let Some(ContentTargetKind::ShellScreen {
        columns: 0,
        max_columns,
    }) = unscrolled
        .geometry
        .content_targets
        .iter()
        .map(|target| target.kind.clone())
        .next()
    else {
        panic!(
            "shell screen target: {:?}",
            unscrolled.geometry.content_targets
        );
    };
    assert!(max_columns > 0);
    let scrolled = fixture_for(u64::MAX).render_item("t", 40);
    assert_plugin_geometry_inside(&scrolled);
    let text = scrolled.text();
    assert!(text.contains("TAIL") && !text.contains("HEAD"), "{text}");
    assert_eq!(
        scrolled.height(),
        unscrolled.height(),
        "rows are never wrapped"
    );
    assert!(scrolled.geometry.content_targets.iter().all(|target| {
        target.kind
            == ContentTargetKind::ShellScreen {
                columns: max_columns,
                max_columns,
            }
    }));
    // The scrolled row still copies the original output bytes it shows.
    let tail = scrolled
        .geometry
        .selectable
        .iter()
        .find(|text| text.text.ends_with("TAIL"))
        .expect("scrolled wide line is selectable");
    assert!(!tail.text.contains("HEAD"));
    let fingerprint = |fixture: &Fixture| {
        let key = UnitKey::Item(id("t"));
        let structure = fixture.resolver.unit_presentation(&key);
        fixture.render.context(&key, 40, structure).fingerprint()
    };
    assert_ne!(
        fingerprint(&fixture_for(8)),
        fingerprint(&fixture_for(0)),
        "the offset is a layout input of its unit"
    );
}

/// A transcript-block adapter that supplies no title shows the host's fallback title for the
/// visual's source (V1: the artifact title, the tool name of a request), else the unit label.
#[test]
fn transcript_block_without_an_adapter_title_uses_the_fallback_title() {
    let mut fixture = shell_fixture(
        StructureConfig::default(),
        ToolInvocationViewStatus::Finished,
    );
    accept_visual(
        &mut fixture,
        AcceptedPluginVisual {
            mode: VisualRenderMode::TranscriptBlock,
            fallback_title: Some("Build report".to_owned()),
            ..AcceptedPluginVisual::rows("p:card", 1, lines(&["body"]))
        },
    );
    let header = fixture.render_item("t", 60).rows[0].clone();
    assert!(header.contains("Build report"), "{header}");
    accept_visual(
        &mut fixture,
        AcceptedPluginVisual {
            mode: VisualRenderMode::TranscriptBlock,
            ..AcceptedPluginVisual::rows("p:card", 2, lines(&["body"]))
        },
    );
    let header = fixture.render_item("t", 60).rows[0].clone();
    assert!(header.contains("Tool result"), "{header}");
}

/// [`tool::body_width`] is the width visual rows are laid out in: a row exactly that wide shows
/// whole inside the indented unit, and what lies past it is clipped.
#[test]
fn body_width_is_the_width_visual_rows_are_laid_out_in() {
    let rules = json!([{ "match": { "kind": "tool" }, "set": { "indent": 4 } }]);
    let mut fixture = shell_fixture(config(rules), ToolInvocationViewStatus::Finished);
    let key = UnitKey::Item(id("t"));
    let body = {
        let item = fixture.units.item(&fixture.items, &id("t")).expect("item");
        tool::body_width(
            item,
            fixture.resolver.unit_presentation(&key),
            &fixture.render.theme,
            40,
        )
    };
    assert!(body > 1 && body < 40, "the indent narrows the body: {body}");
    let cells = usize::try_from(body).expect("cells");
    let fits = format!("{}E", "x".repeat(cells - 1));
    let over = format!("{}#", "y".repeat(cells));
    accept_visual(
        &mut fixture,
        AcceptedPluginVisual::rows("p:card", 1, lines(&[&fits, &over])),
    );
    let rendered = fixture.render_item("t", 40);
    assert!(
        rendered
            .rows
            .iter()
            .any(|row| row.trim_end().ends_with(&fits)),
        "{:?}",
        rendered.rows
    );
    assert!(rendered.text().contains(&"y".repeat(cells)));
    assert!(!rendered.text().contains('#'), "{:?}", rendered.rows);
    assert_geometry_inside(&rendered, 40);
}
