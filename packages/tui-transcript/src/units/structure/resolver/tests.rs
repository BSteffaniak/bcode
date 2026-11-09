//! Structure resolution behavior: policy placements, visibility floor (A16), grouping and
//! group disclosure (U2, X3), suppression, incremental updates (U5, D11) and override
//! reconciliation (D12).

use std::sync::Arc;

use bcode_presentation_policy::models::{
    Disclosure, LayerSource, PresentationRule, RuleLayer, UnitStatus,
};
use bcode_presentation_policy::{CompiledPolicy, PolicyRegistry};
use bcode_session_view_models::{
    ChatMessageView, PermissionView, ToolInvocationView, ToolInvocationViewStatus, ToolTimingView,
    TranscriptChange, TranscriptChanges, TranscriptViewItem, TranscriptViewItemId,
    TranscriptViewItemKind,
};
use serde_json::json;

use super::*;
use crate::units::{UnitKind as DerivedKind, Units};
use crate::window::WindowMode;

fn id(text: &str) -> TranscriptViewItemId {
    TranscriptViewItemId::new(text)
}

fn item(name: &str, kind: TranscriptViewItemKind) -> TranscriptViewItem {
    TranscriptViewItem {
        id: id(name),
        revision: 1,
        sequence: Some(1),
        timestamp_ms: None,
        output_location: None,
        streaming: false,
        kind,
    }
}

fn user(name: &str) -> TranscriptViewItem {
    item(
        name,
        TranscriptViewItemKind::UserMessage {
            message: ChatMessageView::markdown(name),
        },
    )
}

fn reasoning(name: &str) -> TranscriptViewItem {
    item(
        name,
        TranscriptViewItemKind::ReasoningMessage {
            message: ChatMessageView::markdown(name),
        },
    )
}

fn tool(name: &str, status: ToolInvocationViewStatus) -> TranscriptViewItem {
    item(
        name,
        TranscriptViewItemKind::ToolInvocation {
            tool: Box::new(ToolInvocationView {
                tool_call_id: format!("call-{name}"),
                producer_plugin_id: None,
                tool_name: Some("read".to_owned()),
                arguments_json: None,
                working_directory: None,
                request_draft: None,
                status,
                result_text: None,
                is_error: None,
                result: None,
                presentation: None,
                timing: ToolTimingView::default(),
            }),
        },
    )
}

fn permission(name: &str) -> TranscriptViewItem {
    item(
        name,
        TranscriptViewItemKind::Permission {
            permission: PermissionView {
                permission_id: name.to_owned(),
                session_id: None,
                tool_call_id: format!("call-{name}"),
                tool_name: "bash".to_owned(),
                producer_plugin_id: None,
                arguments_json: "{}".to_owned(),
                batch: None,
                agent_id: String::new(),
                title: None,
                policy_source: None,
                detail: None,
                resolved: false,
                approved: None,
                can_remember: false,
            },
        },
    )
}

fn config(rules: serde_json::Value) -> StructureConfig {
    let rules: Vec<PresentationRule> = serde_json::from_value(rules).expect("rules");
    let (policy, diagnostics) = CompiledPolicy::compile(
        &[RuleLayer {
            source: LayerSource::Config,
            rules,
        }],
        &PolicyRegistry::with_host_parts(),
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    StructureConfig {
        policy: Arc::new(policy),
        ..StructureConfig::default()
    }
}

fn resolve(config: StructureConfig, items: &[TranscriptViewItem]) -> StructureResolver {
    let mut resolver = StructureResolver::new(config);
    let _ = resolver.update(&TranscriptChanges::default(), items);
    resolver
}

fn grouping_rules() -> serde_json::Value {
    json!([
        { "match": { "kind": "tool" }, "set": { "group": { "by": "consecutive" } } },
        { "match": { "kind": "group" }, "set": { "disclosure": "collapsed" } }
    ])
}

#[test]
fn default_policy_places_every_item_visibly_with_host_defaults() {
    let items = vec![user("u"), tool("t", ToolInvocationViewStatus::Finished)];
    let resolver = resolve(StructureConfig::default(), &items);
    for item in &items {
        assert_eq!(resolver.placement_of(&item.id), ItemPlacement::Visible);
        let ResolvedUnitPresentation::Item(structure) =
            resolver.unit_presentation(&UnitKey::Item(item.id.clone()))
        else {
            panic!("item structure");
        };
        assert_eq!(structure.presentation.disclosure, Disclosure::Expanded);
        assert_eq!(structure.rendering, None);
    }
}

#[test]
fn a16_hidden_units_leave_markers_and_suppression_leaves_nothing() {
    let rules = json!([{ "match": { "kind": "reasoning" }, "set": { "disclosure": "hidden" } }]);
    let items = vec![user("u"), reasoning("r")];
    let resolver = resolve(config(rules.clone()), &items);
    assert_eq!(resolver.placement_of(&id("r")), ItemPlacement::Hidden);

    let suppressed = StructureConfig {
        reasoning_suppressed: true,
        ..config(rules)
    };
    let resolver = resolve(suppressed, &items);
    assert_eq!(resolver.placement_of(&id("r")), ItemPlacement::Suppressed);
    let mut units = Units::new();
    let _ = units.apply(
        &TranscriptChanges::default(),
        &items,
        WindowMode::Live,
        &resolver,
    );
    assert!(units.owner_unit(&id("r")).is_none(), "no unit, no marker");
}

#[test]
fn a16_visibility_floor_applies_after_rules_and_manual_toggles() {
    let rules = json!([
        { "match": { "kind": "tool" }, "set": { "disclosure": "hidden" } },
        { "match": { "kind": "permission" }, "set": { "disclosure": "collapsed" } }
    ]);
    let items = vec![
        tool("ok", ToolInvocationViewStatus::Finished),
        tool("bad", ToolInvocationViewStatus::Failed),
        permission("p"),
    ];
    let mut resolver = resolve(config(rules), &items);
    assert_eq!(resolver.placement_of(&id("ok")), ItemPlacement::Hidden);
    assert_eq!(resolver.placement_of(&id("bad")), ItemPlacement::Visible);
    assert_eq!(
        resolver.effective_disclosure(&DisclosureTarget::Unit(id("p"))),
        Some(Disclosure::Expanded),
        "awaiting a decision stays expanded"
    );
    assert!(
        !resolver.set_disclosure(&DisclosureTarget::Unit(id("p")), Disclosure::Collapsed),
        "a pending decision never collapses"
    );
    assert_eq!(resolver.toggle(&DisclosureTarget::Unit(id("p"))), None);
    assert!(resolver.set_disclosure(&DisclosureTarget::Unit(id("bad")), Disclosure::Hidden));
    let _ = resolver.update(&TranscriptChanges::default(), &items);
    assert_ne!(
        resolver.effective_disclosure(&DisclosureTarget::Unit(id("bad"))),
        Some(Disclosure::Hidden),
        "failed units never drop below summary, even after a manual toggle"
    );
}

#[test]
fn x3_u2_collapsed_groups_fold_members_into_one_header_and_reveal_running_members() {
    let items = vec![
        user("u"),
        tool("a", ToolInvocationViewStatus::Finished),
        tool("b", ToolInvocationViewStatus::Finished),
        tool("c", ToolInvocationViewStatus::Running),
    ];
    let resolver = resolve(config(grouping_rules()), &items);
    let renderings = ["a", "b", "c"].map(|name| match resolver.placement_of(&id(name)) {
        ItemPlacement::Grouped {
            rendering, member, ..
        } => (rendering, member),
        other => panic!("{name} not grouped: {other:?}"),
    });
    assert_eq!(
        renderings,
        [
            (MemberRendering::Nothing, true),
            (MemberRendering::Nothing, true),
            (MemberRendering::Full, true),
        ],
        "running members are revealed in collapsed groups"
    );

    let mut units = Units::new();
    let _ = units.apply(
        &TranscriptChanges::default(),
        &items,
        WindowMode::Live,
        &resolver,
    );
    let header = UnitKey::GroupHeader(id("a"));
    assert_eq!(
        units.entry(&header).map(|entry| entry.kind),
        Some(DerivedKind::GroupHeader {
            members: 3,
            expanded: false
        })
    );
    assert_eq!(units.owner_unit(&id("a")), Some(&header));
    assert_eq!(units.owner_unit(&id("c")), Some(&UnitKey::Item(id("c"))));
    let ResolvedUnitPresentation::Group(group) = resolver.unit_presentation(&header) else {
        panic!("group structure");
    };
    assert_eq!(group.tools, vec![("read".to_owned(), 3)]);
    assert_eq!(group.target, DisclosureTarget::Group(id("a")));
}

#[test]
fn u2_expanding_a_group_inserts_members_and_collapsing_maps_them_to_the_header() {
    let items = vec![
        user("u"),
        tool("a", ToolInvocationViewStatus::Finished),
        tool("b", ToolInvocationViewStatus::Finished),
    ];
    let mut resolver = resolve(config(grouping_rules()), &items);
    let mut units = Units::new();
    let _ = units.apply(
        &TranscriptChanges::default(),
        &items,
        WindowMode::Live,
        &resolver,
    );
    let group = DisclosureTarget::Group(id("a"));

    assert_eq!(resolver.toggle(&group), Some(Disclosure::Expanded));
    let update = resolver.update(&TranscriptChanges::default(), &items);
    assert!(update.full);
    let changes = units.apply(
        &update.augment(&TranscriptChanges::default(), &|_| Some(1)),
        &items,
        WindowMode::Live,
        &resolver,
    );
    let inserted = changes.inserted().cloned().collect::<Vec<_>>();
    assert_eq!(
        inserted,
        vec![UnitKey::Item(id("a")), UnitKey::Item(id("b"))]
    );

    assert_eq!(resolver.toggle(&group), Some(Disclosure::Collapsed));
    let update = resolver.update(&TranscriptChanges::default(), &items);
    let changes = units.apply(
        &update.augment(&TranscriptChanges::default(), &|_| Some(1)),
        &items,
        WindowMode::Live,
        &resolver,
    );
    let header = UnitKey::GroupHeader(id("a"));
    assert_eq!(
        changes.correspondence.get(&UnitKey::Item(id("b"))),
        Some(&crate::navigation::Correspondence::Collapsed(header))
    );
}

#[test]
fn d11_streaming_revisions_with_unchanged_facts_resolve_nothing_else() {
    let mut items = vec![user("u"), tool("t", ToolInvocationViewStatus::Running)];
    let mut resolver = resolve(config(grouping_rules()), &items);
    items[1].revision = 2;
    let revised = TranscriptChanges {
        changes: vec![TranscriptChange::Revised {
            id: id("t"),
            revision: 2,
        }],
        ..TranscriptChanges::default()
    };
    assert!(resolver.update(&revised, &items).is_empty());

    items[1] = tool("t", ToolInvocationViewStatus::Finished);
    items[1].revision = 3;
    let finished = TranscriptChanges {
        changes: vec![TranscriptChange::Revised {
            id: id("t"),
            revision: 3,
        }],
        ..TranscriptChanges::default()
    };
    let update = resolver.update(&finished, &items);
    let ResolvedUnitPresentation::Item(structure) =
        resolver.unit_presentation(&UnitKey::Item(id("t")))
    else {
        panic!("item structure");
    };
    assert_eq!(structure.facts.status, Some(UnitStatus::Succeeded));
    assert!(update.changed.is_empty() || update.changed.contains(&id("t")));
    assert!(!update.changed.contains(&id("u")));
}

#[test]
fn d12_overrides_are_forgotten_when_their_items_leave_the_window() {
    let items = vec![user("u"), tool("t", ToolInvocationViewStatus::Finished)];
    let mut resolver = resolve(StructureConfig::default(), &items);
    assert!(resolver.set_disclosure(&DisclosureTarget::Unit(id("t")), Disclosure::Collapsed));
    let _ = resolver.update(&TranscriptChanges::default(), &items);
    assert_eq!(resolver.overrides().len(), 1);
    let remaining = vec![items[0].clone()];
    let removed = TranscriptChanges {
        changes: vec![TranscriptChange::Removed { ids: vec![id("t")] }],
        ..TranscriptChanges::default()
    };
    let _ = resolver.update(&removed, &remaining);
    assert!(resolver.overrides().is_empty());
}

#[test]
fn config_change_re_resolves_every_item() {
    let items = vec![user("u"), reasoning("r")];
    let mut resolver = resolve(StructureConfig::default(), &items);
    resolver.set_config(config(
        json!([{ "match": { "kind": "reasoning" }, "set": { "disclosure": "hidden" } }]),
    ));
    let update = resolver.update(&TranscriptChanges::default(), &items);
    assert!(update.full);
    assert!(update.changed.contains(&id("r")));
    assert_eq!(resolver.placement_of(&id("r")), ItemPlacement::Hidden);
}
