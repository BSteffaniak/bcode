//! Demand-driven exact dependency resolution for source and package compilation.
use super::*;

/// Resource allowances for one compilation, not limits on runtime composition.
#[derive(Debug, Clone, Copy)]
pub struct WorkflowResolutionBudget {
    /// Maximum distinct external definitions loaded.
    pub definitions: usize,
    /// Maximum cumulative encoded definition bytes loaded.
    pub bytes: usize,
}

impl Default for WorkflowResolutionBudget {
    fn default() -> Self {
        Self {
            definitions: 1_000,
            bytes: 16 * 1024 * 1024,
        }
    }
}

/// Collect exact external targets from supported source syntax without lowering calls.
///
/// Plugin payloads, schemas and prompt text are opaque and never interpreted as calls.
/// Package-local calls are intentionally left for package interface resolution.
///
/// # Errors
/// Returns an error for unsupported source profiles, malformed syntax or call contracts.
pub fn workflow_source_dependency_targets(
    source: &str,
    format: WorkflowSourceFormat,
) -> Result<Vec<WorkflowCallTarget>, WorkflowError> {
    let value = decode_workflow_source_value(source, format)?;
    let object = value
        .as_object()
        .ok_or_else(|| authoring_error("source", "workflow source must be an object"))?;
    match (
        object.get("workflow_source_version"),
        object.get("schema_version"),
    ) {
        (Some(version), None)
            if version.as_u64() == Some(u64::from(WORKFLOW_SOURCE_DOCUMENT_VERSION)) =>
        {
            let steps = object
                .get("steps")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| {
                    authoring_error("steps", "workflow source requires a steps array")
                })?;
            if steps.len() > MAX_WORKFLOW_SOURCE_STEPS {
                return Err(authoring_error(
                    "steps",
                    "workflow source step allowance exceeded",
                ));
            }
            let mut targets = Vec::new();
            let mut pending = steps.iter().collect::<Vec<_>>();
            while let Some(operation) = pending.pop() {
                if let Some(call) = operation.get("workflow_call") {
                    let call: WorkflowCallConfiguration = serde_json::from_value(call.clone())
                        .map_err(|_| {
                            authoring_error(
                                "workflow_call",
                                "invalid exact workflow call configuration",
                            )
                        })?;
                    call.validate()?;
                    targets.push(call.target);
                }
                if let Some(nested) = operation
                    .get("fan_out")
                    .and_then(|fan| fan.get("operation"))
                {
                    pending.push(nested);
                }
            }
            Ok(targets)
        }
        (None, Some(_)) => {
            let document: WorkflowAuthoringDocument =
                serde_json::from_value(value).map_err(|_| {
                    authoring_error("source.canonical", "invalid canonical workflow document")
                })?;
            document.validate()?;
            workflow_definition_dependency_targets(&document.definition)
        }
        _ => Err(authoring_error(
            "source.profile",
            "unsupported or ambiguous workflow source profile",
        )),
    }
}

/// Collect external dependencies of a supplied package closure without compiling local calls.
///
/// # Errors
/// Returns an error for invalid manifests, source syntax, or exact call contracts.
pub fn workflow_package_dependency_targets(
    closure: &WorkflowPackageClosure,
) -> Result<Vec<WorkflowCallTarget>, WorkflowError> {
    let mut targets = Vec::new();
    for package in &closure.packages {
        package.manifest.validate()?;
        targets.extend(package.manifest.external_dependencies.values().cloned());
        targets.extend(
            package
                .manifest
                .imports
                .iter()
                .filter_map(|import| import.target.clone()),
        );
        for member in &package.manifest.members {
            targets.extend(workflow_source_dependency_targets(
                &member.source,
                member.format,
            )?);
        }
    }
    Ok(targets)
}

/// Collect exact child calls from a canonical definition.
///
/// # Errors
/// Returns an error for malformed child-call configurations.
pub fn workflow_definition_dependency_targets(
    definition: &WorkflowDefinition,
) -> Result<Vec<WorkflowCallTarget>, WorkflowError> {
    definition
        .nodes
        .iter()
        .filter(|(_, node)| node.kind == NodeKind::WorkflowCall)
        .map(|(id, node)| {
            let call: WorkflowCallConfiguration =
                serde_json::from_value(node.configuration.clone()).map_err(|_| {
                    authoring_error(id, "invalid exact workflow call configuration")
                })?;
            call.validate()?;
            Ok(call.target)
        })
        .collect()
}

/// Resolve only reachable exact definitions, verifying each full immutable identity.
///
/// Lookup must honor its remaining byte allowance before payload loading. Checkpoint is
/// called between reads and node traversal so owners can enforce cancellation/deadlines.
/// Supplied catalog entries are verified too; conflicting references never use key-only reuse.
/// The returned catalog is complete for the requested targets, not a store snapshot.
///
/// # Errors
/// Returns an error for unavailable/conflicting identities, exhausted resource allowances,
/// cancellation, unsupported contracts or lookup failure. No partial catalog is returned.
pub fn resolve_workflow_authoring_dependencies(
    mut catalog: WorkflowAuthoringCatalogSnapshot,
    targets: impl IntoIterator<Item = WorkflowCallTarget>,
    budget: WorkflowResolutionBudget,
    mut lookup: impl FnMut(&WorkflowDefinitionIdentity, usize) -> Result<String, WorkflowError>,
    mut checkpoint: impl FnMut() -> Result<(), WorkflowError>,
) -> Result<WorkflowAuthoringCatalogSnapshot, WorkflowError> {
    catalog.validate()?;
    let mut pending = targets.into_iter().collect::<Vec<_>>();
    let mut resolved = BTreeSet::new();
    let mut remaining = budget.bytes;
    while let Some(target) = pending.pop() {
        checkpoint()?;
        target.validate()?;
        if let WorkflowCallTarget::PackageLocal { reference } = &target {
            let definition = reference.definition(&catalog)?;
            let identity = WorkflowDefinitionIdentity::for_definition(
                reference.workflow_id.clone(),
                definition,
            )?;
            pending.push(WorkflowCallTarget::Definition { identity });
            continue;
        }
        let identity = target.definition_identity()?;
        if !catalog
            .workflow_definitions
            .contains_key(&identity.definition_id)
        {
            if resolved.len() >= budget.definitions {
                return Err(authoring_error(
                    "catalog",
                    "call dependency allowance exceeded",
                ));
            }
            let json = lookup(identity, remaining)?;
            remaining = remaining.checked_sub(json.len()).ok_or_else(|| {
                authoring_error("catalog", "call dependency byte allowance exceeded")
            })?;
            let definition: WorkflowDefinition = serde_json::from_str(&json)
                .map_err(|_| authoring_error("catalog", "invalid exact child definition"))?;
            definition.validate()?;
            catalog
                .workflow_definitions
                .insert(identity.definition_id.clone(), definition);
        }
        let definition = &catalog.workflow_definitions[&identity.definition_id];
        let actual = WorkflowDefinitionIdentity::for_definition(identity.kind.clone(), definition)?;
        if &actual != identity {
            return Err(authoring_error(
                "catalog",
                "exact child identity does not match resolved content",
            ));
        }
        if resolved.insert(identity.definition_id.clone()) {
            if resolved.len() > budget.definitions {
                return Err(authoring_error(
                    "catalog",
                    "call dependency allowance exceeded",
                ));
            }
            checkpoint()?;
            pending.extend(workflow_definition_dependency_targets(definition)?);
        }
    }
    catalog.validate()?;
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_and_ambiguous_source_profiles_fail_closed() {
        for source in [
            r#"{"workflow_source_version":4,"steps":[]}"#,
            r#"{"workflow_source_version":3,"schema_version":1,"steps":[]}"#,
        ] {
            assert!(
                workflow_source_dependency_targets(source, WorkflowSourceFormat::Json).is_err()
            );
        }
    }

    #[test]
    fn opaque_plugin_payloads_are_not_call_dependencies() {
        let source = r#"{"workflow_source_version":3,"steps":[{"id":"plugin","uses":"plugin/action@1","with":{"workflow_call":{"target":"not-a-call"}}}]}"#;
        assert!(
            workflow_source_dependency_targets(source, WorkflowSourceFormat::Json)
                .unwrap()
                .is_empty()
        );
    }

    fn empty_catalog() -> WorkflowAuthoringCatalogSnapshot {
        WorkflowAuthoringCatalogSnapshot {
            version: WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: WorkflowAuthoringCapabilitySummary::from(
                &WorkflowProductionCapabilities::current(),
            ),
            plugins: BTreeSet::new(),
            blocks: BTreeMap::new(),
            node_configuration_schemas: workflow_node_configuration_schemas(),
            workflow_definitions: BTreeMap::new(),
            agent_profiles: BTreeSet::new(),
            authoring_actions: BTreeMap::new(),
        }
    }

    fn gate_definition() -> WorkflowDefinition {
        let source = serde_json::json!({
            "workflow_source_version": WORKFLOW_SOURCE_DOCUMENT_VERSION,
            "workflow_id": "test/dependency", "title": "Dependency",
            "steps": [{"id": "gate", "input": {"schema": {
                "type_name": "test.value/v1", "schema": {"type": "object"}
            }}}]
        });
        lower_workflow_authoring_source(
            &source.to_string(),
            WorkflowSourceFormat::Json,
            &empty_catalog(),
        )
        .expect("gate source")
        .document
        .definition
    }

    #[test]
    fn repeated_exact_calls_load_once_and_conflicting_identities_fail() {
        let definition = gate_definition();
        let identity =
            WorkflowDefinitionIdentity::for_definition("test/dependency", &definition).unwrap();
        let target = WorkflowCallTarget::Definition {
            identity: identity.clone(),
        };
        let json = serde_json::to_string(&definition).unwrap();
        let mut reads = 0;
        let catalog = resolve_workflow_authoring_dependencies(
            empty_catalog(),
            [target.clone(), target],
            WorkflowResolutionBudget {
                definitions: 1,
                bytes: json.len(),
            },
            |_, remaining| {
                assert_eq!(remaining, json.len());
                reads += 1;
                Ok(json.clone())
            },
            || Ok(()),
        )
        .expect("exact reused target");
        assert_eq!(reads, 1);
        assert_eq!(catalog.workflow_definitions.len(), 1);
        let mut conflicting = identity;
        conflicting.kind = "test/other-kind".into();
        assert!(
            resolve_workflow_authoring_dependencies(
                catalog,
                [WorkflowCallTarget::Definition {
                    identity: conflicting
                }],
                WorkflowResolutionBudget::default(),
                |_, _| panic!("present target is verified without reading"),
                || Ok(())
            )
            .is_err()
        );
    }

    #[test]
    fn exhausted_count_and_bytes_do_not_return_partial_catalogs() {
        let definition = gate_definition();
        let target = WorkflowCallTarget::Definition {
            identity: WorkflowDefinitionIdentity::for_definition("test/dependency", &definition)
                .unwrap(),
        };
        assert!(
            resolve_workflow_authoring_dependencies(
                empty_catalog(),
                [target.clone()],
                WorkflowResolutionBudget {
                    definitions: 0,
                    bytes: 1
                },
                |_, _| panic!("count bound precedes read"),
                || Ok(())
            )
            .is_err()
        );
        assert!(
            resolve_workflow_authoring_dependencies(
                empty_catalog(),
                [target],
                WorkflowResolutionBudget {
                    definitions: 1,
                    bytes: 1
                },
                |_, remaining| {
                    assert_eq!(remaining, 1);
                    Ok(serde_json::to_string(&definition).unwrap())
                },
                || Ok(())
            )
            .is_err()
        );
    }

    #[test]
    fn cancellation_precedes_dependency_lookup() {
        let catalog = empty_catalog();
        let target = WorkflowCallTarget::Definition {
            identity: WorkflowDefinitionIdentity {
                kind: "child".into(),
                definition_id: "child".into(),
                definition_version: 1,
            },
        };
        let result = resolve_workflow_authoring_dependencies(
            catalog,
            [target],
            WorkflowResolutionBudget::default(),
            |_, _| panic!("cancelled resolution must not read storage"),
            || Err(authoring_error("computation", "cancelled")),
        );
        assert!(result.is_err());
    }
}
