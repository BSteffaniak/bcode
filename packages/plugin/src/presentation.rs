//! Manifest-declared structural presentation defaults for a plugin's own tools.
//!
//! Presentation is display-only and never affects whether a plugin or its tools load. The
//! `[presentation]` table therefore decodes tolerantly: each tool entry and rule decodes on its
//! own, and entries this build cannot decode (malformed, or using vocabulary from a newer host)
//! are dropped with a diagnostic instead of failing the manifest. [`Self::effective`] applies the
//! remaining bounds and ownership checks the same way.
//!
//! [`Self::effective`]: PluginPresentationDeclaration::effective

use std::collections::{BTreeMap, BTreeSet};

use bcode_presentation_policy_models::{
    LayerSource, MAX_RULES_PER_LAYER, PresentationRule, RuleLayer,
};
use serde::{Deserialize, Serialize};

/// Maximum tools a plugin may describe in `[presentation]`.
pub const MAX_PRESENTATION_TOOLS: usize = 256;
/// Maximum target argument fields declared for one tool.
pub const MAX_TARGET_FIELDS: usize = 8;
/// Maximum UTF-8 bytes of one target argument field name.
pub const MAX_TARGET_FIELD_BYTES: usize = 64;
/// Maximum UTF-8 bytes of a described tool name.
pub const MAX_PRESENTATION_TOOL_NAME_BYTES: usize = 256;
/// Maximum diagnostics retained for one declaration.
pub const MAX_PRESENTATION_DIAGNOSTICS: usize = 32;

/// `[presentation]` table of a plugin manifest.
///
/// ```toml
/// [[presentation.tools]]
/// tool_name = "filesystem.read"
/// target_fields = ["path"]
///
/// [[presentation.rules]]
/// match = { tool = "filesystem.read" }
/// set = { sections = { output = { disclosure = "collapsed" } } }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PluginPresentationDeclaration {
    /// Per-tool presentation metadata.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<PluginToolPresentationMetadata>,
    /// Default presentation rules, applied only to this plugin's own units.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<PresentationRule>,
    /// Why entries were dropped while decoding or in [`Self::effective`], at most
    /// [`MAX_PRESENTATION_DIAGNOSTICS`]. Never serialized.
    #[serde(skip)]
    pub diagnostics: Vec<String>,
}

/// Presentation metadata for one tool owned by the declaring plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginToolPresentationMetadata {
    /// Exact model-callable tool name owned by this plugin.
    pub tool_name: String,
    /// Top-level argument fields, in priority order, whose first present string value is the
    /// unit's `target` fact (a path, command, or pattern).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub target_fields: Vec<String>,
}

impl<'de> Deserialize<'de> for PluginPresentationDeclaration {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(Self::from_value(value))
    }
}

impl PluginPresentationDeclaration {
    /// Decode a declaration tolerantly; see the module documentation.
    #[must_use]
    pub fn from_value(value: serde_json::Value) -> Self {
        let mut declaration = Self::default();
        let serde_json::Value::Object(table) = value else {
            declaration.diagnose("[presentation] must be a table; ignored".to_owned());
            return declaration;
        };
        for (key, value) in table {
            match key.as_str() {
                "tools" => {
                    for (index, value) in
                        declaration.entries("tools", value).into_iter().enumerate()
                    {
                        match serde_json::from_value(value) {
                            Ok(tool) => declaration.tools.push(tool),
                            Err(error) => declaration
                                .diagnose(format!("presentation tool {index} ignored: {error}")),
                        }
                    }
                }
                "rules" => {
                    for (index, value) in
                        declaration.entries("rules", value).into_iter().enumerate()
                    {
                        match serde_json::from_value(value) {
                            Ok(rule) => declaration.rules.push(rule),
                            Err(error) => declaration
                                .diagnose(format!("presentation rule {index} ignored: {error}")),
                        }
                    }
                }
                _ => declaration.diagnose(format!("unknown presentation key {key:?} ignored")),
            }
        }
        declaration
    }

    /// Array entries of `key`, or none (with a diagnostic) when `value` is not an array.
    fn entries(&mut self, key: &str, value: serde_json::Value) -> Vec<serde_json::Value> {
        if let serde_json::Value::Array(entries) = value {
            entries
        } else {
            self.diagnose(format!("presentation {key} must be an array; ignored"));
            Vec::new()
        }
    }

    fn diagnose(&mut self, message: String) {
        if self.diagnostics.len() < MAX_PRESENTATION_DIAGNOSTICS {
            self.diagnostics.push(message);
        }
    }

    /// Whether nothing is declared.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.rules.is_empty()
    }

    /// Target argument fields declared for `tool_name`, in priority order.
    #[must_use]
    pub fn target_fields(&self, tool_name: &str) -> Option<&[String]> {
        self.tools
            .iter()
            .find(|tool| tool.tool_name == tool_name)
            .map(|tool| tool.target_fields.as_slice())
    }

    /// Default rules as a plugin layer scoped to `plugin_id`'s own units.
    ///
    /// Rules without a producer condition gain `producer = plugin_id`. Use the rules of
    /// [`Self::effective`], which drops rules naming another producer.
    #[must_use]
    pub fn rule_layer(&self, plugin_id: &str) -> RuleLayer {
        RuleLayer {
            source: LayerSource::Plugin(plugin_id.to_owned()),
            rules: self
                .rules
                .iter()
                .map(|rule| {
                    let mut rule = rule.clone();
                    if rule.matcher.producer.is_unset() {
                        rule.matcher.producer.0.push(plugin_id.to_owned());
                    }
                    rule
                })
                .collect(),
        }
    }

    /// The usable part of this declaration for the plugin `plugin_id`.
    ///
    /// `provides_tools` says whether the plugin provides `bcode.tool/v1`. Entries are dropped,
    /// each with a diagnostic appended to the decoding diagnostics, when:
    /// * tools are declared by a plugin that provides no tools;
    /// * they exceed [`MAX_PRESENTATION_TOOLS`] tools or [`MAX_RULES_PER_LAYER`] rules;
    /// * a tool name is blank, longer than [`MAX_PRESENTATION_TOOL_NAME_BYTES`], or repeated
    ///   (the first entry is kept);
    /// * a tool declares more than [`MAX_TARGET_FIELDS`] target fields, or a field name is
    ///   invalid (see [`valid_target_field`]) or repeated;
    /// * a rule's `producer` condition names any plugin other than `plugin_id`.
    ///
    /// Property values inside rules are checked by presentation policy compilation, which
    /// reports its own diagnostics.
    #[must_use]
    pub fn effective(&self, plugin_id: &str, provides_tools: bool) -> Self {
        let mut effective = Self {
            diagnostics: self.diagnostics.clone(),
            ..Self::default()
        };
        if !self.tools.is_empty() && !provides_tools {
            effective.diagnose(
                "presentation tools ignored: the plugin does not provide bcode.tool/v1".to_owned(),
            );
        }
        let tools = if provides_tools { &self.tools[..] } else { &[] };
        if tools.len() > MAX_PRESENTATION_TOOLS {
            effective.diagnose(format!(
                "presentation tools beyond the first {MAX_PRESENTATION_TOOLS} ignored"
            ));
        }
        let mut names = BTreeSet::new();
        for tool in tools.iter().take(MAX_PRESENTATION_TOOLS) {
            let name = tool.tool_name.trim();
            let reason =
                if name.is_empty() || tool.tool_name.len() > MAX_PRESENTATION_TOOL_NAME_BYTES {
                    Some("its tool_name must be non-empty and bounded")
                } else if names.contains(name) {
                    Some("it is declared more than once")
                } else if tool.target_fields.len() > MAX_TARGET_FIELDS {
                    Some("it declares too many target fields")
                } else if !distinct_valid_fields(&tool.target_fields) {
                    Some("it has an invalid or repeated target field")
                } else {
                    None
                };
            if let Some(reason) = reason {
                effective.diagnose(format!("presentation tool {name:?} ignored: {reason}"));
            } else {
                names.insert(name);
                effective.tools.push(PluginToolPresentationMetadata {
                    tool_name: name.to_owned(),
                    target_fields: tool.target_fields.clone(),
                });
            }
        }
        if self.rules.len() > MAX_RULES_PER_LAYER {
            effective.diagnose(format!(
                "presentation rules beyond the first {MAX_RULES_PER_LAYER} ignored"
            ));
        }
        for (index, rule) in self.rules.iter().take(MAX_RULES_PER_LAYER).enumerate() {
            if rule
                .matcher
                .producer
                .0
                .iter()
                .any(|producer| producer != plugin_id)
            {
                effective.diagnose(format!(
                    "presentation rule {index} ignored: it may only target the plugin's own units"
                ));
            } else {
                effective.rules.push(rule.clone());
            }
        }
        effective
    }
}

/// Default rules of `declarations` (plugin id to declaration) as presentation-policy layers.
///
/// There is one [`LayerSource::Plugin`] layer per plugin with rules, in plugin-id order, each
/// scoped to that plugin's own units (see [`PluginPresentationDeclaration::rule_layer`]).
#[must_use]
pub fn presentation_rule_layers(
    declarations: &BTreeMap<String, PluginPresentationDeclaration>,
) -> Vec<RuleLayer> {
    declarations
        .iter()
        .filter(|(_, declaration)| !declaration.rules.is_empty())
        .map(|(plugin_id, declaration)| declaration.rule_layer(plugin_id))
        .collect()
}

/// Declared target argument fields of `declarations`.
///
/// Keyed by tool name, then declaring plugin id, with ordered field names. Tools without
/// target fields are skipped.
#[must_use]
pub fn presentation_target_fields(
    declarations: &BTreeMap<String, PluginPresentationDeclaration>,
) -> BTreeMap<String, BTreeMap<String, Vec<String>>> {
    let mut fields = BTreeMap::<String, BTreeMap<String, Vec<String>>>::new();
    for (plugin_id, declaration) in declarations {
        for tool in &declaration.tools {
            if !tool.target_fields.is_empty() {
                fields
                    .entry(tool.tool_name.clone())
                    .or_default()
                    .insert(plugin_id.clone(), tool.target_fields.clone());
            }
        }
    }
    fields
}

/// Whether `fields` are all valid target field names with no repeats.
fn distinct_valid_fields(fields: &[String]) -> bool {
    let mut seen = BTreeSet::new();
    fields
        .iter()
        .all(|field| valid_target_field(field) && seen.insert(field.as_str()))
}

/// Whether `field` is a valid target argument field name: 1..=[`MAX_TARGET_FIELD_BYTES`]
/// bytes of ASCII letters, digits, `_`, and `-`.
#[must_use]
pub fn valid_target_field(field: &str) -> bool {
    !field.is_empty()
        && field.len() <= MAX_TARGET_FIELD_BYTES
        && field
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> PluginPresentationDeclaration {
        toml::from_str(source).expect("presentation declaration")
    }

    #[test]
    fn parses_tools_and_rules_and_scopes_rules_to_the_plugin() {
        let declaration = parse(
            r#"
            [[tools]]
            tool_name = "filesystem.read"
            target_fields = ["path", "file"]

            [[rules]]
            match = { tool = "filesystem.read" }
            set = { sections = { output = { disclosure = "collapsed" } }, indent = 2 }

            [[rules]]
            match = { producer = "bcode.filesystem", kind = "tool" }
            set = { disclosure = "summary" }
            "#,
        );
        assert_eq!(declaration.diagnostics, Vec::<String>::new());
        let effective = declaration.effective("bcode.filesystem", true);
        assert_eq!(effective, declaration);
        assert_eq!(
            effective.target_fields("filesystem.read"),
            Some(&["path".to_owned(), "file".to_owned()][..])
        );
        assert_eq!(effective.target_fields("filesystem.write"), None);

        let layer = effective.rule_layer("bcode.filesystem");
        assert_eq!(
            layer.source,
            LayerSource::Plugin("bcode.filesystem".to_owned())
        );
        assert!(
            layer
                .rules
                .iter()
                .all(|rule| rule.matcher.producer.0 == ["bcode.filesystem".to_owned()])
        );
        assert_eq!(layer.rules[0].set.indent, Some(2));
    }

    #[test]
    fn missing_table_and_empty_declarations_are_valid() {
        let declaration = parse("");
        assert!(declaration.is_empty());
        assert_eq!(declaration.effective("bcode.any", false), declaration);
        assert_eq!(declaration.rule_layer("bcode.any").rules, Vec::new());
    }

    #[test]
    fn undecodable_entries_are_dropped_with_diagnostics_and_the_rest_kept() {
        let declaration = parse(
            r#"
            unknown = 1

            [[tools]]
            tool_name = "a"
            target = ["path"]

            [[tools]]
            tool_name = "b"
            target_fields = ["path"]

            [[rules]]
            match = { tool = "b" }
            set = { motion = "slide" }

            [[rules]]
            match = { tool = "b" }
            set = { disclosure = "folded" }

            [[rules]]
            match = { tool = "b" }
            set = { disclosure = "collapsed" }
            "#,
        );
        assert_eq!(declaration.tools.len(), 1);
        assert_eq!(declaration.tools[0].tool_name, "b");
        assert_eq!(declaration.rules.len(), 1);
        assert_eq!(
            declaration.diagnostics.len(),
            4,
            "{:?}",
            declaration.diagnostics
        );
        assert!(
            declaration
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("unknown presentation key"))
        );

        let not_arrays = parse("tools = 3\nrules = \"x\"");
        assert!(not_arrays.is_empty());
        assert_eq!(not_arrays.diagnostics.len(), 2);
        let not_a_table: PluginPresentationDeclaration =
            serde_json::from_value(serde_json::json!(["x"])).expect("tolerant");
        assert!(not_a_table.is_empty());
        assert_eq!(not_a_table.diagnostics.len(), 1);
    }

    #[test]
    fn rules_for_other_producers_are_dropped() {
        let declaration = parse(
            r#"
            [[rules]]
            match = { producer = ["bcode.filesystem", "bcode.shell"] }
            set = { disclosure = "collapsed" }

            [[rules]]
            set = { disclosure = "summary" }
            "#,
        );
        let effective = declaration.effective("bcode.filesystem", true);
        assert_eq!(effective.rules, declaration.rules[1..]);
        assert_eq!(effective.diagnostics.len(), 1);
    }

    #[test]
    fn invalid_tool_metadata_is_dropped_and_valid_entries_kept() {
        let tool = |name: &str, fields: &[&str]| PluginToolPresentationMetadata {
            tool_name: name.to_owned(),
            target_fields: fields.iter().map(|field| (*field).to_owned()).collect(),
        };
        let valid = tool("valid", &["path"]);
        let invalid = [
            vec![tool(" ", &["path"])],
            vec![tool(&"t".repeat(MAX_PRESENTATION_TOOL_NAME_BYTES + 1), &[])],
            vec![tool("read", &["path", "path"])],
            vec![tool("read", &[""])],
            vec![tool("read", &["has space"])],
            vec![tool("read", &["nested.path"])],
            vec![tool("read", &[&"f".repeat(MAX_TARGET_FIELD_BYTES + 1)])],
            vec![tool("read", &["a", "b", "c", "d", "e", "f", "g", "h", "i"])],
        ];
        for mut tools in invalid {
            tools.push(valid.clone());
            let declaration = PluginPresentationDeclaration {
                tools,
                ..PluginPresentationDeclaration::default()
            };
            let effective = declaration.effective("bcode.plugin", true);
            assert_eq!(
                effective.tools,
                std::slice::from_ref(&valid),
                "{declaration:?}"
            );
            assert_eq!(effective.diagnostics.len(), 1, "{declaration:?}");
        }
        let repeated = PluginPresentationDeclaration {
            tools: vec![tool("read", &["path"]), tool("read", &["file"])],
            ..PluginPresentationDeclaration::default()
        };
        assert_eq!(
            repeated.effective("bcode.plugin", true).tools,
            [tool("read", &["path"])]
        );
        let without_tool_service = repeated.effective("bcode.plugin", false);
        assert_eq!(without_tool_service.tools, Vec::new());
        assert_eq!(without_tool_service.diagnostics.len(), 1);

        let too_many = PluginPresentationDeclaration {
            tools: (0..=MAX_PRESENTATION_TOOLS)
                .map(|index| tool(&format!("tool{index}"), &[]))
                .collect(),
            rules: vec![PresentationRule::default(); MAX_RULES_PER_LAYER + 1],
            diagnostics: Vec::new(),
        };
        let bounded = too_many.effective("bcode.plugin", true);
        assert_eq!(bounded.tools.len(), MAX_PRESENTATION_TOOLS);
        assert_eq!(bounded.rules.len(), MAX_RULES_PER_LAYER);
        assert_eq!(bounded.diagnostics.len(), 2);
        let widest = tool(
            "read",
            &[
                "a",
                "b",
                "c",
                "d",
                "e",
                "f",
                "g",
                &"h".repeat(MAX_TARGET_FIELD_BYTES),
            ],
        );
        let declaration = PluginPresentationDeclaration {
            tools: vec![widest.clone()],
            ..PluginPresentationDeclaration::default()
        };
        assert_eq!(declaration.effective("bcode.plugin", true).tools, [widest]);
    }
}
