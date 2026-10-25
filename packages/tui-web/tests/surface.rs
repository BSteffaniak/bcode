use bcode_session_view_models::{
    ChatMessageView, SessionViewSnapshot, TranscriptViewItem, TranscriptViewItemId,
    TranscriptViewItemKind,
};
use bcode_tui_web::SessionSurface;

const SESSION: &str = "00000000-0000-4000-8000-000000000001";
const EPOCH: &str = "00000000-0000-4000-8000-000000000002";

#[test]
fn markdown_snapshot_and_configured_editor_share_a_local_surface() {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.session_id = Some(SESSION.parse().unwrap());
    snapshot.transcript.items.push(TranscriptViewItem {
        id: TranscriptViewItemId::event(1),
        revision: 1,
        sequence: Some(1),
        timestamp_ms: None,
        output_location: None,
        streaming: false,
        kind: TranscriptViewItemKind::AssistantMessage {
            message: ChatMessageView::markdown("# Browser integration\n\nHello **world** 🦀"),
        },
    });
    let observation = serde_json::json!({
        "schema_version": 1, "epoch": EPOCH, "revision": 1, "snapshot": snapshot,
    })
    .to_string();
    let mut surface = SessionSurface::new(SESSION.into(), 80, 20).unwrap();
    assert!(surface.accept_snapshot(&observation).unwrap());
    surface
        .present::<()>(|frame| {
            assert_eq!(frame.buffer().area().width, 80);
            Ok(())
        })
        .unwrap();
    assert!(!surface.accept_snapshot(&observation).unwrap());
    surface
        .configure_bindings(r#"{"enter":"newline","ctrl+enter":"submit"}"#)
        .unwrap();
    assert!(
        !surface
            .input(r#"{"type":"text","text":"日本語🦀é"}"#)
            .unwrap()
    );
    assert!(
        !surface
            .input(r#"{"type":"key","key":"Backspace","code":"Backspace"}"#)
            .unwrap()
    );
    assert_eq!(surface.draft(), "日本語🦀");
    assert!(
        !surface
            .input(r#"{"type":"key","key":"Enter","code":"Enter"}"#)
            .unwrap()
    );
    assert_eq!(surface.draft(), "日本語🦀\n");
    assert!(
        surface
            .input(r#"{"type":"key","key":"Enter","code":"Enter","ctrl":true}"#)
            .unwrap()
    );
    for (width, height) in [(0, 0), (1, 1), (2, 2), (80, 20)] {
        surface.resize(width, height).unwrap();
        surface.present::<()>(|_| Ok(())).unwrap();
    }
}
