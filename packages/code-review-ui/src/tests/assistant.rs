//! The review assistant dock: opening and placing it, a conversation with a session bound to
//! the review, its permission prompt, and the review following what the session's review tools
//! change.

use super::interaction::{Setup, added_line, cursor, find, fixture, move_to, setup};
use super::stable;
use bcode_code_review::assistant::{Location, ReviewRef, ReviewTools, Side};
use bcode_code_review::document::{ItemKey, ReviewItem};
use bcode_code_review_models::{GetReviewAssistantRequest, ReviewAssistantLookup, ReviewLineSide};
use bcode_session_view::follow::SessionViewFollowUpdate;
use bcode_session_view_models::{
    ChatMessageView, PermissionView, SessionConnectionViewStatus, SessionViewSnapshot,
    ToolInvocationView, ToolInvocationViewStatus, ToolTimingView, TranscriptViewItem,
    TranscriptViewItemId, TranscriptViewItemKind,
};

fn item(id: &str, kind: TranscriptViewItemKind) -> TranscriptViewItem {
    TranscriptViewItem {
        id: TranscriptViewItemId::new(id),
        revision: 1,
        sequence: Some(1),
        timestamp_ms: Some(1),
        output_location: None,
        streaming: false,
        kind,
    }
}

fn user(text: &str) -> TranscriptViewItem {
    item(
        "user",
        TranscriptViewItemKind::UserMessage {
            message: ChatMessageView::markdown(text),
        },
    )
}

fn answer(text: &str) -> TranscriptViewItem {
    item(
        "answer",
        TranscriptViewItemKind::AssistantMessage {
            message: ChatMessageView::markdown(text),
        },
    )
}

/// A finished `name` call with `arguments`.
fn tool(id: &str, name: &str, arguments: &str) -> TranscriptViewItem {
    item(
        id,
        TranscriptViewItemKind::ToolInvocation {
            tool: Box::new(ToolInvocationView {
                tool_call_id: id.to_owned(),
                producer_plugin_id: Some("bcode.code_review".to_owned()),
                tool_name: Some(name.to_owned()),
                arguments_json: Some(arguments.to_owned()),
                working_directory: None,
                request_draft: None,
                status: ToolInvocationViewStatus::Finished,
                result_text: Some("done".to_owned()),
                is_error: Some(false),
                result: None,
                presentation: None,
                timing: ToolTimingView::default(),
            }),
        },
    )
}

fn snapshot(items: Vec<TranscriptViewItem>) -> SessionViewSnapshot {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.connection_status = SessionConnectionViewStatus::Attached;
    snapshot.transcript.items = items;
    snapshot
}

fn send(
    setup: &mut Setup,
    session: &str,
    snapshot: SessionViewSnapshot,
) -> crate::testing::harness::Screen {
    setup
        .scenario
        .host()
        .session_view(session)
        .expect("the dock follows its session")
        .try_send(SessionViewFollowUpdate::Snapshot(Box::new(snapshot)))
        .expect("delivered");
    stable(&mut setup.scenario)
}

/// Put the cursor on the first added line, open the dock, and send `text`; returns the
/// session and the message it got.
fn ask_assistant(setup: &mut Setup, text: &str) -> (String, String) {
    let index = find(&setup.scenario, 0, added_line);
    move_to(&mut setup.scenario, index);
    setup.scenario.key("ctrl+k");
    setup.scenario.paste(text);
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    setup.backend.messages().last().cloned().expect("sent")
}

/// File and new-side line of the first added line.
fn first_added(setup: &Setup) -> (String, u32) {
    let index = find(&setup.scenario, 0, added_line);
    let doc = setup.scenario.surface().document().expect("document");
    match doc.item(index).map(ReviewItem::key) {
        Some(ItemKey::Line { file, line, .. }) => (file.path.clone(), line),
        Some(ItemKey::LinePair { file, new, .. }) => (file.path.clone(), new.expect("new side")),
        other => panic!("not a line: {other:?}"),
    }
}

#[test]
fn ctrl_k_opens_the_dock_and_asks_a_session_bound_to_the_review() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 160, 40);
    setup.scenario.key("ctrl+k");
    let screen = stable(&mut setup.scenario);
    let text = screen.text();
    assert!(screen.contains("Assistant"), "{text}");
    assert!(screen.contains("Ask about this review."), "{text}");
    // Wide: the dock is to the right of the document.
    let title = screen.find("Assistant").expect("dock title");
    assert!(title.x > 80, "{title:?}");

    // esc returns to the review with the dock open; ctrl+k then focuses it again.
    setup.scenario.key("esc");
    let (path, line) = first_added(&setup);
    let (session, message) = ask_assistant(&mut setup, "Is this safe?");
    assert!(message.starts_with("Is this safe?"), "{message}");
    assert!(
        message.contains(&format!("{path}:{line}")),
        "the cursor's place is attached: {message}"
    );
    assert!(
        setup
            .backend
            .sessions()
            .iter()
            .any(|(id, title)| *id == session && title.starts_with("Review assistant:"))
    );
    let binding = setup
        .backend
        .call_now(GetReviewAssistantRequest {
            repo_path: setup.workspace.repo_root.clone(),
            lookup: ReviewAssistantLookup::Session {
                session_id: session.clone(),
            },
        })
        .binding
        .expect("the session is bound to the review");
    assert_eq!(binding.workspace.id, setup.workspace.id);
    assert_ne!(
        binding.resolved.len(),
        0,
        "the resolved sources are recorded"
    );

    let screen = send(
        &mut setup,
        &session,
        snapshot(vec![user(&message), answer("It is safe: ASSISTANT_ANSWER")]),
    );
    let text = screen.text();
    assert!(screen.contains("▍you"), "{text}");
    assert!(screen.contains("ASSISTANT_ANSWER"), "{text}");
    assert!(
        !screen.contains("[Review context"),
        "the question shows as typed: {text}"
    );

    // ctrl+x detaches the place; the next message is the text alone, in the same session.
    setup.scenario.key("ctrl+x");
    setup.scenario.paste("And the tests?");
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    let (again, message) = setup.backend.messages().last().cloned().expect("sent");
    assert_eq!(again, session);
    assert_eq!(message, "And the tests?");

    // esc returns to the review with the dock open; ctrl+k focuses it, and focused, closes it.
    setup.scenario.key("esc");
    let screen = stable(&mut setup.scenario);
    assert!(screen.contains("ASSISTANT_ANSWER"));
    setup.scenario.keys(&["ctrl+k", "ctrl+k"]);
    let screen = stable(&mut setup.scenario);
    assert!(!screen.contains("ASSISTANT_ANSWER"), "{}", screen.text());
}

#[test]
fn the_dock_sits_under_the_review_when_the_body_is_narrow() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 100, 40);
    setup.scenario.key("ctrl+k");
    let screen = stable(&mut setup.scenario);
    let title = screen.find("Assistant").expect("dock title");
    let review = screen.find("Review").expect("review title");
    assert!(title.y > review.y + 5, "{title:?} under {review:?}");
    // Too short for a bottom panel: no dock, and focus stays usable in the review.
    setup.scenario.resize(100, 16);
    let screen = stable(&mut setup.scenario);
    assert!(
        !screen.contains("Ask about this review."),
        "{}",
        screen.text()
    );
    setup.scenario.key("j");
    stable(&mut setup.scenario);
}

#[test]
fn the_dock_answers_its_sessions_permission_prompt_with_the_feed_keys() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 160, 40);
    let (session, message) = ask_assistant(&mut setup, "Draft a comment here.");
    let mut asking = snapshot(vec![user(&message)]);
    asking.runtime.active_turn_id = Some("turn-1".to_owned());
    asking.permissions.push(PermissionView {
        permission_id: "permission-1".to_owned(),
        session_id: None,
        tool_call_id: "call-1".to_owned(),
        tool_name: "code_review.add_comment".to_owned(),
        producer_plugin_id: Some("bcode.code_review".to_owned()),
        arguments_json: r#"{"path":"a.rs","line":3,"body":"Rename this"}"#.to_owned(),
        batch: None,
        agent_id: "build".to_owned(),
        title: None,
        policy_source: None,
        detail: Some("build agent asks before code_review.add_comment".to_owned()),
        resolved: false,
        approved: None,
        can_remember: true,
    });
    let screen = send(&mut setup, &session, asking);
    let text = screen.text();
    assert!(screen.contains("? Use code_review.add_comment?"), "{text}");
    assert!(screen.contains("y allow"), "{text}");
    // The prompt replaces the input, so `y` answers it rather than typing.
    setup.scenario.key("y");
    stable(&mut setup.scenario);
    assert_eq!(
        setup.scenario.host().permission_answers(),
        vec![("permission-1".to_owned(), true, false)]
    );
    assert!(stable(&mut setup.scenario).contains("answering…"));
}

#[test]
fn the_review_follows_what_the_sessions_review_tools_change() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 160, 50);
    let (path, line) = first_added(&setup);
    let (session, message) = ask_assistant(&mut setup, "Review this file.");
    // A navigation already finished when the session is first observed is not replayed.
    let navigate = format!(r#"{{"path":"{path}","line":{line}}}"#);
    let before = cursor(&setup.scenario);
    send(
        &mut setup,
        &session,
        snapshot(vec![
            tool("old-nav", "code_review.navigate", &navigate),
            user(&message),
        ]),
    );
    assert_eq!(cursor(&setup.scenario), before);

    // The tools write through the store the review reads, as the plugin does.
    let services = setup.backend.services();
    let mut data = bcode_code_review::ReviewData::default();
    let mut tools = ReviewTools::new(&mut data, services.store());
    let review = tools
        .open(
            &setup.workspace.repo_root,
            &ReviewRef::Session(session.clone()),
        )
        .expect("bound review");
    tools
        .add_comment(
            &review,
            &Location {
                path: Some(path.clone()),
                line: Some(line),
                end_line: None,
                side: Side::New,
                source: None,
            },
            "TOOL_DRAFTED_COMMENT",
            None,
        )
        .expect("drafted");
    tools
        .mark_viewed(&review, std::slice::from_ref(&path), true)
        .expect("viewed");
    let mut items = vec![
        tool("old-nav", "code_review.navigate", &navigate),
        user(&message),
        tool("write-1", "code_review.add_comment", "{}"),
        tool("view-1", "code_review.mark_viewed", "{}"),
    ];
    let screen = send(&mut setup, &session, snapshot(items.clone()));
    assert!(screen.contains("TOOL_DRAFTED_COMMENT"), "{}", screen.text());
    let viewed = {
        let doc = setup.scenario.surface().document().expect("document");
        let file = doc
            .file_keys()
            .find(|file| file.path == path)
            .cloned()
            .expect("file");
        doc.viewed_state(&file)
            .is_some_and(bcode_code_review::viewed::ViewedState::is_viewed)
    };
    assert!(viewed, "the file shows as viewed");

    // A new navigation moves the cursor to the line.
    setup.scenario.key("g");
    stable(&mut setup.scenario);
    items.push(tool("nav-2", "code_review.navigate", &navigate));
    send(&mut setup, &session, snapshot(items));
    match cursor(&setup.scenario) {
        ItemKey::Line {
            file,
            side,
            line: at,
        } => {
            assert_eq!(
                (file.path.as_str(), side, at),
                (path.as_str(), ReviewLineSide::New, line)
            );
        }
        ItemKey::LinePair { file, new, .. } => {
            assert_eq!((file.path.as_str(), new), (path.as_str(), Some(line)));
        }
        other => panic!("cursor moved elsewhere: {other:?}"),
    }
}
