//! TUI tests.

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use bcode_agent_profile::AgentInfo;
use bcode_client::AttachedSessionHistory;
use bcode_config::{
    TuiAccentTransitionCurve, TuiAccentTransitionMode, TuiAgentAccentPolicy, TuiConfig,
    TuiThemeConfig, TuiThemeVariant, TuiThinkingConfig,
};
use bcode_session_models::{
    ClientId, SessionEvent, SessionEventKind, SessionId, SessionInputHistoryEntry,
    SessionProjectionKind, SessionSummary, SessionTitleSource, SessionTokenUsage,
    SessionTraceEvent, SessionTracePayload, SessionTracePhase, ToolArtifactRef,
    ToolInvocationResult, TurnPermissionMode, TurnToolPolicy,
};
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_text_edit::TextMotion;
use bmux_tui::buffer::Buffer;
use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};

#[test]
fn launch_options_build_exact_portable_turn_options() {
    let options = crate::TuiLaunchOptions {
        permission_mode: TurnPermissionMode::Bypass,
        tool_policy: TurnToolPolicy::Disabled,
    }
    .turn_execution_options();

    assert_eq!(options.permission_mode, TurnPermissionMode::Bypass);
    assert_eq!(options.tools, TurnToolPolicy::Disabled);
    assert_eq!(options.agent_profile, None);
}

#[test]
fn launch_options_compose_without_overwriting_other_turn_options() {
    let launch = crate::TuiLaunchOptions {
        permission_mode: TurnPermissionMode::Bypass,
        tool_policy: TurnToolPolicy::Disabled,
    };
    let mut options = launch.turn_execution_options();
    options.agent_profile = Some("plan".to_owned());
    options.provider_plugin_id = Some("provider".to_owned());
    options.model_id = Some("model".to_owned());
    options.reasoning = Some(Box::new(bcode_session_models::TurnReasoningOptions {
        effort: Some("high".to_owned()),
        summary: Some("detailed".to_owned()),
    }));

    assert_eq!(options.permission_mode, TurnPermissionMode::Bypass);
    assert_eq!(options.tools, TurnToolPolicy::Disabled);
    assert_eq!(options.agent_profile.as_deref(), Some("plan"));
    assert_eq!(options.provider_plugin_id.as_deref(), Some("provider"));
    assert_eq!(options.model_id.as_deref(), Some("model"));
    assert_eq!(
        options
            .reasoning
            .as_ref()
            .and_then(|value| value.effort.as_deref()),
        Some("high")
    );
}

#[test]
fn composer_submission_carries_launch_execution_defaults() {
    let (event_sender, event_receiver) = crate::history_flow::session_stream_channel();
    let mut chat = crate::session_flow::ActiveChat {
        app: crate::app::BmuxApp::new_with_history(None, &[], &[]),
        agents: crate::session_flow::AgentCatalog::default(),
        attachment: crate::session_flow::ChatSessionAttachment::Draft,
        event_sender,
        event_receiver,
        event_task: None,
        opening_session_progress: None,
        pending_effects: crate::effects::TuiEffectQueue::default(),
    };
    chat.app.replace_composer_with("test mode");
    chat.app.stage_submission();
    let launch = crate::TuiLaunchOptions {
        permission_mode: TurnPermissionMode::Bypass,
        tool_policy: TurnToolPolicy::Disabled,
    };

    assert!(crate::composer_flow::stage_session_message(
        std::path::Path::new("."),
        launch,
        &mut chat,
        bcode_ipc::PromptPlacement::FollowUp,
    ));
    let execution = chat
        .pending_effects
        .queued_execution_options()
        .expect("queued submit execution options");
    assert_eq!(execution.permission_mode, TurnPermissionMode::Bypass);
    assert_eq!(execution.tools, TurnToolPolicy::Disabled);
    drop(chat);
}

#[test]
fn skill_invocation_carries_launch_execution_defaults() {
    let (event_sender, event_receiver) = crate::history_flow::session_stream_channel();
    let session_id = SessionId::new();
    let mut chat = crate::session_flow::ActiveChat {
        app: crate::app::BmuxApp::new_with_history(Some(session_id), &[], &[]),
        agents: crate::session_flow::AgentCatalog::default(),
        attachment: crate::session_flow::ChatSessionAttachment::Attached { session_id },
        event_sender,
        event_receiver,
        event_task: None,
        opening_session_progress: None,
        pending_effects: crate::effects::TuiEffectQueue::default(),
    };
    let launch = crate::TuiLaunchOptions {
        permission_mode: TurnPermissionMode::Bypass,
        tool_policy: TurnToolPolicy::Disabled,
    };

    crate::skill_flow::start_skill_action(
        std::path::Path::new("."),
        launch,
        &mut chat,
        crate::effects::SkillActionKind::Invoke,
        bcode_skill_models::SkillId::new("test-skill"),
        "arguments".to_owned(),
    );
    let execution = chat
        .pending_effects
        .queued_execution_options()
        .expect("queued skill execution options");
    assert_eq!(execution.permission_mode, TurnPermissionMode::Bypass);
    assert_eq!(execution.tools, TurnToolPolicy::Disabled);
    drop(chat);
}

#[test]
fn execution_mode_indicator_remains_visible_with_transient_status() {
    let mut app = crate::app::BmuxApp::new_with_history(None, &[], &[]);
    app.set_execution_mode_indicator(Some("DANGER: PERMISSION BYPASS ACTIVE".to_owned()));
    app.set_status("starting daemon…".to_owned());

    assert_eq!(
        app.execution_mode_indicator(),
        Some("DANGER: PERMISSION BYPASS ACTIVE")
    );
    assert_eq!(app.status(), "starting daemon…");
    drop(app);
}

fn context_occupancy(tokens: u64) -> bcode_session_models::RequestContextOccupancy {
    bcode_session_models::RequestContextOccupancy {
        context_epoch: 0,
        observation_sequence: 1,
        observation: bcode_session_models::RequestContextObservation {
            request: bcode_session_models::ModelRequestIdentity {
                provider_plugin_id: "provider.example".to_owned(),
                requested_model_id: None,
                effective_model_id: "model-example".to_owned(),
                request_id: "request".to_owned(),
                model_turn_id: "turn-1".to_owned(),
                round: 0,
                request_fingerprint: "fingerprint".to_owned(),
                effective_auth_profile: None,
                context_format_version: None,
                compatibility_key: None,
                context_epoch: 0,
            },
            context_through_sequence: 1,
            context_tokens: bcode_session_models::RequestContextTokenCount::ProviderExact(tokens),
            local_estimate: bcode_session_models::LocalContextEstimate {
                tokens,
                algorithm_version: 1,
            },
        },
    }
}

use super::{
    activity::ActivityState,
    app::{BmuxApp, KeyActivationOutcome},
    input,
    input::KeyRequest,
    keymap::{BmuxAction, BmuxKeyActivation, BmuxKeyBinding, BmuxKeyMap, BmuxScope},
    pending_submissions::PendingSubmissions,
    render, slash_palette, slash_palette_render,
    time_format::{format_duration_nanos, format_millis},
};

fn theme_transition_config(curve: TuiAccentTransitionCurve) -> TuiConfig {
    TuiConfig {
        theme: TuiThemeConfig {
            accent_transition: TuiAccentTransitionMode::Transition,
            accent_transition_ms: Some(100),
            accent_transition_curve: curve,
            ..TuiThemeConfig::default()
        },
        ..TuiConfig::default()
    }
}

fn disable_theme_transition(app: &mut BmuxApp) {
    app.apply_tui_config(TuiConfig {
        theme: TuiThemeConfig {
            accent_transition: TuiAccentTransitionMode::Immediate,
            ..TuiThemeConfig::default()
        },
        ..TuiConfig::default()
    });
}

fn agent_infos(items: &[(&str, bool)]) -> Vec<AgentInfo> {
    agent_infos_with_accents(
        &items
            .iter()
            .map(|(id, is_default)| (*id, *is_default, None))
            .collect::<Vec<_>>(),
    )
}

fn agent_infos_with_accents(items: &[(&str, bool, Option<&str>)]) -> Vec<AgentInfo> {
    items
        .iter()
        .map(|(id, is_default, accent)| AgentInfo {
            id: (*id).to_owned(),
            name: (*id).to_owned(),
            description: String::new(),
            badge: None,
            accent: accent.map(ToOwned::to_owned),
            aliases: Vec::new(),
            is_default: *is_default,
        })
        .collect()
}

fn alt_char(ch: char) -> KeyStroke {
    KeyStroke {
        key: KeyCode::Char(ch),
        modifiers: Modifiers {
            alt: true,
            ..Modifiers::NONE
        },
    }
}

fn ctrl_key(ch: char) -> KeyStroke {
    ctrl_key_code(KeyCode::Char(ch))
}

fn ctrl_key_code(key: KeyCode) -> KeyStroke {
    KeyStroke {
        key,
        modifiers: Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        },
    }
}

fn event(session_id: SessionId, sequence: u64, kind: SessionEventKind) -> SessionEvent {
    SessionEvent {
        schema_version: 1,
        sequence,
        timestamp_ms: 1,
        session_id,
        provenance: None,
        kind,
    }
}

fn key(key: KeyCode) -> KeyStroke {
    KeyStroke {
        key,
        modifiers: Modifiers::NONE,
    }
}

fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
    MouseEvent::new(kind, Point::new(x, y))
}

fn output_line_y(buffer: &Buffer, needle: &str) -> Option<u16> {
    (0..buffer.area().height).find(|row| {
        buffer
            .row_symbols(*row)
            .is_some_and(|line| line.contains(needle))
    })
}

fn render_app_text(app: &mut BmuxApp) -> String {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 40));
    let mut frame = Frame::new(&mut buffer);
    render::render(app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    rendered_text(&buffer)
}

fn rendered_text(buffer: &Buffer) -> String {
    (0..buffer.area().height)
        .filter_map(|row| buffer.row_symbols(row))
        .collect::<Vec<_>>()
        .join("\n")
}

#[path = "child_monitor_tests.rs"]
mod child_monitor_tests;

fn session_summary(session_id: SessionId) -> SessionSummary {
    SessionSummary {
        origin: None,
        id: session_id,
        parent_id: None,
        name: Some("Opened session".to_owned()),
        explicit_name: Some("Opened session".to_owned()),
        derived_title: None,
        automatic_title: None,
        automatic_title_source_sequence: None,
        title_source: SessionTitleSource::Explicit,
        client_count: 1,
        created_at_ms: 1,
        updated_at_ms: 2,
        working_directory: "/tmp/bcode-tui-test".into(),
        import: None,
        execution: None,
        location: None,
    }
}

fn shell_plugin_host() -> bcode_plugin::PluginHost {
    let bundled = [bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/shell-plugin/bcode-plugin.toml"),
        bcode_shell_plugin::static_plugin(),
    )];
    let selected = bcode_plugin::filter_selected_static_plugins(
        &bundled,
        &bcode_plugin::PluginSelection::all_enabled(),
    )
    .expect("static shell plugin manifest should parse");
    bcode_plugin::PluginHost::load_static_plugins(&selected)
        .expect("static shell plugin should load")
}

fn shell_run_artifact() -> bcode_session_models::ToolArtifact {
    bcode_session_models::ToolArtifact {
        artifact_id: "call-shell-shell-run".to_owned(),
        producer_plugin_id: "bcode.shell".to_owned(),
        schema: "bcode.shell.run".to_owned(),
        schema_version: 1,
        tool_call_id: Some("call-shell".to_owned()),
        title: Some("Shell run".to_owned()),
        metadata: serde_json::json!({
            "mode": "terminal",
            "exit_code": 0,
            "timed_out": false,
            "cancelled": false,
            "duration_ms": 12,
            "output_tail": "shell raw output\n",
            "output_truncated": false,
            "output_bytes": 17,
            "retained_output_bytes": 17,
            "columns": 80,
            "rows": 24
        }),
        refs: Vec::new(),
    }
}

fn shift_key(key: KeyCode) -> KeyStroke {
    KeyStroke {
        key,
        modifiers: Modifiers {
            shift: true,
            ..Modifiers::NONE
        },
    }
}

#[test]
fn theme_transition_curves_shape_midpoint_progress() {
    let target = bmux_tui::style::Color::Rgb(100, 0, 0);
    let started_at = Instant::now();

    let mut linear = BmuxApp::new_with_history(None, &[], &[]);
    linear.apply_tui_config(theme_transition_config(TuiAccentTransitionCurve::Linear));
    assert_eq!(
        linear.animated_accent(target, started_at),
        bmux_tui::style::Color::Rgb(100, 116, 139)
    );
    assert_eq!(
        linear.animated_accent(target, started_at + Duration::from_millis(50)),
        bmux_tui::style::Color::Rgb(100, 58, 70)
    );
    drop(linear);

    let mut ease_in = BmuxApp::new_with_history(None, &[], &[]);
    ease_in.apply_tui_config(theme_transition_config(TuiAccentTransitionCurve::EaseIn));
    assert_eq!(
        ease_in.animated_accent(target, started_at),
        bmux_tui::style::Color::Rgb(100, 116, 139)
    );
    assert_eq!(
        ease_in.animated_accent(target, started_at + Duration::from_millis(50)),
        bmux_tui::style::Color::Rgb(100, 102, 122)
    );
    drop(ease_in);

    let mut ease_out = BmuxApp::new_with_history(None, &[], &[]);
    ease_out.apply_tui_config(theme_transition_config(TuiAccentTransitionCurve::EaseOut));
    assert_eq!(
        ease_out.animated_accent(target, started_at),
        bmux_tui::style::Color::Rgb(100, 116, 139)
    );
    assert_eq!(
        ease_out.animated_accent(target, started_at + Duration::from_millis(50)),
        bmux_tui::style::Color::Rgb(100, 15, 18)
    );
    drop(ease_out);
}

#[test]
fn immediate_theme_transition_ignores_curve() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    app.apply_tui_config(TuiConfig {
        theme: TuiThemeConfig {
            accent_transition: TuiAccentTransitionMode::Immediate,
            accent_transition_ms: Some(100),
            accent_transition_curve: TuiAccentTransitionCurve::EaseIn,
            ..TuiThemeConfig::default()
        },
        ..TuiConfig::default()
    });

    assert_eq!(
        app.animated_accent(bmux_tui::style::Color::Rgb(1, 2, 3), Instant::now()),
        bmux_tui::style::Color::Rgb(1, 2, 3)
    );
    drop(app);
}

fn compaction_trace_event(
    session_id: SessionId,
    sequence: u64,
    phase: SessionTracePhase,
    reason: &str,
    compacted: bool,
    message: &str,
) -> SessionEvent {
    SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence,
        timestamp_ms: sequence,
        session_id,
        provenance: None,
        kind: SessionEventKind::TraceEvent {
            trace: Box::new(SessionTraceEvent {
                timestamp_ms: sequence,
                turn_id: Some("turn-1".to_owned()),
                phase,
                payload: SessionTracePayload::ContextCompaction {
                    reason: reason.to_owned(),
                    projected_context_chars: 0,
                    compacted,
                    message: Some(message.to_owned()),
                },
            }),
        },
    }
}

#[test]
fn compaction_trace_lifecycle_preserves_status_and_projects_active_progress() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
    app.set_status("existing notice".to_owned());

    let event = |sequence, phase, reason, compacted, message| {
        compaction_trace_event(session_id, sequence, phase, reason, compacted, message)
    };

    app.absorb_session_event(&event(
        1,
        SessionTracePhase::ContextCompactionDiagnostic,
        "strategy_resolved",
        false,
        "automatic compaction strategy OverflowOnly",
    ));
    assert_eq!(app.activity(), &ActivityState::Idle);
    assert_eq!(app.status(), "existing notice");

    app.absorb_session_event(&SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence: 0,
        timestamp_ms: 0,
        session_id,
        provenance: None,
        kind: SessionEventKind::ModelTurnStarted {
            turn_id: "turn-1".to_owned(),
        },
    });
    app.absorb_session_event(&event(
        2,
        SessionTracePhase::ContextCompactionStarted,
        "compaction_started",
        false,
        "older context",
    ));
    assert_eq!(
        app.activity(),
        &ActivityState::Compacting {
            detail: "older context".to_owned()
        }
    );
    assert_eq!(app.status(), "existing notice");

    for (sequence, message) in [
        (3, "Accepted chunk 1; more context remains"),
        (4, "Resuming accepted checkpoint"),
        (5, "Repairing summary"),
    ] {
        app.absorb_session_event(&event(
            sequence,
            SessionTracePhase::ContextCompactionDiagnostic,
            "opaque_reason",
            false,
            message,
        ));
        assert_eq!(
            app.activity(),
            &ActivityState::Compacting {
                detail: message.to_owned()
            }
        );
        assert_eq!(app.status(), "existing notice");
    }

    app.absorb_session_event(&event(
        4,
        SessionTracePhase::ContextCompactionStarted,
        "stale_start",
        false,
        "out-of-order progress must not overwrite repair",
    ));
    assert_eq!(
        app.activity(),
        &ActivityState::Compacting {
            detail: "Repairing summary".to_owned()
        }
    );

    app.absorb_session_event(&event(
        6,
        SessionTracePhase::ContextCompactionFinished,
        "compaction_finished",
        true,
        "compacted context",
    ));
    assert_eq!(app.activity(), &ActivityState::PreparingModelRequest);
    assert_eq!(app.status(), "existing notice");

    app.absorb_session_event(&event(
        7,
        SessionTracePhase::ContextCompactionStarted,
        "compaction_started",
        false,
        "older context",
    ));
    app.absorb_session_event(&event(
        8,
        SessionTracePhase::ContextCompactionSkipped,
        "nothing_to_compact",
        false,
        "nothing new to compact",
    ));
    assert_eq!(app.activity(), &ActivityState::PreparingModelRequest);
    assert_eq!(app.status(), "existing notice");
    drop(app);
}

#[test]
fn compaction_trace_unattributed_diagnostics_cannot_claim_the_active_turn() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
    app.absorb_session_event(&SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence: 1,
        timestamp_ms: 1,
        session_id,
        provenance: None,
        kind: SessionEventKind::ModelTurnStarted {
            turn_id: "turn-1".to_owned(),
        },
    });
    let mut sequence = 2;
    for phase in [
        SessionTracePhase::ContextCompactionStarted,
        SessionTracePhase::ContextCompactionDiagnostic,
        SessionTracePhase::ContextCompactionFinished,
    ] {
        let mut event = compaction_trace_event(
            session_id,
            sequence,
            phase,
            "opaque_reason",
            false,
            "Accepted chunk",
        );
        if let SessionEventKind::TraceEvent { trace } = &mut event.kind {
            trace.turn_id = None;
        }
        app.absorb_session_event(&event);
        assert_eq!(app.activity(), &ActivityState::PreparingModelRequest);
        sequence += 1;
    }
    // Once the producer retains the originating ID, the same payload is visible.
    app.absorb_session_event(&compaction_trace_event(
        session_id,
        sequence,
        SessionTracePhase::ContextCompactionStarted,
        "opaque_reason",
        false,
        "Resuming accepted chunks",
    ));
    assert_eq!(
        app.activity(),
        &ActivityState::Compacting {
            detail: "Resuming accepted chunks".to_owned(),
        }
    );
    drop(app);
}

#[test]
fn compaction_trace_cancellation_and_restart_reject_stale_activity() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
    let event = |sequence, phase, reason, compacted, message| {
        compaction_trace_event(session_id, sequence, phase, reason, compacted, message)
    };
    let turn_event = |sequence, kind| SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence,
        timestamp_ms: sequence,
        session_id,
        provenance: None,
        kind,
    };
    app.absorb_session_event(&turn_event(
        8,
        SessionEventKind::ModelTurnStarted {
            turn_id: "turn-1".to_owned(),
        },
    ));
    app.absorb_session_event(&event(
        9,
        SessionTracePhase::ContextCompactionStarted,
        "started",
        false,
        "more context",
    ));
    app.absorb_session_event(&turn_event(
        10,
        SessionEventKind::ModelTurnCancelRequested {
            turn_id: "turn-1".to_owned(),
            requested_at_ms: Some(10),
            client_id: None,
        },
    ));
    let cancelling = app.activity().clone();
    app.absorb_session_event(&event(
        11,
        SessionTracePhase::ContextCompactionDiagnostic,
        "accepted",
        false,
        "stale accepted chunk",
    ));
    assert_eq!(app.activity(), &cancelling);
    app.absorb_session_event(&turn_event(
        12,
        SessionEventKind::ModelTurnFinished {
            turn_id: "turn-1".to_owned(),
            outcome: bcode_session_models::ModelTurnOutcome::Cancelled,
            message: None,
        },
    ));
    let terminal = app.activity().clone();
    app.absorb_session_event(&event(
        13,
        SessionTracePhase::ContextCompactionStarted,
        "started",
        false,
        "stale start",
    ));
    assert_eq!(app.activity(), &terminal);
    app.absorb_session_event(&turn_event(
        14,
        SessionEventKind::ModelTurnStarted {
            turn_id: "turn-2".to_owned(),
        },
    ));
    app.absorb_session_event(&event(
        15,
        SessionTracePhase::ContextCompactionStarted,
        "started",
        false,
        "stale old turn",
    ));
    assert_eq!(app.activity(), &ActivityState::PreparingModelRequest);
    let status = app.status().to_owned();
    let mut resumed = event(
        16,
        SessionTracePhase::ContextCompactionStarted,
        "resuming",
        false,
        "Resuming accepted chunks",
    );
    if let SessionEventKind::TraceEvent { trace } = &mut resumed.kind {
        trace.turn_id = Some("turn-2".to_owned());
    }
    app.absorb_session_event(&resumed);
    assert_eq!(
        app.activity(),
        &ActivityState::Compacting {
            detail: "Resuming accepted chunks".to_owned()
        }
    );
    assert_eq!(app.status(), status);
    drop(app);
}

#[test]
fn provider_tool_call_delta_trace_does_not_replace_status() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
    app.set_status("existing notice".to_owned());
    app.absorb_session_event(&SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence: 1,
        timestamp_ms: 1,
        session_id,
        provenance: None,
        kind: SessionEventKind::TraceEvent {
            trace: Box::new(SessionTraceEvent {
                timestamp_ms: 0,
                turn_id: Some("turn-1".to_owned()),
                phase: SessionTracePhase::ModelProviderEvent,
                payload: SessionTracePayload::ProviderEvent {
                    event_type: "tool_call_delta".to_owned(),
                    detail: None,
                },
            }),
        },
    });

    assert_eq!(app.status(), "existing notice");
    drop(app);
}

#[test]
fn model_turn_progress_uses_specific_activity_phases_without_overwriting_notices() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
    app.set_status("existing notice".to_owned());

    app.absorb_session_event(&SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence: 1,
        timestamp_ms: 1,
        session_id,
        provenance: None,
        kind: SessionEventKind::ModelTurnStarted {
            turn_id: "turn-1".to_owned(),
        },
    });
    assert_eq!(app.activity(), &ActivityState::PreparingModelRequest);
    assert_eq!(app.status(), "existing notice");

    app.absorb_session_event(&SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence: 2,
        timestamp_ms: 2,
        session_id,
        provenance: None,
        kind: SessionEventKind::TraceEvent {
            trace: Box::new(SessionTraceEvent {
                timestamp_ms: 2,
                turn_id: Some("turn-1".to_owned()),
                phase: SessionTracePhase::ModelRequestBuilt,
                payload: SessionTracePayload::ModelRequestBuilt {
                    provider: "openai".to_owned(),
                    model: "gpt".to_owned(),
                    agent_id: "build".to_owned(),
                    message_count: 1,
                    tool_count: 0,
                    system_prompt_chars: 0,
                    prompt_cache_mode: "off".to_owned(),
                    conversation_reuse_mode: "off".to_owned(),
                    uses_previous_provider_response: false,
                    metadata: BTreeMap::new(),
                    request: None,
                },
            }),
        },
    });
    assert_eq!(
        app.activity(),
        &ActivityState::StartingProviderRequest {
            provider: "openai".to_owned(),
            round: None,
        }
    );

    app.absorb_session_event(&SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence: 3,
        timestamp_ms: 3,
        session_id,
        provenance: None,
        kind: SessionEventKind::TraceEvent {
            trace: Box::new(SessionTraceEvent {
                timestamp_ms: 3,
                turn_id: Some("turn-1".to_owned()),
                phase: SessionTracePhase::ModelProviderRoundStarted,
                payload: SessionTracePayload::ProviderRound {
                    provider_turn_id: Some("provider-turn-1".to_owned()),
                    provider: "openai".to_owned(),
                    round: Some(1),
                    stop_reason: None,
                    duration_ms: None,
                    error: None,
                },
            }),
        },
    });
    assert_eq!(
        app.activity(),
        &ActivityState::WaitingForProvider {
            provider: "openai".to_owned(),
            round: Some(1),
        }
    );
    assert_eq!(app.status(), "existing notice");
    drop(app);
}

#[test]
fn provider_tool_call_progress_status_formats_bytes() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
    app.absorb_session_event(&SessionEvent {
        schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        sequence: 1,
        timestamp_ms: 1,
        session_id,
        provenance: None,
        kind: SessionEventKind::TraceEvent {
            trace: Box::new(SessionTraceEvent {
                timestamp_ms: 0,
                turn_id: Some("turn-1".to_owned()),
                phase: SessionTracePhase::ModelProviderEvent,
                payload: SessionTracePayload::ProviderStreamEvent(
                    bcode_session_models::ProviderStreamEvent::ToolCallProgress {
                        tool_call_id: "call-1".to_owned(),
                        tool_name: "example.write".to_owned(),
                        argument_bytes: 1536,
                    },
                ),
            }),
        },
    });

    assert_eq!(
        app.activity(),
        &ActivityState::ProviderStream {
            detail: "assembling example.write arguments (1.5 KiB received)".to_owned(),
        }
    );
    drop(app);
}

#[test]
fn live_provider_tool_call_progress_updates_status() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
    app.absorb_session_live_event(&bcode_session_models::SessionLiveEvent {
        session_id,
        kind: bcode_session_models::SessionLiveEventKind::ProviderStreamProgress {
            turn_id: "turn-1".to_owned(),
            event: bcode_session_models::ProviderStreamEvent::ToolCallProgress {
                tool_call_id: "call-1".to_owned(),
                tool_name: "example.write".to_owned(),
                argument_bytes: 4096,
            },
        },
    });

    assert_eq!(
        app.activity(),
        &ActivityState::ProviderStream {
            detail: "assembling example.write arguments (4.0 KiB received)".to_owned(),
        }
    );
    drop(app);
}

#[test]
fn identical_pending_prompts_reconcile_by_identity() {
    let mut pending = PendingSubmissions::default();
    pending.stage("same".to_owned());
    let first = pending.items()[0].id().clone();
    pending.stage("same".to_owned());
    let second = pending.items()[1].id().clone();
    assert_ne!(first, second);
    pending.remove_id(&second);
    assert_eq!(pending.items().len(), 1);
    assert_eq!(pending.items()[0].id(), &first);
    pending.remove_id(&second);
    assert_eq!(pending.items().len(), 1);
}

#[test]
fn render_includes_status_and_composer() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 10));
    let cursor = {
        let mut frame = Frame::new(&mut buffer);
        render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
        frame.cursor()
    };

    let output = rendered_text(&buffer);

    assert!(buffer.row_symbols(0).unwrap().contains("bcode"));
    assert!(!output.contains("TUI is attached"));
    assert!(output.contains("Message"));
    assert!(cursor.is_some());
}

#[test]
fn composer_expands_and_scrolls_when_input_exceeds_max_rows() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    app.replace_composer_with("0\n1\n2\n3\n4\n5\n6\n7\n8\n9");
    let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 20));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(app);
    let output = rendered_text(&buffer);

    assert!(output.contains("Message"));
    assert!(output.contains('9'));
}

#[test]
fn duration_formatting_uses_readable_units() {
    assert_eq!(format_duration_nanos(12), "12ns");
    assert_eq!(format_duration_nanos(1_500), "1.5µs");
    assert_eq!(format_duration_nanos(1_500_000), "1.5ms");
    assert_eq!(format_millis(30_000), "30.0s");
    assert_eq!(format_millis(30_100), "30.1s");
    assert_eq!(format_millis(120_000), "2m");
    assert_eq!(format_millis(90_000), "1m 30s");
}

#[test]
fn multiline_paste_preserves_line_breaks_in_composer() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);

    app.paste_composer_text("first\nsecond\r\nthird\rfourth");

    assert_eq!(app.composer().text(), "first\nsecond\nthird\nfourth");
    drop(app);
}

#[test]
fn escape_requires_double_tap_to_interrupt_without_exiting_chat() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

    let first = input::handle_key(&mut app, &keymap, key(KeyCode::Escape));
    let second = input::handle_key(&mut app, &keymap, key(KeyCode::Escape));

    assert_eq!(first.request, KeyRequest::None);
    assert!(first.redraw);
    assert_eq!(app.status(), "repeat escape to cancel");
    assert_eq!(second.request, KeyRequest::Interrupt);
    assert!(!app.should_exit());
    drop(app);
}

#[test]
fn multi_tap_key_activation_supports_three_taps() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    let now = Instant::now();
    let binding = BmuxKeyBinding::new(
        key(KeyCode::Escape),
        BmuxAction::AppInterrupt,
        BmuxKeyActivation::MultiTap {
            required_taps: 3,
            window_ms: 1_500,
            prompt: "tap again".to_owned(),
        },
    );

    let first = app.activate_key_binding_for_test(BmuxScope::Chat, &binding, now);
    let second = app.activate_key_binding_for_test(
        BmuxScope::Chat,
        &binding,
        now + Duration::from_millis(100),
    );
    let third = app.activate_key_binding_for_test(
        BmuxScope::Chat,
        &binding,
        now + Duration::from_millis(200),
    );
    drop(app);

    assert_eq!(first, KeyActivationOutcome::Pending);
    assert_eq!(second, KeyActivationOutcome::Pending);
    assert_eq!(
        third,
        KeyActivationOutcome::Activated(BmuxAction::AppInterrupt)
    );
}

#[test]
fn multi_tap_key_activation_resets_after_timeout() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    let now = Instant::now();
    let binding = BmuxKeyBinding::new(
        key(KeyCode::Escape),
        BmuxAction::AppInterrupt,
        BmuxKeyActivation::MultiTap {
            required_taps: 2,
            window_ms: 500,
            prompt: "tap again".to_owned(),
        },
    );

    let first = app.activate_key_binding_for_test(BmuxScope::Chat, &binding, now);
    let expired_second = app.activate_key_binding_for_test(
        BmuxScope::Chat,
        &binding,
        now + Duration::from_millis(600),
    );
    drop(app);

    assert_eq!(first, KeyActivationOutcome::Pending);
    assert_eq!(expired_second, KeyActivationOutcome::Pending);
}

#[test]
fn other_key_resets_pending_multi_tap_activation() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

    let escape = input::handle_key(&mut app, &keymap, key(KeyCode::Escape));
    let tab = input::handle_key(&mut app, &keymap, key(KeyCode::Tab));
    let escape_again = input::handle_key(&mut app, &keymap, key(KeyCode::Escape));
    drop(app);

    assert_eq!(escape.request, KeyRequest::None);
    assert_eq!(tab.request, KeyRequest::CycleAgent);
    assert_eq!(escape_again.request, KeyRequest::None);
}

#[test]
fn immediate_key_activation_runs_without_pending_state() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    let binding = BmuxKeyBinding::new(
        key(KeyCode::Tab),
        BmuxAction::AgentCycle,
        BmuxKeyActivation::Immediate,
    );

    let outcome = app.activate_key_binding_for_test(BmuxScope::Chat, &binding, Instant::now());
    drop(app);

    assert_eq!(
        outcome,
        KeyActivationOutcome::Activated(BmuxAction::AgentCycle)
    );
}

#[test]
fn configured_interrupt_binding_stays_immediate() {
    let mut config = bcode_config::TuiConfig::default();
    config.keybindings.chat = BTreeMap::from([("ctrl+c".to_owned(), "app.interrupt".to_owned())]);
    let keymap = BmuxKeyMap::from_config(&config);
    let mut app = BmuxApp::new_with_history(None, &[], &[]);

    let outcome = input::handle_key(&mut app, &keymap, ctrl_key('c'));
    drop(app);

    assert_eq!(outcome.request, KeyRequest::Interrupt);
}

#[test]
fn configured_ctrl_enter_submits_while_enter_inserts_newline() {
    let mut config = bcode_config::TuiConfig::default();
    config.keybindings.chat = BTreeMap::from([
        (
            "ctrl+enter".to_owned(),
            "tui.input.submitSteering".to_owned(),
        ),
        ("enter".to_owned(), "tui.input.newLine".to_owned()),
    ]);
    let keymap = BmuxKeyMap::from_config(&config);
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    app.replace_composer_with("draft");

    let enter = input::handle_key(&mut app, &keymap, key(KeyCode::Enter));
    assert!(enter.redraw);
    assert!(!matches!(enter.request, KeyRequest::Submit { .. }));
    assert_eq!(app.composer().text(), "draft\n");

    let ctrl_enter = input::handle_key(&mut app, &keymap, ctrl_key_code(KeyCode::Enter));
    drop(app);
    assert_eq!(
        ctrl_enter.request,
        KeyRequest::Submit {
            placement: bcode_ipc::PromptPlacement::Steering,
        }
    );
}

#[test]
fn shared_composer_control_inserts_text_and_rejects_command_modifiers() {
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());
    let mut app = BmuxApp::new_with_history(None, &[], &[]);

    let plain = input::handle_key(&mut app, &keymap, key(KeyCode::Char('c')));
    assert!(plain.redraw);
    assert_eq!(app.composer().text(), "c");

    let shifted = input::handle_key(
        &mut app,
        &keymap,
        KeyStroke::with_modifiers(
            KeyCode::Char('A'),
            Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
        ),
    );
    assert!(shifted.redraw);
    assert_eq!(app.composer().text(), "cA");

    let command = input::handle_key(
        &mut app,
        &keymap,
        KeyStroke::with_modifiers(
            KeyCode::Char('x'),
            Modifiers {
                super_key: true,
                ..Modifiers::NONE
            },
        ),
    );
    assert!(!command.redraw);
    assert_eq!(app.composer().text(), "cA");
    drop(app);
}

#[test]
fn default_tab_requests_agent_cycle_in_chat_input() {
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    app.replace_composer_with("draft");

    assert_eq!(
        keymap.action_for_key(BmuxScope::Chat, key(KeyCode::Tab)),
        Some(BmuxAction::AgentCycle)
    );

    let outcome = input::handle_key(&mut app, &keymap, key(KeyCode::Tab));

    assert!(outcome.redraw);
    assert_eq!(outcome.request, KeyRequest::CycleAgent);
    assert!(!matches!(outcome.request, KeyRequest::Submit { .. }));
    assert_eq!(app.composer().text(), "draft");
    drop(app);
}

#[test]
fn default_shift_tab_requests_thinking_effort_cycle_in_chat_input() {
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    app.replace_composer_with("draft");
    let shift_tab = KeyStroke::with_modifiers(
        KeyCode::Tab,
        Modifiers {
            shift: true,
            ..Modifiers::NONE
        },
    );

    assert_eq!(
        keymap.action_for_key(BmuxScope::Chat, shift_tab),
        Some(BmuxAction::ThinkingEffortCycle)
    );

    let outcome = input::handle_key(&mut app, &keymap, shift_tab);

    assert!(outcome.redraw);
    assert_eq!(outcome.request, KeyRequest::CycleThinkingEffort);
    assert_eq!(app.composer().text(), "draft");
    drop(app);
}

#[test]
fn configured_agent_cycle_binding_can_be_changed() {
    let mut config = bcode_config::TuiConfig::default();
    config.keybindings.chat = BTreeMap::from([("ctrl+a".to_owned(), "tui.agent.cycle".to_owned())]);
    let keymap = BmuxKeyMap::from_config(&config);

    assert_eq!(
        keymap.action_for_key(BmuxScope::Chat, key(KeyCode::Tab)),
        None
    );
    assert_eq!(
        keymap.action_for_key(BmuxScope::Chat, ctrl_key('a')),
        Some(BmuxAction::AgentCycle)
    );
}

#[test]
fn agent_catalog_applies_configured_accent() {
    let agents =
        agent_infos_with_accents(&[("plan", false, Some("#6b7280")), ("build", true, None)]);
    let catalog = super::session_flow::AgentCatalog::from_agents(agents);
    let mut app = BmuxApp::new_with_history(None, &[], &[]);

    catalog.apply_agent_to_app(&mut app, "plan");

    assert_eq!(app.current_agent_id(), "plan");
    assert_eq!(app.current_agent_accent(), Some("#6b7280"));
    drop(app);
}

#[test]
fn runtime_restore_resolves_agent_accent_and_preserves_pending_selection() {
    use bmux_tui::style::Color;

    let catalog = super::session_flow::AgentCatalog::from_agents(agent_infos_with_accents(&[
        ("plan", false, Some("#6b7280")),
        ("build", true, Some("#22d3ee")),
    ]));
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    let mut config = bcode_config::TuiConfig::default();
    config.theme.accent_transition = bcode_config::TuiAccentTransitionMode::Immediate;
    app.apply_tui_config(config);
    catalog.apply_agent_to_app(&mut app, "build");

    for agent_id in ["plan", "build", "plan"] {
        catalog.apply_runtime_selection(
            &mut app,
            bcode_ipc::SessionRuntimeSelection {
                agent_id: Some(agent_id.to_owned()),
                ..Default::default()
            },
        );
        assert_eq!(app.current_agent_id(), agent_id);
        let expected = if agent_id == "plan" {
            Color::Rgb(107, 114, 128)
        } else {
            Color::Rgb(34, 211, 238)
        };
        assert_eq!(app.presented_theme().accent, expected);
    }

    app.set_pending_agent("build", Some("#22d3ee".to_owned()));
    catalog.apply_runtime_selection(
        &mut app,
        bcode_ipc::SessionRuntimeSelection {
            agent_id: Some("plan".to_owned()),
            ..Default::default()
        },
    );
    assert_eq!(app.current_agent_accent(), Some("#6b7280"));
    assert_eq!(app.display_agent_id(), "build");
    assert_eq!(app.display_agent_accent(), Some("#22d3ee"));
    assert_eq!(app.presented_theme().accent, Color::Rgb(34, 211, 238));
    drop(app);
}

#[test]
fn runtime_restore_without_agent_preserves_accent_but_unknown_agent_clears_it() {
    let catalog = super::session_flow::AgentCatalog::default();
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    app.set_current_agent("plan", Some("#6b7280".to_owned()));
    catalog.apply_runtime_selection(&mut app, bcode_ipc::SessionRuntimeSelection::default());
    assert_eq!(app.current_agent_id(), "plan");
    assert_eq!(app.current_agent_accent(), Some("#6b7280"));
    catalog.apply_runtime_selection(
        &mut app,
        bcode_ipc::SessionRuntimeSelection {
            agent_id: Some("unknown".to_owned()),
            ..Default::default()
        },
    );
    assert_eq!(app.current_agent_id(), "unknown");
    assert_eq!(app.current_agent_accent(), None);
    drop(app);
}

#[test]
fn next_agent_preserves_list_order_and_wraps() {
    let agents = agent_infos(&[("plan", false), ("review", false), ("build", true)]);

    assert_eq!(
        super::session_flow::next_agent(&agents, "plan").map(|agent| agent.id.as_str()),
        Some("review")
    );
    assert_eq!(
        super::session_flow::next_agent(&agents, "review").map(|agent| agent.id.as_str()),
        Some("build")
    );
    assert_eq!(
        super::session_flow::next_agent(&agents, "build").map(|agent| agent.id.as_str()),
        Some("plan")
    );
}

#[test]
fn next_agent_uses_default_or_first_when_current_is_unknown() {
    let agents = agent_infos(&[("plan", false), ("build", true)]);
    let no_default = agent_infos(&[("plan", false), ("build", false)]);

    assert_eq!(
        super::session_flow::next_agent(&agents, "custom").map(|agent| agent.id.as_str()),
        Some("build")
    );
    assert_eq!(
        super::session_flow::next_agent(&no_default, "custom").map(|agent| agent.id.as_str()),
        Some("plan")
    );
    assert!(super::session_flow::next_agent(&[], "custom").is_none());
}

#[test]
fn default_ctrl_v_maps_to_clipboard_image_paste() {
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

    assert_eq!(
        keymap.action_for_key(BmuxScope::Chat, ctrl_key('v')),
        Some(BmuxAction::ClipboardPasteImage)
    );
}

#[test]
fn configured_clipboard_image_paste_binding_can_be_changed() {
    let mut config = bcode_config::TuiConfig::default();
    config.keybindings.chat =
        BTreeMap::from([("alt+v".to_owned(), "app.clipboard.pasteImage".to_owned())]);
    let keymap = BmuxKeyMap::from_config(&config);

    assert_eq!(keymap.action_for_key(BmuxScope::Chat, ctrl_key('v')), None);
    assert_eq!(
        keymap.action_for_key(BmuxScope::Chat, alt_char('v')),
        Some(BmuxAction::ClipboardPasteImage)
    );
}

#[test]
fn configured_bindings_can_keep_multiple_keys_for_same_action() {
    let mut config = bcode_config::TuiConfig::default();
    config.keybindings.chat = BTreeMap::from([
        ("enter".to_owned(), "tui.input.newLine".to_owned()),
        ("shift+enter".to_owned(), "tui.input.newLine".to_owned()),
        (
            "ctrl+enter".to_owned(),
            "tui.input.submitSteering".to_owned(),
        ),
    ]);
    let keymap = BmuxKeyMap::from_config(&config);

    assert_eq!(
        keymap.action_for_key(BmuxScope::Chat, key(KeyCode::Enter)),
        Some(BmuxAction::InputNewLine)
    );
    assert_eq!(
        keymap.action_for_key(BmuxScope::Chat, shift_key(KeyCode::Enter)),
        Some(BmuxAction::InputNewLine)
    );
    assert_eq!(
        keymap.action_for_key(BmuxScope::Chat, ctrl_key_code(KeyCode::Enter)),
        Some(BmuxAction::InputSubmitSteering)
    );
}

#[test]
fn ctrl_d_clears_input_before_exit() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    app.replace_composer_with("draft");
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

    let first = input::handle_key(&mut app, &keymap, ctrl_key('d'));
    let second = input::handle_key(&mut app, &keymap, ctrl_key('d'));

    assert!(first.redraw);
    assert!(app.composer().is_empty());
    assert!(second.redraw);
    assert!(app.should_exit());
    drop(app);
}

#[test]
fn shift_arrows_extend_composer_selection() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    app.replace_composer_with("hello");
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

    let outcome = input::handle_key(&mut app, &keymap, shift_key(KeyCode::Left));

    assert!(outcome.redraw);
    assert_eq!(app.composer().selected_text(), Some("o".to_owned()));
    drop(app);
}

#[test]
fn composer_mouse_drag_extends_selection() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    app.set_composer_content_area(Rect::new(2, 8, 20, 1));
    app.replace_composer_with("hello world");

    let down = app.handle_composer_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 8));
    let drag = app.handle_composer_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 7, 8));
    let up = app.handle_composer_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 7, 8));

    assert!(matches!(
        down,
        bmux_tui_components::text_input::TextInputOutcome::Redraw
    ));
    assert!(matches!(
        drag,
        bmux_tui_components::text_input::TextInputOutcome::Redraw
    ));
    assert!(matches!(
        up,
        bmux_tui_components::text_input::TextInputOutcome::Redraw
    ));
    assert_eq!(app.composer().selected_text(), Some("hello".to_owned()));
    drop(app);
}

#[test]
fn composer_drag_beyond_visible_edge_scrolls_selection() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    app.replace_composer_with("0\n1\n2\n3\n4");
    app.composer_mut().move_cursor(TextMotion::Start);
    app.set_composer_content_area(Rect::new(2, 8, 20, 2));

    let _ = app.handle_composer_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 8));
    let outcome = app.handle_composer_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 2, 10));

    assert!(matches!(
        outcome,
        bmux_tui_components::text_input::TextInputOutcome::Redraw
    ));
    assert_eq!(app.composer_scroll_offset_for_render(), 1);
    assert_eq!(app.composer().selected_text(), Some("0\n1\n".to_owned()));
    drop(app);
}

#[test]
fn composer_double_click_selects_word_and_triple_click_selects_all() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    app.set_composer_content_area(Rect::new(2, 8, 20, 1));
    app.replace_composer_with("hello world");

    let _ = app.handle_composer_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 8));
    let _ = app.handle_composer_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 8));
    assert_eq!(app.composer().selected_text(), Some("hello".to_owned()));

    let _ = app.handle_composer_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 8));
    assert_eq!(
        app.composer().selected_text(),
        Some("hello world".to_owned())
    );
    drop(app);
}

#[test]
fn input_history_updates_status_and_restores_draft() {
    let history = [
        SessionInputHistoryEntry {
            timestamp_ms: 1,
            sequence: 1,
            text: "first prompt".to_owned(),
        },
        SessionInputHistoryEntry {
            timestamp_ms: 1,
            sequence: 2,
            text: "second prompt".to_owned(),
        },
    ];
    let mut app = BmuxApp::new_with_history(None, &[], &history);
    app.replace_composer_with("draft prompt");

    assert!(app.previous_input_history());
    assert_eq!(app.composer().text(), "second prompt");
    assert_eq!(app.status(), "input history 2/2");

    assert!(app.previous_input_history());
    assert_eq!(app.composer().text(), "first prompt");
    assert_eq!(app.status(), "input history 1/2");

    assert!(app.next_input_history());
    assert_eq!(app.composer().text(), "second prompt");
    assert_eq!(app.status(), "input history 2/2");

    assert!(app.next_input_history());
    assert_eq!(app.composer().text(), "draft prompt");
    assert_eq!(app.status(), "draft restored");
    drop(app);
}

#[test]
fn input_history_empty_and_not_browsing_update_status() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);

    assert!(app.previous_input_history());
    assert_eq!(app.status(), "no input history in this session");

    assert!(app.next_input_history());
    assert_eq!(app.status(), "not browsing input history");
    drop(app);
}

#[test]
fn composer_edit_after_history_resets_navigation() {
    let history = [SessionInputHistoryEntry {
        timestamp_ms: 1,
        sequence: 1,
        text: "history prompt".to_owned(),
    }];
    let mut app = BmuxApp::new_with_history(None, &[], &history);
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

    assert!(app.previous_input_history());
    let outcome = input::handle_key(&mut app, &keymap, key(KeyCode::Char('!')));
    assert!(outcome.redraw);
    assert!(app.next_input_history());

    assert_eq!(app.status(), "not browsing input history");
    drop(app);
}

#[test]
fn input_history_moves_within_multiline_entry_before_cycling() {
    let history = [
        SessionInputHistoryEntry {
            timestamp_ms: 1,
            sequence: 1,
            text: "older prompt\nolder second line".to_owned(),
        },
        SessionInputHistoryEntry {
            timestamp_ms: 1,
            sequence: 2,
            text: "newest prompt\nnewest second line".to_owned(),
        },
    ];
    let mut app = BmuxApp::new_with_history(None, &[], &history);
    app.set_composer_content_area(Rect::new(0, 0, 40, 3));
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Up)).redraw);
    assert_eq!(app.composer().text(), "newest prompt\nnewest second line");
    assert_eq!(app.status(), "input history 2/2");

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Up)).redraw);
    assert_eq!(app.composer().text(), "newest prompt\nnewest second line");
    assert_eq!(app.status(), "input history 2/2");

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Up)).redraw);
    assert_eq!(app.composer().text(), "older prompt\nolder second line");
    assert_eq!(app.status(), "input history 1/2");

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Up)).redraw);
    assert_eq!(app.composer().text(), "older prompt\nolder second line");
    assert_eq!(app.status(), "input history 1/2");

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Down)).redraw);
    assert_eq!(app.composer().text(), "older prompt\nolder second line");
    assert_eq!(app.status(), "input history 1/2");

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Down)).redraw);
    assert_eq!(app.composer().text(), "newest prompt\nnewest second line");
    assert_eq!(app.status(), "input history 2/2");

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Down)).redraw);
    assert!(app.composer().is_empty());
    assert_eq!(app.status(), "draft restored");
    drop(app);
}

#[test]
fn input_history_restores_empty_draft_from_newest_entry_bottom() {
    let history = [SessionInputHistoryEntry {
        timestamp_ms: 1,
        sequence: 1,
        text: "history prompt".to_owned(),
    }];
    let mut app = BmuxApp::new_with_history(None, &[], &history);
    app.set_composer_content_area(Rect::new(0, 0, 40, 3));
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Up)).redraw);
    assert_eq!(app.composer().text(), "history prompt");
    assert_eq!(app.status(), "input history 1/1");

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Down)).redraw);
    assert!(app.composer().is_empty());
    assert_eq!(app.status(), "draft restored");
    drop(app);
}

#[test]
fn live_user_message_does_not_overwrite_saved_history_draft() {
    let session_id = SessionId::new();
    let history = [SessionInputHistoryEntry {
        timestamp_ms: 1,
        sequence: 1,
        text: "older prompt".to_owned(),
    }];
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &history);
    let keymap = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());

    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Up)).redraw);
    assert_eq!(app.composer().text(), "older prompt");
    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::UserMessage {
            text: "newly committed prompt".to_owned(),
            client_id: ClientId::new(),
            admission: bcode_session_models::TurnAdmissionMetadata::default(),
        },
    ));
    assert!(input::handle_key(&mut app, &keymap, key(KeyCode::Down)).redraw);

    assert!(app.composer().is_empty());
    assert_eq!(app.status(), "draft restored");
    drop(app);
}

#[test]
fn empty_and_slash_submissions_do_not_enter_input_history() {
    let history = [SessionInputHistoryEntry {
        timestamp_ms: 1,
        sequence: 1,
        text: "real prompt".to_owned(),
    }];
    let mut app = BmuxApp::new_with_history(None, &[], &history);

    app.stage_submission();
    let empty = app.take_pending_submission();
    app.clear_pending_submission(&empty);
    app.replace_composer_with("/help");
    app.stage_submission();
    let slash = app.take_pending_submission();
    app.clear_pending_submission(&slash);

    assert!(app.previous_input_history());
    assert_eq!(app.composer().text(), "real prompt");
    assert_eq!(app.status(), "input history 1/1");
    drop(app);
}

#[test]
fn header_uses_attach_summary_title_when_recent_history_lacks_title_events() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        42,
        SessionEventKind::AssistantMessage {
            text: "recent response".to_owned(),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[]);
    app.apply_session_summary(&SessionSummary {
        origin: None,
        id: session_id,
        parent_id: None,
        name: Some("Canonical title".to_owned()),
        explicit_name: Some("Canonical title".to_owned()),
        derived_title: None,
        automatic_title: None,
        automatic_title_source_sequence: None,
        title_source: SessionTitleSource::Explicit,
        client_count: 1,
        created_at_ms: 1,
        updated_at_ms: 2,
        working_directory: "/tmp/bcode-tui-test".into(),
        import: None,
        execution: None,
        location: None,
    });
    let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 10));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(app);

    assert!(buffer.row_symbols(0).unwrap().contains("Canonical title"));
    assert!(!buffer.row_symbols(0).unwrap().contains("Untitled session"));
}

#[test]
fn header_drops_low_priority_segments_in_narrow_panes() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
    app.apply_session_summary(&SessionSummary {
        origin: None,
        id: session_id,
        parent_id: None,
        name: Some("A readable session title".to_owned()),
        explicit_name: Some("A readable session title".to_owned()),
        derived_title: None,
        automatic_title: None,
        automatic_title_source_sequence: None,
        title_source: SessionTitleSource::Explicit,
        client_count: 1,
        created_at_ms: 1,
        updated_at_ms: 2,
        working_directory: "/tmp/bcode-tui-test".into(),
        import: None,
        execution: None,
        location: None,
    });
    app.apply_model_status(bcode_model::SessionModelStatus {
        provider_plugin_id: Some("very-long-provider-plugin-id".to_owned()),
        requested_model_id: None,
        effective_model_id: None,
        model_id: Some("very-long-model-id".to_owned()),
        display_name: None,
        context_window: None,
        context_occupancy: None,
        request_context_error: None,
        auth_profile: None,
        context_format_version: None,
        compatibility_key: None,
        max_output_tokens: None,
        reasoning: None,
        reasoning_effort: None,
        reasoning_summary: None,
        prompt_cache_mode: None,
        conversation_reuse_mode: None,
        compaction_mode: None,
        compaction_backend: None,
        proactive_compaction_threshold_percent: None,
        proactive_compaction_threshold_tokens: None,
        proactive_compaction_effective_threshold_tokens: None,
        proactive_compaction_threshold_source: None,
        cache: None,
        metadata_source: None,
        pricing: None,
    });
    let mut buffer = Buffer::empty(Rect::new(0, 0, 36, 8));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(app);
    let header = buffer.row_symbols(0).unwrap();

    assert!(header.contains("bcode"));
    assert!(header.contains("build"));
    assert!(!header.contains("provider"));
    assert!(!header.contains("thinking"));
}

#[test]
fn header_shortens_session_id_on_wide_panes() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 240, 8));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    let header = buffer.row_symbols(0).unwrap();

    assert!(header.contains(&format!("#{}", &session_id.to_string()[..8])));
    assert!(!header.contains(&session_id.to_string()[9..]));
}

#[test]
fn adaptive_bcode_agent_accent_policy_is_intentional_across_variants() {
    for variant in [TuiThemeVariant::Dark, TuiThemeVariant::Light] {
        let configured_theme = crate::theme::definition::ThemeCatalog::bundled()
            .expect("bundled themes parse")
            .resolve(
                &crate::theme::definition::ThemeSelection::new("bcode").variant(match variant {
                    TuiThemeVariant::Dark => crate::theme::definition::ResolvedThemeVariant::Dark,
                    TuiThemeVariant::Light => crate::theme::definition::ResolvedThemeVariant::Light,
                    TuiThemeVariant::Auto => unreachable!("variants are explicit"),
                }),
            )
            .expect("adaptive bcode resolves");
        let configured_accent = crate::theme::resolved_accent(Some(&configured_theme));

        let mut theme_only = BmuxApp::new_with_history(None, &[], &[]);
        theme_only.apply_tui_config(TuiConfig {
            theme: TuiThemeConfig {
                name: "bcode".to_owned(),
                variant,
                agent_accent: TuiAgentAccentPolicy::ThemeOnly,
                accent_transition: TuiAccentTransitionMode::Immediate,
                ..TuiThemeConfig::default()
            },
            ..TuiConfig::default()
        });
        theme_only.set_agent_metadata_hydrated(true);
        theme_only.set_current_agent("build", Some("#22d3ee".to_owned()));
        assert_eq!(theme_only.presented_theme().accent, configured_accent);
        drop(theme_only);

        let mut agent_first = BmuxApp::new_with_history(None, &[], &[]);
        agent_first.apply_tui_config(TuiConfig {
            theme: TuiThemeConfig {
                name: "bcode".to_owned(),
                variant,
                agent_accent: TuiAgentAccentPolicy::AgentWithThemeFallback,
                accent_transition: TuiAccentTransitionMode::Immediate,
                ..TuiThemeConfig::default()
            },
            ..TuiConfig::default()
        });
        agent_first.set_agent_metadata_hydrated(true);
        agent_first.set_current_agent("build", Some("#22d3ee".to_owned()));
        assert_eq!(
            agent_first.presented_theme().accent,
            bmux_tui::style::Color::Rgb(34, 211, 238)
        );
        drop(agent_first);
    }
}

#[test]
fn header_accent_color_tracks_arbitrary_selected_agent() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    disable_theme_transition(&mut app);
    app.set_agent_metadata_hydrated(true);
    app.set_current_agent_id("one");
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 8));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(app);

    assert_eq!(
        buffer.get(Point::new(0, 0)).and_then(|cell| cell.style.fg),
        Some(bmux_tui::style::Color::Rgb(52, 211, 153))
    );
}

#[test]
fn composer_border_accent_color_tracks_arbitrary_selected_agent() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    disable_theme_transition(&mut app);
    app.set_agent_metadata_hydrated(true);
    app.set_current_agent_id("two");
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 8));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    let border_y = app.composer_content_area().y.saturating_sub(1);
    drop(app);

    assert_eq!(
        buffer
            .get(Point::new(0, border_y))
            .and_then(|cell| cell.style.fg),
        Some(bmux_tui::style::Color::Cyan)
    );
}

#[test]
fn same_agent_gets_same_accent_across_chrome() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    app.set_agent_metadata_hydrated(true);
    app.set_current_agent_id("custom-agent");
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 8));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    let header_accent = buffer.get(Point::new(0, 0)).and_then(|cell| cell.style.fg);
    let border_y = app.composer_content_area().y.saturating_sub(1);
    drop(app);
    let composer_accent = buffer
        .get(Point::new(0, border_y))
        .and_then(|cell| cell.style.fg);

    assert_eq!(header_accent, composer_accent);
}

#[test]
fn configured_agent_accent_overrides_fallback_color() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    disable_theme_transition(&mut app);
    app.set_current_agent("quiet-plan", Some("#6b7280".to_owned()));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 8));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(app);

    assert_eq!(
        buffer.get(Point::new(0, 0)).and_then(|cell| cell.style.fg),
        Some(bmux_tui::style::Color::Rgb(107, 114, 128))
    );
}

#[test]
fn invalid_configured_agent_accent_falls_back_to_agent_color() {
    let mut fallback_app = BmuxApp::new_with_history(None, &[], &[]);
    disable_theme_transition(&mut fallback_app);
    fallback_app.set_agent_metadata_hydrated(true);
    fallback_app.set_current_agent_id("quiet-plan");
    let mut fallback_buffer = Buffer::empty(Rect::new(0, 0, 100, 8));
    let mut fallback_frame = Frame::new(&mut fallback_buffer);
    render::render(
        &mut fallback_app,
        &mut bmux_tui::paint::PaintCx::new(&mut fallback_frame),
    );
    drop(fallback_app);
    let fallback_accent = fallback_buffer
        .get(Point::new(0, 0))
        .and_then(|cell| cell.style.fg);

    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    disable_theme_transition(&mut app);
    app.set_agent_metadata_hydrated(true);
    app.set_current_agent("quiet-plan", Some("not-a-color".to_owned()));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 8));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(app);

    assert_eq!(
        buffer.get(Point::new(0, 0)).and_then(|cell| cell.style.fg),
        fallback_accent
    );
}

#[test]
fn live_session_rename_overrides_attach_summary_title() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
    app.apply_session_summary(&SessionSummary {
        origin: None,
        id: session_id,
        parent_id: None,
        name: Some("Old title".to_owned()),
        explicit_name: Some("Old title".to_owned()),
        derived_title: None,
        automatic_title: None,
        automatic_title_source_sequence: None,
        title_source: SessionTitleSource::Explicit,
        client_count: 1,
        created_at_ms: 1,
        updated_at_ms: 2,
        working_directory: "/tmp/bcode-tui-test".into(),
        import: None,
        execution: None,
        location: None,
    });

    app.absorb_session_event(&event(
        session_id,
        7,
        SessionEventKind::SessionRenamed {
            name: Some("New title".to_owned()),
        },
    ));

    assert_eq!(app.session_title(), Some("New title"));
    drop(app);
}

#[test]
fn header_and_footer_include_model_agent_and_token_context() {
    let session_id = SessionId::new();
    let history = [
        event(
            session_id,
            1,
            SessionEventKind::SessionCreated {
                origin: None,
                name: Some("Visual parity work".to_owned()),
                working_directory: "/tmp/bcode-tui-test".into(),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::AgentChanged {
                agent_id: "plan".to_owned(),
            },
        ),
        event(
            session_id,
            3,
            SessionEventKind::ModelUsage {
                turn_id: "turn-1".to_owned(),
                usage: SessionTokenUsage {
                    input_tokens: Some(512),
                    output_tokens: Some(128),
                    total_tokens: Some(640),
                    cached_input_tokens: Some(256),
                    cache_write_input_tokens: Some(128),
                    reasoning_tokens: Some(64),
                    ..SessionTokenUsage::default()
                },
            },
        ),
    ];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[]);
    app.apply_model_status(bcode_model::SessionModelStatus {
        provider_plugin_id: Some("provider.example".to_owned()),
        requested_model_id: None,
        effective_model_id: None,
        model_id: Some("model-example".to_owned()),
        display_name: None,
        context_window: Some(1024),
        context_occupancy: Some(Box::new(context_occupancy(512))),
        request_context_error: None,
        auth_profile: None,
        context_format_version: None,
        compatibility_key: None,
        max_output_tokens: None,
        reasoning: None,
        reasoning_effort: None,
        reasoning_summary: None,
        prompt_cache_mode: None,
        conversation_reuse_mode: None,
        compaction_mode: None,
        compaction_backend: None,
        proactive_compaction_threshold_percent: None,
        proactive_compaction_threshold_tokens: None,
        proactive_compaction_effective_threshold_tokens: None,
        proactive_compaction_threshold_source: None,
        cache: None,
        metadata_source: None,
        pricing: None,
    });
    let mut buffer = Buffer::empty(Rect::new(0, 0, 180, 12));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(app);
    let output = rendered_text(&buffer);

    assert!(
        buffer
            .row_symbols(0)
            .unwrap()
            .contains("Visual parity work")
    );
    assert!(buffer.row_symbols(0).unwrap().contains("provider.example"));
    assert!(output.contains("model-example"));
    assert!(output.contains("plan"));
    assert!(!output.contains("agent plan"));
    assert!(output.contains("512/1k 50%"), "{output}");
    assert!(output.contains("read 256"));
    assert!(output.contains("write 128"));
    assert!(output.contains("spent 640"));
}

#[test]
fn plugin_status_is_rendered_and_atomic_replacement_removes_stale_text() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    app.set_plugin_status(vec![bcode_session_view_models::PluginStatusView {
        plugin_id: "test-plugin".to_owned(),
        note_id: "test".to_owned(),
        text: "Plugin active · normal messages remain usable".to_owned(),
        priority: 20,
        metadata: std::collections::BTreeMap::new(),
    }]);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 160, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    assert!(rendered_text(&buffer).contains("Plugin active"));

    app.set_plugin_status(Vec::new());
    let mut buffer = Buffer::empty(Rect::new(0, 0, 160, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(app);
    assert!(!rendered_text(&buffer).contains("Plugin active"));
}

#[test]
fn status_line_prioritizes_context_over_spent_tokens() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        1,
        SessionEventKind::ModelUsage {
            turn_id: "turn-1".to_owned(),
            usage: SessionTokenUsage {
                input_tokens: Some(512),
                output_tokens: Some(128),
                total_tokens: Some(640),
                cached_input_tokens: None,
                cache_write_input_tokens: None,
                reasoning_tokens: None,
                ..SessionTokenUsage::default()
            },
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[]);
    app.apply_model_status(bcode_model::SessionModelStatus {
        provider_plugin_id: Some("provider.example".to_owned()),
        requested_model_id: None,
        effective_model_id: None,
        model_id: Some("model-example".to_owned()),
        display_name: None,
        context_window: Some(128_000),
        context_occupancy: Some(Box::new(context_occupancy(512))),
        request_context_error: None,
        auth_profile: None,
        context_format_version: None,
        compatibility_key: None,
        max_output_tokens: None,
        reasoning: None,
        reasoning_effort: None,
        reasoning_summary: None,
        prompt_cache_mode: None,
        conversation_reuse_mode: None,
        compaction_mode: None,
        compaction_backend: None,
        proactive_compaction_threshold_percent: None,
        proactive_compaction_threshold_tokens: None,
        proactive_compaction_effective_threshold_tokens: None,
        proactive_compaction_threshold_source: None,
        cache: None,
        metadata_source: None,
        pricing: None,
    });
    let mut buffer = Buffer::empty(Rect::new(0, 0, 68, 8));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(app);
    let output = rendered_text(&buffer);

    assert!(output.contains("512/128k 0%"), "{output}");
    assert!(!output.contains("spent 640"), "{output}");
}

#[test]
fn status_line_includes_unknown_context_before_spent_tokens() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        1,
        SessionEventKind::ModelUsage {
            turn_id: "turn-1".to_owned(),
            usage: SessionTokenUsage {
                input_tokens: Some(75_700),
                output_tokens: Some(1_524_300),
                total_tokens: Some(1_600_000),
                cached_input_tokens: Some(75_700),
                cache_write_input_tokens: None,
                reasoning_tokens: None,
                ..SessionTokenUsage::default()
            },
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[]);
    app.set_status("model: bcode.openai-compatible/gpt-5.5; active skills: 0".to_owned());
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 8));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(app);
    let output = rendered_text(&buffer);

    assert!(output.contains("—/— —%"), "{output}");
    assert!(!output.contains("spent 1.6m"), "{output}");
}

#[test]
fn status_line_drops_low_priority_segments_in_narrow_panes() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
    app.set_daemon_connection(super::app::DaemonConnectionState::Connected);
    app.set_status("important status message".to_owned());
    let mut buffer = Buffer::empty(Rect::new(0, 0, 36, 8));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(app);
    let output = rendered_text(&buffer);

    assert!(output.contains("ready"));
    assert!(output.contains("—/— —%"));
    assert!(!output.contains("enter send"));
    assert!(!output.contains("spent"));
}

#[test]
fn draft_agent_selection_updates_header() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);

    app.set_current_agent_id("plan");

    let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 10));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(app);

    assert!(buffer.row_symbols(0).unwrap().contains("plan"));
    assert!(!buffer.row_symbols(0).unwrap().contains("agent plan"));
}

#[test]
fn new_draft_preserves_selected_agent() {
    let (sender, receiver) = crate::history_flow::session_stream_channel();
    let mut chat = super::session_flow::ActiveChat {
        app: BmuxApp::new_with_history(None, &[], &[]),
        agents: super::session_flow::AgentCatalog::default(),
        attachment: super::session_flow::ChatSessionAttachment::Draft,
        event_sender: sender,
        event_receiver: receiver,
        event_task: None,
        opening_session_progress: None,
        pending_effects: super::effects::TuiEffectQueue::default(),
    };
    chat.app.set_current_agent_id("plan");

    super::session_flow::switch_to_draft_session(&mut chat);

    assert_eq!(chat.app.current_agent_id(), "plan");
}

#[test]
fn migration_stage_families_and_terminal_failure_render_through_status_chrome() {
    use bcode_session_models::{
        SessionMigrationProgress, SessionMigrationProgressUnit, SessionMigrationStage,
        SessionOpenOperationId, SessionOpenOperationSnapshot,
    };

    let session_id = SessionId::new();
    let render_status = |snapshot: SessionOpenOperationSnapshot| {
        let status = super::chat_loop::test_session_open_progress_status(&snapshot);
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
        app.set_status(status.clone());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 240, 12));
        let mut frame = Frame::new(&mut buffer);
        render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
        drop(app);
        let output = rendered_text(&buffer);
        assert!(
            output.contains(&status),
            "missing rendered status {status:?}: {output}"
        );
        status
    };
    let snapshot =
        |stage, completed_units, total_units, unit, message: &str| SessionOpenOperationSnapshot {
            operation_id: SessionOpenOperationId::new(),
            revision: 1,
            session_id,
            source_writer_epoch: Some(3),
            target_writer_epoch: 4,
            progress: SessionMigrationProgress {
                stage,
                completed_units,
                total_units,
                unit,
                message: message.to_owned(),
            },
            outcome: None,
            backup_path: None,
        };

    for (unit, completed, total, expected) in [
        (SessionMigrationProgressUnit::Files, 2, 4, "2 / 4 files"),
        (
            SessionMigrationProgressUnit::Bytes,
            1024,
            2048,
            "1.0 KiB / 2.0 KiB",
        ),
        (
            SessionMigrationProgressUnit::Events,
            25,
            100,
            "25 / 100 events",
        ),
    ] {
        let status = render_status(snapshot(
            SessionMigrationStage::RebuildingProjections,
            Some(completed),
            Some(total),
            Some(unit),
            "Rebuilding projections",
        ));
        assert!(status.contains(expected));
        assert!(status.contains("epoch 3 → 4"));
    }

    for stage in [
        SessionMigrationStage::WaitingForOwnership,
        SessionMigrationStage::InspectingStorage,
        SessionMigrationStage::PlanningBackup,
        SessionMigrationStage::CopyingBackup,
        SessionMigrationStage::VerifyingBackup,
        SessionMigrationStage::PreparingSchema,
        SessionMigrationStage::ReadingCanonicalHistory,
        SessionMigrationStage::RebuildingProjections,
        SessionMigrationStage::ValidatingProjections,
        SessionMigrationStage::Committing,
        SessionMigrationStage::ValidatingWriteReadiness,
        SessionMigrationStage::OpeningSession,
        SessionMigrationStage::Complete,
        SessionMigrationStage::Failed,
    ] {
        let message = format!("Stage {stage:?}");
        let status = render_status(snapshot(stage, None, None, None, &message));
        assert!(status.contains(&message));
    }

    let mut failed = BmuxApp::new_with_history(Some(session_id), &[], &[]);
    failed.set_status(
        "session open failed during Verifying retained backup: hash mismatch; retained backup /tmp/backup; bcode session diagnose"
            .to_owned(),
    );
    let mut buffer = Buffer::empty(Rect::new(0, 0, 240, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut failed, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(failed);
    let output = rendered_text(&buffer);
    assert!(output.contains("Verifying retained backup"));
    assert!(output.contains("hash mismatch"));
    assert!(output.contains("/tmp/backup"));
    assert!(output.contains("bcode session diagnose"));
}

#[test]
fn leaving_opening_session_keeps_detached_observer_stale_and_allows_reselection() {
    let (sender, receiver) = crate::history_flow::session_stream_channel();
    let session_id = SessionId::new();
    let mut chat = super::session_flow::ActiveChat {
        app: BmuxApp::new_with_history(Some(session_id), &[], &[]),
        agents: super::session_flow::AgentCatalog::default(),
        attachment: super::session_flow::ChatSessionAttachment::Opening {
            session_id,
            anchor_sequence: None,
        },
        event_sender: sender,
        event_receiver: receiver,
        event_task: None,
        opening_session_progress: Some(bcode_session_models::SessionOpenOperationSnapshot {
            operation_id: bcode_session_models::SessionOpenOperationId::new(),
            revision: 2,
            session_id,
            source_writer_epoch: Some(3),
            target_writer_epoch: 4,
            progress: bcode_session_models::SessionMigrationProgress {
                stage: bcode_session_models::SessionMigrationStage::CopyingBackup,
                completed_units: Some(1),
                total_units: Some(2),
                unit: Some(bcode_session_models::SessionMigrationProgressUnit::Files),
                message: "Copying backup".to_owned(),
            },
            outcome: None,
            backup_path: None,
        }),
        pending_effects: super::effects::TuiEffectQueue::default(),
    };
    let request = super::session_flow::initial_transcript_window_request(Rect::new(0, 0, 80, 24));
    chat.replace_effect(super::effects::TuiEffect::OpenSession {
        session_id,
        initial_window_request: request.clone(),
        event_sender: chat.event_sender.clone(),
        allow_daemon_start: true,
    });

    super::session_flow::switch_to_draft_session(&mut chat);

    assert_eq!(chat.opening_session_id(), None);
    assert_eq!(chat.opening_session_progress, None);
    assert!(chat.pending_effects.has_open_session(session_id));
    super::session_flow::complete_switch_session(
        &mut chat,
        session_id,
        Err(super::TuiError::SessionUnavailable {
            session_id,
            reason: "stale completion".to_owned(),
        }),
    );
    assert_eq!(chat.attached_session_id(), None);
    assert_eq!(chat.app.status(), "New draft");

    super::session_flow::start_switch_session(&mut chat, session_id, request);
    assert_eq!(chat.opening_session_id(), Some(session_id));
    assert!(chat.pending_effects.has_open_session(session_id));
    drop(chat);
}

#[tokio::test]
async fn hydrated_search_hit_opens_canonical_around_sequence_window_and_anchors_result() {
    let (sender, receiver) = crate::history_flow::session_stream_channel();
    let session_id = SessionId::new();
    let mut chat = super::session_flow::ActiveChat {
        app: BmuxApp::new_with_history(None, &[], &[]),
        agents: super::session_flow::AgentCatalog::default(),
        attachment: super::session_flow::ChatSessionAttachment::Draft,
        event_sender: sender,
        event_receiver: receiver,
        event_task: None,
        opening_session_progress: None,
        pending_effects: super::effects::TuiEffectQueue::default(),
    };
    let canonical_event = event(
        session_id,
        7,
        SessionEventKind::AssistantMessage {
            text: "canonical search result".to_owned(),
        },
    );
    let hydrated = bcode_session_search::HydratedSessionSearchHit {
        hit: bcode_session_search::SessionSearchHit {
            locator: bcode_session_search::SessionSearchLocator {
                session_id,
                sequence: 7,
                record_id: Some("search-result".to_owned()),
            },
            content_kind: bcode_session_search::SearchContentKind::AssistantMessage,
            matched_field: bcode_session_search::SearchField::Text,
            provider_id: "provider".to_owned(),
            provider_rank: 1,
            provider_score: None,
            preview: Some("provider preview".to_owned()),
            preview_truncated: false,
        },
        outcome: bcode_session_search::SearchHitHydrationOutcome::Hydrated,
        event: Some(Box::new(canonical_event.clone())),
        message: None,
    };
    let request = super::session_flow::initial_transcript_window_request(Rect::new(0, 0, 80, 24));

    super::session_flow::start_switch_session_from_search_hit(&mut chat, &hydrated, request)
        .expect("hydrated search hit navigates");
    assert_eq!(chat.opening_session_id(), Some(session_id));
    assert_eq!(chat.attachment.opening_anchor_sequence(), Some(7));
    let initial_window_request = chat
        .pending_effects
        .open_session_request(session_id)
        .expect("canonical open effect");
    assert_eq!(
        initial_window_request.anchor,
        bcode_session_models::ProjectionWindowAnchor::AroundSequence(7)
    );

    let (_event_sender, event_receiver) = tokio::sync::broadcast::channel::<SessionEvent>(1);
    super::session_flow::complete_switch_session(
        &mut chat,
        session_id,
        Ok((
            AttachedSessionHistory {
                submissions: Vec::new(),
                session: session_summary(session_id),
                history: vec![canonical_event],
                input_history: Vec::new(),
                usage_summary: bcode_session_models::SessionUsageSummary {
                    cumulative_metered_tokens: 1_234,
                    totals_micros: std::collections::BTreeMap::from([(
                        "USD".to_owned(),
                        62_874_262,
                    )]),
                    estimated_usage_count: 524,
                    observed_usage_count: 524,
                    ..bcode_session_models::SessionUsageSummary::default()
                },
                import_warnings: Vec::new(),
                draft: None,
                runtime_selection: bcode_ipc::SessionRuntimeSelection::default(),
                projection_window: None,
            },
            tokio::spawn(async move {
                drop(event_receiver);
            }),
        )),
    );

    assert_eq!(chat.attached_session_id(), Some(session_id));
    assert_eq!(
        chat.app.session_view_snapshot().runtime.cost.totals_micros["USD"],
        62_874_262
    );
    assert_eq!(
        chat.app
            .session_view_snapshot()
            .runtime
            .cumulative_metered_tokens,
        1_234
    );
    assert_eq!(chat.attachment.opening_anchor_sequence(), None);
    assert_eq!(chat.app.status(), "jumped to search result");
    assert!(chat.app.has_transcript_sequence(7));
    drop(chat);
}

#[tokio::test]
async fn async_session_open_preserves_typed_draft() {
    let (sender, receiver) = crate::history_flow::session_stream_channel();
    let session_id = SessionId::new();
    let mut chat = super::session_flow::ActiveChat {
        app: BmuxApp::new_with_history(Some(session_id), &[], &[]),
        agents: super::session_flow::AgentCatalog::default(),
        attachment: super::session_flow::ChatSessionAttachment::Opening {
            session_id,
            anchor_sequence: None,
        },
        event_sender: sender,
        event_receiver: receiver,
        event_task: None,
        opening_session_progress: None,
        pending_effects: super::effects::TuiEffectQueue::default(),
    };
    chat.app.replace_composer_with("draft while opening");
    chat.agents = super::session_flow::AgentCatalog::from_agents(agent_infos_with_accents(&[(
        "plan",
        false,
        Some("#6b7280"),
    )]));
    let (_event_sender, event_receiver) = tokio::sync::broadcast::channel::<SessionEvent>(1);
    let mut attached = AttachedSessionHistory {
        submissions: Vec::new(),
        session: session_summary(session_id),
        history: vec![event(
            session_id,
            1,
            SessionEventKind::AssistantMessage {
                text: "previous answer".to_owned(),
            },
        )],
        input_history: vec![SessionInputHistoryEntry {
            timestamp_ms: 1,
            sequence: 1,
            text: "previous prompt".to_owned(),
        }],
        usage_summary: bcode_session_models::SessionUsageSummary::default(),
        import_warnings: Vec::new(),
        draft: None,
        runtime_selection: bcode_ipc::SessionRuntimeSelection::default(),
        projection_window: None,
    };

    attached.runtime_selection.agent_id = Some("plan".to_owned());
    super::session_flow::complete_switch_session(
        &mut chat,
        session_id,
        Ok((
            attached,
            tokio::spawn(async move {
                drop(event_receiver);
            }),
        )),
    );

    assert_eq!(chat.app.current_agent_id(), "plan");
    assert_eq!(chat.app.current_agent_accent(), Some("#6b7280"));
    assert_eq!(chat.app.composer().text(), "draft while opening");
    assert_eq!(chat.app.status(), "session writable and attached");
    drop(chat);
}

#[tokio::test]
async fn async_session_open_initial_state_preserves_existing_draft() {
    let (sender, receiver) = crate::history_flow::session_stream_channel();
    let session_id = SessionId::new();
    let mut chat = super::session_flow::ActiveChat {
        app: BmuxApp::new_with_history(None, &[], &[]),
        agents: super::session_flow::AgentCatalog::default(),
        attachment: super::session_flow::ChatSessionAttachment::Draft,
        event_sender: sender,
        event_receiver: receiver,
        event_task: None,
        opening_session_progress: None,
        pending_effects: super::effects::TuiEffectQueue::default(),
    };
    chat.app.replace_composer_with("draft before opening");

    super::session_flow::start_switch_session(
        &mut chat,
        session_id,
        super::session_flow::initial_transcript_window_request(Rect::new(0, 0, 80, 24)),
    );

    assert_eq!(chat.app.composer().text(), "draft before opening");
}

#[tokio::test]
async fn async_session_open_failure_clears_progress_and_remains_visible() {
    let (sender, receiver) = crate::history_flow::session_stream_channel();
    let session_id = SessionId::new();
    let mut chat = super::session_flow::ActiveChat {
        app: BmuxApp::new_with_history(Some(session_id), &[], &[]),
        agents: super::session_flow::AgentCatalog::default(),
        attachment: super::session_flow::ChatSessionAttachment::Opening {
            session_id,
            anchor_sequence: None,
        },
        event_sender: sender,
        event_receiver: receiver,
        event_task: None,
        opening_session_progress: Some(bcode_session_models::SessionOpenOperationSnapshot {
            operation_id: bcode_session_models::SessionOpenOperationId::new(),
            revision: 1,
            session_id,
            source_writer_epoch: Some(3),
            target_writer_epoch: 4,
            progress: bcode_session_models::SessionMigrationProgress {
                stage: bcode_session_models::SessionMigrationStage::CopyingBackup,
                completed_units: Some(1),
                total_units: Some(2),
                unit: Some(bcode_session_models::SessionMigrationProgressUnit::Files),
                message: "Copying backup".to_owned(),
            },
            outcome: None,
            backup_path: None,
        }),
        pending_effects: super::effects::TuiEffectQueue::default(),
    };

    super::session_flow::complete_switch_session(
        &mut chat,
        session_id,
        Err(super::TuiError::SessionUnavailable {
            session_id,
            reason: "backup verification failed".to_owned(),
        }),
    );

    assert_eq!(chat.opening_session_id(), None);
    assert_eq!(chat.opening_session_progress, None);
    assert!(chat.app.status().contains("backup verification failed"));
    // A failed open must not leave a session-scoped identity the view cannot serve.
    assert_eq!(chat.attached_session_id(), None);
    assert_eq!(chat.viewing_session_id(), None);
    drop(chat);
}

#[tokio::test]
async fn racing_session_open_result_does_not_capture_attachment() {
    let (sender, receiver) = crate::history_flow::session_stream_channel();
    let opening_session_id = SessionId::new();
    let superseded_session_id = SessionId::new();
    let mut chat = super::session_flow::ActiveChat {
        app: BmuxApp::new_with_history(Some(opening_session_id), &[], &[]),
        agents: super::session_flow::AgentCatalog::default(),
        attachment: super::session_flow::ChatSessionAttachment::Opening {
            session_id: opening_session_id,
            anchor_sequence: None,
        },
        event_sender: sender,
        event_receiver: receiver,
        event_task: None,
        opening_session_progress: None,
        pending_effects: super::effects::TuiEffectQueue::default(),
    };

    super::session_flow::complete_switch_session(
        &mut chat,
        superseded_session_id,
        Err(super::TuiError::SessionUnavailable {
            session_id: superseded_session_id,
            reason: "superseded".to_owned(),
        }),
    );

    // The in-flight open must remain authoritative rather than being cleared by a stale result.
    assert_eq!(chat.opening_session_id(), Some(opening_session_id));
    assert_eq!(chat.attached_session_id(), None);
    drop(chat);
}

#[test]
fn detached_session_view_still_permits_session_scoped_dispatch() {
    let (sender, receiver) = crate::history_flow::session_stream_channel();
    let session_id = SessionId::new();
    let mut chat = super::session_flow::ActiveChat {
        app: BmuxApp::new_with_history(Some(session_id), &[], &[]),
        agents: super::session_flow::AgentCatalog::default(),
        attachment: super::session_flow::ChatSessionAttachment::Attached { session_id },
        event_sender: sender,
        event_receiver: receiver,
        event_task: None,
        opening_session_progress: None,
        pending_effects: super::effects::TuiEffectQueue::default(),
    };

    chat.mark_stream_detached(session_id);

    // Commands travel over independent client requests, so a reconnecting view stays dispatchable.
    assert_eq!(chat.attached_session_id(), Some(session_id));
    assert_eq!(chat.viewing_session_id(), Some(session_id));

    chat.mark_attached(session_id);
    assert_eq!(chat.attached_session_id(), Some(session_id));
    drop(chat);
}

#[test]
fn opening_session_view_defers_session_scoped_dispatch() {
    let (sender, receiver) = crate::history_flow::session_stream_channel();
    let session_id = SessionId::new();
    let chat = super::session_flow::ActiveChat {
        app: BmuxApp::new_with_history(Some(session_id), &[], &[]),
        agents: super::session_flow::AgentCatalog::default(),
        attachment: super::session_flow::ChatSessionAttachment::Opening {
            session_id,
            anchor_sequence: None,
        },
        event_sender: sender,
        event_receiver: receiver,
        event_task: None,
        opening_session_progress: None,
        pending_effects: super::effects::TuiEffectQueue::default(),
    };

    // Identity is known for presentation, but session-scoped work must wait for attachment.
    assert_eq!(chat.viewing_session_id(), Some(session_id));
    assert_eq!(chat.attached_session_id(), None);
    drop(chat);
}

#[tokio::test]
async fn async_session_open_initial_state_preserves_plugin_host() {
    let (sender, receiver) = crate::history_flow::session_stream_channel();
    let session_id = SessionId::new();
    let mut chat = super::session_flow::ActiveChat {
        app: BmuxApp::new_with_history(None, &[], &[]),
        agents: super::session_flow::AgentCatalog::default(),
        attachment: super::session_flow::ChatSessionAttachment::Draft,
        event_sender: sender,
        event_receiver: receiver,
        event_task: None,
        opening_session_progress: None,
        pending_effects: super::effects::TuiEffectQueue::default(),
    };
    chat.app.set_plugin_host(Arc::new(filesystem_plugin_host()));

    super::session_flow::start_switch_session(
        &mut chat,
        session_id,
        super::session_flow::initial_transcript_window_request(Rect::new(0, 0, 80, 24)),
    );

    assert!(chat.app.has_plugin_backend());
}

#[tokio::test]
async fn async_session_open_completion_preserves_plugin_host() {
    let (sender, receiver) = crate::history_flow::session_stream_channel();
    let session_id = SessionId::new();
    let mut chat = super::session_flow::ActiveChat {
        app: BmuxApp::new_with_history(Some(session_id), &[], &[]),
        agents: super::session_flow::AgentCatalog::default(),
        attachment: super::session_flow::ChatSessionAttachment::Opening {
            session_id,
            anchor_sequence: None,
        },
        event_sender: sender,
        event_receiver: receiver,
        event_task: None,
        opening_session_progress: None,
        pending_effects: super::effects::TuiEffectQueue::default(),
    };
    chat.app.set_plugin_host(Arc::new(filesystem_plugin_host()));
    let (_event_sender, event_receiver) = tokio::sync::broadcast::channel::<SessionEvent>(1);
    let attached = AttachedSessionHistory {
        submissions: Vec::new(),
        session: session_summary(session_id),
        history: Vec::new(),
        input_history: Vec::new(),
        usage_summary: bcode_session_models::SessionUsageSummary::default(),
        import_warnings: Vec::new(),
        draft: None,
        runtime_selection: bcode_ipc::SessionRuntimeSelection::default(),
        projection_window: None,
    };

    super::session_flow::complete_switch_session(
        &mut chat,
        session_id,
        Ok((
            attached,
            tokio::spawn(async move {
                drop(event_receiver);
            }),
        )),
    );

    assert!(chat.app.has_plugin_backend());
}

#[test]
fn switch_to_draft_session_preserves_plugin_host() {
    let (sender, receiver) = crate::history_flow::session_stream_channel();
    let session_id = SessionId::new();
    let mut chat = super::session_flow::ActiveChat {
        app: BmuxApp::new_with_history(Some(session_id), &[], &[]),
        agents: super::session_flow::AgentCatalog::default(),
        attachment: super::session_flow::ChatSessionAttachment::Attached { session_id },
        event_sender: sender,
        event_receiver: receiver,
        event_task: None,
        opening_session_progress: None,
        pending_effects: super::effects::TuiEffectQueue::default(),
    };
    chat.app.set_plugin_host(Arc::new(filesystem_plugin_host()));

    super::session_flow::switch_to_draft_session(&mut chat);

    assert!(chat.app.has_plugin_backend());
}

#[test]
fn slash_pending_submission_clears_after_take() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    app.replace_composer_with("/plan");
    app.stage_submission();
    let message = app.take_pending_submission();

    app.clear_pending_submission(&message);

    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 10));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(app);
    let output = rendered_text(&buffer);

    assert!(!output.contains("/plan"));
    assert!(!output.contains("[sending]"));
}

#[test]
fn taken_pending_submission_can_be_restored_after_send_failure() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    app.replace_composer_with("hello");
    app.stage_submission();
    let message = app.take_pending_submission();

    app.restore_pending_submission(&message);

    assert_eq!(app.composer().text(), "hello");
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 10));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(app);
    let output = rendered_text(&buffer);

    assert!(!output.contains("[sending]"));
}

#[test]
fn slash_palette_renders_above_composer() {
    let palette = slash_palette::SlashPalette::from_items(vec![
        ("/plan", "Switch to plan agent"),
        ("/build", "Switch to build agent"),
    ]);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 20));
    let mut frame = Frame::new(&mut buffer);

    slash_palette_render::render_palette(
        &palette,
        Rect::new(2, 18, 76, 1),
        &mut bmux_tui::paint::PaintCx::new(&mut frame),
        render::TuiTheme::for_agent("build", None, true),
        bcode_tui_theme::chrome::MenuChrome::default(),
    );
    let output = rendered_text(&buffer);

    assert!(output.contains("Commands"));
    assert!(output.contains("/plan"));
    assert_eq!(buffer.row_symbols(0).unwrap().trim(), "");
    assert!(buffer.row_symbols(13).unwrap().contains("Commands"));
}

#[test]
fn slash_palette_layout_is_bounded_at_tiny_sizes() {
    let palette = slash_palette::SlashPalette::from_items(vec![("/界👩‍💻e\u{301}", "Description")]);
    for width in 0..16 {
        for composer_y in 1..6 {
            let bounds = Rect::new(0, 0, width, 8);
            let composer = Rect::new(2, composer_y, width.saturating_sub(4), 1);
            let area = slash_palette_render::slash_palette_area(
                bounds,
                slash_palette_render::composer_panel_area(composer),
                palette.item_count(),
                bcode_tui_theme::chrome::MenuChrome::default(),
            );
            if let Some(area) = area {
                assert!(area.right() <= bounds.right());
                assert!(area.bottom() <= composer_y.saturating_sub(1));
                assert!(area.height >= 3);
            }
            let mut buffer = Buffer::empty(bounds);
            let mut frame = Frame::new(&mut buffer);
            slash_palette_render::render_palette(
                &palette,
                composer,
                &mut bmux_tui::paint::PaintCx::new(&mut frame),
                render::TuiTheme::for_agent("build", None, true),
                bcode_tui_theme::chrome::MenuChrome::default(),
            );
            if area.is_none() {
                assert!(buffer.cells().iter().all(|cell| cell.symbol == " "));
            }
        }
    }
}

#[test]
fn slash_palette_mouse_agrees_with_painted_scrolled_rows() {
    let mut palette = slash_palette::SlashPalette::from_items(vec![
        ("/zero", "Zero"),
        ("/one", "One"),
        ("/two", "Two"),
        ("/three", "Three"),
        ("/four", "Four"),
        ("/five", "Five"),
        ("/six", "Six"),
        ("/seven", "Seven"),
        ("/eight", "Eight"),
    ]);
    palette.select_command("/eight");
    let bounds = Rect::new(0, 0, 50, 20);
    let composer = Rect::new(2, 18, 46, 1);
    let area = slash_palette_render::slash_palette_area(
        bounds,
        slash_palette_render::composer_panel_area(composer),
        palette.item_count(),
        bcode_tui_theme::chrome::MenuChrome::default(),
    )
    .unwrap();
    let list = slash_palette_render::slash_palette_list_area(
        bounds,
        composer,
        palette.item_count(),
        bcode_tui_theme::chrome::MenuChrome::default(),
    )
    .unwrap();
    let mut buffer = Buffer::empty(bounds);
    let mut frame = Frame::new(&mut buffer);
    slash_palette_render::render_palette(
        &palette,
        composer,
        &mut bmux_tui::paint::PaintCx::new(&mut frame),
        render::TuiTheme::for_agent("build", None, true),
        bcode_tui_theme::chrome::MenuChrome::default(),
    );
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let hit = slash_palette_render::slash_palette_row_from_mouse(
                bounds,
                composer,
                x,
                y,
                palette.item_count(),
                bcode_tui_theme::chrome::MenuChrome::default(),
            );
            if y == area.y || y == area.bottom() - 1 || x < area.x + 2 || x >= area.right() - 2 {
                assert_eq!(hit, None, "chrome/padding must not select at {x},{y}");
            } else {
                let row = usize::from(y - list.y);
                assert_eq!(hit, Some(row));
                let source = palette
                    .visible_items(usize::from(list.height))
                    .nth(row)
                    .unwrap();
                assert!(
                    buffer
                        .row_symbols(y)
                        .unwrap()
                        .contains(source.item.command())
                );
                let mut clicked = palette.clone();
                assert_eq!(
                    clicked.select_visible_row(row, usize::from(list.height)),
                    Some(source.item.command())
                );
            }
        }
    }
    assert!(
        buffer
            .row_symbols(area.bottom() - 2)
            .unwrap()
            .contains("› /eight")
    );
}

#[test]
fn slash_palette_command_first_unicode_clipping_and_styles() {
    use bmux_tui::geometry::Point;
    use bmux_tui::style::Modifier;

    let palette = slash_palette::SlashPalette::from_items(vec![
        ("/selected", "Selection"),
        ("/界👩‍💻e\u{301}", "Muted description"),
        ("/this-command-is-longer-than-thirty-columns", "Details"),
    ]);
    let theme = render::TuiTheme::for_agent("build", None, true);
    for width in [9, 16, 28, 80] {
        let bounds = Rect::new(0, 0, width, 20);
        let composer = Rect::new(2, 18, width - 4, 1);
        let mut buffer = Buffer::empty(bounds);
        let mut frame = Frame::new(&mut buffer);
        slash_palette_render::render_palette(
            &palette,
            composer,
            &mut bmux_tui::paint::PaintCx::new(&mut frame),
            theme,
            bcode_tui_theme::chrome::MenuChrome::default(),
        );
        let area = slash_palette_render::slash_palette_area(
            bounds,
            slash_palette_render::composer_panel_area(composer),
            palette.item_count(),
            bcode_tui_theme::chrome::MenuChrome::default(),
        )
        .unwrap();
        for y in area.y..area.bottom() {
            assert_eq!(
                buffer.get(Point::new(area.right() - 1, y)).unwrap().symbol,
                if y == area.y {
                    "┐"
                } else if y == area.bottom() - 1 {
                    "┘"
                } else {
                    "│"
                }
            );
        }
        if width == 80 {
            let output = rendered_text(&buffer);
            assert!(output.contains("/界👩‍💻e\u{301}  [cmd]  Muted description"));
            assert!(output.contains("/this-command-is-longer-than-thirty-columns"));
            let command = buffer.get(Point::new(area.x + 4, area.y + 2)).unwrap();
            assert!(command.style.modifiers.contains(Modifier::BOLD));
            assert_eq!(command.style.fg, theme.text.fg);
            let description_x = area.x
                + 4
                + u16::try_from(bmux_tui::text_width::display_width("/界👩‍💻e\u{301}")).unwrap()
                + 9;
            let description = buffer.get(Point::new(description_x, area.y + 2)).unwrap();
            assert_eq!(description.style.fg, theme.muted.fg);
            assert!(!description.style.modifiers.contains(Modifier::BOLD));
            assert_eq!(description.style.bg, theme.raised.bg);
        }
    }
}

#[test]
fn slash_palette_repaints_presented_focus_and_selection_styles() {
    use bmux_tui::geometry::Point;
    use bmux_tui::style::Color;

    let palette =
        slash_palette::SlashPalette::from_items(vec![("/plan", "Plan"), ("/build", "Build")]);
    let bounds = Rect::new(0, 0, 50, 20);
    let composer = Rect::new(2, 18, 46, 1);
    let area = slash_palette_render::slash_palette_area(
        bounds,
        slash_palette_render::composer_panel_area(composer),
        palette.item_count(),
        bcode_tui_theme::chrome::MenuChrome::default(),
    )
    .unwrap();
    let mut buffer = Buffer::empty(bounds);
    for accent in [Color::Red, Color::Green] {
        let mut theme = render::TuiTheme::for_agent("build", None, true);
        theme.focused = theme.focused.fg(accent).bg(Color::Yellow);
        theme.selection = theme.selection.bg(Color::Blue);
        let mut frame = Frame::new(&mut buffer);
        slash_palette_render::render_palette(
            &palette,
            composer,
            &mut bmux_tui::paint::PaintCx::new(&mut frame),
            theme,
            bcode_tui_theme::chrome::MenuChrome::default(),
        );
        for point in [
            Point::new(area.x, area.y + 1),
            Point::new(area.x + 3, area.y),
            Point::new(area.x + 2, area.y + 1),
        ] {
            assert_eq!(buffer.get(point).unwrap().style.fg, Some(accent));
        }
        for x in area.x + 2..area.right() - 2 {
            assert_eq!(
                buffer.get(Point::new(x, area.y + 1)).unwrap().style.bg,
                Some(Color::Blue),
                "selected row background at {x}"
            );
        }
        assert_eq!(
            buffer
                .get(Point::new(area.right() - 3, area.y + 2))
                .unwrap()
                .style
                .bg,
            theme.raised.bg
        );
    }
}

#[test]
fn history_rebuild_does_not_duplicate_initial_history() {
    let session_id = SessionId::new();
    let history = [
        event(
            session_id,
            1,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "first".to_owned(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::AssistantMessage {
                text: "second".to_owned(),
            },
        ),
    ];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[]);

    app.apply_thinking_config(TuiThinkingConfig::default());

    assert_eq!(shared_message_texts(&app, "You"), ["first"]);
    assert_eq!(shared_message_texts(&app, "Assistant"), ["second"]);
    drop(app);
}

#[test]
fn initial_transcript_window_request_uses_viewport_targets() {
    let request = super::history_flow::initial_transcript_window_request(Rect::new(0, 0, 100, 20));

    assert_eq!(request.projection, SessionProjectionKind::Transcript);
    assert_eq!(request.target.width_columns, Some(100));
    assert_eq!(request.target.min_items, Some(12));
    assert_eq!(request.target.min_estimated_rows, Some(40));
    assert_eq!(request.limits.max_events_scanned, 2_048);
}

#[test]
fn live_event_overlapping_initial_history_is_ignored() {
    let session_id = SessionId::new();
    let history = [
        event(
            session_id,
            1,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "question".to_owned(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ),
        event(
            session_id,
            2,
            SessionEventKind::AssistantMessage {
                text: "answer".to_owned(),
            },
        ),
    ];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[]);

    app.absorb_session_event(&history[1]);

    assert_eq!(shared_message_texts(&app, "Assistant"), ["answer"]);
    drop(app);
}

#[test]
fn newer_live_event_after_initial_history_is_absorbed() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        1,
        SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: "question".to_owned(),
            admission: bcode_session_models::TurnAdmissionMetadata::default(),
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[]);

    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::AssistantMessage {
            text: "answer".to_owned(),
        },
    ));

    assert!(
        shared_message_texts(&app, "Assistant")
            .iter()
            .any(|text| text == "answer")
    );
    drop(app);
}

/// Texts of the user (`You`) or assistant (`Assistant`) messages in the host shell's session view.
fn shared_message_texts(app: &BmuxApp, role: &str) -> Vec<String> {
    app.session_view_snapshot()
        .transcript
        .items
        .iter()
        .filter_map(|item| match (&item.kind, role) {
            (bcode_session_view_models::TranscriptViewItemKind::UserMessage { message }, "You")
            | (
                bcode_session_view_models::TranscriptViewItemKind::AssistantMessage { message },
                "Assistant",
            ) => Some(message.text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn live_file_write_statusline_is_not_duplicated_and_truncates_path() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call_write".to_owned(),
            producer_plugin_id: None,
            tool_name: "filesystem_write".to_owned(),
            arguments_json: serde_json::json!({
                "path": "/Users/braden/projects/bcode/packages/tui/src/render.rs",
                "contents": "fn main() {}\n",
            })
            .to_string(),
            working_directory: None,
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 72, 16));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(app);
    let output = rendered_text(&buffer);

    assert!(output.contains("tool filesystem_write"));
    assert!(!output.contains("\"path\":"), "{output}");
    assert!(
        !output.contains("/Users/braden/projects/bcode/packages/tui/src/render.rs"),
        "{output}"
    );
    assert!(!output.contains("File change preview"));
    assert!(!output.contains("tool filesystem_write · running tool filesystem_write"));
}

#[test]
fn denied_file_permission_marks_preview_failed() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    let args = serde_json::json!({
        "path": "src/lib.rs",
        "old_text": "old\n",
        "new_text": "new\n",
    })
    .to_string();
    app.absorb_session_event(&event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call_edit".to_owned(),
            producer_plugin_id: None,
            tool_name: "filesystem_edit".to_owned(),
            arguments_json: args.clone(),
            working_directory: None,
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        2,
        SessionEventKind::PermissionRequested {
            permission_id: "perm_edit".to_owned(),
            tool_call_id: "call_edit".to_owned(),
            producer_plugin_id: None,
            tool_name: "filesystem_edit".to_owned(),
            arguments_json: args,
            batch: None,
            policy_source: None,
            policy_reason: None,
        },
    ));
    app.absorb_session_event(&event(
        session_id,
        3,
        SessionEventKind::PermissionResolved {
            permission_id: "perm_edit".to_owned(),
            approved: false,
        },
    ));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 40));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(app);
    let output = rendered_text(&buffer);

    assert!(!output.contains("File change preview · filesystem_edit"));
}

#[test]
fn transcript_renders_filesystem_edit_request_without_core_inline_preview() {
    let session_id = SessionId::new();
    let history = [event(
        session_id,
        1,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call_edit".to_owned(),
            producer_plugin_id: None,
            tool_name: "example.edit".to_owned(),
            arguments_json: serde_json::json!({
                "path": "src/lib.rs",
                "old_text": "fn answer() -> i32 {\n    41\n}\n",
                "new_text": "fn answer() -> i32 {\n    42\n}\n",
            })
            .to_string(),
            working_directory: None,
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[]);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 100, 18));
    let mut frame = Frame::new(&mut buffer);

    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    let output = rendered_text(&buffer);

    assert!(output.contains("example.edit"), "{output}");
    assert!(!output.contains("\"path\""), "{output}");
    assert!(!output.contains("\"old_text\""), "{output}");
    assert!(!output.contains("\"new_text\""), "{output}");
    assert!(!output.contains("File change preview"), "{output}");
    assert!(!output.contains("replaced 1 line with 1 line"), "{output}");
}

#[test]
fn staged_user_message_does_not_navigate_before_semantic_acceptance() {
    let session_id = SessionId::new();
    let history = (0..12)
        .map(|sequence| {
            event(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            )
        })
        .collect::<Vec<_>>();
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[]);
    app.set_plugin_host(Arc::new(shell_plugin_host()));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));

    app.replace_composer_with("new prompt");
    app.stage_submission();
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    std::thread::sleep(Duration::from_millis(220));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(app);

    assert_ne!(output_line_y(&buffer, "You · sending"), Some(1));
}

#[test]
fn accepted_markdown_submission_preserves_submitted_user_message_transition() {
    let session_id = SessionId::new();
    let history = (0..12)
        .map(|sequence| {
            event(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            )
        })
        .collect::<Vec<_>>();
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[]);
    app.set_plugin_host(Arc::new(shell_plugin_host()));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));

    app.replace_composer_with(
        "# New prompt\n\n- first\n- second with **emphasis**\n\n```sh\ncargo test\n```",
    );
    app.stage_submission();
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    std::thread::sleep(Duration::from_millis(220));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    assert_ne!(output_line_y(&buffer, "You · sending"), Some(1));

    app.mark_pending_submission_sent();
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    std::thread::sleep(Duration::from_millis(220));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(app);

    assert_ne!(output_line_y(&buffer, "message 11"), Some(1));
}

#[test]
fn cleared_submission_does_not_anchor() {
    let session_id = SessionId::new();
    let history = (0..12)
        .map(|sequence| {
            event(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            )
        })
        .collect::<Vec<_>>();
    let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[]);
    app.set_plugin_host(Arc::new(shell_plugin_host()));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));

    app.replace_composer_with("/help");
    app.stage_submission();
    let slash = app.take_pending_submission();
    app.clear_pending_submission(&slash);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 12));
    let mut frame = Frame::new(&mut buffer);
    render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    drop(app);

    assert_ne!(output_line_y(&buffer, "message 11"), Some(1));
}

#[test]
fn transcript_resident_window_does_not_trim_with_active_tool() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);

    for turn in 0..500_u64 {
        let user_sequence = turn.saturating_mul(2).saturating_add(1);
        app.absorb_session_event(&event(
            session_id,
            user_sequence,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: format!("user {turn}"),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ));
        app.absorb_session_event(&event(
            session_id,
            user_sequence.saturating_add(1),
            SessionEventKind::AssistantMessage {
                text: format!("assistant {turn}"),
            },
        ));
    }
    app.absorb_session_event(&event(
        session_id,
        1_001,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "active-tool".to_owned(),
            producer_plugin_id: None,
            tool_name: "shell.run".to_owned(),
            arguments_json: "{}".to_owned(),
            working_directory: None,
        },
    ));
    for index in 0..50_u64 {
        app.absorb_session_event(&event(
            session_id,
            1_002_u64.saturating_add(index),
            SessionEventKind::AssistantDelta {
                text: format!("still active {index}"),
            },
        ));
    }

    assert!(app.resident_transcript_event_count() > 1_024);
    drop(app);
}

#[test]
fn transcript_resident_window_prunes_old_tool_state_after_trim() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);

    for turn in 0..360_u64 {
        let base = turn.saturating_mul(3).saturating_add(1);
        let tool_call_id = format!("tool-{turn}");
        app.absorb_session_event(&event(
            session_id,
            base,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: format!("user {turn}"),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ));
        app.absorb_session_event(&event(
            session_id,
            base.saturating_add(1),
            SessionEventKind::ToolCallRequested {
                tool_call_id: tool_call_id.clone(),
                producer_plugin_id: None,
                tool_name: "shell.run".to_owned(),
                arguments_json: "{}".to_owned(),
                working_directory: None,
            },
        ));
        app.absorb_session_event(&event(
            session_id,
            base.saturating_add(2),
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: tool_call_id,
                    model_output: "ok".to_owned(),
                    is_error: false,
                    presentation: None,
                    result: None,
                    content: Vec::new(),
                },
            },
        ));
    }

    assert!(app.resident_transcript_event_count() <= 600);
    assert!(app.session_view_snapshot().tools.len() < 360);
    drop(app);
}

#[test]
fn transcript_resident_window_keeps_pending_submission_repeating_committed_text() {
    let session_id = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
    let mut sequence = 0_u64;
    let mut next_sequence = || {
        sequence = sequence.saturating_add(1);
        sequence
    };
    for turn in 0..500_u64 {
        let text = if turn == 495 {
            "yes".to_owned()
        } else {
            format!("user {turn}")
        };
        app.absorb_session_event(&event(
            session_id,
            next_sequence(),
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text,
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ));
        app.absorb_session_event(&event(
            session_id,
            next_sequence(),
            SessionEventKind::AssistantMessage {
                text: format!("assistant {turn}"),
            },
        ));
    }
    // The committed "yes" stays resident across a trim; its replay must not forget the
    // queued follow-up with the same text.
    app.replace_composer_with("yes");
    app.stage_submission();
    let _ = app.take_pending_submission();
    for turn in 0..30_u64 {
        app.absorb_session_event(&event(
            session_id,
            next_sequence(),
            SessionEventKind::AssistantMessage {
                text: format!("live {turn}"),
            },
        ));
    }

    let pending = app
        .pending_submissions()
        .iter()
        .map(|pending| pending.text().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(pending, ["yes"]);
    assert!(app.resident_transcript_event_count() > 1_024);
    drop(app);
}

fn filesystem_plugin_host() -> bcode_plugin::PluginHost {
    let bundled = [bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/filesystem-plugin/bcode-plugin.toml"),
        bcode_filesystem_plugin::static_plugin(),
    )];
    let selected = bcode_plugin::filter_selected_static_plugins(
        &bundled,
        &bcode_plugin::PluginSelection::all_enabled(),
    )
    .expect("static filesystem plugin manifest should parse");
    bcode_plugin::PluginHost::load_static_plugins(&selected)
        .expect("static filesystem plugin should load")
}

fn filesystem_visual_source_colors(
    app: &BmuxApp,
    schema: &str,
    payload: &serde_json::Value,
) -> Vec<bmux_tui::style::Color> {
    let context = bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
        80,
        bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
        None,
    )
    .with_theme(render::plugin_theme_for_app(app));
    let visual = app
        .plugin_presentation()
        .expect("filesystem presentation installed")
        .routed_visual(
            "filesystem-theme-test",
            0,
            schema,
            1,
            Some("bcode.filesystem"),
            payload,
            &context,
        )
        .expect("filesystem visual resolves");
    visual
        .rows
        .iter()
        .flat_map(|line| line.spans.iter())
        .filter_map(|span| span.style.fg)
        .collect()
}

#[test]
fn filesystem_read_and_grep_follow_resolved_bundled_themes() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    disable_theme_transition(&mut app);
    app.set_plugin_host(Arc::new(filesystem_plugin_host()));
    let read = serde_json::json!({
        "path": "src/lib.rs",
        "contents": "let value = 1;\npub fn main() {}",
        "start_line": 1,
        "truncated": false
    });
    let grep = serde_json::json!({
        "matches": [{"path": "src/lib.rs", "line_number": 1, "line": "pub fn main() {}"}],
        "backend": "rust",
        "partial": false
    });

    assert!(app.apply_theme("terminal-native"));
    let terminal_read = filesystem_visual_source_colors(&app, "bcode.filesystem.read", &read);
    let terminal_grep = filesystem_visual_source_colors(&app, "bcode.filesystem.grep", &grep);
    assert!(terminal_read.contains(&bmux_tui::style::Color::Cyan));
    assert!(terminal_grep.contains(&bmux_tui::style::Color::Blue));
    assert!(terminal_grep.contains(&bmux_tui::style::Color::Cyan));

    assert!(app.apply_theme("nord"));
    let nord_read = filesystem_visual_source_colors(&app, "bcode.filesystem.read", &read);
    let nord_grep = filesystem_visual_source_colors(&app, "bcode.filesystem.grep", &grep);
    drop(app);
    let nord_keyword = bmux_tui::style::Color::Rgb(129, 161, 193);
    let nord_function = bmux_tui::style::Color::Rgb(136, 192, 208);
    assert!(nord_read.contains(&nord_function));
    assert!(nord_grep.contains(&nord_keyword));
    assert!(nord_grep.contains(&nord_function));
    assert_ne!(nord_read, terminal_read);
}

#[test]
fn replayed_legacy_shell_artifact_does_not_read_files_during_render() {
    let session_id = SessionId::new();
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let pty_path = temp_dir.path().join("raw-pty.txt");
    std::fs::write(&pty_path, "first\rsecond\n").expect("write pty artifact");
    let mut artifact = shell_run_artifact();
    artifact.metadata["output_tail"] =
        serde_json::Value::String("artifact raw fallback\n".to_owned());
    artifact.refs.push(ToolArtifactRef {
        key: "terminal_pty_stream".to_owned(),
        content_type: Some("application/x-bcode-terminal-pty-stream; charset=utf-8".to_owned()),
        storage_uri: Some(format!("file://{}", pty_path.to_string_lossy())),
        byte_len: Some(13),
        metadata: Some(serde_json::json!({
            "stream": "pty",
            "columns": 80,
            "rows": 24,
            "retained_tail_bytes": 13,
            "tail_truncated": false,
        })),
    });
    let events = vec![event(
        session_id,
        1,
        SessionEventKind::ToolInvocationResultRecorded {
            record: bcode_session_models::ToolInvocationResultRecord {
                invocation_id: "call-shell".to_owned(),
                model_output: "shell completed".to_owned(),
                is_error: false,
                presentation: None,
                result: Some(ToolInvocationResult::Artifact {
                    artifact: Box::new(artifact),
                }),
                content: Vec::new(),
            },
        },
    )];
    let mut app = BmuxApp::new_with_history(Some(session_id), &events, &[]);
    app.set_plugin_host(Arc::new(shell_plugin_host()));

    let rendered = render_app_text(&mut app);
    drop(app);

    assert!(!rendered.contains("second"), "{rendered}");
    assert!(!rendered.contains("first"), "{rendered}");
    assert!(!rendered.contains("artifact raw fallback"), "{rendered}");
}

#[test]
fn thinking_label_uses_effective_values() {
    let mut app = BmuxApp::new_with_history(None, &[], &[]);
    app.apply_model_status(bcode_model::SessionModelStatus {
        provider_plugin_id: None,
        requested_model_id: None,
        effective_model_id: None,
        model_id: None,
        display_name: None,
        context_window: None,
        context_occupancy: None,
        request_context_error: None,
        auth_profile: None,
        context_format_version: None,
        compatibility_key: None,
        max_output_tokens: None,
        reasoning: Some(bcode_model::ModelReasoningInfo {
            control: None,
            effort_values: vec!["low".to_owned(), "medium".to_owned(), "high".to_owned()],
            default_effort: Some("medium".to_owned()),
            visible_summary_supported: true,
            summary_values: vec!["auto".to_owned(), "detailed".to_owned()],
            default_summary: Some("auto".to_owned()),
            raw_reasoning_supported: false,
            source: bcode_model::ModelReasoningCapabilitySource::KnownModelTable,
        }),
        reasoning_effort: None,
        reasoning_summary: Some("detailed".to_owned()),
        prompt_cache_mode: None,
        conversation_reuse_mode: None,
        compaction_mode: None,
        compaction_backend: None,
        proactive_compaction_threshold_percent: None,
        proactive_compaction_threshold_tokens: None,
        proactive_compaction_effective_threshold_tokens: None,
        proactive_compaction_threshold_source: None,
        cache: None,
        metadata_source: None,
        pricing: None,
    });
    app.set_reasoning_visible(true);

    assert_eq!(
        app.thinking_label(),
        "display: all · request effort: medium · provider summary: detailed"
    );
    assert_eq!(app.model_header_label(), "default [medium]");
    drop(app);
}

#[test]
fn thinking_dialog_cycles_supported_values_in_semantic_order() {
    let status = bcode_model::SessionModelStatus {
        provider_plugin_id: None,
        requested_model_id: None,
        effective_model_id: None,
        model_id: None,
        display_name: None,
        context_window: None,
        context_occupancy: None,
        request_context_error: None,
        auth_profile: None,
        context_format_version: None,
        compatibility_key: None,
        max_output_tokens: None,
        reasoning: Some(bcode_model::ModelReasoningInfo {
            control: None,
            effort_values: vec!["high".to_owned(), "low".to_owned(), "medium".to_owned()],
            default_effort: Some("low".to_owned()),
            visible_summary_supported: true,
            summary_values: vec!["auto".to_owned(), "detailed".to_owned()],
            default_summary: Some("auto".to_owned()),
            raw_reasoning_supported: false,
            source: bcode_model::ModelReasoningCapabilitySource::KnownModelTable,
        }),
        reasoning_effort: Some("low".to_owned()),
        reasoning_summary: Some("auto".to_owned()),
        prompt_cache_mode: None,
        conversation_reuse_mode: None,
        compaction_mode: None,
        compaction_backend: None,
        proactive_compaction_threshold_percent: None,
        proactive_compaction_threshold_tokens: None,
        proactive_compaction_effective_threshold_tokens: None,
        proactive_compaction_threshold_source: None,
        cache: None,
        metadata_source: None,
        pricing: None,
    };
    let mut dialog = super::thinking_dialog::ThinkingDialogState::new(
        false,
        bcode_config::TuiThinkingMode::All,
        &status,
    );

    assert_eq!(dialog.effort_values(), ["low", "medium", "high"]);
    dialog.cycle_focused();
    assert!(dialog.visible());
    dialog.focus_next();
    dialog.cycle_focused();
    assert_eq!(dialog.mode(), bcode_config::TuiThinkingMode::Summary);
    dialog.focus_next();
    dialog.cycle_focused();
    assert_eq!(dialog.effort(), Some("medium"));
    dialog.focus_next();
    dialog.cycle_focused();
    assert_eq!(dialog.summary(), Some("detailed"));
}

#[test]
fn thinking_dialog_can_start_focused_on_effort_or_summary() {
    let status = bcode_model::SessionModelStatus {
        provider_plugin_id: None,
        requested_model_id: None,
        effective_model_id: None,
        model_id: None,
        display_name: None,
        context_window: None,
        context_occupancy: None,
        request_context_error: None,
        auth_profile: None,
        context_format_version: None,
        compatibility_key: None,
        max_output_tokens: None,
        reasoning: Some(bcode_model::ModelReasoningInfo {
            control: None,
            effort_values: vec!["low".to_owned(), "medium".to_owned()],
            default_effort: Some("low".to_owned()),
            visible_summary_supported: true,
            summary_values: vec!["auto".to_owned(), "detailed".to_owned()],
            default_summary: Some("auto".to_owned()),
            raw_reasoning_supported: false,
            source: bcode_model::ModelReasoningCapabilitySource::KnownModelTable,
        }),
        reasoning_effort: Some("low".to_owned()),
        reasoning_summary: Some("auto".to_owned()),
        prompt_cache_mode: None,
        conversation_reuse_mode: None,
        compaction_mode: None,
        compaction_backend: None,
        proactive_compaction_threshold_percent: None,
        proactive_compaction_threshold_tokens: None,
        proactive_compaction_effective_threshold_tokens: None,
        proactive_compaction_threshold_source: None,
        cache: None,
        metadata_source: None,
        pricing: None,
    };

    let effort = super::thinking_dialog::ThinkingDialogState::new_focused(
        false,
        bcode_config::TuiThinkingMode::All,
        &status,
        super::thinking_dialog::ThinkingDialogFocus::Effort,
    );
    let summary = super::thinking_dialog::ThinkingDialogState::new_focused(
        false,
        bcode_config::TuiThinkingMode::All,
        &status,
        super::thinking_dialog::ThinkingDialogFocus::Summary,
    );

    assert_eq!(effort.focused_row(), 2);
    assert_eq!(summary.focused_row(), 3);
}

#[test]
fn thinking_dialog_does_not_cycle_when_reasoning_is_unsupported() {
    let status = bcode_model::SessionModelStatus {
        provider_plugin_id: None,
        requested_model_id: None,
        effective_model_id: None,
        model_id: None,
        display_name: None,
        context_window: None,
        context_occupancy: None,
        request_context_error: None,
        auth_profile: None,
        context_format_version: None,
        compatibility_key: None,
        max_output_tokens: None,
        reasoning: None,
        reasoning_effort: None,
        reasoning_summary: None,
        prompt_cache_mode: None,
        conversation_reuse_mode: None,
        compaction_mode: None,
        compaction_backend: None,
        proactive_compaction_threshold_percent: None,
        proactive_compaction_threshold_tokens: None,
        proactive_compaction_effective_threshold_tokens: None,
        proactive_compaction_threshold_source: None,
        cache: None,
        metadata_source: None,
        pricing: None,
    };
    let mut dialog = super::thinking_dialog::ThinkingDialogState::new(
        false,
        bcode_config::TuiThinkingMode::All,
        &status,
    );

    dialog.focus_next();
    dialog.cycle_focused();
    assert_eq!(dialog.mode(), bcode_config::TuiThinkingMode::Summary);
    dialog.focus_next();
    dialog.cycle_focused();
    assert_eq!(dialog.effort(), None);
    assert_eq!(dialog.effort_values(), [] as [String; 0]);
    dialog.focus_next();
    dialog.cycle_focused();
    assert_eq!(dialog.summary(), None);
    assert_eq!(dialog.summary_values(), [] as [String; 0]);
}

#[test]
fn plain_slash_menus_list_rows_without_chrome_and_mouse_agrees_with_paint() {
    use bcode_tui_theme::chrome::{ChromeText, MenuChrome, MenuFrame, Visibility};
    let menus = MenuChrome {
        frame: MenuFrame::Plain,
        marker: ChromeText::new("❯"),
        tags: Visibility::Hidden,
        rows: Some(3),
    };
    let mut palette = slash_palette::SlashPalette::from_items(vec![
        ("/add-dir", "Add a directory"),
        ("/branch", "Branch"),
        ("/clear", "Clear"),
        ("/diff", "Diff"),
    ]);
    palette.select_command("/branch");
    let bounds = Rect::new(0, 0, 50, 12);
    let composer = Rect::new(2, 10, 46, 1);
    let area = slash_palette_render::slash_palette_area(
        bounds,
        slash_palette_render::composer_panel_area(composer),
        palette.item_count(),
        menus,
    )
    .expect("area");
    assert_eq!(
        area.height, 3,
        "rows bound the menu and no frame rows are added"
    );
    let mut buffer = Buffer::empty(bounds);
    let mut frame = Frame::new(&mut buffer);
    slash_palette_render::render_palette(
        &palette,
        composer,
        &mut bmux_tui::paint::PaintCx::new(&mut frame),
        render::TuiTheme::for_agent("build", None, true),
        menus,
    );
    let rows: Vec<String> = (area.y..area.bottom())
        .map(|y| buffer.row_symbols(y).unwrap().trim_end().to_owned())
        .collect();
    assert!(
        rows.iter().any(|row| row.ends_with("❯ /branch   Branch")),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .all(|row| !row.contains("[cmd]") && !row.contains('│')),
        "{rows:?}"
    );
    for y in area.y..area.bottom() {
        let hit = slash_palette_render::slash_palette_row_from_mouse(
            bounds,
            composer,
            area.x,
            y,
            palette.item_count(),
            menus,
        );
        let row = usize::from(y - area.y);
        assert_eq!(hit, Some(row));
        let item = palette
            .visible_items(usize::from(area.height))
            .nth(row)
            .unwrap();
        assert!(buffer.row_symbols(y).unwrap().contains(item.item.command()));
    }
}
