//! Live-apply, cancel, and persistence behavior of the appearance picker.

use bmux_keyboard::{KeyCode, KeyStroke};

use super::*;
use crate::appearance_picker::{AppearanceAxis, AppearancePickerOutcome};

fn test_chat() -> ActiveChat {
    let session_id = bcode_session_models::SessionId::new();
    let (event_sender, event_receiver) = super::super::history_flow::session_stream_channel();
    ActiveChat {
        app: super::super::app::BmuxApp::new_with_history(Some(session_id), &[], &[]),
        agents: super::super::session_flow::AgentCatalog::default(),
        attachment: super::super::session_flow::ChatSessionAttachment::Attached { session_id },
        event_sender,
        event_receiver,
        event_task: None,
        opening_session_progress: None,
        pending_effects: super::super::effects::TuiEffectQueue::default(),
    }
}

fn test_loop_state() -> ChatLoopState {
    let client = bcode_client::BcodeClient::default_endpoint();
    let passive = client
        .clone()
        .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
    ChatLoopState::new(&client, &passive, false)
}

fn keymap() -> super::super::keymap::BmuxKeyMap {
    super::super::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default())
}

fn key(loop_state: &mut ChatLoopState, chat: &mut ActiveChat, code: KeyCode) {
    assert!(loop_state.handle_appearance_picker_key(chat, KeyStroke::simple(code), &keymap()));
}

/// Select `id` on `axis` the way a click does, applying the outcome live.
fn choose(loop_state: &mut ChatLoopState, chat: &mut ActiveChat, axis: AppearanceAxis, id: &str) {
    let picker = loop_state.appearance_picker.as_mut().expect("picker open");
    let row = match axis {
        AppearanceAxis::Palette => picker
            .palettes()
            .entries()
            .iter()
            .position(|entry| entry.id == id),
        AppearanceAxis::Profile => picker
            .profiles()
            .entries()
            .iter()
            .position(|entry| entry.id.as_deref() == Some(id)),
    }
    .unwrap_or_else(|| panic!("{id} is listed"));
    let outcome = picker.select_row(axis, row);
    assert!(
        matches!(
            outcome,
            AppearancePickerOutcome::PreviewPalette(_) | AppearancePickerOutcome::PreviewProfile(_)
        ),
        "{outcome:?}"
    );
    loop_state.apply_appearance_picker_outcome(chat, outcome);
}

fn queued_persistence(chat: &mut ActiveChat) -> (Vec<String>, Vec<Option<String>>) {
    let (effects, _) = chat.pending_effects.drain_runtime();
    let mut themes = Vec::new();
    let mut profiles = Vec::new();
    for (_, effect) in effects {
        match effect {
            TuiEffect::PersistThemeSelection { name } => themes.push(name),
            TuiEffect::PersistProfileSelection { name } => profiles.push(name),
            _ => {}
        }
    }
    (themes, profiles)
}

#[tokio::test]
async fn both_axes_apply_live_and_cancel_restores_both() {
    let mut chat = test_chat();
    let mut loop_state = test_loop_state();
    let original_theme = chat.app.presented_theme();
    let original_profile = chat.app.tui_config().theme.profile.clone();
    assert_ne!(original_profile.as_deref(), Some("zen"));

    loop_state.open_appearance_picker(&mut chat, AppearanceAxis::Palette);
    assert_eq!(
        loop_state.foreground(),
        Some(super::super::foreground::Foreground::Appearance)
    );
    choose(
        &mut loop_state,
        &mut chat,
        AppearanceAxis::Palette,
        "bcode-light",
    );
    let light = chat.app.presented_theme();
    assert_ne!(light.canvas, original_theme.canvas, "palette applies live");

    choose(&mut loop_state, &mut chat, AppearanceAxis::Profile, "zen");
    assert_eq!(chat.app.tui_config().theme.profile.as_deref(), Some("zen"));
    // Switching the profile keeps the previewed palette.
    assert_eq!(chat.app.presented_theme().canvas, light.canvas);

    key(&mut loop_state, &mut chat, KeyCode::Escape);
    assert!(loop_state.appearance_picker.is_none());
    assert_eq!(chat.app.tui_config().theme.profile, original_profile);
    assert_eq!(chat.app.presented_theme().canvas, original_theme.canvas);
    assert_eq!(chat.app.presented_theme().text, original_theme.text);
    let (themes, profiles) = queued_persistence(&mut chat);
    assert!(
        themes.is_empty() && profiles.is_empty(),
        "cancel persists nothing"
    );
    drop(chat);
}

#[tokio::test]
async fn axis_keys_route_vertical_movement_to_the_active_column() {
    let mut chat = test_chat();
    let mut loop_state = test_loop_state();
    loop_state.open_appearance_picker(&mut chat, AppearanceAxis::Palette);
    let palette_before = loop_state
        .appearance_picker
        .as_ref()
        .and_then(|picker| picker.selected_palette().map(str::to_owned));

    key(&mut loop_state, &mut chat, KeyCode::Right);
    key(&mut loop_state, &mut chat, KeyCode::Down);
    let picker = loop_state.appearance_picker.as_ref().unwrap();
    assert_eq!(picker.axis(), AppearanceAxis::Profile);
    assert_eq!(picker.selected_palette().map(str::to_owned), palette_before);
    let profile = picker.selected_profile().map(str::to_owned);
    assert!(profile.is_some(), "moved off the theme default row");
    assert_eq!(chat.app.tui_config().theme.profile, profile, "applied live");

    key(&mut loop_state, &mut chat, KeyCode::Left);
    key(&mut loop_state, &mut chat, KeyCode::Down);
    let picker = loop_state.appearance_picker.as_ref().unwrap();
    assert_eq!(picker.axis(), AppearanceAxis::Palette);
    assert_ne!(picker.selected_palette().map(str::to_owned), palette_before);
    assert_eq!(picker.selected_profile().map(str::to_owned), profile);
}

#[tokio::test]
async fn apply_persists_theme_then_profile_in_sequence() {
    let mut chat = test_chat();
    let mut loop_state = test_loop_state();
    loop_state.open_appearance_picker(&mut chat, AppearanceAxis::Palette);
    choose(
        &mut loop_state,
        &mut chat,
        AppearanceAxis::Palette,
        "bcode-light",
    );
    choose(&mut loop_state, &mut chat, AppearanceAxis::Profile, "zen");
    key(&mut loop_state, &mut chat, KeyCode::Enter);
    assert!(loop_state.appearance_picker.is_none());

    // Only the theme write starts; the profile waits for it to finish.
    let (themes, profiles) = queued_persistence(&mut chat);
    assert_eq!(themes, vec!["bcode-light".to_owned()]);
    assert_eq!(profiles, Vec::<Option<String>>::new());

    let mut settings = TuiRuntimeSettings::bootstrap(std::path::PathBuf::from("."), &[]);
    let mut draft = DraftAutosave::new(std::path::PathBuf::from("."), String::new());
    apply_effect_result(
        &mut settings,
        &mut chat,
        &mut draft,
        &mut loop_state,
        TuiEffectResult::ThemeSelectionPersisted {
            name: Some("bcode-light".to_owned()),
            result: Ok(std::path::PathBuf::from("tui.toml")),
        },
    );
    let (themes, profiles) = queued_persistence(&mut chat);
    assert_eq!(themes, Vec::<String>::new());
    assert_eq!(profiles, vec![Some("zen".to_owned())]);
    assert_eq!(chat.app.tui_config().theme.name, "bcode-light");
    assert_eq!(chat.app.tui_config().theme.profile.as_deref(), Some("zen"));
}

#[tokio::test]
async fn apply_persists_only_the_changed_axis() {
    let mut chat = test_chat();
    let mut loop_state = test_loop_state();
    loop_state.open_appearance_picker(&mut chat, AppearanceAxis::Palette);
    choose(&mut loop_state, &mut chat, AppearanceAxis::Profile, "zen");
    key(&mut loop_state, &mut chat, KeyCode::Enter);
    let (themes, profiles) = queued_persistence(&mut chat);
    assert_eq!(themes, Vec::<String>::new());
    assert_eq!(profiles, vec![Some("zen".to_owned())]);
    assert!(loop_state.pending_profile_persist.is_none());
}

#[tokio::test]
async fn unlisted_configured_selections_are_never_saved_unless_chosen() {
    let mut chat = test_chat();
    let mut loop_state = test_loop_state();
    let mut config = chat.app.tui_config().clone();
    // A profile id the catalog does not list (for example one skipped by
    // validation), and a theme the palette catalog has no row for.
    config.theme.name = "unlisted-user-theme".to_owned();
    config.theme.profile = Some("ghost-profile".to_owned());
    chat.app.apply_tui_config(config);
    let canvas = chat.app.presented_theme().canvas;

    // Opening and applying straight away saves nothing.
    loop_state.open_appearance_picker(&mut chat, AppearanceAxis::Palette);
    let picker = loop_state.appearance_picker.as_ref().expect("picker open");
    assert_eq!(picker.original_palette(), None);
    assert!(!picker.palette_changed() && !picker.profile_changed());
    key(&mut loop_state, &mut chat, KeyCode::Enter);
    assert!(loop_state.appearance_picker.is_none());
    assert_eq!(queued_persistence(&mut chat), (Vec::new(), Vec::new()));
    assert_eq!(
        chat.app.tui_config().theme.profile.as_deref(),
        Some("ghost-profile")
    );

    // Changing only the profile saves only the profile and previews no
    // fallback palette row.
    loop_state.open_appearance_picker(&mut chat, AppearanceAxis::Palette);
    choose(&mut loop_state, &mut chat, AppearanceAxis::Profile, "zen");
    assert_eq!(chat.app.presented_theme().canvas, canvas);
    key(&mut loop_state, &mut chat, KeyCode::Enter);
    assert_eq!(
        queued_persistence(&mut chat),
        (Vec::new(), vec![Some("zen".to_owned())])
    );

    // Cancel restores an unlisted configured profile verbatim.
    let mut config = chat.app.tui_config().clone();
    config.theme.profile = Some("ghost-profile".to_owned());
    chat.app.apply_tui_config(config);
    loop_state.open_appearance_picker(&mut chat, AppearanceAxis::Palette);
    choose(&mut loop_state, &mut chat, AppearanceAxis::Profile, "zen");
    assert_eq!(chat.app.tui_config().theme.profile.as_deref(), Some("zen"));
    key(&mut loop_state, &mut chat, KeyCode::Escape);
    assert_eq!(
        chat.app.tui_config().theme.profile.as_deref(),
        Some("ghost-profile")
    );
    assert_eq!(queued_persistence(&mut chat), (Vec::new(), Vec::new()));

    // An explicit palette choice is saved even from a fallback row.
    loop_state.open_appearance_picker(&mut chat, AppearanceAxis::Palette);
    choose(
        &mut loop_state,
        &mut chat,
        AppearanceAxis::Palette,
        "bcode-light",
    );
    key(&mut loop_state, &mut chat, KeyCode::Enter);
    assert_eq!(
        queued_persistence(&mut chat).0,
        vec!["bcode-light".to_owned()]
    );
    drop(chat);
}

#[tokio::test]
async fn profile_route_opens_on_the_profile_axis_and_wheel_previews_live() {
    let mut chat = test_chat();
    let mut loop_state = test_loop_state();
    loop_state.open_appearance_picker(&mut chat, AppearanceAxis::Profile);
    let picker = loop_state.appearance_picker.as_ref().expect("picker open");
    assert_eq!(picker.axis(), AppearanceAxis::Profile);
    assert_eq!(
        loop_state.foreground(),
        Some(super::super::foreground::Foreground::Appearance)
    );

    let area = Rect::new(0, 0, 130, 44);
    let theme = super::super::render::TuiTheme::for_app(&chat.app);
    let column = super::super::appearance_picker_render::appearance_picker_layout(
        loop_state.appearance_picker.as_ref().unwrap(),
        area,
        theme,
    )
    .profile
    .expect("wide layout shows the profile column");
    let notch = bmux_tui::event::MouseEvent::new(
        bmux_tui::event::MouseEventKind::ScrollDown,
        bmux_tui::geometry::Point::new(column.list.x, column.list.y),
    );
    assert!(loop_state.handle_appearance_picker_mouse(&mut chat, notch, area));
    let picker = loop_state.appearance_picker.as_ref().unwrap();
    let profile = picker.selected_profile().map(str::to_owned);
    assert!(
        profile.is_some(),
        "the wheel moved off the theme default row"
    );
    assert_eq!(chat.app.tui_config().theme.profile, profile, "applied live");
    key(&mut loop_state, &mut chat, KeyCode::Escape);
    assert_eq!(queued_persistence(&mut chat), (Vec::new(), Vec::new()));
}
