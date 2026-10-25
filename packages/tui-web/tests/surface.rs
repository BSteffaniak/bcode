use bcode_session_view_models::{
    ChatMessageView, SessionViewSnapshot, TranscriptViewItem, TranscriptViewItemId,
    TranscriptViewItemKind,
};
use bcode_tui_web::SessionSurface;

const SESSION: &str = "00000000-0000-4000-8000-000000000001";
const EPOCH: &str = "00000000-0000-4000-8000-000000000002";

#[test]
fn committed_navigation_click_keyboard_and_failed_output_share_one_scene() {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.session_id = Some(SESSION.parse().unwrap());
    snapshot.transcript.has_older_history = true;
    snapshot.transcript.has_newer_history = true;
    snapshot.transcript.source_start_sequence = Some(10);
    snapshot.transcript.source_end_sequence = Some(20);
    let json =
        serde_json::json!({"schema_version":1,"epoch":EPOCH,"revision":1,"snapshot":snapshot})
            .to_string();
    let mut surface = SessionSurface::new(SESSION.into(), 100, 12).unwrap();
    surface.accept_snapshot(&json).unwrap();
    let mut older = None;
    surface
        .present::<()>(|frame| {
            older = frame
                .hits()
                .regions()
                .iter()
                .find(|region| region.id.as_str() == "bcode-web/history-older")
                .map(|region| region.area);
            Ok(())
        })
        .unwrap();
    let area = older.unwrap();
    for (action, buttons) in [("down", 1), ("up", 0)] {
        surface.input(&serde_json::json!({"type":"pointer","action":action,"x":area.x,"y":area.y,"button":0,"buttons":buttons}).to_string()).unwrap();
    }
    let intents: serde_json::Value =
        serde_json::from_str(&surface.take_intents().unwrap()).unwrap();
    assert_eq!(
        intents,
        serde_json::json!([{"type":"history","direction":"older"}])
    );
    surface
        .configure_bindings(r#"{"ctrl+l":"latest"}"#)
        .unwrap();
    surface
        .input(r#"{"type":"key","key":"l","code":"KeyL","ctrl":true}"#)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&surface.take_intents().unwrap()).unwrap(),
        serde_json::json!([{"type":"history","direction":"tail"}])
    );
    let committed = surface.accessibility().unwrap();
    snapshot.transcript.has_older_history = false;
    snapshot.transcript.has_newer_history = false;
    surface
        .accept_snapshot(
            &serde_json::json!({"schema_version":1,"epoch":EPOCH,"revision":2,"snapshot":snapshot})
                .to_string(),
        )
        .unwrap();
    assert!(surface.present(|_| Err::<(), _>("output failed")).is_err());
    assert_eq!(surface.accessibility().unwrap(), committed);
    surface.resize(0, 0).unwrap();
    surface
        .present::<()>(|frame| {
            assert!(frame.hits().regions().is_empty());
            Ok(())
        })
        .unwrap();
    assert!(!surface.accessibility().unwrap().contains("Load older"));
    surface.resize(100, 12).unwrap();
    surface
        .present::<()>(|frame| {
            assert!(
                frame
                    .hits()
                    .regions()
                    .iter()
                    .any(|region| region.id.as_str() == "bcode-web/editor")
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn permission_uses_shared_decisions_and_never_activates_uncommitted_identity() {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.session_id = Some(SESSION.parse().unwrap());
    let permission = bcode_session_view_models::PermissionView {
        permission_id: "p1".into(),
        session_id: snapshot.session_id,
        tool_call_id: "call".into(),
        tool_name: "tool".into(),
        arguments_json: "{}".into(),
        batch: Some(bcode_session_view_models::PermissionBatchView {
            batch_id: "batch".into(),
            call_index: 0,
            call_count: 1,
        }),
        agent_id: "build".into(),
        title: None,
        policy_source: None,
        detail: None,
        resolved: false,
        approved: None,
        can_remember: false,
    };
    snapshot.permissions.push(permission);
    let mut surface = SessionSurface::new(SESSION.into(), 100, 30).unwrap();
    surface
        .configure_bindings(r#"{"ctrl+y":"permission_approve","ctrl+n":"permission_deny"}"#)
        .unwrap();
    surface
        .accept_snapshot(
            &serde_json::json!({"schema_version":1,"epoch":EPOCH,"revision":1,"snapshot":snapshot})
                .to_string(),
        )
        .unwrap();
    surface
        .input(r#"{"type":"key","key":"y","code":"KeyY","ctrl":true}"#)
        .unwrap();
    assert_eq!(surface.take_intents().unwrap(), "[]");
    let mut approve = None;
    surface
        .present::<()>(|frame| {
            let decisions = frame
                .hits()
                .regions()
                .iter()
                .filter(|region| region.id.as_str().starts_with("bcode-web/permission/"))
                .collect::<Vec<_>>();
            assert_eq!(decisions.len(), 2); // Batch mutation deliberately unavailable without canonical CAS.
            approve = Some(decisions[0].area);
            Ok(())
        })
        .unwrap();
    let area = approve.unwrap();
    for (action, buttons) in [("down", 1), ("up", 0)] {
        surface.input(&serde_json::json!({"type":"pointer","action":action,"x":area.x,"y":area.y,"button":0,"buttons":buttons}).to_string()).unwrap();
    }
    let intents: serde_json::Value =
        serde_json::from_str(&surface.take_intents().unwrap()).unwrap();
    let action: serde_json::Value =
        serde_json::from_str(intents[0]["action_json"].as_str().unwrap()).unwrap();
    assert_eq!(
        action,
        serde_json::json!({"type":"permission","permission_id":"p1","approved":true,"remember":false})
    );
    surface
        .input(r#"{"type":"key","key":"y","code":"KeyY","ctrl":true}"#)
        .unwrap();
    assert_eq!(surface.take_intents().unwrap(), "[]");
    snapshot.permissions[0].permission_id = "p2".into();
    surface
        .accept_snapshot(
            &serde_json::json!({"schema_version":1,"epoch":EPOCH,"revision":2,"snapshot":snapshot})
                .to_string(),
        )
        .unwrap();
    assert!(surface.present(|_| Err::<(), _>("output failed")).is_err());
    surface
        .input(r#"{"type":"key","key":"y","code":"KeyY","ctrl":true}"#)
        .unwrap();
    assert_eq!(surface.take_intents().unwrap(), "[]");
    surface.present::<()>(|_| Ok(())).unwrap();
    surface
        .input(r#"{"type":"key","key":"n","code":"KeyN","ctrl":true}"#)
        .unwrap();
    let intents: serde_json::Value =
        serde_json::from_str(&surface.take_intents().unwrap()).unwrap();
    assert!(intents[0]["action_json"].as_str().unwrap().contains("p2"));
}

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
    let recovery = surface.export_recovery().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&recovery).unwrap();
    assert!(parsed["editor"].get("content_area").is_none());
    let mut remounted = SessionSurface::new(SESSION.into(), 1, 1).unwrap();
    remounted.restore_recovery(&recovery).unwrap();
    assert_eq!(remounted.draft(), surface.draft());
    assert!(remounted.restore_recovery(&recovery).is_err());
    let mut malformed = parsed;
    malformed["editor"]["cursor"] = serde_json::json!(1);
    let mut fresh = SessionSurface::new(SESSION.into(), 1, 1).unwrap();
    assert!(fresh.restore_recovery(&malformed.to_string()).is_err());
    assert!(fresh.draft().is_empty());
    for (width, height) in [(0, 0), (1, 1), (2, 2), (80, 20)] {
        surface.resize(width, height).unwrap();
        surface.present::<()>(|_| Ok(())).unwrap();
    }
}
