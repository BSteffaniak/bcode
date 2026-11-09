//! Plugin visual producer behavior (L8, A8, E3, E5): accepted adapter rows render inside the
//! host tool chrome as a bounded, clipped, keyed `visual` section; without an accepted visual
//! the unit renders the renderer-neutral canonical fallback; acceptance changes the layout
//! fingerprint.

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
        AcceptedPluginVisual {
            adapter: "example.shell/card".to_owned(),
            revision: 1,
            rows: rows
                .iter()
                .map(|row| Line::raw((*row).to_owned()))
                .collect::<Vec<_>>()
                .into(),
        },
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
