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

/// The panel's active conversation, started (and the panel closed again) when there is none:
/// only that conversation may use the app's tools.
pub(super) fn active_session(scenario: &mut Scenario<ReviewApp>) -> String {
    if let Some(session) = scenario.surface().assistant().session() {
        return session.to_owned();
    }
    scenario.key("ctrl+k");
    scenario.paste("Help me with this review");
    scenario.key("enter");
    settled(scenario);
    scenario.key("ctrl+k");
    settled(scenario);
    scenario
        .surface()
        .assistant()
        .session()
        .expect("the conversation started")
        .to_owned()
}

/// Call the app's tool `tool` with `arguments` as the panel's conversation, as the daemon does
/// once the call passed the permission rules, and let the app answer.
fn call(scenario: &mut Scenario<ReviewApp>, tool: &str, arguments: &str) -> ClientToolReply {
    let session = active_session(scenario);
    call_as(scenario, &session, tool, arguments)
}

/// Call the app's tool `tool` with `arguments` as `session`, and let the app answer.
fn call_as(
    scenario: &mut Scenario<ReviewApp>,
    session: &str,
    tool: &str,
    arguments: &str,
) -> ClientToolReply {
    let mut answer = scenario.host().call_client_tool(session, tool, arguments);
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
}

/// The permission request the daemon shows for `tool` called with `arguments`.
fn asking(id: &str, tool: &str, arguments: &str) -> bcode_session_view_models::PermissionView {
    bcode_session_view_models::PermissionView {
        permission_id: id.to_owned(),
        session_id: None,
        tool_call_id: format!("call-{id}"),
        tool_name: tool.to_owned(),
        producer_plugin_id: None,
        arguments_json: arguments.to_owned(),
        batch: None,
        agent_id: crate::assistant::profile::PROFILE_ID.to_owned(),
        title: None,
        policy_source: None,
        detail: None,
        resolved: false,
        approved: None,
        can_remember: true,
    }
}

/// The open review's id, if a review is on screen.
fn open_review_id(setup: &super::assistant::Setup) -> Option<String> {
    setup
        .app()
        .review()
        .and_then(crate::ReviewSurface::workspace)
        .map(|workspace| workspace.id.clone())
}

#[test]
fn a_declined_open_review_never_changes_the_screen_and_an_allowed_one_does() {
    let mut setup = super::assistant::setup(160, 40, true);
    setup.scenario.key("ctrl+k");
    let session = setup.send("Take me home");
    let arguments = r#"{"screen":"home"}"#;
    // The model calls open_review; Bcode asks before the call reaches the app.
    let mut declined =
        setup
            .backend
            .host()
            .gate_client_tool("p-1", &session, actions::OPEN_REVIEW, arguments);
    let mut prompt = super::assistant::snapshot(vec![super::assistant::user("Take me home")]);
    prompt.runtime.active_turn_id = Some("turn-1".to_owned());
    prompt
        .permissions
        .push(asking("p-1", actions::OPEN_REVIEW, arguments));
    let screen = setup.deliver(&session, prompt.clone());
    assert!(screen.contains("y allow"), "{}", screen.text());
    setup.scenario.key("n");
    setup.settle();
    assert!(matches!(
        declined.try_recv(),
        Ok(ClientToolReply::Failed { ref code, .. }) if code == "permission_denied"
    ));
    assert_eq!(setup.app().route_name(), RouteName::Review);
    assert_eq!(open_review_id(&setup), Some(setup.review.id.clone()));

    // Allowed, the same call moves the user.
    let mut allowed =
        setup
            .backend
            .host()
            .gate_client_tool("p-2", &session, actions::OPEN_REVIEW, arguments);
    prompt.permissions = vec![asking("p-2", actions::OPEN_REVIEW, arguments)];
    setup.deliver(&session, prompt);
    setup.scenario.key("y");
    for _ in 0..4 {
        setup.settle();
    }
    assert!(matches!(
        allowed.try_recv(),
        Ok(ClientToolReply::Completed(_))
    ));
    assert_eq!(setup.app().route_name(), RouteName::Home);
}

#[test]
fn the_first_turn_of_a_new_conversation_already_has_the_apps_tools_and_profile() {
    let mut setup = super::assistant::setup(160, 40, false);
    assert_eq!(setup.app().route_name(), RouteName::Home);
    setup.scenario.key("ctrl+k");
    let session = setup.send("Create a review for my most recent PR stack");
    let host = setup.backend.host();
    let (turn_session, offered) = host.turn_tools().first().cloned().expect("a turn");
    assert_eq!(turn_session, session);
    for tool in [
        crate::assistant::profile::CURRENT_VIEW,
        actions::FIND_PULL_REQUESTS,
        actions::OPEN_REVIEW,
    ] {
        assert!(
            offered.iter().any(|name| name == tool),
            "{tool} in {offered:?}"
        );
    }
    let (_, request) = host.turns().first().cloned().expect("a turn");
    assert_eq!(
        request.agent.as_deref(),
        Some(crate::assistant::profile::PROFILE_ID)
    );
    // A conversation switched to from the history is claimed before its next turn.
    setup.scenario.key("ctrl+n");
    let other = setup.send("Another question");
    assert_ne!(other, session);
    assert_eq!(host.released_sessions(), vec![session]);
    let (_, offered) = host.turn_tools().last().cloned().expect("a turn");
    assert!(offered.iter().any(|name| name == actions::OPEN_REVIEW));
}

#[test]
fn messages_wait_for_the_profile_while_the_tool_connection_is_down() {
    let mut setup = super::assistant::setup(160, 40, false);
    let host = setup.backend.host();
    host.set_tool_link_state(crate::host::ToolLinkState::Failed(
        "Bcode restarted".to_owned(),
    ));
    setup.scenario.key("ctrl+k");
    setup.scenario.paste("Hello");
    setup.scenario.key("enter");
    setup.settle();
    assert!(host.turns().is_empty(), "no turn runs without the profile");
    let notice = setup.app().assistant().notice().map(str::to_owned);
    assert!(
        notice
            .as_deref()
            .is_some_and(|notice| notice.contains("waiting") && notice.contains("Bcode restarted")),
        "{notice:?}"
    );
    // Registered again: the message goes, with the profile and the tools.
    host.set_tool_link_state(crate::host::ToolLinkState::Ready);
    setup.settle();
    let (session, request) = host.turns().first().cloned().expect("sent once registered");
    assert_eq!(request.text, "Hello");
    assert_eq!(
        request.agent.as_deref(),
        Some(crate::assistant::profile::PROFILE_ID)
    );
    let (_, offered) = host.turn_tools().first().cloned().expect("a turn");
    assert!(offered.iter().any(|name| name == actions::OPEN_REVIEW));
    assert_eq!(setup.app().assistant().session(), Some(session.as_str()));
    assert_eq!(setup.app().assistant().notice(), None);
}

#[test]
fn only_the_panels_active_conversation_reads_or_moves_the_screen() {
    let mut setup = super::assistant::setup(160, 40, true);
    setup.scenario.key("ctrl+k");
    let first = setup.send("First conversation");
    setup.scenario.key("ctrl+n");
    let second = setup.send("Second conversation");
    // The first conversation's turns may still call the tools it was offered: refused.
    for tool in [
        crate::assistant::profile::CURRENT_VIEW,
        actions::OPEN_REVIEW,
    ] {
        let answer = call_as(&mut setup.scenario, &first, tool, r#"{"screen":"home"}"#);
        assert!(
            matches!(
                answer,
                ClientToolReply::Failed { ref code, .. }
                    if code == crate::app::NOT_ACTIVE_CONVERSATION
            ),
            "{tool}: {answer:?}"
        );
    }
    assert_eq!(setup.app().route_name(), RouteName::Review);
    assert_eq!(
        setup.backend.host().claimed_sessions(),
        vec![second.clone()]
    );
    let view = done(call_as(
        &mut setup.scenario,
        &second,
        crate::assistant::profile::CURRENT_VIEW,
        "{}",
    ));
    assert_eq!(view["screen"], "review");
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

#[test]
fn moving_the_user_off_a_review_saves_it_first_and_never_drops_unsaved_comments() {
    // A saved review: the move persists what the reviewer changed before leaving, as exiting
    // does, even inside the presentation save interval.
    let mut setup = super::assistant::setup(160, 40, true);
    let session = active_session(&mut setup.scenario);
    setup.scenario.key("shift+h");
    let home = done(call_as(
        &mut setup.scenario,
        &session,
        actions::OPEN_REVIEW,
        r#"{"screen":"home"}"#,
    ));
    assert_eq!(home["screen"]["screen"], "home", "{home}");
    assert_eq!(setup.app().route_name(), RouteName::Home);
    let stored = setup
        .backend
        .call_now(bcode_code_review_models::GetReviewWorkspaceRequest {
            repo_path: setup.review.repo_root.clone(),
            workspace_id: setup.review.id.clone(),
        })
        .workspace
        .expect("saved");
    assert!(
        stored
            .presentation
            .is_some_and(|state| state.hide_resolved_threads),
        "the review saved its presentation before closing"
    );

    // An unsaved review with a comment is not left: the user is told how to save it.
    let fixture = super::app::fixture();
    let backend = std::sync::Arc::new(TestBackend::default());
    let entry = crate::entry::Entry::Sources {
        title: None,
        sources: vec![bcode_code_review_models::ReviewSourceKind::LastCommit],
    };
    let (mut scenario, _) = super::app::open(&fixture, &backend, entry, 160, 40);
    scenario.key("shift+c");
    scenario.paste("unsaved note");
    scenario.key("ctrl+s");
    settled(&mut scenario);
    let refusal = refused(call(
        &mut scenario,
        actions::OPEN_REVIEW,
        r#"{"screen":"home"}"#,
    ));
    assert!(refusal.contains("not saved"), "{refusal}");
    assert_eq!(scenario.surface().route_name(), RouteName::Review);
    let status = scenario
        .surface()
        .review()
        .and_then(crate::ReviewSurface::status)
        .map(str::to_owned);
    assert!(
        status
            .as_deref()
            .is_some_and(|status| status.contains("not saved")),
        "{status:?}"
    );
}

/// Call `tool` until it answers without asking to be called again (history loading).
fn call_until_loaded(
    scenario: &mut Scenario<ReviewApp>,
    tool: &str,
    arguments: &str,
) -> ClientToolReply {
    for _ in 0..10 {
        let reply = call(scenario, tool, arguments);
        match &reply {
            ClientToolReply::Failed { message, .. } if message.contains("loading") => {
                settled(scenario);
            }
            _ => return reply,
        }
    }
    panic!("{tool} kept loading");
}

#[test]
fn a_comparison_or_view_change_with_one_bad_argument_changes_nothing() {
    let (_fixture, _backend, _id, mut scenario) = reopened_after_a_restack(TestBackend::default());
    let session = active_session(&mut scenario);
    let before = settled(&mut scenario).text();
    let view_before = scenario
        .surface()
        .view_context()
        .review
        .and_then(|review| review.comparison)
        .map(|comparison| comparison.view);
    let bad = refused(call_as(
        &mut scenario,
        &session,
        actions::SET_COMPARISON,
        r#"{"view":"changes","only_changed":true,"mark_style":"sparkles"}"#,
    ));
    assert!(bad.contains("sparkles"), "{bad}");
    let bad = refused(call_as(
        &mut scenario,
        &session,
        actions::SET_VIEW,
        r#"{"wrap":true,"hide_resolved_threads":true,"layout":"diagonal"}"#,
    ));
    assert!(bad.contains("diagonal"), "{bad}");
    assert_eq!(
        scenario
            .surface()
            .view_context()
            .review
            .and_then(|review| review.comparison)
            .map(|comparison| comparison.view),
        view_before,
        "the view did not change"
    );
    assert_eq!(
        settled(&mut scenario).text(),
        before,
        "the screen is as it was"
    );
}

#[test]
fn a_go_to_with_one_bad_place_does_not_move_the_layer() {
    let (_fixture, _backend, _id, mut scenario) = reopened_after_a_restack(TestBackend::default());
    let session = active_session(&mut scenario);
    let current = |scenario: &Scenario<ReviewApp>| {
        scenario
            .surface()
            .view_context()
            .review
            .and_then(|review| review.stack)
            .and_then(|stack| stack.current)
    };
    let before = current(&scenario);
    let other = if before == Some(101) { 102 } else { 101 };
    let before_screen = settled(&mut scenario).text();
    let thread = refused(call_as(
        &mut scenario,
        &session,
        actions::GO_TO,
        &format!(r#"{{"layer":{other},"thread":"no-such-thread"}}"#),
    ));
    assert!(thread.contains("no-such-thread"), "{thread}");
    let path = refused(call_as(
        &mut scenario,
        &session,
        actions::GO_TO,
        &format!(r#"{{"layer":{other},"path":"no/such/file.rs"}}"#),
    ));
    assert!(path.contains("no/such/file.rs"), "{path}");
    assert_eq!(current(&scenario), before, "the layer did not change");
    assert_eq!(
        settled(&mut scenario).text(),
        before_screen,
        "the screen is as it was"
    );
    // The same places are fine once they are real.
    let moved = done(call_as(
        &mut scenario,
        &session,
        actions::GO_TO,
        &format!(r#"{{"layer":{other}}}"#),
    ));
    assert_eq!(
        moved["screen"]["review"]["stack"]["current"], other,
        "{moved}"
    );
}

#[test]
fn pushes_stack_at_and_versions_are_reachable_like_the_compare_menu() {
    let (_fixture, _backend, _id, mut scenario) = reopened_after_a_restack(TestBackend::default());
    let compared = done(call_until_loaded(
        &mut scenario,
        actions::SET_COMPARISON,
        r#"{"baseline":"pushes","from_push":1}"#,
    ));
    let comparison = &compared["screen"]["review"]["comparison"];
    assert!(
        comparison["baseline"]
            .as_str()
            .is_some_and(|name| name.starts_with("between=")),
        "{compared}"
    );
    let pushes = comparison["pushes"]["items"]
        .as_array()
        .expect("pushes listed");
    assert_eq!(pushes[0]["number"], 1, "{compared}");
    let beyond = refused(call(
        &mut scenario,
        actions::SET_COMPARISON,
        r#"{"baseline":"pushes","from_push":99}"#,
    ));
    assert!(beyond.contains("pushes 1 to"), "{beyond}");

    let at = pushes[0]["at_ms"].as_u64().expect("time");
    let then = done(call(
        &mut scenario,
        actions::SET_COMPARISON,
        &format!(r#"{{"baseline":"stack_at","at_ms":{at}}}"#),
    ));
    assert!(
        then["screen"]["review"]["comparison"]["baseline"]
            .as_str()
            .is_some_and(|name| name.starts_with("stack_at=")),
        "{then}"
    );

    let version = done(call(
        &mut scenario,
        actions::GO_TO,
        r#"{"layer":101,"version":1}"#,
    ));
    assert!(
        version["done"]
            .as_str()
            .is_some_and(|done| done.contains("#101 v1")),
        "{version}"
    );
    let missing = refused(call(
        &mut scenario,
        actions::GO_TO,
        r#"{"layer":101,"version":50}"#,
    ));
    assert!(missing.contains("versions v1 to"), "{missing}");
}

#[test]
fn complete_pass_refresh_publish_and_pull_request_verdicts_act_as_their_keys() {
    let (fixture, backend, id, mut scenario) = reopened_after_a_restack(TestBackend::default());
    // Refresh checks for changes, as `r` does.
    let refreshed = done(call(&mut scenario, actions::REFRESH, "{}"));
    assert_eq!(refreshed["screen"]["screen"], "review", "{refreshed}");
    // Completing the pass records the revisions on screen, as `shift+m` does.
    let before = super::app::stored(&backend, &fixture, &id).reviewed_revisions;
    let passed = done(call(&mut scenario, actions::COMPLETE_PASS, "{}"));
    assert!(passed["done"].is_string(), "{passed}");
    settled(&mut scenario);
    let after = super::app::stored(&backend, &fixture, &id).reviewed_revisions;
    assert!(!after.is_empty(), "the pass was recorded");
    assert!(
        after
            .values()
            .any(|revision| !before.values().any(|kept| kept == revision))
            || before.is_empty(),
        "the pass recorded the revisions on screen now"
    );
    // A verdict opens the pull request dialog for the reviewer to confirm; nothing is sent.
    let approve = done(call(
        &mut scenario,
        actions::PULL_REQUEST_REVIEW,
        r#"{"action":"approve"}"#,
    ));
    assert!(
        approve["done"]
            .as_str()
            .is_some_and(|done| done.contains("confirm")),
        "{approve}"
    );
    let unknown = refused(call(
        &mut scenario,
        actions::PULL_REQUEST_REVIEW,
        r#"{"action":"merge"}"#,
    ));
    assert!(unknown.contains("merge"), "{unknown}");
    // The reviewer dismisses the dialog.
    if scenario
        .surface()
        .review()
        .is_some_and(|review| review.modal().is_some())
    {
        scenario.key("esc");
        settled(&mut scenario);
    }
    assert_eq!(scenario.surface().route_name(), RouteName::Review);
    // Publishing opens the publish dialog, never publishing by itself.
    let publish = done(call(&mut scenario, actions::PUBLISH, "{}"));
    assert!(
        publish["done"]
            .as_str()
            .is_some_and(|done| done.contains("confirm")),
        "{publish}"
    );
    assert!(matches!(
        scenario
            .surface()
            .review()
            .and_then(crate::ReviewSurface::modal),
        Some(crate::surface::Modal::Publish(_))
    ));
    // Publishing and verdicts always ask, and the panel never offers to remember them.
    for name in [actions::PUBLISH, actions::PULL_REQUEST_REVIEW] {
        assert!(
            crate::assistant::panel::NEVER_REMEMBER.contains(&name),
            "{name} is asked every time"
        );
    }
}

#[test]
fn verdicts_need_pull_requests_and_refresh_reloads_home() {
    let mut setup = super::assistant::setup(160, 40, true);
    let no_pulls = refused(call(
        &mut setup.scenario,
        actions::PULL_REQUEST_REVIEW,
        r#"{"action":"approve"}"#,
    ));
    assert!(no_pulls.contains("no pull requests"), "{no_pulls}");
    done(call(
        &mut setup.scenario,
        actions::OPEN_REVIEW,
        r#"{"screen":"home"}"#,
    ));
    let reloaded = done(call(&mut setup.scenario, actions::REFRESH, "{}"));
    assert_eq!(reloaded["screen"]["screen"], "home", "{reloaded}");
    assert!(
        reloaded["done"]
            .as_str()
            .is_some_and(|done| done.contains("reloading")),
        "{reloaded}"
    );
}

#[test]
fn open_review_shows_the_wizard_filled_in_for_the_user_to_confirm() {
    let mut setup = super::assistant::setup(160, 40, false);
    let shown = done(call(
        &mut setup.scenario,
        actions::OPEN_REVIEW,
        r#"{"screen":"new_review","title":"Uncommitted docs","sources":[{"kind":"working_tree","paths":["docs"]},{"kind":"last_commit"}]}"#,
    ));
    assert!(
        shown["done"]
            .as_str()
            .is_some_and(|done| done.contains("filled in")),
        "{shown}"
    );
    assert_eq!(setup.app().route_name(), RouteName::NewReview);
    let wizard = setup.app().wizard().expect("wizard");
    assert_eq!(wizard.step_number(), 3, "at the last step to confirm");
    assert_eq!(
        wizard.source_labels().len(),
        2,
        "{:?}",
        wizard.source_labels()
    );
    let screen = setup.settle();
    assert!(screen.contains("Uncommitted docs"), "{}", screen.text());
    // Nothing is created until the user confirms.
    assert!(
        !screen.contains("Creating"),
        "the review is not created by the tool"
    );
    let bad = refused(call(
        &mut setup.scenario,
        actions::OPEN_REVIEW,
        r#"{"screen":"new_review","sources":[{"kind":"teleport"}]}"#,
    ));
    assert!(bad.contains("teleport"), "{bad}");
}
