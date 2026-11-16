//! `OpenAI` strict JSON Schema dialect shared by Responses and Chat Completions integrations.

/// Upper bound on the size of a schema after local references are inlined.
const MAX_INLINED_SCHEMA_BYTES: usize = 1024 * 1024;

/// The `OpenAI` strict-mode JSON Schema dialect.
///
/// Strict mode requires every declared property and closes every object, so optional properties
/// accept `null` on the wire; [`bcode_model_schema::omit_null_optional_properties`] restores the
/// canonical value. `default` and Rust integer/float `format` annotations are removed, annotated
/// constant `oneOf` unions collapse to `enum`, and annotations beside `$ref` are dropped.
#[must_use]
pub fn strict_schema_dialect() -> bcode_model_schema::SchemaDialect {
    bcode_model_schema::SchemaDialect {
        unsupported_keywords: std::collections::BTreeMap::from([(
            "default".to_owned(),
            bcode_model_schema::UnsupportedKeywordPolicy::Remove,
        )]),
        unsupported_formats: [
            "uint8", "uint16", "uint32", "uint64", "uint128", "int8", "int16", "int32", "int64",
            "int128", "float", "double",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        object_properties: bcode_model_schema::ObjectPropertyPolicy::RequireAllAndClose,
        one_of: bcode_model_schema::OneOfPolicy::CollapseAnnotatedConstants,
        reference_siblings:
            bcode_model_schema::ReferenceSiblingPolicy::RemoveAnnotationsRejectSemantic,
        ..bcode_model_schema::SchemaDialect::default()
    }
}

/// Normalize a canonical JSON Schema into the `OpenAI` strict dialect with local references
/// inlined.
///
/// # Errors
///
/// Returns an error when the schema is not a JSON object, cannot be represented in the strict
/// dialect (see [`bcode_model_schema::normalize`]), or exceeds the inlining size bound.
pub fn strict_schema(
    schema: &serde_json::Value,
) -> Result<serde_json::Value, bcode_model_schema::SchemaPortabilityError> {
    if !schema.is_object() {
        return Err(bcode_model_schema::SchemaPortabilityError::new(
            "",
            "strict schema must be a JSON object",
        ));
    }
    bcode_model_schema::normalize(schema, &strict_schema_dialect()).and_then(|schema| {
        bcode_model_schema::inline_local_references(&schema, MAX_INLINED_SCHEMA_BYTES)
    })
}
