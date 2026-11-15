//! The review assistant's app tools: moving within the review on screen, changing its
//! comparison and view, opening another review, home or the wizard, finding pull requests and
//! stacks, and which of them Bcode asks about before they run.

use super::app::settled;
use super::compare::reopened_after_a_restack;
use crate::app::{ReviewApp, RouteName};
use crate::assistant::actions;
use crate::host::ClientToolReply;
use crate::testing::backend::TestBackend;
use crate::testing::harness::Scenario;
use bcode_tool_models::client_tool::ClientToolPolicy;

const SESSION: &str = "11111111-2222-4333-8444-555555555555";

/// Call the app's tool `tool` with `arguments` on `host`, as the daemon does once the call
/// passed the permission rules, and let the app answer.
fn call(scenario: &mut Scenario<ReviewApp>, tool: &str, arguments: &str) -> ClientToolReply {
    let mut answer = scenario.host().call_client_tool(SESSION, tool, arguments);
    for _ in 0..40 {
        settled(scenario);
        if let Ok(reply) = answer.try_recv() {
            return reply;
        }
    }
    panic!("{tool} was not answered");
}

fn done(reply: ClientToolReply) -> serde_json::Value {
    match reply {
        ClientToolReply::Completed(text) => serde_json::from_str(&text).expect("json"),
        ClientToolReply::Failed { code, message } => panic!("{code}: {message}"),
    }
}

fn refused(reply: ClientToolReply) -> String {
    match reply {
        ClientToolReply::Failed { message, .. } => message,
        ClientToolReply::Completed(text) => panic!("answered: {text}"),
    }
}

#[test]
fn moving_comparing_and_showing_in_a_stack_follow_the_keys_and_answer_the_new_screen() {
    let (_fixture, _backend, id, mut scenario) = reopened_after_a_restack(TestBackend::default());
    settled(&mut scenario);
    assert!(
        scenario
            .host()
            .client_tool_names()
            .iter()
            .any(|name| name == actions::GO_TO),
        "{:?}",
        scenario.host().client_tool_names()
    );

    let moved = done(call(&mut scenario, actions::GO_TO, r#"{"layer":102}"#));
    assert_eq!(
        moved["screen"]["review"]["stack"]["current"], 102,
        "{moved}"
    );
    let context = scenario.surface().view_context();
    assert_eq!(
        context
            .review
            .as_ref()
            .and_then(|review| review.stack.as_ref())
            .and_then(|stack| stack.current),
        Some(102)
    );
    let missing = refused(call(&mut scenario, actions::GO_TO, r#"{"layer":999}"#));
    assert!(missing.contains("999"), "{missing}");
    let elsewhere = refused(call(
        &mut scenario,
        actions::GO_TO,
        r#"{"review":"another-review","layer":101}"#,
    ));
    assert!(elsewhere.contains("open_review"), "{elsewhere}");
    let into_file = done(call(
        &mut scenario,
        actions::GO_TO,
        r#"{"layer":101,"path":"restacked.txt"}"#,
    ));
    assert_eq!(
        into_file["screen"]["review"]["file"]["path"], "restacked.txt",
        "{into_file}"
    );

    let changes = done(call(
        &mut scenario,
        actions::SET_COMPARISON,
        r#"{"view":"changes","all_layers":true}"#,
    ));
    let comparison = &changes["screen"]["review"]["comparison"];
    assert_eq!(comparison["view"], "changes", "{changes}");
    assert_eq!(
        changes["screen"]["review"]["stack"]["view"], "all_prs",
        "{changes}"
    );
    let review = scenario.surface().review().expect("review");
    assert_eq!(
        review.comparing().view(),
        bcode_code_review::document::CompareView::Changes
    );
    let off = done(call(
        &mut scenario,
        actions::SET_COMPARISON,
        r#"{"baseline":"off"}"#,
    ));
    assert_eq!(
        off["screen"]["review"]["comparison"]["baseline"], "off",
        "{off}"
    );
    let bad = refused(call(
        &mut scenario,
        actions::SET_COMPARISON,
        r#"{"baseline":"ago"}"#,
    ));
    assert!(bad.contains("ago_minutes"), "{bad}");

    let side = done(call(
        &mut scenario,
        actions::SET_VIEW,
        r#"{"layout":"side_by_side","hide_resolved_threads":true}"#,
    ));
    assert!(
        side["done"]
            .as_str()
            .is_some_and(|text| text.contains("side_by_side"))
    );
    let doc = scenario
        .surface()
        .review()
        .and_then(crate::ReviewSurface::document)
        .expect("doc");
    assert_eq!(
        doc.layout(),
        bcode_code_review::document::LayoutMode::SideBySide
    );
    let _ = id;
}

#[test]
fn open_review_takes_the_user_to_another_review_at_a_place_home_or_the_wizard() {
    let mut setup = super::assistant::setup(160, 40, true);
    let other = setup.other.id.clone();
    let opened = done(call(
        &mut setup.scenario,
        actions::OPEN_REVIEW,
        &format!(r#"{{"review":"{other}"}}"#),
    ));
    assert_eq!(opened["screen"]["review"]["id"], other.as_str(), "{opened}");
    assert_eq!(setup.app().route_name(), RouteName::Review);
    // A place in it once it loaded.
    let review = setup.review.id.clone();
    let at_file = done(call(
        &mut setup.scenario,
        actions::OPEN_REVIEW,
        &format!(
            r#"{{"review":"{review}","path":"{}"}}"#,
            crate::testing::fixture::GitFixture::modified_path(0)
        ),
    ));
    assert_eq!(
        at_file["screen"]["review"]["id"],
        review.as_str(),
        "{at_file}"
    );
    let missing = refused(call(
        &mut setup.scenario,
        actions::OPEN_REVIEW,
        r#"{"review":"no-such-review"}"#,
    ));
    assert!(missing.contains("no saved review"), "{missing}");
    assert_eq!(
        setup.app().route_name(),
        RouteName::Review,
        "a failed open stays put"
    );
    let home = done(call(
        &mut setup.scenario,
        actions::OPEN_REVIEW,
        r#"{"screen":"home"}"#,
    ));
    assert_eq!(home["screen"]["screen"], "home");
    assert_eq!(setup.app().route_name(), RouteName::Home);
    // Review tools need a review on screen.
    let no_review = refused(call(&mut setup.scenario, actions::GO_TO, r#"{"layer":1}"#));
    assert!(no_review.contains("open_review"), "{no_review}");
    done(call(
        &mut setup.scenario,
        actions::OPEN_REVIEW,
        r#"{"screen":"new_review"}"#,
    ));
    assert_eq!(setup.app().route_name(), RouteName::NewReview);
}

#[test]
fn moving_the_user_to_another_screen_asks_and_moves_within_a_review_do_not() {
    let tools = crate::assistant::profile::client_tools();
    let policy = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.name == name)
            .map_or_else(|| panic!("{name} is registered"), |tool| tool.policy)
    };
    for name in [
        actions::OPEN_REVIEW,
        actions::COMPLETE_PASS,
        actions::PUBLISH,
        actions::PULL_REQUEST_REVIEW,
    ] {
        assert_eq!(policy(name), ClientToolPolicy::Mutating, "{name} asks");
    }
    for name in [
        actions::GO_TO,
        actions::SET_COMPARISON,
        actions::SET_VIEW,
        actions::REFRESH,
        actions::FIND_PULL_REQUESTS,
        crate::assistant::profile::CURRENT_VIEW,
    ] {
        assert_eq!(policy(name), ClientToolPolicy::ReadOnly, "{name} runs");
    }
    // A call Bcode denied never reaches the app: the screen stays as it was.
    let setup = super::assistant::setup(160, 40, true);
    assert_eq!(setup.app().route_name(), RouteName::Review);
    assert_eq!(
        setup
            .app()
            .review()
            .and_then(crate::ReviewSurface::workspace)
            .map(|workspace| workspace.id.clone()),
        Some(setup.review.id.clone())
    );
}

#[test]
fn find_pull_requests_lists_the_stack_with_a_source_ready_to_create() {
    let (_fixture, _backend, _id, mut scenario) = reopened_after_a_restack(TestBackend::default());
    let found = done(call(
        &mut scenario,
        actions::FIND_PULL_REQUESTS,
        r#"{"filter":"all"}"#,
    ));
    let stacks = found["stacks"].as_array().expect("stacks");
    let numbers: Vec<u64> = stacks
        .iter()
        .flat_map(|stack| stack["layers"].as_array().into_iter().flatten())
        .filter_map(|layer| layer["number"].as_u64())
        .collect();
    for number in [101, 102, 103] {
        assert!(numbers.contains(&number), "{found}");
    }
    // The fake forge's pull requests chain into one stack, bottom first.
    let stack = &stacks[0];
    assert_eq!(
        stack["create_review_source"]["kind"], "stack_layers",
        "{found}"
    );
    let layers: Vec<u64> = stack["create_review_source"]["layers"]
        .as_array()
        .expect("layers")
        .iter()
        .filter_map(serde_json::Value::as_u64)
        .collect();
    assert!(layers.ends_with(&[101, 102, 103]), "{layers:?}");
}
