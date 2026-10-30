//! Export the native resolved theme/keymap without making the web host resolve configuration.

use std::collections::BTreeMap;
use std::path::Path;

use bmux_keyboard::{KeyCode, KeyStroke, parse_key_chord};
use bmux_text_edit::{TextDelete, TextEditCommand, TextMotion};
use bmux_tui::style::{Color, Modifier, Style};
use serde_json::{Value, json};

use crate::keymap::BmuxKeyMap;

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
    export_with_actions(&config.tui, project, actions)
}

fn export_with_actions(
    config: &bcode_config::TuiConfig,
    project: &Path,
    actions: Vec<bcode_plugin_sdk::tui::keymap::KeymapAction>,
) -> Result<String, String> {
    let theme = crate::theme::resolve_configured_theme(config, project);
    let mut keymap = BmuxKeyMap::try_from_config(config).map_err(|error| error.to_string())?;
    keymap.integrate_contributors(Ok(actions));
    if let Some(error) = keymap.error() {
        return Err(error.to_owned());
    }
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
    ] {
        styles.insert(name, style(value));
    }
    let mut bindings = Vec::new();
    for action in keymap.registry().actions() {
        for key in keymap.registry().keys_for_action(&action.scope, &action.id) {
            let chord = parse_key_chord(key).map_err(|error| error.to_string())?;
            if chord.is_empty() || chord.len() > 4 {
                return Err("browser key chord exceeds bounds".into());
            }
            let binding = if action.scope == "chat" && chord.len() == 1 {
                if let Some(command) = keymap.editor_command_for_key(chord[0]) {
                    json!({"type":"editor", "command": command_value(command)})
                } else if let Some(motion) = keymap.editor_selection_motion_for_key(chord[0]) {
                    json!({"type":"selection", "motion": motion_value(motion)})
                } else {
                    json!({"type":"host", "action":action.id})
                }
            } else {
                json!({"type":"host", "action":action.id})
            };
            bindings.push(json!({"scope":action.scope, "chord":chord.into_iter().map(stroke).collect::<Vec<_>>(), "binding":binding}));
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
    if bindings.len() > 512 {
        return Err("browser bindings exceed bounds".into());
    }
    let contents =
        json!({"schema_version":1,"revision":"pending","styles":styles,"bindings":bindings});
    let fingerprint = serde_json::to_string(&contents).map_err(|error| error.to_string())?;
    use std::hash::{Hash as _, Hasher as _};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    fingerprint.hash(&mut hash);
    let mut contents = contents;
    contents["revision"] = format!("{:016x}", hash.finish()).into();
    let encoded = serde_json::to_string(&contents).map_err(|error| error.to_string())?;
    if encoded.len() > 64 * 1024 {
        return Err("browser presentation policy exceeds bounds".into());
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

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

fn color(color: Option<Color>) -> (Option<[u8; 3]>, Option<u8>) {
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
