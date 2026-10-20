//! Package-local interface contracts and finite recursive dependency analysis.
use super::*;

/// An unresolved executable reference within a package publication.
///
/// This is not an executable identity. Admission must resolve `member_id` against
/// the calling run's immutable lock and compare both interfaces before allocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageLocalCall {
    /// Compatibility boundary for package-local interface references.
    pub version: u32,
    /// Member selected from the parent's immutable package lock.
    pub member_id: String,
    /// Logical member definition name; never an executable content hash.
    pub workflow_id: String,
    /// Exact child input contract declared before executable compilation.
    pub input: ValueSchema,
    /// Exact child output contract declared before executable compilation.
    pub output: ValueSchema,
}

impl WorkflowPackageLocalCall {
    /// Validate the supported representation and declared interfaces.
    ///
    /// # Errors
    /// Rejects unsupported versions, invalid member identities or invalid schemas.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.version != 1 {
            return Err(authoring_error(
                "package_call.version",
                "unsupported package-local call version",
            ));
        }
        validate_authoring_id("package_call.member_id", &self.member_id)?;
        validate_authoring_id("package_call.workflow_id", &self.workflow_id)?;
        validate_runtime_value_schema("package_call.input", &self.input)?;
        validate_runtime_value_schema("package_call.output", &self.output)
    }

    /// Find the package-local definition for finite advisory policy analysis.
    ///
    /// # Errors
    /// Rejects missing or ambiguous local definitions and incompatible interfaces.
    pub fn definition<'a>(
        &self,
        catalog: &'a WorkflowAuthoringCatalogSnapshot,
    ) -> Result<&'a WorkflowDefinition, WorkflowError> {
        self.validate()?;
        let prefix = format!("{}@", self.workflow_id);
        let mut matches = catalog
            .workflow_definitions
            .iter()
            .filter(|(key, definition)| {
                key.starts_with(&prefix)
                    && definition.input == self.input
                    && definition.output == self.output
            });
        let (_, definition) = matches.next().ok_or_else(|| {
            authoring_error("package_call", "local compiled interface is unavailable")
        })?;
        if matches.next().is_some() {
            return Err(authoring_error(
                "package_call",
                "local compiled definition is ambiguous",
            ));
        }
        Ok(definition)
    }

    /// Verify an immutable lock selection and its exact compiled content.
    ///
    /// # Errors
    /// Rejects invalid contracts, missing members, content drift or interface mismatch.
    pub fn resolve(
        &self,
        lock: &WorkflowPackageLock,
        definition: &WorkflowDefinition,
    ) -> Result<WorkflowDefinitionIdentity, WorkflowError> {
        self.validate()?;
        lock.validate()?;
        let member = lock
            .members
            .iter()
            .find(|member| member.member_id == self.member_id)
            .ok_or_else(|| {
                authoring_error(
                    "package_call.member_id",
                    "member is absent from the immutable parent lock",
                )
            })?;
        let identity = WorkflowDefinitionIdentity::for_definition(
            member.definition_identity.kind.clone(),
            definition,
        )?;
        if identity != member.definition_identity
            || identity.kind != self.workflow_id
            || definition.input != self.input
            || definition.output != self.output
        {
            return Err(authoring_error(
                "package_call",
                "locked executable identity or declared interface differs",
            ));
        }
        Ok(identity)
    }
}

/// Collect explicit interfaces for local references before compiling any member.
///
/// Recursive references require declared boundaries; nonrecursive legacy members can
/// retain inference when no other member needs their interface before lowering.
///
/// # Errors
/// Rejects invalid sources, missing members or undeclared referenced boundaries.
pub fn workflow_package_local_interfaces(
    manifest: &WorkflowPackageManifest,
) -> Result<BTreeMap<String, WorkflowPackageLocalCall>, WorkflowError> {
    let members = manifest
        .members
        .iter()
        .map(|member| (member.member_id.as_str(), member))
        .collect::<BTreeMap<_, _>>();
    if members.len() != manifest.members.len() {
        return Err(authoring_error(
            "package.members",
            "duplicate member identity",
        ));
    }
    let referenced = manifest
        .members
        .iter()
        .flat_map(|member| member.dependencies.iter())
        .collect::<BTreeSet<_>>();
    let mut interfaces = BTreeMap::new();
    for id in referenced {
        let member = members.get(id.as_str()).ok_or_else(|| {
            authoring_error("package.members.dependencies", "local member is missing")
        })?;
        let value = decode_workflow_source_value(&member.source, member.format)
            .map_err(|error| qualify_workflow_package_member_error(member, error))?;
        let workflow_id = value
            .get("workflow_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                qualify_workflow_package_member_error(
                    member,
                    authoring_error("workflow_id", "member workflow identity is required"),
                )
            })?
            .to_string();
        let interface = if value.get("schema_version").is_some() {
            let document: WorkflowAuthoringDocument =
                serde_json::from_value(value).map_err(|_| {
                    authoring_error("package.members.source", "invalid canonical member source")
                })?;
            document.validate()?;
            WorkflowPackageSourceInterface {
                input: Some(document.definition.input),
                output: Some(document.definition.output),
            }
        } else {
            WorkflowPackageSourceInterface::from_source(&value)?
        };
        let call = WorkflowPackageLocalCall {
            version: 1,
            member_id: id.clone(),
            workflow_id,
            input: interface.input.ok_or_else(|| {
                authoring_error(
                    "package.members.input",
                    "referenced local members require an explicit input interface",
                )
            })?,
            output: interface.output.ok_or_else(|| {
                authoring_error(
                    "package.members.output",
                    "referenced local members require an explicit output interface",
                )
            })?,
        };
        call.validate()?;
        interfaces.insert(id.clone(), call);
    }
    Ok(interfaces)
}

/// Rewrite declared local calls to interface references without executable child hashes.
///
/// The canonical call target owner must recognize `package_local` and resolve its
/// reference at admission. This function never reads publication state or grants authority.
///
/// # Errors
/// Rejects undeclared, unavailable or conflicting local calls and invalid source syntax.
pub fn lower_workflow_package_local_references(
    member: &WorkflowPackageMember,
    interfaces: &BTreeMap<String, WorkflowPackageLocalCall>,
) -> Result<serde_json::Value, WorkflowError> {
    let mut source = decode_workflow_source_value(&member.source, member.format)?;
    let Some(steps) = source
        .get_mut("steps")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return Ok(source);
    };
    for step in steps {
        lower_local_operation(step, member, interfaces)?;
    }
    Ok(source)
}

fn lower_local_operation(
    operation: &mut serde_json::Value,
    member: &WorkflowPackageMember,
    interfaces: &BTreeMap<String, WorkflowPackageLocalCall>,
) -> Result<(), WorkflowError> {
    if let Some(call) = operation.get("package_call") {
        let call: WorkflowPackageSourceCall = serde_json::from_value(call.clone())
            .map_err(|_| authoring_error("package_call", "invalid package call reference"))?;
        if let WorkflowPackageSourceCall::Member(id) = call {
            if !member.dependencies.contains(&id) {
                return Err(authoring_error(
                    "package_call.member",
                    "local call is not a declared direct dependency",
                ));
            }
            if operation.get("workflow_call").is_some() {
                return Err(authoring_error(
                    "package_call",
                    "local call conflicts with explicit workflow call",
                ));
            }
            let reference = interfaces.get(&id).ok_or_else(|| {
                authoring_error(
                    "package_call.member",
                    "declared local interface is unavailable",
                )
            })?;
            reference.validate()?;
            let object = operation
                .as_object_mut()
                .ok_or_else(|| authoring_error("package_call", "operation must be an object"))?;
            object.remove("package_call");
            object.insert(
                "workflow_call".into(),
                serde_json::json!({
                    "version": WORKFLOW_CALL_VERSION,
                    "target": {"kind":"package_local", "reference":reference}
                }),
            );
        }
    }
    if let Some(nested) = operation
        .get_mut("fan_out")
        .and_then(|fan| fan.get_mut("operation"))
    {
        lower_local_operation(nested, member, interfaces)?;
    }
    Ok(())
}

/// Plan members independently using local interfaces, then analyze the finite compiled catalog.
///
/// # Errors
/// Rejects invalid manifests/interfaces, unresolved imports, incompatible compilation or locks.
pub fn plan_workflow_package_recursive(
    manifest: &WorkflowPackageManifest,
    catalog: &WorkflowAuthoringCatalogSnapshot,
) -> Result<WorkflowPackagePlan, WorkflowError> {
    plan_recursive_with_checkpoint(manifest, catalog, &mut || Ok(()))
}

/// Internal checkpoint-aware planner; the private module keeps this out of the public API.
pub fn plan_recursive_with_checkpoint(
    manifest: &WorkflowPackageManifest,
    catalog: &WorkflowAuthoringCatalogSnapshot,
    checkpoint: &mut dyn FnMut() -> Result<(), WorkflowError>,
) -> Result<WorkflowPackagePlan, WorkflowError> {
    checkpoint()?;
    manifest.validate()?;
    checkpoint()?;
    catalog.validate()?;
    let interfaces = workflow_package_local_interfaces(manifest)?;
    let external = manifest
        .external_dependencies
        .iter()
        .map(|(id, target)| (id.clone(), target.clone()))
        .chain(manifest.imports.iter().filter_map(|import| {
            import
                .target
                .clone()
                .map(|target| (import.import_id.clone(), target))
        }))
        .collect::<BTreeMap<_, _>>();
    if manifest
        .imports
        .iter()
        .any(|import| import.target.is_none())
    {
        return Err(authoring_error(
            "package.imports",
            "unresolved imports require closure planning",
        ));
    }
    let mut lowered = Vec::new();
    let mut resolved = catalog.clone();
    for member in &manifest.members {
        checkpoint()?;
        let mut value = lower_workflow_package_local_references(member, &interfaces)
            .map_err(|error| qualify_workflow_package_member_error(member, error))?;
        resolve_package_calls(&mut value, member, &BTreeMap::new(), &external)?;
        let encoded = serde_json::to_string(&value)
            .map_err(|_| authoring_error("package.source", "member encoding failed"))?;
        let validated = lower_workflow_authoring_source_for_compilation(
            &encoded,
            WorkflowSourceFormat::Json,
            &resolved,
        )
        .map_err(|error| qualify_workflow_package_member_error(member, error))?;
        let profile = validated.result.profile;
        // Retain source validation, as the inferred planner does. Lock digests below
        // describe the materialized document, not this pre-compilation report.
        let validation = validated.result.validation;
        let mut document = validated.result.document;
        checkpoint()?;
        let definition = prepare_workflow_package_definition(&document)?;
        document.definition = definition;
        let identity = WorkflowDefinitionIdentity::for_definition(
            &document.workflow_id,
            &document.definition,
        )?;
        resolved
            .workflow_definitions
            .insert(identity.definition_id.clone(), document.definition.clone());
        lowered.push(RecursivePackageLoweredMember {
            member,
            profile,
            validation,
            source_map: validated.result.source_map,
            document,
            identity,
        });
    }
    compile_recursive_package_members(manifest, lowered, &resolved, checkpoint)
}

struct RecursivePackageLoweredMember<'a> {
    member: &'a WorkflowPackageMember,
    profile: WorkflowSourceProfile,
    validation: WorkflowValidationReport,
    source_map: WorkflowSourceMap,
    document: WorkflowAuthoringDocument,
    identity: WorkflowDefinitionIdentity,
}

fn compile_recursive_package_members(
    manifest: &WorkflowPackageManifest,
    lowered: Vec<RecursivePackageLoweredMember<'_>>,
    resolved: &WorkflowAuthoringCatalogSnapshot,
    checkpoint: &mut dyn FnMut() -> Result<(), WorkflowError>,
) -> Result<WorkflowPackagePlan, WorkflowError> {
    let mut planned = Vec::new();
    let mut locked = Vec::new();
    for RecursivePackageLoweredMember {
        member,
        profile,
        validation,
        source_map,
        document,
        identity,
    } in lowered
    {
        checkpoint()?;
        let preview = document.compilation_preview(resolved, None);
        let compiled = preview.compiled.ok_or_else(|| {
            authoring_error("package.members", "recursive member compilation failed")
        })?;
        if compiled.definition_identity != identity {
            return Err(authoring_error(
                "package.members",
                "member configuration changes independently compiled identity",
            ));
        }
        let closure = workflow_package_recursive_closure(
            manifest,
            &member.member_id,
            manifest.members.len(),
            &mut *checkpoint,
        )?;
        locked.push(WorkflowPackageLockedMember {
            member_id: member.member_id.clone(),
            source_digest_sha256: document.source_digest_sha256()?,
            executable_digest_sha256: document.executable_source_digest_sha256()?,
            definition_identity: identity.clone(),
            published_revision: None,
            dependency_closure: closure.clone(),
        });
        let lowering = WorkflowSourceLoweringResult {
            version: WORKFLOW_SOURCE_LOWERING_VERSION,
            profile,
            validation,
            document,
            source_map: source_map.clone(),
        };
        planned.push(WorkflowPackagePlannedMember {
            member_id: member.member_id.clone(),
            member_source_map: WorkflowPackageMemberSourceMap {
                version: WORKFLOW_PACKAGE_MEMBER_SOURCE_MAP_VERSION,
                member_id: member.member_id.clone(),
                source_name: member.source_name.clone(),
                source_map,
            },
            lowering,
            definition_identity: identity,
            dependency_closure: closure,
        });
    }
    planned.sort_by(|a, b| a.member_id.cmp(&b.member_id));
    locked.sort_by(|a, b| a.member_id.cmp(&b.member_id));
    checkpoint()?;
    let plan = assemble_recursive_package_plan(manifest, planned, locked)?;
    checkpoint()?;
    Ok(plan)
}

fn assemble_recursive_package_plan(
    manifest: &WorkflowPackageManifest,
    planned: Vec<WorkflowPackagePlannedMember>,
    locked: Vec<WorkflowPackageLockedMember>,
) -> Result<WorkflowPackagePlan, WorkflowError> {
    let exports = manifest
        .exports
        .iter()
        .map(|(export, id)| {
            let member = locked
                .iter()
                .find(|member| &member.member_id == id)
                .ok_or_else(|| {
                    authoring_error("package.exports", "export member is unavailable")
                })?;
            Ok(WorkflowPackageLockedExport {
                export: export.clone(),
                member_id: id.clone(),
                definition_identity: member.definition_identity.clone(),
                published_revision: None,
            })
        })
        .collect::<Result<Vec<_>, WorkflowError>>()?;
    let mut imports = manifest
        .imports
        .iter()
        .map(|import| {
            Ok(WorkflowPackageLockedImport {
                import_id: import.import_id.clone(),
                package_id: import.package_id.clone(),
                export: import.export.clone(),
                package_lock_digest_sha256: import.package_lock_digest_sha256.clone().ok_or_else(
                    || authoring_error("package.imports", "immutable import lock is unavailable"),
                )?,
                target: import.target.clone().ok_or_else(|| {
                    authoring_error("package.imports", "import target is unavailable")
                })?,
            })
        })
        .collect::<Result<Vec<_>, WorkflowError>>()?;
    imports.sort_by(|left, right| left.import_id.cmp(&right.import_id));
    let lock = WorkflowPackageLock {
        version: WORKFLOW_PACKAGE_LOCK_VERSION,
        package_id: manifest.package_id.clone(),
        package_source_digest_sha256: canonical_sha256(manifest, "package")?,
        imports,
        exports,
        members: locked,
    };
    lock.validate()?;
    Ok(WorkflowPackagePlan {
        version: WORKFLOW_PACKAGE_PLAN_VERSION,
        package_id: manifest.package_id.clone(),
        members: planned,
        lock,
    })
}

fn prepare_workflow_package_definition(
    document: &WorkflowAuthoringDocument,
) -> Result<WorkflowDefinition, WorkflowError> {
    let normalized = document.normalized()?;
    let configuration =
        merge_authoring_configuration(document.configuration_defaults.as_ref(), None)?;
    validate_value_against_schema(
        "configuration",
        &configuration,
        &document.configuration_schema,
    )?;
    let mut definition = normalized.definition;
    let mut limits = normalized.run_limits;
    let mut defaults = normalized.plugin_input_defaults;
    let mut input = serde_json::json!({});
    for binding in &normalized.bindings {
        let source = authoring_value_at_path(&configuration, &binding.configuration_path)?.clone();
        let value = if let Some(transform) = &binding.transform {
            transform.evaluate(&[WorkflowTransformInput {
                name: WORKFLOW_TRANSFORM_SOURCE_CURRENT,
                value: &source,
            }])?
        } else {
            source
        };
        apply_authoring_binding(
            &mut definition,
            &mut limits,
            &mut defaults,
            &mut input,
            binding,
            value,
        )?;
    }
    materialize_plugin_input_defaults(&mut definition, &defaults, &mut input)?;
    limits.validate()?;
    normalize_authored_definition(definition)
}

/// Compute one finite local definition closure without invocation expansion.
///
/// A recursive call edge is not a cyclic ownership relation. Each member appears
/// at most once; runtime recursion and descendant policy remain admission-owned.
///
/// # Errors
/// Rejects missing references, duplicate members or exhausted analysis allowance.
pub fn workflow_package_recursive_closure(
    manifest: &WorkflowPackageManifest,
    root: &str,
    member_budget: usize,
    mut checkpoint: impl FnMut() -> Result<(), WorkflowError>,
) -> Result<Vec<String>, WorkflowError> {
    let members = manifest
        .members
        .iter()
        .map(|member| (member.member_id.as_str(), member))
        .collect::<BTreeMap<_, _>>();
    if members.len() != manifest.members.len() {
        return Err(authoring_error(
            "package.members",
            "duplicate member identity",
        ));
    }
    let mut visited = BTreeSet::new();
    let mut pending = vec![root.to_string()];
    while let Some(id) = pending.pop() {
        checkpoint()?;
        if visited.contains(&id) {
            continue;
        }
        let member = members.get(id.as_str()).ok_or_else(|| {
            authoring_error("package.members.dependencies", "local member is missing")
        })?;
        if visited.len() >= member_budget {
            return Err(authoring_error(
                "package.members.dependencies",
                "local dependency analysis allowance exceeded",
            ));
        }
        visited.insert(id);
        pending.extend(member.dependencies.iter().cloned());
    }
    visited.remove(root);
    Ok(visited.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> WorkflowPackageManifest {
        let member = |id: &str, dependency: &str| WorkflowPackageMember {
            member_id: id.into(),
            source_name: format!("{id}.workflow.json"),
            format: WorkflowSourceFormat::Json,
            source: serde_json::json!({
                "workflow_source_version": 3,
                "workflow_id": format!("authored/{id}"),
                "title": format!("Member {id}"),
                "input": {"type_name": "value/v1", "schema": {"type": "object"}},
                "output": {"type_name": "value/v1", "schema": {"type": "object"}},
                "steps": [{"id": "call", "package_call": {"member": dependency}}]
            })
            .to_string(),
            dependencies: vec![dependency.into()],
            external_dependencies: Vec::new(),
        };
        WorkflowPackageManifest {
            version: WORKFLOW_PACKAGE_MANIFEST_VERSION,
            package_id: "test/recursive".into(),
            exports: BTreeMap::from([("main".into(), "a".into())]),
            external_dependencies: BTreeMap::new(),
            imports: Vec::new(),
            members: vec![member("a", "b"), member("b", "a")],
        }
    }

    #[test]
    fn mutually_recursive_interfaces_and_closure_are_finite() {
        let manifest = manifest();
        let interfaces = workflow_package_local_interfaces(&manifest).unwrap();
        assert_eq!(interfaces.len(), 2);
        assert_eq!(interfaces["a"].input, interfaces["b"].output);
        assert_eq!(
            workflow_package_recursive_closure(&manifest, "a", 2, || Ok(())).unwrap(),
            ["b"]
        );
    }

    fn compiled_catalog() -> WorkflowAuthoringCatalogSnapshot {
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

    #[test]
    fn mutually_recursive_package_compiles_finite_policy_graph_and_resolves_lock() {
        let manifest = manifest();
        let plan = plan_workflow_package(&manifest, &compiled_catalog()).unwrap();
        assert_eq!(plan.members.len(), 2);
        let mut catalog = compiled_catalog();
        for member in &plan.members {
            assert_eq!(
                member.definition_identity.kind,
                format!("authored/{}", member.member_id)
            );
            assert_eq!(
                member.definition_identity,
                WorkflowDefinitionIdentity::for_definition(
                    &member.lowering.document.workflow_id,
                    &member.lowering.document.definition,
                )
                .unwrap()
            );
            catalog.workflow_definitions.insert(
                member.definition_identity.definition_id.clone(),
                member.lowering.document.definition.clone(),
            );
        }
        for member in &plan.members {
            let preview = member.lowering.document.compilation_preview(&catalog, None);
            let compiled = preview.compiled.unwrap();
            assert_eq!(compiled.definition_identity, member.definition_identity);
            let WorkflowPermissionGraph::V1 { definitions, .. } =
                compiled.permissions.graph.unwrap();
            assert_eq!(definitions.len(), 2);
            let call: WorkflowCallConfiguration = serde_json::from_value(
                member.lowering.document.definition.nodes["call"]
                    .configuration
                    .clone(),
            )
            .unwrap();
            let WorkflowCallTarget::PackageLocal { reference } = call.target else {
                panic!("local reference required")
            };
            assert!(
                WorkflowCallTarget::PackageLocal {
                    reference: reference.clone()
                }
                .definition_identity()
                .is_err()
            );
            let child = plan
                .members
                .iter()
                .find(|child| child.member_id == reference.member_id)
                .unwrap();
            assert_eq!(
                reference
                    .resolve(&plan.lock, &child.lowering.document.definition)
                    .unwrap(),
                child.definition_identity
            );
            let mut incompatible = reference;
            incompatible.input = ValueSchema::of::<u32>();
            assert!(
                incompatible
                    .resolve(&plan.lock, &child.lowering.document.definition)
                    .is_err()
            );
        }
    }

    #[test]
    fn recursive_lowering_keeps_interfaces_without_child_hashes() {
        let manifest = manifest();
        let interfaces = workflow_package_local_interfaces(&manifest).unwrap();
        let lowered =
            lower_workflow_package_local_references(&manifest.members[0], &interfaces).unwrap();
        assert!(lowered["steps"][0].get("package_call").is_none());
        let reference: WorkflowPackageLocalCall = serde_json::from_value(
            lowered["steps"][0]["workflow_call"]["target"]["reference"].clone(),
        )
        .unwrap();
        assert_eq!(reference, interfaces["b"]);
        assert_eq!(
            lowered["steps"][0]["workflow_call"]["target"]["kind"],
            "package_local"
        );
        let mut undeclared = manifest.members[0].clone();
        undeclared.dependencies.clear();
        assert!(lower_workflow_package_local_references(&undeclared, &interfaces).is_err());
    }

    #[test]
    fn local_analysis_obeys_count_and_cancellation_before_traversal() {
        let manifest = manifest();
        assert!(workflow_package_recursive_closure(&manifest, "a", 1, || Ok(())).is_err());
        assert!(
            workflow_package_recursive_closure(&manifest, "a", 2, || Err(authoring_error(
                "computation",
                "cancelled"
            )))
            .is_err()
        );
    }

    #[test]
    fn referenced_members_without_declared_interfaces_fail_closed() {
        let mut manifest = manifest();
        manifest.members[0].source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "authored/a",
            "title": "Missing interfaces",
            "steps": []
        })
        .to_string();
        assert!(workflow_package_local_interfaces(&manifest).is_err());
    }

    #[test]
    fn recursive_member_lowering_errors_are_package_qualified() {
        let mut manifest = manifest();
        let mut source: serde_json::Value =
            serde_json::from_str(&manifest.members[0].source).unwrap();
        source.as_object_mut().unwrap().remove("title");
        manifest.members[0].source = source.to_string();
        let error = plan_workflow_package(&manifest, &compiled_catalog()).unwrap_err();
        assert!(matches!(
            error,
            WorkflowError::Build { path, .. }
                if path == "package.members.a.source.source.structured"
        ));
    }

    #[test]
    fn package_checkpoints_cancel_each_phase_without_returning_partial_plans() {
        let recursive = manifest();
        let mut inferred = manifest();
        inferred.members[1].dependencies.clear();
        inferred.members[1].source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "authored/b",
            "title": "Inferred child",
            "steps": [{"id": "input", "input": {
                "schema": {"type_name": "value/v1", "schema": {"type": "object"}}
            }}]
        })
        .to_string();
        assert!(workflow_package_local_interfaces(&inferred).is_err());
        for manifest in [recursive, inferred] {
            let catalog = compiled_catalog();
            let expected = plan_workflow_package(&manifest, &catalog).unwrap();
            let mut phases = 0;
            let actual = plan_workflow_package_with_checkpoint(&manifest, &catalog, || {
                phases += 1;
                Ok(())
            })
            .unwrap();
            assert_eq!(actual, expected);
            for stop in 1..=phases {
                let mut observed = 0;
                let error = plan_workflow_package_with_checkpoint(&manifest, &catalog, || {
                    observed += 1;
                    if observed == stop {
                        Err(authoring_error("computation", "cancelled"))
                    } else {
                        Ok(())
                    }
                })
                .unwrap_err();
                assert_eq!(observed, stop);
                assert!(matches!(
                    error,
                    WorkflowError::Build { path, message }
                        if path == "computation" && message == "cancelled"
                ));
            }
        }
    }

    #[test]
    fn closure_checkpoints_preserve_plans_and_propagate_cancellation() {
        let manifest = manifest();
        let closure = WorkflowPackageClosure {
            version: WORKFLOW_PACKAGE_CLOSURE_VERSION,
            entry_package_id: manifest.package_id.clone(),
            packages: vec![WorkflowPackageClosureSource {
                package_id: manifest.package_id.clone(),
                source_name: None,
                manifest,
            }],
        };
        let catalog = compiled_catalog();
        let expected = plan_workflow_package_closure(&closure, &catalog).unwrap();
        let mut phases = 0;
        let actual = plan_workflow_package_closure_with_checkpoint(&closure, &catalog, || {
            phases += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(actual, expected);
        for stop in 1..=phases {
            let mut observed = 0;
            let error = plan_workflow_package_closure_with_checkpoint(&closure, &catalog, || {
                observed += 1;
                if observed == stop {
                    Err(authoring_error("computation", "cancelled"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert_eq!(observed, stop);
            assert!(matches!(
                error,
                WorkflowError::Build { path, message }
                    if path == "computation" && message == "cancelled"
            ));
        }
    }

    #[test]
    fn future_local_reference_version_is_preserved_and_rejected() {
        let manifest = manifest();
        let mut reference = workflow_package_local_interfaces(&manifest)
            .unwrap()
            .remove("a")
            .unwrap();
        reference.version = 2;
        let encoded = serde_json::to_string(&reference).unwrap();
        let decoded: WorkflowPackageLocalCall = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.version, 2);
        assert!(decoded.validate().is_err());
    }
}
