//! Plugin visual scenarios (E1, E3, C36).
//!
//! The scenarios load the bundled filesystem plugin through the production static plugin host
//! ([`TranscriptScenarioDriver::load_bundled_plugins`]) and record a file read whose result is
//! the plugin's artifact, so the plugin's visual adapter presents the tool result instead of
//! the canonical fallback. They run on the V2 engine in the application (`v2_plugin_visuals` in
//! `mod.rs`).

use bcode_session_models::SessionEventKind;

use super::Scenario;
use super::driver::{ScenarioPlugin, TranscriptScenarioDriver};
use super::interaction::{InteractionScenario, attach_all, find, right_of};
use super::session::ScenarioSession;

const WIDTH: u16 = 80;
const TALL: u16 = 40;
/// The read file's source.
const SOURCE: &str = "pub fn alpha() {}\npub fn beta() {}";
/// The tool's model output: the canonical fallback shows it, the adapter does not.
const FALLBACK: &str = "FALLBACKOUTPUT";

/// A session of one turn plus a file read recorded with the filesystem plugin's artifact.
fn read_session() -> ScenarioSession {
    let mut session = ScenarioSession::with_turns(1, 1);
    session.push_kind(SessionEventKind::ToolCallRequested {
        tool_call_id: "call-read".to_owned(),
        producer_plugin_id: Some("bcode.filesystem".to_owned()),
        tool_name: "read".to_owned(),
        arguments_json: serde_json::json!({ "path": "test.rs" }).to_string(),
        working_directory: None,
    });
    session.push_kind(SessionEventKind::ToolInvocationResultRecorded {
        record: bcode_session_models::ToolInvocationResultRecord {
            invocation_id: "call-read".to_owned(),
            model_output: FALLBACK.to_owned(),
            is_error: false,
            presentation: None,
            result: Some(bcode_session_models::ToolInvocationResult::Artifact {
                artifact: Box::new(bcode_session_models::ToolArtifact {
                    artifact_id: "call-read".to_owned(),
                    producer_plugin_id: "bcode.filesystem".to_owned(),
                    schema: "bcode.filesystem.read".to_owned(),
                    schema_version: 1,
                    tool_call_id: Some("call-read".to_owned()),
                    title: Some("File contents".to_owned()),
                    metadata: serde_json::json!({
                        "path": "test.rs",
                        "contents": SOURCE,
                        "start_line": 1,
                    }),
                    refs: Vec::new(),
                }),
            }),
            content: Vec::new(),
        },
    });
    session
}

/// The read session attached with the filesystem plugin loaded and presented.
fn read_scenario<D: TranscriptScenarioDriver>() -> Scenario<D> {
    let mut scenario = attach_all::<D>(read_session(), WIDTH, TALL);
    scenario
        .driver
        .load_bundled_plugins(&[ScenarioPlugin::Filesystem]);
    scenario.present();
    scenario
}

/// E1: with the producing plugin loaded, its visual adapter presents the tool result (the file
/// contents) in place of the canonical fallback, inside the host's tool chrome.
pub fn e1_plugin_visual_presents_adapter_rows_instead_of_the_fallback<
    D: TranscriptScenarioDriver,
>() {
    // Without the plugin the canonical fallback shows the model output.
    let mut fallback = attach_all::<D>(read_session(), WIDTH, TALL);
    let frame = fallback.present();
    fallback.check(frame.contains(FALLBACK), || {
        "the canonical fallback does not show the model output".to_owned()
    });
    let mut scenario = read_scenario::<D>();
    let frame = scenario.present();
    scenario.check(
        frame.contains("pub fn alpha() {}") && frame.contains("pub fn beta() {}"),
        || "the adapter's rows are not presented".to_owned(),
    );
    scenario.check(!frame.contains(FALLBACK), || {
        "the canonical fallback still renders next to the adapter rows".to_owned()
    });
}

/// C36 (E3): copying adapter rows copies the adapter's source (the file's lines), not the
/// rendered rows (line numbers, gutters); the two source lines join with one newline.
pub fn c36_plugin_visual_copy_uses_the_adapter_source<D: TranscriptScenarioDriver>() {
    let mut scenario = read_scenario::<D>();
    let from = find(&scenario, "pub fn alpha");
    let to = find(&scenario, "pub fn beta");
    scenario.drag_select(from, right_of(to, 16));
    scenario.present();
    let selected = scenario.selected().unwrap_or_default();
    scenario.check(selected.contains(SOURCE), || {
        format!("the selection does not copy the adapter source: {selected:?}")
    });
    scenario.check(
        !selected.contains('│') && !selected.contains(FALLBACK),
        || format!("the selection copied rendered chrome: {selected:?}"),
    );
}
