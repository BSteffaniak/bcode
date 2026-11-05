//! Canonical operation projection. All locators point back to a typed canonical event.
use super::{
    Database, SessionDb, SessionDbError, SessionDbResult, SessionEvent, SessionEventKind,
    SessionId, SortDirection, last_sequence, required_non_negative_u64, seq_to_value,
    strict_event_from_row,
};
use bcode_harness_models::{
    HarnessActivity, HarnessAttention, HarnessCompleteness, HarnessConnectivity,
    HarnessContinuation, HarnessExecutionStatus, HarnessInteraction, HarnessObservationKind,
    HarnessObservationPage, MAX_HARNESS_OBSERVATIONS, MAX_HARNESS_WIRE_BYTES,
};
use bcode_session_models::external_harness::ExternalHarnessOperationRecord as R;
use bcode_session_models::external_harness::{
    ExternalHarnessOperation, ExternalHarnessOperationRecord, ExternalHarnessOwner,
    ExternalHarnessRequest, ExternalHarnessRequestKind, ExternalHarnessResponse,
    ExternalHarnessResponseKind, ExternalHarnessState,
};
use switchy::database::query::FilterableQuery;
const KIND: &str = "harness_operation_v1";
fn op_key(namespace: &str, id: &str) -> String {
    bcode_session_models::external_backend::external_backend_key(namespace, id)
}
fn invalid() -> SessionDbError {
    SessionDbError::InvalidData("canonical harness operation conflict or stale owner".into())
}

fn operation(event: &SessionEvent) -> SessionDbResult<&ExternalHarnessOperation> {
    let SessionEventKind::ExternalHarnessOperation { operation } = &event.kind else {
        return Err(invalid());
    };
    operation.validate().map_err(SessionDbError::InvalidData)?;
    if operation.owner.session_id != event.session_id {
        return Err(invalid());
    }
    Ok(operation)
}
async fn latest(
    db: &dyn Database,
    session: SessionId,
    namespace: &str,
    id: &str,
) -> SessionDbResult<Option<SessionEvent>> {
    let rows = db
        .select("turn_evidence")
        .columns(&["event_seq"])
        .where_eq("turn_id", op_key(namespace, id))
        .where_eq("evidence_kind", KIND)
        .sort("event_seq", SortDirection::Desc)
        .limit(1)
        .execute(db)
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let sequence = required_non_negative_u64(row, "event_seq")?;
    let lengths = db
        .select("events")
        .project_as(
            switchy::database::query::byte_length(switchy::database::query::identifier("payload")),
            "bytes",
        )
        .where_eq("event_seq", seq_to_value(sequence))
        .limit(1)
        .execute(db)
        .await?;
    if required_non_negative_u64(lengths.first().ok_or_else(invalid)?, "bytes")?
        > (bcode_harness_models::MAX_HARNESS_WIRE_BYTES * 6 + 4096) as u64
    {
        return Err(invalid());
    }
    let rows = db
        .select("events")
        .columns(&["event_seq", "payload"])
        .where_eq("event_seq", seq_to_value(sequence))
        .limit(1)
        .execute(db)
        .await?;
    let event = strict_event_from_row(rows.first().ok_or_else(invalid)?, session)?;
    let op = operation(&event)?;
    if !keys(op)
        .iter()
        .any(|(ns, identity)| ns == namespace && identity == id)
    {
        return Err(invalid());
    }
    Ok(Some(event))
}
fn mutation_key(request: &ExternalHarnessRequest) -> String {
    use ExternalHarnessRequestKind as R;
    match &request.kind {
        R::Open(_) => "open".into(),
        R::Submit { submission, .. } => format!("submit:{}", submission.submission_id),
        R::Respond { response, .. } => format!("respond:{}", response.request_id),
        R::Interrupt(v) => format!("interrupt:{}", v.mutation_id),
        R::Close(v) => format!("close:{}", v.mutation_id),
        R::Reconcile(_) => format!("reconcile:{}", request.request_id),
    }
}
fn submission_key(submission_id: &str, attempt_id: &str) -> String {
    format!("{}:{submission_id}:{attempt_id}", submission_id.len())
}
fn claim_keys(request: &ExternalHarnessRequest) -> Vec<(String, String)> {
    let mut keys = vec![
        ("claim".into(), request.request_id.clone()),
        ("mutation".into(), mutation_key(request)),
    ];
    if let ExternalHarnessRequestKind::Submit {
        target: bcode_harness_models::HarnessSessionTarget::Prepared(prepared),
        ..
    } = &request.kind
    {
        keys.push(("prepared_submit".into(), prepared.open_id.clone()));
    }
    if let ExternalHarnessRequestKind::Respond { response, .. } = &request.kind {
        keys.push(("answered".into(), response.request_id.clone()));
    }
    if let ExternalHarnessRequestKind::Submit { submission, .. } = &request.kind {
        keys.push((
            "submission_claim".into(),
            submission_key(&submission.submission_id, &submission.attempt_id),
        ));
    }
    keys
}
fn keys(op: &ExternalHarnessOperation) -> Vec<(String, String)> {
    let mut keys = match &op.record {
        R::Configured { .. } => vec![("config".into(), "current".into())],
        R::DeliveryClaimed { request } => claim_keys(request),
        R::DeliveryCompleted { request, response } => {
            let mut keys = vec![("completion".into(), request.request_id.clone())];
            match &response.kind {
                ExternalHarnessResponseKind::Prepared(_)
                | ExternalHarnessResponseKind::Opened(_)
                | ExternalHarnessResponseKind::Reconciled(
                    bcode_harness_models::HarnessReconciliation::Known { .. }
                    | bcode_harness_models::HarnessReconciliation::KnownPrepared { .. },
                ) => keys.push(("binding".into(), "current".into())),
                _ => {}
            }
            let receipt = match &response.kind {
                ExternalHarnessResponseKind::Receipt(receipt) => Some(receipt),
                ExternalHarnessResponseKind::Reconciled(
                    bcode_harness_models::HarnessReconciliation::Known { receipt, .. }
                    | bcode_harness_models::HarnessReconciliation::KnownPrepared { receipt, .. },
                ) => receipt.as_ref(),
                _ => None,
            };
            if let Some(receipt) = receipt {
                keys.push((
                    "receipt".into(),
                    format!(
                        "{}:{}:{}",
                        receipt.submission_id.len(),
                        receipt.submission_id,
                        receipt.attempt_id
                    ),
                ));
            }
            if let ExternalHarnessRequestKind::Respond { response, .. } = &request.kind {
                keys.push(("answered".into(), response.request_id.clone()));
            }
            keys
        }
        R::ObservationPage { page, after, .. } => {
            let mut keys = vec![
                ("history".into(), "all".into()),
                ("page".into(), "current".into()),
                (
                    "page_input".into(),
                    format!(
                        "{}:{}",
                        op.owner.generation,
                        serde_json::to_string(after).unwrap_or_default()
                    ),
                ),
            ];
            for observation in &page.observations {
                if matches!(
                    observation.kind,
                    HarnessObservationKind::BindingConfirmed { .. }
                ) {
                    keys.push(("binding".into(), "current".into()));
                }
                if let HarnessObservationKind::Settlement { native_turn_id } = &observation.kind {
                    keys.push(("settled".into(), native_turn_id.clone()));
                }
                if let HarnessObservationKind::SubmissionAccepted {
                    submission_id,
                    attempt_id,
                    ..
                } = &observation.kind
                {
                    keys.push((
                        "submission_acceptance".into(),
                        submission_key(submission_id, attempt_id),
                    ));
                }
                keys.push(("observation".into(), observation.observation_id.clone()));
                if let HarnessObservationKind::Interaction { interaction } = &observation.kind {
                    keys.push(("interaction".into(), interaction.request_id.clone()));
                }
            }
            keys
        }
    };
    keys.sort();
    keys.dedup();
    keys
}
pub(super) async fn project(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<bool> {
    if !matches!(
        event.kind,
        SessionEventKind::ExternalHarnessOperation { .. }
    ) {
        return Ok(false);
    }
    for (namespace, id) in keys(operation(event)?) {
        db.insert("turn_evidence")
            .value("turn_id", op_key(&namespace, &id))
            .value("evidence_kind", KIND)
            .value("segment_order", seq_to_value(event.sequence))
            .value("event_seq", seq_to_value(event.sequence))
            .execute(db)
            .await?;
    }
    Ok(true)
}
async fn state(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
) -> SessionDbResult<Option<ExternalHarnessState>> {
    let Some(event) = latest(db, owner.session_id, "config", "current").await? else {
        return Ok(None);
    };
    let config = operation(&event)?;
    if &config.owner != owner {
        return Err(invalid());
    }
    let ExternalHarnessOperationRecord::Configured {
        instance,
        capabilities,
        open_request,
    } = &config.record
    else {
        return Err(invalid());
    };
    let mut result = ExternalHarnessState {
        owner: owner.clone(),
        instance: instance.clone(),
        capabilities: capabilities.clone(),
        binding: None,
        target: None,
        observation_after: None,
        prepared_open: open_request.clone(),
    };
    if let Some(event) = latest(db, owner.session_id, "binding", "current").await? {
        let op = operation(&event)?;
        if &op.owner != owner {
            return Err(invalid());
        }
        if let ExternalHarnessOperationRecord::DeliveryCompleted { response, .. } = &op.record {
            result.binding = match &response.kind {
                ExternalHarnessResponseKind::Opened(binding)
                | ExternalHarnessResponseKind::Reconciled(
                    bcode_harness_models::HarnessReconciliation::Known { binding, .. },
                ) => Some(binding.clone()),
                ExternalHarnessResponseKind::Prepared(_)
                | ExternalHarnessResponseKind::Reconciled(
                    bcode_harness_models::HarnessReconciliation::KnownPrepared { .. },
                ) => None,
                _ => return Err(invalid()),
            };
            result.target = match &response.kind {
                ExternalHarnessResponseKind::Prepared(v)
                | ExternalHarnessResponseKind::Reconciled(
                    bcode_harness_models::HarnessReconciliation::KnownPrepared {
                        prepared: v, ..
                    },
                ) => Some(bcode_harness_models::HarnessSessionTarget::Prepared(
                    v.clone(),
                )),
                _ => result
                    .binding
                    .clone()
                    .map(bcode_harness_models::HarnessSessionTarget::Confirmed),
            };
        } else if let ExternalHarnessOperationRecord::ObservationPage { page, .. } = &op.record {
            let binding = page
                .observations
                .iter()
                .find_map(|v| match &v.kind {
                    HarnessObservationKind::BindingConfirmed { binding, .. } => {
                        Some(binding.clone())
                    }
                    _ => None,
                })
                .ok_or_else(invalid)?;
            result.binding = Some(binding.clone());
            result.target = Some(bcode_harness_models::HarnessSessionTarget::Confirmed(
                binding,
            ));
        }
    }
    if let Some(event) = latest(db, owner.session_id, "page", "current").await? {
        let page_operation = operation(&event)?;
        if &page_operation.owner != owner {
            return Err(invalid());
        }
        if let ExternalHarnessOperationRecord::ObservationPage { page, .. } = &page_operation.record
        {
            result.observation_after.clone_from(&page.next_after);
        }
    }
    Ok(Some(result))
}
pub(super) async fn validate(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
    if !matches!(
        event.kind,
        SessionEventKind::ExternalHarnessOperation { .. }
    ) {
        return Ok(());
    }
    super::external_backend::external_index_ready(db).await?;
    let op = operation(event)?;
    let current = state(db, &op.owner).await?;
    if let R::Configured {
        instance,
        capabilities,
        open_request,
    } = &op.record
    {
        if op.owner.generation != 1
            || current.as_ref().is_some_and(|v| {
                &v.instance != instance
                    || &v.capabilities != capabilities
                    || v.prepared_open.as_ref() != open_request.as_ref()
            })
        {
            return Err(invalid());
        }
        return Ok(());
    }
    let current = current.ok_or_else(invalid)?;
    match &op.record {
        R::DeliveryClaimed { request } => {
            validate_claim(db, op, &current, request).await?;
        }
        R::DeliveryCompleted { request, response } => {
            validate_completion(db, op, &current, request, response).await?;
        }
        R::ObservationPage {
            after,
            page,
            instance,
        } => {
            validate_page(db, op, &current, after.as_deref(), page, instance).await?;
        }
        R::Configured { .. } => {}
    }
    Ok(())
}
async fn interaction(
    db: &dyn Database,
    session: SessionId,
    id: &str,
) -> SessionDbResult<Option<HarnessInteraction>> {
    let Some(event) = latest(db, session, "interaction", id).await? else {
        return Ok(None);
    };
    let ExternalHarnessOperationRecord::ObservationPage { page, .. } = &operation(&event)?.record
    else {
        return Err(invalid());
    };
    Ok(page.observations.iter().find_map(|v| match &v.kind {
        HarnessObservationKind::Interaction { interaction } if interaction.request_id == id => {
            Some(interaction.clone())
        }
        _ => None,
    }))
}
impl SessionDb {
    /// Exact canonical submission-to-native-turn acceptance, independent of native request IDs.
    /// # Errors
    /// Rejects malformed identities, stale indexes, inconsistent evidence and concurrent changes.
    pub async fn external_harness_submission_acceptance(
        &self,
        submission_id: &str,
        attempt_id: &str,
    ) -> SessionDbResult<Option<bcode_harness_models::HarnessObservation>> {
        if [submission_id, attempt_id].iter().any(|id| {
            id.is_empty()
                || id.len() > bcode_harness_models::MAX_HARNESS_ID_BYTES
                || id.chars().any(char::is_control)
        }) {
            return Err(invalid());
        }
        super::external_backend::external_index_ready(&**self.db).await?;
        let before = last_sequence(&**self.db).await?;
        let result = if let Some(event) = latest(
            &**self.db,
            self.session_id,
            "submission_acceptance",
            &submission_key(submission_id, attempt_id),
        )
        .await?
        {
            let op = operation(&event)?;
            let R::ObservationPage { page, .. } = &op.record else {
                return Err(invalid());
            };
            let mut matches=page.observations.iter().filter(|v|matches!(&v.kind,HarnessObservationKind::SubmissionAccepted { submission_id: id,attempt_id: attempt,.. } if id==submission_id&&attempt==attempt_id));
            let observation = matches.next().ok_or_else(invalid)?;
            if matches.next().is_some() {
                return Err(invalid());
            }
            let claim = latest(
                &**self.db,
                self.session_id,
                "submission_claim",
                &submission_key(submission_id, attempt_id),
            )
            .await?
            .ok_or_else(invalid)?;
            if operation(&claim)?.owner != op.owner || claim.sequence >= event.sequence {
                return Err(invalid());
            }
            Some(observation.clone())
        } else {
            None
        };
        super::external_backend::external_index_ready(&**self.db).await?;
        if last_sequence(&**self.db).await? != before {
            return Err(invalid());
        }
        Ok(result)
    }
    /// Compare a complete retained request without adopting its historical execution owner.
    /// # Errors
    /// Rejects conflicting content, stale locators, malformed evidence and concurrent appends.
    pub async fn external_harness_retained_request(
        &self,
        request: &ExternalHarnessRequest,
    ) -> SessionDbResult<Option<bcode_session_models::ExternalHarnessClaim>> {
        use bcode_session_models::ExternalHarnessClaim;
        request.request().validate().map_err(|_| invalid())?;
        if request.target().0 != self.session_id.to_string() {
            return Err(invalid());
        }
        super::external_backend::external_index_ready(&**self.db).await?;
        let before = last_sequence(&**self.db).await?;
        let claim = latest(&**self.db, self.session_id, "claim", &request.request_id).await?;
        let result = if let Some(claim) = claim {
            let retained = operation(&claim)?;
            if !matches!(&retained.record, R::DeliveryClaimed { request: original } if original == request)
            {
                return Err(invalid());
            }
            if let Some(completed) = latest(
                &**self.db,
                self.session_id,
                "completion",
                &request.request_id,
            )
            .await?
            {
                let completion = operation(&completed)?;
                let R::DeliveryCompleted {
                    request: original,
                    response,
                } = &completion.record
                else {
                    return Err(invalid());
                };
                if original != request
                    || completion.owner != retained.owner
                    || completed.sequence <= claim.sequence
                {
                    return Err(invalid());
                }
                Some(ExternalHarnessClaim::Recorded(Box::new(response.clone())))
            } else {
                Some(ExternalHarnessClaim::Uncertain)
            }
        } else {
            // A new request ID cannot bypass an existing mutation's uncertainty.
            if latest(
                &**self.db,
                self.session_id,
                "mutation",
                &mutation_key(request),
            )
            .await?
            .is_some()
                || latest(
                    &**self.db,
                    self.session_id,
                    "completion",
                    &request.request_id,
                )
                .await?
                .is_some()
            {
                return Err(invalid());
            }
            None
        };
        super::external_backend::external_index_ready(&**self.db).await?;
        if last_sequence(&**self.db).await? != before {
            return Err(invalid());
        }
        Ok(result)
    }
    /// Bounded canonical snapshot without owner admission, plugin access or recovery.
    /// # Errors
    /// Rejects stale/missing locators, incompatible storage and concurrent snapshot changes.
    pub async fn external_harness_snapshot(&self) -> SessionDbResult<Option<ExternalHarnessState>> {
        super::external_backend::external_index_ready(&**self.db).await?;
        let before = last_sequence(&**self.db).await?;
        let result =
            if let Some(event) = latest(&**self.db, self.session_id, "config", "current").await? {
                let owner = operation(&event)?.owner.clone();
                state(&**self.db, &owner).await?
            } else {
                None
            };
        // Queries are exact-key and bounded. A concurrent append invalidates this read;
        // never return a mixed snapshot or retry/replay an unbounded history.
        super::external_backend::external_index_ready(&**self.db).await?;
        if last_sequence(&**self.db).await? != before {
            return Err(invalid());
        }
        Ok(result)
    }
    pub(crate) async fn harness_binding_event(&self) -> SessionDbResult<Option<SessionEvent>> {
        super::external_backend::external_index_ready(&**self.db).await?;
        let Some(event) = latest(&**self.db, self.session_id, "binding", "current").await? else {
            return Ok(None);
        };
        if matches!(&operation(&event)?.record, ExternalHarnessOperationRecord::DeliveryCompleted { response,.. }
            if matches!(&response.kind, ExternalHarnessResponseKind::Prepared(_) | ExternalHarnessResponseKind::Reconciled(bcode_harness_models::HarnessReconciliation::KnownPrepared { .. })))
        {
            return Ok(None);
        }
        Ok(Some(event))
    }
    pub(crate) async fn harness_history(
        &self,
        after: Option<String>,
        limit: u16,
    ) -> SessionDbResult<bcode_harness_models::HarnessObservationPage> {
        if limit == 0 || usize::from(limit) > MAX_HARNESS_OBSERVATIONS {
            return Err(invalid());
        }
        super::external_backend::external_index_ready(&**self.db).await?;
        let (sequence, offset) = history_position(after)?;
        let rows = self
            .db
            .select("turn_evidence")
            .columns(&["event_seq"])
            .where_eq("turn_id", op_key("history", "all"))
            .where_eq("evidence_kind", KIND)
            .where_gte("event_seq", seq_to_value(sequence))
            .sort("event_seq", SortDirection::Asc)
            .limit(2)
            .execute(&**self.db)
            .await?;
        let unknown = HarnessExecutionStatus {
            connectivity: HarnessConnectivity::Unknown,
            activity: HarnessActivity::Unknown,
            attention: HarnessAttention::Unknown,
            continuation: HarnessContinuation::Unknown,
            completeness: HarnessCompleteness::Unknown,
        };
        let Some(row) = rows.first() else {
            return Ok(HarnessObservationPage {
                observations: vec![],
                next_after: Some(format!("h1:{sequence}:{offset}")),
                completeness: HarnessCompleteness::Complete,
                status: unknown,
            });
        };
        let seq = required_non_negative_u64(row, "event_seq")?;
        let lengths = self
            .db
            .select("events")
            .project_as(
                switchy::database::query::byte_length(switchy::database::query::identifier(
                    "payload",
                )),
                "bytes",
            )
            .where_eq("event_seq", seq_to_value(seq))
            .limit(1)
            .execute(&**self.db)
            .await?;
        if required_non_negative_u64(lengths.first().ok_or_else(invalid)?, "bytes")?
            > (MAX_HARNESS_WIRE_BYTES * 6 + 4096) as u64
        {
            return Err(invalid());
        }
        let events = self
            .db
            .select("events")
            .columns(&["event_seq", "payload"])
            .where_eq("event_seq", seq_to_value(seq))
            .limit(1)
            .execute(&**self.db)
            .await?;
        let event = strict_event_from_row(events.first().ok_or_else(invalid)?, self.session_id)?;
        let ExternalHarnessOperationRecord::ObservationPage { page, .. } =
            &operation(&event)?.record
        else {
            return Err(invalid());
        };
        let start = if seq == sequence { offset } else { 0 };
        if start > page.observations.len() {
            return Err(invalid());
        }
        let end = (start + usize::from(limit)).min(page.observations.len());
        let observations = page.observations[start..end].to_vec();
        let next_after = Some(if end < page.observations.len() {
            format!("h1:{seq}:{end}")
        } else {
            format!("h1:{}:0", seq.checked_add(1).ok_or_else(invalid)?)
        });
        Ok(HarnessObservationPage {
            observations,
            next_after,
            completeness: if end < page.observations.len() || rows.len() > 1 {
                HarnessCompleteness::Partial
            } else {
                page.completeness
            },
            status: page.status.clone(),
        })
    }
    pub(crate) async fn harness_state(
        &self,
        owner: &ExternalHarnessOwner,
    ) -> SessionDbResult<Option<ExternalHarnessState>> {
        super::external_backend::external_index_ready(&**self.db).await?;
        state(&**self.db, owner).await
    }
    pub(crate) async fn harness_latest(
        &self,
        session: SessionId,
        namespace: &str,
        id: &str,
    ) -> SessionDbResult<Option<SessionEvent>> {
        super::external_backend::external_index_ready(&**self.db).await?;
        latest(&**self.db, session, namespace, id).await
    }
    pub(crate) async fn harness_interaction(
        &self,
        session: SessionId,
        id: &str,
    ) -> SessionDbResult<Option<HarnessInteraction>> {
        super::external_backend::external_index_ready(&**self.db).await?;
        interaction(&**self.db, session, id).await
    }
}

async fn validate_claim(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    current: &ExternalHarnessState,
    request: &ExternalHarnessRequest,
) -> SessionDbResult<()> {
    if current.instance != request.instance
        || (matches!(request.kind, ExternalHarnessRequestKind::Open(_))
            && current.prepared_open.as_ref().is_some_and(|v| v != request))
        || latest(db, op.owner.session_id, "claim", &request.request_id)
            .await?
            .is_some()
        || latest(db, op.owner.session_id, "mutation", &mutation_key(request))
            .await?
            .is_some()
    {
        return Err(invalid());
    }
    match &request.kind {
        ExternalHarnessRequestKind::Open(open) => {
            if current.target.is_some() || open.resume != current.binding {
                return Err(invalid());
            }
        }
        ExternalHarnessRequestKind::Submit { target, .. }
        | ExternalHarnessRequestKind::Respond { target, .. } => {
            if current.target.as_ref() != Some(target) {
                return Err(invalid());
            }
        }
        ExternalHarnessRequestKind::Interrupt(control)
        | ExternalHarnessRequestKind::Close(control) => {
            if current.target.as_ref() != Some(&control.target) {
                return Err(invalid());
            }
        }
        ExternalHarnessRequestKind::Reconcile(_) => {}
    }
    if let ExternalHarnessRequestKind::Submit {
        target: bcode_harness_models::HarnessSessionTarget::Prepared(prepared),
        submission,
    } = &request.kind
        && (!matches!(
            submission.placement,
            bcode_harness_models::HarnessPlacement::NextTurn
        ) || latest(
            db,
            op.owner.session_id,
            "prepared_submit",
            &prepared.open_id,
        )
        .await?
        .is_some())
    {
        return Err(invalid());
    }
    if let ExternalHarnessRequestKind::Respond { response, .. } = &request.kind {
        if latest(db, op.owner.session_id, "answered", &response.request_id)
            .await?
            .is_some()
        {
            return Err(invalid());
        }
        let interaction = interaction(db, op.owner.session_id, &response.request_id)
            .await?
            .ok_or_else(invalid)?;
        response.validate_for(&interaction).map_err(|_| invalid())?;
    }
    Ok(())
}

async fn validate_completion(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    current: &ExternalHarnessState,
    request: &ExternalHarnessRequest,
    response: &ExternalHarnessResponse,
) -> SessionDbResult<()> {
    if let Some(target) = &current.target {
        match &response.kind {
            ExternalHarnessResponseKind::Opened(binding)
            | ExternalHarnessResponseKind::Reconciled(
                bcode_harness_models::HarnessReconciliation::Known { binding, .. },
            ) => {
                let open_id = match target {
                    bcode_harness_models::HarnessSessionTarget::Prepared(v) => v.open_id.as_str(),
                    bcode_harness_models::HarnessSessionTarget::Confirmed(_) => "confirmed",
                };
                target
                    .validate_confirmation(open_id, binding)
                    .map_err(|_| invalid())?;
            }
            ExternalHarnessResponseKind::Prepared(prepared)
            | ExternalHarnessResponseKind::Reconciled(
                bcode_harness_models::HarnessReconciliation::KnownPrepared { prepared, .. },
            ) if target
                != &bcode_harness_models::HarnessSessionTarget::Prepared(prepared.clone()) =>
            {
                return Err(invalid());
            }
            _ => {}
        }
    }
    let event = latest(db, op.owner.session_id, "claim", &request.request_id)
        .await?
        .ok_or_else(invalid)?;
    if operation(&event)?.owner != op.owner
        || !matches!(&operation(&event)?.record,R::DeliveryClaimed { request: original } if original==request)
    {
        return Err(invalid());
    }
    if latest(db, op.owner.session_id, "completion", &request.request_id)
        .await?
        .is_some()
    {
        return Err(invalid());
    }
    Ok(())
}

async fn validate_page(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    current: &ExternalHarnessState,
    after: Option<&str>,
    page: &bcode_harness_models::HarnessObservationPage,
    instance: &bcode_harness_models::HarnessInstance,
) -> SessionDbResult<()> {
    if *instance != current.instance {
        return Err(invalid());
    }
    let previous = latest(db, op.owner.session_id, "page", "current").await?;
    let expected = previous
        .as_ref()
        .map(operation)
        .transpose()?
        .and_then(|v| match &v.record {
            R::ObservationPage { page, .. } => page.next_after.clone(),
            _ => None,
        });
    if after != expected.as_deref()
        || (!page.observations.is_empty() && page.next_after.as_deref() == after)
    {
        return Err(invalid());
    }
    if current.target.is_none() {
        return Err(invalid());
    }
    validate_page_observations(db, op, current, page).await?;
    Ok(())
}

fn history_position(after: Option<String>) -> SessionDbResult<(u64, usize)> {
    Ok(if let Some(cursor) = after {
        if cursor.len() > 128 {
            return Err(invalid());
        }
        let mut parts = cursor.split(':');
        if parts.next() != Some("h1") {
            return Err(invalid());
        }
        let sequence = parts
            .next()
            .ok_or_else(invalid)?
            .parse::<u64>()
            .map_err(|_| invalid())?;
        let offset = parts
            .next()
            .ok_or_else(invalid)?
            .parse::<usize>()
            .map_err(|_| invalid())?;
        if parts.next().is_some() || sequence > i64::MAX as u64 || offset > MAX_HARNESS_OBSERVATIONS
        {
            return Err(invalid());
        }
        (sequence, offset)
    } else {
        (0, 0)
    })
}

async fn validate_page_observations(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    current: &ExternalHarnessState,
    page: &bcode_harness_models::HarnessObservationPage,
) -> SessionDbResult<()> {
    let mut accepted_in_page = std::collections::BTreeSet::new();
    let mut confirmed = None;
    let mut settled_in_page = std::collections::BTreeSet::new();
    for observation in &page.observations {
        if let HarnessObservationKind::SubmissionAccepted {
            submission_id,
            attempt_id,
            ..
        } = &observation.kind
        {
            let key = submission_key(submission_id, attempt_id);
            if !accepted_in_page.insert(key) {
                return Err(invalid());
            }
            validate_submission_acceptance(db, op, observation).await?;
        }
        if observation
            .native_correlations
            .iter()
            .any(|id| settled_in_page.contains(id))
        {
            return Err(invalid());
        }
        if let HarnessObservationKind::Settlement { native_turn_id } = &observation.kind {
            settled_in_page.insert(native_turn_id.clone());
        }
        if matches!(observation.kind, HarnessObservationKind::Settlement { .. })
            && observation.completeness != bcode_harness_models::HarnessCompleteness::Complete
        {
            return Err(invalid());
        }
        for correlation in &observation.native_correlations {
            if latest(db, op.owner.session_id, "settled", correlation)
                .await?
                .is_some()
            {
                return Err(invalid());
            }
        }
        if let HarnessObservationKind::Interaction { interaction } = &observation.kind
            && (latest(
                db,
                op.owner.session_id,
                "interaction",
                &interaction.request_id,
            )
            .await?
            .is_some()
                || latest(db, op.owner.session_id, "answered", &interaction.request_id)
                    .await?
                    .is_some())
        {
            return Err(invalid());
        }
        if let HarnessObservationKind::BindingConfirmed { open_id, binding } = &observation.kind {
            current
                .target
                .as_ref()
                .ok_or_else(invalid)?
                .validate_confirmation(open_id, binding)
                .map_err(|_| invalid())?;
            if confirmed.as_ref().is_some_and(|v| v != binding) {
                return Err(invalid());
            }
            confirmed = Some(binding.clone());
        }
        if let Some(previous) = latest(
            db,
            op.owner.session_id,
            "observation",
            &observation.observation_id,
        )
        .await?
        {
            // An identical poll retry is handled before append by its retained input
            // cursor. Repeating an ID on a different page must not duplicate history.
            let _ = previous;
            return Err(invalid());
        }
    }
    Ok(())
}

async fn validate_submission_acceptance(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    observation: &bcode_harness_models::HarnessObservation,
) -> SessionDbResult<()> {
    let HarnessObservationKind::SubmissionAccepted {
        submission_id,
        attempt_id,
        ..
    } = &observation.kind
    else {
        return Err(invalid());
    };
    let key = submission_key(submission_id, attempt_id);
    let claim = latest(db, op.owner.session_id, "submission_claim", &key)
        .await?
        .ok_or_else(invalid)?;
    let retained = operation(&claim)?;
    if retained.owner != op.owner {
        return Err(invalid());
    }
    let R::DeliveryClaimed { request } = &retained.record else {
        return Err(invalid());
    };
    let ExternalHarnessRequestKind::Submit { submission, .. } = &request.kind else {
        return Err(invalid());
    };
    if submission.submission_id != *submission_id
        || submission.attempt_id != *attempt_id
        || submission.generation != observation.generation
    {
        return Err(invalid());
    }
    if latest(db, op.owner.session_id, "submission_acceptance", &key)
        .await?
        .is_some()
    {
        return Err(invalid());
    }
    Ok(())
}
