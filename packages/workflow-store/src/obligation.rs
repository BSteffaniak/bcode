//! Atomic same-run adoption of a receipt-backed resource obligation.
//! No native dispatch and no second operation journal: proof lives in both canonical receipts.
use super::*;
use bcode_workflow::{WorkflowObligationAdoption, WorkflowObligationAdoptionProof};
use switchy_database::{Database, DatabaseValue as V, TransactionMode};

#[cfg(test)]
#[path = "obligation_tests.rs"]
mod tests;

const PROOF: &str = "workflow_obligation_adoption";
fn invalid(message: &str) -> WorkflowStoreError {
    WorkflowStoreError::InvalidData(message.into())
}
fn digest(value: &serde_json::Value) -> Result<String, WorkflowStoreError> {
    bcode_workflow::workflow_canonical_value_sha256(value)
        .map_err(|_| invalid("invalid adoption checksum"))
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}
fn validate(request: &WorkflowObligationAdoption) -> Result<(), WorkflowStoreError> {
    for id in [
        &request.obligation_id,
        &request.run_id,
        &request.evidence_subject,
        &request.source.node_id,
        &request.source.activation_id,
        &request.source.dispatch_identity,
        &request.destination.node_id,
        &request.destination.activation_id,
        &request.destination.dispatch_identity,
        &request.destination_grant_id,
    ] {
        validate_id("adoption identity", id)?;
    }
    if request.expected_revision == 0
        || request.ownership_generation == 0
        || request.evidence_revision == 0
        || request.source.attempt == 0
        || request.destination.attempt == 0
        || request.obligation_id != request.source.dispatch_identity
        || request.source.activation_id == request.destination.activation_id
        || !valid_digest(&request.source_receipt_sha256)
        || !valid_digest(&request.evidence_sha256)
        || !valid_digest(&request.destination_grant_scope_sha256)
        || request.resource_lease_ids.len() > 100
    {
        return Err(invalid("invalid obligation adoption"));
    }
    let unique: BTreeSet<_> = request.resource_lease_ids.iter().collect();
    if unique.len() != request.resource_lease_ids.len() {
        return Err(invalid("duplicate adoption lease"));
    }
    for id in &request.resource_lease_ids {
        validate_id("adoption lease", id)?;
    }
    for attempt in [&request.source, &request.destination] {
        if dispatch_identity(
            &request.run_id,
            &attempt.node_id,
            &attempt.activation_id,
            attempt.attempt,
        ) != attempt.dispatch_identity
        {
            return Err(invalid("adoption dispatch identity mismatch"));
        }
    }
    Ok(())
}
fn receipt_row(
    db: &dyn Database,
    run: &str,
    identity: &str,
) -> Result<switchy_database::Row, WorkflowStoreError> {
    database::rows(
        db,
        &db.select("workflow_attempts")
            .columns(&[
                "run_id",
                "node_id",
                "activation_id",
                "attempt",
                "status",
                "receipt_json",
            ])
            .where_eq("run_id", run)
            .where_eq("dispatch_identity", identity)
            .limit(1),
    )?
    .into_iter()
    .next()
    .ok_or_else(|| invalid("adoption source attempt missing"))
}
fn leases(
    db: &dyn Database,
    request: &WorkflowObligationAdoption,
) -> Result<Vec<switchy_database::Row>, WorkflowStoreError> {
    let rows = database::rows(
        db,
        &db.select("workflow_resource_leases")
            .columns(&["lease_id", "resource_key", "mode", "expires_at_ms"])
            .where_eq("run_id", request.run_id.as_str())
            .where_eq("activation_id", request.source.activation_id.as_str())
            .where_eq("released_at_ms", V::Null)
            .limit(101),
    )?;
    let actual = rows
        .iter()
        .map(|row| database::text(row, "lease_id"))
        .collect::<Result<BTreeSet<_>, _>>()?;
    if rows.len() > 100 || actual != request.resource_lease_ids.iter().cloned().collect() {
        return Err(invalid(
            "adoption requires the complete exact resource lease set",
        ));
    }
    Ok(rows)
}

fn verify_body(value: &serde_json::Value, expected: &str) -> Result<(), WorkflowStoreError> {
    let mut original = value.clone();
    original
        .as_object_mut()
        .ok_or_else(|| invalid("invalid adopted receipt body"))?
        .insert(PROOF.into(), serde_json::Value::Null);
    if digest(&original)? == expected {
        return Ok(());
    }
    original
        .as_object_mut()
        .ok_or_else(|| invalid("invalid adopted receipt body"))?
        .remove(PROOF);
    if digest(&original)? != expected {
        return Err(invalid("adopted receipt body checksum changed"));
    }
    Ok(())
}

fn retained_proof(
    db: &dyn Database,
    request: &WorkflowObligationAdoption,
) -> Result<Option<WorkflowObligationAdoptionProof>, WorkflowStoreError> {
    let source = receipt_row(db, &request.run_id, &request.source.dispatch_identity)?;
    let value: serde_json::Value = serde_json::from_str(&database::text(&source, "receipt_json")?)?;
    let Some(proof) = value.get(PROOF).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let proof: WorkflowObligationAdoptionProof = serde_json::from_value(proof.clone())?;
    if proof.request != *request {
        return Err(invalid("conflicting obligation adoption retry"));
    }
    verify_body(&value, &proof.request.source_receipt_sha256)?;
    let destination = receipt_row(db, &request.run_id, &request.destination.dispatch_identity)?;
    let destination: serde_json::Value =
        serde_json::from_str(&database::text(&destination, "receipt_json")?)?;
    if destination.get(PROOF) != value.get(PROOF) {
        return Err(invalid("adoption peer proof mismatch"));
    }
    verify_body(&destination, &proof.destination_receipt_sha256)?;
    Ok(Some(proof))
}

impl WorkflowStore {
    /// Recover exact committed adoption under current authority without planning or mutation.
    /// Graph advance, cancellation, and exhausted grants do not replay a committed transfer.
    /// # Errors
    /// Rejects foreign authority, changed requests and altered retained receipt bodies.
    pub fn recover_owned_obligation_adoption(
        &self,
        request: &WorkflowObligationAdoption,
        authority: &WorkflowExecutionAuthority,
    ) -> Result<Option<WorkflowObligationAdoptionProof>, WorkflowStoreError> {
        validate(request)?;
        let tx = database::begin(&self.connection, TransactionMode::Deferred)?;
        verify_store_schema(tx.as_ref())?;
        verify_execution_authority_in(tx.as_ref(), &request.run_id, authority)?;
        let proof = retained_proof(tx.as_ref(), request)?;
        database::commit(tx)?;
        Ok(proof)
    }

    /// Read retained adoption proof by exact source receipt identity without replaying delivery.
    /// # Errors
    /// Rejects malformed identities, corrupt receipt payloads and storage failures.
    pub fn retained_obligation_adoption(
        &self,
        run_id: &str,
        source_dispatch_identity: &str,
    ) -> Result<Option<WorkflowObligationAdoptionProof>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        validate_id("source dispatch", source_dispatch_identity)?;
        let row = receipt_row(&self.connection, run_id, source_dispatch_identity)?;
        let Some(payload) = database::optional_text(&row, "receipt_json")? else {
            return Ok(None);
        };
        let value: serde_json::Value = serde_json::from_str(&payload)?;
        value
            .get(PROOF)
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(Into::into)
    }

    /// Read one exact pending adoption destination under current run authority.
    /// # Errors
    /// Rejects stale authority, invalid identities, unavailable graph state or storage failures.
    pub fn pending_obligation_destination(
        &self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        authority: &WorkflowExecutionAuthority,
    ) -> Result<Option<PendingActivation>, WorkflowStoreError> {
        let tx = database::begin(&self.connection, TransactionMode::Deferred)?;
        verify_execution_authority_in(tx.as_ref(), run_id, authority)?;
        let result = pending_activation_by_identity(tx.as_ref(), run_id, node_id, activation_id)?;
        database::commit(tx)?;
        Ok(result)
    }

    /// Retain cancellation on the adopted destination before signaling its native owner.
    /// # Errors
    /// Rejects stale authority, missing source cancellation intent or inconsistent adoption proof.
    pub fn propagate_owned_obligation_cancellation(
        &self,
        run_id: &str,
        source_dispatch: &str,
        authority: &WorkflowExecutionAuthority,
        requested_at_ms: u64,
    ) -> Result<String, WorkflowStoreError> {
        let tx = database::begin(&self.connection, TransactionMode::Immediate)?;
        let db: &dyn Database = tx.as_ref();
        verify_execution_authority_in(db, run_id, authority)?;
        if !cancellation_requested_for_run(db, run_id)?
            && !sibling_cancellation_requested_for_attempt(db, source_dispatch)?
        {
            return Err(invalid(
                "obligation cancellation lacks durable source intent",
            ));
        }
        let source = receipt_row(db, run_id, source_dispatch)?;
        let value: serde_json::Value =
            serde_json::from_str(&database::text(&source, "receipt_json")?)?;
        let proof: WorkflowObligationAdoptionProof = serde_json::from_value(
            value
                .get(PROOF)
                .cloned()
                .ok_or_else(|| invalid("obligation cancellation adoption proof missing"))?,
        )?;
        let identity = &proof.request.destination.dispatch_identity;
        let destination = receipt_row(db, run_id, identity)?;
        let receipt: serde_json::Value =
            serde_json::from_str(&database::text(&destination, "receipt_json")?)?;
        if receipt.get(PROOF) != value.get(PROOF) {
            return Err(invalid("obligation cancellation peer changed"));
        }
        let status = database::text(&destination, "status")?;
        if matches!(status.as_str(), "admitted" | "running") {
            if database::block_on(
                db.exec_update_count(
                    &db.update("workflow_attempts")
                        .value("status", "cancelling")
                        .where_eq("dispatch_identity", identity.as_str())
                        .where_eq("status", status),
                ),
            )? != 1
            {
                return Err(invalid("obligation cancellation publication conflict"));
            }
            append_event_in(
                db,
                run_id,
                "obligation_cancellation_requested",
                &serde_json::to_string(&proof)?,
                requested_at_ms,
            )?;
        }
        database::commit(tx)?;
        Ok(identity.clone())
    }

    /// Adopt one existing operation into an explicitly planned canonical pending activation.
    ///
    /// The host must have approved `destination_intent` through ordinary node policy and pinned
    /// the session evidence under current session ownership. This method verifies current workflow
    /// authority, graph, exact receipts/claims, prepares the destination, transfers every lease,
    /// publishes its observation-only receipt and advances graph revision in one transaction.
    /// No native operation is delivered. Identical retries recover proof without replay.
    ///
    /// # Errors
    /// Rejects stale ownership/revision, changed evidence binding/receipts, nonpending destinations,
    /// noncanonical nodes, incompatible resources, cancelled runs and conflicting duplicate requests.
    pub fn adopt_owned_obligation(
        &self,
        request: &WorkflowObligationAdoption,
        authority: &WorkflowExecutionAuthority,
        destination_intent: &serde_json::Value,
        destination_receipt: &serde_json::Value,
        adopted_at_ms: u64,
    ) -> Result<WorkflowObligationAdoptionProof, WorkflowStoreError> {
        validate(request)?;
        let tx = database::begin(&self.connection, TransactionMode::Immediate)?;
        let db: &dyn Database = tx.as_ref();
        verify_store_schema(db)?;
        if detected_typed_store_schema(db) != Some(WORKFLOW_STORE_SCHEMA_VERSION)
            || read_typed_contract_value(db, "workflow_storage_compatibility", "compatibility")?
                != Some(4)
        {
            return Err(invalid(
                "obligation adoption requires explicit workflow schema44 migration",
            ));
        }
        verify_execution_authority_in(db, &request.run_id, authority)?;
        if let Some(proof) = retained_proof(db, request)? {
            if proof.destination_receipt_sha256 != digest(destination_receipt)? {
                return Err(invalid("conflicting obligation adoption retry"));
            }
            database::commit(tx)?;
            return Ok(proof);
        }
        crate::recovery::require_execution(db, &request.run_id)?;
        if authority.generation != request.ownership_generation {
            return Err(invalid("adoption ownership changed"));
        }
        if cancellation_requested_for_run(db, &request.run_id)? {
            return Err(WorkflowStoreError::CancellationPreventsControl);
        }
        let source = receipt_row(db, &request.run_id, &request.source.dispatch_identity)?;
        let source_json = database::optional_text(&source, "receipt_json")?
            .ok_or_else(|| invalid("adoption source receipt absent"))?;
        let source_receipt: serde_json::Value = serde_json::from_str(&source_json)?;
        if !matches!(
            database::text(&source, "status")?.as_str(),
            "admitted" | "running"
        ) || database::text(&source, "node_id")? != request.source.node_id
            || database::text(&source, "activation_id")? != request.source.activation_id
            || database::unsigned(&source, "attempt")? != u64::from(request.source.attempt)
            || digest(&source_receipt)? != request.source_receipt_sha256
            || run_graph::graph_revision(db, &request.run_id)? != Some(request.expected_revision)
            || cancellation_requested_for_run(db, &request.run_id)?
        {
            return Err(invalid(
                "adoption source, graph or cancellation fence changed",
            ));
        }
        let Some(observed_source) =
            source_receipt["call"]["harness"]["kind"]["request"]["submission"].as_object()
        else {
            return Err(invalid("adoption source submission binding missing"));
        };
        let subject = digest(&serde_json::json!({
            "session_id":source_receipt["session_id"],"generation":source_receipt["generation"],
            "submission_id":observed_source.get("submission_id"),"attempt_id":observed_source.get("attempt_id"),
        }))?;
        if subject != request.evidence_subject {
            return Err(invalid("adoption canonical evidence subject mismatch"));
        }
        let destination = pending_activation_by_identity(
            db,
            &request.run_id,
            &request.destination.node_id,
            &request.destination.activation_id,
        )?
        .ok_or_else(|| invalid("adoption destination is not canonical pending work"))?;
        validate_destination(&destination, request, destination_intent)?;
        validate_receipt_binding(&source_receipt, destination_receipt, request)?;
        validate_adoption_grant(db, request, &destination, destination_intent, adopted_at_ms)?;
        let claims = leases(db, request)?;
        transfer_claims(db, request, &destination, &claims)?;
        let prepared = Self::prepare_adopted_activation_in(
            db,
            &destination,
            request,
            destination_intent,
            adopted_at_ms,
        )?;
        transfer_execution_link(db, request)?;
        let proof = publish_adoption(
            db,
            request,
            authority,
            prepared,
            destination_receipt,
            (source_json, source_receipt),
            adopted_at_ms,
        )?;
        database::commit(tx)?;
        Ok(proof)
    }
}

fn publish_adoption(
    db: &dyn Database,
    request: &WorkflowObligationAdoption,
    authority: &WorkflowExecutionAuthority,
    prepared: PreparedActivationDispatch,
    destination_receipt: &serde_json::Value,
    source: (String, serde_json::Value),
    adopted_at_ms: u64,
) -> Result<WorkflowObligationAdoptionProof, WorkflowStoreError> {
    let (source_json, mut source_receipt) = source;
    let revision = request
        .expected_revision
        .checked_add(1)
        .ok_or_else(|| invalid("adoption revision overflow"))?;
    let proof = WorkflowObligationAdoptionProof {
        request: request.clone(),
        adopted_revision: revision,
        destination_receipt_sha256: digest(destination_receipt)?,
    };
    let mut destination_receipt = destination_receipt.clone();
    destination_receipt
        .as_object_mut()
        .ok_or_else(|| invalid("adoption receipt must be an object"))?
        .insert(PROOF.into(), serde_json::to_value(&proof)?);
    source_receipt
        .as_object_mut()
        .ok_or_else(|| invalid("adoption source receipt must be an object"))?
        .insert(PROOF.into(), serde_json::to_value(&proof)?);
    WorkflowStore::persist_dispatch_receipt_transaction(
        db,
        &DispatchReceipt {
            run_id: request.run_id.clone(),
            node_id: request.destination.node_id.clone(),
            activation_id: request.destination.activation_id.clone(),
            attempt: prepared.attempt,
            dispatch_identity: prepared.dispatch_identity,
            receipt: destination_receipt,
            admitted_at_ms: adopted_at_ms,
        },
        Some(authority),
    )?;
    let source_payload = bounded_json("adopted source receipt", &source_receipt)?;
    let source_updated = database::block_on(
        db.exec_update_count(
            &db.update("workflow_attempts")
                .value("receipt_json", source_payload)
                .where_eq(
                    "dispatch_identity",
                    request.source.dispatch_identity.as_str(),
                )
                .where_eq("receipt_json", source_json),
        ),
    )?;
    let revision_updated = database::block_on(
        db.exec_update_count(
            &db.update("workflow_run_graphs")
                .value("revision", database::integer_value(revision)?)
                .where_eq("run_id", request.run_id.as_str())
                .where_eq(
                    "revision",
                    database::integer_value(request.expected_revision)?,
                ),
        ),
    )?;
    if source_updated != 1 || revision_updated != 1 {
        return Err(invalid("adoption publication conflict"));
    }
    preserve_adoption_graph_bindings(db, request, revision)?;
    append_event_in(
        db,
        &request.run_id,
        "obligation_adopted",
        &serde_json::to_string(&proof)?,
        adopted_at_ms,
    )?;
    Ok(proof)
}

fn preserve_adoption_graph_bindings(
    db: &dyn Database,
    request: &WorkflowObligationAdoption,
    revision: u64,
) -> Result<(), WorkflowStoreError> {
    // Adoption changes ownership, not topology. Carry forward only authorization
    // that was already valid at the exact previous revision, in bounded pages.
    let mut cursor: Option<(String, String)> = None;
    loop {
        let mut query = db
            .select("workflow_activation_graph_bindings")
            .columns(&["node_id", "activation_id", "graph_revision"])
            .where_eq("run_id", request.run_id.as_str())
            .sort("node_id", SortDirection::Asc)
            .sort("activation_id", SortDirection::Asc)
            .limit(100);
        if let Some((node, activation)) = &cursor {
            query = query.where_or(switchy_database::boxed![
                switchy_database::query::where_gt("node_id", node.as_str()),
                switchy_database::query::where_and(switchy_database::boxed![
                    switchy_database::query::where_eq("node_id", node.as_str()),
                    switchy_database::query::where_gt("activation_id", activation.as_str())
                ])
            ]);
        }
        let bindings = database::rows(db, &query)?;
        if bindings.is_empty() {
            return Ok(());
        }
        for binding in bindings {
            let node = database::text(&binding, "node_id")?;
            let activation = database::text(&binding, "activation_id")?;
            let admitted = database::unsigned(&binding, "graph_revision")?;
            cursor = Some((node.clone(), activation.clone()));
            if admitted == 0 || admitted > request.expected_revision {
                continue;
            }
            if admitted != request.expected_revision {
                let retained = database::rows(
                    db,
                    &db.select("workflow_leaf_retentions")
                        .columns(&["revision"])
                        .where_eq("run_id", request.run_id.as_str())
                        .where_eq("node_id", node.as_str())
                        .where_eq("activation_id", activation.as_str())
                        .where_eq(
                            "revision",
                            database::integer_value(request.expected_revision)?,
                        )
                        .limit(1),
                )?;
                if retained.is_empty() {
                    continue;
                }
                verify_adoption_retained_edges(db, request, &node, &activation)?;
            }
            database::block_on(
                db.update("workflow_activation_graph_bindings")
                    .value("graph_revision", database::integer_value(revision)?)
                    .where_eq("run_id", request.run_id.as_str())
                    .where_eq("node_id", node.as_str())
                    .where_eq("activation_id", activation.as_str())
                    .where_eq("graph_revision", database::integer_value(admitted)?)
                    .execute(db),
            )?;
        }
    }
}

fn verify_adoption_retained_edges(
    db: &dyn Database,
    request: &WorkflowObligationAdoption,
    node: &str,
    activation: &str,
) -> Result<(), WorkflowStoreError> {
    let mut cursor = None;
    loop {
        // The transaction has already advanced the revision, but topology is unchanged.
        let edges = WorkflowStore::current_run_graph_edges_in_snapshot(
            db,
            &request.run_id,
            request.expected_revision + 1,
            cursor,
            100,
            Some(run_graph::EdgeEndpoint::Source(node)),
        )?;
        if edges.is_empty() {
            return Ok(());
        }
        cursor = edges.last().map(|edge| edge.edge_id);
        for edge in edges {
            if run_graph::retained_edge_activation(
                db,
                &request.run_id,
                request.expected_revision,
                &edge,
            )?
            .as_deref()
                != Some(activation)
            {
                return Err(invalid(
                    "adoption cannot carry forward unreconciled successor binding",
                ));
            }
        }
    }
}

fn validate_receipt_binding(
    source: &serde_json::Value,
    destination: &serde_json::Value,
    request: &WorkflowObligationAdoption,
) -> Result<(), WorkflowStoreError> {
    if source["completion_requirement"] != "async_handoff"
        || destination["completion_requirement"] != "associated_work"
    {
        return Err(invalid(
            "adoption must transfer async source into associated-work responsibility",
        ));
    }
    for key in [
        "owner",
        "session_id",
        "generation",
        "parent_session_id",
        "call",
        "plugin_id",
    ] {
        if source.get(key).is_none() || source.get(key) != destination.get(key) {
            return Err(invalid("adoption receipt execution binding changed"));
        }
    }
    for (receipt, identity) in [
        (source, &request.source),
        (destination, &request.destination),
    ] {
        if receipt["run_id"] != request.run_id
            || receipt["node_id"] != identity.node_id
            || receipt["activation_id"] != identity.activation_id
            || receipt["dispatch_identity"] != identity.dispatch_identity
        {
            return Err(invalid("adoption receipt participant identity mismatch"));
        }
    }
    Ok(())
}

fn validate_destination(
    destination: &PendingActivation,
    request: &WorkflowObligationAdoption,
    intent: &serde_json::Value,
) -> Result<(), WorkflowStoreError> {
    if destination.node.kind != bcode_workflow::NodeKind::PluginBlock {
        return Err(invalid(
            "adoption requires explicit observation plugin node",
        ));
    }
    let block: WorkflowBlockDefinition =
        serde_json::from_value(destination.node.configuration.clone())?;
    if block.operation != "harness.await"
        || block.block_version != 2
        || !block.preparation_required
        || !block.cancellation_supported
        || block.authorization.capability != bcode_workflow::WorkflowToolCapability::Mutating
        || !block.authorization.explicit_grant_required
        || block.reconciliation != bcode_workflow::WorkflowBlockReconciliation::ReceiptStatus
        || intent.get("block") != Some(&serde_json::to_value(&block)?)
        || intent["adoption"] != serde_json::to_value(request)?
    {
        return Err(invalid(
            "adoption destination intent is not its approved canonical observation contract",
        ));
    }
    if intent.get("input") != destination.input.as_ref() {
        return Err(invalid("adoption destination input changed"));
    }
    if intent["adoption"] != serde_json::to_value(request)? {
        return Err(invalid(
            "adoption destination plan does not retain the exact transfer",
        ));
    }
    block
        .validate()
        .map_err(|_| invalid("invalid adoption observation block"))?;
    Ok(())
}

fn transfer_claims(
    db: &dyn Database,
    request: &WorkflowObligationAdoption,
    destination: &PendingActivation,
    rows: &[switchy_database::Row],
) -> Result<(), WorkflowStoreError> {
    let expected: BTreeSet<_> = destination
        .node
        .resources
        .iter()
        .map(|claim| {
            (
                claim.resource.clone(),
                match claim.access {
                    bcode_workflow::ResourceAccess::Read => "read",
                    bcode_workflow::ResourceAccess::Write => "write",
                }
                .to_owned(),
            )
        })
        .collect();
    let actual = rows
        .iter()
        .map(|row| {
            Ok((
                database::text(row, "resource_key")?,
                database::text(row, "mode")?,
            ))
        })
        .collect::<Result<BTreeSet<_>, WorkflowStoreError>>()?;
    if actual != expected
        || expected.len() != destination.node.resources.len()
        || rows
            .iter()
            .any(|row| !matches!(row.get("expires_at_ms"), Some(V::Null)))
    {
        return Err(invalid(
            "adoption destination must declare every nonexpiring source resource exactly",
        ));
    }
    for row in rows {
        let changed = database::block_on(
            db.exec_update_count(
                &db.update("workflow_resource_leases")
                    .value("node_id", request.destination.node_id.as_str())
                    .value("activation_id", request.destination.activation_id.as_str())
                    .where_eq("lease_id", database::text(row, "lease_id")?)
                    .where_eq("activation_id", request.source.activation_id.as_str())
                    .where_eq("released_at_ms", V::Null),
            ),
        )?;
        if changed != 1 {
            return Err(invalid("adoption lease transfer conflict"));
        }
    }
    Ok(())
}

fn validate_adoption_grant(
    db: &dyn Database,
    request: &WorkflowObligationAdoption,
    destination: &PendingActivation,
    intent: &serde_json::Value,
    now: u64,
) -> Result<(), WorkflowStoreError> {
    let row = database::rows(
        db,
        &db.select("workflow_grants")
            .columns(&["scope_json", "expires_at_ms", "max_uses", "uses_consumed"])
            .where_eq("grant_id", request.destination_grant_id.as_str())
            .where_eq("run_id", request.run_id.as_str())
            .where_eq("node_id", request.destination.node_id.as_str())
            .limit(1),
    )?
    .into_iter()
    .next()
    .ok_or_else(|| invalid("adoption destination grant missing"))?;
    let scope: serde_json::Value = serde_json::from_str(&database::text(&row, "scope_json")?)?;
    let typed: bcode_workflow::WorkflowMutationGrantScope = serde_json::from_value(scope.clone())?;
    typed
        .validate()
        .map_err(|_| invalid("invalid typed adoption grant"))?;
    let block: WorkflowBlockDefinition =
        serde_json::from_value(destination.node.configuration.clone())?;
    let input = destination
        .input
        .as_ref()
        .ok_or_else(|| invalid("adoption input missing"))?;
    let prepared = bcode_workflow::prepare_workflow_node_dataflow(
        destination.node.dataflow,
        &destination.node.input,
        &block.input,
        input,
    )
    .map_err(|_| invalid("invalid adoption dataflow"))?;
    let checksum = bcode_workflow::workflow_block_input_sha256(prepared.owner_input())
        .map_err(|_| invalid("adoption input checksum invalid"))?;
    let preparation: bcode_workflow::WorkflowBlockPreparationResponse =
        serde_json::from_value(intent["preparation"].clone())?;
    if bcode_workflow::WorkflowPreparedAdmission::from_descriptor(&preparation.descriptor)?
        != bcode_workflow::WorkflowPreparedAdmission::DeferredUntilAdopted
    {
        return Err(invalid(
            "adoption destination is not owner-reserved for explicit adoption",
        ));
    }
    preparation
        .validate()
        .map_err(|_| invalid("invalid adoption preparation"))?;
    if preparation.owner_id != block.plugin_id
        || preparation.input_sha256 != checksum
        || preparation.operation_facts.is_null()
    {
        return Err(invalid("adoption preparation binding changed"));
    }
    let run =
        run_summary_in(db, &request.run_id)?.ok_or_else(|| invalid("adoption run missing"))?;
    if run.authorization_ceiling != bcode_workflow::WorkflowToolCapability::Mutating
        || typed.definition_id != run.definition_id
        || typed.definition_version != run.definition_version
        || typed.workspace_snapshot != run.workspace_snapshot
        || typed.run_id != request.run_id
        || typed.node_id != destination.node_id
        || typed.activation_id != destination.activation_id
        || typed.plugin_id != block.plugin_id
        || typed.block_id != block.block_id
        || typed.block_version != block.block_version
        || typed.operation != block.operation
        || typed.input_checksum_sha256 != checksum
        || typed.resource_claims != destination.node.resources
        || typed.preparation_descriptor_sha256.as_deref()
            != Some(digest(&preparation.descriptor)?.as_str())
        || typed.operation_facts != Some(preparation.operation_facts)
    {
        return Err(invalid(
            "adoption grant does not approve the exact destination operation",
        ));
    }
    if digest(&scope)? != request.destination_grant_scope_sha256
        || database::optional_unsigned(&row, "expires_at_ms")?.is_some_and(|expiry| expiry < now)
        || database::optional_unsigned(&row, "max_uses")?.is_some_and(|maximum| {
            !database::unsigned(&row, "uses_consumed").is_ok_and(|used| used < maximum)
        })
    {
        return Err(invalid("adoption destination grant stale or exhausted"));
    }
    let used = database::unsigned(&row, "uses_consumed")?;
    let next = used
        .checked_add(1)
        .ok_or_else(|| invalid("adoption grant use overflow"))?;
    if database::block_on(
        db.exec_update_count(
            &db.update("workflow_grants")
                .value("uses_consumed", database::integer_value(next)?)
                .where_eq("grant_id", request.destination_grant_id.as_str())
                .where_eq("uses_consumed", database::integer_value(used)?),
        ),
    )? != 1
    {
        return Err(invalid("adoption grant consumption conflict"));
    }
    Ok(())
}

pub fn require_terminal_obligation_fence(
    db: &dyn Database,
    request: &AttemptReconciliationRequest,
) -> Result<(), WorkflowStoreError> {
    let row = receipt_row(db, &request.run_id, &request.dispatch_identity)?;
    let Some(encoded) = database::optional_text(&row, "receipt_json")? else {
        return Ok(());
    };
    let value: serde_json::Value = serde_json::from_str(&encoded)?;
    if value["completion_requirement"] != "async_handoff" {
        return Ok(());
    }
    let proof: WorkflowObligationAdoptionProof = serde_json::from_value(
        value
            .get(PROOF)
            .cloned()
            .ok_or_else(|| invalid("async source cannot terminalize before durable adoption"))?,
    )?;
    if proof.request.source.dispatch_identity != request.dispatch_identity
        || proof.request.run_id != request.run_id
    {
        return Err(invalid("async source adoption proof mismatch"));
    }
    let destination = receipt_row(
        db,
        &request.run_id,
        &proof.request.destination.dispatch_identity,
    )?;
    let receipt: serde_json::Value =
        serde_json::from_str(&database::text(&destination, "receipt_json")?)?;
    if receipt.get(PROOF) != value.get(PROOF) {
        return Err(invalid("async source adoption destination missing proof"));
    }
    let remaining = database::rows(
        db,
        &db.select("workflow_resource_leases")
            .columns(&["lease_id"])
            .where_eq("run_id", request.run_id.as_str())
            .where_eq("activation_id", request.activation_id.as_str())
            .where_eq("released_at_ms", V::Null)
            .limit(1),
    )?;
    if !remaining.is_empty() {
        return Err(invalid("async source still owns resource responsibility"));
    }
    Ok(())
}

fn transfer_execution_link(
    db: &dyn Database,
    request: &WorkflowObligationAdoption,
) -> Result<(), WorkflowStoreError> {
    let rows = database::rows(
        db,
        &db.select("workflow_execution_sessions")
            .columns(&["session_id", "workspace_snapshot"])
            .where_eq("run_id", request.run_id.as_str())
            .where_eq("node_id", request.source.node_id.as_str())
            .where_eq("activation_id", request.source.activation_id.as_str())
            .where_eq("attempt", i64::from(request.source.attempt))
            .limit(2),
    )?;
    if rows.len() != 1 {
        return Err(invalid("adoption requires one exact source execution link"));
    }
    let changed = database::block_on(
        db.exec_update_count(
            &db.update("workflow_execution_sessions")
                .value("node_id", request.destination.node_id.as_str())
                .value("activation_id", request.destination.activation_id.as_str())
                .value("attempt", i64::from(request.destination.attempt))
                .where_eq("run_id", request.run_id.as_str())
                .where_eq("activation_id", request.source.activation_id.as_str())
                .where_eq("attempt", i64::from(request.source.attempt)),
        ),
    )?;
    if changed != 1 {
        return Err(invalid("adoption execution link changed"));
    }
    Ok(())
}
