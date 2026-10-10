//! Canonical milestone validation and derived bounded queries.
use crate::db::{SessionDb, SessionDbError, SessionDbResult};
use crate::db_event_store::seq_to_value;
use crate::db_row::{required_non_negative_u64, required_string};
use bcode_session_models::{
    SessionEvent, SessionEventKind, SessionId, SessionMilestone, SessionMilestoneContext,
    SessionMilestoneEntry, SessionMilestoneEvaluationCompletion, SessionMilestoneEvaluationRequest,
    SessionMilestoneEvaluationStatus, SessionMilestonePage, SessionMilestonePublicationRequest,
    SessionMilestoneQuery, SessionMilestoneSource, TurnId, TurnPriority,
    estimated_normalized_text_tokens,
};
use switchy::database::{
    Database,
    query::{FilterableQuery, SortDirection},
};

#[cfg(test)]
mod tests;

fn invalid(message: &str) -> SessionDbError {
    SessionDbError::InvalidData(message.to_owned())
}

/// Validate a bounded, nonempty evaluation identity.
/// # Errors
/// Rejects blank identities and identities longer than 256 bytes.
pub fn validate_id(id: &str) -> SessionDbResult<()> {
    if id.trim().is_empty() || id.len() > 256 {
        return Err(invalid("invalid milestone evaluation identity"));
    }
    Ok(())
}

async fn conversational_session(db: &dyn Database, session_id: SessionId) -> SessionDbResult<bool> {
    let row = db
        .select("session_state")
        .columns(&["visibility", "execution_provenance"])
        .where_eq("session_id", session_id.to_string())
        .execute_first(db)
        .await?
        .ok_or_else(|| invalid("missing milestone session classification"))?;
    let visibility = crate::db_row::optional_string(&row, "visibility")
        .map(|value| serde_json::from_str::<bcode_session_models::SessionVisibility>(&value))
        .transpose()?
        .unwrap_or_default();
    Ok(
        visibility == bcode_session_models::SessionVisibility::Visible
            && crate::db_row::optional_string(&row, "execution_provenance").is_none(),
    )
}

async fn validate_conversational_session(
    db: &dyn Database,
    session_id: SessionId,
) -> SessionDbResult<()> {
    if !conversational_session(db, session_id).await? {
        return Err(invalid(
            "milestone generation requires a conversational session",
        ));
    }
    Ok(())
}

async fn event_at(db: &dyn Database, sequence: u64) -> SessionDbResult<SessionEvent> {
    let row = db
        .select("events")
        .columns(&["payload"])
        .where_eq("event_seq", seq_to_value(sequence))
        .execute_first(db)
        .await?
        .ok_or_else(|| invalid("missing milestone source event"))?;
    Ok(crate::persisted::decode_session_event(&required_string(
        &row, "payload",
    )?)?)
}

/// Validate exact canonical anchors and a settled end, without replaying the range.
/// # Errors
/// Rejects invalid geometry, foreign or ineligible events, and unsettled boundaries;
/// propagates storage and decoding failures.
pub async fn validate_source(
    db: &dyn Database,
    source: &SessionMilestoneSource,
    session_id: SessionId,
) -> SessionDbResult<()> {
    validate_conversational_session(db, session_id).await?;
    if !source.is_valid() {
        return Err(invalid("invalid milestone source geometry"));
    }
    let start = event_at(db, source.start_sequence).await?;
    let anchor = event_at(db, source.anchor_sequence).await?;
    let end = event_at(db, source.end_sequence).await?;
    if [start.session_id, anchor.session_id, end.session_id]
        .iter()
        .any(|id| *id != session_id)
    {
        return Err(invalid("source session mismatch"));
    }
    let row = db
        .select("milestone_turns")
        .columns(&["start_seq", "end_seq"])
        .where_eq("eligible", 1_i64)
        .where_eq("start_seq", seq_to_value(source.start_sequence))
        .limit(1)
        .execute_first(db)
        .await?;
    if row.is_none() {
        return Err(invalid(
            "milestone source must start at an eligible conversational turn boundary",
        ));
    }
    let start_turn = row.ok_or_else(|| invalid("missing eligible source start"))?;
    let start_end = required_non_negative_u64(&start_turn, "end_seq")?;
    if start_end > source.end_sequence {
        return Err(invalid("source end precedes settled source start turn"));
    }
    let row = db
        .select("milestone_turns")
        .columns(&["start_seq", "end_seq"])
        .where_eq("eligible", 1_i64)
        .where_eq("end_seq", seq_to_value(source.end_sequence))
        .limit(1)
        .execute_first(db)
        .await?;
    if row.is_none() {
        return Err(invalid(
            "milestone source must end at an eligible settled turn boundary",
        ));
    }
    let end_turn = row.ok_or_else(|| invalid("missing eligible source end"))?;
    let SessionEventKind::ModelTurnFinished {
        turn_id: end_id, ..
    } = &end.kind
    else {
        return Err(invalid("invalid source terminal"));
    };
    let terminal_turn = db
        .select("milestone_turns")
        .columns(&["turn_id"])
        .where_eq("end_seq", seq_to_value(source.end_sequence))
        .where_eq("eligible", 1_i64)
        .limit(1)
        .execute_first(db)
        .await?
        .ok_or_else(|| invalid("missing source terminal turn"))?;
    if required_string(&terminal_turn, "turn_id")? != *end_id
        || required_non_negative_u64(&end_turn, "start_seq")? < source.start_sequence
    {
        return Err(invalid("source terminal turn identity mismatch"));
    }
    if !matches!(end.kind, SessionEventKind::ModelTurnFinished { .. })
        || !matches!(
            start.kind,
            SessionEventKind::UserMessage { .. } | SessionEventKind::SubmissionActivated { .. }
        )
    {
        return Err(invalid(
            "milestone source does not identify conversational turn boundaries",
        ));
    }
    let start_turn = db
        .select("milestone_turns")
        .columns(&["turn_id"])
        .where_eq("start_seq", seq_to_value(source.start_sequence))
        .where_eq("eligible", 1_i64)
        .limit(1)
        .execute_first(db)
        .await?
        .ok_or_else(|| invalid("missing eligible source start"))?;
    let start_id = match &start.kind {
        SessionEventKind::SubmissionActivated { turn_id, .. } => turn_id.to_string(),
        SessionEventKind::UserMessage { .. } => {
            TurnId::from_accepted_event(session_id, start.sequence).to_string()
        }
        _ => return Err(invalid("invalid conversational source start")),
    };
    if required_string(&start_turn, "turn_id")? != start_id {
        return Err(invalid("source start turn identity mismatch"));
    }
    validate_anchor(db, source, session_id, &anchor).await?;
    Ok(())
}

async fn validate_anchor(
    db: &dyn Database,
    source: &SessionMilestoneSource,
    session_id: SessionId,
    anchor: &SessionEvent,
) -> SessionDbResult<()> {
    if !matches!(
        anchor.kind,
        SessionEventKind::UserMessage { .. }
            | SessionEventKind::SubmissionActivated { .. }
            | SessionEventKind::AssistantResponseSegment { .. }
            | SessionEventKind::PositionedAssistantResponseSegment { .. }
    ) {
        return Err(invalid(
            "milestone navigation anchor must be conversational",
        ));
    }
    let row = db
        .select("milestone_turns")
        .columns(&["start_seq", "end_seq", "turn_id"])
        .where_eq("eligible", 1_i64)
        .where_lte("start_seq", seq_to_value(source.anchor_sequence))
        .where_gte("end_seq", seq_to_value(source.anchor_sequence))
        .limit(1)
        .execute_first(db)
        .await?;
    if row.is_none() {
        return Err(invalid(
            "milestone anchor belongs to an ineligible or unsettled turn",
        ));
    }
    let anchor_turn = row.ok_or_else(|| invalid("missing eligible anchor turn"))?;
    if required_non_negative_u64(&anchor_turn, "start_seq")? < source.start_sequence
        || required_non_negative_u64(&anchor_turn, "end_seq")? > source.end_sequence
    {
        return Err(invalid("anchor turn is outside source boundaries"));
    }
    let anchor_turn_id = required_string(&anchor_turn, "turn_id")?;
    let matches_turn = match &anchor.kind {
        SessionEventKind::SubmissionActivated { turn_id, .. } => {
            turn_id.to_string() == anchor_turn_id
        }
        SessionEventKind::UserMessage { .. } => {
            TurnId::from_accepted_event(session_id, anchor.sequence).to_string() == anchor_turn_id
        }
        SessionEventKind::AssistantResponseSegment { turn_id, .. }
        | SessionEventKind::PositionedAssistantResponseSegment { turn_id, .. } => {
            *turn_id == anchor_turn_id
        }
        _ => false,
    };
    if !matches_turn {
        return Err(invalid(
            "anchor event does not belong to eligible settled turn",
        ));
    }
    Ok(())
}

async fn latest_kind_sequence(db: &dyn Database, kind: &str) -> SessionDbResult<u64> {
    db.select("events")
        .columns(&["event_seq"])
        .where_eq("event_type", kind)
        .sort("event_seq", SortDirection::Desc)
        .limit(1)
        .execute_first(db)
        .await?
        .as_ref()
        .map(|row| required_non_negative_u64(row, "event_seq"))
        .transpose()
        .map(|seq| seq.unwrap_or(0))
}

async fn evaluation(
    db: &dyn Database,
    id: &str,
) -> SessionDbResult<(u64, SessionMilestoneEvaluationRequest, bool)> {
    let row = db
        .select("milestone_evaluations")
        .columns(&["admitted_seq", "request_json", "completed"])
        .where_eq("evaluation_id", id)
        .execute_first(db)
        .await?
        .ok_or_else(|| invalid("milestone evaluation was not admitted"))?;
    Ok((
        required_non_negative_u64(&row, "admitted_seq")?,
        serde_json::from_str(&required_string(&row, "request_json")?)?,
        required_non_negative_u64(&row, "completed")? != 0,
    ))
}

async fn validate_active(
    db: &dyn Database,
    id: &str,
) -> SessionDbResult<(u64, SessionMilestoneEvaluationRequest)> {
    let (sequence, request, completed) = evaluation(db, id).await?;
    if completed || sequence != latest_kind_sequence(db, "milestone_evaluation_requested").await? {
        return Err(invalid("stale or completed milestone evaluation"));
    }
    Ok((sequence, request))
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct MilestoneSchedulingState {
    turns: u64,
    text_tokens: u64,
    evaluation_count: u64,
    published_through: u64,
    evaluated_turns: u64,
    evaluated_tokens: u64,
    evaluated_through: u64,
    last_evaluation_at_ms: Option<u64>,
}
async fn scheduling_state(db: &dyn Database) -> SessionDbResult<MilestoneSchedulingState> {
    let row = db
        .select("milestone_scheduling")
        .columns(&["record"])
        .where_eq("singleton", 1_i64)
        .execute_first(db)
        .await?;
    row.as_ref()
        .map(|row| Ok(serde_json::from_str(&required_string(row, "record")?)?))
        .transpose()
        .map(Option::unwrap_or_default)
}
async fn save_scheduling_state(
    db: &dyn Database,
    state: &MilestoneSchedulingState,
) -> SessionDbResult<()> {
    db.upsert("milestone_scheduling")
        .value("singleton", 1_i64)
        .value("record", serde_json::to_string(state)?)
        .execute(db)
        .await?;
    Ok(())
}
fn conversational_admission(admission: &bcode_session_models::TurnAdmissionMetadata) -> bool {
    admission.priority == TurnPriority::Interactive
        && admission.execution.correlation.is_none()
        && admission.origin.is_none()
}

const fn conversational_turn_event(event: &SessionEvent) -> bool {
    matches!(
        event.kind,
        SessionEventKind::UserMessage { .. }
            | SessionEventKind::SubmissionActivated { .. }
            | SessionEventKind::AssistantResponseSegment { .. }
            | SessionEventKind::PositionedAssistantResponseSegment { .. }
            | SessionEventKind::ModelTurnFinished { .. }
    )
}

async fn project_conversation(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
    if conversational_turn_event(event) && !conversational_session(db, event.session_id).await? {
        return Ok(());
    }
    let mut state = scheduling_state(db).await?;
    match &event.kind {
        SessionEventKind::SubmissionActivated {
            turn_id,
            text,
            admission,
            ..
        } => {
            let row = db
                .select("milestone_turns")
                .columns(&["turn_id"])
                .where_eq("turn_id", turn_id.to_string())
                .execute_first(db)
                .await?;
            if row.is_none() {
                db.insert("milestone_turns")
                    .value("turn_id", turn_id.to_string())
                    .value("start_seq", seq_to_value(event.sequence))
                    .value("eligible", i64::from(conversational_admission(admission)))
                    .value(
                        "text_tokens",
                        seq_to_value(estimated_normalized_text_tokens(text)),
                    )
                    .execute(db)
                    .await?;
            }
        }
        SessionEventKind::UserMessage {
            text, admission, ..
        } => {
            db.insert("milestone_turns")
                .value(
                    "turn_id",
                    format!(
                        "{}",
                        TurnId::from_accepted_event(event.session_id, event.sequence)
                    ),
                )
                .value("start_seq", seq_to_value(event.sequence))
                .value("eligible", i64::from(conversational_admission(admission)))
                .value(
                    "text_tokens",
                    seq_to_value(estimated_normalized_text_tokens(text)),
                )
                .execute(db)
                .await?;
        }
        SessionEventKind::AssistantResponseSegment { turn_id, text, .. }
        | SessionEventKind::PositionedAssistantResponseSegment { turn_id, text, .. } => {
            if let Some(row) = db
                .select("milestone_turns")
                .columns(&["text_tokens", "end_seq"])
                .where_eq("turn_id", turn_id.clone())
                .execute_first(db)
                .await?
                && crate::db_row::optional_i64(&row, "end_seq").is_none()
            {
                let tokens = required_non_negative_u64(&row, "text_tokens")?
                    .saturating_add(estimated_normalized_text_tokens(text));
                db.update("milestone_turns")
                    .value("text_tokens", seq_to_value(tokens))
                    .where_eq("turn_id", turn_id.clone())
                    .execute(db)
                    .await?;
            }
        }
        SessionEventKind::ModelTurnFinished { turn_id, .. } => {
            if let Some(row) = db
                .select("milestone_turns")
                .columns(&["eligible", "text_tokens", "end_seq"])
                .where_eq("turn_id", turn_id.clone())
                .execute_first(db)
                .await?
                && crate::db_row::optional_i64(&row, "end_seq").is_none()
            {
                db.update("milestone_turns")
                    .value("end_seq", seq_to_value(event.sequence))
                    .where_eq("turn_id", turn_id.clone())
                    .execute(db)
                    .await?;
                if required_non_negative_u64(&row, "eligible")? != 0 {
                    state.turns = state.turns.saturating_add(1);
                    state.text_tokens = state
                        .text_tokens
                        .saturating_add(required_non_negative_u64(&row, "text_tokens")?);
                    save_scheduling_state(db, &state).await?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Apply canonical milestone and scheduling facts to current derived storage.
/// # Errors
/// Rejects invalid, stale, or conflicting milestone facts and propagates storage failures.
pub async fn project(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
    project_conversation(db, event).await?;
    match &event.kind {
        SessionEventKind::MilestoneEvaluationRequested { request } => {
            project_evaluation_admission(db, event, request).await?;
        }
        SessionEventKind::MilestonePublicationRequested { request } => {
            project_publication_request(db, request).await?;
        }
        SessionEventKind::MilestonePublished { milestone } => {
            project_publication(db, event, milestone).await?;
        }
        SessionEventKind::MilestoneEvaluationCompleted { completion } => {
            project_evaluation_completion(db, event, completion).await?;
        }
        SessionEventKind::AutomaticTitleUpdated {
            evaluation_id,
            title,
        } => {
            project_automatic_title(db, event, evaluation_id, title).await?;
        }
        _ => {}
    }
    Ok(())
}

async fn project_evaluation_admission(
    db: &dyn Database,
    event: &SessionEvent,
    request: &SessionMilestoneEvaluationRequest,
) -> SessionDbResult<()> {
    validate_id(&request.evaluation_id)?;
    if request.expected_generation != event.sequence.saturating_sub(1) {
        return Err(invalid("stale milestone admission generation"));
    }
    validate_source(db, &request.source, event.session_id).await?;
    if request.source.end_sequence > request.expected_generation {
        return Err(invalid("milestone source exceeds admission generation"));
    }
    let state = db
        .select("session_state")
        .columns(&["explicit_name"])
        .where_eq("session_id", event.session_id.to_string())
        .execute_first(db)
        .await?;
    if state
        .as_ref()
        .and_then(|row| crate::db_row::optional_string(row, "explicit_name"))
        != request.expected_explicit_name
    {
        return Err(invalid("explicit name changed before evaluation admission"));
    }
    if request.source.end_sequence <= scheduling_state(db).await?.published_through {
        return Err(invalid("milestone source already published"));
    }
    let latest_settled = db
        .select("milestone_turns")
        .columns(&["end_seq"])
        .where_eq("eligible", 1_i64)
        .where_gt("end_seq", 0_i64)
        .sort("end_seq", SortDirection::Desc)
        .limit(1)
        .execute_first(db)
        .await?
        .ok_or_else(|| invalid("missing settled eligible source"))?;
    if required_non_negative_u64(&latest_settled, "end_seq")? != request.source.end_sequence {
        return Err(invalid(
            "evaluation source must cover latest settled eligible turn",
        ));
    }
    let mut state = scheduling_state(db).await?;
    if request
        .max_requests_per_session
        .is_some_and(|cap| state.evaluation_count >= cap)
    {
        return Err(invalid("milestone evaluation request budget exhausted"));
    }
    state.evaluation_count = state.evaluation_count.saturating_add(1);
    save_scheduling_state(db, &state).await?;
    db.insert("milestone_evaluations")
        .value("evaluation_id", request.evaluation_id.clone())
        .value("admitted_seq", seq_to_value(event.sequence))
        .value("completed", 0_i64)
        .value("turns_at_admission", seq_to_value(state.turns))
        .value("tokens_at_admission", seq_to_value(state.text_tokens))
        .value("request_json", serde_json::to_string(request)?)
        .execute(db)
        .await?;

    Ok(())
}

async fn project_publication_request(
    db: &dyn Database,
    request: &SessionMilestonePublicationRequest,
) -> SessionDbResult<()> {
    validate_active(db, &request.milestone.evaluation_id).await?;
    let row = db
        .select("milestone_evaluations")
        .columns(&["publication_json"])
        .where_eq("evaluation_id", request.milestone.evaluation_id.clone())
        .execute_first(db)
        .await?;
    if row
        .as_ref()
        .and_then(|row| crate::db_row::optional_string(row, "publication_json"))
        .is_some()
    {
        return Err(invalid("duplicate milestone publication request"));
    }
    if request
        .automatic_title
        .as_ref()
        .is_some_and(|title| title.trim().is_empty() || title.len() > 512)
    {
        return Err(invalid("invalid automatic title"));
    }
    db.update("milestone_evaluations")
        .value("publication_json", serde_json::to_string(request)?)
        .where_eq("evaluation_id", request.milestone.evaluation_id.clone())
        .execute(db)
        .await?;

    Ok(())
}

async fn project_publication(
    db: &dyn Database,
    event: &SessionEvent,
    milestone: &SessionMilestone,
) -> SessionDbResult<()> {
    let (_, admitted) = validate_active(db, &milestone.evaluation_id).await?;
    if let Some(row) = db
        .select("milestone_evaluations")
        .columns(&["publication_json"])
        .where_eq("evaluation_id", milestone.evaluation_id.clone())
        .execute_first(db)
        .await?
    {
        let publication: SessionMilestonePublicationRequest =
            serde_json::from_str(&required_string(&row, "publication_json")?)?;
        if publication.milestone != *milestone {
            return Err(invalid(
                "milestone differs from durable publication request",
            ));
        }
    }
    let request = admitted;
    if milestone.source != request.source
        || milestone.header.trim().is_empty()
        || milestone.header.len() > 512
        || milestone.description.trim().is_empty()
        || milestone.description.len() > 16 * 1024
        || milestone.provenance.provider_id.trim().is_empty()
        || milestone.provenance.provider_id.len() > 256
        || milestone.provenance.model_id.trim().is_empty()
        || milestone.provenance.model_id.len() > 256
    {
        return Err(invalid("invalid milestone publication"));
    }
    validate_source(db, &milestone.source, event.session_id).await?;
    let mut state = scheduling_state(db).await?;
    state.published_through = milestone.source.end_sequence;
    save_scheduling_state(db, &state).await?;
    db.insert("session_milestones")
        .value("evaluation_id", milestone.evaluation_id.clone())
        .value("timestamp_ms", seq_to_value(event.timestamp_ms))
        .value("published_seq", seq_to_value(event.sequence))
        .value("milestone_json", serde_json::to_string(milestone)?)
        .execute(db)
        .await?;

    Ok(())
}

async fn project_evaluation_completion(
    db: &dyn Database,
    event: &SessionEvent,
    completion: &SessionMilestoneEvaluationCompletion,
) -> SessionDbResult<()> {
    let (_, request) = validate_active(db, &completion.evaluation_id).await?;
    let mut state = scheduling_state(db).await?;
    state.evaluated_through = request.source.end_sequence;
    let admitted = db
        .select("milestone_evaluations")
        .columns(&["turns_at_admission", "tokens_at_admission"])
        .where_eq("evaluation_id", completion.evaluation_id.clone())
        .execute_first(db)
        .await?
        .ok_or_else(|| invalid("missing admission snapshot"))?;
    state.evaluated_turns = required_non_negative_u64(&admitted, "turns_at_admission")?;
    state.evaluated_tokens = required_non_negative_u64(&admitted, "tokens_at_admission")?;
    state.last_evaluation_at_ms = Some(event.timestamp_ms);
    save_scheduling_state(db, &state).await?;
    db.update("milestone_evaluations")
        .value("completed", 1_i64)
        .where_eq("evaluation_id", completion.evaluation_id.clone())
        .execute(db)
        .await?;

    Ok(())
}

async fn project_automatic_title(
    db: &dyn Database,
    event: &SessionEvent,
    evaluation_id: &str,
    title: &str,
) -> SessionDbResult<()> {
    let (admitted, request) = validate_active(db, evaluation_id).await?;
    if request.expected_explicit_name.is_some() {
        return Err(invalid("automatic title cannot replace an explicit name"));
    }
    let state = db
        .select("session_state")
        .columns(&["explicit_name"])
        .where_eq("session_id", event.session_id.to_string())
        .execute_first(db)
        .await?;
    if state
        .as_ref()
        .and_then(|row| crate::db_row::optional_string(row, "explicit_name"))
        .is_some()
    {
        return Err(invalid("automatic title cannot replace an explicit name"));
    }
    if title.trim().is_empty() || title.len() > 512 {
        return Err(invalid("invalid automatic title"));
    }
    if latest_kind_sequence(db, "session_renamed").await? > admitted {
        return Err(invalid("automatic title fenced by explicit rename"));
    }
    let publication = db
        .select("milestone_evaluations")
        .columns(&["publication_json"])
        .where_eq("evaluation_id", evaluation_id.to_owned())
        .execute_first(db)
        .await?
        .ok_or_else(|| invalid("missing publication request"))?;
    let publication: SessionMilestonePublicationRequest =
        serde_json::from_str(&required_string(&publication, "publication_json")?)?;
    if publication.automatic_title.as_deref() != Some(title) {
        return Err(invalid(
            "automatic title differs from exact publication request",
        ));
    }
    let prior_title = db
        .select("events")
        .columns(&["event_seq"])
        .where_eq("event_type", "automatic_title_updated")
        .where_gt("event_seq", seq_to_value(admitted))
        .where_lt("event_seq", seq_to_value(event.sequence))
        .limit(1)
        .execute_first(db)
        .await?;
    if prior_title.is_some() {
        return Err(invalid("automatic title already published for evaluation"));
    }
    let published = db
        .select("session_milestones")
        .columns(&["published_seq"])
        .where_eq("evaluation_id", evaluation_id.to_owned())
        .execute_first(db)
        .await?;
    if published.is_none() {
        return Err(invalid("automatic title requires published milestone"));
    }
    db.update("session_state")
        .value("automatic_title", title.to_owned())
        .value(
            "automatic_title_source_sequence",
            seq_to_value(event.sequence),
        )
        .where_eq("session_id", event.session_id.to_string())
        .execute(db)
        .await?;

    Ok(())
}

async fn capture_context(
    db: &dyn Database,
    session_id: SessionId,
    max_bytes: usize,
    max_turns: usize,
) -> SessionDbResult<SessionMilestoneContext> {
    validate_conversational_session(db, session_id).await?;
    let generation = crate::db_event_store::last_sequence(db).await?.unwrap_or(0);
    let state = scheduling_state(db).await?;
    let explicit_name = db
        .select("session_state")
        .columns(&["explicit_name"])
        .where_eq("session_id", session_id.to_string())
        .execute_first(db)
        .await?
        .as_ref()
        .and_then(|row| crate::db_row::optional_string(row, "explicit_name"));
    let new_turns =
        u32::try_from(state.turns.saturating_sub(state.evaluated_turns)).unwrap_or(u32::MAX);
    let new_tokens = state.text_tokens.saturating_sub(state.evaluated_tokens);
    let mut context = SessionMilestoneContext {
        source: None,
        generation,
        explicit_name,
        text: String::new(),
        truncated: false,
        new_turns,
        new_tokens,
        last_evaluation_at_ms: state.last_evaluation_at_ms,
        evaluation_count: state.evaluation_count,
    };
    let latest = db
        .select("milestone_turns")
        .columns(&["end_seq"])
        .where_eq("eligible", 1_i64)
        .where_gt("end_seq", 0_i64)
        .sort("end_seq", SortDirection::Desc)
        .limit(1)
        .execute_first(db)
        .await?;
    let end = latest.as_ref().map_or(0, |row| {
        crate::db_row::optional_i64(row, "end_seq")
            .unwrap_or(0)
            .cast_unsigned()
    });
    if end <= state.published_through {
        return Ok(context);
    }
    let turns = db
        .select("milestone_turns")
        .columns(&["start_seq", "end_seq", "turn_id"])
        .where_eq("eligible", 1_i64)
        .where_gt("end_seq", seq_to_value(state.published_through))
        .where_lte("end_seq", seq_to_value(end))
        .sort("end_seq", SortDirection::Desc)
        .limit(max_turns + 1)
        .execute(db)
        .await?;
    context.truncated = turns.len() > max_turns;
    let selected = &turns[..turns.len().min(max_turns)];
    let lower = selected
        .last()
        .map(|row| required_non_negative_u64(row, "start_seq"))
        .transpose()?
        .unwrap_or(end)
        .saturating_sub(1);
    populate_context_text(
        db,
        session_id,
        &mut context,
        selected,
        lower,
        end,
        max_bytes,
    )
    .await?;
    Ok(context)
}

async fn populate_context_text(
    db: &dyn Database,
    session_id: SessionId,
    context: &mut SessionMilestoneContext,
    selected: &[switchy::database::Row],
    lower: u64,
    end: u64,
    max_bytes: usize,
) -> SessionDbResult<()> {
    let rows = db
        .select("events")
        .columns(&["payload"])
        .where_in(
            "event_type",
            vec![
                "user_message",
                "submission_activated",
                "assistant_message",
                "assistant_response_segment",
                "positioned_assistant_response_segment",
            ],
        )
        .where_gt("event_seq", seq_to_value(lower))
        .where_lte("event_seq", seq_to_value(end))
        .sort("event_seq", SortDirection::Desc)
        .limit(401)
        .execute(db)
        .await?;
    context.truncated |= rows.len() > 400;
    let outcomes = context_outcomes(db, selected).await?;
    if outcomes.len() > max_bytes {
        context.truncated = true;
        return Ok(());
    }
    let mut chunks = Vec::new();
    let mut used = outcomes.len();
    let mut start = None;
    for row in rows.iter().take(400) {
        let event = crate::persisted::decode_session_event(&required_string(row, "payload")?)?;
        if event.session_id != session_id {
            return Err(invalid("milestone context session identity mismatch"));
        }
        let eligible = match &event.kind {
            SessionEventKind::SubmissionActivated { turn_id, .. } => selected.iter().any(|row| {
                required_string(row, "turn_id").is_ok_and(|id| id == turn_id.to_string())
            }),
            SessionEventKind::UserMessage { .. } => selected.iter().any(|row| {
                required_non_negative_u64(row, "start_seq")
                    .is_ok_and(|start| start == event.sequence)
            }),
            SessionEventKind::AssistantResponseSegment { turn_id, .. }
            | SessionEventKind::PositionedAssistantResponseSegment { turn_id, .. } => selected
                .iter()
                .any(|row| required_string(row, "turn_id").is_ok_and(|id| id == *turn_id)),
            _ => false,
        };
        if !eligible {
            continue;
        }
        let (role, text) = match &event.kind {
            SessionEventKind::UserMessage { text, .. }
            | SessionEventKind::SubmissionActivated { text, .. } => ("user", text),
            SessionEventKind::AssistantMessage { text }
            | SessionEventKind::AssistantResponseSegment { text, .. }
            | SessionEventKind::PositionedAssistantResponseSegment { text, .. } => {
                ("assistant", text)
            }
            _ => return Err(invalid("non-conversational milestone context event")),
        };
        let prefix = format!("[{role} sequence={}]\n", event.sequence);
        let available = max_bytes.saturating_sub(used + prefix.len() + 1);
        if available == 0 {
            context.truncated = true;
            break;
        }
        let mut length = available.min(text.len());
        while !text.is_char_boundary(length) {
            length -= 1;
        }
        chunks.push(format!("{prefix}{}\n", &text[..length]));
        used += prefix.len() + length + 1;
        start = Some(event.sequence);
        if length < text.len() {
            context.truncated = true;
            break;
        }
    }
    chunks.reverse();
    context.text = format!("{}{outcomes}", chunks.concat());
    if let Some(start_sequence) = start {
        context.source = Some(SessionMilestoneSource {
            start_sequence: lower.saturating_add(1),
            end_sequence: end,
            anchor_sequence: start_sequence,
        });
    }

    Ok(())
}

async fn context_outcomes(
    db: &dyn Database,
    selected: &[switchy::database::Row],
) -> SessionDbResult<String> {
    let mut outcomes = Vec::new();
    for row in selected {
        let terminal = event_at(db, required_non_negative_u64(row, "end_seq")?).await?;
        if let SessionEventKind::ModelTurnFinished {
            turn_id, outcome, ..
        } = terminal.kind
        {
            if turn_id != required_string(row, "turn_id")? {
                return Err(invalid("settled turn identity mismatch"));
            }
            outcomes.push(format!(
                "[turn {turn_id} outcome={}]\n",
                serde_json::to_string(&outcome)?
            ));
        } else {
            return Err(invalid("settled turn boundary is not terminal"));
        }
    }
    outcomes.reverse();
    let outcomes = outcomes.concat();

    Ok(outcomes)
}

impl SessionDb {
    /// Capture request-only conversational input using indexed, bounded canonical reads.
    /// # Errors
    /// Rejects invalid budgets, stale projections and malformed canonical rows.
    pub async fn milestone_context(
        &self,
        max_bytes: usize,
        max_turns: usize,
    ) -> SessionDbResult<SessionMilestoneContext> {
        if max_bytes == 0 || max_bytes > 256 * 1024 || max_turns == 0 || max_turns > 200 {
            return Err(invalid("invalid milestone context budget"));
        }
        let tx = self.db.begin_transaction().await?;
        let result = capture_context(&*tx, self.session_id(), max_bytes, max_turns).await;
        let rollback = tx.rollback().await;
        let context = result?;
        rollback?;
        Ok(context)
    }
    /// List a bounded publication-ordered page, without generation plugin dependencies.
    /// # Errors
    /// Rejects invalid page sizes and malformed or inaccessible current storage.
    pub async fn milestones(
        &self,
        query: SessionMilestoneQuery,
    ) -> SessionDbResult<SessionMilestonePage> {
        self.validate_read_readiness().await?;
        if query.limit == 0 || query.limit > 200 {
            return Err(invalid("milestone page limit must be 1..=200"));
        }
        let mut select = self
            .db
            .select("session_milestones")
            .columns(&["published_seq", "timestamp_ms", "milestone_json"])
            .sort("published_seq", SortDirection::Asc)
            .limit(query.limit + 1);
        if let Some(after) = query.after_sequence {
            select = select.where_gt("published_seq", seq_to_value(after));
        }
        let rows = select.execute(&**self.db).await?;
        let milestones = rows
            .iter()
            .take(query.limit)
            .map(|row| {
                Ok(SessionMilestoneEntry {
                    event_sequence: required_non_negative_u64(row, "published_seq")?,
                    timestamp_ms: required_non_negative_u64(row, "timestamp_ms")?,
                    milestone: serde_json::from_str(&required_string(row, "milestone_json")?)?,
                })
            })
            .collect::<SessionDbResult<Vec<_>>>()?;
        let next_after_sequence = (rows.len() > query.limit)
            .then(|| milestones.last().map(|entry| entry.event_sequence))
            .flatten();
        Ok(SessionMilestonePage {
            milestones,
            next_after_sequence,
        })
    }
    pub(crate) async fn milestone_evaluation(
        &self,
        id: &str,
    ) -> SessionDbResult<Option<(u64, SessionMilestoneEvaluationRequest, bool)>> {
        if self
            .db
            .select("milestone_evaluations")
            .columns(&["evaluation_id"])
            .where_eq("evaluation_id", id)
            .execute_first(&**self.db)
            .await?
            .is_none()
        {
            return Ok(None);
        }
        evaluation(&**self.db, id).await.map(Some)
    }
    pub(crate) async fn milestone_publication(
        &self,
        id: &str,
    ) -> SessionDbResult<Option<SessionMilestonePublicationRequest>> {
        let row = self
            .db
            .select("milestone_evaluations")
            .columns(&["publication_json"])
            .where_eq("evaluation_id", id)
            .execute_first(&**self.db)
            .await?;
        row.as_ref()
            .and_then(|row| crate::db_row::optional_string(row, "publication_json"))
            .map(|json| serde_json::from_str(&json).map_err(SessionDbError::from))
            .transpose()
    }
    pub(crate) async fn milestone_by_evaluation(
        &self,
        id: &str,
    ) -> SessionDbResult<Option<SessionMilestone>> {
        self.db
            .select("session_milestones")
            .columns(&["milestone_json"])
            .where_eq("evaluation_id", id)
            .execute_first(&**self.db)
            .await?
            .as_ref()
            .map(|row| {
                Ok(serde_json::from_str(&required_string(
                    row,
                    "milestone_json",
                )?)?)
            })
            .transpose()
    }
    /// Latest durable evaluation checkpoint; a bounded indexed read.
    /// # Errors
    /// Returns storage errors or malformed evaluation records.
    pub async fn milestone_evaluation_checkpoint(
        &self,
    ) -> SessionDbResult<Option<SessionMilestoneEvaluationStatus>> {
        self.validate_read_readiness().await?;
        let row = self
            .db
            .select("milestone_evaluations")
            .columns(&["evaluation_id"])
            .sort("admitted_seq", SortDirection::Desc)
            .limit(1)
            .execute_first(&**self.db)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        self.milestone_evaluation_status(&required_string(&row, "evaluation_id")?)
            .await
    }
    /// Exact durable evaluation status, without replay or write admission.
    /// # Errors
    /// Returns malformed record or storage errors.
    pub async fn milestone_evaluation_status(
        &self,
        id: &str,
    ) -> SessionDbResult<Option<SessionMilestoneEvaluationStatus>> {
        validate_id(id)?;
        self.validate_read_readiness().await?;
        Ok(self
            .milestone_evaluation(id)
            .await?
            .map(
                |(admitted_sequence, request, completed)| SessionMilestoneEvaluationStatus {
                    admitted_sequence,
                    request,
                    completed,
                },
            ))
    }
    pub(crate) async fn milestone_title_already_published(
        &self,
        admitted: u64,
    ) -> SessionDbResult<bool> {
        Ok(latest_kind_sequence(&**self.db, "automatic_title_updated").await? > admitted)
    }
    pub(crate) async fn milestone_title_fence(&self, admitted: u64) -> SessionDbResult<bool> {
        Ok(latest_kind_sequence(&**self.db, "session_renamed").await? <= admitted)
    }
}
