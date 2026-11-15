//! The review assistant reaching threads with the app's tools: pull request threads named
//! exactly as the review tools (`code_review.list_threads`) and the context name them, current
//! and outdated ones, in a stack shown one layer at a time or all at once; and blank optional
//! arguments taken as not given.

use super::app::{create, fixture, settled};
use super::assistant_actions::active_session;
use super::publish::{layer, resume};
use crate::app::ReviewApp;
use crate::assistant::actions;
use crate::config::conversation::{ConversationConfig, SignalsConfig};
use crate::host::ClientToolReply;
use crate::testing::backend::TestBackend;
use crate::testing::fake_provider::{FAKE_REPOSITORY, FakeProvider};
use crate::testing::fixture::GitFixture;
use crate::testing::harness::Scenario;
use bcode_code_review::document::ItemKey;
use bcode_code_review_models::{
    ConversationCapabilities, DiffSide, Participant, PullActivity, PullState, RemoteAnchor,
    RemoteComment, RemoteThread, VerdictSupport, ViewerAbilities,
};
use std::sync::Arc;

fn comment(number: u64, id: &str, body: &str) -> RemoteComment {
    RemoteComment {
        id: id.to_owned(),
        author: Participant {
            login: "raj".to_owned(),
            ..Participant::default()
        },
        body: body.to_owned(),
        created_at_ms: Some(1_700_000_000_000),
        updated_at_ms: None,
        url: Some(format!(
            "https://example.com/{FAKE_REPOSITORY}/pull/{number}#{id}"
        )),
        review_id: None,
    }
}

fn activity(number: u64, threads: Vec<RemoteThread>) -> PullActivity {
    PullActivity {
        number,
        title: format!("Layer {number}"),
        head_oid: String::new(),
        state: PullState::Open,
        author: None,
        description: String::new(),
        url: Some(format!(
            "https://example.com/{FAKE_REPOSITORY}/pull/{number}"
        )),
        decision: None,
        reviews: Vec::new(),
        requested: Vec::new(),
        threads,
        comments: Vec::new(),
        checks: bcode_code_review_models::Checks::default(),
        viewer: ViewerAbilities {
            is_author: false,
            can_review: true,
            can_judge: true,
            can_comment: true,
        },
        updated_at_ms: None,
    }
}

/// A thread on line 3 of `path`, current or outdated (written on an old commit, its line gone).
fn thread(id: &str, number: u64, path: &str, outdated: bool) -> RemoteThread {
    RemoteThread {
        id: id.to_owned(),
        anchor: RemoteAnchor {
            path: path.to_owned(),
            side: DiffSide::New,
            line: (!outdated).then_some(3),
            original_line: Some(3),
            original_commit_oid: outdated.then(|| "0ldc0mm1t".to_owned()),
            ..RemoteAnchor::default()
        },
        resolved: false,
        outdated,
        can_reply: true,
        can_resolve: true,
        comments: vec![comment(number, &format!("{id}-c"), &format!("About {id}"))],
    }
}

const fn everything() -> ConversationCapabilities {
    ConversationCapabilities {
        activity: true,
        watch: None,
        thread_reply: true,
        thread_resolve: true,
        comment_post: true,
        review_preview: true,
        verdicts: VerdictSupport {
            comment: true,
            approve: true,
            request_changes: true,
        },
        replies_in_review: true,
    }
}

/// The three-layer stack, with a current thread on #102's file and an outdated one on
/// #103's, opened; and the ids the review tools give its pull request threads.
fn stack_with_threads() -> (
    GitFixture,
    Arc<TestBackend>,
    Scenario<ReviewApp>,
    Vec<String>,
) {
    let fixture = fixture();
    let provider = FakeProvider::new(fixture.root()).with_conversation(everything());
    provider.set_activity(activity(
        102,
        vec![thread(
            "PRRT_current",
            102,
            &GitFixture::stack_layer_path(2),
            false,
        )],
    ));
    provider.set_activity(activity(
        103,
        vec![thread(
            "PRRT_outdated",
            103,
            &GitFixture::stack_layer_path(3),
            true,
        )],
    ));
    let backend = Arc::new(
        TestBackend::default()
            .with_provider(provider)
            .with_conversation_config(ConversationConfig {
                signals: SignalsConfig {
                    min_gap_ms: 0,
                    ..SignalsConfig::default()
                },
                ..ConversationConfig::default()
            }),
    );
    let workspace = create(
        &backend,
        &fixture,
        "Threads",
        vec![layer(101), layer(102), layer(103)],
    );
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    let ids = listed_thread_ids(&backend, &scenario);
    (fixture, backend, scenario, ids)
}

/// The pull request thread ids `code_review.list_threads` gives: the ids of the snapshot the
/// app keeps for the review tools.
fn listed_thread_ids(backend: &TestBackend, scenario: &Scenario<ReviewApp>) -> Vec<String> {
    let workspace = scenario
        .surface()
        .review()
        .and_then(|review| review.workspace())
        .expect("workspace");
    let scope = crate::notes::NoteScope::for_workspace(workspace);
    let saved: bcode_code_review_models::GetReviewConversationResponse =
        backend.call_now(bcode_code_review_models::GetReviewConversationRequest {
            repo_path: scope.repo_path,
            target: scope.target,
            scope: scope.scope,
        });
    saved
        .snapshot
        .expect("pull request threads saved for the review tools")
        .threads
        .into_iter()
        .map(|record| record.id)
        .collect()
}

fn call(scenario: &mut Scenario<ReviewApp>, tool: &str, arguments: &str) -> ClientToolReply {
    let session = active_session(scenario);
    let mut answer = scenario.host().call_client_tool(&session, tool, arguments);
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

/// The id of the thread the cursor is on, as the context gives it.
fn focused(scenario: &Scenario<ReviewApp>) -> Option<String> {
    scenario
        .surface()
        .view_context()
        .review?
        .threads
        .focused
        .map(|thread| thread.id)
}

fn current_layer(scenario: &Scenario<ReviewApp>) -> Option<u64> {
    scenario.surface().view_context().review?.stack?.current
}

fn id_of<'a>(ids: &'a [String], thread: &str) -> &'a str {
    ids.iter()
        .find(|id| id.ends_with(&format!(":{thread}")))
        .unwrap_or_else(|| panic!("{thread} not listed in {ids:?}"))
}

#[test]
fn go_to_reaches_pull_request_threads_by_the_id_the_review_tools_give() {
    let (_fixture, _backend, mut scenario, ids) = stack_with_threads();
    let current = id_of(&ids, "PRRT_current").to_owned();
    let outdated = id_of(&ids, "PRRT_outdated").to_owned();
    assert_eq!(current, format!("{FAKE_REPOSITORY}#102:PRRT_current"));

    // One layer at a time: named with its layer, or alone (its layer is entered).
    let moved = done(call(
        &mut scenario,
        actions::GO_TO,
        &format!(r#"{{"layer":102,"thread":"{current}"}}"#),
    ));
    assert_eq!(
        focused(&scenario).as_deref(),
        Some(current.as_str()),
        "{moved}"
    );
    assert_eq!(current_layer(&scenario), Some(102));
    let moved = done(call(
        &mut scenario,
        actions::GO_TO,
        &format!(r#"{{"thread":"{outdated}"}}"#),
    ));
    assert_eq!(
        focused(&scenario).as_deref(),
        Some(outdated.as_str()),
        "{moved}"
    );
    assert_eq!(current_layer(&scenario), Some(103));
    let said = moved["done"].as_str().unwrap_or_default();
    assert!(said.contains("layer 103"), "{said}");
    assert!(said.contains("on its file"), "{said}");

    // The failing call as the assistant made it: the thread wins over a stale line and an
    // unneeded version, and the answer says so.
    let moved = done(call(
        &mut scenario,
        actions::GO_TO,
        &format!(
            r#"{{"layer":101,"path":"{}","line":324,"side":"new","thread":"{current}","version":1}}"#,
            GitFixture::stack_layer_path(2)
        ),
    ));
    assert_eq!(
        focused(&scenario).as_deref(),
        Some(current.as_str()),
        "{moved}"
    );
    assert_eq!(current_layer(&scenario), Some(102));
    let said = moved["done"].as_str().unwrap_or_default();
    assert!(said.contains("not where the thread is shown"), "{said}");
    assert!(said.contains("`version` not used"), "{said}");
    assert!(said.contains("went to its layer"), "{said}");
    // Named another layer while the thread's own is the one shown: it stays.
    done(call(
        &mut scenario,
        actions::GO_TO,
        &format!(r#"{{"layer":101,"thread":"{current}"}}"#),
    ));
    assert_eq!(current_layer(&scenario), Some(102));
    assert_eq!(focused(&scenario).as_deref(), Some(current.as_str()));

    // Every pull request at once.
    done(call(
        &mut scenario,
        actions::SET_COMPARISON,
        r#"{"all_layers":true}"#,
    ));
    for id in [&outdated, &current] {
        let moved = done(call(
            &mut scenario,
            actions::GO_TO,
            &format!(r#"{{"thread":"{id}"}}"#),
        ));
        assert_eq!(focused(&scenario).as_deref(), Some(id.as_str()), "{moved}");
    }

    // An id no thread has is refused, saying where ids come from.
    let missing = refused(call(
        &mut scenario,
        actions::GO_TO,
        &format!(r#"{{"thread":"{FAKE_REPOSITORY}#102:PRRT_none"}}"#),
    ));
    assert!(missing.contains("PRRT_none"), "{missing}");
}

#[test]
fn every_thread_id_the_context_gives_is_one_go_to_takes() {
    let (_fixture, _backend, mut scenario, ids) = stack_with_threads();
    done(call(
        &mut scenario,
        actions::SET_COMPARISON,
        r#"{"all_layers":true}"#,
    ));
    let mut seen = Vec::new();
    for layer in [2, 3] {
        done(call(
            &mut scenario,
            actions::GO_TO,
            &format!(r#"{{"path":"{}"}}"#, GitFixture::stack_layer_path(layer)),
        ));
        let review = scenario.surface().view_context().review.expect("review");
        for line in &review.threads.on_file.items {
            let id = line
                .split_once("thread id ")
                .and_then(|(_, rest)| rest.split_once(')'))
                .map_or_else(
                    || panic!("no thread id in {line:?}"),
                    |(id, _)| id.to_owned(),
                );
            seen.push(id);
        }
    }
    seen.sort();
    let mut listed = ids;
    listed.sort();
    assert_eq!(
        seen, listed,
        "the context names the threads as the tools do"
    );
    for id in &seen {
        done(call(
            &mut scenario,
            actions::GO_TO,
            &format!(r#"{{"thread":"{id}"}}"#),
        ));
        assert_eq!(focused(&scenario).as_deref(), Some(id.as_str()));
    }
}

#[test]
fn blank_optional_arguments_are_taken_as_not_given() {
    let (_fixture, _backend, mut scenario, _ids) = stack_with_threads();
    let path = GitFixture::stack_layer_path(2);
    let moved = done(call(
        &mut scenario,
        actions::GO_TO,
        &format!(
            r#"{{"review":"","layer":102,"path":"{path}","line":3,"side":"","source":"","thread":""}}"#
        ),
    ));
    assert_eq!(moved["screen"]["review"]["file"]["path"], path, "{moved}");
    let review = scenario.surface().review().expect("review");
    assert!(
        matches!(review.cursor(), Some(ItemKey::Line { file, line: 3, .. }) if file.path == path),
        "{:?}",
        review.cursor()
    );
    let nothing = refused(call(
        &mut scenario,
        actions::GO_TO,
        r#"{"thread":"","path":" ","layer":""}"#,
    ));
    assert!(nothing.contains("name a `layer`"), "{nothing}");

    let changed = done(call(
        &mut scenario,
        actions::SET_COMPARISON,
        r#"{"baseline":"","view":"changes","mark_style":"","seen_hunks":null}"#,
    ));
    assert_eq!(
        changed["screen"]["review"]["comparison"]["view"], "changes",
        "{changed}"
    );
    let shown = done(call(
        &mut scenario,
        actions::SET_VIEW,
        r#"{"layout":"","file_list":"","wrap":true}"#,
    ));
    assert!(shown["done"].is_string(), "{shown}");
    let view = call(
        &mut scenario,
        crate::assistant::profile::CURRENT_VIEW,
        r#"{"sections":"","max_files":null}"#,
    );
    assert!(matches!(view, ClientToolReply::Completed(_)), "{view:?}");
    let verdict = refused(call(
        &mut scenario,
        actions::PULL_REQUEST_REVIEW,
        r#"{"action":""}"#,
    ));
    assert!(verdict.contains("action"), "{verdict}");
}

#[test]
fn blank_values_are_dropped_from_every_object_and_nothing_else() {
    let cleaned: serde_json::Value = serde_json::from_str(&crate::app::without_blank_arguments(
        r#"{"a":"","b":" ","c":null,"d":"x","e":0,"f":false,"g":{"h":"","i":1},"j":[{"k":""},""]}"#,
    ))
    .expect("json");
    assert_eq!(
        cleaned,
        serde_json::json!({"d":"x","e":0,"f":false,"g":{"i":1},"j":[{},""]})
    );
    assert_eq!(crate::app::without_blank_arguments(""), "{}");
    assert_eq!(crate::app::without_blank_arguments("[1]"), "[1]");
}
