//! Provider-neutral schemas for compaction's two distinct response contracts.

use super::synthesis::{Rejection, sections};

#[derive(Clone, Copy)]
pub(super) enum ResponseContract {
    Update,
    Consolidation,
}

impl ResponseContract {
    pub(super) fn validate(self, text: &str) -> Result<(), Rejection> {
        use std::sync::OnceLock;
        static UPDATE: OnceLock<jsonschema::Validator> = OnceLock::new();
        static CONSOLIDATION: OnceLock<jsonschema::Validator> = OnceLock::new();
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|error| decode_error(&error))?;
        let slot = match self {
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
            Self::Update => (
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
    fn response_schemas_accept_null_as_unchanged_and_reject_wrong_shapes() {
        for contract in [ResponseContract::Update, ResponseContract::Consolidation] {
            let request = contract.request();
            let validator = jsonschema::validator_for(&request.schema).unwrap();
            let mut response = match contract {
                ResponseContract::Update => serde_json::json!({
                    "format": sections::SECTION_UPDATE_FORMAT,
                    "current_scope": [], "reconciles": [], "supersedes_statements": [],
                    "sections": {"decisions": null}
                }),
                ResponseContract::Consolidation => serde_json::json!({
                    "format": "bcode.compaction.section-consolidation.v1",
                    "sections": {"decisions": null}
                }),
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
