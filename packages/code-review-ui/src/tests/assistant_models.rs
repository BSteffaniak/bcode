//! The assistant's model, reasoning effort and permission chips, as bdata's panel offers them:
//! the model picker over the models Bcode offers, effort cycling, the app default that new
//! conversations and Ask AI cards get (and that survives a restart), the per-conversation
//! permission categories that narrow what the next turn is offered, failures and usage in the
//! conversation, conversation management, and the chips' hit regions at every width.

use super::assistant::{Setup, setup, setup_with, snapshot, user};
use crate::app::ReviewApp;
use crate::assistant::profile::{AssistantConfig, Category};
use crate::config::ReviewViewConfig;
use crate::entry::Entry;
use crate::host::{ModelCatalog, ModelOption, ProviderModels};
use crate::testing::backend::TestBackend;
use crate::testing::harness::Scenario;
use bcode_code_review::document::ItemKey;
use bcode_session_models::ModelTurnOutcome;
use bcode_session_view_models::{
    ChatMessageView, TranscriptViewItem, TranscriptViewItemId, TranscriptViewItemKind,
};
use bmux_tui::event::MouseEventKind;
use std::time::Duration;

const PROVIDER: &str = "bcode.fake-provider";

fn model(id: &str, name: &str, efforts: &[&str], default: Option<&str>) -> ModelOption {
    ModelOption {
        provider: Some(PROVIDER.to_owned()),
        provider_name: Some("Fake provider".to_owned()),
        model_id: id.to_owned(),
        display_name: name.to_owned(),
        context_window: Some(128_000),
        price: Some(("$1.00".to_owned(), "$2.00".to_owned())),
        effort_values: efforts.iter().map(|effort| (*effort).to_owned()).collect(),
        default_effort: default.map(str::to_owned),
    }
}

/// Bcode's default is the echo model (no effort); the provider also offers a model with three
/// efforts and a model with a wide name; another provider fails to list.
fn catalog() -> ModelCatalog {
    ModelCatalog {
        default: Some(Ok(model("fake-echo", "Fake echo", &[], None))),
        providers: vec![
            ProviderModels {
                provider: PROVIDER.to_owned(),
                name: "Fake provider".to_owned(),
                models: Ok(vec![
                    model("fake-echo", "Fake echo", &[], None),
                    model(
                        "fake-smart",
                        "Fake smart",
                        &["low", "medium", "high"],
                        Some("medium"),
                    ),
                    model("fake-wide", "模型 🚀 wide", &["low"], None),
                ]),
            },
            ProviderModels {
                provider: "bcode.broken".to_owned(),
                name: "Broken provider".to_owned(),
                models: Err("signed out".to_owned()),
            },
        ],
    }
}

fn with_models(setup: &Setup) {
    setup.backend.host().set_model_catalog(Ok(catalog()));
}

/// A review with the panel focused and a first message sent; returns its session.
fn conversation(setup: &mut Setup) -> String {
    with_models(setup);
    setup.scenario.key("ctrl+k");
    setup.settle();
    setup.send("first")
}

/// Choose the model whose rows match `filter` in the open picker.
fn choose(setup: &mut Setup, filter: &str) {
    setup.scenario.paste(filter);
    setup.scenario.key("enter");
    setup.settle();
}

fn last_turn_line(setup: &Setup, session: &str) -> String {
    setup
        .backend
        .host()
        .model_log()
        .into_iter()
        .rev()
        .find(|line| line.starts_with(&format!("submit {session}")))
        .expect("a turn")
}

#[test]
fn the_model_picker_lists_bcodes_models_and_the_next_turn_runs_with_the_choice() {
    let mut setup = setup(140, 40, true);
    let session = conversation(&mut setup);
    assert!(
        setup.settle().contains("Bcode's model (Fake echo)"),
        "the chip names Bcode's default until a model is chosen"
    );
    setup.scenario.key("alt+m");
    setup.settle();
    let labels: Vec<String> = setup
        .app()
        .assistant()
        .overlay_labels()
        .expect("the picker opened")
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert!(
        labels[0].contains("Bcode's default (Fake echo)"),
        "{labels:?}"
    );
    assert!(
        labels
            .iter()
            .any(|label| label.contains("Fake smart fake-smart"))
    );
    assert!(
        labels
            .iter()
            .any(|label| label.contains("Broken provider") && label.contains("signed out")),
        "a provider that failed says why: {labels:?}"
    );
    choose(&mut setup, "smart");
    assert!(setup.app().assistant().overlay_labels().is_none(), "chosen");
    assert_eq!(
        setup
            .backend
            .host()
            .session_choice(&session)
            .model
            .as_deref(),
        Some("fake-smart")
    );
    let screen = setup.settle();
    assert!(screen.contains("● Fake smart"), "{}", screen.text());
    assert!(screen.contains("◆ effort medium"), "{}", screen.text());
    setup.send("second");
    assert_eq!(
        last_turn_line(&setup, &session),
        format!("submit {session} fake-smart default")
    );
}

#[test]
fn effort_cycles_the_models_values_and_a_model_without_effort_says_so() {
    let mut setup = setup(140, 40, true);
    let session = conversation(&mut setup);
    setup.scenario.key("shift+tab");
    setup.settle();
    assert!(
        setup
            .app()
            .assistant()
            .notice()
            .is_some_and(|notice| notice.contains("no reasoning effort")),
        "{:?}",
        setup.app().assistant().notice()
    );
    setup.scenario.key("alt+m");
    choose(&mut setup, "smart");
    setup.scenario.key("shift+tab");
    setup.settle();
    assert_eq!(
        setup
            .backend
            .host()
            .session_choice(&session)
            .effort
            .as_deref(),
        Some("high"),
        "from the model's default, medium"
    );
    setup.scenario.key("shift+tab");
    setup.settle();
    assert_eq!(
        setup
            .backend
            .host()
            .session_choice(&session)
            .effort
            .as_deref(),
        Some("low"),
        "wraps"
    );
    assert!(setup.settle().contains("◆ effort low"));
    setup.send("third");
    assert_eq!(
        last_turn_line(&setup, &session),
        format!("submit {session} fake-smart low")
    );
}

#[test]
fn the_choice_is_the_default_of_new_conversations_and_survives_a_restart() {
    let mut setup = setup(140, 40, true);
    let first = conversation(&mut setup);
    setup.scenario.key("alt+m");
    choose(&mut setup, "smart");
    setup.scenario.key("shift+tab");
    setup.settle();
    setup.scenario.key("ctrl+n");
    setup.settle();
    let second = setup.send("a new conversation");
    assert_ne!(second, first);
    let log = setup.backend.host().model_log();
    let set = log
        .iter()
        .position(|line| *line == format!("model {second} fake-smart"))
        .expect("the new session got the default");
    let effort = log
        .iter()
        .position(|line| *line == format!("effort {second} high"))
        .expect("and its effort");
    let submitted = log
        .iter()
        .position(|line| line.starts_with(&format!("submit {second}")))
        .expect("submitted");
    assert!(set < submitted && effort < submitted, "{log:?}");

    // A restarted app over the same store starts from the same default.
    let app = ReviewApp::new(
        setup.backend.services(),
        setup.review.repo_root.clone(),
        ReviewViewConfig::default(),
        Entry::Workspace {
            workspace: Box::new(setup.review.clone()),
        },
    )
    .with_scroll_motion(Duration::ZERO);
    let mut scenario = Scenario::with_shared_host(app, setup.backend.host(), 140, 40);
    scenario.key("ctrl+k");
    let screen = super::app::settled(&mut scenario);
    assert!(screen.contains("● Fake smart"), "{}", screen.text());
    assert!(screen.contains("◆ effort high"), "{}", screen.text());
}

#[test]
fn turning_a_category_off_narrows_the_next_turn_and_on_again_restores_it() {
    let mut setup = setup(140, 40, true);
    let session = conversation(&mut setup);
    assert_eq!(
        setup
            .backend
            .host()
            .turns()
            .last()
            .unwrap()
            .1
            .tool_allowlist,
        None,
        "every profile tool by default"
    );
    // The permissions overlay lists the categories; shell is the third.
    setup.scenario.key("alt+p");
    setup.settle();
    let labels = setup.app().assistant().overlay_labels().expect("opened");
    assert!(labels[2].contains("shell"), "{labels:?}");
    setup.scenario.keys(&["down", "down", "enter"]);
    setup.settle();
    assert!(
        setup
            .app()
            .assistant()
            .categories_off()
            .contains(&Category::Shell)
    );
    setup.send("no commands please");
    let allowlist = setup
        .backend
        .host()
        .turns()
        .last()
        .and_then(|(_, request)| request.tool_allowlist.clone())
        .expect("narrowed");
    assert!(!allowlist.iter().any(|tool| tool == "shell.run"));
    assert!(allowlist.iter().any(|tool| tool == "filesystem.read"));
    assert!(
        allowlist
            .iter()
            .any(|tool| tool == "code_review.add_comment")
    );
    // The chip turns it back on.
    let screen = setup.settle();
    let chip = screen.find("$ shell").expect("the shell chip");
    setup.scenario.click(chip.x, chip.y);
    setup.settle();
    setup.send("commands again");
    assert_eq!(
        setup
            .backend
            .host()
            .turns()
            .last()
            .unwrap()
            .1
            .tool_allowlist,
        None
    );
    let _ = session;
}

#[test]
fn a_category_off_in_configuration_stays_off() {
    let mut config = AssistantConfig::default();
    config.categories.insert(Category::Shell, false);
    let mut setup = setup_with(
        TestBackend::default().with_assistant_config(config),
        140,
        40,
        true,
    );
    conversation(&mut setup);
    let screen = setup.settle();
    let chip = screen.find("$ shell").expect("the shell chip shows, off");
    setup.scenario.click(chip.x, chip.y);
    setup.settle();
    assert!(
        setup
            .app()
            .assistant()
            .notice()
            .is_some_and(|notice| notice.contains("off in configuration")),
        "{:?}",
        setup.app().assistant().notice()
    );
    assert!(setup.app().assistant().categories_off().is_empty());
}

#[test]
fn refused_messages_and_failed_turns_say_why_in_the_conversation() {
    let mut setup = setup(140, 40, true);
    let session = conversation(&mut setup);
    setup
        .backend
        .host()
        .fail_next_submit("agent_profile_unavailable");
    setup.scenario.paste("again");
    setup.scenario.key("enter");
    let screen = setup.settle();
    assert!(
        setup
            .app()
            .assistant()
            .failure()
            .is_some_and(|failure| failure.contains("tool connection")),
        "{:?}",
        setup.app().assistant().failure()
    );
    assert!(
        screen.contains("agent_profile_unavailable"),
        "{}",
        screen.text()
    );

    let mut limited = snapshot(vec![user("first")]);
    limited.runtime.last_turn_outcome = Some(ModelTurnOutcome::ToolRoundLimitReached);
    let screen = setup.deliver(&session, limited);
    assert!(screen.contains("tool-round limit"), "{}", screen.text());
    assert!(screen.contains("it to continue"), "{}", screen.text());

    let mut unreachable = snapshot(vec![user("first")]);
    unreachable.runtime.last_turn_outcome = Some(ModelTurnOutcome::ProviderUnavailable);
    unreachable.runtime.last_turn_message = Some("unknown model fake-gone".to_owned());
    let screen = setup.deliver(&session, unreachable);
    assert!(
        screen.contains("unknown model fake-gone"),
        "{}",
        screen.text()
    );
    assert!(screen.contains("check the model"), "{}", screen.text());
}

#[test]
fn usage_shows_tokens_and_cost_on_the_title_row() {
    let mut setup = setup(220, 40, true);
    let session = conversation(&mut setup);
    let mut used = snapshot(vec![user("first")]);
    used.runtime.cumulative_metered_tokens = 12_345;
    used.runtime
        .cost
        .totals_micros
        .insert("USD".to_owned(), 12_300);
    let screen = setup.deliver(&session, used);
    let title = screen.row_text(0);
    assert!(title.contains("12.3k tok"), "{title}");
    assert!(title.contains("$0.0123"), "{title}");
}

#[test]
fn the_model_chip_region_is_its_painted_cells_at_every_width() {
    for width in [100u16, 40, 24, 12, 1] {
        // Small enough that the focused panel covers the whole area.
        let mut setup = setup(width.min(99), 14, true);
        setup.backend.host().set_model_catalog(Ok(ModelCatalog {
            default: Some(Ok(model("fake-wide", "模型 🚀 wide", &["low"], None))),
            providers: Vec::new(),
        }));
        setup.scenario.key("ctrl+k");
        let screen = setup.settle();
        let chip = screen.find("模型");
        if width.min(99) < crate::assistant::panel::COMPACT_WIDTH {
            assert!(chip.is_none(), "{width}: compact panels show no chips");
            // A click where a chip would be opens nothing.
            setup.scenario.click(1, 1);
            setup.settle();
            assert!(
                setup.app().assistant().overlay_labels().is_none(),
                "{width}"
            );
            continue;
        }
        let chip = chip.unwrap_or_else(|| panic!("{width}: {}", screen.text()));
        // The cell just past the panel's last column is never a chip.
        setup.scenario.click(chip.x, chip.y);
        setup.settle();
        assert!(
            setup.app().assistant().overlay_labels().is_some(),
            "{width}: the chip's cells open the picker"
        );
        setup.scenario.key("esc");
        setup.settle();
        // A cell of the chips row past every chip opens nothing.
        let row = setup.settle().row_text(chip.y);
        let used = u16::try_from(
            bmux_tui::text::Line::from_spans(vec![bmux_tui::text::Span::raw(
                row.trim_end().to_owned(),
            )])
            .width(),
        )
        .unwrap_or(0);
        if used + 1 < width.min(99) {
            setup.scenario.click(used + 1, chip.y);
            setup.settle();
            assert!(
                setup.app().assistant().overlay_labels().is_none(),
                "{width}: past the chips"
            );
        }
    }
}

#[test]
fn new_keys_bind_nothing_else_in_the_scopes_resolved_with_theirs() {
    use crate::keymap::{
        ASSISTANT_HISTORY_SCOPE, ASSISTANT_SCOPE, COMPOSER_SCOPE, FEED_SCOPE, REVIEW_SCOPE,
    };
    let registry = crate::keymap::registry(std::collections::BTreeMap::new()).expect("keymap");
    let stroke = |key: &str| bmux_keyboard::parse_key_stroke(key).expect("key");
    let cases: [(&str, &str, &str, &[&str]); 8] = [
        (
            ASSISTANT_SCOPE,
            "alt+m",
            "assistant_model",
            &[ASSISTANT_HISTORY_SCOPE],
        ),
        (
            ASSISTANT_SCOPE,
            "shift+tab",
            "assistant_effort",
            &[ASSISTANT_HISTORY_SCOPE],
        ),
        (
            ASSISTANT_SCOPE,
            "alt+p",
            "assistant_permissions",
            &[ASSISTANT_HISTORY_SCOPE],
        ),
        (
            ASSISTANT_SCOPE,
            "alt+t",
            "assistant_tools",
            &[ASSISTANT_HISTORY_SCOPE],
        ),
        (FEED_SCOPE, "alt+m", "feed_model", &[REVIEW_SCOPE]),
        (FEED_SCOPE, "shift+tab", "feed_effort", &[REVIEW_SCOPE]),
        (COMPOSER_SCOPE, "alt+m", "composer_model", &[REVIEW_SCOPE]),
        (
            COMPOSER_SCOPE,
            "shift+tab",
            "composer_effort",
            &[REVIEW_SCOPE],
        ),
    ];
    for (scope, key, action, partners) in cases {
        assert_eq!(
            registry.action_for_key(scope, stroke(key)),
            Some(format!("bcode.code_review.{action}").as_str()),
            "{scope} {key}"
        );
        for partner in partners {
            assert_eq!(
                registry.action_for_key(partner, stroke(key)),
                None,
                "{key} is free in {partner}"
            );
        }
    }
}

fn answer(text: &str) -> TranscriptViewItem {
    TranscriptViewItem {
        id: TranscriptViewItemId::new("answer"),
        revision: 1,
        sequence: Some(2),
        timestamp_ms: Some(2),
        output_location: None,
        streaming: false,
        kind: TranscriptViewItemKind::AssistantMessage {
            message: ChatMessageView::markdown(text),
        },
    }
}

/// Move the review's cursor down to a diff line.
fn to_a_line(setup: &mut Setup) {
    for _ in 0..12 {
        let on_line = setup
            .app()
            .review()
            .and_then(crate::ReviewSurface::cursor)
            .is_some_and(|key| matches!(key, ItemKey::Line { .. } | ItemKey::LinePair { .. }));
        if on_line {
            return;
        }
        setup.scenario.key("j");
        setup.settle();
    }
    panic!("no line under the cursor");
}

#[test]
fn ask_ai_cards_get_the_default_and_change_their_own_model() {
    let mut setup = setup(160, 44, true);
    with_models(&setup);
    setup.scenario.key("ctrl+k");
    setup.settle();
    setup.scenario.key("alt+m");
    choose(&mut setup, "smart");
    // Closed, so only the review shows a model.
    setup.scenario.key("ctrl+k");
    setup.settle();
    assert!(!setup.app().assistant().is_open());
    to_a_line(&mut setup);
    // The question's composer names the model it is asked with.
    setup.scenario.key("a");
    let screen = setup.settle();
    assert!(screen.contains("● Fake smart"), "{}", screen.text());
    setup.scenario.paste("why this line?");
    setup.scenario.key("enter");
    setup.settle();
    let (card, _) = setup
        .backend
        .host()
        .messages()
        .last()
        .cloned()
        .expect("asked");
    let log = setup.backend.host().model_log();
    assert!(
        log.contains(&format!("model {card} fake-smart")),
        "the card's session got the default: {log:?}"
    );
    // The card's model button opens the picker for that card's session only.
    let screen = setup.settle();
    let button = screen
        .find("● Fake smart")
        .expect("the card names its model");
    setup.scenario.click(button.x, button.y);
    setup.settle();
    assert!(setup.app().assistant().overlay_labels().is_some());
    choose(&mut setup, "wide");
    assert_eq!(
        setup.backend.host().session_choice(&card).model.as_deref(),
        Some("fake-wide")
    );
    let panel = setup.app().assistant().session().map(str::to_owned);
    assert!(panel.is_none(), "the panel's conversation was not touched");
    assert_eq!(
        setup
            .app()
            .assistant()
            .models()
            .default
            .as_ref()
            .and_then(|default| default.model_id.as_deref()),
        Some("fake-wide"),
        "and it is the app default now"
    );
}

#[test]
fn answers_copy_and_draft_a_comment_at_the_cursor_line() {
    let mut setup = setup(160, 44, true);
    to_a_line(&mut setup);
    let session = conversation(&mut setup);
    let screen = setup.deliver(
        &session,
        snapshot(vec![user("first"), answer("Use a cap of 30s.")]),
    );
    let draft = screen
        .find("Draft comment")
        .expect("the answer offers Draft comment");
    let copy = screen
        .column_of(draft.y, "Copy")
        .map(|x| bmux_tui::geometry::Point { x, y: draft.y })
        .expect("and Copy beside it");
    setup.scenario.click(copy.x, copy.y);
    setup.settle();
    assert_eq!(
        setup.backend.host().clipboard().last().map(String::as_str),
        Some("Use a cap of 30s.")
    );
    // The notice line moved the conversation; the button is found again.
    let draft = setup.settle().find("Draft comment").expect("still offered");
    setup.scenario.click(draft.x, draft.y);
    setup.settle();
    let drafts = setup
        .app()
        .review()
        .map(|review| review.notes().drafts().to_vec())
        .unwrap_or_default();
    assert!(
        drafts.iter().any(|draft| draft.body == "Use a cap of 30s."),
        "{drafts:?} {:?}",
        setup.app().assistant().notice(),
    );
}

#[test]
fn open_in_bcode_shows_only_with_a_conversation_and_the_title_row_closes_and_zooms() {
    let mut setup = setup(140, 40, true);
    with_models(&setup);
    setup.scenario.key("ctrl+k");
    let screen = setup.settle();
    assert!(!screen.row_text(0).contains('↗'), "{}", screen.row_text(0));
    setup.scenario.key("ctrl+o");
    setup.settle();
    assert_eq!(
        setup.app().assistant().notice(),
        Some("start or open a conversation first")
    );
    setup.send("first");
    let screen = setup.settle();
    assert!(screen.row_text(0).contains('↗'), "{}", screen.row_text(0));
    // A double click on the title zooms the panel over the whole area, and again restores it.
    let title = screen.find("Assistant").expect("title");
    setup.scenario.click(title.x, title.y);
    setup.scenario.click(title.x, title.y);
    setup.settle();
    assert!(setup.app().assistant().zoomed());
    let title = setup.settle().find("Assistant").expect("title");
    setup.scenario.click(title.x, title.y);
    setup.scenario.click(title.x, title.y);
    setup.settle();
    assert!(!setup.app().assistant().zoomed());
    let screen = setup.settle();
    let close = screen
        .find("✕ ")
        .filter(|at| at.y == 0)
        .expect("the title row's ✕");
    setup.scenario.click(close.x, close.y);
    setup.settle();
    assert!(!setup.app().assistant().is_open());
}

#[test]
fn history_rows_delete_with_their_cross_after_a_second_press() {
    let mut setup = setup(140, 40, true);
    let first = conversation(&mut setup);
    setup.scenario.key("ctrl+n");
    setup.settle();
    let second = setup.send("second conversation");
    for session in [&first, &second] {
        setup
            .backend
            .host()
            .add_catalog_session(crate::host::AppSession {
                id: session.clone(),
                title: Some(format!("conversation {session}")),
                origin_app: Some(crate::host::REVIEW_SESSION_APP_ID.to_owned()),
                ..crate::host::AppSession::default()
            });
    }
    setup.scenario.key("alt+h");
    let screen = setup.settle();
    let cross = (1..screen.height())
        .find_map(|y| {
            screen
                .column_of(y, " ✕")
                .map(|x| bmux_tui::geometry::Point { x, y })
        })
        .expect("the selected row's ✕");
    // Below the title row, whose own ✕ closes the panel.
    let crosses = (1..screen.height())
        .filter(|y| screen.row_text(*y).contains(" ✕"))
        .count();
    assert_eq!(crosses, 1, "only one row shows it");
    setup.scenario.click(cross.x + 1, cross.y);
    setup.settle();
    assert!(
        setup.backend.host().deleted_sessions().is_empty(),
        "armed only"
    );
    assert!(
        setup
            .app()
            .assistant()
            .hints()
            .iter()
            .any(|(_, label)| label.contains("again deletes")),
        "the footer names the armed row"
    );
    setup.scenario.click(cross.x + 1, cross.y);
    setup.settle();
    assert_eq!(setup.backend.host().deleted_sessions().len(), 1);
}

#[test]
fn delete_every_conversation_confirms_with_the_count_from_the_palette() {
    let mut setup = setup(140, 40, true);
    let first = conversation(&mut setup);
    setup.scenario.key("ctrl+n");
    setup.settle();
    let second = setup.send("second conversation");
    for session in [&first, &second] {
        setup
            .backend
            .host()
            .add_catalog_session(crate::host::AppSession {
                id: session.clone(),
                title: Some("conversation".to_owned()),
                origin_app: Some(crate::host::REVIEW_SESSION_APP_ID.to_owned()),
                ..crate::host::AppSession::default()
            });
    }
    setup.scenario.key("esc");
    setup.settle();
    setup.scenario.key(":");
    setup.settle();
    setup.scenario.paste("delete every conversation");
    setup.settle();
    setup.scenario.key("enter");
    let screen = setup.settle();
    assert!(
        screen.contains("Delete all 2 conversations"),
        "{}",
        screen.text()
    );
    setup.scenario.key("y");
    setup.settle();
    let mut deleted = setup.backend.host().deleted_sessions();
    deleted.sort();
    let mut expected = vec![first, second];
    expected.sort();
    assert_eq!(deleted, expected);
    assert!(setup.app().assistant().session().is_none());
}

#[test]
fn each_screen_shows_its_own_empty_state_and_the_palettes_list_the_assistant() {
    let mut setup = setup(140, 40, false);
    setup.scenario.key("ctrl+k");
    let screen = setup.settle();
    assert!(
        screen.contains("create or open a review"),
        "{}",
        screen.text()
    );
    setup.scenario.key("esc");
    setup.settle();
    setup.scenario.key(":");
    let screen = setup.settle();
    assert!(
        screen.contains("Assistant: Choose the assistant's model"),
        "{}",
        screen.text()
    );
    setup.scenario.key("esc");
    setup.open_from_home("Retry backoff");
    setup.scenario.key("ctrl+k");
    let screen = setup.settle();
    assert!(
        screen.contains("Ask about this review"),
        "{}",
        screen.text()
    );
}

#[test]
fn a_configured_model_bcode_does_not_offer_says_what_runs_instead() {
    let config = AssistantConfig {
        model: Some(format!("{PROVIDER}/fake-gone")),
        ..AssistantConfig::default()
    };
    let mut setup = setup_with(
        TestBackend::default().with_assistant_config(config),
        140,
        40,
        true,
    );
    let session = conversation(&mut setup);
    assert!(
        setup
            .backend
            .host()
            .model_log()
            .contains(&format!("model {session} fake-gone")),
        "the configured default was asked for"
    );
    let failure = setup.app().assistant().failure().map(str::to_owned);
    assert!(
        failure
            .as_deref()
            .is_some_and(|failure| failure.contains("does not list the model fake-gone")),
        "{failure:?}"
    );
}

#[test]
fn a_model_alias_bcode_resolves_is_not_reported_and_marks_its_model() {
    let config = AssistantConfig {
        model: Some(format!("{PROVIDER}/smart-latest")),
        ..AssistantConfig::default()
    };
    let mut setup = setup_with(
        TestBackend::default().with_assistant_config(config),
        140,
        40,
        true,
    );
    setup
        .backend
        .host()
        .alias_model("smart-latest", "fake-smart");
    conversation(&mut setup);
    assert_eq!(setup.app().assistant().failure(), None);
    setup.scenario.key("alt+m");
    setup.settle();
    let labels = setup.app().assistant().overlay_labels().expect("opened");
    assert!(
        labels.iter().any(|label| label.starts_with("● Fake smart")),
        "{labels:?}"
    );
}

/// Ask a question at a diff line with the panel closed; returns the card's session.
fn ask_at_a_line(setup: &mut Setup, question: &str) -> String {
    to_a_line(setup);
    setup.scenario.key("a");
    setup.settle();
    setup.scenario.paste(question);
    setup.scenario.key("enter");
    setup.settle();
    setup
        .backend
        .host()
        .messages()
        .last()
        .cloned()
        .expect("asked")
        .0
}

/// Choose `filter`'s model in the panel with no conversation: only the app default changes.
fn choose_default(setup: &mut Setup, filter: &str) {
    setup.scenario.key("ctrl+k");
    setup.settle();
    setup.scenario.key("alt+m");
    choose(setup, filter);
    setup.scenario.key("ctrl+k");
    setup.settle();
}

#[test]
fn a_follow_up_question_shows_and_changes_its_cards_model() {
    let mut setup = setup(160, 44, true);
    with_models(&setup);
    choose_default(&mut setup, "smart");
    let card = ask_at_a_line(&mut setup, "why this line?");
    // The app default moves on; the card keeps its own model.
    choose_default(&mut setup, "wide");
    // A question at the same line is a follow-up in the card's session, and says its model.
    setup.scenario.key("a");
    let screen = setup.settle();
    assert!(
        !screen.contains("模型 🚀 wide"),
        "the follow-up does not name the app default: {}",
        screen.text()
    );
    assert!(screen.contains("● Fake smart"), "{}", screen.text());
    setup.scenario.paste("and the next one?");
    // The composer's model key changes the card's session, keeping the question.
    setup.scenario.key("alt+m");
    choose(&mut setup, "echo");
    assert_eq!(
        setup.backend.host().session_choice(&card).model.as_deref(),
        Some("fake-echo")
    );
    setup.scenario.key("enter");
    setup.settle();
    let (session, text) = setup
        .backend
        .host()
        .messages()
        .last()
        .cloned()
        .expect("followed up");
    assert_eq!(session, card);
    assert!(text.contains("and the next one?"), "{text}");
    assert_eq!(
        last_turn_line(&setup, &card),
        format!("submit {card} fake-echo default"),
        "the follow-up runs on the model chosen for it"
    );
}

#[test]
fn an_effort_chosen_on_a_card_is_saved_with_the_cards_model() {
    let mut setup = setup(160, 44, true);
    with_models(&setup);
    choose_default(&mut setup, "smart");
    let card = ask_at_a_line(&mut setup, "why this line?");
    // The card moves to the wide model (one effort value), then the default back to smart.
    let screen = setup.settle();
    let button = screen.find("● Fake smart").expect("the card's model");
    setup.scenario.click(button.x, button.y);
    choose(&mut setup, "wide");
    choose_default(&mut setup, "smart");
    // The card's effort key, with the cursor on the card.
    let screen = setup.settle();
    let button = screen.find("● 模型 🚀 wide").expect("the card's model");
    setup.scenario.click(button.x, button.y);
    setup.scenario.key("esc");
    setup.settle();
    setup.scenario.key("shift+tab");
    setup.settle();
    assert_eq!(
        setup.backend.host().session_choice(&card).effort.as_deref(),
        Some("low")
    );
    let default = setup
        .app()
        .assistant()
        .models()
        .default
        .clone()
        .expect("the default");
    assert_eq!(
        (default.model_id.as_deref(), default.effort.as_deref()),
        (Some("fake-wide"), Some("low")),
        "the effort is saved with the model it was chosen for"
    );
}

#[test]
fn a_card_whose_default_bcode_refuses_says_why() {
    let mut setup = setup(160, 44, true);
    with_models(&setup);
    choose_default(&mut setup, "smart");
    setup
        .backend
        .host()
        .refuse_model("fake-smart", "not allowed for this account");
    ask_at_a_line(&mut setup, "why this line?");
    let screen = setup.settle();
    assert!(
        screen.contains("not allowed for this account"),
        "{}",
        screen.text()
    );
}

#[test]
fn choosing_bcodes_default_when_bcode_cannot_name_it_changes_nothing() {
    let mut setup = setup(140, 40, true);
    let mut models = catalog();
    models.default = Some(Err("no default model configured".to_owned()));
    setup.backend.host().set_model_catalog(Ok(models));
    setup.scenario.key("ctrl+k");
    setup.settle();
    let session = setup.send("first");
    setup.scenario.key("alt+m");
    choose(&mut setup, "smart");
    setup.scenario.key("alt+m");
    setup.settle();
    setup.scenario.paste("Bcode's default");
    setup.settle();
    assert!(
        setup
            .app()
            .assistant()
            .overlay_labels()
            .is_some_and(|labels| labels.iter().any(|label| label.contains("Bcode's default"))),
    );
    setup.scenario.key("enter");
    setup.settle();
    let failure = setup.app().assistant().failure().map(str::to_owned);
    assert!(
        failure
            .as_deref()
            .is_some_and(|failure| failure.contains("no default model configured")
                && failure.contains("keeps Fake smart")),
        "{failure:?}"
    );
    assert_eq!(
        setup
            .backend
            .host()
            .session_choice(&session)
            .model
            .as_deref(),
        Some("fake-smart")
    );
    assert_eq!(
        setup
            .app()
            .assistant()
            .models()
            .default
            .as_ref()
            .and_then(|default| default.model_id.as_deref()),
        Some("fake-smart"),
        "the app default is kept"
    );
}

#[test]
fn session_models_that_cannot_be_read_are_read_once_and_say_so() {
    let mut setup = setup(160, 44, true);
    with_models(&setup);
    setup
        .backend
        .host()
        .fail_session_models(Some("session not found"));
    setup.scenario.key("ctrl+k");
    setup.settle();
    let conversation = setup.send("first");
    assert!(setup.settle().contains("⚠ model unknown"));
    setup.scenario.key("alt+m");
    setup.settle();
    let labels = setup.app().assistant().overlay_labels().expect("opened");
    assert!(
        labels[0].contains("could not read") && labels[0].contains("session not found"),
        "{labels:?}"
    );
    setup.scenario.key("esc");
    setup.settle();
    setup.scenario.key("esc");
    setup.settle();
    let card = ask_at_a_line(&mut setup, "why this line?");
    for _ in 0..5 {
        setup.settle();
    }
    let host = setup.backend.host();
    assert_eq!(host.session_model_reads(&conversation), 1);
    assert_eq!(host.session_model_reads(&card), 1);
    let screen = setup.settle();
    assert!(screen.contains("⚠ model unknown"), "{}", screen.text());
}

#[test]
fn switching_conversations_keeps_each_ones_model_and_categories() {
    let mut setup = setup(140, 40, true);
    let first = conversation(&mut setup);
    setup.scenario.key("alt+m");
    choose(&mut setup, "smart");
    setup.scenario.key("alt+p");
    setup.settle();
    setup.scenario.keys(&["down", "down", "enter"]);
    setup.settle();
    assert!(
        setup
            .app()
            .assistant()
            .categories_off()
            .contains(&Category::Shell)
    );
    setup.scenario.key("ctrl+n");
    setup.settle();
    let second = setup.send("second conversation");
    setup.scenario.key("alt+m");
    choose(&mut setup, "wide");
    assert!(setup.app().assistant().categories_off().is_empty());
    for (session, title) in [(&first, "alpha talk"), (&second, "beta talk")] {
        setup
            .backend
            .host()
            .add_catalog_session(crate::host::AppSession {
                id: session.clone(),
                title: Some(title.to_owned()),
                origin_app: Some(crate::host::REVIEW_SESSION_APP_ID.to_owned()),
                ..crate::host::AppSession::default()
            });
    }
    let open = |setup: &mut Setup, title: &str| {
        setup.scenario.key("alt+h");
        setup.settle();
        setup.scenario.paste(title);
        setup.scenario.key("enter");
        setup.settle()
    };
    let screen = open(&mut setup, "alpha");
    assert_eq!(setup.session().as_deref(), Some(first.as_str()));
    assert!(screen.contains("● Fake smart"), "{}", screen.text());
    assert!(
        setup
            .app()
            .assistant()
            .categories_off()
            .contains(&Category::Shell),
        "shell stays off for the conversation it was turned off in"
    );
    setup.send("still no commands");
    let allowlist = setup
        .backend
        .host()
        .turns()
        .last()
        .and_then(|(_, request)| request.tool_allowlist.clone())
        .expect("narrowed");
    assert!(!allowlist.iter().any(|tool| tool == "shell.run"));
    let screen = open(&mut setup, "beta");
    assert_eq!(setup.session().as_deref(), Some(second.as_str()));
    assert!(screen.contains("● 模型 🚀 wide"), "{}", screen.text());
    assert!(setup.app().assistant().categories_off().is_empty());
}

#[test]
fn delete_every_conversation_pages_bcodes_catalog_and_says_what_remains() {
    let mut setup = setup(140, 40, true);
    with_models(&setup);
    let total = crate::assistant::picker::MAX_DELETE_ALL + 3;
    for index in 0..total {
        setup
            .backend
            .host()
            .add_catalog_session(crate::host::AppSession {
                id: format!("s-{index:04}"),
                title: Some(format!("conversation {index}")),
                origin_app: Some(crate::host::REVIEW_SESSION_APP_ID.to_owned()),
                updated_at_ms: u64::try_from(index).unwrap_or(0),
                ..crate::host::AppSession::default()
            });
    }
    setup
        .backend
        .host()
        .refuse_delete("s-1002", "it is running");
    setup.scenario.key(":");
    setup.settle();
    setup.scenario.paste("delete every conversation");
    setup.settle();
    setup.scenario.key("enter");
    let screen = setup.settle();
    assert!(
        screen.contains("Delete the newest 1000 conversations"),
        "{}",
        screen.text()
    );
    setup.scenario.key("y");
    setup.settle();
    assert_eq!(
        setup.backend.host().deleted_sessions().len(),
        crate::assistant::picker::MAX_DELETE_ALL - 1
    );
    let notice = setup.app().assistant().notice().map(str::to_owned);
    assert!(
        notice
            .as_deref()
            .is_some_and(|notice| notice.contains("deleted 999 conversations")
                && notice.contains("Bcode kept 1 (it is running)")
                && notice.contains("more remain")),
        "{notice:?}"
    );
}

#[test]
fn the_history_cross_returns_to_the_selected_row_when_the_pointer_leaves() {
    let mut setup = setup(140, 40, true);
    let first = conversation(&mut setup);
    setup.scenario.key("ctrl+n");
    setup.settle();
    let second = setup.send("second conversation");
    for session in [&first, &second] {
        setup
            .backend
            .host()
            .add_catalog_session(crate::host::AppSession {
                id: session.clone(),
                title: Some(format!("conversation {session}")),
                origin_app: Some(crate::host::REVIEW_SESSION_APP_ID.to_owned()),
                ..crate::host::AppSession::default()
            });
    }
    setup.scenario.key("alt+h");
    let screen = setup.settle();
    let cross_row = |screen: &crate::testing::harness::Screen| {
        (1..screen.height())
            .filter(|y| screen.row_text(*y).contains(" ✕"))
            .collect::<Vec<_>>()
    };
    let selected = cross_row(&screen);
    assert_eq!(selected.len(), 1);
    let selected = selected[0];
    let other = [selected + 1, selected.saturating_sub(1)]
        .into_iter()
        .find(|y| screen.row_text(*y).contains("conversation"))
        .expect("the other row");
    let x = screen.column_of(other, "conversation").expect("its title");
    setup.scenario.mouse(MouseEventKind::Move, x, other);
    assert_eq!(
        cross_row(&setup.settle()),
        vec![other],
        "follows the pointer"
    );
    setup.scenario.mouse(MouseEventKind::Move, 1, 1);
    assert_eq!(
        cross_row(&setup.settle()),
        vec![selected],
        "back on the selected row"
    );
}

#[test]
fn picker_rows_effort_and_category_chips_act_on_their_own_cells() {
    let mut setup = setup(140, 40, true);
    let session = conversation(&mut setup);
    setup.scenario.key("alt+m");
    let screen = setup.settle();
    // A provider's header row chooses nothing; a model's row chooses it.
    let header = screen.find("Broken provider").expect("a header row");
    setup.scenario.click(header.x, header.y);
    setup.settle();
    assert!(setup.app().assistant().overlay_labels().is_some());
    let row = setup.settle().find("Fake smart fake-smart").expect("a row");
    setup.scenario.click(row.x, row.y);
    setup.settle();
    assert!(setup.app().assistant().overlay_labels().is_none());
    assert_eq!(
        setup
            .backend
            .host()
            .session_choice(&session)
            .model
            .as_deref(),
        Some("fake-smart")
    );
    for width in [140u16, 60, 40] {
        let mut setup = super::assistant::setup(width, 30, true);
        let session = conversation(&mut setup);
        setup.scenario.key("alt+m");
        choose(&mut setup, "smart");
        let screen = setup.settle();
        assert!(
            width < 140 || screen.contains("◆"),
            "{width}: room for every chip"
        );
        if let Some(effort) = screen.find("◆") {
            setup.scenario.click(effort.x, effort.y);
            setup.settle();
            assert_eq!(
                setup
                    .backend
                    .host()
                    .session_choice(&session)
                    .effort
                    .as_deref(),
                Some("high"),
                "{width}: the effort chip cycles"
            );
        }
        let screen = setup.settle();
        if let Some(shell) = screen.find("$") {
            setup.scenario.click(shell.x, shell.y);
            setup.settle();
            assert!(
                setup
                    .app()
                    .assistant()
                    .categories_off()
                    .contains(&Category::Shell),
                "{width}: the shell chip toggles"
            );
        }
    }
}

#[test]
fn the_question_composers_model_button_opens_the_picker_only_where_painted() {
    for width in [160u16, 80, 60, 40] {
        let backend = TestBackend::default();
        backend.host().set_model_catalog(Ok(catalog()));
        let mut setup = setup_with(backend, width, 44, true);
        to_a_line(&mut setup);
        setup.scenario.key("a");
        let screen = setup.settle();
        let row = screen.find("Cancel").map(|at| at.y);
        let button =
            row.and_then(|y| Some((screen.column_of(y, "● ")?, screen.column_of(y, "▾")?, y)));
        let Some((start, arrow, y)) = button else {
            assert!(
                width < 160,
                "the widest row shows the button: {}",
                screen.text()
            );
            continue;
        };
        assert!(
            screen.row_text(y).contains("Bcode's model"),
            "{}",
            screen.row_text(y)
        );
        assert!(arrow < width, "{width}: within the row");
        for x in [start, arrow] {
            setup.scenario.click(x, y);
            setup.settle();
            assert!(
                setup.app().assistant().overlay_labels().is_some(),
                "{width}: the button's cells open the picker"
            );
            setup.scenario.key("esc");
            setup.settle();
        }
        if arrow + 3 < width {
            setup.scenario.click(arrow + 3, y);
            setup.settle();
            assert!(
                setup.app().assistant().overlay_labels().is_none(),
                "{width}: past the button"
            );
        }
    }
}

#[test]
fn a_long_answer_shows_in_full_in_the_panel_and_scrolls_to_its_top() {
    let mut setup = setup(160, 44, true);
    to_a_line(&mut setup);
    let session = conversation(&mut setup);
    let text = super::feed::long_answer("panel", 60);
    let screen = setup.deliver(&session, snapshot(vec![user("first"), answer(&text)]));
    // The newest rows show: the answer's end and its buttons, which act where painted.
    assert!(
        screen.contains("let panel_end = true;"),
        "{}",
        screen.text()
    );
    let draft = screen
        .find("Draft comment")
        .expect("the answer offers Draft comment");
    setup.scenario.click(draft.x, draft.y);
    setup.settle();
    let drafts = setup
        .app()
        .review()
        .map(|review| review.notes().drafts().to_vec())
        .unwrap_or_default();
    assert!(
        drafts.iter().any(|draft| draft.body.trim() == text.trim()),
        "{drafts:?}"
    );
    // Scrolling up shows every earlier row of the answer, none elided.
    let mut seen = String::new();
    for _ in 0..10 {
        seen.push_str(&setup.settle().text());
        setup.scenario.key("pageup");
    }
    seen.push_str(&setup.settle().text());
    assert!(seen.contains("panel plan"), "{seen}");
    for step in 1..=60 {
        assert!(
            seen.contains(&format!("panel step {step} done")),
            "step {step}"
        );
    }
}

#[test]
fn an_answer_cut_at_its_byte_bound_says_so() {
    let mut setup = setup(160, 44, true);
    let session = conversation(&mut setup);
    let huge = "word ".repeat(20_000);
    let screen = setup.deliver(&session, snapshot(vec![user("first"), answer(&huge)]));
    assert!(
        screen.contains("answer too long; the rest is in Bcode"),
        "{}",
        screen.text()
    );
}

#[test]
fn a_long_message_shows_in_full_in_the_panel_wrapped_with_its_paragraphs() {
    use std::fmt::Write as _;
    let mut setup = setup(160, 44, true);
    let session = conversation(&mut setup);
    let mut message = String::new();
    for line in 1..=12 {
        let _ = writeln!(message, "message line {line}");
    }
    message.push('\n');
    let long: Vec<String> = (1..=40).map(|word| format!("word{word}")).collect();
    message.push_str(&long.join(" "));
    let screen = setup.deliver(&session, snapshot(vec![user(&message), answer("ok")]));
    let mut seen = screen.text();
    for _ in 0..6 {
        setup.scenario.key("pageup");
        seen.push_str(&setup.settle().text());
    }
    for line in 1..=12 {
        assert!(
            seen.contains(&format!("message line {line}")),
            "line {line}: {seen}"
        );
    }
    // The long line wraps under the bar instead of being cut at the panel's edge.
    assert!(seen.contains("word1 "), "{seen}");
    assert!(seen.contains("word40"), "{seen}");
}
