//! The live feed of AI cards: asking stays in the review, and the card follows the linked
//! session (streaming answer, reasoning, tool lines, permission prompts, stop, errors).

use super::interaction::{Setup, added_line, cursor, find, fixture, move_to, setup};
use super::stable;
use crate::action::Action;
use bcode_code_review::document::ItemKey;
use bcode_session_models::ModelTurnOutcome;
use bcode_session_view::follow::SessionViewFollowUpdate;
use bcode_session_view_models::{
    ChatMessageView, PermissionView, SessionConnectionViewStatus, SessionViewSnapshot,
    ToolInvocationView, ToolInvocationViewStatus, ToolTimingView, TranscriptViewItem,
    TranscriptViewItemId, TranscriptViewItemKind,
};

/// Ask `question` about the first added line; returns the linked session and the prompt sent.
fn ask(setup: &mut Setup, question: &str) -> (String, String) {
    let index = find(&setup.scenario, 0, added_line);
    move_to(&mut setup.scenario, index);
    setup.scenario.key("a");
    setup.scenario.paste(question);
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    let (session, prompt) = setup.backend.messages().last().cloned().expect("asked");
    (session, prompt)
}

fn item(id: &str, streaming: bool, kind: TranscriptViewItemKind) -> TranscriptViewItem {
    TranscriptViewItem {
        id: TranscriptViewItemId::new(id),
        revision: 1,
        sequence: Some(1),
        timestamp_ms: Some(1),
        output_location: None,
        streaming,
        kind,
    }
}

fn user(prompt: &str) -> TranscriptViewItem {
    item(
        "user",
        false,
        TranscriptViewItemKind::UserMessage {
            message: ChatMessageView::markdown(prompt),
        },
    )
}

fn assistant(text: &str, streaming: bool) -> TranscriptViewItem {
    item(
        "assistant",
        streaming,
        TranscriptViewItemKind::AssistantMessage {
            message: ChatMessageView::markdown(text),
        },
    )
}

fn reasoning(text: &str) -> TranscriptViewItem {
    item(
        "reasoning",
        false,
        TranscriptViewItemKind::ReasoningMessage {
            message: ChatMessageView::markdown(text),
        },
    )
}

fn tool(
    status: ToolInvocationViewStatus,
    arguments: &str,
    result: Option<&str>,
) -> TranscriptViewItem {
    item(
        "tool",
        false,
        TranscriptViewItemKind::ToolInvocation {
            tool: Box::new(ToolInvocationView {
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: None,
                tool_name: Some("shell".to_owned()),
                arguments_json: Some(arguments.to_owned()),
                working_directory: None,
                request_draft: None,
                status,
                result_text: result.map(str::to_owned),
                is_error: Some(false),
                result: None,
                presentation: None,
                timing: ToolTimingView::default(),
            }),
        },
    )
}

fn permission(id: &str) -> PermissionView {
    PermissionView {
        permission_id: id.to_owned(),
        session_id: None,
        tool_call_id: "call-1".to_owned(),
        tool_name: "shell".to_owned(),
        producer_plugin_id: None,
        arguments_json: r#"{"command":"rm -rf target"}"#.to_owned(),
        batch: None,
        agent_id: "build".to_owned(),
        title: Some("Permission requested".to_owned()),
        policy_source: None,
        detail: Some("shell commands ask first".to_owned()),
        resolved: false,
        approved: None,
        can_remember: true,
    }
}

fn snapshot(items: Vec<TranscriptViewItem>, running: bool) -> SessionViewSnapshot {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.connection_status = SessionConnectionViewStatus::Attached;
    snapshot.transcript.items = items;
    if running {
        snapshot.runtime.active_turn_id = Some("turn-1".to_owned());
    }
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
        .expect("subscribed")
        .try_send(SessionViewFollowUpdate::Snapshot(Box::new(snapshot)))
        .expect("delivered");
    stable(&mut setup.scenario)
}

#[test]
fn asking_streams_reasoning_tools_and_the_answer_into_the_card() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 120, 60);
    let (session, prompt) = ask(&mut setup, "Why this change?");
    assert!(matches!(
        cursor(&setup.scenario),
        ItemKey::AgentThread { .. }
    ));
    let mut running = snapshot(
        vec![
            user(&prompt),
            reasoning("weighing the rename against the callers"),
            tool(
                ToolInvocationViewStatus::Running,
                r#"{"command":"cargo test -p review"}"#,
                None,
            ),
            assistant("The change **renames** the", true),
        ],
        true,
    );
    running.runtime.cumulative_metered_tokens = 12_345;
    running
        .runtime
        .cost
        .totals_micros
        .insert("USD".to_owned(), 12_345);
    let screen = send(&mut setup, &session, running);
    let text = screen.text();
    // The question as asked (not the whole prompt), reasoning collapsed, the running tool, and
    // the streamed text, with usage in the card's title.
    assert!(screen.contains("▍you"), "{text}");
    assert!(screen.contains("▍ Why this change?"), "{text}");
    assert!(!screen.contains("You are helping"), "{text}");
    assert!(screen.contains("▸ thinking · 6 words"), "{text}");
    assert!(screen.contains("◌ shell cargo test -p review"), "{text}");
    assert!(screen.contains("The change renames the"), "{text}");
    assert!(screen.contains("working…"), "{text}");
    assert!(screen.contains("12.3k tokens"), "{text}");
    assert!(screen.contains("$0.0123"), "{text}");
    assert!(screen.contains("Stop esc"), "{text}");
    assert!(screen.contains("Open in Bcode o"), "{text}");
    assert!(
        !setup
            .scenario
            .actions()
            .iter()
            .any(|action| matches!(action, Action::OpenSession { .. }))
    );
    // A spinner frame was scheduled while the session works.
    assert!(setup.scenario.host().delayed_redraw_requests() > 0);

    // The turn finishes: the tool's last output lines and an expander, the whole answer.
    let output: Vec<String> = (1..=10).map(|line| format!("out line {line}")).collect();
    let screen = send(
        &mut setup,
        &session,
        snapshot(
            vec![
                user(&prompt),
                reasoning("weighing the rename against the callers"),
                tool(
                    ToolInvocationViewStatus::Finished,
                    r#"{"command":"cargo test -p review"}"#,
                    Some(&output.join("\n")),
                ),
                assistant("The change **renames** the handler.", false),
            ],
            false,
        ),
    );
    let text = screen.text();
    assert!(screen.contains("Bcode answered"), "{text}");
    assert!(screen.contains("✓ shell"), "{text}");
    assert!(screen.contains("out line 10"), "{text}");
    assert!(screen.contains("out line 8"), "{text}");
    assert!(!screen.contains("out line 7"), "{text}");
    assert!(screen.contains("The change renames the handler."), "{text}");
    assert!(!screen.contains("Stop esc"), "{text}");
    assert!(screen.contains("Follow up"), "{text}");
    // `All 10 lines` shows every line; the reasoning opens on a click as well.
    let all = screen.find("All 10 lines").expect("expander");
    setup.scenario.click(all.x, all.y);
    let screen = stable(&mut setup.scenario);
    assert!(screen.contains("out line 1\u{20}"), "{}", screen.text());
    assert!(screen.contains("Fewer lines"), "{}", screen.text());
    let thinking = screen.find("thinking · 6 words").expect("reasoning");
    setup.scenario.click(thinking.x, thinking.y);
    let screen = stable(&mut setup.scenario);
    assert!(screen.contains("▾ thinking"), "{}", screen.text());
    assert!(
        screen.contains("weighing the rename against the callers"),
        "{}",
        screen.text()
    );
}

#[test]
fn permission_requests_are_answered_from_the_card() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 120, 60);
    let (session, prompt) = ask(&mut setup, "Clean the build?");
    let mut asking = snapshot(
        vec![
            user(&prompt),
            tool(
                ToolInvocationViewStatus::Waiting,
                r#"{"command":"rm -rf target"}"#,
                None,
            ),
        ],
        true,
    );
    asking.permissions.push(permission("perm-1"));
    let screen = send(&mut setup, &session, asking.clone());
    let text = screen.text();
    assert!(screen.contains("Bcode needs you"), "{text}");
    assert!(screen.contains("? Use shell?"), "{text}");
    assert!(screen.contains("rm -rf target"), "{text}");
    assert!(screen.contains("shell commands ask first"), "{text}");
    for button in ["Allow y", "Always a", "Decline n", "Never d"] {
        assert!(screen.contains(button), "{button}: {text}");
    }
    // The prompt replaces the follow-up actions.
    assert!(!screen.contains("Follow up"), "{text}");
    // `y` allows once; the prompt waits for the session to apply the answer.
    setup.scenario.key("y");
    let screen = stable(&mut setup.scenario);
    assert_eq!(
        setup.scenario.host().permission_answers(),
        [("perm-1".to_owned(), true, false)]
    );
    assert!(screen.contains("answering…"), "{}", screen.text());
    assert!(!screen.contains("Allow y"), "{}", screen.text());
    // Another request: `d` declines it and remembers the rule.
    asking.permissions = vec![permission("perm-2")];
    send(&mut setup, &session, asking);
    setup.scenario.key("d");
    stable(&mut setup.scenario);
    assert_eq!(
        setup.scenario.host().permission_answers()[1],
        ("perm-2".to_owned(), false, true)
    );
    // Without a pending request the same keys keep their review meaning: `a` asks again.
    let screen = send(
        &mut setup,
        &session,
        snapshot(vec![user(&prompt), assistant("Cleaned.", false)], false),
    );
    assert!(!screen.contains("? Use shell?"), "{}", screen.text());
    setup.scenario.key("a");
    stable(&mut setup.scenario);
    assert!(setup.scenario.surface().composer().is_some());
}

#[test]
fn stop_cancels_the_running_turn_and_errors_show_in_the_card() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 120, 60);
    let (session, prompt) = ask(&mut setup, "Explain the handler");
    send(
        &mut setup,
        &session,
        snapshot(vec![user(&prompt), assistant("It starts by", true)], true),
    );
    setup.scenario.key("esc");
    stable(&mut setup.scenario);
    assert_eq!(
        setup.scenario.host().cancelled_turns(),
        std::slice::from_ref(&session)
    );
    assert!(
        !setup
            .scenario
            .actions()
            .iter()
            .any(|action| matches!(action, Action::Close { .. }))
    );
    let mut stopped = snapshot(vec![user(&prompt), assistant("It starts by", false)], false);
    stopped.runtime.last_turn_outcome = Some(ModelTurnOutcome::Cancelled);
    let screen = send(&mut setup, &session, stopped);
    assert!(screen.contains("Bcode stopped"), "{}", screen.text());
    assert!(screen.contains("· stopped"), "{}", screen.text());
    // Off a working card, `esc` is the review's again.
    setup.scenario.key("esc");
    stable(&mut setup.scenario);
    assert_eq!(setup.scenario.host().cancelled_turns().len(), 1);

    // A failed turn shows its error in the card.
    let mut failed = snapshot(vec![user(&prompt)], false);
    failed.runtime.last_turn_outcome = Some(ModelTurnOutcome::Error);
    failed.runtime.last_turn_message = Some("provider exploded".to_owned());
    let screen = send(&mut setup, &session, failed);
    assert!(screen.contains("Bcode failed"), "{}", screen.text());
    assert!(screen.contains("⚠ provider exploded"), "{}", screen.text());
}

#[test]
fn an_off_screen_card_that_needs_the_reviewer_raises_a_toast() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 100, 16);
    let (session, prompt) = ask(&mut setup, "Is this safe?");
    // Visible cards raise no toast.
    send(
        &mut setup,
        &session,
        snapshot(vec![user(&prompt), assistant("Checking", true)], true),
    );
    assert_eq!(setup.scenario.surface().toasts(), Vec::<&str>::new());
    setup.scenario.keys(&["pagedown", "pagedown"]);
    stable(&mut setup.scenario);
    let mut asking = snapshot(vec![user(&prompt)], true);
    asking.permissions.push(permission("perm-1"));
    let screen = send(&mut setup, &session, asking);
    assert!(!screen.contains("Use shell?"), "{}", screen.text());
    assert!(
        setup
            .scenario
            .surface()
            .toasts()
            .iter()
            .any(|toast| toast.contains("Bcode needs your permission to use shell")),
        "{:?}",
        setup.scenario.surface().toasts()
    );
}

#[test]
fn a_stopped_stream_keeps_the_answer_and_drops_the_prompt() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 120, 60);
    let (session, prompt) = ask(&mut setup, "Why rename it?");
    let mut answered = snapshot(
        vec![
            user(&prompt),
            assistant("Callers expect the new name.", false),
        ],
        false,
    );
    answered.permissions.push(permission("perm-1"));
    send(&mut setup, &session, answered);
    setup
        .scenario
        .host()
        .session_view(&session)
        .expect("subscribed")
        .try_send(SessionViewFollowUpdate::Disconnected {
            message: "daemon went away".to_owned(),
        })
        .expect("delivered");
    let screen = stable(&mut setup.scenario);
    let text = screen.text();
    assert!(screen.contains("stream stopped"), "{text}");
    assert!(screen.contains("Callers expect the new name."), "{text}");
    assert!(screen.contains("⚠ daemon went away"), "{text}");
    // The request can no longer be answered here, so its keys are the review's again.
    assert!(!screen.contains("Allow y"), "{text}");
    setup.scenario.key("y");
    stable(&mut setup.scenario);
    assert_eq!(
        setup.scenario.host().permission_answers(),
        Vec::<(String, bool, bool)>::new()
    );
}

#[test]
fn deleting_a_question_stops_following_its_session() {
    let fixture = fixture();
    let mut setup = setup(&fixture, 120, 60);
    let (session, prompt) = ask(&mut setup, "Is this needed?");
    let screen = send(
        &mut setup,
        &session,
        snapshot(vec![user(&prompt), assistant("No.", false)], false),
    );
    let follow = setup
        .scenario
        .host()
        .session_view(&session)
        .expect("subscribed");
    assert!(!follow.is_closed());
    let delete = screen.find("Delete").expect("delete button");
    setup.scenario.click(delete.x, delete.y);
    let screen = stable(&mut setup.scenario);
    assert!(!screen.contains("Is this needed?"), "{}", screen.text());
    assert!(follow.is_closed(), "the session is still followed");
}
