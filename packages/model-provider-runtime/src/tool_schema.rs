//! Reversible provider-wire representation of tool input schemas for strict dialects.
//!
//! Strict tool dialects close every object, so a canonical tool schema with a dynamic
//! string-keyed map cannot be sent as is. [`ToolWireSchemas`] gives every tool the most faithful
//! wire schema its dialect accepts and decodes provider tool-call arguments back into the
//! canonical representation before they leave the provider:
//!
//! * A schema the dialect accepts directly is sent strict, unchanged apart from the dialect's own
//!   normalization.
//! * A schema with dynamic maps is sent strict with each map encoded as an array of key/value
//!   records, and the tool's call arguments are decoded back into maps.
//! * A schema the dialect cannot express even with map encoding is sent non-strict in its
//!   canonical form, so one tool cannot make every request fail. The host still validates its
//!   arguments against the canonical schema.
use std::collections::{BTreeMap, BTreeSet};

use bcode_model::{ProviderTurnEvent, ToolCall, ToolDefinition};
use bcode_model_schema::{ObjectMapEncoding, SchemaPortabilityError};
use serde_json::Value;

/// How one tool is presented to a strict provider dialect.
#[derive(Debug, Clone)]
pub struct ToolWireSchema {
    schema: Value,
    strict: bool,
    maps: Option<ObjectMapEncoding>,
    fallback_reason: Option<String>,
}

impl ToolWireSchema {
    /// The provider-wire input schema, already normalized for the dialect when strict.
    #[must_use]
    pub const fn schema(&self) -> &Value {
        &self.schema
    }

    /// Whether the provider may enforce this tool's schema strictly.
    #[must_use]
    pub const fn strict(&self) -> bool {
        self.strict
    }

    /// Why the tool is sent non-strict, when the dialect cannot express its schema.
    #[must_use]
    pub fn fallback_reason(&self) -> Option<&str> {
        self.fallback_reason.as_deref()
    }

    /// Whether call arguments arrive in a wire representation that needs decoding.
    #[must_use]
    pub const fn is_adapted(&self) -> bool {
        self.maps.is_some()
    }
}

/// Wire schemas for every tool of one strict provider request.
#[derive(Debug, Clone, Default)]
pub struct ToolWireSchemas {
    tools: BTreeMap<String, ToolWireSchema>,
}

impl ToolWireSchemas {
    /// Compile wire schemas for `tools` with the dialect normalization `strict`.
    ///
    /// `strict` maps a canonical (or map-encoded) schema to the provider's strict dialect, or
    /// reports why it cannot. Compilation never fails: a tool the dialect cannot express is sent
    /// non-strict with its canonical schema and a [`ToolWireSchema::fallback_reason`].
    pub fn compile(
        tools: &[ToolDefinition],
        strict: impl Fn(&Value) -> Result<Value, SchemaPortabilityError>,
    ) -> Self {
        Self {
            tools: tools
                .iter()
                .map(|tool| (tool.name.clone(), compile_tool(&tool.input_schema, &strict)))
                .collect(),
        }
    }

    /// The wire schema of the tool named `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ToolWireSchema> {
        self.tools.get(name)
    }

    /// Names of tools whose call arguments need decoding.
    #[must_use]
    pub fn adapted_tools(&self) -> BTreeSet<&str> {
        self.tools
            .iter()
            .filter(|(_, tool)| tool.is_adapted())
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Whether any tool's call arguments need decoding.
    #[must_use]
    pub fn is_adapted(&self) -> bool {
        self.tools.values().any(ToolWireSchema::is_adapted)
    }

    /// Decode a provider tool call into the tool's canonical argument representation.
    ///
    /// Calls to tools without an adapted schema are returned unchanged. Arguments that do not
    /// decode (malformed records or duplicate keys) are also returned unchanged so canonical
    /// validation rejects them and the model sees the tool error.
    #[must_use]
    pub fn decode(&self, call: ToolCall) -> ToolCall {
        let Some(maps) = self
            .tools
            .get(&call.name)
            .and_then(|tool| tool.maps.as_ref())
        else {
            return call;
        };
        let wire = bcode_model_schema::omit_null_optional_properties(
            maps.schema(),
            call.arguments.clone(),
        );
        match maps.decode(wire) {
            Ok(arguments) => ToolCall { arguments, ..call },
            Err(_) => call,
        }
    }
}

/// Per-turn decoding of tool-call events made against [`ToolWireSchemas`].
///
/// Calls to adapted tools stream wire-form argument text, so their argument deltas are withheld
/// and the finished call is published with canonical arguments. Calls to other tools pass
/// through unchanged.
#[derive(Debug, Clone, Default)]
pub struct ToolCallDecoder {
    schemas: ToolWireSchemas,
    adapted_calls: BTreeSet<String>,
}

impl ToolCallDecoder {
    /// Decode tool-call events for `schemas`.
    #[must_use]
    pub const fn new(schemas: ToolWireSchemas) -> Self {
        Self {
            schemas,
            adapted_calls: BTreeSet::new(),
        }
    }

    /// Return the event to publish in place of `event`, or `None` to withhold it.
    pub fn filter(&mut self, event: ProviderTurnEvent) -> Option<ProviderTurnEvent> {
        if !self.schemas.is_adapted() {
            return Some(event);
        }
        match event {
            ProviderTurnEvent::ToolCallStarted { call_id, name } => {
                if self
                    .schemas
                    .get(&name)
                    .is_some_and(ToolWireSchema::is_adapted)
                {
                    self.adapted_calls.insert(call_id.clone());
                }
                Some(ProviderTurnEvent::ToolCallStarted { call_id, name })
            }
            ProviderTurnEvent::ToolCallDelta { call_id, .. }
                if self.adapted_calls.contains(&call_id) =>
            {
                None
            }
            ProviderTurnEvent::ToolCallFinished { call } => {
                self.adapted_calls.remove(&call.id);
                Some(ProviderTurnEvent::ToolCallFinished {
                    call: self.schemas.decode(call),
                })
            }
            event => Some(event),
        }
    }
}

fn compile_tool(
    schema: &Value,
    strict: &impl Fn(&Value) -> Result<Value, SchemaPortabilityError>,
) -> ToolWireSchema {
    let direct = match strict(schema) {
        Ok(wire) => {
            return ToolWireSchema {
                schema: wire,
                strict: true,
                maps: None,
                fallback_reason: None,
            };
        }
        Err(error) => error,
    };
    let encoded = ObjectMapEncoding::compile(schema)
        .ok()
        .filter(ObjectMapEncoding::is_adapted)
        .and_then(|maps| strict(maps.schema()).ok().map(|wire| (wire, maps)));
    match encoded {
        Some((wire, maps)) => ToolWireSchema {
            schema: wire,
            strict: true,
            maps: Some(maps),
            fallback_reason: None,
        },
        None => ToolWireSchema {
            schema: schema.clone(),
            strict: false,
            maps: None,
            fallback_reason: Some(direct.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn strict_dialect(schema: &Value) -> Result<Value, SchemaPortabilityError> {
        bcode_model_schema::normalize(
            schema,
            &bcode_model_schema::SchemaDialect {
                object_properties: bcode_model_schema::ObjectPropertyPolicy::RequireAllAndClose,
                ..bcode_model_schema::SchemaDialect::default()
            },
        )
    }

    fn tool(name: &str, input_schema: Value) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: String::new(),
            input_schema,
        }
    }

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "call".into(),
            name: name.into(),
            arguments,
        }
    }

    #[test]
    fn closed_schemas_stay_strict_and_unadapted() {
        let schemas = ToolWireSchemas::compile(
            &[tool(
                "plain",
                json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
            )],
            strict_dialect,
        );
        let plain = schemas.get("plain").unwrap();
        assert!(plain.strict());
        assert!(!plain.is_adapted());
        assert_eq!(plain.schema()["additionalProperties"], false);
        let arguments = json!({"text": "hi"});
        assert_eq!(
            schemas.decode(call("plain", arguments.clone())).arguments,
            arguments
        );
    }

    #[test]
    fn optional_maps_are_encoded_strict_and_decoded_to_canonical() {
        let schemas = ToolWireSchemas::compile(
            &[tool(
                "mapped",
                json!({
                    "type": "object",
                    "properties": {
                        "app": {"type": "string"},
                        "facets": {"type": "object", "additionalProperties": {"type": "string"}}
                    },
                    "required": ["app"],
                    "additionalProperties": false
                }),
            )],
            strict_dialect,
        );
        let mapped = schemas.get("mapped").unwrap();
        assert!(mapped.strict());
        assert!(mapped.is_adapted());
        assert!(strict_dialect(mapped.schema()).is_ok());
        assert_eq!(schemas.adapted_tools(), BTreeSet::from(["mapped"]));
        assert_eq!(
            schemas
                .decode(call(
                    "mapped",
                    json!({"app": "bcode", "facets": [{"key": "a.b", "value": "c"}]}),
                ))
                .arguments,
            json!({"app": "bcode", "facets": {"a.b": "c"}})
        );
        assert_eq!(
            schemas
                .decode(call("mapped", json!({"app": "bcode", "facets": null})))
                .arguments,
            json!({"app": "bcode"})
        );
    }

    #[test]
    fn malformed_encoded_arguments_pass_through_for_canonical_rejection() {
        let schemas = ToolWireSchemas::compile(
            &[tool(
                "mapped",
                json!({"type":"object","properties":{"m":{"type":"object","additionalProperties":{"type":"string"}}}}),
            )],
            strict_dialect,
        );
        let duplicate = json!({"m": [{"key": "a", "value": "1"}, {"key": "a", "value": "2"}]});
        assert_eq!(
            schemas.decode(call("mapped", duplicate.clone())).arguments,
            duplicate
        );
    }

    #[test]
    fn inexpressible_schemas_fall_back_to_non_strict_canonical() {
        let canonical = json!({
            "type": "object",
            "properties": {"fixed": {"type": "string"}},
            "additionalProperties": {"type": "string"}
        });
        let schemas = ToolWireSchemas::compile(
            &[
                tool("mixed", canonical.clone()),
                tool("plain", json!({"type":"object","properties":{}})),
            ],
            strict_dialect,
        );
        let mixed = schemas.get("mixed").unwrap();
        assert!(!mixed.strict());
        assert_eq!(mixed.schema(), &canonical);
        assert!(mixed.fallback_reason().is_some());
        assert!(schemas.get("plain").unwrap().strict());
        let arguments = json!({"fixed": "x", "other": "y"});
        assert_eq!(
            schemas.decode(call("mixed", arguments.clone())).arguments,
            arguments
        );
    }
}
