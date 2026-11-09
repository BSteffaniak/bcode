//! Group header and hidden-marker behavior (X3, U2, A16): header fields per disclosure,
//! group toggles, revealed and header-row members, hidden markers, and the item stack's
//! focus-target nesting.

use bcode_session_view_models::ToolInvocationViewStatus;
use serde_json::json;

use super::harness::{Fixture, assert_geometry_inside, config, id, tool, user};
use super::*;
use crate::units::UnitKey;

fn grouped(group_disclosure: &str) -> serde_json::Value {
    json!([
        { "match": { "kind": "tool" }, "set": { "group": { "by": "consecutive" } } },
        { "match": { "kind": "group" }, "set": { "disclosure": group_disclosure } }
    ])
}

fn three_reads(
    last: ToolInvocationViewStatus,
) -> Vec<bcode_session_view_models::TranscriptViewItem> {
    vec![
        user("u"),
        tool("a", ToolInvocationViewStatus::Finished),
        tool("b", ToolInvocationViewStatus::Finished),
        tool("c", last),
    ]
}

#[test]
fn x3_collapsed_group_is_one_header_row_with_count_tools_and_status() {
    let fixture = Fixture::new(
        config(grouped("collapsed")),
        three_reads(ToolInvocationViewStatus::Finished),
    );
    let header = UnitKey::GroupHeader(id("a"));
    assert_eq!(
        fixture.keys(),
        vec![
            UnitKey::Boundary(crate::units::BoundaryEdge::Older),
            UnitKey::Item(id("u")),
            header.clone()
        ]
    );
    let rendered = fixture.render(&header, 60);
    assert_eq!(rendered.rows[0].trim_end(), "▸ 3 tool calls · read ×3 · ok");
    assert_eq!(
        rendered.height(),
        2,
        "header plus the group's spacing after"
    );
    let target = DisclosureTarget::Group(id("a"));
    assert_eq!(rendered.focus_row(&target), Some(0));
    assert_eq!(rendered.hit(&target), Some(LogicalRect::new(0, 0, 60, 1)));
    assert_geometry_inside(&rendered, 60);
}

#[test]
fn x3_x1_toggling_a_group_expands_members_after_the_header() {
    let mut fixture = Fixture::new(
        config(grouped("collapsed")),
        three_reads(ToolInvocationViewStatus::Finished),
    );
    fixture.toggle(&DisclosureTarget::Group(id("a")));
    let keys = fixture.keys();
    let header = keys
        .iter()
        .position(|key| key == &UnitKey::GroupHeader(id("a")))
        .expect("header");
    assert_eq!(
        keys[header + 1..],
        [
            UnitKey::Item(id("a")),
            UnitKey::Item(id("b")),
            UnitKey::Item(id("c"))
        ]
    );
    let rendered = fixture.render(&UnitKey::GroupHeader(id("a")), 60);
    assert!(rendered.rows[0].starts_with("▾ 3 tool calls"));
    assert_eq!(rendered.height(), 1, "members carry the spacing");
    // Members render indented by the member indent.
    let member = fixture.render_item("b", 60);
    assert!(
        member.rows[0].starts_with("  ▾ Tool result · read"),
        "{:?}",
        member.rows
    );
}

#[test]
fn x3_summary_groups_show_member_header_rows_that_toggle_each_member() {
    let fixture = Fixture::new(
        config(grouped("summary")),
        three_reads(ToolInvocationViewStatus::Finished),
    );
    for name in ["a", "b", "c"] {
        let member = fixture.render_item(name, 60);
        // Header rows are contiguous; the last one carries the group's trailing spacing.
        let expected = if name == "c" { 2 } else { 1 };
        assert_eq!(member.height(), expected, "{name}: {:?}", member.rows);
        assert!(
            member.rows[0].starts_with("  ▸ Tool result · read"),
            "{:?}",
            member.rows
        );
        let target = DisclosureTarget::Unit(id(name));
        assert_eq!(member.focus_row(&target), Some(0));
        assert_eq!(member.hit(&target).map(|rect| rect.y), Some(0));
        assert_geometry_inside(&member, 60);
    }
}

#[test]
fn a16_failed_members_are_revealed_in_collapsed_groups() {
    let fixture = Fixture::new(
        config(grouped("collapsed")),
        three_reads(ToolInvocationViewStatus::Failed),
    );
    let keys = fixture.keys();
    assert!(keys.contains(&UnitKey::GroupHeader(id("a"))));
    assert!(
        keys.contains(&UnitKey::Item(id("c"))),
        "failed member revealed"
    );
    assert!(!keys.contains(&UnitKey::Item(id("a"))));
    let header = fixture.render(&UnitKey::GroupHeader(id("a")), 60);
    assert!(header.rows[0].contains("failed"), "{:?}", header.rows);
    // The revealed member is the group's last rendered row, so it closes the group: no blank
    // row separates the collapsed header from it (Structure v2 closer parity with V1).
    assert_eq!(header.height(), 1, "{:?}", header.rows);
    let failed = fixture.render_item("c", 60);
    assert!(failed.text().contains("tool failed"), "{:?}", failed.rows);
}

#[test]
fn group_closing_spacing_moves_to_the_header_when_revealed_members_leave() {
    let mut items = three_reads(ToolInvocationViewStatus::Running);
    let mut fixture = Fixture::new(config(grouped("collapsed")), items.clone());
    assert!(fixture.keys().contains(&UnitKey::Item(id("c"))));
    assert_eq!(
        fixture.render(&UnitKey::GroupHeader(id("a")), 60).height(),
        1,
        "the running member follows the header directly"
    );
    items[3] = tool("c", ToolInvocationViewStatus::Finished);
    items[3].revision = 2;
    let revised = bcode_session_view_models::TranscriptChanges {
        changes: vec![bcode_session_view_models::TranscriptChange::Revised {
            id: id("c"),
            revision: 2,
        }],
        ..Default::default()
    };
    fixture.items = items;
    let _ = fixture.resolver.update(&revised, &fixture.items);
    let _ = fixture.units.apply(
        &revised,
        &fixture.items,
        crate::window::WindowMode::Live,
        &fixture.resolver,
    );
    assert!(!fixture.keys().contains(&UnitKey::Item(id("c"))));
    assert_eq!(
        fixture.render(&UnitKey::GroupHeader(id("a")), 60).height(),
        2,
        "with nothing revealed the header closes the group"
    );
}

#[test]
fn running_groups_reserve_a_spinner_slot_and_no_text_under_it() {
    let fixture = Fixture::new(
        config(grouped("collapsed")),
        three_reads(ToolInvocationViewStatus::Running),
    );
    let rendered = fixture.render(&UnitKey::GroupHeader(id("a")), 40);
    let slots = &rendered.geometry.paint_slots;
    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0].kind, crate::layout::PaintSlotKind::Spinner);
    let x = usize::try_from(slots[0].rect.x).expect("x");
    assert_eq!(
        rendered.rows[0].chars().nth(x),
        Some(' '),
        "slot cell is blank"
    );
    // Too narrow for a text column beside the slot: the slot is dropped (B4).
    let narrow = fixture.render(&UnitKey::GroupHeader(id("a")), 4);
    assert_eq!(narrow.geometry.paint_slots, Vec::new());
}

#[test]
fn b8_group_headers_stay_one_bounded_row_at_tiny_widths() {
    let fixture = Fixture::new(
        config(grouped("collapsed")),
        three_reads(ToolInvocationViewStatus::Finished),
    );
    for width in [0u16, 1, 2, 3, 5, 8, 13] {
        let rendered = fixture.render(&UnitKey::GroupHeader(id("a")), width);
        assert_eq!(rendered.height(), 2, "width {width}");
        assert_geometry_inside(&rendered, width);
    }
}

#[test]
fn a16_hidden_items_leave_one_marker_row() {
    let rules = json!([{ "match": { "kind": "tool" }, "set": { "disclosure": "hidden" } }]);
    let fixture = Fixture::new(
        config(rules),
        vec![
            user("u"),
            tool("a", ToolInvocationViewStatus::Finished),
            tool("b", ToolInvocationViewStatus::Finished),
        ],
    );
    let marker = UnitKey::HiddenMarker(id("a"));
    assert!(fixture.keys().contains(&marker));
    let rendered = fixture.render(&marker, 30);
    assert_eq!(
        rendered.rows,
        vec!["▸ 2 hidden".to_owned() + &" ".repeat(20)]
    );
}

#[test]
fn group_text_helpers_count_and_elide() {
    let tools = vec![("read".to_owned(), 3), ("grep".to_owned(), 1)];
    assert_eq!(tools_text(&tools), "read ×3, grep");
    let many: Vec<(String, u64)> = (0..=MAX_HEADER_TOOLS)
        .map(|index| (format!("t{index}"), 1))
        .collect();
    assert!(tools_text(&many).ends_with(", …"));
    assert_eq!(count_text(1), "1 tool call");
    assert_eq!(effective_indent(8, 6), 3);
    assert_eq!(effective_indent(2, 80), 2);
}

#[test]
fn nested_stacks_renumber_focus_targets_under_the_first() {
    let row = |target: &str| -> Box<dyn ItemContent> {
        Box::new(DisclosureRow::new(
            common::single_row(target),
            Some(DisclosureTarget::Unit(id(target))),
        ))
    };
    let inner = ItemStack::new().nested().with(row("b")).with(row("c"));
    let stack = ItemStack::new()
        .nested()
        .with(row("a"))
        .with(Box::new(inner))
        .with(row("d"));
    let node = stack.layout(Constraints::for_width(10), &mut LayoutCx::new());
    assert_eq!(node.size.height, 4);
    let targets = stack.geometry(&node).focus_targets;
    let summary: Vec<_> = targets
        .iter()
        .map(|target| (target.local, target.parent, target.rows.start))
        .collect();
    assert_eq!(
        summary,
        vec![
            (0, None, 0),
            (1, Some(0), 1),
            (2, Some(1), 2),
            (3, Some(0), 3)
        ]
    );
}
