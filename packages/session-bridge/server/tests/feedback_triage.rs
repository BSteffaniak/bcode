//! Durable feedback triage: an application's feedback is triaged even when the intake agent
//! cannot take it at submission, after the application closed and the daemon restarted, and
//! exactly once.
//!
//! This is its own test binary because it points the process-wide state location at each
//! test's directory: the feedback plugin keeps its store there, and its dispatcher reaches the
//! daemon at that location's default endpoint, where these tests serve it. Tests take turns.

mod support;

use std::path::Path;
use std::time::Duration;

use bcode_client::BcodeClient;
use bcode_feedback_models::{
    FeedbackId, FeedbackItem, GetFeedbackRequest, IntakeSession, LeaseTriageRequest,
    OP_INTAKE_SESSION, OP_LEASE_TRIAGE, RecordFeedbackRequest, RecordForTriageRequest,
    TriageDispatchState, TriageIntent, TriageLease, TriageState,
};
use bcode_session_bridge::models::{ToolDescriptor, ToolPolicy, VersionRange};
use bcode_session_bridge::{BridgeClient, BridgeEvents, FeedbackSubmission};
use bcode_session_bridge_server::serve;
use bcode_session_models::{
    SessionEventKind, SessionHistoryDirection, SessionHistoryQuery, SessionId,
};
use support::{Served, options, private_directory, serve_daemon, state_location};

/// How long the daemon's dispatcher gets to triage recovered items.
const RECOVERY: Duration = Duration::from_secs(90);

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A state root this process now resolves as its own, and the endpoint its daemon serves.
struct Root {
    directory: tempfile::TempDir,
    endpoint: bcode_ipc::IpcEndpoint,
    _state: bcode_config::StateLocationGuard,
}

fn root() -> Root {
    for variable in ["BCODE_IPC_ENDPOINT", "BCODE_SOCKET"] {
        assert!(
            std::env::var_os(variable).is_none(),
            "{variable} would point these tests at another daemon; unset it"
        );
    }
    let directory = tempfile::tempdir().expect("directory");
    let state = bcode_config::push_process_state_location(&state_location(directory.path()));
    let endpoint = bcode_ipc::default_endpoint();
    let socket = endpoint.as_unix_socket().expect("unix socket endpoint");
    private_directory(socket.parent().expect("socket directory"));
    Root {
        directory,
        endpoint,
        _state: state,
    }
}

impl Root {
    async fn daemon(&self, provider: bool) -> Served {
        serve_daemon(
            self.directory.path(),
            self.endpoint.clone(),
            plugins(),
            provider,
        )
        .await
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }
}

fn plugins() -> Vec<(bcode_plugin::StaticBundledPlugin, &'static str)> {
    vec![
        (
            bcode_plugin::StaticBundledPlugin::from_definition(
                include_str!("../../../../plugins/feedback-plugin/bcode-plugin.toml"),
                bcode_feedback_plugin::plugin_definition,
            ),
            "bcode.feedback",
        ),
        // Its agents may triage feedback without asking.
        (
            bcode_plugin::StaticBundledPlugin::from_definition(
                include_str!("../../../../plugins/default-agents-plugin/bcode-plugin.toml"),
                bcode_default_agents_plugin::plugin_definition,
            ),
            "bcode.default-agents",
        ),
    ]
}

async fn bridge(client: &BcodeClient) -> (BridgeClient, BridgeEvents) {
    let (app_side, bridge_side) = tokio::io::duplex(1024 * 1024);
    let (bridge_read, bridge_write) = tokio::io::split(bridge_side);
    tokio::spawn(serve(
        bridge_read,
        bridge_write,
        client.clone(),
        options(VersionRange::supported()),
    ));
    let (app_read, app_write) = tokio::io::split(app_side);
    BridgeClient::connect(app_read, app_write, "test-app", VersionRange::supported())
        .await
        .expect("bridge")
}

fn capture_tool() -> ToolDescriptor {
    ToolDescriptor {
        name: "test-app.capture".into(),
        description: "Capture the application's recent activity".into(),
        input_schema: serde_json::json!({"type": "object"}),
        policy: ToolPolicy::ReadOnly,
        max_duration_ms: 10_000,
    }
}

/// Feedback whose text makes the fake model triage item `id` with `title`.
fn triaging(id: u64, title: &str) -> RecordFeedbackRequest {
    RecordFeedbackRequest {
        app: "test-app".into(),
        text: format!(r#"tool-call feedback.triage {{"id":"fb-{id}","title":"{title}"}}"#),
        facets: Vec::new(),
    }
}

async fn item(client: &BcodeClient, id: FeedbackId) -> FeedbackItem {
    bcode_feedback_client::get(client, &GetFeedbackRequest { id })
        .await
        .expect("get")
}

async fn wait_until_triaged(client: &BcodeClient, ids: &[FeedbackId]) -> Vec<FeedbackItem> {
    let deadline = tokio::time::Instant::now() + RECOVERY;
    loop {
        let mut items = Vec::new();
        for id in ids {
            items.push(item(client, *id).await);
        }
        if items
            .iter()
            .all(|item| item.triage_state == TriageState::Triaged)
        {
            return items;
        }
        if tokio::time::Instant::now() >= deadline {
            let summary: Vec<String> = history(client)
                .await
                .iter()
                .map(|kind| format!("{kind:?}").chars().take(220).collect())
                .collect();
            panic!("not triaged in time: {items:#?}\nintake history: {summary:#?}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn intake_session(client: &BcodeClient) -> Option<SessionId> {
    let session: IntakeSession = bcode_feedback_client::call(client, OP_INTAKE_SESSION, &())
        .await
        .expect("intake session");
    session.session_id.map(|id| id.parse().expect("session id"))
}

/// Every event of the intake session's recent history.
async fn history(client: &BcodeClient) -> Vec<SessionEventKind> {
    let Some(session_id) = intake_session(client).await else {
        return Vec::new();
    };
    client
        .session_history_page(
            session_id,
            SessionHistoryQuery {
                cursor: None,
                limit: bcode_session_models::MAX_SESSION_HISTORY_READ_EVENTS,
                direction: SessionHistoryDirection::Backward,
            },
        )
        .await
        .expect("history")
        .events
        .into_iter()
        .map(|event| event.kind)
        .collect()
}

/// How many times the intake agent called `feedback.triage` for `id`.
fn triage_calls(history: &[SessionEventKind], id: FeedbackId) -> usize {
    let needle = format!(r#""id":"{id}""#);
    history
        .iter()
        .filter(|kind| match kind {
            SessionEventKind::ToolCallRequested {
                tool_name,
                arguments_json,
                ..
            }
            | SessionEventKind::PositionedToolCallRequested {
                tool_name,
                arguments_json,
                ..
            } => tool_name == "feedback.triage" && arguments_json.contains(&needle),
            _ => false,
        })
        .count()
}

/// The intake turns admitted for item `id`.
fn admitted(history: &[SessionEventKind], id: FeedbackId) -> usize {
    let id = id.to_string();
    history
        .iter()
        .filter(|kind| {
            matches!(kind, SessionEventKind::SubmissionAccepted { submission }
                if submission
                    .admission
                    .origin
                    .as_ref()
                    .and_then(|origin| origin.correlation_id.as_deref())
                    == Some(id.as_str()))
        })
        .count()
}

fn state(submission: &FeedbackSubmission) -> TriageDispatchState {
    submission
        .item
        .triage_dispatch
        .as_ref()
        .expect("the triage request is saved with the item")
        .state
}

#[tokio::test(flavor = "multi_thread")]
async fn feedback_given_during_an_intake_outage_is_triaged_once_after_the_app_closes_and_the_daemon_restarts()
 {
    let _serial = SERIAL.lock().await;
    let root = root();

    // The intake agent's model provider is missing: no intake turn can triage anything.
    let outage = root.daemon(false).await;
    let (app, events) = bridge(&outage.client).await;
    app.register_tools("test-app", vec![capture_tool()], Vec::new())
        .await
        .expect("register");
    let instructions = Some("test-app.capture records the last seconds of the pane.".to_owned());
    let first = app
        .feedback_submit(triaging(1, "First"), instructions.clone(), true)
        .await
        .expect("first submit");
    let second = app
        .feedback_submit(triaging(2, "Second"), instructions, true)
        .await
        .expect("second submit");
    let ids = [first.item.id, second.item.id];
    assert_eq!(ids, [FeedbackId(1), FeedbackId(2)]);
    for submission in [&first, &second] {
        assert_eq!(submission.item.triage_state, TriageState::Pending);
        assert!(
            matches!(
                state(submission),
                TriageDispatchState::Queued | TriageDispatchState::Started
            ),
            "{submission:#?}"
        );
    }

    // The application closes, then its daemon stops, while the provider is still out.
    drop(events);
    drop(app);
    for id in ids {
        let item = item(&outage.client, id).await;
        assert_eq!(item.triage_state, TriageState::Pending, "{item:#?}");
    }
    outage.stop().await;

    // A restarted daemon, whose provider is back, triages both without anyone asking.
    let restored = root.daemon(true).await;
    let items = wait_until_triaged(&restored.client, &ids).await;
    assert_eq!(
        items
            .iter()
            .map(|item| item.headline().to_owned())
            .collect::<Vec<_>>(),
        ["First", "Second"]
    );
    for item in &items {
        let dispatch = item.triage_dispatch.as_ref().expect("request");
        assert_eq!(dispatch.state, TriageDispatchState::Completed, "{item:#?}");
    }
    let history = history(&restored.client).await;
    for id in ids {
        assert_eq!(
            triage_calls(&history, id),
            1,
            "{id} is triaged exactly once"
        );
    }
    assert!(root.path().join("state").exists());
    restored.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn skipped_triage_stays_untriaged_until_a_person_asks() {
    let _serial = SERIAL.lock().await;
    let root = root();
    let daemon = root.daemon(true).await;
    let (app, _events) = bridge(&daemon.client).await;
    let recorded = app
        .feedback_record(triaging(1, "Later"))
        .await
        .expect("record");
    assert_eq!(recorded.intake, None);
    assert_eq!(recorded.triage_error, None);
    assert_eq!(state(&recorded), TriageDispatchState::Skipped);

    // Several dispatcher passes go by without delivering it.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let skipped = item(&daemon.client, recorded.item.id).await;
    assert_eq!(skipped.triage_state, TriageState::Pending);
    assert_eq!(
        skipped.triage_dispatch.map(|dispatch| dispatch.state),
        Some(TriageDispatchState::Skipped)
    );
    assert_eq!(
        triage_calls(&history(&daemon.client).await, recorded.item.id),
        0
    );

    // `feedback triage fb-1`: the person asks, and it is delivered.
    bcode_feedback_client::request_triage(&daemon.client, recorded.item.id)
        .await
        .expect("request");
    let delivery = bcode_feedback_client::deliver(
        &daemon.client,
        recorded.item.id,
        &bcode_feedback_client::holder("test"),
        None,
    )
    .await
    .expect("deliver");
    assert!(delivery.failure.is_none(), "{delivery:#?}");
    wait_until_triaged(&daemon.client, &[recorded.item.id]).await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delivery_whose_acknowledgement_was_lost_does_not_start_a_second_turn() {
    let _serial = SERIAL.lock().await;
    let root = root();
    let daemon = root.daemon(true).await;
    let client = &daemon.client;
    // The model answers without triaging, so the item stays deliverable.
    let item = bcode_feedback_client::record_for_triage(
        client,
        &RecordForTriageRequest {
            item: RecordFeedbackRequest {
                app: "test-app".into(),
                text: "the pane froze".into(),
                facets: Vec::new(),
            },
            triage: TriageIntent::Requested { instructions: None },
        },
    )
    .await
    .expect("record");

    // A dispatcher delivers it, and its turn is admitted, but the dispatcher never learns:
    // it records nothing and its (short) lease runs out.
    let lease: TriageLease = bcode_feedback_client::call::<Option<TriageLease>>(
        client,
        OP_LEASE_TRIAGE,
        &LeaseTriageRequest {
            id: item.id,
            holder: "lost".into(),
            lease_ms: 1,
        },
    )
    .await
    .expect("lease")
    .expect("queued");
    let session = bcode_feedback_client::ensure_intake_session(client)
        .await
        .expect("session");
    let lost = bcode_feedback_client::submit_generation(client, session, &lease.item, None, 1)
        .await
        .expect("admitted");
    tokio::time::sleep(Duration::from_millis(5)).await;

    // Another dispatcher delivers the same generation again.
    let retried = bcode_feedback_client::deliver(
        client,
        item.id,
        &bcode_feedback_client::holder("test"),
        None,
    )
    .await
    .expect("deliver");
    // The daemon's dispatcher may hold the item and redeliver it first; either way the
    // generation's turn is the one already admitted.
    let mut current = retried.item;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while current
        .triage_dispatch
        .as_ref()
        .is_none_or(|dispatch| dispatch.state != TriageDispatchState::Started)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "not started: {current:#?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        current = self::item(client, item.id).await;
    }
    let dispatch = current.triage_dispatch.expect("request");
    if let Some(receipt) = retried.intake {
        assert_eq!(
            receipt.turn_id, lost.turn_id,
            "the same turn, not a second one"
        );
    }
    assert_eq!(
        dispatch.turn_id.as_deref(),
        Some(lost.turn_id.0.as_str()),
        "the same turn, not a second one"
    );
    assert_eq!(dispatch.generation, 1);
    let history = history(client).await;
    assert_eq!(
        admitted(&history, item.id),
        1,
        "one intake turn for the generation: {history:#?}"
    );
    daemon.stop().await;
}
