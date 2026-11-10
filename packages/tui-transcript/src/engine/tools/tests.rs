//! Transcript tooling through the engine façade: bulk disclosure, detail cycling, inspection,
//! per-tool defaults, inline shell expansion and inline interaction reservations.

use web_time::Instant;

use bcode_presentation_policy::models::{Disclosure, LayerSource, PresentationRule, UnitKind};
use bcode_session_models::{
    ClientId, SessionEvent, SessionEventKind, SessionId, TurnAdmissionMetadata,
};
use bcode_session_view_models::{TranscriptViewItemId, TranscriptViewItemKind};

use crate::engine::{
    DefaultRuleRefusal, DetailCycle, HostRequest, InlineInteractionPlacement, TranscriptEngine,
};
use crate::focus::FocusTargetId;
use crate::frame::FrameClock;
use crate::navigation::MotionConfig;
use crate::units::UnitKey;
use crate::units::structure::{
    DisclosureTarget, InlineInteraction, MAX_INLINE_INTERACTION_ROWS, ResolvedUnitPresentation,
    StructureConfig,
};
use crate::viewport::ViewportGeometry;
use crate::window::HistoryWindowResult;

const WIDTH: u64 = 60;
const HEIGHT: u64 = 40;

fn event(sequence: u64, kind: SessionEventKind) -> SessionEvent {
    SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence,
        timestamp_ms: sequence,
        session_id: SessionId::new(),
        provenance: None,
        kind,
    }
}

fn user(sequence: u64) -> SessionEvent {
    event(
        sequence,
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: format!("u{sequence}"),
            admission: TurnAdmissionMetadata::default(),
        },
    )
}

fn tool(sequence: u64, name: &str, arguments: &str) -> SessionEvent {
    event(
        sequence,
        SessionEventKind::ToolCallRequested {
            tool_call_id: format!("call-{sequence}"),
            producer_plugin_id: None,
            tool_name: name.to_owned(),
            arguments_json: arguments.to_owned(),
            working_directory: None,
        },
    )
}

fn exchange(sequence: u64, invocation: &str) -> SessionEvent {
    event(
        sequence,
        SessionEventKind::ToolExchangeRequested {
            request: bcode_session_models::ToolExchangeRequest {
                invocation_id: invocation.to_owned(),
                exchange_id: format!("exchange-{sequence}"),
                producer_id: "test.producer".to_owned(),
                schema: "test.question".to_owned(),
                schema_version: 1,
                payload: serde_json::json!({ "question": "continue?" }),
                response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
            },
        },
    )
}

fn config(rules: serde_json::Value) -> StructureConfig {
    let rules: Vec<PresentationRule> = serde_json::from_value(rules).expect("rules");
    let (policy, _) = bcode_presentation_policy::CompiledPolicy::compile(
        &[bcode_presentation_policy::models::RuleLayer {
            source: LayerSource::Config,
            rules,
        }],
        &bcode_presentation_policy::PolicyRegistry::with_host_parts(),
    );
    StructureConfig {
        policy: std::sync::Arc::new(policy),
        ..StructureConfig::default()
    }
}

fn attach(events: &[SessionEvent], structure: StructureConfig, at: Instant) -> TranscriptEngine {
    let mut engine =
        TranscriptEngine::new(ViewportGeometry::new(WIDTH, HEIGHT), MotionConfig::INSTANT)
            .with_structure_config(structure);
    let update = engine.attach(at);
    let HostRequest::Window(issued) = update.requests[0] else {
        panic!("attach requests a window");
    };
    let result = HistoryWindowResult {
        generation: issued.generation,
        target: issued.request,
        events: events.to_vec(),
        has_older: false,
        has_newer: false,
        first_available_sequence: events.first().map(|event| event.sequence),
        last_available_sequence: events.last().map(|event| event.sequence),
    };
    let _ = engine.apply_window(&result, at).expect("attach");
    engine
}

/// A user message followed by three `read` tools, collapsed by configuration.
fn tools_engine(at: Instant) -> TranscriptEngine {
    let events = [
        user(1),
        tool(2, "read", r#"{"path":"a.rs"}"#),
        tool(3, "read", r#"{"path":"b.rs"}"#),
        tool(4, "read", r#"{"path":"c.rs"}"#),
    ];
    attach(
        &events,
        config(serde_json::json!([
            { "match": { "kind": "tool" }, "set": { "disclosure": "collapsed" } }
        ])),
        at,
    )
}

fn tool_ids(engine: &TranscriptEngine) -> Vec<TranscriptViewItemId> {
    engine
        .session_view()
        .snapshot()
        .transcript
        .items
        .iter()
        .filter(|item| {
            engine
                .structure()
                .item_facts(&item.id)
                .is_some_and(|facts| facts.kind == Some(UnitKind::Tool))
        })
        .map(|item| item.id.clone())
        .collect()
}

fn disclosure_of(engine: &TranscriptEngine, id: &TranscriptViewItemId) -> Option<Disclosure> {
    engine
        .structure()
        .effective_disclosure(&DisclosureTarget::Unit(id.clone()))
}

/// Focus the unit-level target of `id`'s unit.
fn focus_unit(engine: &mut TranscriptEngine, id: &TranscriptViewItemId, at: Instant) {
    let unit = UnitKey::Item(id.clone());
    let local = engine
        .focus_view()
        .targets(&unit)
        .first()
        .expect("unit focus target")
        .local;
    let _ = engine.focus_target(FocusTargetId { unit, local }, at);
    assert_eq!(
        engine.focus_state().disclosure(),
        Some(&DisclosureTarget::Unit(id.clone()))
    );
}

#[test]
fn bulk_expand_collapse_and_reset_apply_to_every_resident_unit_without_focus() {
    let at = Instant::now();
    let mut engine = tools_engine(at);
    let ids = tool_ids(&engine);
    assert_eq!(ids.len(), 3);
    assert!(
        ids.iter()
            .all(|id| disclosure_of(&engine, id) == Some(Disclosure::Collapsed))
    );
    let heights = |engine: &TranscriptEngine| engine.viewport().index().total_height();
    let collapsed = heights(&engine);

    let (update, changed) = engine.set_all_disclosure(Disclosure::Expanded, at);
    assert!(changed && update.invalidated);
    assert!(
        ids.iter()
            .all(|id| disclosure_of(&engine, id) == Some(Disclosure::Expanded))
    );
    assert!(
        heights(&engine) > collapsed,
        "expanded units are re-measured"
    );
    let (_, again) = engine.set_all_disclosure(Disclosure::Expanded, at);
    assert!(!again, "nothing left to expand");

    let (_, changed) = engine.reset_disclosure(at);
    assert!(changed);
    assert!(
        ids.iter()
            .all(|id| disclosure_of(&engine, id) == Some(Disclosure::Collapsed))
    );
    assert_eq!(heights(&engine), collapsed);
    let (_, again) = engine.reset_disclosure(at);
    assert!(!again, "no overrides left");
}

#[test]
fn bulk_actions_with_transcript_focus_apply_to_the_focused_subtree_only() {
    let at = Instant::now();
    let mut engine = tools_engine(at);
    let ids = tool_ids(&engine);
    focus_unit(&mut engine, &ids[1], at);

    let (_, changed) = engine.set_all_disclosure(Disclosure::Expanded, at);
    assert!(changed);
    assert_eq!(disclosure_of(&engine, &ids[1]), Some(Disclosure::Expanded));
    assert_eq!(disclosure_of(&engine, &ids[0]), Some(Disclosure::Collapsed));
    assert_eq!(disclosure_of(&engine, &ids[2]), Some(Disclosure::Collapsed));

    // An override outside the subtree survives a focused reset.
    let _ = engine.set_disclosure(
        &DisclosureTarget::Unit(ids[0].clone()),
        Disclosure::Expanded,
        at,
    );
    let (_, reset) = engine.reset_disclosure(at);
    assert!(reset);
    assert_eq!(disclosure_of(&engine, &ids[1]), Some(Disclosure::Collapsed));
    assert_eq!(disclosure_of(&engine, &ids[0]), Some(Disclosure::Expanded));
}

#[test]
fn detail_cycling_walks_collapsed_summary_expanded_for_every_unit_or_the_focused_one() {
    let at = Instant::now();
    let mut engine = tools_engine(at);
    let ids = tool_ids(&engine);
    // Without focus every unit cycles together, continuing from the last global level.
    let mut seen = Vec::new();
    for _ in 0..3 {
        let (_, cycled) = engine.cycle_detail(at);
        let DetailCycle::Changed(level) = cycled else {
            panic!("cycle changed nothing: {cycled:?}");
        };
        assert!(
            ids.iter()
                .all(|id| disclosure_of(&engine, id) == Some(level))
        );
        seen.push(level);
    }
    assert_eq!(
        seen,
        [
            Disclosure::Collapsed,
            Disclosure::Summary,
            Disclosure::Expanded
        ],
        "the first global cycle starts after the expanded level (the user message collapses)"
    );

    // With focus only the focused unit cycles.
    focus_unit(&mut engine, &ids[0], at);
    let (_, cycled) = engine.cycle_detail(at);
    assert_eq!(cycled, DetailCycle::Changed(Disclosure::Collapsed));
    assert_eq!(disclosure_of(&engine, &ids[0]), Some(Disclosure::Collapsed));
    assert_eq!(disclosure_of(&engine, &ids[1]), Some(Disclosure::Expanded));
}

#[test]
fn fo6_inspection_reports_facts_rule_origins_manual_overrides_and_group_context() {
    let at = Instant::now();
    let mut engine = tools_engine(at);
    let ids = tool_ids(&engine);
    let target = DisclosureTarget::Unit(ids[0].clone());
    let report = engine.inspect(&target).expect("report");
    assert_eq!(report.target, target);
    assert_eq!(report.facts.tool.as_deref(), Some("read"));
    assert_eq!(report.effective, Disclosure::Collapsed);
    assert_eq!(report.manual, None);
    let origin = report
        .explained
        .origins
        .iter()
        .find(|origin| origin.property == "disclosure")
        .expect("disclosure origin");
    assert_eq!(origin.source, LayerSource::Config);

    let _ = engine.toggle_disclosure(&target, at);
    let report = engine.inspect(&target).expect("report");
    assert_eq!(report.manual, Some(Disclosure::Expanded));
    assert_eq!(report.effective, Disclosure::Expanded);
    assert!(
        report
            .explained
            .floor
            .iter()
            .any(|note| note.contains("manual toggle"))
    );
    assert!(engine.inspect_focused().is_none(), "nothing focused");
    focus_unit(&mut engine, &ids[0], at);
    assert_eq!(
        engine.inspect_focused().map(|report| report.target),
        Some(target)
    );
    assert!(
        engine
            .inspect(&DisclosureTarget::Unit(TranscriptViewItemId::new("gone")))
            .is_none()
    );
}

#[test]
fn make_default_builds_a_tool_rule_from_the_focused_units_current_disclosure() {
    let at = Instant::now();
    let mut engine = tools_engine(at);
    assert_eq!(
        engine.focused_default_rule(),
        Err(DefaultRuleRefusal::NotAUnit)
    );
    let ids = tool_ids(&engine);
    focus_unit(&mut engine, &ids[2], at);
    let _ = engine.toggle_disclosure(&DisclosureTarget::Unit(ids[2].clone()), at);
    let rule = engine.focused_default_rule().expect("rule");
    assert_eq!(rule.matcher.kind.0, vec![UnitKind::Tool]);
    assert_eq!(rule.matcher.tool.0, vec!["read".to_owned()]);
    assert_eq!(rule.set.disclosure, Some(Disclosure::Expanded));

    // A non-tool unit refuses.
    let user = engine
        .session_view()
        .snapshot()
        .transcript
        .items
        .iter()
        .find(|item| matches!(item.kind, TranscriptViewItemKind::UserMessage { .. }))
        .map(|item| item.id.clone())
        .expect("user message");
    let unit = UnitKey::Item(user.clone());
    if let Some(target) = engine.focus_view().targets(&unit).first() {
        let local = target.local;
        let _ = engine.focus_target(FocusTargetId { unit, local }, at);
        if engine.focus_state().disclosure() == Some(&DisclosureTarget::Unit(user)) {
            assert_eq!(
                engine.focused_default_rule(),
                Err(DefaultRuleRefusal::NotATool)
            );
        }
    }
}

#[test]
fn c39_focused_shell_screens_toggle_inline_expansion_up_to_128_rows() {
    let at = Instant::now();
    let events = [
        user(1),
        tool(2, "shell", r#"{"command":"seq 300"}"#),
        tool(3, "read", r#"{"path":"a.rs"}"#),
    ];
    let mut engine = attach(
        &events,
        config(serde_json::json!([
            { "match": { "kind": "tool" }, "set": { "disclosure": "collapsed" } }
        ])),
        at,
    );
    let ids = tool_ids(&engine);
    let shell = ids[0].clone();
    assert!(engine.structure().is_shell(&shell));
    assert!(!engine.structure().is_shell(&ids[1]));

    focus_unit(&mut engine, &shell, at);
    let (update, expanded) = engine.toggle_focused_shell(at);
    assert_eq!(expanded, Some(true));
    assert!(update.invalidated);
    let ResolvedUnitPresentation::Item(item) = engine
        .structure()
        .unit_presentation(&UnitKey::Item(shell.clone()))
    else {
        panic!("item structure");
    };
    assert_eq!(item.presentation.disclosure, Disclosure::Expanded);
    let output = item.presentation.section("output");
    assert_eq!(output.disclosure, Disclosure::Expanded);
    assert!(output.preview.lines >= crate::layout::producers::tool::SHELL_MAX_ROWS);
    assert!(
        engine
            .inspect(&DisclosureTarget::Unit(shell.clone()))
            .is_some_and(|report| report.notes.iter().any(|note| note.contains("shell")))
    );

    let (_, collapsed) = engine.toggle_focused_shell(at);
    assert_eq!(collapsed, Some(false));
    assert_eq!(disclosure_of(&engine, &shell), Some(Disclosure::Collapsed));

    // A unit without a shell screen does not toggle.
    focus_unit(&mut engine, &ids[1], at);
    assert_eq!(engine.toggle_focused_shell(at).1, None);
}

#[test]
fn d10_inline_interactions_reserve_bounded_rows_in_their_unit_and_report_placement() {
    let at = Instant::now();
    let events = [
        user(1),
        tool(2, "question", r#"{"question":"continue?"}"#),
        exchange(3, "call-2"),
    ];
    let mut engine = attach(&events, StructureConfig::default(), at);
    let interaction = engine
        .session_view()
        .snapshot()
        .transcript
        .items
        .iter()
        .find_map(|item| match &item.kind {
            TranscriptViewItemKind::Interaction { interaction } => {
                Some((item.id.clone(), interaction.interaction_id.clone()))
            }
            _ => None,
        });
    let Some((item, interaction_id)) = interaction else {
        panic!("the exchange projects an interaction item");
    };
    let unit = UnitKey::Item(item.clone());
    let height = |engine: &TranscriptEngine| {
        engine
            .viewport()
            .index()
            .item_by_key(&unit)
            .map(|measured| measured.height)
    };
    let natural = height(&engine);

    let update =
        engine.set_inline_interaction(Some(InlineInteraction::new(interaction_id.clone(), 7)), at);
    assert!(update.invalidated);
    assert_eq!(height(&engine), Some(7));
    assert_eq!(engine.structure().inline_interaction_item(), Some(&item));
    let snapshot = engine.prepare_frame(FrameClock { now: at });
    let placement = engine
        .inline_interaction_placement(&snapshot)
        .expect("visible reservation");
    assert_eq!(placement.height, 7);
    assert!(placement.top >= 0);
    assert_eq!(
        engine.inline_interaction_placement(&snapshot),
        Some(InlineInteractionPlacement {
            top: placement.top,
            height: 7
        })
    );

    // Unchanged reservations are no-ops; bounds clamp (D10).
    assert!(
        !engine
            .set_inline_interaction(Some(InlineInteraction::new(interaction_id.clone(), 7)), at)
            .invalidated
    );
    assert_eq!(
        InlineInteraction::new(interaction_id.clone(), u32::MAX).rows,
        MAX_INLINE_INTERACTION_ROWS
    );
    assert_eq!(InlineInteraction::new(interaction_id, 0).rows, 1);

    // Dropping the reservation restores the unit's own rows.
    let _ = engine.set_inline_interaction(None, at);
    assert_eq!(height(&engine), natural);
    assert!(engine.structure().inline_interaction_item().is_none());
    let snapshot = engine.prepare_frame(FrameClock { now: at });
    assert!(engine.inline_interaction_placement(&snapshot).is_none());
}
