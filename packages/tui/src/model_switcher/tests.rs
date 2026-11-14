//! Model switcher behavior: grouping, search, choosing, and drawing.

use super::inventory::{
    Access, CurrentChoice, Entry, Fix, Readiness, Row, SectionKind, Source, SourceKey, SourceState,
    SourceUpdate,
};
use super::view::SwitcherTheme;
use super::{ModelSwitcher, Outcome};
use bcode_config::model_choices::ModelChoices;
use bcode_model::execution::{
    ExecutionBillingRoute, ExecutionModel, ExecutionModelAccess, ExecutionModelProvenance,
    ExecutionModelReadiness, ExecutionRoute,
};
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_tui::buffer::Buffer;
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::paint::PaintCx;

fn model(id: &str, name: &str, route: ExecutionRoute, reasoning: bool) -> ExecutionModel {
    ExecutionModel {
        model: bcode_model::ModelInfo {
            model_id: id.into(),
            display_name: name.into(),
            is_default: false,
            context_window: Some(200_000),
            max_output_tokens: Some(64_000),
            max_image_input_base64_bytes: None,
            capabilities: [bcode_model::ModelCapability::ToolCalls].into(),
            feature_support: bcode_model::ModelFeatureSupport::default(),
            reasoning: reasoning.then(|| bcode_model::ModelReasoningInfo {
                effort_values: vec!["low".into(), "medium".into(), "high".into()],
                default_effort: Some("medium".into()),
                ..bcode_model::ModelReasoningInfo::default()
            }),
            cache: bcode_model::ModelCacheInfo::default(),
            metadata_source: None,
            pricing: None,
            api_surface: None,
            visibility: bcode_model::ModelVisibility::Visible,
        },
        native_model_id: id.into(),
        route,
        account_reference: None,
        billing_route: ExecutionBillingRoute::NativeSubscription,
        confirmed_model_id: None,
        provenance: ExecutionModelProvenance::HarnessDiscovery,
        readiness: ExecutionModelReadiness::RouteReady {
            model_access: ExecutionModelAccess::Unknown,
        },
    }
}

fn native_route() -> ExecutionRoute {
    ExecutionRoute::ManagedHarness {
        instance_id: "claude".into(),
        plugin_id: "bcode.claude-code".into(),
    }
}

fn provider_route() -> ExecutionRoute {
    ExecutionRoute::ModelProvider {
        provider_plugin_id: "openai".into(),
    }
}

fn source(key: SourceKey, title: &str) -> Source {
    Source {
        key,
        title: title.into(),
        detail: String::new(),
        state: SourceState::Ready,
    }
}

fn native_key() -> SourceKey {
    SourceKey::Native {
        instance_id: "claude".into(),
    }
}

fn provider_key() -> SourceKey {
    SourceKey::Provider {
        plugin_id: "openai".into(),
    }
}

fn native_update() -> SourceUpdate {
    let key = native_key();
    SourceUpdate {
        source: source(key.clone(), "Claude Code"),
        entries: vec![
            Entry::new(
                key.clone(),
                model("opus", "Opus 5.5", native_route(), true),
                Access::Direct,
                Readiness::Available,
            ),
            Entry::new(
                key,
                model("haiku", "Haiku 5.5", native_route(), false),
                Access::Direct,
                Readiness::Available,
            ),
        ],
    }
}

fn provider_update() -> SourceUpdate {
    let key = provider_key();
    SourceUpdate {
        source: source(key.clone(), "OpenAI"),
        entries: vec![
            Entry::new(
                key.clone(),
                model("gpt-5.5", "GPT-5.5", provider_route(), true),
                Access::Direct,
                Readiness::Confirmed,
            ),
            Entry::new(
                key,
                model("gpt-old", "GPT Old", provider_route(), false),
                Access::Direct,
                Readiness::Unavailable("Not available on this account".into()),
            ),
        ],
    }
}

fn connect_update() -> SourceUpdate {
    let candidate = bcode_provider_auth_models::external::ExternalAuthCandidate {
        binding_id: "binding".into(),
        target: bcode_provider_auth_models::external::ExternalAuthTarget {
            plugin_id: "openai".into(),
            provider_id: "openai".into(),
            method_id: "api-key".into(),
            source_id: "pi".into(),
        },
        application: "Pi".into(),
        available: true,
    };
    SourceUpdate {
        source: source(SourceKey::Connect, "Available to connect"),
        entries: vec![Entry::new(
            SourceKey::Connect,
            model("Pi", "Pi", provider_route(), false),
            Access::Connect(Box::new(candidate)),
            Readiness::Action {
                fix: Fix::Connect,
                reason: "Pi is signed in.".into(),
            },
        )],
    }
}

fn keymap() -> crate::keymap::BmuxKeyMap {
    crate::keymap::BmuxKeyMap::from_config(&bcode_config::BcodeConfig::default().tui)
}

fn switcher(current: Option<&str>, choices: ModelChoices) -> ModelSwitcher {
    let mut switcher = ModelSwitcher::new(
        CurrentChoice {
            route: current.map(|_| native_route()),
            model_id: current.map(ToOwned::to_owned),
            account_reference: None,
        },
        choices,
        keymap(),
    );
    // Sources report in any order; the listing order is fixed.
    switcher.apply(connect_update());
    switcher.apply(provider_update());
    switcher.apply(native_update());
    switcher
}

const fn key(code: KeyCode) -> KeyStroke {
    KeyStroke::simple(code)
}

const fn ctrl(character: char) -> KeyStroke {
    KeyStroke {
        key: KeyCode::Char(character),
        modifiers: Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        },
    }
}

fn type_text(switcher: &mut ModelSwitcher, text: &str) {
    for character in text.chars() {
        switcher.key(key(KeyCode::Char(character)));
    }
}

fn headings(switcher: &ModelSwitcher) -> Vec<String> {
    switcher
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Heading { title, .. } => Some(title.clone()),
            Row::Choice { .. } => None,
        })
        .collect()
}

fn opus_key() -> String {
    super::inventory::choice_key(&native_route(), "opus", &Access::Direct)
}

#[test]
fn sections_list_current_favorites_and_recent_before_sources_in_fixed_order() {
    let mut choices = ModelChoices::default();
    choices.toggle_favorite(&super::inventory::choice_key(
        &provider_route(),
        "gpt-5.5",
        &Access::Direct,
    ));
    choices.record_recent(&super::inventory::choice_key(
        &native_route(),
        "haiku",
        &Access::Direct,
    ));
    let switcher = switcher(Some("opus"), choices);
    assert_eq!(
        headings(&switcher),
        [
            "Current",
            "Favorites",
            "Recent",
            "Claude Code",
            "OpenAI",
            "Available to connect"
        ]
    );
    assert_eq!(switcher.active_entry().unwrap().key, opus_key());
}

#[test]
fn search_ranks_matches_and_drops_personal_sections() {
    let mut choices = ModelChoices::default();
    choices.record_recent(&opus_key());
    let mut switcher = switcher(Some("opus"), choices);
    type_text(&mut switcher, "gpt");
    assert_eq!(headings(&switcher), ["OpenAI"]);
    assert!(switcher.rows.iter().all(|row| !matches!(
        row,
        Row::Heading {
            kind: SectionKind::Recent,
            ..
        }
    )));
    assert_eq!(switcher.active_entry().unwrap().title(), "GPT-5.5");
    // A source title matches too.
    switcher.key(key(KeyCode::Escape));
    type_text(&mut switcher, "claude");
    assert_eq!(headings(&switcher), ["Claude Code"]);
    // Escape clears the search first, then closes.
    assert_eq!(switcher.key(key(KeyCode::Escape)), Outcome::Redraw);
    assert_eq!(switcher.key(key(KeyCode::Escape)), Outcome::Close);
}

#[test]
fn selection_survives_sources_reporting_later() {
    let mut switcher =
        ModelSwitcher::new(CurrentChoice::default(), ModelChoices::default(), keymap());
    switcher.apply(native_update());
    switcher.key(key(KeyCode::Down));
    assert_eq!(switcher.active_entry().unwrap().title(), "Haiku 5.5");
    switcher.apply(provider_update());
    switcher.apply(native_update());
    assert_eq!(switcher.active_entry().unwrap().title(), "Haiku 5.5");
}

#[test]
fn choosing_uses_ready_models_with_effort_and_starts_fixes_for_others() {
    let mut switcher = switcher(None, ModelChoices::default());
    assert_eq!(switcher.active_entry().unwrap().title(), "Opus 5.5");
    // Native tools choose their own effort.
    assert_eq!(switcher.key(key(KeyCode::Right)), Outcome::Ignored);
    let Outcome::Choose(choice) = switcher.key(key(KeyCode::Enter)) else {
        panic!("ready rows are chosen");
    };
    assert_eq!(choice.entry.key, opus_key());
    assert_eq!(choice.effort, None);

    switcher.key(key(KeyCode::Down));
    switcher.key(key(KeyCode::Down));
    assert_eq!(switcher.active_entry().unwrap().title(), "GPT-5.5");
    switcher.key(key(KeyCode::Right));
    let Outcome::Choose(choice) = switcher.key(key(KeyCode::Enter)) else {
        panic!("ready rows are chosen");
    };
    assert_eq!(choice.effort.as_deref(), Some("high"));

    switcher.key(key(KeyCode::End));
    assert!(matches!(
        switcher.key(key(KeyCode::Enter)),
        Outcome::Connect(candidate) if candidate.application == "Pi"
    ));

    switcher.key(key(KeyCode::Up));
    assert_eq!(switcher.active_entry().unwrap().title(), "GPT Old");
    assert_eq!(switcher.key(key(KeyCode::Enter)), Outcome::Redraw);
    assert!(switcher.notice.as_ref().is_some_and(|notice| notice.error));
}

#[test]
fn favorites_toggle_and_ask_to_be_saved() {
    let mut switcher = switcher(None, ModelChoices::default());
    let Outcome::SaveChoices(saved) = switcher.key(ctrl('f')) else {
        panic!("favorites are saved");
    };
    assert!(saved.favorites.contains(&opus_key()));
    assert_eq!(headings(&switcher)[0], "Favorites");
    // The selection stays on the same row rather than jumping to the new section.
    assert_eq!(switcher.active_entry().unwrap().key, opus_key());
}

fn paint(switcher: &mut ModelSwitcher, width: u16, height: u16) -> (Vec<String>, SwitcherTheme) {
    let theme = SwitcherTheme::from_presented(&crate::render::semantic_state_theme());
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    {
        let mut frame = Frame::new(&mut buffer);
        switcher.paint(&theme, area, &mut PaintCx::new(&mut frame));
    }
    let rows = (0..height)
        .filter_map(|row| buffer.row_symbols(row))
        .collect();
    (rows, theme)
}

fn row_of(rows: &[String], text: &str) -> u16 {
    u16::try_from(
        rows.iter()
            .position(|row| row.contains(text))
            .unwrap_or_else(|| panic!("{text} painted: {rows:#?}")),
    )
    .unwrap()
}

fn click(x: u16, y: u16) -> Event {
    Event::Mouse(MouseEvent::new(
        MouseEventKind::Down(MouseButton::Left),
        Point::new(x, y),
    ))
}

#[test]
fn wide_layout_shows_details_and_mouse_selects_then_uses() {
    let mut switcher = switcher(Some("opus"), ModelChoices::default());
    let (rows, theme) = paint(&mut switcher, 140, 40);
    let text = rows.join("\n");
    assert!(text.contains("Switch model"), "{text}");
    assert!(text.contains("CLAUDE CODE"), "{text}");
    assert!(text.contains("Runs in"), "{text}");

    let haiku = row_of(&rows, "Haiku 5.5");
    let column = u16::try_from(rows[usize::from(haiku)].find("Haiku").unwrap()).unwrap();
    assert_eq!(
        switcher.mouse(&click(column, haiku), &theme),
        Outcome::Redraw
    );
    assert_eq!(switcher.active_entry().unwrap().title(), "Haiku 5.5");
    paint(&mut switcher, 140, 40);
    assert!(matches!(
        switcher.mouse(&click(column, haiku), &theme),
        Outcome::Choose(choice) if choice.entry.model.model.model_id == "haiku"
    ));
}

#[test]
fn clicking_an_effort_chip_sets_the_effort() {
    let mut switcher = switcher(None, ModelChoices::default());
    switcher.key(key(KeyCode::Down));
    switcher.key(key(KeyCode::Down));
    let (rows, theme) = paint(&mut switcher, 140, 40);
    assert!(rows.join("\n").contains("Effort"));
    let line = row_of(&rows, " low ");
    let column = rows[usize::from(line)].find(" high ").unwrap();
    let column = u16::try_from(rows[usize::from(line)][..column].chars().count() + 2).unwrap();
    assert_eq!(
        switcher.mouse(&click(column, line), &theme),
        Outcome::Redraw
    );
    let Outcome::Choose(choice) = switcher.key(key(KeyCode::Enter)) else {
        panic!("chosen");
    };
    assert_eq!(choice.effort.as_deref(), Some("high"));
}

#[test]
fn clicking_outside_closes_and_narrow_layouts_stack_a_summary() {
    let mut switcher = switcher(None, ModelChoices::default());
    let (_, theme) = paint(&mut switcher, 140, 40);
    assert_eq!(switcher.mouse(&click(0, 0), &theme), Outcome::Close);

    let (rows, _) = paint(&mut switcher, 70, 24);
    let text = rows.join("\n");
    assert!(!text.contains("Runs in"), "{text}");
    assert!(text.contains("Opus 5.5"), "{text}");
}

#[test]
fn tiny_and_degenerate_areas_paint_without_panicking() {
    let mut switcher = switcher(Some("opus"), ModelChoices::default());
    for (width, height) in [(0, 0), (1, 1), (10, 3), (24, 8), (40, 12), (300, 100)] {
        paint(&mut switcher, width, height);
    }
    switcher.key(key(KeyCode::Tab));
    for (width, height) in [(1, 1), (24, 8), (140, 40)] {
        paint(&mut switcher, width, height);
    }
}

#[test]
fn compare_lists_each_model_once_and_sorts_by_clicked_heading() {
    let mut choices = ModelChoices::default();
    choices.toggle_favorite(&opus_key());
    let mut switcher = switcher(Some("opus"), choices);
    switcher.key(key(KeyCode::Tab));
    let (rows, theme) = paint(&mut switcher, 140, 40);
    let text = rows.join("\n");
    assert_eq!(text.matches("Opus 5.5").count(), 1, "{text}");
    assert!(
        !text.contains("Pi"),
        "connect offers are not models: {text}"
    );
    let header = row_of(&rows, "Model");
    let column = u16::try_from(rows[usize::from(header)].find("Model").unwrap()).unwrap();
    assert_eq!(
        switcher.mouse(&click(column, header), &theme),
        Outcome::Redraw
    );
    let (rows, _) = paint(&mut switcher, 140, 40);
    let first = row_of(&rows, "GPT-5.5");
    assert!(first < row_of(&rows, "Opus 5.5"), "{rows:#?}");
}

#[test]
fn accounts_sharing_an_adapter_keep_distinct_sources_and_recent_keys() {
    let listing = |account: &str| {
        super::discovery::account_update(bcode_model::ProviderAccountModels {
            account_reference: account.into(),
            provider_plugin_id: "bcode.openai-compatible".into(),
            display_name: format!("Provider {account}"),
            models: bcode_model::ModelList {
                models: vec![model("same-model", "Same model", provider_route(), false).model],
                catalog: bcode_model::ModelCatalogHints::default(),
            },
        })
    };
    let first = listing("first");
    let second = listing("second");
    assert_ne!(first.source.key, second.source.key);
    assert_ne!(first.entries[0].key, second.entries[0].key);
    assert_eq!(
        first.entries[0].model.account_reference.as_deref(),
        Some("first")
    );
    assert!(
        matches!(&second.entries[0].access, Access::Account { account_reference } if account_reference == "second")
    );
    assert_eq!(
        first.entries[0].model.readiness,
        ExecutionModelReadiness::Unknown
    );
    let mut inventory = super::inventory::Inventory::default();
    let selected_key = second.entries[0].key.clone();
    inventory.apply(first);
    inventory.apply(second);
    let current = CurrentChoice {
        route: Some(ExecutionRoute::ModelProvider {
            provider_plugin_id: "bcode.openai-compatible".into(),
        }),
        model_id: Some("same-model".into()),
        account_reference: Some("second".into()),
    };
    let rows = inventory.rows("", &current, &std::collections::BTreeSet::new(), &[]);
    let marked: Vec<_> = rows
        .iter()
        .filter_map(|row| match row {
            Row::Choice {
                entry,
                current: true,
                ..
            } => Some(inventory.entries()[*entry].key.clone()),
            _ => None,
        })
        .collect();
    assert_ne!(marked, Vec::<String>::new());
    assert!(marked.iter().all(|key| key == &selected_key));
}
