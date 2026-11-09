use std::collections::BTreeMap;
use std::time::Instant;

use bcode_config::{TuiDiffViewerConfig, TuiDiffViewerLayout};
use bcode_plugin::PluginPresentationDeclaration;
use bcode_plugin::presentation::PluginFactDeclaration;
use bcode_presentation_policy::models::{FactType, FactValue};
use bcode_session_view_models::{
    ToolInvocationView, ToolInvocationViewStatus, ToolTimingView, TranscriptViewItem,
    TranscriptViewItemId, TranscriptViewItemKind,
};
use bcode_tui_transcript::layout::context::DiffLayout;
use bcode_tui_transcript::navigation::MotionConfig;
use bcode_tui_transcript::units::structure::PluginFactSource;

use super::{DeclaredPluginFacts, diff_viewer_options};
use crate::app::BmuxApp;
use crate::transcript_v2_host::TranscriptV2Host;

fn tool_item(producer: &str, tool: &str, arguments: &str) -> TranscriptViewItem {
    TranscriptViewItem {
        id: TranscriptViewItemId::tool("call"),
        revision: 1,
        sequence: Some(1),
        timestamp_ms: None,
        output_location: None,
        streaming: false,
        kind: TranscriptViewItemKind::ToolInvocation {
            tool: Box::new(ToolInvocationView {
                tool_call_id: "call".to_owned(),
                producer_plugin_id: Some(producer.to_owned()),
                tool_name: Some(tool.to_owned()),
                arguments_json: Some(arguments.to_owned()),
                working_directory: None,
                request_draft: None,
                status: ToolInvocationViewStatus::Finished,
                result_text: None,
                is_error: None,
                result: None,
                presentation: None,
                timing: ToolTimingView {
                    started_at_ms: None,
                    finished_at_ms: None,
                    timeout_ms: None,
                    timed_out: None,
                    duration_ms: None,
                },
            }),
        },
    }
}

fn declarations() -> BTreeMap<String, PluginPresentationDeclaration> {
    let fact = |name: &str, value_type, tool: Option<&str>, argument: &str| PluginFactDeclaration {
        name: name.to_owned(),
        value_type,
        tool: tool.map(str::to_owned),
        schema: None,
        argument: Some(argument.to_owned()),
        payload: None,
    };
    BTreeMap::from([(
        "shell".to_owned(),
        PluginPresentationDeclaration {
            facts: vec![
                fact(
                    "shell.background",
                    FactType::Bool,
                    Some("run"),
                    "background",
                ),
                fact("shell.args", FactType::Int, None, "args"),
                fact(
                    "shell.other_tool",
                    FactType::Bool,
                    Some("other"),
                    "background",
                ),
            ],
            ..PluginPresentationDeclaration::default()
        },
    )])
}

#[test]
fn declared_facts_come_from_the_producing_plugins_arguments() {
    let facts = DeclaredPluginFacts::new(&declarations());
    let item = tool_item("shell", "run", r#"{"background": true, "args": [1, 2, 3]}"#);
    assert_eq!(
        facts.facts(&item, "shell", Some("run")),
        BTreeMap::from([
            ("shell.args".to_owned(), FactValue::Int(3)),
            ("shell.background".to_owned(), FactValue::Bool(true)),
        ])
    );
    assert!(
        facts.facts(&item, "other", Some("run")).is_empty(),
        "another producer's units never carry the facts"
    );
    let invalid = tool_item("shell", "run", "not json");
    assert!(facts.facts(&invalid, "shell", Some("run")).is_empty());
    assert_eq!(
        facts.generation(),
        DeclaredPluginFacts::new(&declarations()).generation(),
        "the generation is deterministic"
    );
    assert_ne!(
        facts.generation(),
        DeclaredPluginFacts::new(&BTreeMap::new()).generation()
    );
}

#[test]
fn diff_options_follow_the_effective_diff_viewer_configuration() {
    let options = diff_viewer_options(TuiDiffViewerConfig {
        layout: TuiDiffViewerLayout::SideBySide,
        side_by_side_breakpoint: 90,
    });
    assert_eq!(options.layout, DiffLayout::SideBySide);
    assert_eq!(options.side_by_side_breakpoint, 90);
}

#[test]
fn render_inputs_are_applied_once_and_follow_changes() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    let mut host = TranscriptV2Host::new();
    let at = Instant::now();
    assert!(host.sync_render_inputs(&mut app, at), "first sync applies");
    assert!(!host.sync_render_inputs(&mut app, at), "unchanged inputs");
    let structure = host.engine().structure().config().key();

    assert!(app.apply_user_structure_rules(vec![
        bcode_presentation_policy::models::PresentationRule::default()
    ]));
    assert!(host.sync_render_inputs(&mut app, at));
    assert_ne!(host.engine().structure().config().key(), structure);

    let mut config = app.tui_config().clone();
    config.reduce_motion = true;
    app.apply_tui_config(config);
    let _ = host.sync_render_inputs(&mut app, at);
    assert_eq!(host.engine().motion(), MotionConfig::INSTANT);

    assert!(host.set_markdown_details("transcript:item:x:details".to_owned(), true));
    assert!(!host.set_markdown_details("transcript:item:x:details".to_owned(), true));
    assert!(host.sync_render_inputs(&mut app, at));
    drop(app);
    assert_eq!(
        host.engine()
            .render_config()
            .markdown
            .details_open
            .get("transcript:item:x:details"),
        Some(&true)
    );
}
