use crate::migration::{
    SessionEventLogMigration, SessionEventLogMigrationError, SessionMigrationAction,
    SessionMigrationApplyStatus, SessionMigrationBackupPolicy, SessionMigrationJournalEntry,
    SessionMigrationJournalStatus, SessionMigrationPlanItem, SessionMigrationReport,
    SessionMigrationReportItem,
};
use crate::{SessionEventStore, SessionStoreError, current_unix_millis, reader, write_event_frame};
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, RuntimeWorkId, RuntimeWorkKind, RuntimeWorkStatus,
    SessionEvent, SessionEventKind, SessionId, ToolInvocationStreamEvent,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;

const SESSION_EVENT_CHAIN_MIGRATION_ID: &str = "sessions-events-to-current";

const BUILTIN_SESSION_EVENT_MIGRATIONS: &[NoOpSessionEventMigration] = &[
    NoOpSessionEventMigration::new("sessions-events-v0-to-v1", 0, 1),
    NoOpSessionEventMigration::new("sessions-events-v1-to-v2", 1, 2),
    NoOpSessionEventMigration::new("sessions-events-v2-to-v3", 2, 3),
    NoOpSessionEventMigration::new("sessions-events-v3-to-v4", 3, 4),
    NoOpSessionEventMigration::new("sessions-events-v4-to-v5", 4, 5),
    NoOpSessionEventMigration::new("sessions-events-v5-to-v6", 5, 6),
    NoOpSessionEventMigration::new("sessions-events-v6-to-v7", 6, 7),
    NoOpSessionEventMigration::new("sessions-events-v7-to-v8", 7, 8),
    NoOpSessionEventMigration::new("sessions-events-v8-to-v9", 8, 9),
    NoOpSessionEventMigration::new("sessions-events-v9-to-v10", 9, 10),
    NoOpSessionEventMigration::new("sessions-events-v10-to-v11", 10, 11),
    NoOpSessionEventMigration::new("sessions-events-v11-to-v12", 11, 12),
    NoOpSessionEventMigration::new("sessions-events-v12-to-v13", 12, 13),
];

trait SessionEventMigrationStep {
    fn id(&self) -> &'static str;
    fn source_schema(&self) -> u16;
    fn target_schema(&self) -> u16;
    fn migrate_event(
        &self,
        event: SessionEvent,
    ) -> Result<Vec<SessionEvent>, SessionEventLogMigrationError>;
}

#[derive(Debug, Clone, Copy)]
struct NoOpSessionEventMigration {
    id: &'static str,
    from_schema: u16,
    to_schema: u16,
}

impl NoOpSessionEventMigration {
    const fn new(id: &'static str, from_schema: u16, to_schema: u16) -> Self {
        Self {
            id,
            from_schema,
            to_schema,
        }
    }
}

impl SessionEventMigrationStep for NoOpSessionEventMigration {
    fn id(&self) -> &'static str {
        self.id
    }

    fn source_schema(&self) -> u16 {
        self.from_schema
    }

    fn target_schema(&self) -> u16 {
        self.to_schema
    }

    fn migrate_event(
        &self,
        event: SessionEvent,
    ) -> Result<Vec<SessionEvent>, SessionEventLogMigrationError> {
        if self.from_schema == 10 && self.to_schema == 11 {
            return Ok(migrate_v10_event_to_v11(event));
        }
        if self.from_schema == 12 && self.to_schema == 13 {
            return Ok(migrate_v12_event_to_v13(event));
        }
        Ok(vec![event])
    }
}

struct TypedSessionEventMigration<'a, M> {
    migration: &'a M,
}

impl<M> SessionEventMigrationStep for TypedSessionEventMigration<'_, M>
where
    M: SessionEventLogMigration,
{
    fn id(&self) -> &'static str {
        M::ID
    }

    fn source_schema(&self) -> u16 {
        M::FROM_SCHEMA
    }

    fn target_schema(&self) -> u16 {
        M::TO_SCHEMA
    }

    fn migrate_event(
        &self,
        event: SessionEvent,
    ) -> Result<Vec<SessionEvent>, SessionEventLogMigrationError> {
        self.migration.migrate_event(event).map(|event| vec![event])
    }
}

impl SessionEventStore {
    /// Rewrite every older event in a canonical log through the built-in
    /// step-by-step event schema chain until it reaches the current schema.
    ///
    /// Mixed-version logs are handled per event. For example, a log containing
    /// schema 7, 8, and 9 events applies 7->8->9 to the first event, 8->9 to
    /// the second event, and leaves the current event unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error if the log cannot be read, backed up, migrated,
    /// validated, atomically replaced, or reindexed.
    pub fn migrate_event_log_to_current(
        &self,
        session_id: SessionId,
    ) -> Result<SessionMigrationReport, SessionStoreError> {
        let steps = BUILTIN_SESSION_EVENT_MIGRATIONS
            .iter()
            .map(|migration| migration as &dyn SessionEventMigrationStep)
            .collect::<Vec<_>>();
        self.migrate_event_log_with_journal(
            session_id,
            SESSION_EVENT_CHAIN_MIGRATION_ID,
            CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            &steps,
        )
    }

    /// Rewrite all older readable event logs in this store to the current event
    /// schema through the built-in step-by-step migration chain.
    ///
    /// Logs with read issues or future schema versions are left untouched so the
    /// caller can surface repair/future-version diagnostics normally.
    ///
    /// # Errors
    ///
    /// Returns an error if a required migration fails while rewriting an older
    /// readable log.
    pub fn migrate_all_event_logs_to_current(
        &self,
    ) -> Result<Vec<SessionMigrationReport>, SessionStoreError> {
        let mut reports = Vec::new();
        if !self.root().exists() {
            return Ok(reports);
        }
        for entry in fs::read_dir(self.root())? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("events") {
                continue;
            }
            let session_id = crate::parse_session_file_name(&path)?;
            let report = reader::read_events(&path)?;
            if !report.issues.is_empty()
                || report
                    .max_schema_version
                    .is_some_and(|version| version > CURRENT_SESSION_EVENT_SCHEMA_VERSION)
                || report
                    .min_schema_version
                    .is_none_or(|version| version >= CURRENT_SESSION_EVENT_SCHEMA_VERSION)
            {
                continue;
            }
            reports.push(self.migrate_event_log_to_current(session_id)?);
        }
        Ok(reports)
    }

    /// Rewrite a canonical session event log through a registered event migration.
    ///
    /// The executor owns backup, temp writes, validation, atomic replacement, and
    /// derived index rebuild. The migration implementation only transforms events.
    /// Mixed logs containing both the migration source schema and target schema
    /// are handled per event.
    ///
    /// # Errors
    ///
    /// Returns an error if the log cannot be read, backed up, migrated, validated,
    /// atomically replaced, or reindexed.
    pub fn migrate_event_log<M: SessionEventLogMigration>(
        &self,
        session_id: SessionId,
        migration: &M,
    ) -> Result<SessionMigrationReport, SessionStoreError> {
        let typed = TypedSessionEventMigration { migration };
        let steps = [&typed as &dyn SessionEventMigrationStep];
        self.migrate_event_log_with_journal(session_id, M::ID, M::TO_SCHEMA, &steps)
    }

    fn migrate_event_log_with_journal(
        &self,
        session_id: SessionId,
        migration_id: &'static str,
        target_schema: u16,
        steps: &[&dyn SessionEventMigrationStep],
    ) -> Result<SessionMigrationReport, SessionStoreError> {
        let started_at_ms = current_unix_millis();
        let run_id = format!("session-event-migration-{started_at_ms}");
        let session_ids = vec![session_id];
        let migration_ids = steps
            .iter()
            .map(|step| step.id().to_string())
            .collect::<Vec<_>>();
        crate::migration::append_journal_entry(
            self.root(),
            &SessionMigrationJournalEntry {
                run_id: run_id.clone(),
                domain: "sessions/events".to_string(),
                status: SessionMigrationJournalStatus::Started,
                dry_run: false,
                backup: true,
                backup_dir: None,
                started_at_ms,
                finished_at_ms: None,
                migration_ids: migration_ids.clone(),
                session_ids: session_ids.clone(),
                error: None,
            },
        )?;

        let result = self.migrate_event_log_inner(session_id, migration_id, target_schema, steps);
        let finished_at_ms = current_unix_millis();
        match &result {
            Ok(report) => {
                crate::migration::append_journal_entry(
                    self.root(),
                    &SessionMigrationJournalEntry {
                        run_id,
                        domain: "sessions/events".to_string(),
                        status: SessionMigrationJournalStatus::Completed,
                        dry_run: false,
                        backup: true,
                        backup_dir: report
                            .backup_dir
                            .as_ref()
                            .map(|path| path.display().to_string()),
                        started_at_ms,
                        finished_at_ms: Some(finished_at_ms),
                        migration_ids,
                        session_ids,
                        error: None,
                    },
                )?;
            }
            Err(error) => {
                let _ = crate::migration::append_journal_entry(
                    self.root(),
                    &SessionMigrationJournalEntry {
                        run_id,
                        domain: "sessions/events".to_string(),
                        status: SessionMigrationJournalStatus::Failed,
                        dry_run: false,
                        backup: true,
                        backup_dir: None,
                        started_at_ms,
                        finished_at_ms: Some(finished_at_ms),
                        migration_ids,
                        session_ids,
                        error: Some(error.to_string()),
                    },
                );
            }
        }
        result
    }

    fn migrate_event_log_inner(
        &self,
        session_id: SessionId,
        migration_id: &'static str,
        target_schema: u16,
        steps: &[&dyn SessionEventMigrationStep],
    ) -> Result<SessionMigrationReport, SessionStoreError> {
        let path = self.event_path(session_id);
        let report = reader::read_events(&path)?;
        if !report.issues.is_empty() {
            return Err(SessionStoreError::InvalidSessionId(format!(
                "session {session_id} has read issues and must be repaired before event migration"
            )));
        }
        if report.min_schema_version == Some(target_schema)
            && report.max_schema_version == Some(target_schema)
        {
            return Ok(SessionMigrationReport {
                domain: "sessions/events",
                dry_run: false,
                backup_dir: None,
                items: vec![SessionMigrationReportItem {
                    migration_id,
                    session_id,
                    action: SessionMigrationAction::RewriteCanonicalEvents,
                    status: SessionMigrationApplyStatus::Skipped,
                    message: "already at target schema".to_string(),
                }],
            });
        }

        let steps_by_from = validate_event_migration_steps(steps, target_schema)?;
        let found_version = report.min_schema_version;
        let plan_item = SessionMigrationPlanItem {
            migration_id,
            session_id,
            current_version: target_schema,
            found_version,
            action: SessionMigrationAction::RewriteCanonicalEvents,
            reason: format!("canonical event migration to schema {target_schema}"),
            automatic: false,
            backup_policy: SessionMigrationBackupPolicy::Required,
        };
        let backup_dir = self.backup_canonical_events(&[plan_item])?;
        let tmp_path = path.with_extension("events.tmp");
        let mut tmp = fs::File::create(&tmp_path)?;
        let mut migrated_count = 0_usize;
        let mut used_steps = BTreeSet::new();
        let mut sequence_map = BTreeMap::new();
        let mut next_sequence = 0_u64;
        for event in report.events {
            let source_sequence = event.sequence;
            let events = migrate_event_to_schema(
                event,
                session_id,
                target_schema,
                &steps_by_from,
                &mut used_steps,
            )?;
            for mut event in events {
                remap_event_sequence_references(&mut event, &sequence_map);
                event.sequence = next_sequence;
                sequence_map.insert(source_sequence, next_sequence);
                next_sequence = next_sequence.saturating_add(1);
                write_event_frame(&mut tmp, &event)?;
                migrated_count = migrated_count.saturating_add(1);
            }
            sequence_map
                .entry(source_sequence)
                .or_insert_with(|| next_sequence.saturating_sub(1));
        }
        tmp.flush()?;
        drop(tmp);
        let validation = reader::read_events(&tmp_path)?;
        if validation.events.len() != migrated_count
            || validation.min_schema_version != Some(target_schema)
            || validation.max_schema_version != Some(target_schema)
            || !validation.issues.is_empty()
            || validation
                .events
                .iter()
                .any(|event| event.session_id != session_id)
        {
            return Err(SessionStoreError::InvalidSessionId(format!(
                "migrated session log validation failed for {session_id}"
            )));
        }
        fs::rename(&tmp_path, &path)?;
        self.reindex_session(session_id)?;
        Ok(SessionMigrationReport {
            domain: "sessions/events",
            dry_run: false,
            backup_dir: Some(backup_dir),
            items: vec![SessionMigrationReportItem {
                migration_id,
                session_id,
                action: SessionMigrationAction::RewriteCanonicalEvents,
                status: SessionMigrationApplyStatus::Applied,
                message: format!(
                    "migrated canonical events to schema {target_schema} through {} step(s)",
                    used_steps.len()
                ),
            }],
        })
    }
}

fn validate_event_migration_steps<'a>(
    steps: &'a [&'a dyn SessionEventMigrationStep],
    target_schema: u16,
) -> Result<BTreeMap<u16, &'a dyn SessionEventMigrationStep>, SessionStoreError> {
    let mut steps_by_from = BTreeMap::new();
    for step in steps {
        if step.id().is_empty() {
            return Err(SessionStoreError::InvalidSessionId(
                "session event migration id cannot be empty".to_string(),
            ));
        }
        let from_schema = step.source_schema();
        let to_schema = step.target_schema();
        if to_schema != from_schema.saturating_add(1) || to_schema > target_schema {
            return Err(SessionStoreError::InvalidSessionId(format!(
                "invalid session event migration edge {}: {from_schema}->{to_schema}",
                step.id()
            )));
        }
        if steps_by_from.insert(from_schema, *step).is_some() {
            return Err(SessionStoreError::InvalidSessionId(format!(
                "duplicate session event migration source schema: {from_schema}"
            )));
        }
    }
    Ok(steps_by_from)
}

fn remap_event_sequence_references(event: &mut SessionEvent, sequence_map: &BTreeMap<u64, u64>) {
    if let SessionEventKind::ContextCompacted {
        compacted_through_sequence,
        ..
    } = &mut event.kind
    {
        *compacted_through_sequence = sequence_map
            .range(..=*compacted_through_sequence)
            .next_back()
            .map_or(0, |(_, sequence)| *sequence);
    }
}

fn migrate_event_to_schema(
    event: SessionEvent,
    session_id: SessionId,
    target_schema: u16,
    steps_by_from: &BTreeMap<u16, &dyn SessionEventMigrationStep>,
    used_steps: &mut BTreeSet<&'static str>,
) -> Result<Vec<SessionEvent>, SessionStoreError> {
    if event.session_id != session_id {
        return Err(SessionStoreError::InvalidSessionId(format!(
            "session {session_id} contains event for different session {} at sequence {}",
            event.session_id, event.sequence
        )));
    }
    if event.schema_version > target_schema {
        return Err(SessionStoreError::InvalidSessionId(format!(
            "session {session_id} event {} has future schema {} greater than current {target_schema}",
            event.sequence, event.schema_version
        )));
    }
    let mut events = vec![event];
    loop {
        let Some(from_schema) = events.iter().map(|event| event.schema_version).min() else {
            return Ok(events);
        };
        if from_schema >= target_schema {
            return Ok(events);
        }
        let step = steps_by_from.get(&from_schema).ok_or_else(|| {
            SessionStoreError::InvalidSessionId(format!(
                "session {session_id} cannot migrate schema {from_schema} to {target_schema}: missing {from_schema}->{} step",
                from_schema.saturating_add(1)
            ))
        })?;
        let mut next_events = Vec::new();
        for event in events {
            if event.schema_version == from_schema {
                for mut migrated in step
                    .migrate_event(event)
                    .map_err(|error| SessionStoreError::InvalidSessionId(error.to_string()))?
                {
                    migrated.schema_version = step.target_schema();
                    next_events.push(migrated);
                }
            } else {
                next_events.push(event);
            }
        }
        events = next_events;
        used_steps.insert(step.id());
    }
}

fn migrate_v10_event_to_v11(event: SessionEvent) -> Vec<SessionEvent> {
    match &event.kind {
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            tool_name,
            ..
        } => {
            let mut started = event.clone();
            started.kind = SessionEventKind::RuntimeWorkStarted {
                work_id: RuntimeWorkId::new(format!("tool_{tool_call_id}")),
                kind: RuntimeWorkKind::Tool,
                label: tool_name.clone(),
                tool_call_id: Some(tool_call_id.clone()),
                plugin_id: None,
                service_interface: None,
                operation: None,
                started_at_ms: None,
                cancellable: false,
            };
            vec![event, started]
        }
        SessionEventKind::ToolCallFinished {
            tool_call_id,
            result,
            is_error,
        } => {
            let mut finished = event.clone();
            finished.kind = SessionEventKind::RuntimeWorkFinished {
                work_id: RuntimeWorkId::new(format!("tool_{tool_call_id}")),
                status: infer_runtime_work_status(result, *is_error),
                finished_at_ms: None,
                message: None,
            };
            vec![event, finished]
        }
        _ => vec![event],
    }
}

fn migrate_v12_event_to_v13(event: SessionEvent) -> Vec<SessionEvent> {
    if matches!(
        &event.kind,
        SessionEventKind::ToolInvocationStream {
            event: ToolInvocationStreamEvent::OutputDelta { .. }
        }
    ) {
        Vec::new()
    } else {
        vec![event]
    }
}

fn infer_runtime_work_status(result: &str, is_error: bool) -> RuntimeWorkStatus {
    if !is_error {
        return RuntimeWorkStatus::Completed;
    }
    let lower = result.to_ascii_lowercase();
    if lower.contains("timed_out: true") || lower.contains("\"timed_out\":true") {
        RuntimeWorkStatus::TimedOut
    } else {
        RuntimeWorkStatus::Failed
    }
}
