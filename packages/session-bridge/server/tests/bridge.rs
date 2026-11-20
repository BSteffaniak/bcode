//! The session bridge end to end: a real in-process daemon with the fake provider, the bridge
//! serving it, and an application speaking the bridge protocol through the thin client.

mod support;

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use bcode_session_bridge::models::{
    AgentProfile, BridgeEvent, ContextBlock, Origin, OriginFilter, SessionView, ToolCallStatus,
    ToolDescriptor, ToolOutcome, ToolPolicy, TurnOutcome, VersionRange, ViewItemKind,
};
use bcode_session_bridge::{BridgeClient, BridgeEvents};
use bcode_session_bridge_server::serve;
use support::{WAIT, bridge, connect, invocation, next_event, options, start_daemon};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn origin() -> Origin {
    Origin {
        app_id: "app".into(),
        display_name: Some("App".into()),
        context: std::iter::once(("connection".to_owned(), "replica".to_owned())).collect(),
        background: false,
    }
}

fn tool(name: &str, policy: ToolPolicy) -> ToolDescriptor {
    ToolDescriptor {
        name: name.into(),
        description: "Read the application's live state".into(),
        input_schema: serde_json::json!({"type": "object"}),
        policy,
        max_duration_ms: 10_000,
    }
}

fn profile() -> AgentProfile {
    AgentProfile {
        id: "app.reader".into(),
        name: "Reader".into(),
        description: String::new(),
        base_agent: "build".into(),
        instructions: "Use the app's tools.".into(),
        tools: vec!["app.lookup".into(), "app.change".into()],
    }
}

/// Sessions matching `filter` once the catalog lists `expected` of them. A fresh store's
/// catalog is degraded until its first discovery completes, as an application refreshing a
/// list sees.
async fn listed(app: &BridgeClient, filter: OriginFilter, expected: usize) -> Vec<String> {
    tokio::time::timeout(WAIT, async {
        loop {
            let page = app
                .list_sessions(Some(filter.clone()), None, None, 10)
                .await
                .expect("list");
            if page.sessions.len() == expected {
                break page
                    .sessions
                    .into_iter()
                    .map(|session| session.id)
                    .collect();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the catalog lists the sessions")
}

/// Observe until a snapshot satisfies `done`.
async fn observed(events: &mut BridgeEvents, done: impl Fn(&SessionView) -> bool) -> SessionView {
    next_event(events, |event| match event {
        BridgeEvent::Observation { view, .. } if done(&view) => Some(*view),
        _ => None,
    })
    .await
}

fn answered(view: &SessionView) -> bool {
    !view.turn_active
        && view
            .items
            .iter()
            .any(|item| item.kind == ViewItemKind::Assistant && !item.streaming)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_application_negotiates_any_version_inside_the_bridge_window() {
    let daemon = start_daemon(Vec::new()).await;
    // A newer bridge that still accepts the two versions before its own.
    let window = VersionRange { min: 1, max: 3 };
    let (app, _events) = connect(&daemon, window, VersionRange::exactly(1))
        .await
        .expect("an older application connects");
    assert_eq!(app.hello().version, 1);
    assert_eq!(app.hello().bridge_versions, window);
    let listed = app
        .list_sessions(None, None, None, 10)
        .await
        .expect("an older application uses sessions");
    assert!(listed.sessions.is_empty());

    let too_new = connect(&daemon, window, VersionRange { min: 4, max: 5 })
        .await
        .expect_err("outside the window");
    assert_eq!(too_new.code(), Some("version_mismatch"));
    assert!(too_new.to_string().ends_with("update Bcode"), "{too_new}");

    let too_old = connect(
        &daemon,
        VersionRange { min: 2, max: 3 },
        VersionRange::exactly(1),
    )
    .await
    .expect_err("older than the window");
    assert!(
        too_old.to_string().ends_with("update the application"),
        "{too_old}"
    );
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_before_hello_and_unknown_fields_are_refused() {
    let daemon = start_daemon(Vec::new()).await;
    let (app_side, bridge_side) = tokio::io::duplex(64 * 1024);
    let (bridge_read, bridge_write) = tokio::io::split(bridge_side);
    tokio::spawn(serve(
        bridge_read,
        bridge_write,
        daemon.client.clone(),
        options(VersionRange::supported()),
    ));
    let (app_read, mut app_write) = tokio::io::split(app_side);
    let mut lines = BufReader::new(app_read).lines();
    let mut exchange = async |line: &str| {
        app_write.write_all(line.as_bytes()).await.expect("write");
        app_write.write_all(b"\n").await.expect("write");
        let reply = lines.next_line().await.expect("read").expect("reply");
        serde_json::from_str::<serde_json::Value>(&reply).expect("json")
    };
    let early = exchange(r#"{"id":1,"request":{"op":"list_agents"}}"#).await;
    assert_eq!(early["error"]["code"], "hello_required");
    let hello = exchange(
        r#"{"id":2,"request":{"op":"hello","client_name":"raw","versions":{"min":1,"max":1}}}"#,
    )
    .await;
    assert_eq!(hello["ok"]["version"], 1);
    let unknown =
        exchange(r#"{"id":3,"request":{"op":"summary","session_id":"x","surprise":true}}"#).await;
    assert_eq!(unknown["id"], 3);
    assert_eq!(unknown["error"]["code"], "invalid_request");
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_app_session_runs_its_tools_and_is_observed_listed_and_continued() {
    let daemon = start_daemon(Vec::new()).await;
    let (app, mut events) = bridge(&daemon).await;
    app.register_tools(
        "app",
        vec![tool("app.lookup", ToolPolicy::ReadOnly)],
        vec![profile()],
    )
    .await
    .expect("register");
    let session = app
        .create_session(
            Some("orders".into()),
            daemon.workspace(),
            Some(origin()),
            None,
            None,
        )
        .await
        .expect("create");
    assert_eq!(
        session.origin.as_ref().map(|o| o.app_id.as_str()),
        Some("app")
    );

    let filter = OriginFilter {
        app_id: Some("app".into()),
        context: std::iter::once(("connection".to_owned(), "replica".to_owned())).collect(),
    };
    assert_eq!(
        listed(&app, filter, 1).await,
        std::slice::from_ref(&session.id)
    );

    let observation = app.observe(&session.id, 50).await.expect("observe");
    let (turn, queued) = app
        .submit(
            &session.id,
            r#"tool-call app.lookup {"key":"answer"}"#,
            Vec::new(),
            None,
        )
        .await
        .expect("submit");
    assert!(!turn.is_empty());
    assert!(!queued);
    let call = invocation(&mut events).await;
    assert_eq!(call.tool_name, "app.lookup");
    assert_eq!(call.session_id, session.id);
    app.tool_result(
        &call.invocation_id,
        ToolOutcome::Completed {
            content: "live value: 42".into(),
        },
    )
    .await
    .expect("answer");
    let view = observed(&mut events, answered).await;
    let tool = view
        .items
        .iter()
        .find_map(|item| item.tool.as_ref())
        .expect("tool item");
    assert_eq!(tool.name, "app.lookup");
    assert_eq!(tool.status, ToolCallStatus::Finished);
    assert_eq!(tool.result.as_deref(), Some("live value: 42"));
    assert_eq!(
        view.last_turn.as_ref().map(|turn| turn.outcome),
        Some(TurnOutcome::Completed)
    );
    app.stop_observing(&observation).await.expect("stop");

    // Bounded history and the other session reads.
    let history = app
        .history_page(&session.id, None, 20)
        .await
        .expect("history");
    let kinds = history
        .items
        .iter()
        .map(|item| item.kind)
        .collect::<BTreeSet<_>>();
    assert!(kinds.contains(&ViewItemKind::User), "{kinds:?}");
    assert!(kinds.contains(&ViewItemKind::Tool), "{kinds:?}");
    assert!(kinds.contains(&ViewItemKind::Assistant), "{kinds:?}");
    let renamed = app.rename(&session.id, "renamed").await.expect("rename");
    assert_eq!(renamed.title.as_deref(), Some("renamed"));
    assert_eq!(
        app.summary(&session.id)
            .await
            .expect("summary")
            .title
            .as_deref(),
        Some("renamed")
    );
    app.usage(&session.id).await.expect("usage");
    let agents = app.list_agents().await.expect("agents");
    assert!(agents.iter().any(|agent| agent.id == "app.reader"));
    assert!(!app.list_models().await.expect("models").is_empty());
    assert_eq!(
        app.open_command(&session.id).await.expect("argv"),
        ["/opt/bcode/bin/bcode", "tui", session.id.as_str()]
    );

    // The same session is an ordinary Bcode session for every other client.
    let listed = daemon
        .client
        .session_summary(session.id.parse().expect("id"))
        .await
        .expect("bcode sees it");
    assert_eq!(
        listed.origin.map(|origin| origin.app_id),
        Some("app".into())
    );
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mutating_tools_ask_and_any_client_answers_through_the_bridge() {
    let daemon = start_daemon(Vec::new()).await;
    let (app, mut events) = bridge(&daemon).await;
    app.register_tools(
        "app",
        vec![tool("app.change", ToolPolicy::Mutating)],
        Vec::new(),
    )
    .await
    .expect("register");
    let session = app
        .create_session(None, daemon.workspace(), Some(origin()), None, None)
        .await
        .expect("create");
    app.submit(&session.id, r#"tool-call app.change {}"#, Vec::new(), None)
        .await
        .expect("submit");
    let pending = tokio::time::timeout(WAIT, async {
        loop {
            let pending = app
                .pending_permissions(Some(&session.id))
                .await
                .expect("pending");
            if let Some(permission) = pending.into_iter().next() {
                break permission;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the call asks first");
    assert_eq!(pending.tool_name, "app.change");
    app.resolve_permission(&pending.permission_id, true, false)
        .await
        .expect("approve");
    let call = invocation(&mut events).await;
    assert_eq!(call.tool_name, "app.change");
    app.tool_result(
        &call.invocation_id,
        ToolOutcome::Failed {
            code: "read_only_database".into(),
            message: "the connection is read-only".into(),
        },
    )
    .await
    .expect("answer");
    let again = app
        .resolve_permission(&pending.permission_id, true, false)
        .await
        .expect_err("already answered");
    assert_eq!(again.code(), Some("permission_not_found"));
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_turn_cancels_the_app_call_and_a_released_session_stops_offering_tools() {
    let daemon = start_daemon(Vec::new()).await;
    let (app, mut events) = bridge(&daemon).await;
    app.register_tools(
        "app",
        vec![tool("app.lookup", ToolPolicy::ReadOnly)],
        Vec::new(),
    )
    .await
    .expect("register");
    let session = app
        .create_session(None, daemon.workspace(), Some(origin()), None, None)
        .await
        .expect("create");
    app.observe(&session.id, 50).await.expect("observe");
    app.submit(&session.id, "tool-call app.lookup {}", Vec::new(), None)
        .await
        .expect("submit");
    let call = invocation(&mut events).await;

    // The application has not answered; cancelling the turn tells it to stop the call.
    app.cancel_turn(&session.id).await.expect("cancel");
    let cancelled = next_event(&mut events, |event| match event {
        BridgeEvent::ToolCancel { invocation_id } => Some(invocation_id),
        _ => None,
    })
    .await;
    assert_eq!(cancelled, call.invocation_id);
    let ended = observed(&mut events, |view| {
        !view.turn_active && view.last_turn.is_some()
    })
    .await;
    assert_eq!(
        ended.last_turn.map(|turn| turn.outcome),
        Some(TurnOutcome::Cancelled)
    );
    // A late answer for the ended call is accepted and dropped.
    app.tool_result(
        &call.invocation_id,
        ToolOutcome::Completed {
            content: "too late".into(),
        },
    )
    .await
    .expect("late answer");

    // Once released, the session no longer reaches the application's tools.
    app.release_session(&session.id).await.expect("release");
    app.submit(&session.id, "tool-call app.lookup {}", Vec::new(), None)
        .await
        .expect("submit again");
    let view = observed(&mut events, |view| {
        !view.turn_active
            && view
                .items
                .iter()
                .filter_map(|item| item.tool.as_ref())
                .filter(|tool| tool.name == "app.lookup")
                .count()
                == 2
    })
    .await;
    let second = view
        .items
        .iter()
        .filter_map(|item| item.tool.as_ref())
        .rfind(|tool| tool.name == "app.lookup")
        .expect("second call");
    assert!(second.is_error, "{second:?}");
    assert_ne!(second.result.as_deref(), Some("too late"));
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn context_blocks_and_profiles_pass_through_and_fail_closed_after_close() {
    let daemon = start_daemon(Vec::new()).await;
    let (app, mut events) = bridge(&daemon).await;
    app.register_tools(
        "app",
        vec![tool("app.lookup", ToolPolicy::ReadOnly)],
        vec![profile()],
    )
    .await
    .expect("register");
    let session = app
        .create_session(
            None,
            daemon.workspace(),
            Some(origin()),
            Some("app.reader".into()),
            None,
        )
        .await
        .expect("create with the app's profile");
    app.observe(&session.id, 20).await.expect("observe");
    app.submit(
        &session.id,
        "echo-all question",
        vec![
            ContextBlock {
                label: "screen".into(),
                text: "orders table".into(),
                request_only: false,
            },
            ContextBlock {
                label: "selection".into(),
                text: "row 7 of 20".into(),
                request_only: true,
            },
        ],
        None,
    )
    .await
    .expect("submit");
    let view = observed(&mut events, answered).await;
    let answer = view
        .items
        .iter()
        .rev()
        .find(|item| item.kind == ViewItemKind::Assistant)
        .expect("answer");
    assert!(answer.text.contains("orders table"), "{}", answer.text);
    assert!(answer.text.contains("row 7 of 20"), "{}", answer.text);
    let user = view
        .items
        .iter()
        .find(|item| item.kind == ViewItemKind::User)
        .expect("question");
    assert_eq!(user.text, "echo-all question");

    // Closing the bridge drops the application's registration and with it the profile; a turn
    // that selects the profile then fails closed instead of running under another agent.
    app.close().await;
    let (other, _events) = bridge(&daemon).await;
    let refused = tokio::time::timeout(WAIT, async {
        loop {
            match other
                .submit(&session.id, "hello", Vec::new(), Some("app.reader".into()))
                .await
            {
                Err(error) if error.code() == Some("agent_profile_unavailable") => break error,
                _ => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    })
    .await
    .expect("the session's profile fails closed");
    assert!(refused.to_string().contains("app.reader"), "{refused}");
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sdk_conversations_import_once_and_list_with_their_origin() {
    let daemon = start_daemon(Vec::new()).await;
    let (app, _events) = bridge(&daemon).await;
    let file = daemon.directory.path().join("conversation.json");
    write_conversation(&file);
    let (session, warnings) = app
        .import_sdk_session(
            file.display().to_string(),
            daemon.workspace(),
            Some(origin()),
        )
        .await
        .expect("import");
    assert!(warnings.is_empty(), "{warnings:?}");
    let (again, _) = app
        .import_sdk_session(
            file.display().to_string(),
            daemon.workspace(),
            Some(origin()),
        )
        .await
        .expect("import again");
    assert_eq!(again.id, session.id);
    let filter = OriginFilter {
        app_id: Some("app".into()),
        context: std::collections::BTreeMap::new(),
    };
    assert_eq!(
        listed(&app, filter, 1).await,
        std::slice::from_ref(&session.id)
    );
    app.delete(&session.id).await.expect("delete");
    daemon.stop().await;
}

fn write_conversation(path: &Path) {
    let conversation = serde_json::json!({
        "session_id": "5f0b6c1e-2222-4f5c-9e6b-0123456789ab",
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "How many orders shipped?"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "Twelve."}]}
        ]
    });
    std::fs::write(path, serde_json::to_vec(&conversation).expect("json")).expect("write");
}
