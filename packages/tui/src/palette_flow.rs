//! Root-owned plugin surface option and outcome semantics.

use super::effects::TuiEffect;
use super::session_flow::ActiveChat;

fn insert_surface_session_id(
    map: &mut serde_json::Map<String, serde_json::Value>,
    session_id: Option<bcode_session_models::SessionId>,
) {
    if let Some(session_id) = session_id {
        map.insert(
            "session_id".to_string(),
            serde_json::Value::String(session_id.to_string()),
        );
    }
}

pub async fn hydrate_root_plugin_surface_options(
    client: &bcode_client::BcodeClient,
    session_id: Option<bcode_session_models::SessionId>,
    options: serde_json::Value,
) -> serde_json::Value {
    let mut map = match options {
        serde_json::Value::Object(map) => map,
        value => {
            let mut map = serde_json::Map::new();
            if !value.is_null() {
                map.insert("command_options".to_string(), value);
            }
            map
        }
    };
    if let Ok(status) = client.default_model_status().await
        && let Ok(value) = serde_json::to_value(status)
    {
        map.insert("default_model_status".to_string(), value);
    }
    if let Ok(status) = client.server_status().await
        && let Ok(value) = serde_json::to_value(status)
    {
        map.insert("server_status".to_string(), value);
    }
    insert_surface_session_id(&mut map, session_id);
    if let Some(session_id) = session_id {
        if let Ok(status) = client.session_model_status(session_id).await
            && let Ok(value) = serde_json::to_value(status)
        {
            map.insert("session_model_status".to_string(), value);
        }
        if let Ok(skills) = client.active_skills(session_id).await
            && let Ok(value) = serde_json::to_value(skills)
        {
            map.insert("active_skills".to_string(), value);
        }
    }
    serde_json::Value::Object(map)
}

pub fn apply_plugin_surface_outcome(
    chat: &mut ActiveChat,
    plugin_id: &str,
    outcome: Option<serde_json::Value>,
) {
    let Some(outcome) = outcome else {
        return;
    };
    let Ok(outcome) =
        serde_json::from_value::<bcode_plugin_sdk::tui::PluginTuiSurfaceOutcome>(outcome)
    else {
        chat.app
            .set_status("plugin surface returned an invalid outcome".to_owned());
        return;
    };
    if let Some(status) = outcome.status {
        chat.app.set_status(status);
    }
    if let Some(text) = outcome.append_text {
        let (text, format) = text.into_parts();
        chat.push_presentation_note(plugin_id.to_owned(), text, format);
    }
    if let Some(action) = outcome.invoke_command {
        if let bcode_command::CommandAction::Plugin {
            plugin_id,
            command_id,
        } = action
        {
            let arguments = outcome
                .command_args
                .into_iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(" ");
            let working_directory = chat.app.working_directory().map_or_else(
                || std::env::current_dir().unwrap_or_default(),
                std::path::Path::to_path_buf,
            );
            chat.start_effect(TuiEffect::InvokePluginCommand {
                plugin_id,
                command_id,
                arguments: Some(arguments),
                working_directory,
                session_id: chat.session_id,
            });
        }
    }
    if let Some(path) = outcome.set_session_working_directory {
        if let Some(session_id) = chat.app.session_id() {
            chat.start_effect(TuiEffect::AttachWorktree {
                session_id,
                path: std::path::PathBuf::from(&path),
            });
            chat.app.set_status(format!("attaching worktree {path}"));
        } else {
            chat.app
                .set_status("no active session for worktree attach".to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::insert_surface_session_id;

    #[test]
    fn plugin_surface_outcome_supports_legacy_and_formatted_text() {
        let legacy = serde_json::from_value::<bcode_plugin_sdk::tui::PluginTuiSurfaceOutcome>(
            serde_json::json!({"append_text": "* literal"}),
        )
        .expect("legacy surface outcome");
        let (legacy_text, legacy_format) = legacy.append_text.expect("legacy text").into_parts();
        assert_eq!(legacy_text, "* literal");
        assert_eq!(legacy_format, bcode_command::CommandTextFormat::PlainText);

        for (format, expected) in [
            ("markdown", bcode_command::CommandTextFormat::Markdown),
            ("json", bcode_command::CommandTextFormat::Json),
        ] {
            let outcome = serde_json::from_value::<bcode_plugin_sdk::tui::PluginTuiSurfaceOutcome>(
                serde_json::json!({
                    "append_text": {"text": "* value", "format": format}
                }),
            )
            .expect("formatted surface outcome");
            assert_eq!(
                outcome.append_text.expect("formatted text").into_parts(),
                ("* value".to_owned(), expected)
            );
        }
    }

    #[test]
    fn plugin_surface_options_include_active_session_id() {
        let session_id = bcode_session_models::SessionId::new();
        let mut map = serde_json::Map::new();

        insert_surface_session_id(&mut map, Some(session_id));

        assert_eq!(
            map.get("session_id").and_then(serde_json::Value::as_str),
            Some(session_id.to_string().as_str())
        );
    }

    #[test]
    fn draft_plugin_surface_options_omit_session_id() {
        let mut map = serde_json::Map::new();
        insert_surface_session_id(&mut map, None);
        assert!(!map.contains_key("session_id"));
    }
}
