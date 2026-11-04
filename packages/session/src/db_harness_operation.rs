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

fn evidence_maintenance_required() -> SessionDbError {
    SessionDbError::InvalidData(
        "managed harness evidence index coverage is missing or stale; explicit session maintenance must backfill canonical harness evidence before evidence reads or page appends".into(),
    )
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
fn completion_keys(
    request: &ExternalHarnessRequest,
    response: &ExternalHarnessResponse,
) -> Vec<(String, String)> {
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

fn keys(op: &ExternalHarnessOperation) -> Vec<(String, String)> {
    let mut keys = match &op.record {
        R::Configured { .. } => vec![("config".into(), "current".into())],
        R::DeliveryClaimed { request } => claim_keys(request),
        R::DeliveryCompleted { request, response } => completion_keys(request, response),
        R::ObservationPage { page, after, .. } => {
            let mut keys = vec![
                ("history".into(), "all".into()),
                ("terminal_coverage_v2".into(), "current".into()),
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
            if page.completeness == HarnessCompleteness::Gap
                || page.status.completeness == HarnessCompleteness::Gap
                || page.observations.iter().any(observation_gap)
            {
                keys.push(("evidence_gap".into(), op.owner.generation.to_string()));
            }
            for observation in &page.observations {
                if matches!(observation.kind, HarnessObservationKind::Result { .. }) {
                    for turn in &observation.native_correlations {
                        keys.push(("turn_result".into(), turn.clone()));
                    }
                }
                if matches!(
                    observation.kind,
                    HarnessObservationKind::BindingConfirmed { .. }
                ) {
                    keys.push(("binding".into(), "current".into()));
                }
                if let HarnessObservationKind::NativeTurnTerminal { native_turn_id, .. } =
                    &observation.kind
                {
                    keys.push(("native_terminal".into(), native_turn_id.clone()));
                }
                if let HarnessObservationKind::Settlement { native_turn_id } = &observation.kind {
                    keys.push(("settled".into(), native_turn_id.clone()));
                }
                if let HarnessObservationKind::SubmissionAccepted {
                    submission_id,
                    attempt_id,
                    native_turn_id,
                } = &observation.kind
                {
                    keys.push(("accepted_turn".into(), native_turn_id.clone()));
                    keys.push((
                        "submission_acceptance".into(),
                        submission_key(submission_id, attempt_id),
                    ));
                }
                if let Some(key) = submission_frame_key(op.owner.generation, &observation.kind) {
                    keys.push(("submission_native_frame_v2".into(), key));
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
/// Compare the complete harness locator projection with current canonical events.
///
/// This is an explicit migration validation, not a normal read or repair path.
/// Expected keys use the same projector as ingestion, including v2 coverage and
/// attribution keys; historical observations never gain new terminal evidence.
///
/// # Errors
/// Returns an error for invalid canonical events, failed database reads, or any
/// missing, duplicate, corrupt, or unexpected harness projection locator.
pub(super) async fn validate_migration_keys(db: &dyn Database) -> SessionDbResult<()> {
    let events = super::strict_events(db).await?;
    super::validate_contiguous_canonical_events(&events)?;
    let mut expected = std::collections::BTreeSet::new();
    for event in &events {
        if matches!(
            event.kind,
            SessionEventKind::ExternalHarnessOperation { .. }
        ) {
            for (namespace, id) in keys(operation(event)?) {
                expected.insert((op_key(&namespace, &id), event.sequence, event.sequence));
            }
        }
    }
    let rows = db
        .select("turn_evidence")
        .columns(&["turn_id", "segment_order", "event_seq"])
        .where_eq("evidence_kind", KIND)
        .execute(db)
        .await?;
    let mut actual = std::collections::BTreeSet::new();
    for row in &rows {
        let locator = (
            super::required_string(row, "turn_id")?,
            required_non_negative_u64(row, "segment_order")?,
            required_non_negative_u64(row, "event_seq")?,
        );
        if !actual.insert(locator) {
            return Err(SessionDbError::MigrationHistoryIncompatible {
                reason: "duplicate canonical harness projection locator".to_owned(),
            });
        }
    }
    if actual != expected {
        return Err(SessionDbError::MigrationHistoryIncompatible {
            reason: "canonical harness projection locators are missing, corrupt, or unexpected"
                .to_owned(),
        });
    }
    Ok(())
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
    validate_operation(db, event).await
}

async fn validate_operation(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
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
fn submission_native_frame(kind: &HarnessObservationKind) -> Option<(&str, &str, &str)> {
    match kind {
        HarnessObservationKind::SubmissionConsumed {
            submission_id,
            attempt_id,
            native_frame_id,
        }
        | HarnessObservationKind::SubmissionResultObserved {
            submission_id,
            attempt_id,
            native_frame_id,
            ..
        } => Some((submission_id, attempt_id, native_frame_id)),
        _ => None,
    }
}

fn submission_frame_key(generation: u64, kind: &HarnessObservationKind) -> Option<String> {
    let (submission, attempt, frame) = submission_native_frame(kind)?;
    let stage = match kind {
        HarnessObservationKind::SubmissionConsumed { .. } => "consumed",
        HarnessObservationKind::SubmissionResultObserved { .. } => "result_observed",
        _ => return None,
    };
    Some(submission_key(
        &generation.to_string(),
        &submission_key(
            stage,
            &submission_key(frame, &submission_key(submission, attempt)),
        ),
    ))
}

fn observation_gap(observation: &bcode_harness_models::HarnessObservation) -> bool {
    observation.completeness == HarnessCompleteness::Gap
        || matches!(&observation.kind, HarnessObservationKind::Status { status }
            if status.completeness == HarnessCompleteness::Gap)
}

fn evidence_position(
    event: &SessionEvent,
    predicate: impl Fn(&bcode_harness_models::HarnessObservation) -> bool,
) -> SessionDbResult<(usize, bcode_harness_models::HarnessObservation)> {
    let R::ObservationPage { page, .. } = &operation(event)?.record else {
        return Err(invalid());
    };
    let mut matches = page
        .observations
        .iter()
        .enumerate()
        .filter(|(_, v)| predicate(v));
    let (offset, observation) = matches.next().ok_or_else(invalid)?;
    if matches.next().is_some() {
        return Err(invalid());
    }
    Ok((offset, observation.clone()))
}

async fn validate_evidence_coverage(
    db: &dyn Database,
    session: SessionId,
    owner: &ExternalHarnessOwner,
    accepted: &SessionEvent,
) -> SessionDbResult<()> {
    let coverage = latest(db, session, "terminal_coverage_v2", "current")
        .await?
        .ok_or_else(evidence_maintenance_required)?;
    let tail = latest(db, session, "page", "current")
        .await?
        .ok_or_else(invalid)?;
    if coverage.sequence != tail.sequence || operation(&coverage)?.owner != *owner {
        return Err(evidence_maintenance_required());
    }
    let covered_acceptance = db
        .select("turn_evidence")
        .columns(&["event_seq"])
        .where_eq("turn_id", op_key("terminal_coverage_v2", "current"))
        .where_eq("evidence_kind", KIND)
        .where_eq("event_seq", seq_to_value(accepted.sequence))
        .limit(1)
        .execute(db)
        .await?;
    if covered_acceptance.is_empty() {
        return Err(evidence_maintenance_required());
    }
    Ok(())
}

async fn terminal_observations(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
    native_turn_id: &str,
    accepted_position: (u64, usize),
) -> SessionDbResult<Vec<bcode_harness_models::HarnessObservation>> {
    let mut terminal = Vec::new();
    for namespace in ["turn_result", "native_terminal", "settled"] {
        if let Some(event) = latest(db, owner.session_id, namespace, native_turn_id).await? {
            if operation(&event)?.owner != *owner {
                return Err(invalid());
            }
            let (position, observation) = evidence_position(&event, |v| match &v.kind {
                HarnessObservationKind::Result { .. } => {
                    namespace == "turn_result"
                        && v.native_correlations.iter().any(|id| id == native_turn_id)
                }
                HarnessObservationKind::NativeTurnTerminal {
                    native_turn_id: id, ..
                } => namespace == "native_terminal" && id == native_turn_id,
                HarnessObservationKind::Settlement { native_turn_id: id } => {
                    namespace == "settled" && id == native_turn_id
                }
                _ => false,
            })?;
            if (event.sequence, position) <= accepted_position {
                return Err(invalid());
            }
            terminal.push(((event.sequence, position), observation));
        }
    }
    terminal.sort_by_key(|(position, _)| *position);
    Ok(terminal
        .into_iter()
        .map(|(_, observation)| observation)
        .collect())
}

async fn validate_accepted_submission_claim(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
    accepted: &SessionEvent,
    submission_id: &str,
    attempt_id: &str,
) -> SessionDbResult<()> {
    let claim = latest(
        db,
        owner.session_id,
        "submission_claim",
        &submission_key(submission_id, attempt_id),
    )
    .await?
    .ok_or_else(invalid)?;
    let claim_op = operation(&claim)?;
    let R::DeliveryClaimed { request } = &claim_op.record else {
        return Err(invalid());
    };
    if claim_op.owner != *owner
        || claim.sequence >= accepted.sequence
        || !matches!(&request.kind, ExternalHarnessRequestKind::Submit { submission, .. }
            if submission.submission_id == submission_id
                && submission.attempt_id == attempt_id
                && submission.generation == owner.generation)
    {
        return Err(invalid());
    }
    Ok(())
}

impl SessionDb {
    /// Explicitly rebuild managed-harness evidence using the current typed event projector.
    ///
    /// Caller must hold exclusive session maintenance and write guards. This is a maintenance
    /// capability, never a normal read/open/append fallback. Canonical events and unrelated
    /// evidence are unchanged; all harness locators (including coverage) are replaced atomically
    /// at their original event sequences. The migration domain owns version classification,
    /// backup/recovery and writer/schema-fence advancement after successful target validation.
    ///
    /// # Errors
    /// Returns an error for invalid canonical history or typed harness events, database failure,
    /// or transaction failure. A failed rebuild does not publish partial coverage.
    pub async fn reindex_external_harness_evidence(
        &self,
        _maintenance: &crate::lease::SessionMaintenanceGuard,
        _write: &crate::lease::SessionWriteGuard,
    ) -> SessionDbResult<usize> {
        let tx = self.db.begin_transaction().await?;
        super::configure_turso_connection(&*tx).await?;
        let events = super::strict_events(&*tx).await?;
        super::validate_contiguous_canonical_events(&events)?;
        // Validate typed canonical payloads before replacing any derived rows. The existing
        // projector owns every namespace and encoding; maintenance must not duplicate them.
        for event in &events {
            if event.session_id != self.session_id {
                return Err(invalid());
            }
            if matches!(
                event.kind,
                SessionEventKind::ExternalHarnessOperation { .. }
            ) {
                operation(event)?;
            }
        }
        tx.delete("turn_evidence")
            .where_eq("evidence_kind", KIND)
            .execute(&*tx)
            .await?;
        let mut projected = 0;
        for event in &events {
            if matches!(
                event.kind,
                SessionEventKind::ExternalHarnessOperation { .. }
            ) {
                validate_operation(&*tx, event).await?;
                project(&*tx, event).await?;
                projected += 1;
            }
        }
        tx.commit().await?;
        Ok(projected)
    }

    /// Bounded exact submission evidence from canonical indexed events, never a history scan.
    ///
    /// The tuple carries the current owner and at most acceptance, result, native terminal,
    /// and settlement in canonical sequence/page-offset order. Complete requires all four;
    /// a failed/interrupted terminal remains visible in Partial evidence without a result.
    /// `next_after` is the canonical cursor immediately after acceptance. Partial means
    /// missing/incomplete evidence; Gap fences explicit loss anywhere since acceptance, including
    /// uncorrelated status/page loss. None means no configured harness. Historical pages without
    /// terminal-index coverage require explicit maintenance, never implicit repair.
    /// # Errors
    /// Rejects invalid identities, foreign/conflicting evidence, stale indexes and mixed snapshots.
    pub async fn external_harness_submission_evidence(
        &self,
        submission_id: &str,
        attempt_id: &str,
    ) -> SessionDbResult<Option<(ExternalHarnessOwner, HarnessObservationPage)>> {
        if [submission_id, attempt_id].iter().any(|id| {
            id.is_empty()
                || id.len() > bcode_harness_models::MAX_HARNESS_ID_BYTES
                || id.chars().any(char::is_control)
        }) {
            return Err(invalid());
        }
        let db = &**self.db;
        super::external_backend::external_index_ready(db).await?;
        let before = last_sequence(db).await?;
        let Some(config) = latest(db, self.session_id, "config", "current").await? else {
            super::external_backend::external_index_ready(db).await?;
            if last_sequence(db).await? != before {
                return Err(invalid());
            }
            return Ok(None);
        };
        let owner = operation(&config)?.owner.clone();
        state(db, &owner).await?.ok_or_else(invalid)?;
        let mut page = HarnessObservationPage {
            observations: Vec::new(),
            next_after: None,
            completeness: HarnessCompleteness::Partial,
            status: HarnessExecutionStatus {
                connectivity: HarnessConnectivity::Unknown,
                activity: HarnessActivity::Unknown,
                attention: HarnessAttention::Unknown,
                continuation: HarnessContinuation::Unknown,
                completeness: HarnessCompleteness::Partial,
            },
        };
        if let Some(accepted) = latest(
            db,
            self.session_id,
            "submission_acceptance",
            &submission_key(submission_id, attempt_id),
        )
        .await?
        {
            if operation(&accepted)?.owner != owner {
                return Err(invalid());
            }
            let (offset, acceptance) = evidence_position(&accepted, |v| {
                matches!(&v.kind, HarnessObservationKind::SubmissionAccepted {
                    submission_id: id, attempt_id: attempt, ..
                } if id == submission_id && attempt == attempt_id)
            })?;
            let HarnessObservationKind::SubmissionAccepted { native_turn_id, .. } =
                &acceptance.kind
            else {
                return Err(invalid());
            };
            // Coverage is canonical-event checked at both ends. Older projections cannot
            // silently advertise absence of newly indexed result or loss evidence.
            validate_evidence_coverage(db, self.session_id, &owner, &accepted).await?;
            validate_accepted_submission_claim(db, &owner, &accepted, submission_id, attempt_id)
                .await?;
            let terminal =
                terminal_observations(db, &owner, native_turn_id, (accepted.sequence, offset))
                    .await?;
            page.next_after = Some(format!("h1:{}:{}", accepted.sequence, offset + 1));
            page.completeness = if acceptance.completeness == HarnessCompleteness::Complete
                && terminal.len() == 3
                && terminal
                    .iter()
                    .all(|v| v.completeness == HarnessCompleteness::Complete)
            {
                HarnessCompleteness::Complete
            } else {
                HarnessCompleteness::Partial
            };
            page.observations.push(acceptance);
            page.observations.extend(terminal);
            if let Some(gap) = latest(
                db,
                self.session_id,
                "evidence_gap",
                &owner.generation.to_string(),
            )
            .await?
            {
                if operation(&gap)?.owner != owner {
                    return Err(invalid());
                }
                if gap.sequence >= accepted.sequence {
                    page.completeness = HarnessCompleteness::Gap;
                }
            }
        }
        page.status.completeness = page.completeness;
        super::external_backend::external_index_ready(db).await?;
        if last_sequence(db).await? != before {
            return Err(invalid());
        }
        Ok(Some((owner, page)))
    }

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
    if let Some(previous) = &previous {
        let coverage = latest(db, op.owner.session_id, "terminal_coverage_v2", "current")
            .await?
            .ok_or_else(evidence_maintenance_required)?;
        if coverage.sequence != previous.sequence {
            return Err(evidence_maintenance_required());
        }
    }
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

async fn validate_native_terminal(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    observation: &bcode_harness_models::HarnessObservation,
    accepted_turns_in_page: &std::collections::BTreeSet<String>,
    native_terminals_in_page: &mut std::collections::BTreeSet<String>,
) -> SessionDbResult<()> {
    if let HarnessObservationKind::NativeTurnTerminal { native_turn_id, .. } = &observation.kind {
        if observation.completeness != HarnessCompleteness::Complete
            || !native_terminals_in_page.insert(native_turn_id.clone())
            || latest(db, op.owner.session_id, "native_terminal", native_turn_id)
                .await?
                .is_some()
        {
            return Err(invalid());
        }
        if !accepted_turns_in_page.contains(native_turn_id) {
            let accepted = latest(db, op.owner.session_id, "accepted_turn", native_turn_id)
                .await?
                .ok_or_else(invalid)?;
            if operation(&accepted)?.owner != op.owner {
                return Err(invalid());
            }
            evidence_position(&accepted, |v| {
                matches!(&v.kind,
                HarnessObservationKind::SubmissionAccepted { native_turn_id: id, .. }
                    if id == native_turn_id)
            })?;
        }
    }
    Ok(())
}

async fn validate_result_correlations(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    observation: &bcode_harness_models::HarnessObservation,
) -> SessionDbResult<()> {
    for correlation in &observation.native_correlations {
        if matches!(observation.kind, HarnessObservationKind::Result { .. }) {
            let previous = latest(db, op.owner.session_id, "turn_result", correlation).await?;
            if let Some(previous) = previous {
                let (_, retained) = evidence_position(&previous, |v| {
                    matches!(v.kind, HarnessObservationKind::Result { .. })
                        && v.native_correlations.contains(correlation)
                })?;
                if operation(&previous)?.owner != op.owner
                    || (retained.completeness == HarnessCompleteness::Complete
                        && retained.kind != observation.kind)
                {
                    return Err(invalid());
                }
            }
        }
        if latest(db, op.owner.session_id, "settled", correlation)
            .await?
            .is_some()
        {
            return Err(invalid());
        }
    }
    Ok(())
}

async fn validate_interaction_and_binding(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    current: &ExternalHarnessState,
    observation: &bcode_harness_models::HarnessObservation,
    confirmed: &mut Option<bcode_harness_models::HarnessSessionBinding>,
) -> SessionDbResult<()> {
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
        *confirmed = Some(binding.clone());
    }
    Ok(())
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
    let mut results_in_page = std::collections::BTreeSet::new();
    let mut frames_in_page = std::collections::BTreeSet::new();
    let mut accepted_turns_in_page = std::collections::BTreeSet::new();
    let mut native_terminals_in_page = std::collections::BTreeSet::new();
    for observation in &page.observations {
        if let HarnessObservationKind::SubmissionAccepted { native_turn_id, .. } = &observation.kind
            && (!accepted_turns_in_page.insert(native_turn_id.clone())
                || latest(db, op.owner.session_id, "accepted_turn", native_turn_id)
                    .await?
                    .is_some())
        {
            return Err(invalid());
        }
        validate_native_terminal(
            db,
            op,
            observation,
            &accepted_turns_in_page,
            &mut native_terminals_in_page,
        )
        .await?;
        if let Some(key) = submission_frame_key(op.owner.generation, &observation.kind) {
            // Merged native frames may attribute multiple retained attempts. Only the exact
            // generation/stage/frame/submission/attempt tuple is immutable and unique.
            // Replays are handled only by exact retained page input.
            if !frames_in_page.insert(key.clone())
                || latest(db, op.owner.session_id, "submission_native_frame_v2", &key)
                    .await?
                    .is_some()
            {
                return Err(invalid());
            }
            validate_submission_attribution(db, op, observation).await?;
        }
        if matches!(observation.kind, HarnessObservationKind::Result { .. }) {
            for correlation in &observation.native_correlations {
                if !results_in_page.insert(correlation) {
                    return Err(invalid());
                }
            }
        }
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
            validate_submission_attribution(db, op, observation).await?;
        }
        if observation
            .native_correlations
            .iter()
            .any(|id| settled_in_page.contains(id))
        {
            return Err(invalid());
        }
        if let HarnessObservationKind::Settlement { native_turn_id } = &observation.kind
            && (!settled_in_page.insert(native_turn_id.clone())
                || latest(db, op.owner.session_id, "settled", native_turn_id)
                    .await?
                    .is_some())
        {
            return Err(invalid());
        }
        if matches!(observation.kind, HarnessObservationKind::Settlement { .. })
            && observation.completeness != bcode_harness_models::HarnessCompleteness::Complete
        {
            return Err(invalid());
        }
        validate_result_correlations(db, op, observation).await?;
        validate_interaction_and_binding(db, op, current, observation, &mut confirmed).await?;
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

async fn validate_submission_attribution(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    observation: &bcode_harness_models::HarnessObservation,
) -> SessionDbResult<()> {
    let (submission_id, attempt_id): (&str, &str) = match &observation.kind {
        HarnessObservationKind::SubmissionAccepted {
            submission_id,
            attempt_id,
            ..
        } => (submission_id, attempt_id),
        kind => {
            let (submission_id, attempt_id, _) =
                submission_native_frame(kind).ok_or_else(invalid)?;
            (submission_id, attempt_id)
        }
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
    if submission.submission_id != submission_id
        || submission.attempt_id != attempt_id
        || submission.generation != observation.generation
    {
        return Err(invalid());
    }
    if matches!(
        observation.kind,
        HarnessObservationKind::SubmissionAccepted { .. }
    ) && latest(db, op.owner.session_id, "submission_acceptance", &key)
        .await?
        .is_some()
    {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod migration_key_tests {
    use super::*;
    use bcode_session_migration_target::{MigrationReceipt, MigrationTarget as _};
    use std::collections::BTreeMap;

    fn current_fixture_events() -> Vec<SessionEvent> {
        // Current-format input only: historical decoding is tested by session-migration.
        let session = "00000000-0000-0000-0000-000000000046";
        (0..=8)
            .map(|sequence| {
                let kind = if sequence == 0 {
                    serde_json::json!({"session_created": {
                        "name": "current target fixture", "working_directory": "/tmp"
                    }})
                } else {
                        let observations = match sequence {
                            5 => vec![serde_json::json!({"kind": "submission_accepted",
                                "submission_id": "submission", "attempt_id": "attempt",
                                "native_turn_id": "turn"})],
                            6 => vec![serde_json::json!({"kind": "result", "text": "result"})],
                            7 => vec![serde_json::json!({"kind": "settlement", "native_turn_id": "turn"})],
                            _ => Vec::new(),
                        };
                        let observations: Vec<_> = observations.into_iter().map(|kind| {
                            serde_json::json!({
                                "observation_id": format!("observation-{sequence}"),
                                "session_id": session, "generation": 1,
                                "native_correlations": ["turn"], "completeness": "complete",
                                "kind": kind
                            })
                        }).collect();
                        serde_json::json!({"external_harness_operation": {"operation": {
                            "version": 1,
                            "owner": {"session_id": session, "generation": 1,
                                "artifact_id": "fixture-artifact", "daemon_id": "fixture-daemon"},
                            "record": {"kind": "observation_page",
                                "instance": {"instance_id": "fixture", "plugin_id": "fixture",
                                    "executable": "/not-invoked", "config_namespace": "fixture",
                                    "auth": {"provider_id": "fixture", "profile": "native",
                                        "credential_namespace": "fixture", "custody": "native_managed"}},
                                "after": null,
                                "page": {"observations": observations,
                                    "next_after": format!("page-{sequence}"),
                                    "completeness": "complete",
                                    "status": {"connectivity": "connected", "activity": "running",
                                        "attention": "none", "continuation": "available",
                                        "completeness": "complete"}}
                            }
                        }}})
                };
                crate::persisted::decode_session_event(&serde_json::json!({
                    "schema_version": bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                    "sequence": sequence, "timestamp_ms": sequence + 1,
                    "session_id": session, "provenance": null, "kind": kind
                }).to_string()).expect("current target fixture")
            })
            .collect()
    }

    fn target_fixture(with_attribution: bool) -> Vec<SessionEvent> {
        let mut events = current_fixture_events();
        if with_attribution {
            let SessionEventKind::ExternalHarnessOperation { operation } = &mut events[6].kind
            else {
                panic!("fixture result operation");
            };
            let R::ObservationPage { page, .. } = &mut operation.record else {
                panic!("fixture result page");
            };
            let template = page.observations[0].clone();
            for (id, kind) in [
                (
                    "consumed",
                    HarnessObservationKind::SubmissionConsumed {
                        submission_id: "submission".into(),
                        attempt_id: "attempt".into(),
                        native_frame_id: "consumed-frame".into(),
                    },
                ),
                (
                    "observed",
                    HarnessObservationKind::SubmissionResultObserved {
                        submission_id: "submission".into(),
                        attempt_id: "attempt".into(),
                        native_frame_id: "result-frame".into(),
                        is_error: false,
                        queued_user_sends: Some(0),
                    },
                ),
                (
                    "terminal",
                    HarnessObservationKind::NativeTurnTerminal {
                        native_turn_id: "turn".into(),
                        disposition: bcode_harness_models::HarnessTerminalDisposition::Completed,
                    },
                ),
            ] {
                let mut observation = template.clone();
                observation.observation_id = id.into();
                observation.kind = kind;
                page.observations.push(observation);
            }
        }
        events
    }

    async fn seeded_target(root: &std::path::Path, with_attribution: bool) -> SessionDb {
        let events = target_fixture(with_attribution);
        let db = SessionDb::open_turso_in_root(events[0].session_id, root)
            .await
            .expect("open target DB");
        let metrics = bcode_metrics::MetricsRegistry::in_memory();
        let mut target = super::super::SessionMigrationTarget::new(db.database(), &metrics);
        target
            .materialize_current_schema()
            .await
            .expect("materialize target");
        for event in &events {
            super::super::insert_event(db.database(), event, None)
                .await
                .expect("canonical migrated row");
            target
                .ingest_projectors(event)
                .await
                .expect("real target ingestion");
        }
        target
            .finalize_projectors(Some(8))
            .await
            .expect("finalize projectors");
        let facts = target
            .validate_strict_current()
            .await
            .expect("valid exact keys");
        super::super::validate_strict_target_facts(&facts).expect("complete current projections");
        super::super::set_storage_writer_contract(db.database(), 18)
            .await
            .expect("source writer epoch");
        db
    }

    fn receipt() -> MigrationReceipt {
        MigrationReceipt {
            operation_id: "exact-key-fixture".into(),
            session_id: target_fixture(false)[0].session_id,
            source_writer_epoch: 18,
            target_writer_epoch: bcode_session_models::CURRENT_SESSION_STORAGE_WRITER_EPOCH,
            migration_step_ids: vec!["session-writer-epoch-18-to-19".into()],
            source_event_count: 9,
            source_event_tail: Some(8),
            source_payload_digest_sha256: "fixture-source".into(),
            target_event_count: 9,
            target_event_tail: Some(8),
            target_payload_digest_sha256: "fixture-target".into(),
            converted_events: BTreeMap::new(),
            retired_known_events: BTreeMap::new(),
            completed_at_ms: 1,
        }
    }

    async fn assert_finalization_rejected(db: &SessionDb) {
        let epoch = db.storage_writer_epoch().await.expect("writer epoch");
        assert_eq!(epoch, 18);
        let metrics = bcode_metrics::MetricsRegistry::in_memory();
        let mut target = super::super::SessionMigrationTarget::new(db.database(), &metrics);
        assert!(target.validate_strict_current().await.is_err());
        assert!(target.persist_migration_receipt(&receipt()).await.is_err());
        assert!(target.finalize_writer_contract().await.is_err());
        assert_eq!(
            db.storage_writer_epoch().await.expect("writer epoch"),
            epoch
        );
        assert!(
            db.database()
                .select("session_migration_receipts")
                .columns(&["operation_id"])
                .where_eq("operation_id", "exact-key-fixture")
                .execute_first(db.database())
                .await
                .expect("receipt query")
                .is_none()
        );
    }

    #[tokio::test]
    async fn migration_target_rejects_missing_exact_harness_keys() {
        let events = target_fixture(true);
        let mut locators = vec![
            op_key("terminal_coverage_v2", "current"),
            op_key("native_terminal", "turn"),
        ];
        locators.extend(
            keys(operation(&events[6]).expect("canonical operation"))
                .into_iter()
                .filter(|(namespace, _)| namespace == "submission_native_frame_v2")
                .map(|(namespace, id)| op_key(&namespace, &id)),
        );
        assert_eq!(
            locators.len(),
            4,
            "coverage, terminal, consumed and result attribution"
        );
        for key in locators {
            let root = tempfile::tempdir().expect("temp root");
            let db = seeded_target(root.path(), true).await;
            db.database()
                .delete("turn_evidence")
                .where_eq("evidence_kind", KIND)
                .where_eq("turn_id", key)
                .execute(db.database())
                .await
                .expect("remove locator");
            assert_finalization_rejected(&db).await;
        }
    }

    #[tokio::test]
    async fn migration_target_rejects_corrupt_exact_harness_keys() {
        for column in ["event_seq", "segment_order", "turn_id"] {
            let root = tempfile::tempdir().expect("temp root");
            let db = seeded_target(root.path(), true).await;
            let query = db
                .database()
                .update("turn_evidence")
                .where_eq("evidence_kind", KIND)
                .where_eq("turn_id", op_key("terminal_coverage_v2", "current"))
                .where_eq("event_seq", seq_to_value(6));
            if column == "turn_id" {
                query
                    .value(column, "corrupt-key")
                    .execute(db.database())
                    .await
                    .expect("corrupt key");
            } else {
                query
                    .value(column, seq_to_value(0))
                    .execute(db.database())
                    .await
                    .expect("corrupt locator");
            }
            assert_finalization_rejected(&db).await;
        }
    }

    #[tokio::test]
    async fn migration_target_key_failure_rolls_back_without_publication() {
        let root = tempfile::tempdir().expect("temp root");
        let db = seeded_target(root.path(), true).await;
        let tx = db
            .database()
            .begin_transaction()
            .await
            .expect("migration transaction");
        tx.delete("turn_evidence")
            .where_eq("evidence_kind", KIND)
            .where_eq("turn_id", op_key("terminal_coverage_v2", "current"))
            .execute(&*tx)
            .await
            .expect("damage target in transaction");
        let metrics = bcode_metrics::MetricsRegistry::in_memory();
        let mut target = super::super::SessionMigrationTarget::new(&*tx, &metrics);
        assert!(target.validate_strict_current().await.is_err());
        assert!(target.persist_migration_receipt(&receipt()).await.is_err());
        assert!(target.finalize_writer_contract().await.is_err());
        tx.rollback().await.expect("rollback failed migration");
        validate_migration_keys(db.database())
            .await
            .expect("original keys restored");
        assert_eq!(db.storage_writer_epoch().await.expect("writer epoch"), 18);
        assert!(
            db.database()
                .select("session_migration_receipts")
                .columns(&["operation_id"])
                .where_eq("operation_id", "exact-key-fixture")
                .execute_first(db.database())
                .await
                .expect("receipt query")
                .is_none()
        );
    }

    #[tokio::test]
    async fn migration_target_preserves_absence_of_terminal_evidence() {
        let root = tempfile::tempdir().expect("temp root");
        let db = seeded_target(root.path(), false).await;
        for namespace in ["native_terminal", "submission_native_frame_v2"] {
            let rows = db
                .database()
                .select("turn_evidence")
                .columns(&["turn_id"])
                .where_eq("evidence_kind", KIND)
                .execute(db.database())
                .await
                .expect("locator rows");
            let prefix = op_key(namespace, "");
            assert!(rows.iter().all(|row| {
                !super::super::required_string(row, "turn_id")
                    .expect("key")
                    .starts_with(&prefix)
            }));
        }
        let metrics = bcode_metrics::MetricsRegistry::in_memory();
        let mut target = super::super::SessionMigrationTarget::new(db.database(), &metrics);
        target
            .persist_migration_receipt(&receipt())
            .await
            .expect("valid legacy receipt");
        target
            .finalize_writer_contract()
            .await
            .expect("valid legacy writer");
        assert_eq!(
            db.storage_writer_epoch().await.expect("writer epoch"),
            u64::from(bcode_session_models::CURRENT_SESSION_STORAGE_WRITER_EPOCH)
        );
        for (namespace, id, sequence) in [
            (
                "submission_acceptance",
                submission_key("submission", "attempt"),
                5,
            ),
            ("accepted_turn", "turn".into(), 5),
            ("turn_result", "turn".into(), 6),
            ("settled", "turn".into(), 7),
            ("terminal_coverage_v2", "current".into(), 8),
        ] {
            let row = db
                .database()
                .select("turn_evidence")
                .columns(&["event_seq"])
                .where_eq("evidence_kind", KIND)
                .where_eq("turn_id", op_key(namespace, &id))
                .where_eq("event_seq", seq_to_value(sequence))
                .execute_first(db.database())
                .await
                .expect("legacy key query");
            assert!(row.is_some(), "missing legacy {namespace} evidence");
        }
        db.database()
            .insert("turn_evidence")
            .value("turn_id", op_key("native_terminal", "turn"))
            .value("evidence_kind", KIND)
            .value("event_seq", seq_to_value(6))
            .value("segment_order", seq_to_value(6))
            .execute(db.database())
            .await
            .expect("invent terminal locator");
        assert!(target.validate_strict_current().await.is_err());
        assert!(target.finalize_writer_contract().await.is_err());
    }
}
