//! Export the native resolved theme/keymap without making the web host resolve configuration.

use std::collections::BTreeMap;
use std::hash::{Hash as _, Hasher as _};
use std::path::Path;

use bmux_keyboard::{KeyCode, KeyStroke, parse_key_chord};
use bmux_text_edit::{TextDelete, TextEditCommand, TextMotion};
use bmux_tui::style::{Color, Modifier, Style};
use serde_json::{Value, json};

use crate::keymap::{BmuxKeyActivation, BmuxKeyMap, BmuxScope};

/// Resolve the native reasoning visibility setting for semantic projection.
#[must_use]
pub const fn reasoning_policy(
    config: bcode_config::TuiThinkingConfig,
) -> bcode_session_view_models::ReasoningPresentationPolicy {
    use bcode_session_view_models::ReasoningPresentationPolicy;
    if !config.show {
        return ReasoningPresentationPolicy::Hidden;
    }
    match config.mode {
        bcode_config::TuiThinkingMode::All => ReasoningPresentationPolicy::All,
        bcode_config::TuiThinkingMode::Summary => ReasoningPresentationPolicy::Summary,
        bcode_config::TuiThinkingMode::Raw => ReasoningPresentationPolicy::Raw,
    }
}

/// Resolve the same theme and effective bindings used by the native shell.
///
/// # Errors
/// Returns invalid configuration or a policy exceeding the browser contract's
/// bounds. No credentials or unrelated configuration is exported.
pub fn export(config: &bcode_config::TuiConfig, project: &Path) -> Result<String, String> {
    export_with_actions(config, project, Vec::new())
}

/// Export presentation with the enabled plugin contributors used by the native host.
///
/// # Errors
/// Returns plugin discovery, contributor conflict, or presentation configuration errors.
pub fn export_with_plugins(
    config: &bcode_config::BcodeConfig,
    project: &Path,
    selection: &bcode_plugin::PluginSelection,
    plugins: &[bcode_plugin::StaticBundledPlugin],
) -> Result<String, String> {
    let extensions = crate::bundled_tui_extensions();
    let presentation = crate::plugin_tui::load_default_presentation_with_static_bundled(
        selection,
        config.tui.visual_adapters.clone(),
        plugins,
        &extensions,
    )
    .map_err(|error| error.to_string())?;
    let actions = presentation
        .keymap_actions()
        .map_err(|error| error.to_string())?;
    let visual_policy = presentation.portable_visual_policy();
    drop(presentation);
    export_with_actions_and_visual_policy(
        &config.tui,
        project,
        actions,
        json!({
            "enabled_plugins": visual_policy.enabled_plugins,
            "preferred": visual_policy.preferred,
            "disabled": visual_policy.disabled,
        }),
    )
}

fn export_with_actions(
    config: &bcode_config::TuiConfig,
    project: &Path,
    actions: Vec<bcode_plugin_sdk::tui::keymap::KeymapAction>,
) -> Result<String, String> {
    export_with_actions_and_visual_policy(
        config,
        project,
        actions,
        json!({"enabled_plugins": [], "preferred": [], "disabled": []}),
    )
}

fn export_with_actions_and_visual_policy(
    config: &bcode_config::TuiConfig,
    project: &Path,
    actions: Vec<bcode_plugin_sdk::tui::keymap::KeymapAction>,
    visual_policy: Value,
) -> Result<String, String> {
    let theme = crate::theme::resolve_configured_theme(config, project);
    let mut keymap = BmuxKeyMap::try_from_config(config).map_err(|error| error.to_string())?;
    keymap.integrate_contributors(Ok(actions));
    if let Some(error) = keymap.error() {
        return Err(error.to_owned());
    }
    let styles = export_styles(&theme);
    let mut bindings = Vec::new();
    for action in keymap.registry().actions() {
        for key in keymap.registry().keys_for_action(&action.scope, &action.id) {
            let chord = parse_key_chord(key).map_err(|error| error.to_string())?;
            if chord.is_empty() || chord.len() > 4 {
                return Err("browser key chord exceeds bounds".into());
            }
            let binding =
                if action.scope == "chat" && chord.len() == 1 {
                    keymap.editor_command_for_key(chord[0]).map_or_else(
                        || {
                            keymap.editor_selection_motion_for_key(chord[0]).map_or_else(
                        || json!({"type":"host", "action":action.id}),
                        |motion| json!({"type":"selection", "motion":motion_value(motion)}),
                    )
                        },
                        |command| json!({"type":"editor", "command":command_value(command)}),
                    )
                } else {
                    json!({"type":"host", "action":action.id})
                };
            let multi_tap = if action.scope == "chat" && chord.len() == 1 {
                keymap
                    .binding_for_key(BmuxScope::Chat, chord[0])
                    .and_then(|binding| match binding.activation() {
                        BmuxKeyActivation::Immediate => None,
                        BmuxKeyActivation::MultiTap {
                            required_taps,
                            window_ms,
                            ..
                        } => Some(json!({"taps": required_taps, "window_ms": window_ms})),
                    })
            } else {
                None
            };
            bindings.push(json!({"scope":action.scope, "chord":chord.into_iter().map(stroke).collect::<Vec<_>>(), "binding":binding, "multi_tap":multi_tap}));
        }
    }
    // Include explicitly disabled keys: absence must not revive browser defaults.
    for (scope, entries) in config.keybindings.scoped_overrides_with(str::to_owned) {
        for (key, action) in entries {
            if !action.is_empty() {
                continue;
            }
            let chord = parse_key_chord(&key).map_err(|error| error.to_string())?;
            bindings.push(json!({"scope":scope,"chord":chord.into_iter().map(stroke).collect::<Vec<_>>(),"binding":{"type":"ignored"}}));
        }
    }
    let binding_count = bindings.len();
    let mut contents = json!({"schema_version":1,"revision":"pending","styles":styles,"bindings":bindings,
            "interaction":config.interactions,"visual_policy":visual_policy});
    contents["visual_policy"] = visual_policy;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(&contents)
        .map_err(|error| error.to_string())?
        .hash(&mut hash);
    contents["revision"] = format!("{:016x}", hash.finish()).into();
    let encoded = serde_json::to_string(&contents).map_err(|error| error.to_string())?;
    validate_policy_bounds(binding_count, encoded.len())?;
    tracing::debug!(
        binding_count,
        encoded_bytes = encoded.len(),
        "exported complete browser presentation policy"
    );
    Ok(encoded)
}

fn export_styles(theme: &crate::theme::PresentedTheme) -> BTreeMap<&'static str, Value> {
    let mut styles = BTreeMap::new();
    for (name, value) in [
        ("surface", theme.canvas),
        ("text", theme.text),
        ("muted", theme.muted),
        ("selection", theme.selection),
        ("border", theme.border),
        ("focused", theme.focused),
        ("title", theme.markdown.heading),
        ("error", theme.error),
        ("code", theme.markdown.inline_code),
        ("markdown.text", theme.markdown.text),
        ("markdown.heading", theme.markdown.heading),
        ("markdown.link", theme.markdown.link),
        ("markdown.strong", theme.markdown.strong),
        ("markdown.emphasis", theme.markdown.emphasis),
        ("markdown.strikethrough", theme.markdown.strikethrough),
        ("markdown.inline_code", theme.markdown.inline_code),
        ("markdown.code_block_text", theme.markdown.code_block_text),
        (
            "markdown.code_block_border",
            theme.markdown.code_block_border,
        ),
        ("markdown.blockquote_bar", theme.markdown.blockquote_bar),
        ("markdown.alert_note", theme.markdown.alert_note),
        ("markdown.alert_tip", theme.markdown.alert_tip),
        ("markdown.alert_important", theme.markdown.alert_important),
        ("markdown.alert_warning", theme.markdown.alert_warning),
        ("markdown.alert_caution", theme.markdown.alert_caution),
        ("markdown.list_marker", theme.markdown.list_marker),
        ("markdown.task_checked", theme.markdown.task_checked),
        ("markdown.task_unchecked", theme.markdown.task_unchecked),
        ("markdown.table_border", theme.markdown.table_border),
        ("markdown.horizontal_rule", theme.markdown.horizontal_rule),
    ] {
        styles.insert(name, style(value));
    }
    let component = theme.component_theme();
    for (name, value) in [
        ("component.raised", component.surfaces.raised),
        ("component.overlay", component.surfaces.overlay),
        ("component.disabled", component.disabled),
        ("component.info", component.info),
        ("component.success", component.success),
        ("component.warning", component.warning),
    ] {
        styles.insert(name, style(value));
    }
    if let Some(scrim) = component.surfaces.scrim {
        styles.insert("component.scrim", style(scrim));
    }
    styles
}

fn validate_policy_bounds(binding_count: usize, encoded_bytes: usize) -> Result<(), String> {
    // All effective scopes are retained. These are admission limits, never truncation limits.
    if binding_count > 4096 || encoded_bytes > 1024 * 1024 {
        return Err(format!(
            "browser presentation policy exceeds bounds: bindings={binding_count} (max 4096), encoded_bytes={encoded_bytes} (max 1048576)"
        ));
    }
    Ok(())
}

fn stroke(stroke: KeyStroke) -> Value {
    let key = match stroke.key {
        KeyCode::Char(' ') | KeyCode::Space => " ".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Escape => "Escape".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::Left => "ArrowLeft".into(),
        KeyCode::Right => "ArrowRight".into(),
        KeyCode::Up => "ArrowUp".into(),
        KeyCode::Down => "ArrowDown".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::Delete => "Delete".into(),
        KeyCode::PageUp => "PageUp".into(),
        KeyCode::PageDown => "PageDown".into(),
        other => format!("{other:?}"),
    };
    json!({"key":key,"ctrl":stroke.modifiers.ctrl,"alt":stroke.modifiers.alt,"shift":stroke.modifiers.shift,"meta":stroke.modifiers.super_key})
}

fn style(style: Style) -> Value {
    let (foreground, foreground_palette) = color(style.fg);
    let (background, background_palette) = color(style.bg);
    json!({"foreground":foreground,"background":background,
        "foreground_palette":foreground_palette,"background_palette":background_palette,
        "bold":style.modifiers.contains(Modifier::BOLD),"italic":style.modifiers.contains(Modifier::ITALIC),
        "underline":style.modifiers.contains(Modifier::UNDERLINE),"reversed":style.modifiers.contains(Modifier::REVERSED),
        "dim":style.modifiers.contains(Modifier::DIM),"strikethrough":style.modifiers.contains(Modifier::CROSSED_OUT),
        "slow_blink":style.modifiers.contains(Modifier::SLOW_BLINK),"hidden":style.modifiers.contains(Modifier::HIDDEN)})
}

const fn color(color: Option<Color>) -> (Option<[u8; 3]>, Option<u8>) {
    let index = match color {
        None | Some(Color::Default) => return (None, None),
        Some(Color::Rgb(r, g, b)) => return (Some([r, g, b]), None),
        Some(Color::Indexed(index)) => index,
        Some(Color::Black) => 0,
        Some(Color::Red) => 1,
        Some(Color::Green) => 2,
        Some(Color::Yellow) => 3,
        Some(Color::Blue) => 4,
        Some(Color::Magenta) => 5,
        Some(Color::Cyan) => 6,
        Some(Color::White) => 7,
        Some(Color::BrightBlack) => 8,
        Some(Color::BrightRed) => 9,
        Some(Color::BrightGreen) => 10,
        Some(Color::BrightYellow) => 11,
        Some(Color::BrightBlue) => 12,
        Some(Color::BrightMagenta) => 13,
        Some(Color::BrightCyan) => 14,
        Some(Color::BrightWhite) => 15,
    };
    (None, Some(index))
}

fn command_value(command: TextEditCommand) -> Value {
    match command {
        TextEditCommand::Insert(value) => json!({"Insert":value}),
        TextEditCommand::InsertString(value) => json!({"InsertString":value}),
        TextEditCommand::Paste(value) => json!({"Paste":value}),
        TextEditCommand::Move(motion) => json!({"Move":motion_value(motion)}),
        TextEditCommand::Delete(delete) => json!({"Delete":match delete {
            TextDelete::Backward=>"Backward",TextDelete::Forward=>"Forward",TextDelete::WordBackward=>"WordBackward",
            TextDelete::WordForward=>"WordForward",TextDelete::ToLineStart=>"ToLineStart",TextDelete::ToLineEnd=>"ToLineEnd",
            TextDelete::ToStart=>"ToStart",TextDelete::ToEnd=>"ToEnd",
        }}),
        TextEditCommand::Clear => json!("Clear"),
    }
}

fn motion_value(motion: TextMotion) -> Value {
    match motion {
        TextMotion::Absolute(index) => json!({"Absolute":index}),
        TextMotion::Left => json!("Left"),
        TextMotion::Right => json!("Right"),
        TextMotion::WordLeft => json!("WordLeft"),
        TextMotion::WordRight => json!("WordRight"),
        TextMotion::Start => json!("Start"),
        TextMotion::End => json!("End"),
        TextMotion::LineStart => json!("LineStart"),
        TextMotion::LineEnd => json!("LineEnd"),
        TextMotion::VisualUp => json!("VisualUp"),
        TextMotion::VisualDown => json!("VisualDown"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_admission_rejects_one_over_each_limit_with_exact_measurements() {
        assert!(validate_policy_bounds(4096, 1024 * 1024).is_ok());
        let count_error = validate_policy_bounds(4097, 123).unwrap_err();
        assert!(count_error.contains("bindings=4097"));
        assert!(count_error.contains("encoded_bytes=123"));
        let byte_error = validate_policy_bounds(123, 1024 * 1024 + 1).unwrap_err();
        assert!(byte_error.contains("bindings=123"));
        assert!(byte_error.contains("encoded_bytes=1048577"));
    }

    #[test]
    fn complete_native_defaults_and_large_contributor_sets_fit_the_bounded_policy() {
        let config = bcode_config::TuiConfig::default();
        let defaults = export(&config, Path::new(".")).expect("complete native defaults");
        let native: Value = serde_json::from_str(&defaults).unwrap();
        eprintln!(
            "native browser policy: bindings={}, encoded_bytes={}",
            native["bindings"].as_array().unwrap().len(),
            defaults.len()
        );
        let actions = (0..600)
            .map(|index| bcode_plugin_sdk::tui::keymap::KeymapAction {
                id: format!("plugin.test.action{index}"),
                scope: format!("plugin_test_scope{index}"),
                description: "Contributor-owned effective shortcut".into(),
                defaults: vec!["space".into()],
            })
            .collect();
        let encoded = export_with_actions(&config, Path::new("."), actions).unwrap();
        let contributed: Value = serde_json::from_str(&encoded).unwrap();
        let bindings = contributed["bindings"].as_array().unwrap();
        assert_eq!(
            bindings.len(),
            native["bindings"].as_array().unwrap().len() + 600
        );
        for index in 0..600 {
            assert!(bindings.iter().any(|binding| binding["scope"]
                == format!("plugin_test_scope{index}")
                && binding["chord"][0]["key"] == " "
                && binding["binding"]
                    == json!({"type":"host", "action":format!("plugin.test.action{index}")})));
        }
    }

    #[test]
    fn exports_every_effective_native_scope_without_truncating_defaults_or_overrides() {
        let mut config = bcode_config::TuiConfig::default();
        config
            .keybindings
            .chat
            .insert("enter".into(), String::new());
        config
            .keybindings
            .chat
            .insert("ctrl+g".into(), "tui.editor.moveCursorLeft".into());
        let keymap = BmuxKeyMap::try_from_config(&config).unwrap();
        let exported: Value =
            serde_json::from_str(&export(&config, Path::new(".")).unwrap()).unwrap();
        let bindings = exported["bindings"].as_array().unwrap();
        let mut expected = 0;
        for action in keymap.registry().actions() {
            for key in keymap.registry().keys_for_action(&action.scope, &action.id) {
                let chord = parse_key_chord(key)
                    .unwrap()
                    .into_iter()
                    .map(stroke)
                    .collect::<Vec<_>>();
                let matches = bindings
                    .iter()
                    .filter(|binding| {
                        binding["scope"] == action.scope && binding["chord"] == json!(chord)
                    })
                    .collect::<Vec<_>>();
                assert_eq!(matches.len(), 1, "{} {key}", action.scope);
                if matches[0]["binding"]["type"] == "host" {
                    assert_eq!(matches[0]["binding"]["action"], action.id);
                }
                expected += 1;
            }
        }
        assert_eq!(bindings.len(), expected + 1); // The explicit Enter unbind is retained too.
        assert!(bindings.iter().any(|binding| binding["scope"] == "chat"
            && binding["chord"]
                == json!([stroke(bmux_keyboard::parse_key_stroke("enter").unwrap())])
            && binding["binding"] == json!({"type":"ignored"})));
    }

    #[test]
    fn exports_resolved_configuration_without_reinterpreting_editor_actions() {
        let mut config = bcode_config::TuiConfig::default();
        config
            .keybindings
            .chat
            .insert("ctrl+g".into(), "tui.editor.moveCursorLeft".into());
        config
            .keybindings
            .chat
            .insert("enter".into(), String::new());
        let result = export(&config, Path::new(".")).expect("native resolved policy");
        let policy: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(policy["schema_version"], 1);
        assert!(
            policy["revision"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        let bindings = policy["bindings"].as_array().unwrap();
        assert!(
            bindings
                .iter()
                .any(|binding| binding["binding"] == json!({"type":"ignored"})
                    && binding["chord"][0]["key"] == "Enter")
        );
        assert!(
            bindings
                .iter()
                .any(|binding| binding["chord"][0]["key"] == "g"
                    && binding["binding"] == json!({"type":"editor","command":{"Move":"Left"}}))
        );
        assert_eq!(export(&config, Path::new(".")).unwrap(), result);
    }

    #[test]
    fn exports_native_markdown_and_component_roles_and_interaction_policy() {
        let mut config = bcode_config::TuiConfig::default();
        config.interactions.placement = bcode_config::TuiInteractionPlacement::Pinned;
        let theme = crate::theme::resolve_configured_theme(&config, Path::new("."));
        let components = theme.component_theme();
        let result: Value =
            serde_json::from_str(&export(&config, Path::new(".")).unwrap()).unwrap();
        for (role, expected) in [
            ("markdown.text", theme.markdown.text),
            ("markdown.heading", theme.markdown.heading),
            ("markdown.link", theme.markdown.link),
            ("markdown.code_block_text", theme.markdown.code_block_text),
            (
                "markdown.code_block_border",
                theme.markdown.code_block_border,
            ),
            ("markdown.alert_warning", theme.markdown.alert_warning),
            ("component.raised", components.surfaces.raised),
            ("component.overlay", components.surfaces.overlay),
            ("component.disabled", components.disabled),
            ("component.warning", components.warning),
        ] {
            assert_eq!(result["styles"][role], style(expected), "{role}");
        }
        assert_eq!(
            result["styles"]["markdown.heading"],
            style(theme.markdown.heading)
        );
        assert_eq!(
            result["styles"]["markdown.link"],
            style(theme.markdown.link)
        );
        assert_eq!(
            result["styles"]["markdown.inline_code"],
            style(theme.markdown.inline_code)
        );
        assert_eq!(
            result["interaction"],
            serde_json::to_value(config.interactions).unwrap()
        );
        assert!(result["styles"].as_object().unwrap().len() <= 64);
    }

    #[test]
    fn visual_selection_is_explicit_and_changes_the_policy_revision() {
        let config = bcode_config::TuiConfig::default();
        let project = Path::new(".");
        let empty: Value = serde_json::from_str(&export(&config, project).unwrap()).unwrap();
        assert_eq!(empty["visual_policy"]["enabled_plugins"], json!([]));
        let selection = json!({
            "enabled_plugins": ["bcode.question"],
            "preferred": ["bcode.question/question-outcome-component"],
            "disabled": [],
        });
        let selected: Value = serde_json::from_str(
            &export_with_actions_and_visual_policy(&config, project, Vec::new(), selection.clone())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(selected["visual_policy"], selection);
        assert_ne!(empty["revision"], selected["revision"]);
        assert_eq!(empty["styles"], selected["styles"]);
        assert_eq!(empty["bindings"], selected["bindings"]);
    }

    #[test]
    fn cancel_activation_comes_from_the_effective_native_binding() {
        let mut config = bcode_config::TuiConfig::default();
        let project = Path::new(".");
        let policy: Value = serde_json::from_str(&export(&config, project).unwrap()).unwrap();
        let interrupt = policy["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|binding| {
                binding["scope"] == "chat" && binding["binding"]["action"] == "app.interrupt"
            })
            .unwrap();
        assert_eq!(interrupt["multi_tap"], json!({"taps":2,"window_ms":1500}));
        config
            .keybindings
            .chat
            .insert("ctrl+g".into(), "app.interrupt".into());
        let policy: Value = serde_json::from_str(&export(&config, project).unwrap()).unwrap();
        let remapped = policy["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|binding| {
                binding["scope"] == "chat"
                    && binding["chord"][0]["key"] == "g"
                    && binding["chord"][0]["ctrl"] == true
            })
            .unwrap();
        assert_eq!(remapped["binding"]["action"], "app.interrupt");
        assert_eq!(remapped["multi_tap"], Value::Null);
    }

    #[test]
    fn styles_preserve_palette_and_modifiers_without_rgb_guesses() {
        let result = style(
            Style::new()
                .fg(Color::Indexed(196))
                .bg(Color::Rgb(1, 2, 3))
                .add_modifier(
                    Modifier::DIM
                        .union(Modifier::HIDDEN)
                        .union(Modifier::CROSSED_OUT),
                ),
        );
        assert_eq!(result["foreground"], Value::Null);
        assert_eq!(result["foreground_palette"], 196);
        assert_eq!(result["background"], json!([1, 2, 3]));
        assert_eq!(result["dim"], true);
        assert_eq!(result["hidden"], true);
        assert_eq!(result["strikethrough"], true);
    }
}
