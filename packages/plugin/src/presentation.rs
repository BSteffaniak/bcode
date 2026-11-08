//! Manifest-declared structural presentation defaults for a plugin's own tools.
//!
//! Besides target fields and default rules, a plugin may declare namespaced *facts* (typed values
//! the host reads from a fixed top-level field of its own units' tool arguments or contribution
//! payloads, so rules can match them) and namespaced *properties* (typed presentation settings
//! with defaults that themes and configuration may set for the plugin's units and that its visual
//! adapters receive in their render context). Both are display data: they never affect
//! authorization, dispatch, execution, or persisted session state, and they apply only to units
//! the declaring plugin produced.
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
    FactMatcher, FactType, FactValue, LayerSource, MAX_PLUGIN_DECLARATIONS,
    MAX_PLUGIN_ENUM_VALUE_BYTES, MAX_PLUGIN_ENUM_VALUES, MAX_RULES_PER_LAYER, OneOrMany,
    PluginFactSchema, PluginPresentationSchemas, PluginPropertySchema, PresentationRule,
    PropertyPatch, PropertyType, RuleLayer, valid_plugin_key,
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
/// Maximum UTF-8 bytes of a fact's contribution schema filter.
pub const MAX_FACT_SCHEMA_BYTES: usize = 256;

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
///
/// [[presentation.facts]]
/// name = "bcode.filesystem.lines"
/// type = "int"
/// payload = "lines"
///
/// [[presentation.properties]]
/// name = "bcode.filesystem.show_line_numbers"
/// type = "bool"
/// default = true
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PluginPresentationDeclaration {
    /// Per-tool presentation metadata.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<PluginToolPresentationMetadata>,
    /// Default presentation rules, applied only to this plugin's own units.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<PresentationRule>,
    /// Facts the host derives for this plugin's own units.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<PluginFactDeclaration>,
    /// Presentation properties of this plugin's own units.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub properties: Vec<PluginPropertyDeclaration>,
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

/// A namespaced fact the host derives for the declaring plugin's own units.
///
/// The value is read from one top-level field, of the unit's tool arguments (`argument`) or of
/// its contribution payload (`payload`); exactly one is set. A `bool` fact takes a JSON boolean,
/// an `int` fact a JSON integer or the length of a JSON array or object, and a `string` fact a
/// JSON string bounded to one line. Other values leave the fact unset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginFactDeclaration {
    /// Namespaced name, `<plugin id>.<key>` (see [`valid_plugin_key`]).
    pub name: String,
    /// Value type.
    #[serde(rename = "type")]
    pub value_type: FactType,
    /// Only units of this tool, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Only contribution units with this payload schema (without version), when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Top-level tool argument field holding the value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument: Option<String>,
    /// Top-level contribution payload field holding the value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
}

/// Value kind of a plugin presentation property.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginPropertyKind {
    /// Boolean.
    Bool,
    /// Integer, optionally bounded by `min` and `max`.
    Int,
    /// One of `values`.
    Enum,
}

/// A namespaced presentation property of the declaring plugin's own units.
///
/// Themes and configuration set it like any property (`set = { plugin = { "<name>" = value } }`);
/// values that violate the declaration are ignored with a warning. Visual adapters receive the
/// resolved value in their render context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginPropertyDeclaration {
    /// Namespaced name, `<plugin id>.<key>` (see [`valid_plugin_key`]).
    pub name: String,
    /// Value kind.
    #[serde(rename = "type")]
    pub kind: PluginPropertyKind,
    /// Value used when no rule sets the property.
    pub default: FactValue,
    /// Smallest accepted value of an `int` property.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    /// Largest accepted value of an `int` property.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
    /// Accepted values of an `enum` property, at most [`MAX_PLUGIN_ENUM_VALUES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
}

impl PluginPropertyDeclaration {
    /// The policy schema of this declaration for `plugin_id`, or why it is invalid.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    /// * `min` or `max` is given for a non-`int` property, or `min > max`;
    /// * `values` is given for a non-`enum` property, or an `enum` property has no values, more
    ///   than [`MAX_PLUGIN_ENUM_VALUES`], a blank, overlong, or repeated value;
    /// * `default` does not satisfy the declared type and bounds.
    pub fn schema(&self, plugin_id: &str) -> Result<PluginPropertySchema, String> {
        if self.kind != PluginPropertyKind::Int && (self.min.is_some() || self.max.is_some()) {
            return Err("min and max apply only to int properties".to_owned());
        }
        if self.kind != PluginPropertyKind::Enum && !self.values.is_empty() {
            return Err("values apply only to enum properties".to_owned());
        }
        let value_type = match self.kind {
            PluginPropertyKind::Bool => PropertyType::Bool,
            PluginPropertyKind::Int => {
                let (min, max) = (self.min.unwrap_or(i64::MIN), self.max.unwrap_or(i64::MAX));
                if min > max {
                    return Err("min exceeds max".to_owned());
                }
                PropertyType::Int { min, max }
            }
            PluginPropertyKind::Enum => {
                let mut seen = BTreeSet::new();
                if self.values.is_empty()
                    || self.values.len() > MAX_PLUGIN_ENUM_VALUES
                    || !self.values.iter().all(|value| {
                        !value.trim().is_empty()
                            && value.len() <= MAX_PLUGIN_ENUM_VALUE_BYTES
                            && seen.insert(value.as_str())
                    })
                {
                    return Err(format!(
                        "an enum needs 1..={MAX_PLUGIN_ENUM_VALUES} distinct, non-blank values of \
                         at most {MAX_PLUGIN_ENUM_VALUE_BYTES} bytes"
                    ));
                }
                PropertyType::Enum {
                    values: self.values.clone(),
                }
            }
        };
        if let Some(why) = value_type.reject(&self.default) {
            return Err(format!("invalid default: {why}"));
        }
        Ok(PluginPropertySchema {
            plugin: plugin_id.to_owned(),
            value_type,
            default: self.default.clone(),
        })
    }
}

impl PluginFactDeclaration {
    /// Why this declaration is invalid, if it is (the name is checked separately).
    fn problem(&self) -> Option<&'static str> {
        let ((Some(field), None) | (None, Some(field))) = (&self.argument, &self.payload) else {
            return Some("exactly one of argument and payload must be set");
        };
        if !valid_target_field(field) {
            return Some("its field name is invalid");
        }
        if self.tool.as_ref().is_some_and(|tool| {
            tool.trim().is_empty() || tool.len() > MAX_PRESENTATION_TOOL_NAME_BYTES
        }) {
            return Some("its tool must be non-empty and bounded");
        }
        if self
            .schema
            .as_ref()
            .is_some_and(|schema| schema.trim().is_empty() || schema.len() > MAX_FACT_SCHEMA_BYTES)
        {
            return Some("its schema must be non-empty and bounded");
        }
        None
    }
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
                "facts" => {
                    for (index, value) in
                        declaration.entries("facts", value).into_iter().enumerate()
                    {
                        match serde_json::from_value(value) {
                            Ok(fact) => declaration.facts.push(fact),
                            Err(error) => declaration
                                .diagnose(format!("presentation fact {index} ignored: {error}")),
                        }
                    }
                }
                "properties" => {
                    for (index, value) in declaration
                        .entries("properties", value)
                        .into_iter()
                        .enumerate()
                    {
                        match serde_json::from_value(value) {
                            Ok(property) => declaration.properties.push(property),
                            Err(error) => declaration.diagnose(format!(
                                "presentation property {index} ignored: {error}"
                            )),
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
        self.tools.is_empty()
            && self.rules.is_empty()
            && self.facts.is_empty()
            && self.properties.is_empty()
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

    /// Declared property defaults as a plugin layer scoped to `plugin_id`'s own units, or `None`
    /// when no property is declared.
    ///
    /// The layer holds one rule setting every property to its default; it is stacked below the
    /// plugin's own rules (see [`presentation_rule_layers`]), so every layer above may override
    /// it. Use the properties of [`Self::effective`], whose defaults are valid.
    #[must_use]
    pub fn default_layer(&self, plugin_id: &str) -> Option<RuleLayer> {
        (!self.properties.is_empty()).then(|| RuleLayer {
            source: LayerSource::Plugin(plugin_id.to_owned()),
            rules: vec![PresentationRule {
                matcher: FactMatcher {
                    producer: OneOrMany(vec![plugin_id.to_owned()]),
                    ..FactMatcher::default()
                },
                set: PropertyPatch {
                    plugin: self
                        .properties
                        .iter()
                        .map(|property| (property.name.clone(), property.default.clone()))
                        .collect(),
                    ..PropertyPatch::default()
                },
            }],
        })
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
    /// * a rule's `producer` condition names any plugin other than `plugin_id`;
    /// * they exceed [`MAX_PLUGIN_DECLARATIONS`] facts or properties;
    /// * a fact or property name is not namespaced under `plugin_id` (see [`valid_plugin_key`])
    ///   or is repeated (the first entry is kept);
    /// * a fact does not set exactly one valid `argument` or `payload` field, or has a blank or
    ///   overlong `tool` or `schema`;
    /// * a property's bounds, values, or default are invalid (see
    ///   [`PluginPropertyDeclaration::schema`]).
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
        effective.effective_facts(&self.facts, plugin_id);
        effective.effective_properties(&self.properties, plugin_id);
        effective
    }

    fn effective_facts(&mut self, facts: &[PluginFactDeclaration], plugin_id: &str) {
        if facts.len() > MAX_PLUGIN_DECLARATIONS {
            self.diagnose(format!(
                "presentation facts beyond the first {MAX_PLUGIN_DECLARATIONS} ignored"
            ));
        }
        let mut names = BTreeSet::new();
        for fact in facts.iter().take(MAX_PLUGIN_DECLARATIONS) {
            let reason = if !valid_plugin_key(&fact.name, plugin_id) {
                Some("its name must be namespaced under the plugin id")
            } else if names.contains(fact.name.as_str()) {
                Some("it is declared more than once")
            } else {
                fact.problem()
            };
            if let Some(reason) = reason {
                self.diagnose(format!(
                    "presentation fact {:?} ignored: {reason}",
                    fact.name
                ));
            } else {
                names.insert(fact.name.as_str());
                self.facts.push(fact.clone());
            }
        }
    }

    fn effective_properties(&mut self, properties: &[PluginPropertyDeclaration], plugin_id: &str) {
        if properties.len() > MAX_PLUGIN_DECLARATIONS {
            self.diagnose(format!(
                "presentation properties beyond the first {MAX_PLUGIN_DECLARATIONS} ignored"
            ));
        }
        let mut names = BTreeSet::new();
        for property in properties.iter().take(MAX_PLUGIN_DECLARATIONS) {
            let reason = if !valid_plugin_key(&property.name, plugin_id) {
                Some("its name must be namespaced under the plugin id".to_owned())
            } else if names.contains(property.name.as_str()) {
                Some("it is declared more than once".to_owned())
            } else {
                property.schema(plugin_id).err()
            };
            if let Some(reason) = reason {
                self.diagnose(format!(
                    "presentation property {:?} ignored: {reason}",
                    property.name
                ));
            } else {
                names.insert(property.name.as_str());
                self.properties.push(property.clone());
            }
        }
    }
}

/// Default rules of `declarations` (plugin id to declaration) as presentation-policy layers.
///
/// Plugins are stacked in plugin-id order. A plugin that declares properties first gets a
/// [`LayerSource::Plugin`] layer holding their defaults (see
/// [`PluginPresentationDeclaration::default_layer`]); a plugin with rules then gets one layer of
/// them (see [`PluginPresentationDeclaration::rule_layer`]). Every layer is scoped to that
/// plugin's own units.
#[must_use]
pub fn presentation_rule_layers(
    declarations: &BTreeMap<String, PluginPresentationDeclaration>,
) -> Vec<RuleLayer> {
    declarations
        .iter()
        .flat_map(|(plugin_id, declaration)| {
            declaration
                .default_layer(plugin_id)
                .into_iter()
                .chain((!declaration.rules.is_empty()).then(|| declaration.rule_layer(plugin_id)))
        })
        .collect()
}

/// Facts and properties declared by `declarations`, for the presentation-policy registry.
///
/// Entries whose properties fail [`PluginPropertyDeclaration::schema`] are skipped; use effective
/// declarations, which contain none. When plugin ids nest (`bcode` and `bcode.git`), a name
/// belongs to the plugin with the longest id it is namespaced under, so plugin `bcode` cannot
/// declare `bcode.git.*` names.
#[must_use]
pub fn presentation_schemas(
    declarations: &BTreeMap<String, PluginPresentationDeclaration>,
) -> PluginPresentationSchemas {
    let owns = |plugin_id: &str, name: &str| namespace_owner(declarations, name) == Some(plugin_id);
    let mut schemas = PluginPresentationSchemas::default();
    for (plugin_id, declaration) in declarations {
        for fact in declaration
            .facts
            .iter()
            .filter(|fact| owns(plugin_id, &fact.name))
        {
            schemas.facts.insert(
                fact.name.clone(),
                PluginFactSchema {
                    plugin: plugin_id.clone(),
                    value_type: fact.value_type,
                },
            );
        }
        for property in declaration
            .properties
            .iter()
            .filter(|property| owns(plugin_id, &property.name))
        {
            if let Ok(schema) = property.schema(plugin_id) {
                schemas.properties.insert(property.name.clone(), schema);
            }
        }
    }
    schemas
}

/// The declaring plugin with the longest id that `name` is namespaced under.
fn namespace_owner<'a>(
    declarations: &'a BTreeMap<String, PluginPresentationDeclaration>,
    name: &str,
) -> Option<&'a str> {
    declarations
        .keys()
        .filter(|plugin_id| {
            name.strip_prefix(plugin_id.as_str())
                .is_some_and(|rest| rest.starts_with('.'))
        })
        .max_by_key(|plugin_id| plugin_id.len())
        .map(String::as_str)
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
            ..PluginPresentationDeclaration::default()
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

    /// Git declarations: two valid facts, three invalid ones, three valid properties, and four
    /// invalid ones (the last does not decode).
    const GIT_DECLARATIONS: &str = r#"
            [[facts]]
            name = "bcode.git.files"
            type = "int"
            payload = "files"

            [[facts]]
            name = "bcode.git.branch"
            type = "string"
            tool = "git.clone"
            argument = "branch"

            [[facts]]
            name = "bcode.shell.code"
            type = "int"
            payload = "code"

            [[facts]]
            name = "bcode.git.both"
            type = "bool"
            payload = "a"
            argument = "b"

            [[facts]]
            name = "bcode.git.files"
            type = "bool"
            payload = "dup"

            [[properties]]
            name = "bcode.git.show_stats"
            type = "bool"
            default = true

            [[properties]]
            name = "bcode.git.max_files"
            type = "int"
            default = 5
            min = 1
            max = 20

            [[properties]]
            name = "bcode.git.style"
            type = "enum"
            default = "short"
            values = ["short", "long"]

            [[properties]]
            name = "bcode.git.bad_default"
            type = "int"
            default = 50
            max = 20

            [[properties]]
            name = "bcode.git.bad_enum"
            type = "enum"
            default = "x"
            values = []

            [[properties]]
            name = "bcode.git.misplaced"
            type = "bool"
            default = false
            min = 0

            [[properties]]
            name = "bcode.git.oops"
            type = "color"
            default = "red"
            "#;

    #[test]
    fn facts_and_properties_decode_validate_and_feed_schemas_and_default_layers() {
        let declaration = parse(GIT_DECLARATIONS);
        assert_eq!(
            declaration.diagnostics.len(),
            1,
            "{:?}",
            declaration.diagnostics
        );
        let effective = declaration.effective("bcode.git", true);
        assert_eq!(
            effective
                .facts
                .iter()
                .map(|fact| fact.name.as_str())
                .collect::<Vec<_>>(),
            ["bcode.git.files", "bcode.git.branch"]
        );
        assert_eq!(
            effective
                .properties
                .iter()
                .map(|property| property.name.as_str())
                .collect::<Vec<_>>(),
            [
                "bcode.git.show_stats",
                "bcode.git.max_files",
                "bcode.git.style"
            ]
        );
        // One decode diagnostic plus three dropped facts and three dropped properties.
        assert_eq!(
            effective.diagnostics.len(),
            7,
            "{:?}",
            effective.diagnostics
        );

        let declarations = BTreeMap::from([("bcode.git".to_owned(), effective)]);
        let schemas = presentation_schemas(&declarations);
        assert_eq!(schemas.facts["bcode.git.files"].value_type, FactType::Int);
        assert_eq!(schemas.facts["bcode.git.branch"].plugin, "bcode.git");
        assert_eq!(
            schemas.properties["bcode.git.max_files"].value_type,
            PropertyType::Int { min: 1, max: 20 }
        );

        // Defaults stack below the plugin's own rules, scoped to its units.
        let layers = presentation_rule_layers(&declarations);
        assert_eq!(layers.len(), 1, "no rules, so only the defaults layer");
        let defaults = &layers[0].rules[0];
        assert_eq!(defaults.matcher.producer.0, ["bcode.git".to_owned()]);
        assert_eq!(
            defaults.set.plugin,
            BTreeMap::from([
                ("bcode.git.max_files".to_owned(), FactValue::Int(5)),
                ("bcode.git.show_stats".to_owned(), FactValue::Bool(true)),
                (
                    "bcode.git.style".to_owned(),
                    FactValue::Text("short".to_owned())
                ),
            ])
        );
        let mut with_rules = declarations;
        with_rules
            .get_mut("bcode.git")
            .expect("declared")
            .rules
            .push(
                parse("[[rules]]\nset = { disclosure = \"summary\" }")
                    .rules
                    .remove(0),
            );
        let layers = presentation_rule_layers(&with_rules);
        assert_eq!(layers.len(), 2);
        assert!(layers[0].rules[0].set.disclosure.is_none());
        assert!(layers[1].rules[0].set.disclosure.is_some());
    }

    #[test]
    fn nested_plugin_ids_cannot_claim_each_others_names() {
        let outer = parse(
            r#"
            [[facts]]
            name = "bcode.git.files"
            type = "bool"
            payload = "files"

            [[facts]]
            name = "bcode.mode"
            type = "string"
            payload = "mode"

            [[properties]]
            name = "bcode.git.show_stats"
            type = "int"
            default = 1
            "#,
        )
        .effective("bcode", true);
        let inner = parse(GIT_DECLARATIONS).effective("bcode.git", true);
        let declarations =
            BTreeMap::from([("bcode".to_owned(), outer), ("bcode.git".to_owned(), inner)]);
        let schemas = presentation_schemas(&declarations);
        assert_eq!(schemas.facts["bcode.git.files"].plugin, "bcode.git");
        assert_eq!(schemas.facts["bcode.git.files"].value_type, FactType::Int);
        assert_eq!(schemas.facts["bcode.mode"].plugin, "bcode");
        assert_eq!(
            schemas.properties["bcode.git.show_stats"].plugin,
            "bcode.git"
        );
        assert_eq!(
            schemas.properties["bcode.git.show_stats"].value_type,
            PropertyType::Bool
        );
    }

    #[test]
    fn fact_and_property_counts_are_bounded() {
        let fact = |index: usize| PluginFactDeclaration {
            name: format!("bcode.git.f{index}"),
            value_type: FactType::Bool,
            tool: None,
            schema: None,
            argument: Some("x".to_owned()),
            payload: None,
        };
        let property = |index: usize| PluginPropertyDeclaration {
            name: format!("bcode.git.p{index}"),
            kind: PluginPropertyKind::Bool,
            default: FactValue::Bool(true),
            min: None,
            max: None,
            values: Vec::new(),
        };
        let declaration = PluginPresentationDeclaration {
            facts: (0..=MAX_PLUGIN_DECLARATIONS).map(fact).collect(),
            properties: (0..=MAX_PLUGIN_DECLARATIONS).map(property).collect(),
            ..PluginPresentationDeclaration::default()
        };
        let effective = declaration.effective("bcode.git", false);
        assert_eq!(effective.facts.len(), MAX_PLUGIN_DECLARATIONS);
        assert_eq!(effective.properties.len(), MAX_PLUGIN_DECLARATIONS);
        assert_eq!(effective.diagnostics.len(), 2);
    }
}
