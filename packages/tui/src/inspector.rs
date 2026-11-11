//! Structure inspector helpers and per-tool default persistence of the host shell.
//!
//! The transcript engine's inspector explains how a disclosure target resolved: its facts and
//! every resolved property with the rule and layer that set it. The shared [`property_rows`]
//! flattening is the one `bcode theme explain` prints, so the CLI and the inspector agree row
//! for row.
//!
//! Making a tool's disclosure the default queues a rule here; defaults persist to user state
//! through a [`TuiEffect`](crate::effects::TuiEffect), never by file I/O on the interactive
//! path.

use std::path::PathBuf;

use bcode_presentation_policy::PropertyOrigin;
use bcode_presentation_policy::models::{
    Disclosure, LayerSource, PresentationRule, ResolvedPresentation, UnitFacts,
};
use bmux_tui::component::LayoutNode;
use bmux_tui::geometry::Rect;

use crate::app::BmuxApp;

/// Hit-region id of the painted inspector panel. It sits above transcript
/// hit regions, so clicks on the panel never reach units underneath.
pub const INSPECTOR_HIT_ID: &str = "transcript-inspector";
/// Rows one inspector scroll step moves.
pub const INSPECTOR_SCROLL_ROWS: i64 = 3;
/// Interactive defaults waiting to be persisted, at most.
const MAX_PENDING_DEFAULTS: usize = 32;
/// Bytes of one rendered fact or property value, at most; longer values end
/// with an ellipsis. Values are bounded for work, never measured for layout.
const MAX_VALUE_BYTES: usize = 256;
/// Fact and property rows in one report, at most each.
const MAX_REPORT_ROWS: usize = 256;

/// Interactive per-tool default rules awaiting persistence.
#[derive(Debug, Clone, Default)]
pub struct PendingDefaults {
    /// Default rules waiting for the persistence effect, oldest first; at
    /// most one rule per matcher.
    rules: Vec<PresentationRule>,
    /// Whether a persistence effect is running; pending rules wait for it.
    in_flight: bool,
}

impl PendingDefaults {
    /// Queue `rule` for persistence, replacing a queued rule with the same
    /// matcher; the oldest rule is dropped beyond the bound.
    fn queue(&mut self, rule: PresentationRule) {
        self.rules.retain(|queued| queued.matcher != rule.matcher);
        self.rules.push(rule);
        let excess = self.rules.len().saturating_sub(MAX_PENDING_DEFAULTS);
        self.rules.drain(..excess);
    }
}

/// The inspector body as last painted.
///
/// Holds the terminal rectangle its text view occupied and the view's
/// resolved layout. Scrollbar clicks and drags route through exactly this
/// geometry, so they agree with what is on screen.
#[derive(Debug, Clone)]
pub struct PaintedInspectorBody {
    /// Terminal rectangle the body text view painted into.
    pub area: Rect,
    /// The body text view's resolved layout.
    pub layout: LayoutNode,
}

/// One resolved property and the rule that set it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedProperty {
    /// Property path (`disclosure`, `sections.output.visible`, …), matching
    /// [`PropertyOrigin::property`].
    pub property: String,
    /// Resolved value.
    pub value: serde_json::Value,
    /// Layer and rule that set it; `None` for host defaults.
    pub origin: Option<PropertyOrigin>,
}

/// Flatten a resolved presentation into property rows, attaching origins.
///
/// Sections expand to `sections.<name>.<field>` and plugin properties use
/// their namespaced key, matching [`PropertyOrigin::property`] names. Shared
/// with `bcode theme explain`, so the CLI and the inspector agree row for row.
#[must_use]
pub fn property_rows(
    presentation: &ResolvedPresentation,
    origins: &[PropertyOrigin],
) -> Vec<InspectedProperty> {
    let Ok(serde_json::Value::Object(fields)) = serde_json::to_value(presentation) else {
        return Vec::new();
    };
    let mut values = Vec::new();
    for (name, value) in fields {
        match (name.as_str(), value) {
            ("sections", serde_json::Value::Object(sections)) => {
                for (section, value) in sections {
                    if let serde_json::Value::Object(section_fields) = value {
                        for (field, value) in section_fields {
                            values.push((format!("sections.{section}.{field}"), value));
                        }
                    }
                }
            }
            ("plugin", serde_json::Value::Object(plugin)) => values.extend(plugin),
            (_, value) => values.push((name, value)),
        }
    }
    values
        .into_iter()
        .map(|(property, value)| InspectedProperty {
            origin: origins
                .iter()
                .find(|origin| origin.property == property)
                .cloned(),
            property,
            value,
        })
        .collect()
}

/// Short human label for a layer source (`theme:compact`, `config`, …).
#[must_use]
pub fn layer_label(source: &LayerSource) -> String {
    match source {
        LayerSource::Host => "host".to_owned(),
        LayerSource::Plugin(id) => format!("plugin:{id}"),
        LayerSource::Theme(id) => format!("theme:{id}"),
        LayerSource::Config => "config".to_owned(),
        LayerSource::UserState => "user_state".to_owned(),
    }
}

/// Origin label of a property row: `<layer>#<rule>`, or `default`.
#[must_use]
pub fn origin_label(origin: Option<&PropertyOrigin>) -> String {
    origin.map_or_else(
        || "default".to_owned(),
        |origin| format!("{}#{}", layer_label(&origin.source), origin.rule),
    )
}

/// Facts as `(name, value)` rows, omitting unknown (null) facts; plugin facts
/// use their namespaced key.
#[must_use]
pub fn fact_rows(facts: &UnitFacts) -> Vec<(String, String)> {
    let Ok(serde_json::Value::Object(fields)) = serde_json::to_value(facts) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for (name, value) in fields {
        match (name.as_str(), value) {
            (_, serde_json::Value::Null) => {}
            ("plugin", serde_json::Value::Object(plugin)) => {
                rows.extend(
                    plugin
                        .into_iter()
                        .map(|(key, value)| (key, value_text(&value))),
                );
            }
            (_, value) => rows.push((name, value_text(&value))),
        }
    }
    rows.truncate(MAX_REPORT_ROWS);
    rows
}

/// Display text of a JSON value: strings unquoted, others compact JSON, at
/// most [`MAX_VALUE_BYTES`] bytes (cut on a character boundary).
#[must_use]
pub fn value_text(value: &serde_json::Value) -> String {
    let text = match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    bounded_text(text)
}

/// `text` cut to at most [`MAX_VALUE_BYTES`] bytes on a character boundary,
/// ending with an ellipsis when cut. Bounds work, never measures layout.
#[must_use]
pub fn bounded_text(mut text: String) -> String {
    if text.len() <= MAX_VALUE_BYTES {
        return text;
    }
    let mut end = MAX_VALUE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push('…');
    text
}

/// Human label of a disclosure.
#[must_use]
pub const fn disclosure_name(disclosure: Disclosure) -> &'static str {
    match disclosure {
        Disclosure::Expanded => "expanded",
        Disclosure::Summary => "summary",
        Disclosure::Collapsed => "collapsed",
        Disclosure::Hidden => "hidden",
    }
}

impl BmuxApp {
    /// Queue a per-tool default `rule` for persistence to user state (the transcript's
    /// make-default action); the rule is applied live once stored.
    pub fn queue_default_rule(&mut self, rule: PresentationRule) {
        self.set_status(format!("Saving {}…", default_label(&rule)));
        self.pending_defaults.queue(rule);
    }

    /// Interactive default rules to persist now, unless a persistence effect
    /// is already running (they then wait for it to finish).
    pub fn take_pending_default_rules(&mut self) -> Option<Vec<PresentationRule>> {
        let defaults = &mut self.pending_defaults;
        if defaults.in_flight || defaults.rules.is_empty() {
            return None;
        }
        defaults.in_flight = true;
        Some(std::mem::take(&mut defaults.rules))
    }

    /// Apply the outcome of persisting `saved`: on success the stored user
    /// state rules recompile live. Returns whether the UI changed.
    pub fn finish_default_rules(
        &mut self,
        saved: &[PresentationRule],
        result: Result<(PathBuf, Vec<PresentationRule>), String>,
    ) -> bool {
        self.pending_defaults.in_flight = false;
        let labels = saved
            .iter()
            .map(default_label)
            .collect::<Vec<_>>()
            .join(", ");
        match result {
            Ok((path, rules)) => {
                self.apply_user_structure_rules(rules);
                self.set_status(format!(
                    "Saved {labels} to user state at {}",
                    bcode_plugin_sdk::path::display_from_current_dir(&path)
                ));
            }
            Err(error) => self.set_status(format!("Could not save {labels}: {error}")),
        }
        true
    }
}

/// `default for <tool>: <disclosure>` for a per-tool default rule.
fn default_label(rule: &PresentationRule) -> String {
    let tools = rule.matcher.tool.0.join(", ");
    let disclosure = rule.set.disclosure.map_or("unchanged", disclosure_name);
    format!("default for {tools}: {disclosure}")
}

#[cfg(test)]
mod tests {
    use bcode_presentation_policy::models::{FactMatcher, OneOrMany, PropertyPatch, UnitKind};

    use super::*;

    fn tool_rule(tool: &str, disclosure: Disclosure) -> PresentationRule {
        PresentationRule {
            matcher: FactMatcher {
                kind: OneOrMany(vec![UnitKind::Tool]),
                tool: OneOrMany(vec![tool.to_owned()]),
                ..FactMatcher::default()
            },
            set: PropertyPatch {
                disclosure: Some(disclosure),
                ..PropertyPatch::default()
            },
        }
    }

    #[test]
    fn queued_defaults_persist_one_rule_per_tool_and_wait_for_a_running_effect() {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        assert_eq!(app.take_pending_default_rules(), None);
        app.queue_default_rule(tool_rule("read", Disclosure::Expanded));
        // Queuing again replaces the queued rule instead of duplicating it.
        app.queue_default_rule(tool_rule("read", Disclosure::Collapsed));
        let saved = app.take_pending_default_rules().expect("queued rule");
        assert_eq!(saved, [tool_rule("read", Disclosure::Collapsed)]);
        // While the effect runs, new defaults wait for it.
        app.queue_default_rule(tool_rule("grep", Disclosure::Summary));
        assert_eq!(app.take_pending_default_rules(), None);
        assert!(app.finish_default_rules(&saved, Ok((PathBuf::from("tui.toml"), saved.clone()))));
        assert!(app.status().contains("default for read: collapsed"));
        assert_eq!(app.user_structure_rules(), saved.as_slice());
        // The rule queued during the effect persists next.
        assert_eq!(
            app.take_pending_default_rules().map(|rules| rules.len()),
            Some(1)
        );
        assert!(app.finish_default_rules(&[], Err("disk full".to_owned())));
        assert!(app.status().contains("disk full"));
        drop(app);
    }

    #[test]
    fn values_are_bounded_on_character_boundaries() {
        let long = "日本語🚀".repeat(100);
        let text = value_text(&serde_json::Value::String(long));
        assert!(text.len() <= MAX_VALUE_BYTES + '…'.len_utf8());
        assert!(text.ends_with('…'));
        assert_eq!(value_text(&serde_json::json!(3)), "3");
        assert_eq!(value_text(&serde_json::json!("e\u{301}")), "e\u{301}");
    }
}
