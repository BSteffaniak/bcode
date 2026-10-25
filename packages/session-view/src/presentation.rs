//! Renderer-neutral presentation of structured model output and correlated activity.

use bcode_session_view_models::SessionViewSnapshot;

pub use bcode_session_view_models::presentation::model_output_text;

/// Current activity label from existing plugin status and host-correlated presentation.
/// A status contribution is required: old transcript entries cannot resurrect terminal work.
#[must_use]
pub fn persistent_activity_text(snapshot: &SessionViewSnapshot) -> Option<String> {
    let status = snapshot
        .plugin_status
        .values()
        .find(|status| status.metadata.contains_key("run_id"))?;
    status.metadata.get("run_id")?.as_str()?;
    if status
        .metadata
        .get("status")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| matches!(value, "Completed" | "Failed" | "Cancelled"))
    {
        return None;
    }
    // Current plugin status owns lifecycle and attention. Historical activity may
    // describe a previous stage, so it must not replace the live contribution.
    Some(status.text.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_fields_are_readable_without_json_or_execution_claims() {
        let text = model_output_text(
            r#"{"implementation_prompt":"Inspect 界\nthen test","stop_condition":"Tests pa"#,
        );
        assert!(text.contains("Implementation instructions\nInspect 界\nthen test"));
        assert!(text.contains("Stop condition\nTests pa"));
        assert!(!text.contains("implementation_prompt"));
    }
    #[test]
    fn terminal_status_cannot_be_reopened_by_presentation() {
        let mut snapshot = SessionViewSnapshot::empty();
        let status = bcode_session_view_models::PluginStatusView {
            plugin_id: "test".into(),
            note_id: "active".into(),
            text: "Waiting for evaluator".into(),
            priority: 20,
            metadata: std::collections::BTreeMap::from([
                ("run_id".into(), serde_json::json!("run")),
                ("status".into(), serde_json::json!("Running")),
            ]),
        };
        snapshot.plugin_status.insert("test".into(), status);
        assert_eq!(
            persistent_activity_text(&snapshot).as_deref(),
            Some("Waiting for evaluator")
        );
        snapshot
            .plugin_status
            .get_mut("test")
            .unwrap()
            .metadata
            .insert("status".into(), serde_json::json!("Completed"));
        assert!(persistent_activity_text(&snapshot).is_none());
        snapshot.plugin_status.clear();
        assert!(persistent_activity_text(&snapshot).is_none());
    }

    #[test]
    fn malformed_and_unknown_objects_never_fall_back_to_json() {
        for text in ["{", "{broken", r#"{"secret_envelope":true}"#] {
            assert!(!model_output_text(text).starts_with('{'));
        }
        assert_eq!(model_output_text("Ordinary prose"), "Ordinary prose");
        assert!(persistent_activity_text(&SessionViewSnapshot::empty()).is_none());
    }
}
