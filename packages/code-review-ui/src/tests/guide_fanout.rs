//! A guide written by child agents: the fan-out started from the guide panel, children created
//! under the assistant's conversation with its model and effort and bound to their own area,
//! at most `max_parallel` at once, synthesis asked of the conversation once every area is
//! written (reusing the areas' citation ids), stopping every child, resuming after the app is
//! closed and opened again, the cost confirmation, and the verifier's checks.
//!
//! Children and the conversation are scripted the way the fake provider replays a turn: each
//! write runs through the review tools against the store the app reads, and the session's
//! transcript is delivered to the app as running, then finished.

use super::assistant::{Setup, answer, setup_with, snapshot, user};
use crate::testing::backend::TestBackend;
use bcode_code_review::assistant::guide::GuideWriteOutcome;
use bcode_code_review::assistant::{ReviewRef, ReviewTools};
use bcode_code_review::document::ItemKey;
use bcode_code_review::guide::config::{GuideConfig, VerifyMode};
use bcode_code_review::guide::write::GuideOp;
use bcode_code_review_models::{
    GenerationState, GetGuideVersionRequest, GrantMode, GuideAreaStatus, GuideStatus,
    ListGuideAreasRequest, ReviewLineSide,
};
use bcode_session_view_models::SessionViewSnapshot;

const PARENT_MODEL: &str = "sonnet-test";
const PARENT_EFFORT: &str = "high";

fn backend(config: GuideConfig) -> TestBackend {
    TestBackend::default().with_guide_config(config)
}

/// A fan-out of one child at a time, never asking to confirm, without a verifier.
fn serial() -> GuideConfig {
    GuideConfig {
        max_parallel: 1,
        confirm_above_tokens: u64::MAX,
        verify: VerifyMode::Never,
        ..GuideConfig::default()
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

/// The assistant's conversation, running `PARENT_MODEL` at `PARENT_EFFORT`.
fn conversation(setup: &mut Setup) -> String {
    setup
        .backend
        .host()
        .set_model_catalog(Ok(crate::host::ModelCatalog::default()));
    setup.scenario.key("ctrl+k");
    setup.settle();
    let session = setup.send("Hello");
    let host = setup.backend.host();
    let runtime = runtime();
    runtime
        .block_on(crate::host::ReviewHost::set_session_model(
            host.as_ref(),
            session.clone(),
            None,
            PARENT_MODEL.to_owned(),
        ))
        .expect("model");
    runtime
        .block_on(crate::host::ReviewHost::set_session_reasoning(
            host.as_ref(),
            session.clone(),
            Some(PARENT_EFFORT.to_owned()),
        ))
        .expect("effort");
    // The conversation's turn ends, so the review has the keyboard back.
    setup.deliver(&session, snapshot(vec![user("Hello"), answer("Hi.")]));
    session
}

/// Open the guide panel and press `f`.
fn fan_out(setup: &mut Setup) {
    setup.scenario.key("esc");
    setup.settle();
    setup.scenario.key("alt+g");
    setup.settle();
    setup.scenario.key("f");
    setup.settle();
}

fn settle_until(setup: &mut Setup, what: &str, done: impl Fn(&Setup) -> bool) {
    for _ in 0..20 {
        if done(setup) {
            return;
        }
        setup.settle();
    }
    let screen = setup.settle();
    assert!(done(setup), "{what}:\n{}", screen.text());
}

/// The guide on screen's id, version and fan-out progress.
fn progress(setup: &Setup) -> Option<(String, u32, GenerationState)> {
    let guide = setup.app().review()?.guide()?;
    Some((
        guide.guide.guide_id.clone(),
        guide.version.version,
        guide.version.generation.clone()?,
    ))
}

/// The stored fan-out progress of `guide_id`'s current version.
fn stored(setup: &Setup, guide_id: &str) -> (GuideStatus, GenerationState) {
    let record = setup
        .backend
        .call_now(GetGuideVersionRequest {
            repo_path: setup.review.repo_root.clone(),
            guide_id: guide_id.to_owned(),
            version: None,
        })
        .version
        .expect("version");
    (
        record.version.status,
        record.version.generation.expect("fan-out progress"),
    )
}

/// A new-side changed line the review shows, with its text.
fn cited_line(setup: &Setup) -> (String, u32) {
    let review = setup.app().review().expect("review");
    let doc = review.document().expect("document");
    (0..doc.len())
        .filter_map(|index| doc.item(index))
        .find_map(|item| match item.key() {
            ItemKey::Line {
                file,
                line,
                side: ReviewLineSide::New,
            } => Some((file.path.clone(), line)),
            ItemKey::LinePair {
                file,
                new: Some(new),
                ..
            } => Some((file.path.clone(), new)),
            _ => None,
        })
        .expect("a new-side line")
}

/// Run `ops` as `session` through the review tools.
fn write_as(setup: &Setup, session: &str, guide_id: &str, ops: &serde_json::Value) -> bool {
    let services = setup.backend.services();
    let data = bcode_code_review::ReviewData::default();
    let mut tools = ReviewTools::new(&data, services.store());
    let opened = tools
        .open(
            &setup.review.repo_root,
            &ReviewRef::Session(session.to_owned()),
        )
        .expect("bound review");
    let ops: Vec<GuideOp> = serde_json::from_value(ops.clone()).expect("ops");
    matches!(
        tools
            .guide_write(
                &opened,
                Some(session),
                guide_id,
                None,
                None,
                false,
                ops,
                &GuideConfig::default(),
            )
            .expect("write ran"),
        GuideWriteOutcome::Written(_)
    )
}

/// `session`'s turn: running, then finished.
fn run_turn(setup: &mut Setup, session: &str, text: &str) {
    let mut running: SessionViewSnapshot = snapshot(vec![user(text)]);
    running.runtime.active_turn_id = Some("turn-1".to_owned());
    setup.deliver(session, running);
    setup.deliver(session, snapshot(vec![user(text), answer("Done.")]));
}

/// The synthesis message sent to the conversation `parent`.
fn synthesis_text(setup: &Setup, parent: &str) -> String {
    setup
        .backend
        .host()
        .turns()
        .into_iter()
        .rev()
        .find(|(session, request)| session == parent && request.text.contains("Synthesize"))
        .map(|(_, request)| request.text)
        .expect("synthesis was asked")
}

/// The running child writes its area (one cited step) and its turn ends.
fn child_writes_its_area(setup: &mut Setup, guide_id: &str, child: &str, area_id: &str) {
    let (path, line) = cited_line(setup);
    assert!(write_as(
        setup,
        child,
        guide_id,
        &serde_json::json!([{"op": "area_result", "area_id": area_id, "sections": [
            {"id": "a1", "kind": "step", "title": format!("Area {area_id}"), "body": "Changes here [[1]].",
             "citations": [{"path": path, "side": "new", "start_line": line, "end_line": line}]}
        ]}]),
    ));
    run_turn(setup, child, "write your area");
}

/// The child session writing `area_id`, once it runs.
fn child_of(setup: &Setup, area_id: &str) -> Option<String> {
    let (guide_id, _, _) = progress(setup)?;
    let (_, generation) = stored(setup, &guide_id);
    let area = generation.area(area_id)?;
    (area.status == GuideAreaStatus::Running)
        .then(|| area.session_id.clone())
        .flatten()
}

#[test]
#[allow(clippy::too_many_lines)]
fn children_write_only_their_areas_one_at_a_time_and_the_conversation_synthesizes_them() {
    let mut setup = setup_with(backend(serial()), 170, 50, true);
    let parent = conversation(&mut setup);
    fan_out(&mut setup);
    settle_until(&mut setup, "the fan-out starts", |setup| {
        progress(setup).is_some() && !setup.backend.host().child_sessions().is_empty()
    });
    let (guide_id, version, generation) = progress(&setup).expect("progress");
    let areas: Vec<String> = generation
        .areas
        .iter()
        .map(|area| area.area_id.clone())
        .collect();
    assert_ne!(areas.len(), 0);
    let host = setup.backend.host();
    assert_eq!(
        host.child_sessions().len(),
        1,
        "max_parallel 1: one child runs, the rest wait"
    );
    let screen = setup.settle();
    assert!(
        screen.contains(&format!("areas 0/{}", areas.len())),
        "the panel shows the fan-out: {}",
        screen.text()
    );

    for (index, area_id) in areas.iter().enumerate() {
        settle_until(&mut setup, "the area's child runs", |setup| {
            child_of(setup, area_id).is_some()
        });
        let child = child_of(&setup, area_id).expect("child");
        let children = host.child_sessions();
        assert_eq!(children.len(), index + 1, "{children:?}");
        assert_eq!(
            children[index],
            (parent.clone(), child.clone()),
            "a child of the conversation"
        );
        let (_, request) = host
            .turns()
            .into_iter()
            .find(|(session, _)| *session == child)
            .expect("the child's turn");
        assert_eq!(
            request.agent.as_deref(),
            Some(crate::assistant::profile::GUIDE_WRITER_PROFILE_ID)
        );
        assert!(request.text.contains(area_id.as_str()), "{}", request.text);
        assert!(
            host.model_log()
                .contains(&format!("submit {child} {PARENT_MODEL} {PARENT_EFFORT}")),
            "the child runs the conversation's model and effort: {:?}",
            host.model_log()
        );
        let bound = setup.binding(&child).expect("the child is bound");
        assert_eq!(
            bound.guide_grant.map(|grant| grant.mode),
            Some(GrantMode::Area {
                area_id: area_id.clone()
            })
        );
        child_writes_its_area(&mut setup, &guide_id, &child, area_id);
    }

    // Every area row was written by its own child.
    let rows = setup
        .backend
        .call_now(ListGuideAreasRequest {
            repo_path: setup.review.repo_root.clone(),
            guide_id: guide_id.clone(),
            version,
        })
        .areas;
    assert_eq!(rows.len(), areas.len());
    let children = host.child_sessions();
    for row in &rows {
        assert!(
            children
                .iter()
                .any(|(_, child)| row.area.session_id.as_ref() == Some(child)),
            "{row:?}"
        );
    }

    // Synthesis is asked of the conversation, reusing the areas' citation ids.
    settle_until(&mut setup, "synthesis is asked for", |setup| {
        setup
            .backend
            .host()
            .turns()
            .iter()
            .any(|(session, request)| *session == parent && request.text.contains("Synthesize"))
    });
    assert_eq!(stored(&setup, &guide_id).0, GuideStatus::Synthesizing);
    let reused = rows[0]
        .area
        .citations
        .keys()
        .next()
        .cloned()
        .expect("an area citation");
    assert!(write_as(
        &setup,
        &parent,
        &guide_id,
        &serde_json::json!([
            {"op": "put_section", "id": "s1", "body": "Every area [[1]].", "citations": [reused]},
            {"op": "set_status", "status": "complete"}
        ]),
    ));
    let synthesis = synthesis_text(&setup, &parent);
    run_turn(&mut setup, &parent, &synthesis);
    settle_until(&mut setup, "the fan-out ends", |setup| {
        setup
            .app()
            .review()
            .is_some_and(|review| !review.fan_out_running())
    });
    let record = setup
        .backend
        .call_now(GetGuideVersionRequest {
            repo_path: setup.review.repo_root.clone(),
            guide_id,
            version: None,
        })
        .version
        .expect("version")
        .version;
    assert_eq!(record.status, GuideStatus::Complete);
    assert_eq!(
        record.citations.get(&reused).map(|citation| citation.lines),
        rows[0]
            .area
            .citations
            .get(&reused)
            .map(|citation| citation.lines),
        "the synthesized guide reuses the area's checked citation"
    );
}

#[test]
fn stopping_stops_every_child_and_reopening_offers_resume_that_writes_the_missing_areas() {
    let config = GuideConfig {
        max_parallel: 16,
        ..serial()
    };
    let mut setup = setup_with(backend(config), 170, 50, true);
    let parent = conversation(&mut setup);
    fan_out(&mut setup);
    settle_until(&mut setup, "children run", |setup| {
        progress(setup).is_some_and(|(_, _, generation)| {
            generation
                .areas
                .iter()
                .all(|area| area.status == GuideAreaStatus::Running)
        })
    });
    let (guide_id, _, generation) = progress(&setup).expect("progress");
    let children: Vec<String> = generation
        .areas
        .iter()
        .filter_map(|area| area.session_id.clone())
        .collect();
    assert_eq!(children.len(), generation.areas.len());

    // The first area is written before the stop.
    let first = generation.areas[0].area_id.clone();
    child_writes_its_area(&mut setup, &guide_id, &children[0], &first);
    settle_until(&mut setup, "the first area is ready", |setup| {
        stored(setup, &guide_id).1.areas[0].status == GuideAreaStatus::Complete
    });

    setup.scenario.key("shift+x");
    settle_until(&mut setup, "the version is stopped", |setup| {
        stored(setup, &guide_id).0 == GuideStatus::Cancelled
    });
    let cancelled = setup.backend.host().cancelled_turns();
    for session in children.iter().skip(1).chain(std::iter::once(&parent)) {
        assert!(
            cancelled.contains(session),
            "{session} stopped: {cancelled:?}"
        );
    }
    let (_, after) = stored(&setup, &guide_id);
    assert_eq!(
        after.areas[0].status,
        GuideAreaStatus::Complete,
        "written areas stay"
    );

    // Close the app and open it again over the same state: the guide offers to resume.
    setup.reopen(170, 50);
    let mut reopened = setup;
    let parent = conversation(&mut reopened);
    reopened.scenario.key("esc");
    reopened.settle();
    reopened.scenario.key("alt+g");
    settle_until(&mut reopened, "the guide is read", |setup| {
        progress(setup).is_some()
    });
    let screen = reopened.settle();
    let total = after.areas.len();
    assert!(
        screen.contains(&format!("1/{total} area")) && screen.contains("Resume"),
        "{}",
        screen.text()
    );
    let before = reopened.backend.host().child_sessions().len();
    reopened.scenario.key("f");
    settle_until(&mut reopened, "missing areas get new children", |setup| {
        setup.backend.host().child_sessions().len() == before + total - 1
    });
    for area in stored(&reopened, &guide_id).1.areas.iter().skip(1) {
        let child = area.session_id.clone().expect("a new child");
        assert!(
            reopened
                .backend
                .host()
                .child_sessions()
                .contains(&(parent.clone(), child.clone()))
        );
        child_writes_its_area(&mut reopened, &guide_id, &child, &area.area_id);
    }
    settle_until(&mut reopened, "resuming reaches synthesis", |setup| {
        setup
            .backend
            .host()
            .turns()
            .iter()
            .any(|(session, request)| *session == parent && request.text.contains("Synthesize"))
    });
}

#[test]
fn a_costly_fan_out_waits_for_confirmation_and_esc_drops_it() {
    let config = GuideConfig {
        confirm_above_tokens: 0,
        ..serial()
    };
    let mut setup = setup_with(backend(config), 170, 50, true);
    conversation(&mut setup);
    fan_out(&mut setup);
    let screen = setup.settle();
    assert!(screen.contains("Fan out:"), "{}", screen.text());
    assert!(screen.contains("on sonnet-test"), "{}", screen.text());
    assert_eq!(setup.backend.host().child_sessions().len(), 0);
    setup.scenario.key("esc");
    let screen = setup.settle();
    assert!(!screen.contains("Fan out:"), "{}", screen.text());

    setup.scenario.key("f");
    setup.settle();
    setup.scenario.key("enter");
    settle_until(&mut setup, "the confirmed fan-out starts", |setup| {
        !setup.backend.host().child_sessions().is_empty()
    });
}

#[test]
fn the_verifier_records_checks_that_render_beside_the_content_and_unsupported_ones_are_refreshed() {
    let config = GuideConfig {
        max_parallel: 16,
        verify: VerifyMode::Always,
        ..serial()
    };
    let mut setup = setup_with(backend(config), 170, 50, true);
    let parent = conversation(&mut setup);
    fan_out(&mut setup);
    settle_until(&mut setup, "children run", |setup| {
        progress(setup).is_some_and(|(_, _, generation)| {
            generation
                .areas
                .iter()
                .all(|area| area.status == GuideAreaStatus::Running)
        })
    });
    let (guide_id, _, generation) = progress(&setup).expect("progress");
    for area in &generation.areas {
        let child = area.session_id.clone().expect("child");
        child_writes_its_area(&mut setup, &guide_id, &child, &area.area_id);
    }
    settle_until(&mut setup, "synthesis is asked for", |setup| {
        setup
            .backend
            .host()
            .turns()
            .iter()
            .any(|(session, request)| *session == parent && request.text.contains("Synthesize"))
    });
    assert!(write_as(
        &setup,
        &parent,
        &guide_id,
        &serde_json::json!([
            {"op": "put_section", "id": "s1", "body": "Retries back off before giving up."},
            {"op": "set_status", "status": "complete"}
        ]),
    ));
    let synthesis = synthesis_text(&setup, &parent);
    run_turn(&mut setup, &parent, &synthesis);
    settle_until(&mut setup, "the verifier runs", |setup| {
        setup.backend.host().turns().iter().any(|(_, request)| {
            request.agent.as_deref() == Some(crate::assistant::profile::GUIDE_VERIFIER_PROFILE_ID)
        })
    });
    let (verifier, _) = setup
        .backend
        .host()
        .turns()
        .into_iter()
        .find(|(_, request)| {
            request.agent.as_deref() == Some(crate::assistant::profile::GUIDE_VERIFIER_PROFILE_ID)
        })
        .expect("verifier turn");
    assert_eq!(
        setup
            .binding(&verifier)
            .and_then(|binding| binding.guide_grant)
            .map(|grant| grant.mode),
        Some(GrantMode::Verify)
    );
    assert!(
        !write_as(
            &setup,
            &verifier,
            &guide_id,
            &serde_json::json!([{"op": "put_section", "id": "s1", "body": "Rewritten."}]),
        ),
        "the verifier cannot change content"
    );
    assert!(write_as(
        &setup,
        &verifier,
        &guide_id,
        &serde_json::json!([{"op": "record_checks", "section": "s1", "checks": [
            {"claim": "Retries back off", "status": "unsupported", "note": "no backoff cited"}
        ]}]),
    ));
    run_turn(&mut setup, &verifier, "verify");
    settle_until(&mut setup, "unsupported claims are handed back", |setup| {
        setup
            .backend
            .host()
            .turns()
            .iter()
            .any(|(session, request)| *session == parent && request.text.contains("verifier found"))
    });
    // The check shows in the section, and its content stays.
    setup.scenario.key("alt+g");
    let screen = setup.settle();
    assert!(
        screen.contains("Retries back off before giving up."),
        "{}",
        screen.text()
    );
    assert!(screen.contains("unsupported"), "{}", screen.text());
}

/// Run `ops` on `guide_id` as `session` in the review `scenario` shows.
fn write_in(
    backend: &TestBackend,
    repo: &std::path::Path,
    session: &str,
    guide_id: &str,
    ops: &serde_json::Value,
) -> bool {
    let services = backend.services();
    let data = bcode_code_review::ReviewData::default();
    let mut tools = ReviewTools::new(&data, services.store());
    let opened = tools
        .open(repo, &ReviewRef::Session(session.to_owned()))
        .expect("bound review");
    let ops: Vec<GuideOp> = serde_json::from_value(ops.clone()).expect("ops");
    matches!(
        tools
            .guide_write(
                &opened,
                Some(session),
                guide_id,
                None,
                None,
                false,
                ops,
                &GuideConfig::default()
            )
            .expect("write ran"),
        GuideWriteOutcome::Written(_)
    )
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_stack_of_three_layers_gets_one_layer_guide_each_linked_from_the_stack_guide() {
    use super::app::{create, fixture, settled};
    use super::publish::{layer, resume};
    use bcode_code_review_models::{GuideScope, ListReviewGuidesRequest, SectionKind};
    let fixture = fixture();
    let provider = crate::testing::fake_provider::FakeProvider::new(fixture.root());
    let backend = std::sync::Arc::new(
        backend(GuideConfig {
            max_parallel: 16,
            ..serial()
        })
        .with_provider(provider),
    );
    let workspace = create(
        &backend,
        &fixture,
        "Stack",
        vec![layer(101), layer(102), layer(103)],
    );
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    let host = backend.host();
    host.set_model_catalog(Ok(crate::host::ModelCatalog::default()));
    scenario.key("ctrl+k");
    settled(&mut scenario);
    scenario.paste("Hello");
    scenario.key("enter");
    settled(&mut scenario);
    let (parent, _) = host.turns().last().cloned().expect("a turn");
    let deliver = |scenario: &mut crate::testing::harness::Scenario<crate::app::ReviewApp>,
                   session: &str,
                   snapshot: SessionViewSnapshot| {
        host.session_view(session)
            .expect("followed")
            .try_send(
                bcode_session_view::follow::SessionViewFollowUpdate::Snapshot(Box::new(snapshot)),
            )
            .expect("delivered");
        settled(scenario);
    };
    deliver(
        &mut scenario,
        &parent,
        snapshot(vec![user("Hello"), answer("Hi.")]),
    );
    scenario.key("esc");
    scenario.key("alt+g");
    settled(&mut scenario);
    scenario.key("f");
    let started = |scenario: &crate::testing::harness::Scenario<crate::app::ReviewApp>| {
        scenario
            .surface()
            .review()
            .and_then(|review| review.guide())
            .and_then(|guide| {
                matches!(guide.guide.scope, GuideScope::Stack { .. })
                    .then(|| guide.version.generation.clone())
                    .flatten()
                    .filter(|generation| {
                        generation
                            .areas
                            .iter()
                            .all(|area| area.status == GuideAreaStatus::Running)
                    })
                    .map(|generation| (guide.guide.guide_id.clone(), generation))
            })
    };
    for _ in 0..20 {
        if started(&scenario).is_some() {
            break;
        }
        settled(&mut scenario);
    }
    let (stack_id, generation) = started(&scenario).expect("a stack guide fans out per layer");
    assert!(generation.areas.len() >= 3, "{generation:?}");
    let repo = fixture.root().to_path_buf();
    for area in &generation.areas {
        let child = area.session_id.clone().expect("child");
        let layer_guide = area.guide_id.clone().expect("the layer's own guide");
        assert_eq!(
            setup_grant(&backend, &repo, &child),
            Some((layer_guide.clone(), GrantMode::Generate))
        );
        assert!(write_in(
            &backend,
            &repo,
            &child,
            &layer_guide,
            &serde_json::json!([
                {"op": "put_section", "id": "s1", "kind": "purpose", "title": "Purpose", "body": format!("What {} adds.", area.title)},
                {"op": "set_status", "status": "complete"}
            ]),
        ));
        let mut running = snapshot(vec![user("write")]);
        running.runtime.active_turn_id = Some("turn-1".to_owned());
        deliver(&mut scenario, &child, running);
        deliver(
            &mut scenario,
            &child,
            snapshot(vec![user("write"), answer("Done.")]),
        );
    }
    for _ in 0..20 {
        if host
            .turns()
            .iter()
            .any(|(session, request)| *session == parent && request.text.contains("layer guides"))
        {
            break;
        }
        settled(&mut scenario);
    }
    assert!(
        host.turns()
            .iter()
            .any(|(session, request)| *session == parent && request.text.contains("layer guides")),
        "the conversation is asked to write the stack guide"
    );
    let stack = backend
        .call_now(GetGuideVersionRequest {
            repo_path: repo,
            guide_id: stack_id.clone(),
            version: None,
        })
        .version
        .expect("stack version")
        .version;
    let links: Vec<String> = stack
        .section("layers")
        .expect("the Layers section")
        .children
        .iter()
        .filter_map(|section| match &section.kind {
            SectionKind::LayerLink { guide_id, .. } => Some(guide_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        links,
        generation
            .areas
            .iter()
            .filter_map(|area| area.guide_id.clone())
            .collect::<Vec<_>>()
    );
    let scope = crate::notes::NoteScope::for_workspace(&workspace);
    let guides = backend
        .call_now(ListReviewGuidesRequest {
            repo_path: scope.repo_path,
            target: scope.target,
            scope: scope.scope,
            include_archived: false,
        })
        .guides;
    for link in &links {
        let layer = guides
            .iter()
            .find(|record| record.guide.guide_id == *link)
            .expect("the layer guide");
        assert_eq!(
            layer.guide.parent_guide_id.as_deref(),
            Some(stack_id.as_str())
        );
    }

    // Stepping to a layer shows that layer's guide (`guide.follow_layer`).
    let shown = |scenario: &crate::testing::harness::Scenario<crate::app::ReviewApp>| {
        scenario
            .surface()
            .review()
            .and_then(|review| review.guide())
            .map(|guide| guide.guide.guide_id.clone())
    };
    scenario.key("esc");
    settled(&mut scenario);
    scenario.key(".");
    for _ in 0..10 {
        if shown(&scenario).is_some_and(|id| links.contains(&id)) {
            break;
        }
        settled(&mut scenario);
    }
    assert!(
        shown(&scenario).is_some_and(|id| links.contains(&id)),
        "{:?}: {}",
        shown(&scenario),
        settled(&mut scenario).text()
    );
}

/// The guide grant `session`'s binding records: its guide and mode.
fn setup_grant(
    backend: &TestBackend,
    repo: &std::path::Path,
    session: &str,
) -> Option<(String, GrantMode)> {
    backend
        .call_now(bcode_code_review_models::GetReviewAssistantRequest {
            repo_path: repo.to_path_buf(),
            lookup: bcode_code_review_models::ReviewAssistantLookup::Session {
                session_id: session.to_owned(),
            },
        })
        .binding?
        .guide_grant
        .map(|grant| (grant.guide_id, grant.mode))
}

#[test]
fn synthesis_waits_for_its_own_turn_while_the_reviewer_chats() {
    let mut setup = setup_with(backend(serial()), 170, 50, true);
    let parent = conversation(&mut setup);
    fan_out(&mut setup);
    settle_until(&mut setup, "the fan-out starts", |setup| {
        progress(setup).is_some() && !setup.backend.host().child_sessions().is_empty()
    });
    let (guide_id, _, generation) = progress(&setup).expect("progress");
    let areas: Vec<String> = generation
        .areas
        .iter()
        .map(|area| area.area_id.clone())
        .collect();
    // The reviewer's own turn runs while the last area is written.
    let chat = vec![user("Hello"), answer("Hi."), user("What is left?")];
    for (index, area_id) in areas.iter().enumerate() {
        settle_until(&mut setup, "the area's child runs", |setup| {
            child_of(setup, area_id).is_some()
        });
        if index + 1 == areas.len() {
            let mut running = snapshot(chat.clone());
            running.runtime.active_turn_id = Some("turn-chat".to_owned());
            setup.deliver(&parent, running);
        }
        let child = child_of(&setup, area_id).expect("child");
        child_writes_its_area(&mut setup, &guide_id, &child, area_id);
    }
    let asked = |setup: &Setup| {
        setup
            .backend
            .host()
            .turns()
            .iter()
            .any(|(session, request)| *session == parent && request.text.contains("Synthesize"))
    };
    for _ in 0..4 {
        setup.settle();
    }
    assert!(
        !asked(&setup),
        "the synthesis waits while the reviewer's turn runs"
    );

    // The reviewer's turn ends: the synthesis is sent, and the run waits for its turn.
    let mut ended = chat;
    ended.push(answer("Two areas."));
    setup.deliver(&parent, snapshot(ended.clone()));
    settle_until(&mut setup, "synthesis is asked for", asked);
    for _ in 0..4 {
        setup.settle();
    }
    assert!(
        setup
            .app()
            .review()
            .is_some_and(crate::surface::ReviewSurface::fan_out_running),
        "the reviewer's turn ending does not end the synthesis"
    );
    assert_eq!(stored(&setup, &guide_id).0, GuideStatus::Synthesizing);

    // Its own turn ends: the version is complete.
    let synthesis = synthesis_text(&setup, &parent);
    ended.push(user(&synthesis));
    let mut running = snapshot(ended.clone());
    running.runtime.active_turn_id = Some("turn-synthesis".to_owned());
    setup.deliver(&parent, running);
    ended.push(answer("Synthesized."));
    setup.deliver(&parent, snapshot(ended));
    settle_until(&mut setup, "the fan-out ends", |setup| {
        setup
            .app()
            .review()
            .is_some_and(|review| !review.fan_out_running())
    });
    assert_eq!(stored(&setup, &guide_id).0, GuideStatus::Complete);
}

#[test]
fn stop_while_a_child_is_being_created_stops_that_child_too() {
    let mut setup = setup_with(backend(serial()), 170, 50, true);
    let parent = conversation(&mut setup);
    setup.scenario.key("esc");
    setup.settle();
    setup.scenario.key("alt+g");
    setup.settle();
    setup.scenario.key("f");
    // Step until the run is on screen and its first child's creation is queued, not run.
    for _ in 0..40 {
        let running = setup
            .app()
            .review()
            .is_some_and(crate::surface::ReviewSurface::fan_out_running);
        if running
            && setup.backend.host().child_sessions().is_empty()
            && setup.scenario.host().pending_tasks() > 0
        {
            break;
        }
        setup.scenario.step_async();
    }
    assert_eq!(setup.backend.host().child_sessions().len(), 0);

    // Stop before the child's creation ran: the creation finishes after the run was read.
    setup.scenario.key("shift+x");
    setup.settle();
    let (_, child) = setup
        .backend
        .host()
        .child_sessions()
        .first()
        .cloned()
        .expect("the child was created");
    let (guide_id, _, _) = progress(&setup).expect("progress");
    settle_until(&mut setup, "the version is stopped", |setup| {
        stored(setup, &guide_id).0 == GuideStatus::Cancelled
    });
    settle_until(&mut setup, "the late child is stopped", |setup| {
        setup.backend.host().cancelled_turns().contains(&child)
    });
    let (_, generation) = stored(&setup, &guide_id);
    assert!(
        generation
            .areas
            .iter()
            .all(|area| area.status != GuideAreaStatus::Running),
        "no area of a stopped version shows running: {generation:?}"
    );
    assert!(
        setup.backend.host().cancelled_turns().contains(&parent),
        "the conversation's turn is stopped"
    );
    assert!(
        setup
            .app()
            .review()
            .is_some_and(|review| !review.fan_out_running())
    );
}

#[test]
fn resuming_stops_children_still_recorded_running_before_starting_new_ones() {
    let mut setup = setup_with(backend(serial()), 170, 50, true);
    let parent = conversation(&mut setup);
    fan_out(&mut setup);
    settle_until(&mut setup, "the first child runs", |setup| {
        progress(setup).is_some_and(|(_, _, generation)| {
            generation
                .areas
                .iter()
                .any(|area| area.status == GuideAreaStatus::Running)
        })
    });
    let (guide_id, _, _) = progress(&setup).expect("progress");
    let (_, generation) = stored(&setup, &guide_id);
    let old = generation
        .areas
        .iter()
        .find_map(|area| area.session_id.clone())
        .expect("a running child");

    // The screen is rebuilt while the child runs: the run is dropped, the guide offers Resume.
    setup.reopen(170, 50);
    let mut reopened = setup;
    let _ = parent;
    conversation(&mut reopened);
    reopened.scenario.key("esc");
    reopened.settle();
    reopened.scenario.key("alt+g");
    settle_until(&mut reopened, "the guide is read", |setup| {
        progress(setup).is_some()
    });
    let before = reopened.backend.host().child_sessions().len();
    reopened.scenario.key("f");
    settle_until(&mut reopened, "a new child writes the area", |setup| {
        setup.backend.host().child_sessions().len() > before
    });
    assert!(
        reopened.backend.host().cancelled_turns().contains(&old),
        "the earlier child is stopped before the area gets a new one"
    );
}
