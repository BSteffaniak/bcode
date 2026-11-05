//! Provider-neutral schemas for compaction's two distinct response contracts.

use super::synthesis::{Rejection, sections};

#[derive(Clone, Copy)]
pub(super) enum ResponseContract {
    Update,
    Replace,
    Consolidation,
    Recollection,
}

impl ResponseContract {
    pub(super) fn validate(self, text: &str) -> Result<(), Rejection> {
        let result = self.validate_inner(text);
        if let Some(context) = bcode_xray::CaptureContext::current() {
            context.emit(
                "context",
                "compaction_response_validated",
                serde_json::json!({
                    "contract": match self {
                        Self::Update => "update", Self::Replace => "replace",
                        Self::Consolidation => "consolidation", Self::Recollection => "recollection",
                    },
                    "accepted": result.is_ok(),
                    "response_bytes": text.len(),
                }),
                false,
            );
            crate::xray_context::sensitive(&context, "compaction_response", text);
        }
        result
    }

    fn validate_inner(self, text: &str) -> Result<(), Rejection> {
        use std::sync::OnceLock;
        static RECOLLECTION: OnceLock<jsonschema::Validator> = OnceLock::new();
        static REPLACE: OnceLock<jsonschema::Validator> = OnceLock::new();
        static UPDATE: OnceLock<jsonschema::Validator> = OnceLock::new();
        static CONSOLIDATION: OnceLock<jsonschema::Validator> = OnceLock::new();
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|error| decode_error(&error))?;
        let slot = match self {
            Self::Recollection => &RECOLLECTION,
            Self::Replace => &REPLACE,
            Self::Update => &UPDATE,
            Self::Consolidation => &CONSOLIDATION,
        };
        let validator = slot.get_or_init(|| {
            jsonschema::validator_for(&self.request().schema)
                .expect("compaction response schema is valid")
        });
        if let Err(error) = validator.validate(&value) {
            // Instance paths can contain attacker-controlled keys. Select only a known schema
            // field from a fixed vocabulary; never render the instance or error's Display text.
            let schema_path = error.schema_path().to_string();
            let fields = [
                "replacement_source_sequence",
                "replacement_statement",
                "source_sequence",
                "supersedes_statements",
                "current_scope",
                "reconciles",
                "supersedes",
                "statement",
                "active_work_blockers",
                "findings_evidence",
                "decisions",
                "background",
                "sections",
                "format",
            ];
            let required = match error.kind() {
                jsonschema::error::ValidationErrorKind::Required { property } => property.as_str(),
                _ => None,
            };
            let instance_path = error.instance_path().to_string();
            let field = fields
                .into_iter()
                .find(|field| {
                    required == Some(*field)
                        || instance_path
                            .split('/')
                            .chain(schema_path.split('/'))
                            .any(|part| part == *field)
                })
                .unwrap_or("response");
            if matches!(
                error.kind(),
                jsonschema::error::ValidationErrorKind::AdditionalProperties { .. }
            ) {
                return Err(Rejection::UnknownField);
            }
            let rule = match error.kind() {
                jsonschema::error::ValidationErrorKind::Required { .. } => "missing required field",
                jsonschema::error::ValidationErrorKind::AdditionalProperties { .. } => {
                    "unknown field"
                }
                jsonschema::error::ValidationErrorKind::Type { .. } => "wrong field type",
                _ => "schema constraint violated",
            };
            return Err(Rejection::Schema { field, rule });
        }
        Ok(())
    }

    pub(super) fn request(self) -> bcode_model::StructuredOutputRequest {
        let (name, schema) = match self {
            Self::Recollection => (
                "compaction_recollection",
                serde_json::to_value(schemars::schema_for!(super::protected::Recollection)),
            ),
            Self::Update | Self::Replace => (
                "compaction_section_update",
                serde_json::to_value(schemars::schema_for!(sections::Update)),
            ),
            Self::Consolidation => (
                "compaction_consolidation",
                serde_json::to_value(schemars::schema_for!(sections::Consolidation)),
            ),
        };
        let mut schema = schema.expect("compaction schema is serializable");
        schema["properties"]["format"]["const"] = serde_json::Value::String(
            match self {
                Self::Recollection => super::protected::RESPONSE_FORMAT,
                Self::Replace => sections::SECTION_REPLACE_FORMAT,
                Self::Update => sections::SECTION_UPDATE_FORMAT,
                Self::Consolidation => sections::CONSOLIDATION_FORMAT,
            }
            .into(),
        );
        bcode_model::StructuredOutputRequest {
            name: name.into(),
            schema,
            strict: true,
        }
    }
}

/// Never include deserializer messages: unknown keys and rejected values can contain secrets.
pub(super) fn decode_error(error: &serde_json::Error) -> Rejection {
    match error.classify() {
        serde_json::error::Category::Data => Rejection::InvalidShape,
        _ => Rejection::JsonSyntax {
            line: error.line(),
            column: error.column(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_provider_default_and_fixture_responses_satisfy_actual_replace_and_consolidation_schemas()
     {
        use bcode_plugin_sdk::ConcurrentRustPlugin as _;
        let provider = bcode_fake_provider_plugin::FakeProviderPlugin::default();
        for (fixture, contract) in [
            (false, ResponseContract::Replace),
            (true, ResponseContract::Replace),
            (false, ResponseContract::Consolidation),
            (true, ResponseContract::Consolidation),
        ] {
            let mut context = bcode_model::ProviderRequestContext::default();
            if fixture {
                context
                    .settings
                    .insert("fake_compaction_fixture".into(), "scope-v1".into());
            }
            let prompt = concat!(
                "Return only JSON with format bcode.compaction.section-update.v2\n",
                "Prior structured synthesis: {\"format\":\"bcode.compaction.scope.v2\",\"current_scope\":[],\"continuation\":\"\"}",
                "\n\nSource fragments (JSON lines):\n",
                "{\"sequence\":1,\"provenance\":\"user\",\"byte_offset\":0,\"final_fragment\":true,\"text\":\"historical padding\"}\n"
            );
            let mut request = super::super::build_compaction_request(
                crate::SessionId::new(),
                "fake-echo".into(),
                context,
                prompt,
                "replace-schema".into(),
            );
            request.structured_output = Some(contract.request());
            let invocation = |operation: &str, payload: Vec<u8>| {
                serde_json::from_value::<bcode_plugin_sdk::NativeServiceContext>(serde_json::json!({
                    "plugin_id":"bcode.fake-provider",
                    "request":{"interface_id":bcode_model::MODEL_PROVIDER_INTERFACE_ID,"operation":operation,"payload":payload}
                })).unwrap()
            };
            let started = provider.invoke_service_concurrent(invocation(
                bcode_model::OP_START_TURN,
                serde_json::to_vec(&request).unwrap(),
            ));
            assert!(started.error.is_none(), "{started:?}");
            let started: bcode_model::StartTurnResponse =
                serde_json::from_slice(&started.payload).unwrap();
            let polled = provider.invoke_service_concurrent(invocation(
                bcode_model::OP_POLL_TURN_EVENTS,
                serde_json::to_vec(&bcode_model::PollTurnEventsRequest {
                    provider_turn_id: started.provider_turn_id,
                })
                .unwrap(),
            ));
            assert!(polled.error.is_none(), "{polled:?}");
            let polled: bcode_model::PollTurnEventsResponse =
                serde_json::from_slice(&polled.payload).unwrap();
            let text: String = polled
                .events
                .iter()
                .filter_map(|event| match event {
                    bcode_model::ProviderTurnEvent::TextDelta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            contract.validate(&text).unwrap();
            // Responses for the other independently selected contract must fail;
            // prompt wording deliberately remains Replace in this matrix.
            let other = match contract {
                ResponseContract::Consolidation => ResponseContract::Replace,
                _ => ResponseContract::Consolidation,
            };
            assert!(other.validate(&text).is_err());
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert!(value.get("continuation").is_none());
            if matches!(contract, ResponseContract::Consolidation) {
                let prior = super::super::CompactionEvidence {
                    format: super::super::COMPACTION_SUMMARY_FORMAT.into(),
                    protected_users: None,
                    current_scope: Vec::new(), continuation: serde_json::json!({
                        "format":"bcode.compaction.continuation-sections.v1", "decisions":"",
                        "active_work_blockers":"", "findings_evidence":"", "background":"Accepted carry must remain exact."
                    }).to_string(),
                };
                let retained = super::super::synthesis::consolidate_response(
                    &text,
                    &prior,
                    super::super::synthesis::SynthesisPolicy {
                        carry_chars: 8_192,
                        wire_bytes: 16_384,
                        section_chars: 8_192,
                    },
                )
                .unwrap();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&retained.continuation).unwrap(),
                    serde_json::from_str::<serde_json::Value>(&prior.continuation).unwrap(),
                );
                assert_eq!(
                    serde_json::to_value(&retained.current_scope).unwrap(),
                    serde_json::to_value(&prior.current_scope).unwrap(),
                );
            }
        }
    }

    #[test]
    fn response_schemas_accept_null_as_unchanged_and_reject_wrong_shapes() {
        for contract in [
            ResponseContract::Update,
            ResponseContract::Replace,
            ResponseContract::Consolidation,
        ] {
            let request = contract.request();
            let validator = jsonschema::validator_for(&request.schema).unwrap();
            let mut response = match contract {
                ResponseContract::Update | ResponseContract::Replace => serde_json::json!({
                    "format": request.schema["properties"]["format"]["const"],
                    "current_scope": [], "reconciles": [], "supersedes_statements": [],
                    "sections": {"decisions": null}
                }),
                ResponseContract::Consolidation | ResponseContract::Recollection => {
                    serde_json::json!({
                        "format": "bcode.compaction.section-consolidation.v1",
                        "sections": {"decisions": null}
                    })
                }
            };
            assert!(validator.is_valid(&response));
            response["sections"]["decisions"] = serde_json::json!(42);
            assert!(!validator.is_valid(&response));
            response["sections"] = serde_json::json!({"secret_unknown_field": "secret"});
            assert!(!validator.is_valid(&response));
        }
    }

    #[test]
    fn schema_diagnostics_identify_safe_fields_and_rules() {
        let contract = ResponseContract::Update;
        let mut value = serde_json::json!({
            "format": sections::SECTION_UPDATE_FORMAT,
            "current_scope": [], "reconciles": [], "supersedes_statements": [],
            "sections": {"decisions": 42}
        });
        assert!(matches!(
            contract.validate(&value.to_string()),
            Err(Rejection::Schema {
                field: "decisions",
                ..
            })
        ));
        value["sections"] = serde_json::json!({});
        value.as_object_mut().unwrap().remove("reconciles");
        assert_eq!(
            contract.validate(&value.to_string()),
            Err(Rejection::Schema {
                field: "reconciles",
                rule: "missing required field"
            })
        );
        value["reconciles"] = serde_json::json!([]);
        value["private_key_secret"] = serde_json::json!("secret material");
        let diagnostic = contract
            .validate(&value.to_string())
            .unwrap_err()
            .guidance();
        assert!(diagnostic.contains("unknown field"));
        assert!(!diagnostic.contains("private_key_secret"));
        assert!(!diagnostic.contains("secret material"));
    }

    #[test]
    fn deserializer_diagnostics_distinguish_syntax_and_shape_without_values() {
        let syntax = serde_json::from_str::<serde_json::Value>("{\n").unwrap_err();
        assert!(matches!(
            decode_error(&syntax),
            Rejection::JsonSyntax { .. }
        ));
        let shape = serde_json::from_str::<sections::Consolidation>(
            r#"{"format":"test","sections":{"secret":"do not publish"}}"#,
        )
        .err()
        .unwrap();
        assert_eq!(decode_error(&shape), Rejection::InvalidShape);
        assert!(!decode_error(&shape).guidance().contains("secret"));
    }
}
