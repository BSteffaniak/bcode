//! Reversible provider-wire representation of structured output.
//!
//! Provider dialects can require an object root, cannot express dynamic maps in closed objects,
//! and (in strict mode) require every property. [`StructuredOutputWireEncoding`] maps a canonical
//! schema onto a wire schema that every such dialect can express, and decodes provider values back
//! into the canonical representation. [`StructuredOutputCapture`] holds streamed wire text until
//! the turn ends so only canonical output reaches the host.
use bcode_model::{ProviderError, ProviderErrorCategory, ProviderTurnEvent, StopReason};
use serde_json::Value;

/// Upper bound on buffered structured output text.
pub const MAX_STRUCTURED_OUTPUT_WIRE_BYTES: usize = 1024 * 1024;

/// Model guidance for a structured response produced against an encoded wire schema.
pub const STRUCTURED_OUTPUT_WIRE_INSTRUCTION: &str = "For this structured response, follow the wire response schema: when a result envelope is present, put the requested result in its result field. Dynamic JSON maps are represented on the wire as arrays of {\"key\": string, \"value\": value} records. Use unique keys. Use null for an optional field you leave out. The provider adapter restores the original result representation.";

const ENVELOPE_PROPERTY: &str = "result";

/// A compiled, reversible wire representation of a canonical structured-output schema.
#[derive(Debug, Clone)]
pub struct StructuredOutputWireEncoding {
    maps: bcode_model_schema::ObjectMapEncoding,
    enveloped: bool,
    optionals: bool,
}

impl StructuredOutputWireEncoding {
    /// Compile the wire representation of `schema`.
    ///
    /// When `object_root` is true, a schema whose root is not an object is wrapped in an object
    /// with a single required `result` property. Dynamic maps become arrays of key/value records.
    /// The returned [`Self::schema`] is the wire schema before provider dialect normalization;
    /// a strict dialect normalization of it may additionally let optional properties be `null`,
    /// which [`Self::decode`] removes.
    ///
    /// # Errors
    ///
    /// Returns an `invalid_structured_output_schema` error when dynamic maps cannot be encoded
    /// losslessly (see [`bcode_model_schema::ObjectMapEncoding::compile`]).
    pub fn compile(schema: &Value, object_root: bool) -> Result<Self, ProviderError> {
        let enveloped = object_root && schema.get("type").and_then(Value::as_str) != Some("object");
        let mut wire = schema.clone();
        if enveloped {
            let mut envelope = serde_json::json!({
                "type": "object",
                "properties": {ENVELOPE_PROPERTY: wire},
                "required": [ENVELOPE_PROPERTY],
                "additionalProperties": false
            });
            // Local references still resolve from the document root, not the result property.
            for key in ["$schema", "$defs", "definitions"] {
                if let Some(value) = envelope["properties"][ENVELOPE_PROPERTY]
                    .as_object_mut()
                    .and_then(|object| object.remove(key))
                {
                    envelope[key] = value;
                }
            }
            wire = envelope;
        }
        let maps = bcode_model_schema::ObjectMapEncoding::compile(&wire)
            .map_err(|error| invalid_schema(error.to_string()))?;
        let optionals = bcode_model_schema::has_optional_properties(maps.schema());
        Ok(Self {
            maps,
            enveloped,
            optionals,
        })
    }

    /// The wire schema before provider dialect normalization.
    #[must_use]
    pub const fn schema(&self) -> &Value {
        self.maps.schema()
    }

    /// Whether provider values can differ from canonical values and must be decoded.
    ///
    /// This is true when the result is enveloped, maps are encoded, or the schema has optional
    /// properties that a strict dialect lets be `null`.
    #[must_use]
    pub const fn is_adapted(&self) -> bool {
        self.enveloped || self.optionals || self.maps.is_adapted()
    }

    /// Decode a provider value into the canonical representation.
    ///
    /// # Errors
    ///
    /// Returns a message for malformed map records, a missing or extended result envelope, or
    /// excessive nesting. The caller still validates the result against the canonical schema.
    pub fn decode(&self, value: Value) -> Result<Value, String> {
        let value = bcode_model_schema::omit_null_optional_properties(self.maps.schema(), value);
        let value = self.maps.decode(value).map_err(|error| error.to_string())?;
        if !self.enveloped {
            return Ok(value);
        }
        let Value::Object(mut object) = value else {
            return Err("structured result envelope must be an object".into());
        };
        if object.len() != 1 {
            return Err("structured result envelope must contain only result".into());
        }
        object
            .remove(ENVELOPE_PROPERTY)
            .ok_or_else(|| "structured result envelope is missing result".into())
    }

    /// Decode complete provider wire text into canonical JSON text.
    ///
    /// # Errors
    ///
    /// Returns an `invalid_structured_output` error when the text is not JSON or does not decode.
    pub fn decode_text(&self, text: &str) -> Result<String, ProviderError> {
        serde_json::from_str::<Value>(text)
            .map_err(|_| "invalid encoded structured JSON".to_owned())
            .and_then(|value| self.decode(value))
            .and_then(|value| serde_json::to_string(&value).map_err(|error| error.to_string()))
            .map_err(invalid_output)
    }
}

/// Streamed structured-output text held until the provider turn ends.
///
/// Providers publish text deltas as usual; the turn state routes them here while a capture is
/// installed. On a normal end of turn the complete wire text is decoded and published as one
/// canonical text delta. Wire fragments never reach the host.
#[derive(Debug, Clone)]
pub struct StructuredOutputCapture {
    encoding: StructuredOutputWireEncoding,
    text: Result<String, String>,
}

impl StructuredOutputCapture {
    /// Start capturing text for `encoding`.
    #[must_use]
    pub const fn new(encoding: StructuredOutputWireEncoding) -> Self {
        Self {
            encoding,
            text: Ok(String::new()),
        }
    }

    /// Append one streamed text fragment, failing the capture beyond
    /// [`MAX_STRUCTURED_OUTPUT_WIRE_BYTES`].
    pub fn absorb(&mut self, fragment: &str) {
        if let Ok(text) = &mut self.text {
            if text.len().saturating_add(fragment.len()) > MAX_STRUCTURED_OUTPUT_WIRE_BYTES {
                self.text = Err(format!(
                    "encoded structured output exceeds {MAX_STRUCTURED_OUTPUT_WIRE_BYTES} bytes"
                ));
            } else {
                text.push_str(fragment);
            }
        }
    }

    /// Whether `event` ends the capture.
    #[must_use]
    pub const fn ends_with(event: &ProviderTurnEvent) -> bool {
        matches!(
            event,
            ProviderTurnEvent::TurnFinished { .. } | ProviderTurnEvent::Cancelled
        )
    }

    /// The events to publish in place of the terminal `event`.
    ///
    /// * `TurnFinished(EndTurn)`: the decoded canonical text followed by `event`, or an
    ///   `invalid_structured_output` error followed by `TurnFinished(Error)` when decoding fails.
    /// * Any other `TurnFinished`: the captured text unchanged (if any) followed by `event`, so
    ///   tool-call, refusal, truncation and error rounds keep their text.
    /// * `Cancelled`: only `event`; partial wire text is discarded.
    #[must_use]
    pub fn finish(self, event: ProviderTurnEvent) -> Vec<ProviderTurnEvent> {
        match (&event, self.text) {
            (
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                },
                text,
            ) => match text
                .map_err(invalid_output)
                .and_then(|text| self.encoding.decode_text(&text))
            {
                Ok(text) => vec![ProviderTurnEvent::TextDelta { text }, event],
                Err(error) => vec![
                    ProviderTurnEvent::Error { error },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::Error,
                    },
                ],
            },
            (ProviderTurnEvent::TurnFinished { .. }, Ok(text)) if !text.is_empty() => {
                vec![ProviderTurnEvent::TextDelta { text }, event]
            }
            _ => vec![event],
        }
    }
}

fn invalid_schema(message: String) -> ProviderError {
    crate::simple_provider_error(
        "invalid_structured_output_schema",
        ProviderErrorCategory::InvalidRequest,
        message,
    )
}

fn invalid_output(message: String) -> ProviderError {
    crate::simple_provider_error(
        "invalid_structured_output",
        ProviderErrorCategory::InvalidRequest,
        message,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decoded(
        capture: StructuredOutputCapture,
        stop_reason: StopReason,
    ) -> Vec<ProviderTurnEvent> {
        capture.finish(ProviderTurnEvent::TurnFinished { stop_reason })
    }

    #[test]
    fn scalar_map_and_optional_results_round_trip_through_the_wire() {
        for (schema, wire, canonical) in [
            (
                json!({"type": "boolean"}),
                json!({"result": false}),
                json!(false),
            ),
            (
                json!({"type": "object", "properties": {"files": {
                    "type": "object", "additionalProperties": {"type": "string"}
                }}}),
                json!({"files": [{"key": "a", "value": "b"}]}),
                json!({"files": {"a": "b"}}),
            ),
            (
                json!({"type": "object", "required": ["verdict"], "properties": {
                    "verdict": {"type": "string"}, "reason": {"type": "string"}
                }}),
                json!({"verdict": "ok", "reason": null}),
                json!({"verdict": "ok"}),
            ),
        ] {
            let encoding = StructuredOutputWireEncoding::compile(&schema, true).unwrap();
            assert!(encoding.is_adapted());
            assert_eq!(encoding.decode(wire).unwrap(), canonical);
        }
        let exact = json!({"type": "object", "required": ["ok"], "properties": {"ok": {"type": "boolean"}}});
        let encoding = StructuredOutputWireEncoding::compile(&exact, true).unwrap();
        assert!(!encoding.is_adapted());
        assert_eq!(encoding.schema(), &exact);
        assert!(
            !StructuredOutputWireEncoding::compile(&json!({"type": "string"}), false)
                .unwrap()
                .is_adapted()
        );
    }

    #[test]
    fn capture_publishes_only_decoded_text_at_end_of_turn() {
        let schema = json!({"type": "object", "required": ["verdict"], "properties": {
            "verdict": {"type": "string"}, "reason": {"type": "string"}
        }});
        let mut capture = StructuredOutputCapture::new(
            StructuredOutputWireEncoding::compile(&schema, true).unwrap(),
        );
        capture.absorb("{\"verdict\":\"ok\",");
        capture.absorb("\"reason\":null}");

        assert_eq!(
            decoded(capture, StopReason::EndTurn),
            vec![
                ProviderTurnEvent::TextDelta {
                    text: "{\"verdict\":\"ok\"}".to_owned()
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::EndTurn
                }
            ]
        );
    }

    #[test]
    fn capture_reports_undecodable_or_oversized_output_as_an_error() {
        let encoding =
            StructuredOutputWireEncoding::compile(&json!({"type": "string"}), true).unwrap();
        let mut invalid = StructuredOutputCapture::new(encoding.clone());
        invalid.absorb("{\"wrong\":\"x\"}");
        let mut oversized = StructuredOutputCapture::new(encoding);
        oversized.absorb(&"x".repeat(MAX_STRUCTURED_OUTPUT_WIRE_BYTES + 1));

        for capture in [invalid, oversized] {
            let events = decoded(capture, StopReason::EndTurn);
            assert!(matches!(
                events.as_slice(),
                [
                    ProviderTurnEvent::Error { error },
                    ProviderTurnEvent::TurnFinished { stop_reason: StopReason::Error }
                ] if error.code == "invalid_structured_output"
            ));
        }
    }

    #[test]
    fn capture_keeps_text_of_other_terminal_rounds_and_drops_it_on_cancellation() {
        let encoding =
            StructuredOutputWireEncoding::compile(&json!({"type": "string"}), true).unwrap();
        let mut tool_round = StructuredOutputCapture::new(encoding.clone());
        tool_round.absorb("calling a tool");
        assert_eq!(
            decoded(tool_round, StopReason::ToolCall),
            vec![
                ProviderTurnEvent::TextDelta {
                    text: "calling a tool".to_owned()
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::ToolCall
                }
            ]
        );
        let mut cancelled = StructuredOutputCapture::new(encoding);
        cancelled.absorb("{\"res");
        assert_eq!(
            cancelled.finish(ProviderTurnEvent::Cancelled),
            vec![ProviderTurnEvent::Cancelled]
        );
    }
}
