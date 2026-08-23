#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Manual HTML/Actix acceptance fixture with representative long and streaming content.

use std::sync::Arc;

use bcode_session_models::{SessionId, SessionSummary, SessionTitleSource};
use bcode_session_view_models::{
    ChatMessageView, SessionConnectionViewStatus, SessionViewSnapshot, TranscriptViewItem,
    TranscriptViewItemId, TranscriptViewItemKind,
};
use hyperchad::app::renderer::DefaultRenderer;
use hyperchad::app::{App, AppBuilder};

fn acceptance_snapshot(session_id: SessionId) -> SessionViewSnapshot {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.session_id = Some(session_id);
    snapshot.title = Some("HyperChad acceptance fixture".to_owned());
    snapshot.working_directory = Some("/workspace/renderer-acceptance".into());
    snapshot.latest_sequence = Some(500);
    snapshot.connection_status = SessionConnectionViewStatus::Attached;
    snapshot.composer.can_submit = true;
    "Draft preserved while reviewing older history".clone_into(&mut snapshot.composer.draft);
    snapshot.transcript.has_older_history = true;
    snapshot.transcript.has_newer_history = true;
    snapshot.transcript.source_start_sequence = Some(1);
    snapshot.transcript.source_end_sequence = Some(500);
    snapshot.transcript.items = (0..500)
        .map(|index| TranscriptViewItem {
            output_location: None,
            id: TranscriptViewItemId::new(format!("acceptance:{index}")),
            revision: 1,
            sequence: Some(index + 1),
            timestamp_ms: Some(index + 1),
            streaming: index == 499,
            kind: if index.is_multiple_of(2) {
                TranscriptViewItemKind::UserMessage {
                    message: ChatMessageView::markdown(format!(
                        "## Prompt {index}\n\nLong unbroken content must reflow without page overflow: {}",
                        "abcdefghijklmnopqrstuvwxyz".repeat(20)
                    )),
                }
            } else {
                TranscriptViewItemKind::AssistantMessage {
                    message: ChatMessageView::markdown(format!(
                        "### Streamed response {index}\n\n* Live content\n* History controls\n* Keyboard controls\n\n{}",
                        "Representative answer content. ".repeat(30)
                    )),
                }
            },
        })
        .collect();
    snapshot
}

fn acceptance_session(session_id: SessionId) -> SessionSummary {
    SessionSummary {
        id: session_id,
        name: Some("HyperChad acceptance fixture".to_owned()),
        explicit_name: Some("HyperChad acceptance fixture".to_owned()),
        derived_title: None,
        title_source: SessionTitleSource::Explicit,
        client_count: 1,
        created_at_ms: 1,
        updated_at_ms: 500,
        working_directory: "/workspace/renderer-acceptance".into(),
        import: None,
        execution: None,
        location: None,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let session_id = SessionId::new();
    let snapshot = acceptance_snapshot(session_id);
    let sessions = vec![acceptance_session(session_id)];
    let token: Arc<str> = Arc::from("manual-acceptance-token");
    let builder: AppBuilder = bcode_hyperchad::init_with_snapshot(snapshot, sessions)
        .with_router(bcode_hyperchad::router(
            acceptance_snapshot(session_id),
            vec![acceptance_session(session_id)],
        ))
        .with_actix_bind_address("127.0.0.1".to_owned())
        .with_actix_port(43128)
        .with_actix_on_bound(move |address| {
            println!(
                "{}",
                bcode_hyperchad::build_launch_url(address, &token, Some(session_id))
            );
        });
    let (app, renderer_runtime): (App<DefaultRenderer>, _) =
        bcode_hyperchad::build_app_with_runtime(builder)?;
    app.handle_serve()?;
    drop(renderer_runtime);
    Ok(())
}
