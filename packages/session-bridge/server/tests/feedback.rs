//! Feedback through the session bridge: an application records feedback, its own tools are
//! offered to the intake agent, and it attaches what the agent asked for.
//!
//! This is its own test binary because it points the process-wide state location at the
//! test daemon's directory, where the feedback plugin keeps its store.

mod support;

use bcode_session_bridge::models::bcode_feedback_models::{
    AttachFeedbackRequest, AttachmentKind, Facet, FeedbackQuery, RecordFeedbackRequest, TriageState,
};
use bcode_session_bridge::models::{ToolDescriptor, ToolOutcome, ToolPolicy, VersionRange};
use support::{bridge, connect, invocation, start_daemon};

fn capture_tool() -> ToolDescriptor {
    ToolDescriptor {
        name: "test-app.capture".into(),
        description: "Capture the application's recent activity".into(),
        input_schema: serde_json::json!({"type": "object"}),
        policy: ToolPolicy::ReadOnly,
        max_duration_ms: 10_000,
    }
}

fn feedback_plugin() -> (bcode_plugin::StaticBundledPlugin, &'static str) {
    (
        bcode_plugin::StaticBundledPlugin::from_definition(
            include_str!("../../../../plugins/feedback-plugin/bcode-plugin.toml"),
            bcode_feedback_plugin::plugin_definition,
        ),
        "bcode.feedback",
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn an_app_records_feedback_offers_its_tools_to_triage_and_attaches_the_result() {
    let daemon = start_daemon(vec![feedback_plugin()]).await;
    let locations = bcode_config::resolve_state_location_set_with_environment(
        &bcode_config::StateConfig::default(),
        &bcode_config::StateLocationSelection {
            root: Some(daemon.directory.path().join("state")),
            profile: None,
        },
        &bcode_config::ConfigEnvironmentSnapshot::new(
            std::collections::BTreeMap::new(),
            daemon.directory.path().to_path_buf(),
        ),
    )
    .expect("locations");
    let _state = bcode_config::push_process_state_location(locations.primary());

    let (app, mut events) = bridge(&daemon).await;
    app.register_tools("test-app", vec![capture_tool()], Vec::new())
        .await
        .expect("register");

    // The fake model calls whatever tool the message names: here, the app's own tool.
    let submission = app
        .feedback_submit(
            RecordFeedbackRequest {
                app: "test-app".into(),
                text: r#"tool-call test-app.capture {"seconds":3}"#.into(),
                facets: vec![
                    Facet::new("test-app.pane", "2"),
                    Facet {
                        key: "test-app.token".into(),
                        value: "hunter2".into(),
                        sensitive: true,
                    },
                ],
            },
            Some("test-app.capture records the last seconds of the pane.".into()),
            true,
        )
        .await
        .expect("submit");
    assert_eq!(submission.triage_error, None);
    let intake = submission.intake.expect("triage started");
    let item = submission.item;
    assert_eq!(item.app, "test-app");
    assert_eq!(item.triage_state, TriageState::Pending);

    let call = invocation(&mut events).await;
    assert_eq!(call.tool_name, "test-app.capture");
    assert_eq!(
        call.session_id, intake.session_id,
        "the claimed intake session offers the app's tools"
    );
    app.tool_result(
        &call.invocation_id,
        ToolOutcome::Completed {
            content: "captured 3 seconds".into(),
        },
    )
    .await
    .expect("answer");

    let clip = daemon.directory.path().join("clip.txt");
    std::fs::write(&clip, b"MARKER-B\n").expect("clip");
    let attached = app
        .feedback_attach(AttachFeedbackRequest {
            id: item.id,
            source: clip.display().to_string(),
            name: "clip.txt".into(),
            media_type: "text/plain".into(),
            description: Some("the pane around the feedback".into()),
            sensitive: false,
        })
        .await
        .expect("attach");
    assert_eq!(attached.attachments.len(), 1);
    assert_eq!(attached.attachments[0].kind, AttachmentKind::File);
    let state = std::fs::canonicalize(daemon.workspace()).expect("workspace");
    assert!(
        std::path::Path::new(&attached.attachments[0].path).starts_with(&state),
        "the store lives in the daemon's state location: {}",
        attached.attachments[0].path
    );

    let read = app.feedback_get(item.id).await.expect("get");
    assert_eq!(read.attachments, attached.attachments);
    assert!(
        read.facets.iter().any(|facet| facet.value == "hunter2"),
        "the application, acting for the person, sees its own sensitive values"
    );
    let page = app
        .feedback_list(FeedbackQuery {
            app: Some("test-app".into()),
            ..FeedbackQuery::default()
        })
        .await
        .expect("list");
    assert_eq!(page.items.len(), 1);

    // A second submission reuses the same intake session.
    let again = app
        .feedback_submit(
            RecordFeedbackRequest {
                app: "test-app".into(),
                text: "a second, unrelated item".into(),
                facets: Vec::new(),
            },
            None,
            true,
        )
        .await
        .expect("second submit");
    assert_eq!(
        again.intake.map(|intake| intake.session_id),
        Some(intake.session_id)
    );
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn feedback_requests_need_protocol_two() {
    let daemon = start_daemon(Vec::new()).await;
    let (app, _events) = connect(&daemon, VersionRange::supported(), VersionRange::exactly(1))
        .await
        .expect("protocol 1 connects");
    let refused = app
        .feedback_list(FeedbackQuery::default())
        .await
        .expect_err("protocol 1 has no feedback requests");
    assert!(
        refused.to_string().contains("unsupported_request"),
        "{refused}"
    );
    daemon.stop().await;
}
