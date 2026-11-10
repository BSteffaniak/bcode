//! Browser surface behaviour over the V2 transcript engine: the transcript is fed bounded
//! history pages and live batches (answered here by an in-memory canonical event log), the
//! non-transcript state arrives as snapshots without transcript items, and every assertion
//! goes through the committed frame, its hit regions, its selection scene and the intents the
//! surface emits.
use bcode_session_models::{SessionEvent, SessionHistoryDirection, SessionHistoryQuery, SessionId};
use bcode_session_view_models::{SessionViewSnapshot, TranscriptViewItemId};
use bcode_tui_web::SessionSurface;
use bmux_tui::geometry::Rect;

const SESSION: &str = "00000000-0000-4000-8000-000000000001";
const EPOCH: &str = "00000000-0000-4000-8000-000000000002";
const FEED_EPOCH: &str = "00000000-0000-4000-8000-000000000003";
const ITEM_HIT: &str = "bcode-web/transcript-item:";
const LATEST: &str = "bcode-web/latest";

fn session_id() -> SessionId {
    SESSION.parse().unwrap()
}

/// One canonical event of the test session.
fn event(sequence: u64, kind: serde_json::Value) -> SessionEvent {
    serde_json::from_value(serde_json::json!({
        "schema_version": bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        "sequence": sequence, "timestamp_ms": sequence, "session_id": SESSION, "kind": kind,
    }))
    .unwrap()
}

fn user(sequence: u64, text: &str) -> SessionEvent {
    event(
        sequence,
        serde_json::json!({"user_message": {"client_id": EPOCH, "text": text}}),
    )
}

fn assistant(sequence: u64, text: &str) -> SessionEvent {
    event(
        sequence,
        serde_json::json!({"assistant_message": {"text": text}}),
    )
}

/// A finished tool call whose result is `artifact`.
fn tool(first: u64, call: &str, producer: &str, artifact: serde_json::Value) -> Vec<SessionEvent> {
    vec![
        event(
            first,
            serde_json::json!({"tool_call_requested": {"tool_call_id": call,
                "producer_plugin_id": producer, "tool_name": "tool", "arguments_json": "{}"}}),
        ),
        event(
            first + 1,
            serde_json::json!({"tool_invocation_result_recorded": {"record": {
                "invocation_id": call, "model_output": "", "is_error": false,
                "result": {"type": "artifact", "artifact": artifact["artifact"]}}}}),
        ),
    ]
}

/// The snapshot envelope of `snapshot` (the variant without transcript items).
fn envelope(
    revision: u64,
    snapshot: &SessionViewSnapshot,
    policy: Option<&serde_json::Value>,
) -> String {
    let mut value = serde_json::json!({
        "schema_version": 1, "epoch": EPOCH, "revision": revision, "snapshot": snapshot,
    });
    if let Some(policy) = policy {
        value["policy"] = policy.clone();
    }
    value.to_string()
}

fn empty_snapshot() -> SessionViewSnapshot {
    let mut snapshot = SessionViewSnapshot::empty();
    snapshot.session_id = Some(session_id());
    snapshot
}

fn visual_policy(revision: &str, plugin: &str) -> serde_json::Value {
    serde_json::json!({"schema_version":1,"revision":revision,"styles":{},"bindings":[],
        "visual_policy":{"enabled_plugins":[plugin],"preferred":[],"disabled":[]}})
}

/// The in-memory canonical log the feed routes answer from.
struct Log {
    events: Vec<SessionEvent>,
    offset: u64,
}

impl Log {
    const fn new(events: Vec<SessionEvent>) -> Self {
        Self { events, offset: 0 }
    }

    /// Answer one bounded history read exactly as the daemon's indexed read does.
    fn page(&self, query: SessionHistoryQuery) -> serde_json::Value {
        let (events, has_more): (Vec<&SessionEvent>, bool) = match query.direction {
            SessionHistoryDirection::Backward => {
                let cursor = query.cursor.map_or(u64::MAX, |cursor| cursor.sequence);
                let before = self
                    .events
                    .iter()
                    .filter(|event| event.sequence <= cursor)
                    .collect::<Vec<_>>();
                let skip = before.len().saturating_sub(query.limit);
                (before[skip..].to_vec(), skip > 0)
            }
            SessionHistoryDirection::Forward => {
                let cursor = query.cursor.map_or(0, |cursor| cursor.sequence);
                let after = self
                    .events
                    .iter()
                    .filter(|event| event.sequence >= cursor)
                    .collect::<Vec<_>>();
                let more = after.len() > query.limit;
                (after.into_iter().take(query.limit).collect(), more)
            }
        };
        serde_json::json!({"schema_version": 1, "page": {
            "session_id": SESSION, "events": events, "has_more": has_more}})
    }

    /// A live batch of `events` (appended to the log) after the current cursor.
    fn live(&mut self, continuity: &str, events: Vec<SessionEvent>) -> String {
        self.offset += events.len() as u64;
        let body = serde_json::json!({
            "schema_version": 1, "continuity": continuity,
            "cursor": {"epoch": FEED_EPOCH, "offset": self.offset},
            "events": events.iter().map(|event| serde_json::json!({"type": "durable", "event": event})).collect::<Vec<_>>(),
            "more": false,
        })
        .to_string();
        self.events.extend(events);
        body
    }
}

/// Answer every history read the surface claims from `log`. Returns the queries answered.
fn pump(surface: &mut SessionSurface, log: &Log) -> Vec<SessionHistoryQuery> {
    let mut answered = Vec::new();
    while let Some(claim) = surface.next_host_request().unwrap() {
        let claim: serde_json::Value = serde_json::from_str(&claim).unwrap();
        assert_eq!(claim["route"], "/api/session-view/history-page");
        let body: serde_json::Value =
            serde_json::from_str(claim["body"].as_str().unwrap()).unwrap();
        assert_eq!(body["session_id"], SESSION);
        let query: SessionHistoryQuery = serde_json::from_value(body["query"].clone()).unwrap();
        answered.push(query);
        surface
            .accept_history_page(
                claim["claim"].as_str().unwrap(),
                &log.page(query).to_string(),
            )
            .unwrap();
        assert!(answered.len() < 64, "bounded reads");
    }
    answered
}

/// A surface subscribed to the live feed and attached to `log`'s tail window.
fn mounted(
    width: u16,
    height: u16,
    log: &mut Log,
    policy: Option<&serde_json::Value>,
) -> SessionSurface {
    let mut surface = SessionSurface::new(SESSION.into(), width, height).unwrap();
    surface
        .accept_snapshot(&envelope(1, &empty_snapshot(), policy))
        .unwrap();
    let subscription: serde_json::Value =
        serde_json::from_str(&surface.live_request().unwrap()).unwrap();
    assert_eq!(subscription, serde_json::json!({"session_id": SESSION}));
    surface.accept_live(&log.live("start", Vec::new())).unwrap();
    assert!(
        !pump(&mut surface, log).is_empty(),
        "attach loads the tail window"
    );
    surface
}

#[derive(Default)]
struct Frame {
    rows: Vec<String>,
    hits: Vec<(String, Rect)>,
    images: Vec<bmux_tui::image::ImagePlacement>,
}

impl Frame {
    fn text(&self) -> String {
        self.rows.join("\n")
    }

    fn hit(&self, id: &str) -> Option<Rect> {
        self.hits
            .iter()
            .find(|(hit, _)| hit == id)
            .map(|(_, area)| *area)
    }

    /// The smallest item hit containing cell (`x`, `y`).
    fn item_hit_at(&self, x: u16, y: u16) -> Option<Rect> {
        self.item_hits()
            .into_iter()
            .filter(|area| area.contains(bmux_tui::geometry::Point::new(x, y)))
            .min_by_key(|area| u32::from(area.width) * u32::from(area.height))
    }

    fn item_hits(&self) -> Vec<Rect> {
        self.hits
            .iter()
            .filter(|(id, _)| id.starts_with(ITEM_HIT))
            .map(|(_, area)| *area)
            .collect()
    }

    /// Cell of the first occurrence of `text`.
    fn find(&self, text: &str) -> Option<(u16, u16)> {
        self.rows.iter().enumerate().find_map(|(y, row)| {
            let byte = row.find(text)?;
            let column = row[..byte].chars().count();
            Some((u16::try_from(column).ok()?, u16::try_from(y).ok()?))
        })
    }
}

fn present(surface: &mut SessionSurface) -> Frame {
    let mut frame = Frame::default();
    surface
        .present::<()>(|prepared| {
            frame.rows = (0..prepared.buffer().area().height)
                .map(|y| prepared.buffer().row_symbols(y).unwrap_or_default())
                .collect();
            frame.hits = prepared
                .hits()
                .regions()
                .iter()
                .map(|hit| (hit.id.as_str().to_owned(), hit.area))
                .collect();
            frame.images = prepared.image_scene().placements().cloned().collect();
            Ok(())
        })
        .unwrap();
    frame
}

fn pointer(surface: &mut SessionSurface, action: &str, x: u16, y: u16, buttons: u8) {
    surface
        .input(
            &serde_json::json!({"type":"pointer","action":action,"x":x,"y":y,"button":0,"buttons":buttons})
                .to_string(),
        )
        .unwrap();
}

fn click(surface: &mut SessionSurface, area: Rect) {
    pointer(surface, "down", area.x, area.y, 1);
    pointer(surface, "up", area.x, area.y, 0);
}

fn intents(surface: &mut SessionSurface) -> serde_json::Value {
    serde_json::from_str(&surface.take_intents().unwrap()).unwrap()
}

fn key(surface: &mut SessionSurface, key: &str, ctrl: bool) -> bool {
    surface
        .input(&serde_json::json!({"type":"key","key":key,"code":key,"ctrl":ctrl}).to_string())
        .unwrap()
}

/// Answer reads and fire timers (motion frames, paced text) until the transcript is idle.
fn settle(surface: &mut SessionSurface, log: &Log) {
    for _ in 0..200 {
        let reads = pump(surface, log);
        let _ = present(surface);
        match surface.next_deadline_ms() {
            Some(wait) => {
                std::thread::sleep(std::time::Duration::from_secs_f64(wait.min(50.0) / 1000.0));
                let _ = surface.advance();
            }
            None if reads.is_empty() => return,
            None => {}
        }
    }
    panic!("the transcript did not settle");
}

/// A long conversation: `turns` user/assistant pairs with tall Markdown replies.
fn conversation(turns: u64) -> Vec<SessionEvent> {
    (0..turns)
        .flat_map(|turn| {
            [
                user(turn * 2 + 1, &format!("question {turn}")),
                assistant(
                    turn * 2 + 2,
                    &format!("{}Window {turn} reply", "content\n\n".repeat(3)),
                ),
            ]
        })
        .collect()
}

#[test]
fn history_traversal_pages_older_turns_then_returns_to_the_tail() {
    let mut log = Log::new(conversation(400));
    let mut surface = mounted(40, 12, &mut log, None);
    let frame = present(&mut surface);
    assert!(
        frame.text().contains("Window 399 reply"),
        "{}",
        frame.text()
    );
    // Reading upward pages older history through the shared loader: each read is bounded
    // and strictly before the resident window.
    let mut older = Vec::new();
    for _ in 0..2000 {
        key(&mut surface, "PageUp", false);
        older.extend(pump(&mut surface, &log));
        if present(&mut surface).text().contains("question 0 ") {
            break;
        }
    }
    let frame = present(&mut surface);
    assert!(frame.text().contains("question 0 "), "{}", frame.text());
    assert!(!older.is_empty());
    for query in &older {
        assert!(query.limit <= 512);
        if query.limit > 1 {
            assert_eq!(query.direction, SessionHistoryDirection::Backward);
            assert!(query.cursor.is_some_and(|cursor| cursor.sequence < 800));
        }
    }
    // The latest content is one jump away, loading the tail when it was evicted.
    let bar = frame
        .hit(LATEST)
        .expect("the latest bar while reading history");
    click(&mut surface, bar);
    settle(&mut surface, &log);
    let frame = present(&mut surface);
    assert!(
        frame.text().contains("Window 399 reply"),
        "{}",
        frame.text()
    );
    assert!(frame.hit(LATEST).is_none(), "following again");
}

#[test]
fn foreign_history_pages_are_rejected_and_the_read_is_retried() {
    let mut log = Log::new(conversation(4));
    let mut surface = SessionSurface::new(SESSION.into(), 40, 12).unwrap();
    surface
        .accept_snapshot(&envelope(1, &empty_snapshot(), None))
        .unwrap();
    surface.accept_live(&log.live("start", Vec::new())).unwrap();
    let claim: serde_json::Value =
        serde_json::from_str(&surface.next_host_request().unwrap().unwrap()).unwrap();
    let claim = claim["claim"].as_str().unwrap().to_owned();
    let foreign = serde_json::json!({"schema_version": 1, "page": {
        "session_id": FEED_EPOCH, "events": [], "has_more": false}});
    assert!(
        !surface
            .accept_history_page(&claim, &foreign.to_string())
            .unwrap()
    );
    assert!(
        surface
            .accept_history_page(&claim, &foreign.to_string())
            .is_err(),
        "an answered claim is stale"
    );
    assert!(
        surface.next_host_request().unwrap().is_none(),
        "backing off"
    );
    let wait = surface.next_deadline_ms().expect("retry scheduled");
    assert!(wait > 0.0 && wait <= 250.0, "{wait}");
    std::thread::sleep(std::time::Duration::from_millis(260));
    assert!(surface.advance());
    assert!(
        !pump(&mut surface, &log).is_empty(),
        "the window is re-read"
    );
    assert!(present(&mut surface).text().contains("Window 3 reply"));
}

#[test]
fn live_batches_apply_in_order_and_a_gap_reloads_the_tail() {
    let mut log = Log::new(conversation(2));
    let mut surface = mounted(60, 16, &mut log, None);
    assert!(present(&mut surface).text().contains("Window 1 reply"));
    let request: serde_json::Value =
        serde_json::from_str(&surface.live_request().unwrap()).unwrap();
    assert_eq!(
        request["cursor"],
        serde_json::json!({"epoch": FEED_EPOCH, "offset": 0})
    );
    let batch = log.live(
        "continuous",
        vec![user(5, "live question"), assistant(6, "live answer")],
    );
    assert!(surface.accept_live(&batch).unwrap());
    assert!(
        pump(&mut surface, &log).is_empty(),
        "a live window needs no read"
    );
    let frame = present(&mut surface);
    assert!(frame.text().contains("live answer"), "{}", frame.text());
    // Re-delivered durable events are duplicates, never applied twice.
    let replay = serde_json::json!({"schema_version":1,"continuity":"continuous",
        "cursor":{"epoch":FEED_EPOCH,"offset":2},
        "events":[{"type":"durable","event":assistant(6, "live answer")}],"more":false});
    let _ = surface.accept_live(&replay.to_string()).unwrap();
    assert_eq!(
        present(&mut surface).text().matches("live answer").count(),
        1
    );
    // A gap reloads the tail window, which brings the missed events.
    log.events.push(assistant(7, "missed while detached"));
    let gap = log.live("gap", Vec::new());
    assert!(surface.accept_live(&gap).unwrap());
    let reads = pump(&mut surface, &log);
    assert_eq!(reads[0].direction, SessionHistoryDirection::Backward);
    assert!(reads[0].cursor.is_none(), "the tail");
    assert!(
        present(&mut surface)
            .text()
            .contains("missed while detached")
    );
    assert!(
        surface
            .accept_live(r#"{"schema_version":1,"error":"browser_busy"}"#)
            .is_err()
    );
    assert!(
        surface
            .accept_live(r#"{"schema_version":2,"error":"x"}"#)
            .is_err()
    );
}

#[test]
fn canonical_artifact_image_is_admitted_to_real_bmux_scene_and_stale_resize_is_rejected() {
    let image = serde_json::json!({"artifact":{"artifact_id":"image-artifact","producer_plugin_id":"image","schema":"image","schema_version":1,
        "refs":[{"key":"image","content_type":"image/png","byte_len":4}]},"generic_payload":{}});
    let diff = serde_json::json!({"artifact":{"artifact_id":"diff-b-artifact","producer_plugin_id":"bcode.filesystem","schema":"bcode.filesystem.batch","schema_version":1,
        "refs":[{"key":"file-0-old","content_type":"application/octet-stream","byte_len":4}]},
        "generic_payload":{"path":"b.rs","old_text":"old","new_text":"new"}});
    let mut events = vec![user(1, "show it")];
    events.extend(tool(2, "call", "image", image));
    let mut log = Log::new(events);
    let mut surface = mounted(80, 20, &mut log, None);
    let item = TranscriptViewItemId::tool("call");
    let item = item.get();
    assert!(surface.prepare_image(item, "image"));
    surface.resize(40, 20).unwrap();
    assert!(
        !surface.accept_image(vec![255, 0, 0, 255], 1, 1),
        "stale width"
    );
    assert!(surface.prepare_image(item, "image"));
    assert!(!surface.accept_image(vec![255], 1, 1), "malformed pixels");
    assert!(surface.prepare_image(item, "image"));
    assert!(surface.accept_image(vec![255, 0, 0, 255], 1, 1));
    let frame = present(&mut surface);
    assert_eq!(frame.images.len(), 1, "{}", frame.text());
    assert!(
        !surface.accept_image(vec![255, 0, 0, 255], 1, 1),
        "consumed"
    );
    // Accepting an unrelated producer artifact must not evict decoded image A.
    let batch = log.live(
        "continuous",
        tool(4, "diff-b-call", "bcode.filesystem", diff),
    );
    surface.accept_live(&batch).unwrap();
    surface.resize(40, 60).unwrap();
    let policy = visual_policy("image-plus-diff", "bcode.filesystem");
    surface
        .accept_snapshot(&envelope(2, &empty_snapshot(), Some(&policy)))
        .unwrap();
    // Policy installation re-routes visuals; decode A against that exact policy.
    assert!(surface.prepare_image(item, "image"));
    assert!(surface.accept_image(vec![255, 0, 0, 255], 1, 1));
    let before = present(&mut surface).images;
    assert_eq!(before.len(), 1);
    let claim: serde_json::Value =
        serde_json::from_str(&surface.next_visual_artifact(0).unwrap().unwrap()).unwrap();
    let request: serde_json::Value =
        serde_json::from_str(claim["request_json"].as_str().unwrap()).unwrap();
    assert_eq!(request["artifact_id"], "diff-b-artifact");
    let response = serde_json::json!({"artifact_id":"diff-b-artifact","reference_key":"file-0-old","content_type":"application/octet-stream",
        "offset":0,"total_bytes":4,"bytes":[111,108,100,10],"reference_revision":1,"finalized":true}).to_string();
    assert!(
        surface
            .accept_visual_artifact_claim(claim["claim"].as_str().unwrap(), &response, 1)
            .unwrap()
    );
    assert_eq!(present(&mut surface).images, before);
    // Preservation is not blanket validity: the decode belongs to one body width, so a
    // resize that changes it removes the decoded projection until a fresh decode is admitted.
    surface.resize(60, 60).unwrap();
    assert!(present(&mut surface).images.is_empty());
    surface.resize(40, 60).unwrap();
    assert!(present(&mut surface).images.is_empty(), "never resurrected");
    surface.resize(0, 0).unwrap();
    assert!(present(&mut surface).images.is_empty());
}

#[test]
fn committed_markdown_links_and_selected_code_copy_emit_narrow_platform_intents() {
    let mut log = Log::new(vec![
        user(1, "links"),
        // The code block follows the link (and its inserted readable destination) in one
        // message: copy still maps to the code's own source bytes.
        assistant(2, "[safe](https://example.com)\n\n```\nlet x = 1;\n```"),
    ]);
    let mut surface = mounted(80, 20, &mut log, None);
    let frame = present(&mut surface);
    let (x, y) = frame.find("safe").expect("link painted");
    let link = frame.item_hit_at(x, y).expect("link region");
    click(&mut surface, link);
    let sent = intents(&mut surface);
    assert_eq!(sent[0]["type"], "navigate");
    assert_eq!(sent[0]["url"], "https://example.com/");
    // Copy is the logical source of a selection over the code block.
    let frame = present(&mut surface);
    let (x, y) = frame.find("let x = 1;").expect("code painted");
    pointer(&mut surface, "down", x, y, 1);
    pointer(&mut surface, "move", x + 9, y, 1);
    pointer(&mut surface, "up", x + 9, y, 0);
    assert_eq!(surface.selected_text().as_deref(), Some("let x = 1;"));
    surface
        .input(r#"{"type":"key","key":"C","code":"KeyC","ctrl":true,"shift":true}"#)
        .unwrap();
    assert!(
        surface
            .take_clipboard()
            .is_some_and(|text| text.contains("let x = 1;"))
    );
    let frame = present(&mut surface);
    let (x, y) = frame.find("safe").expect("link painted");
    let link = frame.item_hit_at(x, y).expect("link region");
    // Even unchanged items cannot reuse controls after a new local preparation until its
    // geometry has successfully committed.
    surface.resize(80, 20).unwrap();
    click(&mut surface, link);
    assert_eq!(surface.take_intents().unwrap(), "[]");
    assert!(surface.present(|_| Err::<(), _>("output failed")).is_err());
    click(&mut surface, link);
    assert_eq!(surface.take_intents().unwrap(), "[]");
    let _ = present(&mut surface);
    click(&mut surface, link);
    assert_eq!(intents(&mut surface)[0]["type"], "navigate");
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
    let mut surface = SessionSurface::new(SESSION.into(), 80, 12).unwrap();
    let policy = serde_json::to_value(&policy).unwrap();
    surface
        .accept_snapshot(&envelope(1, &empty_snapshot(), Some(&policy)))
        .unwrap();
    let _ = present(&mut surface);
    surface.input(r#"{"type":"text","text":"abc"}"#).unwrap();
    assert!(!key(&mut surface, "Enter", false));
    assert_eq!(surface.draft(), "abc");
    surface
        .input(r#"{"type":"key","key":"ArrowLeft","code":"ArrowLeft"}"#)
        .unwrap();
    surface.input(r#"{"type":"text","text":"X"}"#).unwrap();
    assert_eq!(surface.draft(), "abXc");
    assert!(!key(&mut surface, "x", true));
    assert!(key(&mut surface, "s", true));
}

fn question_interaction(
    id: &str,
    question: &str,
) -> bcode_session_view_models::InteractionViewSummary {
    let request = bcode_question_plugin::parse_question_request(serde_json::json!({
        "questions": [{"question": question, "options":[{"label":"One","value":"one"}], "custom":false}]
    }))
    .unwrap();
    let controller =
        bcode_question_plugin::presentation::QuestionInteractionController::new(request);
    serde_json::from_value(serde_json::json!({
        "interaction_id": id, "producer_id": "bcode.question", "exchange_schema": "bcode.question.request",
        "exchange_schema_version": 1, "kind": "bcode.question", "tool_call_id": null, "title": null,
        "required": true, "snapshot": controller.snapshot(), "resolved": false, "resolution": null
    }))
    .unwrap()
}

#[test]
fn shared_question_requires_committed_snapshot_and_emits_only_canonical_semantic_input() {
    let mut snapshot = empty_snapshot();
    snapshot
        .interactions
        .push(question_interaction("q1", "Choose 日本語🦀é"));
    let mut surface = SessionSurface::new(SESSION.into(), 80, 20).unwrap();
    surface
        .accept_snapshot(&envelope(1, &snapshot, None))
        .unwrap();
    let activate = r#"{"type":"key","key":"Enter","code":"Enter"}"#;
    assert!(!surface.input(activate).unwrap());
    assert_eq!(surface.take_intents().unwrap(), "[]");
    let _ = present(&mut surface);
    // A failed repaint of the same checkpoint must not revoke the input
    // authority of the still-visible committed question.
    assert!(
        surface
            .present(|_| Err::<(), _>("same-scene output failed"))
            .is_err()
    );
    assert!(!surface.input(activate).unwrap());
    let sent = intents(&mut surface);
    assert_eq!(sent.as_array().unwrap().len(), 1);
    let action: serde_json::Value =
        serde_json::from_str(sent[0]["action_json"].as_str().unwrap()).unwrap();
    assert_eq!(action["type"], "interaction");
    assert_eq!(action["interaction_id"], "q1");
    assert_eq!(action["input"]["type"], "activate");
    surface.input(activate).unwrap();
    assert_eq!(surface.take_intents().unwrap(), "[]");
    snapshot.interactions[0].interaction_id = "q2".into();
    surface
        .accept_snapshot(&envelope(2, &snapshot, None))
        .unwrap();
    assert!(surface.present(|_| Err::<(), _>("output failed")).is_err());
    surface.input(activate).unwrap();
    assert_eq!(surface.take_intents().unwrap(), "[]");
    for (width, height) in [(0, 0), (1, 1), (2, 2), (80, 20)] {
        surface.resize(width, height).unwrap();
        let _ = present(&mut surface);
    }
    snapshot.interactions[0].resolved = true;
    surface
        .accept_snapshot(&envelope(3, &snapshot, None))
        .unwrap();
    let _ = present(&mut surface);
    surface
        .input(r#"{"type":"text","text":"composer restored"}"#)
        .unwrap();
    assert_eq!(surface.draft(), "composer restored");
}

#[test]
fn committed_semantics_are_visible_bounded_and_cleared_on_zero_size() {
    let mut surface = SessionSurface::new(SESSION.into(), 80, 12).unwrap();
    let _ = present(&mut surface);
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
    let _ = present(&mut surface);
    let metadata: serde_json::Value =
        serde_json::from_str(&surface.accessibility().unwrap()).unwrap();
    assert!(metadata["regions"].as_array().unwrap().is_empty());
}

/// Scroll up until the latest bar shows; returns its area in the committed frame.
fn read_upward(surface: &mut SessionSurface, log: &Log) -> Rect {
    for _ in 0..8 {
        key(surface, "PageUp", false);
        pump(surface, log);
        if let Some(bar) = present(surface).hit(LATEST) {
            return bar;
        }
    }
    panic!("the latest bar shows while reading above the latest content");
}

#[test]
fn committed_latest_bar_click_keyboard_and_failed_output_share_one_scene() {
    let mut log = Log::new(conversation(20));
    let mut surface = mounted(100, 12, &mut log, None);
    let bar = read_upward(&mut surface, &log);
    let accessibility = surface.accessibility().unwrap();
    assert!(
        accessibility.contains("return to latest"),
        "{accessibility}"
    );
    click(&mut surface, bar);
    assert!(
        present(&mut surface).hit(LATEST).is_none(),
        "following again"
    );
    // A configured key returns to the latest content too.
    surface
        .configure_bindings(r#"{"ctrl+l":"latest"}"#)
        .unwrap();
    let _ = read_upward(&mut surface, &log);
    key(&mut surface, "l", true);
    pump(&mut surface, &log);
    assert!(present(&mut surface).hit(LATEST).is_none());
    // Failed output keeps the committed metadata; a zero-sized surface registers nothing.
    let _ = read_upward(&mut surface, &log);
    let committed = surface.accessibility().unwrap();
    let batch = log.live("continuous", vec![assistant(41, "more below")]);
    surface.accept_live(&batch).unwrap();
    assert!(surface.present(|_| Err::<(), _>("output failed")).is_err());
    assert_eq!(surface.accessibility().unwrap(), committed);
    surface.resize(0, 0).unwrap();
    assert!(present(&mut surface).hits.is_empty());
    assert!(
        !surface
            .accessibility()
            .unwrap()
            .contains("return to latest")
    );
    surface.resize(100, 12).unwrap();
    assert!(present(&mut surface).hit("bcode-web/editor").is_some());
}

#[test]
fn stale_latest_bar_hit_cannot_navigate_after_output_failure() {
    let mut log = Log::new(conversation(20));
    let mut surface = mounted(100, 12, &mut log, None);
    let bar = read_upward(&mut surface, &log);
    // A transcript change prepares a new frame; its output fails, so the old hit is stale.
    let batch = log.live("continuous", vec![assistant(41, "more below")]);
    surface.accept_live(&batch).unwrap();
    assert!(surface.present(|_| Err::<(), _>("output failed")).is_err());
    click(&mut surface, bar);
    assert!(
        present(&mut surface).hit(LATEST).is_some(),
        "the stale click did not jump"
    );
}

#[test]
fn permission_uses_shared_decisions_and_never_activates_uncommitted_identity() {
    let mut snapshot = empty_snapshot();
    let permission = bcode_session_view_models::PermissionView {
        permission_id: "p1".into(),
        session_id: snapshot.session_id,
        tool_call_id: "call".into(),
        tool_name: "tool".into(),
        producer_plugin_id: None,
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
        .accept_snapshot(&envelope(1, &snapshot, None))
        .unwrap();
    key(&mut surface, "y", true);
    assert_eq!(surface.take_intents().unwrap(), "[]");
    let frame = present(&mut surface);
    let decisions = frame
        .hits
        .iter()
        .filter(|(id, _)| id.starts_with("bcode-web/permission/"))
        .collect::<Vec<_>>();
    assert_eq!(decisions.len(), 2); // Only complete-batch CAS choices are actionable.
    let area = decisions[0].1;
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
    click(&mut surface, area);
    let sent = intents(&mut surface);
    let action: serde_json::Value =
        serde_json::from_str(sent[0]["action_json"].as_str().unwrap()).unwrap();
    assert_eq!(
        action,
        serde_json::json!({"type":"permission_batch","batch_id":"batch","approved":true})
    );
    key(&mut surface, "y", true);
    assert_eq!(surface.take_intents().unwrap(), "[]");
    // Individual shortcuts must never silently expand authority to the batch.
    key(&mut surface, "n", true);
    assert_eq!(surface.take_intents().unwrap(), "[]");
    snapshot.permissions[0].permission_id = "p2".into();
    // This replacement is an individual checkpoint, so the shared deny shortcut below can
    // resolve it.
    snapshot.permissions[0].batch = None;
    surface
        .accept_snapshot(&envelope(2, &snapshot, None))
        .unwrap();
    assert!(surface.present(|_| Err::<(), _>("output failed")).is_err());
    key(&mut surface, "y", true);
    assert_eq!(surface.take_intents().unwrap(), "[]");
    let _ = present(&mut surface);
    key(&mut surface, "n", true);
    let sent = intents(&mut surface);
    assert!(sent[0]["action_json"].as_str().unwrap().contains("p2"));
}

fn shell_artifact() -> serde_json::Value {
    serde_json::json!({"artifact":{"artifact_id":"shell-result","producer_plugin_id":"bcode.shell","schema":"bcode.shell.run","schema_version":1,
        "refs":[{"key":"clean_output","content_type":"text/plain","metadata":{"semantic_preview":{"version":1,"command":"printf hello","text":"界🙂 hello\n","bytes":17,"truncated":false}}}]},
        "generic_payload":{}})
}

/// The smallest item hit of `frame`: a producer action cell rather than a header.
fn smallest_item_hit(frame: &Frame) -> Option<Rect> {
    frame
        .item_hits()
        .into_iter()
        .min_by_key(|area| u32::from(area.width) * u32::from(area.height))
}

#[test]
fn producer_copy_action_requires_current_successfully_committed_scene() {
    let mut events = vec![user(1, "run")];
    events.extend(tool(2, "shell-call", "bcode.shell", shell_artifact()));
    let mut log = Log::new(events);
    let policy = visual_policy("shell-actions", "bcode.shell");
    let mut surface = mounted(80, 30, &mut log, Some(&policy));
    let frame = present(&mut surface);
    assert!(frame.text().contains("hello"), "{}", frame.text());
    let area = smallest_item_hit(&frame).expect("a source-backed producer action");
    click(&mut surface, area);
    let sent = intents(&mut surface);
    assert_eq!(sent[0]["type"], "clipboard", "{sent}");
    assert!(!sent[0]["text"].as_str().unwrap().is_empty());
    // Any transcript change revokes the committed actions until the next commit.
    let batch = log.live("continuous", vec![assistant(4, "after the tool")]);
    surface.accept_live(&batch).unwrap();
    click(&mut surface, area);
    assert_eq!(surface.take_intents().unwrap(), "[]");
    assert!(
        surface
            .present(|_| Err::<(), _>("failed producer output"))
            .is_err()
    );
    click(&mut surface, area);
    assert_eq!(surface.take_intents().unwrap(), "[]");
    let frame = present(&mut surface);
    click(&mut surface, smallest_item_hit(&frame).unwrap());
    assert_eq!(intents(&mut surface)[0]["type"], "clipboard");
}

fn filesystem_artifact(id: &str, byte_len: u64) -> serde_json::Value {
    serde_json::json!({"artifact":{"artifact_id":id,"producer_plugin_id":"bcode.filesystem","schema":"bcode.filesystem.batch","schema_version":1,
        "refs":[{"key":"file-0-old","content_type":"application/octet-stream","byte_len":byte_len}]},
        "generic_payload":{"path":"src/lib.rs","old_text":"old","new_text":"new"}})
}

#[test]
fn producer_artifact_pages_pin_versions_and_stop_at_producer_budget() {
    for mismatch in [None, Some("reference_revision"), Some("finalized")] {
        let mut events = vec![user(1, "edit")];
        events.extend(tool(
            2,
            "budget-call",
            "bcode.filesystem",
            filesystem_artifact("budget-artifact", 1_000_000),
        ));
        let mut log = Log::new(events);
        let policy = visual_policy("budget", "bcode.filesystem");
        let mut surface = mounted(80, 20, &mut log, Some(&policy));
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
    let mut events = vec![user(1, "edit")];
    events.extend(tool(
        2,
        "diff-call",
        "bcode.filesystem",
        filesystem_artifact("diff-artifact", 4),
    ));
    let mut log = Log::new(events);
    let policy = visual_policy("visual-policy", "bcode.filesystem");
    let mut surface = mounted(80, 20, &mut log, Some(&policy));
    let claim: serde_json::Value = serde_json::from_str(
        &surface
            .next_visual_artifact(0)
            .unwrap()
            .expect("real filesystem adapter selects artifact"),
    )
    .unwrap();
    let request: serde_json::Value =
        serde_json::from_str(claim["request_json"].as_str().unwrap()).unwrap();
    assert_eq!(
        request["item_id"],
        TranscriptViewItemId::tool("diff-call").get()
    );
    assert_eq!(request["artifact_id"], "diff-artifact");
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
    let mut interaction = question_interaction("malformed", "Required");
    interaction.snapshot.as_mut().unwrap()["answers"] = serde_json::json!([]);
    let mut snapshot = empty_snapshot();
    snapshot.interactions.push(interaction);
    let mut surface = SessionSurface::new(SESSION.into(), 80, 20).unwrap();
    surface
        .accept_snapshot(&envelope(1, &snapshot, None))
        .unwrap();
    let _ = present(&mut surface);
    key(&mut surface, "Enter", false);
    assert_eq!(surface.take_intents().unwrap(), "[]");
}

#[test]
fn inline_and_pinned_questions_share_root_geometry_with_composer() {
    use bcode_tui_web::{BrowserInteraction, BrowserPolicy, InteractionPlacement, OffscreenFocus};
    let interaction = question_interaction("placement", "Placement fixture");
    for (placement, inline) in [
        (InteractionPlacement::Transcript, true),
        (InteractionPlacement::Pinned, false),
    ] {
        let mut snapshot = empty_snapshot();
        snapshot.interactions.push(interaction.clone());
        let mut log = Log::new(vec![
            user(1, "ask me"),
            event(
                2,
                serde_json::json!({"tool_call_requested": {"tool_call_id": "ask",
                    "producer_plugin_id": "bcode.question", "tool_name": "question",
                    "arguments_json": "{}"}}),
            ),
            event(
                3,
                serde_json::json!({"tool_exchange_requested": {"request": {
                    "invocation_id": "ask", "exchange_id": "placement",
                    "producer_id": "bcode.question", "schema": "bcode.question.request",
                    "schema_version": 1, "payload": {}, "response_policy": "required"}}}),
            ),
        ]);
        let policy = serde_json::to_value(BrowserPolicy {
            schema_version: 1,
            revision: format!("{placement:?}"),
            interaction: BrowserInteraction {
                placement,
                offscreen_focus: OffscreenFocus::Suspend,
            },
            ..BrowserPolicy::default()
        })
        .unwrap();
        let mut surface = SessionSurface::new(SESSION.into(), 80, 30).unwrap();
        surface
            .accept_snapshot(&envelope(1, &snapshot, Some(&policy)))
            .unwrap();
        surface.accept_live(&log.live("start", Vec::new())).unwrap();
        pump(&mut surface, &log);
        let frame = present(&mut surface);
        let editor = frame.hit("bcode-web/editor").expect("composer");
        let (_, row) = frame.find("Placement fixture").expect("question painted");
        assert!(
            row < editor.y,
            "{placement:?}: the question is above the composer"
        );
        let (_, user_row) = frame.find("ask me").expect("transcript painted");
        assert!(row > user_row, "{placement:?}: below the asking turn");
        if inline {
            // Inline: the question occupies reserved transcript rows, so the transcript
            // region still reaches the composer.
            assert!(frame.text().contains("Placement fixture"));
        }
        for (_, hit) in &frame.hits {
            assert!(hit.right() <= 80 && hit.bottom() <= 30);
        }
    }
}

#[test]
fn cancel_uses_configured_binding_and_only_committed_active_turn() {
    use bcode_tui_web::{BrowserBinding, BrowserPolicy, BrowserResolvedBinding, BrowserStroke};
    let policy = serde_json::to_value(BrowserPolicy {
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
    })
    .unwrap();
    let mut snapshot = empty_snapshot();
    snapshot.runtime.active_turn_id = Some("turn-1".into());
    let mut surface = SessionSurface::new(SESSION.into(), 80, 20).unwrap();
    surface
        .accept_snapshot(&envelope(1, &snapshot, Some(&policy)))
        .unwrap();
    key(&mut surface, "k", true);
    assert_eq!(surface.take_intents().unwrap(), "[]");
    let _ = present(&mut surface);
    key(&mut surface, "Escape", false);
    assert_eq!(surface.take_intents().unwrap(), "[]");
    key(&mut surface, "k", true);
    let sent = intents(&mut surface);
    let action: serde_json::Value =
        serde_json::from_str(sent[0]["action_json"].as_str().unwrap()).unwrap();
    assert_eq!(
        action,
        serde_json::json!({"type":"cancel_turn","turn_id":"turn-1"})
    );
    let _ = present(&mut surface);
    key(&mut surface, "k", true);
    assert_eq!(surface.take_intents().unwrap(), "[]");
    snapshot.runtime.active_turn_id = Some("turn-2".into());
    surface
        .accept_snapshot(&envelope(2, &snapshot, Some(&policy)))
        .unwrap();
    assert!(surface.present(|_| Err::<(), _>("failed output")).is_err());
    key(&mut surface, "k", true);
    assert_eq!(surface.take_intents().unwrap(), "[]");
    let _ = present(&mut surface);
    key(&mut surface, "k", true);
    let sent = intents(&mut surface);
    let action: serde_json::Value =
        serde_json::from_str(sent[0]["action_json"].as_str().unwrap()).unwrap();
    assert_eq!(action["turn_id"], "turn-2");
}

#[test]
fn committed_disclosure_changes_local_scene_without_platform_intent() {
    let mut log = Log::new(vec![
        user(1, "reveal"),
        assistant(
            2,
            "<details>\n<summary>Reveal</summary>\n\nSecret payload\n\n</details>",
        ),
    ]);
    let mut surface = mounted(80, 20, &mut log, None);
    let frame = present(&mut surface);
    assert!(!frame.text().contains("Secret payload"));
    let (x, y) = frame.find("Reveal").expect("summary painted");
    let summary = frame
        .item_hit_at(x, y)
        .expect("visible disclosure must have a committed hit");
    click(&mut surface, summary);
    assert_eq!(surface.take_intents().unwrap(), "[]");
    assert!(present(&mut surface).text().contains("Secret payload"));
}

#[test]
fn blur_blocks_editor_and_submit_until_focus_returns() {
    let mut surface = SessionSurface::new(SESSION.into(), 40, 8).unwrap();
    let _ = present(&mut surface);
    surface
        .input(r#"{"type":"focus","focused":false}"#)
        .unwrap();
    surface.input(r#"{"type":"text","text":"hidden"}"#).unwrap();
    assert!(!key(&mut surface, "Enter", false));
    assert!(surface.draft().is_empty());
    surface.input(r#"{"type":"focus","focused":true}"#).unwrap();
    surface
        .input(r#"{"type":"text","text":"visible"}"#)
        .unwrap();
    assert_eq!(surface.draft(), "visible");
}

#[test]
fn markdown_transcript_and_configured_editor_share_a_local_surface() {
    let mut log = Log::new(vec![
        user(1, "hi"),
        assistant(2, "# Browser integration\n\nHello **world** 🦀"),
    ]);
    let mut surface = mounted(80, 20, &mut log, None);
    let frame = present(&mut surface);
    assert!(frame.text().contains("Hello world 🦀"), "{}", frame.text());
    let observation = envelope(1, &empty_snapshot(), None);
    assert!(!surface.accept_snapshot(&observation).unwrap());
    surface
        .configure_bindings(r#"{"enter":"newline","ctrl+enter":"submit"}"#)
        .unwrap();
    assert!(
        !surface
            .input(r#"{"type":"text","text":"日本語🦀é"}"#)
            .unwrap()
    );
    assert!(!key(&mut surface, "Backspace", false));
    assert_eq!(surface.draft(), "日本語🦀");
    assert!(!key(&mut surface, "Enter", false));
    assert_eq!(surface.draft(), "日本語🦀\n");
    assert!(key(&mut surface, "Enter", true));
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
        let _ = present(&mut surface);
    }
}

#[test]
fn a_sent_submission_is_a_pending_row_until_its_message_is_incorporated() {
    let mut log = Log::new(conversation(1));
    let mut surface = mounted(60, 16, &mut log, None);
    surface
        .input(r#"{"type":"text","text":"browser prompt"}"#)
        .unwrap();
    let identity = "00000000-0000-4000-8000-000000000009";
    let body = surface.prepare_submission(identity.into()).unwrap();
    assert!(body.contains("browser prompt"));
    let frame = present(&mut surface);
    assert!(frame.text().contains("sending"), "{}", frame.text());
    let receipt = serde_json::json!({"schema_version":1,"outcome":{"type":"submission_accepted","session_id":SESSION,
        "receipt":{"submission_id":identity,"accepted_event_sequence":3}}});
    surface.settle_submission(&receipt.to_string()).unwrap();
    assert!(surface.draft().is_empty());
    assert!(
        present(&mut surface).text().contains("browser prompt"),
        "row kept until incorporated"
    );
    let batch = log.live("continuous", vec![user(3, "browser prompt")]);
    surface.accept_live(&batch).unwrap();
    let frame = present(&mut surface);
    assert_eq!(
        frame.text().matches("browser prompt").count(),
        1,
        "{}",
        frame.text()
    );
    assert!(!frame.text().contains("sending"));
}

#[test]
fn browser_lifecycle_summary_preserves_terminal_reply_and_late_continuation() {
    use bcode_session_view_models::execution::{
        ExecutionAttemptView, ExecutionItemView, ExecutionLifecycleView, ExecutionNotificationView,
        ExecutionStateView,
    };
    let item = |id: &str, state| ExecutionItemView {
        generation: 3,
        id: id.into(),
        native_id: None,
        origin: Some(ExecutionAttemptView {
            submission_id: "s".into(),
            attempt_id: "a".into(),
        }),
        source_work_id: None,
        label: "界 👩‍💻 task".into(),
        state,
        notification: ExecutionNotificationView::Pending,
    };
    let mut snapshot = empty_snapshot();
    snapshot.execution_lifecycle = ExecutionLifecycleView {
        session_id: Some(SESSION.into()),
        generation: Some(3),
        submissions: vec![item("reply", ExecutionStateView::Completed)],
        background: vec![
            item("one", ExecutionStateView::Running),
            item("two", ExecutionStateView::Running),
        ],
        ..ExecutionLifecycleView::default()
    };
    let mut surface = SessionSurface::new(SESSION.into(), 100, 8).unwrap();
    surface
        .accept_snapshot(&envelope(1, &snapshot, None))
        .unwrap();
    let frame = present(&mut surface);
    assert!(
        frame.hits.iter().all(|(id, _)| !id.contains("native-task")),
        "task observations never invent controls"
    );
    assert!(
        surface
            .accessibility()
            .unwrap()
            .contains("Reply complete + 2 background")
    );
    snapshot
        .execution_lifecycle
        .continuations
        .push(item("late", ExecutionStateView::Pending));
    snapshot.execution_lifecycle.uncertain = true;
    surface
        .accept_snapshot(&envelope(2, &snapshot, None))
        .unwrap();
    let _ = present(&mut surface);
    let visible = surface.accessibility().unwrap();
    assert!(visible.contains("Reply complete + 2 background"));
    assert!(visible.contains("1 continuation pending"));
    assert!(visible.contains("coverage unknown"));
    let stale = envelope(1, &snapshot, None);
    assert!(surface.accept_snapshot(&stale).is_err());
}

#[test]
fn a_history_event_beyond_the_browser_bound_degrades_the_transcript_without_retrying() {
    let mut log = Log::new(conversation(2));
    let mut surface = SessionSurface::new(SESSION.into(), 80, 12).unwrap();
    surface
        .accept_snapshot(&envelope(1, &empty_snapshot(), None))
        .unwrap();
    surface.accept_live(&log.live("start", Vec::new())).unwrap();
    let claim: serde_json::Value =
        serde_json::from_str(&surface.next_host_request().unwrap().unwrap()).unwrap();
    // The backend's typed error envelope reaches Rust as the response.
    assert!(
        surface
            .accept_history_page(
                claim["claim"].as_str().unwrap(),
                r#"{"schema_version":1,"error":"history_event_too_large"}"#,
            )
            .unwrap(),
        "the degraded state is shown"
    );
    assert!(surface.next_host_request().unwrap().is_none());
    assert!(
        surface.next_deadline_ms().is_none(),
        "no retry is scheduled for a read that cannot succeed"
    );
    let frame = present(&mut surface);
    assert!(
        frame.text().contains("Transcript history unavailable"),
        "{}",
        frame.text()
    );
    // A transient error is still retried.
    let gap = log.live("gap", Vec::new());
    let _ = surface.accept_live(&gap).unwrap();
    let mut surface = SessionSurface::new(SESSION.into(), 80, 12).unwrap();
    surface
        .accept_snapshot(&envelope(1, &empty_snapshot(), None))
        .unwrap();
    surface.accept_live(&log.live("start", Vec::new())).unwrap();
    let claim: serde_json::Value =
        serde_json::from_str(&surface.next_host_request().unwrap().unwrap()).unwrap();
    assert!(
        !surface
            .accept_history_page(
                claim["claim"].as_str().unwrap(),
                r#"{"schema_version":1,"error":"browser_busy"}"#,
            )
            .unwrap()
    );
    assert!(surface.next_deadline_ms().is_some(), "busy reads back off");
}

#[test]
fn a_gap_while_the_last_newer_page_is_in_flight_rehydrates_the_missed_events() {
    let mut log = Log::new(conversation(700));
    let mut surface = mounted(40, 12, &mut log, None);
    // Read to the session start: the tail is evicted and the window detaches.
    for _ in 0..4000 {
        key(&mut surface, "PageUp", false);
        let _ = pump(&mut surface, &log);
        if present(&mut surface).text().contains("question 0 ") {
            break;
        }
    }
    assert!(present(&mut surface).text().contains("question 0 "));
    // Read down again. The page that reaches the tail is read by the backend, then the feed
    // skips an event (a gap) and delivers the next one before the page's answer arrives.
    let last = log.events.len() as u64;
    let mut reached = false;
    for _ in 0..8000 {
        key(&mut surface, "PageDown", false);
        while let Some(claim) = surface.next_host_request().unwrap() {
            let claim: serde_json::Value = serde_json::from_str(&claim).unwrap();
            let body: serde_json::Value =
                serde_json::from_str(claim["body"].as_str().unwrap()).unwrap();
            let query: SessionHistoryQuery = serde_json::from_value(body["query"].clone()).unwrap();
            let answer = log.page(query);
            if !reached
                && query.direction == SessionHistoryDirection::Forward
                && query.limit > 1
                && answer["page"]["has_more"] == false
            {
                reached = true;
                log.events.push(user(last + 1, "missed question"));
                let _ = surface.accept_live(&log.live("gap", Vec::new())).unwrap();
                let after = log.live("continuous", vec![assistant(last + 2, "after the gap")]);
                let _ = surface.accept_live(&after).unwrap();
            }
            surface
                .accept_history_page(claim["claim"].as_str().unwrap(), &answer.to_string())
                .unwrap();
        }
        let _ = present(&mut surface);
        if reached {
            break;
        }
    }
    assert!(reached, "the last newer page was requested");
    settle(&mut surface, &log);
    if let Some(bar) = present(&mut surface).hit(LATEST) {
        click(&mut surface, bar);
        settle(&mut surface, &log);
    }
    let frame = present(&mut surface);
    assert!(
        frame.text().contains("missed question") && frame.text().contains("after the gap"),
        "{}",
        frame.text()
    );
}

fn transcript_policy(revision: &str, transcript: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"schema_version":1,"revision":revision,"styles":{},"bindings":[],
        "transcript": transcript})
}

#[test]
fn exported_reasoning_visibility_applies_to_the_browser_transcript() {
    let events = vec![
        user(1, "why"),
        event(
            2,
            serde_json::json!({"assistant_reasoning_message": {"text": "pondering deeply"}}),
        ),
        assistant(3, "because"),
    ];
    let shown = transcript_policy("shown", serde_json::json!({"reasoning": "all"}));
    let mut log = Log::new(events.clone());
    let mut visible = mounted(60, 16, &mut log, Some(&shown));
    settle(&mut visible, &log);
    let visible = present(&mut visible).text();
    let hidden = transcript_policy(
        "hidden",
        serde_json::json!({"reasoning": "hidden", "reduce_motion": true,
            "streaming": {"enabled": false, "curve": "linear", "graphemes_per_second": 300, "max_lag_ms": 40}}),
    );
    let mut log = Log::new(events);
    let mut surface = mounted(60, 16, &mut log, Some(&hidden));
    settle(&mut surface, &log);
    let frame = present(&mut surface).text();
    assert!(frame.contains("because"), "{frame}");
    assert_ne!(visible, frame, "the reasoning unit is hidden: {visible}");
    assert!(frame.lines().count() <= visible.lines().count());
    assert!(
        !frame.contains("pondering") && !frame.to_lowercase().contains("thinking"),
        "{frame}"
    );
}

#[test]
fn an_omitted_policy_is_accepted_only_for_the_installed_revision() {
    let policy = transcript_policy("installed", serde_json::json!({}));
    let mut log = Log::new(conversation(1));
    let mut surface = mounted(60, 16, &mut log, Some(&policy));
    assert_eq!(surface.policy_revision(), Some("installed"));
    let slim = |revision: u64, named: &str| {
        serde_json::json!({"schema_version": 1, "epoch": EPOCH, "revision": revision,
            "snapshot": empty_snapshot(), "policy_revision": named})
        .to_string()
    };
    // The unchanged observation without its policy is the same observation.
    assert!(!surface.accept_snapshot(&slim(1, "installed")).unwrap());
    let mut changed = empty_snapshot();
    changed.title = Some("next".into());
    let next = serde_json::json!({"schema_version": 1, "epoch": EPOCH, "revision": 2,
        "snapshot": changed, "policy_revision": "installed"});
    assert!(surface.accept_snapshot(&next.to_string()).unwrap());
    assert_eq!(surface.policy_revision(), Some("installed"));
    assert!(surface.accept_snapshot(&slim(3, "other")).is_err());
    assert!(
        surface
            .accept_snapshot(&envelope(4, &empty_snapshot(), None))
            .is_err(),
        "a policy in effect cannot silently disappear"
    );
}
