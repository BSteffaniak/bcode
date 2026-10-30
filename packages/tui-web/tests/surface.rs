use bcode_session_view_models::{
    ChatMessageView, SessionViewSnapshot, TranscriptViewItem, TranscriptViewItemId,
    TranscriptViewItemKind,
};
use bcode_tui_web::SessionSurface;

const SESSION: &str = "00000000-0000-4000-8000-000000000001";
const EPOCH: &str = "00000000-0000-4000-8000-000000000002";

#[test]
fn history_traversal_progresses_across_disjoint_windows_then_returns_to_tail() {
    let mut surface = SessionSurface::new(SESSION.into(), 40, 8).unwrap();
    let window = |revision, start, end, older, newer| {
        let mut snapshot = SessionViewSnapshot::empty();
        snapshot.session_id = Some(SESSION.parse().unwrap());
        snapshot.transcript.revision = revision;
        snapshot.transcript.source_start_sequence = Some(start);
        snapshot.transcript.source_end_sequence = Some(end);
        snapshot.transcript.has_older_history = older;
        snapshot.transcript.has_newer_history = newer;
        snapshot.transcript.items.push(TranscriptViewItem {
            id: TranscriptViewItemId::new(format!("window-{start}")),
            revision,
            sequence: Some(start),
            timestamp_ms: None,
            output_location: None,
            streaming: false,
            kind: TranscriptViewItemKind::AssistantMessage {
                message: ChatMessageView::markdown(format!(
                    "# Window {start}\n{}",
                    "content\n".repeat(20)
                )),
            },
        });
        serde_json::json!({"schema_version":1,"epoch":EPOCH,"revision":revision,"snapshot":snapshot}).to_string()
    };
    surface
        .accept_snapshot(&window(1, 100, 200, true, false))
        .unwrap();
    surface.present::<()>(|_| Ok(())).unwrap();
    for (revision, start, end) in [(2, 50, 99), (3, 1, 49)] {
        surface
            .accept_history(&window(revision, start, end, start > 1, true), "older")
            .unwrap();
        surface.present::<()>(|_| Ok(())).unwrap();
    }
    assert!(
        surface
            .accept_history(&window(4, 1, 49, false, true), "newer")
            .is_err()
    );
    surface
        .accept_history(&window(4, 50, 99, true, true), "newer")
        .unwrap();
    surface.present::<()>(|_| Ok(())).unwrap();
    surface
        .accept_history(&window(5, 100, 200, true, false), "tail")
        .unwrap();
    surface.present::<()>(|_| Ok(())).unwrap();
}

#[test]
fn canonical_artifact_image_is_admitted_to_real_bmux_scene_and_stale_resize_is_rejected() {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.session_id = Some(SESSION.parse().unwrap());
    snapshot.transcript.revision = 1;
    let item: TranscriptViewItem = serde_json::from_value(serde_json::json!({
        "id":"image-item", "revision":1,"sequence":null,"timestamp_ms":null,"streaming":false,
        "kind":{"type":"tool_invocation","tool":{
            "tool_call_id":"call","producer_plugin_id":"image","tool_name":"image","arguments_json":"{}","working_directory":null,
            "status":"finished","result_text":null,"is_error":false,"timing":{},
            "result":{"type":"artifact","artifact":{
                "artifact":{"artifact_id":"image-artifact","producer_plugin_id":"image","schema":"image","schema_version":1,"metadata":{},"refs":[{"key":"image","content_type":"image/png","byte_len":4}]},
                "generic_payload":{}
            }}
        }}
    })).unwrap();
    snapshot.transcript.items.push(item);
    let mut surface = SessionSurface::new(SESSION.into(), 80, 20).unwrap();
    surface
        .accept_snapshot(
            &serde_json::json!({"schema_version":1,"epoch":EPOCH,"revision":1,"snapshot":snapshot})
                .to_string(),
        )
        .unwrap();
    assert!(surface.prepare_image("image-item", "image"));
    surface.resize(40, 20).unwrap();
    assert!(!surface.accept_image(vec![255, 0, 0, 255], 1, 1));
    assert!(surface.prepare_image("image-item", "image"));
    assert!(!surface.accept_image(vec![255], 1, 1));
    assert!(surface.prepare_image("image-item", "image"));
    assert!(surface.accept_image(vec![255, 0, 0, 255], 1, 1));
    surface
        .present::<()>(|frame| {
            assert_eq!(frame.image_scene().placements().len(), 1);
            Ok(())
        })
        .unwrap();
    // Accepting an unrelated producer artifact must not evict decoded image A.
    snapshot.transcript.revision += 1;
    snapshot.transcript.items.push(serde_json::from_value(serde_json::json!({
        "id":"diff-b","revision":1,"sequence":null,"timestamp_ms":null,"output_location":null,"streaming":false,
        "kind":{"type":"tool_invocation","tool":{
            "tool_call_id":"diff-b-call","producer_plugin_id":"bcode.filesystem","tool_name":"edit","arguments_json":"{}","working_directory":null,
            "status":"finished","result_text":null,"is_error":false,"timing":{},
            "result":{"type":"artifact","artifact":{
                "artifact":{"artifact_id":"diff-b-artifact","producer_plugin_id":"bcode.filesystem","schema":"bcode.filesystem.batch","schema_version":1,"metadata":{},
                    "refs":[{"key":"file-0-old","content_type":"application/octet-stream","byte_len":4}]},
                "generic_payload":{"path":"b.rs","old_text":"old","new_text":"new"}
            }}
        }}
    })).unwrap());
    surface.resize(40, 60).unwrap();
    let policy = serde_json::json!({"schema_version":1,"revision":"image-plus-diff","styles":{},"bindings":[],
        "visual_policy":{"enabled_plugins":["bcode.filesystem"],"preferred":[],"disabled":[]}});
    surface.accept_snapshot(&serde_json::json!({"schema_version":1,"epoch":EPOCH,"revision":2,"snapshot":snapshot,"policy":policy}).to_string()).unwrap();
    // Policy installation can change styles; decode A against that exact policy.
    assert!(surface.prepare_image("image-item", "image"));
    assert!(surface.accept_image(vec![255, 0, 0, 255], 1, 1));
    let mut image_before = None;
    surface
        .present::<()>(|frame| {
            image_before = Some(frame.image_scene().placements().next().unwrap().clone());
            Ok(())
        })
        .unwrap();
    let claim: serde_json::Value =
        serde_json::from_str(&surface.next_visual_artifact(0).unwrap().unwrap()).unwrap();
    let response = serde_json::json!({"artifact_id":"diff-b-artifact","reference_key":"file-0-old","content_type":"application/octet-stream",
        "offset":0,"total_bytes":4,"bytes":[111,108,100,10],"reference_revision":1,"finalized":true}).to_string();
    assert!(
        surface
            .accept_visual_artifact_claim(claim["claim"].as_str().unwrap(), &response, 1)
            .unwrap()
    );
    surface
        .present::<()>(|frame| {
            assert_eq!(frame.image_scene().placements().len(), 1);
            assert_eq!(
                frame.image_scene().placements().next().unwrap(),
                image_before.as_ref().unwrap()
            );
            Ok(())
        })
        .unwrap();
    // Preservation is not blanket validity: changing A's semantic revision
    // still removes its decoded projection until a fresh decode is admitted.
    snapshot.transcript.revision += 1;
    snapshot.transcript.items[0].revision += 1;
    surface.accept_snapshot(&serde_json::json!({"schema_version":1,"epoch":EPOCH,"revision":3,"snapshot":snapshot,"policy":policy}).to_string()).unwrap();
    surface
        .present::<()>(|frame| {
            assert_eq!(frame.image_scene().placements().len(), 0);
            Ok(())
        })
        .unwrap();
    surface.resize(0, 0).unwrap();
    surface
        .present::<()>(|frame| {
            assert_eq!(frame.image_scene().placements().len(), 0);
            Ok(())
        })
        .unwrap();
}

#[test]
fn committed_markdown_links_and_code_copy_emit_narrow_platform_intents() {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.session_id = Some(SESSION.parse().unwrap());
    snapshot.transcript.revision = 1;
    snapshot.transcript.items.push(TranscriptViewItem {
        id: TranscriptViewItemId::new("link-item"),
        revision: 1,
        sequence: None,
        timestamp_ms: None,
        output_location: None,
        streaming: false,
        kind: TranscriptViewItemKind::AssistantMessage {
            message: ChatMessageView::markdown(
                "[safe](https://example.com)\n\n```rust\nlet x = 1;\n```",
            ),
        },
    });
    let mut surface = SessionSurface::new(SESSION.into(), 80, 20).unwrap();
    surface
        .accept_snapshot(
            &serde_json::json!({"schema_version":1,"epoch":EPOCH,"revision":1,"snapshot":snapshot})
                .to_string(),
        )
        .unwrap();
    let mut hits = Vec::new();
    surface
        .present::<()>(|frame| {
            hits = frame
                .hits()
                .regions()
                .iter()
                .filter(|hit| {
                    hit.id.as_str().starts_with("markdown:")
                        || hit.id.as_str().starts_with("transcript-copy:")
                })
                .map(|hit| (hit.id.as_str().to_owned(), hit.area))
                .collect();
            Ok(())
        })
        .unwrap();
    assert!(!hits.is_empty());
    for (id, area) in &hits {
        for (action, buttons) in [("down", 1), ("up", 0)] {
            surface.input(&serde_json::json!({"type":"pointer","action":action,"x":area.x,"y":area.y,"button":0,"buttons":buttons}).to_string()).unwrap();
        }
        let intents: serde_json::Value =
            serde_json::from_str(&surface.take_intents().unwrap()).unwrap();
        if id.starts_with("markdown:") {
            assert_eq!(intents[0]["type"], "navigate");
            assert_eq!(intents[0]["url"], "https://example.com/");
        } else {
            assert_eq!(intents[0]["type"], "clipboard");
            assert!(intents[0]["text"].as_str().unwrap().contains("let x = 1;"));
        }
    }
    // Even unchanged semantic items cannot reuse controls after a new local
    // preparation until its geometry has successfully committed.
    surface.resize(80, 20).unwrap();
    for (_, area) in &hits {
        for (action, buttons) in [("down", 1), ("up", 0)] {
            surface.input(&serde_json::json!({"type":"pointer","action":action,"x":area.x,"y":area.y,"button":0,"buttons":buttons}).to_string()).unwrap();
        }
    }
    assert_eq!(surface.take_intents().unwrap(), "[]");
    assert!(surface.present(|_| Err::<(), _>("output failed")).is_err());
    for (_, area) in &hits {
        for (action, buttons) in [("down", 1), ("up", 0)] {
            surface.input(&serde_json::json!({"type":"pointer","action":action,"x":area.x,"y":area.y,"button":0,"buttons":buttons}).to_string()).unwrap();
        }
    }
    assert_eq!(surface.take_intents().unwrap(), "[]");
}

#[test]
fn resolved_policy_editor_chord_and_unknown_enter_are_admitted_without_default_fallback() {
    use bcode_tui_web::{
        BrowserBinding, BrowserEditorCommand, BrowserMotion, BrowserPolicy, BrowserResolvedBinding,
        BrowserStroke,
    };
    let stroke = |key: &str, ctrl| BrowserStroke {
        key: key.into(),
        ctrl,
        alt: false,
        shift: false,
        meta: false,
    };
    let mut policy = BrowserPolicy {
        schema_version: 1,
        revision: "native-1".into(),
        ..BrowserPolicy::default()
    };
    policy.bindings = vec![
        BrowserBinding {
            scope: "chat".into(),
            chord: vec![stroke("Enter", false)],
            multi_tap: None,
            binding: BrowserResolvedBinding::Host {
                action: "unknown.action".into(),
            },
        },
        BrowserBinding {
            scope: "chat".into(),
            chord: vec![stroke("ArrowLeft", false)],
            multi_tap: None,
            binding: BrowserResolvedBinding::Editor {
                command: BrowserEditorCommand::Move(BrowserMotion::Left),
            },
        },
        BrowserBinding {
            scope: "chat".into(),
            chord: vec![stroke("x", true), stroke("s", true)],
            multi_tap: None,
            binding: BrowserResolvedBinding::Host {
                action: "tui.input.submitSteering".into(),
            },
        },
    ];
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.session_id = Some(SESSION.parse().unwrap());
    let mut surface = SessionSurface::new(SESSION.into(), 80, 12).unwrap();
    surface.accept_snapshot(&serde_json::json!({"schema_version":1,"epoch":EPOCH,"revision":1,"policy":policy,"snapshot":snapshot}).to_string()).unwrap();
    surface.present::<()>(|_| Ok(())).unwrap();
    surface.input(r#"{"type":"text","text":"abc"}"#).unwrap();
    assert!(
        !surface
            .input(r#"{"type":"key","key":"Enter","code":"Enter"}"#)
            .unwrap()
    );
    assert_eq!(surface.draft(), "abc");
    surface
        .input(r#"{"type":"key","key":"ArrowLeft","code":"ArrowLeft"}"#)
        .unwrap();
    surface.input(r#"{"type":"text","text":"X"}"#).unwrap();
    assert_eq!(surface.draft(), "abXc");
    assert!(
        !surface
            .input(r#"{"type":"key","key":"x","code":"KeyX","ctrl":true}"#)
            .unwrap()
    );
    assert!(
        surface
            .input(r#"{"type":"key","key":"s","code":"KeyS","ctrl":true}"#)
            .unwrap()
    );
}

#[test]
fn shared_question_requires_committed_snapshot_and_emits_only_canonical_semantic_input() {
    let request = bcode_question_plugin::parse_question_request(serde_json::json!({
        "questions": [{"question":"Choose 日本語🦀é", "options":[{"label":"One","value":"one"}], "custom":false}]
    })).unwrap();
    let controller =
        bcode_question_plugin::presentation::QuestionInteractionController::new(request);
    let question = controller.snapshot();
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.session_id = Some(SESSION.parse().unwrap());
    snapshot
        .interactions
        .push(bcode_session_view_models::InteractionViewSummary {
            interaction_id: "q1".into(),
            producer_id: Some("bcode.question".into()),
            exchange_schema: Some("bcode.question.request".into()),
            exchange_schema_version: Some(1),
            kind: "bcode.question".into(),
            tool_call_id: None,
            title: None,
            required: true,
            snapshot: Some(serde_json::to_value(question).unwrap()),
            state: Default::default(),
            status_detail: None,
            resolved: false,
            resolution: None,
        });
    let mut surface = SessionSurface::new(SESSION.into(), 80, 20).unwrap();
    let encode = |snapshot: &SessionViewSnapshot, revision| {
        serde_json::json!({
            "schema_version":1,"epoch":EPOCH,"revision":revision,"snapshot":snapshot
        })
        .to_string()
    };
    surface.accept_snapshot(&encode(&snapshot, 1)).unwrap();
    let activate = r#"{"type":"key","key":"Enter","code":"Enter"}"#;
    assert!(!surface.input(activate).unwrap());
    assert_eq!(surface.take_intents().unwrap(), "[]");
    surface.present::<()>(|_| Ok(())).unwrap();
    // A failed repaint of the same checkpoint must not revoke the input
    // authority of the still-visible committed question.
    assert!(
        surface
            .present(|_| Err::<(), _>("same-scene output failed"))
            .is_err()
    );
    assert!(!surface.input(activate).unwrap());
    let intents: serde_json::Value =
        serde_json::from_str(&surface.take_intents().unwrap()).unwrap();
    assert_eq!(intents.as_array().unwrap().len(), 1);
    let action: serde_json::Value =
        serde_json::from_str(intents[0]["action_json"].as_str().unwrap()).unwrap();
    assert_eq!(action["type"], "interaction");
    assert_eq!(action["interaction_id"], "q1");
    assert_eq!(action["input"]["type"], "activate");
    surface.input(activate).unwrap();
    assert_eq!(surface.take_intents().unwrap(), "[]");
    snapshot.interactions[0].interaction_id = "q2".into();
    surface.accept_snapshot(&encode(&snapshot, 2)).unwrap();
    assert!(surface.present(|_| Err::<(), _>("output failed")).is_err());
    surface.input(activate).unwrap();
    assert_eq!(surface.take_intents().unwrap(), "[]");
    for (width, height) in [(0, 0), (1, 1), (2, 2), (80, 20)] {
        surface.resize(width, height).unwrap();
        surface.present::<()>(|_| Ok(())).unwrap();
    }
    snapshot.interactions[0].resolved = true;
    surface.accept_snapshot(&encode(&snapshot, 3)).unwrap();
    surface.present::<()>(|_| Ok(())).unwrap();
    surface
        .input(r#"{"type":"text","text":"composer restored"}"#)
        .unwrap();
    assert_eq!(surface.draft(), "composer restored");
}

#[test]
fn committed_semantics_are_visible_bounded_and_cleared_on_zero_size() {
    let mut surface = SessionSurface::new(SESSION.into(), 80, 12).unwrap();
    surface.present::<()>(|_| Ok(())).unwrap();
    let metadata: serde_json::Value =
        serde_json::from_str(&surface.accessibility().unwrap()).unwrap();
    let regions = metadata["regions"].as_array().unwrap();
    assert!(regions.len() <= 256);
    for region in regions {
        let area = &region["area"];
        assert!(area["width"].as_u64().unwrap() > 0);
        assert!(area["height"].as_u64().unwrap() > 0);
        assert!(area["x"].as_u64().unwrap() + area["width"].as_u64().unwrap() <= 80);
        assert!(area["y"].as_u64().unwrap() + area["height"].as_u64().unwrap() <= 12);
    }
    let committed = surface.accessibility().unwrap();
    assert!(surface.present(|_| Err::<(), _>("failed output")).is_err());
    assert_eq!(surface.accessibility().unwrap(), committed);
    surface.resize(0, 0).unwrap();
    surface.present::<()>(|_| Ok(())).unwrap();
    let metadata: serde_json::Value =
        serde_json::from_str(&surface.accessibility().unwrap()).unwrap();
    assert!(metadata["regions"].as_array().unwrap().is_empty());
}

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
            assert_eq!(decisions.len(), 2); // Only complete-batch CAS choices are actionable.
            approve = Some(decisions[0].area);
            Ok(())
        })
        .unwrap();
    let area = approve.unwrap();
    let accessibility: serde_json::Value =
        serde_json::from_str(&surface.accessibility().unwrap()).unwrap();
    let labels: Vec<_> = accessibility["regions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|region| {
            region["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("bcode-web/permission/"))
        })
        .map(|region| region["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["approve batch", "deny batch"]);
    for (action, buttons) in [("down", 1), ("up", 0)] {
        surface.input(&serde_json::json!({"type":"pointer","action":action,"x":area.x,"y":area.y,"button":0,"buttons":buttons}).to_string()).unwrap();
    }
    let intents: serde_json::Value =
        serde_json::from_str(&surface.take_intents().unwrap()).unwrap();
    let action: serde_json::Value =
        serde_json::from_str(intents[0]["action_json"].as_str().unwrap()).unwrap();
    assert_eq!(
        action,
        serde_json::json!({"type":"permission_batch","batch_id":"batch","approved":true})
    );
    surface
        .input(r#"{"type":"key","key":"y","code":"KeyY","ctrl":true}"#)
        .unwrap();
    assert_eq!(surface.take_intents().unwrap(), "[]");
    // Individual shortcuts must never silently expand authority to the batch.
    surface
        .input(r#"{"type":"key","key":"n","code":"KeyN","ctrl":true}"#)
        .unwrap();
    assert_eq!(surface.take_intents().unwrap(), "[]");
    snapshot.permissions[0].permission_id = "p2".into();
    // This replacement is an individual checkpoint, so the shared deny shortcut
    // below can resolve it. Leaving batch membership here requests an individual
    // batch decision, which the canonical complete-batch CAS correctly rejects.
    snapshot.permissions[0].batch = None;
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
fn producer_copy_translation_requires_current_successfully_committed_scene() {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.session_id = Some(SESSION.parse().unwrap());
    snapshot.transcript.revision = 1;
    snapshot.transcript.items.push(serde_json::from_value(serde_json::json!({
        "id":"shell-action","revision":1,"sequence":null,"timestamp_ms":null,"output_location":null,"streaming":false,
        "kind":{"type":"tool_invocation","tool":{
            "tool_call_id":"shell-call","producer_plugin_id":"bcode.shell","tool_name":"shell","arguments_json":"{}","working_directory":null,
            "status":"finished","result_text":null,"is_error":false,"timing":{},
            "result":{"type":"artifact","artifact":{
                "artifact":{"artifact_id":"shell-result","producer_plugin_id":"bcode.shell","schema":"bcode.shell.run","schema_version":1,"metadata":{},
                    "refs":[{"key":"clean_output","content_type":"text/plain","metadata":{"semantic_preview":{"version":1,"command":"printf hello","text":"界🙂 hello\n","bytes":17,"truncated":false}}}]},
                "generic_payload":{}
            }}
        }}
    })).unwrap());
    let policy = serde_json::json!({"schema_version":1,"revision":"shell-actions","styles":{},"bindings":[],
        "visual_policy":{"enabled_plugins":["bcode.shell"],"preferred":[],"disabled":[]}});
    let envelope = |revision, snapshot: &SessionViewSnapshot| {
        serde_json::json!({
            "schema_version":1,"epoch":EPOCH,"revision":revision,"snapshot":snapshot,"policy":policy
        })
        .to_string()
    };
    let mut surface = SessionSurface::new(SESSION.into(), 80, 30).unwrap();
    surface.accept_snapshot(&envelope(1, &snapshot)).unwrap();
    let mut area = None;
    surface
        .present::<()>(|frame| {
            area = frame
                .hits()
                .regions()
                .iter()
                .find(|hit| hit.id.as_str().starts_with("transcript-action:"))
                .map(|hit| hit.area);
            Ok(())
        })
        .unwrap();
    let area = area.expect("real shell producer must expose a source-backed action");
    let click = |surface: &mut SessionSurface| {
        for (action, buttons) in [("down", 1), ("up", 0)] {
            surface.input(&serde_json::json!({"type":"pointer","action":action,"x":area.x,"y":area.y,"button":0,"buttons":buttons}).to_string()).unwrap();
        }
    };
    click(&mut surface);
    let intents: serde_json::Value =
        serde_json::from_str(&surface.take_intents().unwrap()).unwrap();
    assert_eq!(intents[0]["type"], "clipboard");
    assert!(!intents[0]["text"].as_str().unwrap().is_empty());
    snapshot.transcript.revision += 1;
    snapshot.transcript.items[0].revision += 1;
    surface.accept_snapshot(&envelope(2, &snapshot)).unwrap();
    click(&mut surface);
    assert_eq!(surface.take_intents().unwrap(), "[]");
    assert!(
        surface
            .present(|_| Err::<(), _>("failed producer output"))
            .is_err()
    );
    click(&mut surface);
    assert_eq!(surface.take_intents().unwrap(), "[]");
    surface.present::<()>(|_| Ok(())).unwrap();
    click(&mut surface);
    let intents: serde_json::Value =
        serde_json::from_str(&surface.take_intents().unwrap()).unwrap();
    assert_eq!(intents[0]["type"], "clipboard");
}

#[test]
fn producer_artifact_pages_pin_versions_and_stop_at_producer_budget() {
    for mismatch in [None, Some("reference_revision"), Some("finalized")] {
        let mut snapshot = SessionViewSnapshot::empty();
        snapshot.session_id = Some(SESSION.parse().unwrap());
        snapshot.transcript.items.push(serde_json::from_value(serde_json::json!({
            "id":"budget-item","revision":1,"sequence":null,"timestamp_ms":null,"streaming":false,
            "kind":{"type":"tool_invocation","tool":{
                "tool_call_id":"budget-call","producer_plugin_id":"bcode.filesystem","tool_name":"edit","arguments_json":"{}","working_directory":null,
                "status":"finished","result_text":null,"is_error":false,"timing":{},
                "result":{"type":"artifact","artifact":{
                    "artifact":{"artifact_id":"budget-artifact","producer_plugin_id":"bcode.filesystem","schema":"bcode.filesystem.batch","schema_version":1,"metadata":{},
                        "refs":[{"key":"file-0-old","content_type":"application/octet-stream","byte_len":1000000}]},"generic_payload":{}
                }}
            }}
        })).unwrap());
        let policy = serde_json::json!({"schema_version":1,"revision":"budget","styles":{},"bindings":[],
            "visual_policy":{"enabled_plugins":["bcode.filesystem"],"preferred":[],"disabled":[]}});
        let mut surface = SessionSurface::new(SESSION.into(), 80, 20).unwrap();
        surface.accept_snapshot(&serde_json::json!({"schema_version":1,"epoch":EPOCH,"revision":1,"snapshot":snapshot,"policy":policy}).to_string()).unwrap();
        let mut accepted = 0;
        for page in 0..2 {
            let claim: serde_json::Value =
                serde_json::from_str(&surface.next_visual_artifact(page).unwrap().unwrap())
                    .unwrap();
            let request: serde_json::Value =
                serde_json::from_str(claim["request_json"].as_str().unwrap()).unwrap();
            assert_eq!(request["offset"], page * 4096);
            assert_eq!(request["length"], 4096);
            let mut response = serde_json::json!({"artifact_id":"budget-artifact","reference_key":"file-0-old","content_type":"application/octet-stream",
                "offset":page*4096,"total_bytes":1000000,"bytes":vec![b'x';4096],"reference_revision":1,"finalized":true});
            if page == 1
                && let Some(field) = mismatch
            {
                response[field] = if field == "finalized" {
                    serde_json::json!(false)
                } else {
                    serde_json::json!(2)
                };
            }
            let result = surface.accept_visual_artifact_claim(
                claim["claim"].as_str().unwrap(),
                &response.to_string(),
                page,
            );
            if page == 1 && mismatch.is_some() {
                assert!(result.is_err());
            } else {
                assert!(result.unwrap());
                accepted += 1;
            }
        }
        assert_eq!(accepted, if mismatch.is_some() { 1 } else { 2 });
        assert!(surface.next_visual_artifact(10000).unwrap().is_none());
    }
}

#[test]
fn visual_discovery_has_exact_claims_bounded_retries_and_resize_fences() {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.session_id = Some(SESSION.parse().unwrap());
    let item: TranscriptViewItem = serde_json::from_value(serde_json::json!({
        "id":"diff-item","revision":1,"sequence":null,"timestamp_ms":null,"output_location":null,"streaming":false,
        "kind":{"type":"tool_invocation","tool":{
            "tool_call_id":"diff-call","producer_plugin_id":"bcode.filesystem","tool_name":"edit","arguments_json":"{}","working_directory":null,
            "status":"finished","result_text":null,"is_error":false,"timing":{},
            "result":{"type":"artifact","artifact":{
                "artifact":{"artifact_id":"diff-artifact","producer_plugin_id":"bcode.filesystem","schema":"bcode.filesystem.batch","schema_version":1,"metadata":{},
                    "refs":[{"key":"file-0-old","content_type":"application/octet-stream","byte_len":4}]},
                "generic_payload":{"path":"src/lib.rs","old_text":"old","new_text":"new"}
            }}
        }}
    })).unwrap();
    snapshot.transcript.items.push(item);
    let policy = serde_json::json!({"schema_version":1,"revision":"visual-policy","styles":{},"bindings":[],
        "visual_policy":{"enabled_plugins":["bcode.filesystem"],"preferred":[],"disabled":[]}});
    let mut surface = SessionSurface::new(SESSION.into(), 80, 20).unwrap();
    surface.accept_snapshot(&serde_json::json!({"schema_version":1,"epoch":EPOCH,"revision":1,"snapshot":snapshot,"policy":policy}).to_string()).unwrap();
    let claim: serde_json::Value = serde_json::from_str(
        &surface
            .next_visual_artifact(0)
            .unwrap()
            .expect("real filesystem adapter selects artifact"),
    )
    .unwrap();
    let request: serde_json::Value =
        serde_json::from_str(claim["request_json"].as_str().unwrap()).unwrap();
    assert_eq!(request["item_id"], "diff-item");
    assert_eq!(request["reference_key"], "file-0-old");
    assert_eq!(request["offset"], 0);
    assert!(surface.next_visual_artifact(1).unwrap().is_none());
    surface.fail_visual_artifact("foreign", 2);
    assert!(surface.next_visual_artifact(3).unwrap().is_none());
    surface.fail_visual_artifact(claim["claim"].as_str().unwrap(), 4);
    assert!(surface.next_visual_artifact(1003).unwrap().is_none());
    let retry: serde_json::Value =
        serde_json::from_str(&surface.next_visual_artifact(1004).unwrap().unwrap()).unwrap();
    assert_ne!(retry["claim"], claim["claim"]);
    assert_eq!(retry["request_json"], claim["request_json"]);
    surface.resize(40, 20).unwrap();
    let response = serde_json::json!({"artifact_id":"diff-artifact","reference_key":"file-0-old","content_type":"text/plain",
        "offset":0,"total_bytes":4,"bytes":[111,108,100,10],"reference_revision":1,"finalized":true}).to_string();
    assert!(
        surface
            .accept_visual_artifact_claim(retry["claim"].as_str().unwrap(), &response, 1005)
            .is_err()
    );
    assert!(surface.next_visual_artifact(100_000).unwrap().is_none());
}

#[test]
fn question_rejects_malformed_producer_checkpoint_and_cannot_submit_it() {
    let request = bcode_question_plugin::parse_question_request(serde_json::json!({
        "questions":[{"question":"Required","options":[{"label":"One"}],"custom":false}]
    }))
    .unwrap();
    let mut checkpoint = serde_json::to_value(
        bcode_question_plugin::presentation::QuestionInteractionController::new(request).snapshot(),
    )
    .unwrap();
    checkpoint["answers"] = serde_json::json!([]);
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.session_id = Some(SESSION.parse().unwrap());
    snapshot.interactions.push(serde_json::from_value(serde_json::json!({
        "interaction_id":"malformed","producer_id":"bcode.question","exchange_schema":"bcode.question.request",
        "exchange_schema_version":1,"kind":"bcode.question","tool_call_id":null,"title":null,
        "required":true,"snapshot":checkpoint,"resolved":false,"resolution":null
    })).unwrap());
    let mut surface = SessionSurface::new(SESSION.into(), 80, 20).unwrap();
    surface
        .accept_snapshot(
            &serde_json::json!({"schema_version":1,"epoch":EPOCH,"revision":1,"snapshot":snapshot})
                .to_string(),
        )
        .unwrap();
    surface.present::<()>(|_| Ok(())).unwrap();
    surface
        .input(r#"{"type":"key","key":"Enter","code":"Enter"}"#)
        .unwrap();
    assert_eq!(surface.take_intents().unwrap(), "[]");
}

#[test]
fn inline_and_pinned_questions_share_root_geometry_with_composer() {
    use bcode_tui_web::{BrowserInteraction, BrowserPolicy, InteractionPlacement, OffscreenFocus};
    let request = bcode_question_plugin::parse_question_request(serde_json::json!({
        "questions":[{"question":"Placement fixture","options":[{"label":"One"}],"custom":false}]
    }))
    .unwrap();
    let controller =
        bcode_question_plugin::presentation::QuestionInteractionController::new(request);
    let interaction: bcode_session_view_models::InteractionViewSummary = serde_json::from_value(serde_json::json!({
        "interaction_id":"placement", "producer_id":"bcode.question", "exchange_schema":"bcode.question.request",
        "exchange_schema_version":1, "kind":"bcode.question", "tool_call_id":null,"title":null,
        "required":true,"snapshot":controller.snapshot(),"resolved":false,"resolution":null
    })).unwrap();
    for placement in [
        InteractionPlacement::Transcript,
        InteractionPlacement::Pinned,
    ] {
        let mut snapshot = SessionViewSnapshot::empty();
        snapshot.session_id = Some(SESSION.parse().unwrap());
        snapshot.interactions.push(interaction.clone());
        snapshot.transcript.items.push(TranscriptViewItem {
            id: TranscriptViewItemId::new("question-placement"),
            revision: 1,
            sequence: None,
            timestamp_ms: None,
            output_location: None,
            streaming: false,
            kind: TranscriptViewItemKind::Interaction {
                interaction: interaction.clone(),
            },
        });
        let policy = BrowserPolicy {
            schema_version: 1,
            revision: format!("{placement:?}"),
            interaction: BrowserInteraction {
                placement,
                offscreen_focus: OffscreenFocus::Suspend,
            },
            ..BrowserPolicy::default()
        };
        let mut surface = SessionSurface::new(SESSION.into(), 80, 30).unwrap();
        surface.accept_snapshot(&serde_json::json!({"schema_version":1,"epoch":EPOCH,"revision":1,"snapshot":snapshot,"policy":policy}).to_string()).unwrap();
        surface
            .present::<()>(|frame| {
                assert!(
                    frame
                        .hits()
                        .regions()
                        .iter()
                        .any(|hit| hit.id.as_str() == "bcode-web/editor")
                );
                let text = (0..30)
                    .filter_map(|y| frame.buffer().row_symbols(y))
                    .collect::<String>();
                assert!(text.contains("Placement fixture"), "{placement:?}");
                for hit in frame.hits().regions() {
                    assert!(hit.area.right() <= 80 && hit.area.bottom() <= 30);
                }
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn cancel_uses_configured_binding_and_only_committed_active_turn() {
    use bcode_tui_web::{BrowserBinding, BrowserPolicy, BrowserResolvedBinding, BrowserStroke};
    let policy = BrowserPolicy {
        schema_version: 1,
        revision: "cancel-remapped".into(),
        bindings: vec![BrowserBinding {
            scope: "chat".into(),
            multi_tap: None,
            chord: vec![BrowserStroke {
                key: "k".into(),
                ctrl: true,
                alt: false,
                shift: false,
                meta: false,
            }],
            binding: BrowserResolvedBinding::Host {
                action: "app.interrupt".into(),
            },
        }],
        ..BrowserPolicy::default()
    };
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.session_id = Some(SESSION.parse().unwrap());
    snapshot.runtime.active_turn_id = Some("turn-1".into());
    let encode = |snapshot: &SessionViewSnapshot, revision| {
        serde_json::json!({
        "schema_version":1,"epoch":EPOCH,"revision":revision,"snapshot":snapshot,"policy":policy,
    }).to_string()
    };
    let mut surface = SessionSurface::new(SESSION.into(), 80, 20).unwrap();
    surface.accept_snapshot(&encode(&snapshot, 1)).unwrap();
    let cancel = r#"{"type":"key","key":"k","code":"KeyK","ctrl":true}"#;
    surface.input(cancel).unwrap();
    assert_eq!(surface.take_intents().unwrap(), "[]");
    surface.present::<()>(|_| Ok(())).unwrap();
    surface
        .input(r#"{"type":"key","key":"Escape","code":"Escape"}"#)
        .unwrap();
    assert_eq!(surface.take_intents().unwrap(), "[]");
    surface.input(cancel).unwrap();
    let intents: serde_json::Value =
        serde_json::from_str(&surface.take_intents().unwrap()).unwrap();
    let action: serde_json::Value =
        serde_json::from_str(intents[0]["action_json"].as_str().unwrap()).unwrap();
    assert_eq!(
        action,
        serde_json::json!({"type":"cancel_turn","turn_id":"turn-1"})
    );
    surface.present::<()>(|_| Ok(())).unwrap();
    surface.input(cancel).unwrap();
    assert_eq!(surface.take_intents().unwrap(), "[]");
    snapshot.runtime.active_turn_id = Some("turn-2".into());
    surface.accept_snapshot(&encode(&snapshot, 2)).unwrap();
    assert!(surface.present(|_| Err::<(), _>("failed output")).is_err());
    surface.input(cancel).unwrap();
    assert_eq!(surface.take_intents().unwrap(), "[]");
    surface.present::<()>(|_| Ok(())).unwrap();
    surface.input(cancel).unwrap();
    let intents: serde_json::Value =
        serde_json::from_str(&surface.take_intents().unwrap()).unwrap();
    let action: serde_json::Value =
        serde_json::from_str(intents[0]["action_json"].as_str().unwrap()).unwrap();
    assert_eq!(action["turn_id"], "turn-2");
}

#[test]
fn committed_disclosure_changes_local_scene_without_platform_intent() {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.session_id = Some(SESSION.parse().unwrap());
    snapshot.transcript.items.push(TranscriptViewItem {
        id: TranscriptViewItemId::new("details"),
        revision: 1,
        sequence: None,
        timestamp_ms: None,
        output_location: None,
        streaming: false,
        kind: TranscriptViewItemKind::AssistantMessage {
            message: ChatMessageView::markdown(
                "<details>\n<summary>Reveal</summary>\n\nSecret payload\n\n</details>",
            ),
        },
    });
    let mut surface = SessionSurface::new(SESSION.into(), 80, 20).unwrap();
    surface
        .accept_snapshot(
            &serde_json::json!({"schema_version":1,"epoch":EPOCH,"revision":1,"snapshot":snapshot})
                .to_string(),
        )
        .unwrap();
    let mut hit = None;
    surface
        .present::<()>(|frame| {
            let text = (0..20)
                .filter_map(|y| frame.buffer().row_symbols(y))
                .collect::<String>();
            assert!(!text.contains("Secret payload"));
            hit = frame
                .hits()
                .regions()
                .iter()
                .find(|region| region.id.as_str().starts_with("markdown:"))
                .map(|region| region.area);
            Ok(())
        })
        .unwrap();
    let area = hit.expect("visible disclosure must have a committed hit");
    for (action, buttons) in [("down", 1), ("up", 0)] {
        surface.input(&serde_json::json!({"type":"pointer","action":action,"x":area.x,"y":area.y,"button":0,"buttons":buttons}).to_string()).unwrap();
    }
    assert_eq!(surface.take_intents().unwrap(), "[]");
    surface
        .present::<()>(|frame| {
            let text = (0..20)
                .filter_map(|y| frame.buffer().row_symbols(y))
                .collect::<String>();
            assert!(text.contains("Secret payload"));
            Ok(())
        })
        .unwrap();
}

#[test]
fn blur_blocks_editor_and_submit_until_focus_returns() {
    let mut surface = SessionSurface::new(SESSION.into(), 40, 8).unwrap();
    surface.present::<()>(|_| Ok(())).unwrap();
    surface
        .input(r#"{"type":"focus","focused":false}"#)
        .unwrap();
    surface.input(r#"{"type":"text","text":"hidden"}"#).unwrap();
    assert!(
        !surface
            .input(r#"{"type":"key","key":"Enter","code":"Enter"}"#)
            .unwrap()
    );
    assert!(surface.draft().is_empty());
    surface.input(r#"{"type":"focus","focused":true}"#).unwrap();
    surface
        .input(r#"{"type":"text","text":"visible"}"#)
        .unwrap();
    assert_eq!(surface.draft(), "visible");
}

#[test]
fn stale_history_hit_cannot_request_removed_direction_after_output_failure() {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.session_id = Some(SESSION.parse().unwrap());
    snapshot.transcript.has_older_history = true;
    let observation = |revision, snapshot: &SessionViewSnapshot| {
        serde_json::json!({
            "schema_version":1, "epoch":EPOCH, "revision":revision, "snapshot":snapshot,
        })
        .to_string()
    };
    let mut surface = SessionSurface::new(SESSION.into(), 100, 12).unwrap();
    surface.accept_snapshot(&observation(1, &snapshot)).unwrap();
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
    snapshot.transcript.has_older_history = false;
    surface.accept_snapshot(&observation(2, &snapshot)).unwrap();
    assert!(surface.present(|_| Err::<(), _>("output failed")).is_err());
    for (action, buttons) in [("down", 1), ("up", 0)] {
        surface.input(&serde_json::json!({"type":"pointer","action":action,"x":area.x,"y":area.y,"button":0,"buttons":buttons}).to_string()).unwrap();
    }
    assert_eq!(surface.take_intents().unwrap(), "[]");
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
