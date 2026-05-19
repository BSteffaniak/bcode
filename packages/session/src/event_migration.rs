use crate::migration::{
    SessionEventLogMigration, SessionMigrationAction, SessionMigrationApplyStatus,
    SessionMigrationBackupPolicy, SessionMigrationJournalEntry, SessionMigrationJournalStatus,
    SessionMigrationPlanItem, SessionMigrationReport, SessionMigrationReportItem,
};
use crate::{SessionEventStore, SessionStoreError, current_unix_millis, reader, write_event_frame};
use bcode_session_models::SessionId;
use std::fs;
use std::io::Write as _;

impl SessionEventStore {
    /// Rewrite a canonical session event log through a registered event migration.
    ///
    /// The executor owns backup, temp writes, validation, atomic replacement, and
    /// derived index rebuild. The migration implementation only transforms events.
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
        let started_at_ms = current_unix_millis();
        let run_id = format!("session-event-migration-{started_at_ms}");
        let migration_id = M::ID;
        let session_ids = vec![session_id];
        let migration_ids = vec![migration_id.to_string()];
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

        let result = self.migrate_event_log_inner(session_id, migration_id, migration);
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

    fn migrate_event_log_inner<M: SessionEventLogMigration>(
        &self,
        session_id: SessionId,
        migration_id: &'static str,
        migration: &M,
    ) -> Result<SessionMigrationReport, SessionStoreError> {
        let path = self.event_path(session_id);
        let report = reader::read_events(&path)?;
        if report.max_schema_version == Some(M::TO_SCHEMA)
            && report.min_schema_version == Some(M::TO_SCHEMA)
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
        if report.max_schema_version != Some(M::FROM_SCHEMA)
            || report.min_schema_version != Some(M::FROM_SCHEMA)
        {
            return Err(SessionStoreError::InvalidSessionId(format!(
                "session {session_id} schema range {:?}..{:?} does not match migration {migration_id} {}->{}",
                report.min_schema_version,
                report.max_schema_version,
                M::FROM_SCHEMA,
                M::TO_SCHEMA
            )));
        }

        let plan_item = SessionMigrationPlanItem {
            migration_id,
            session_id,
            current_version: M::TO_SCHEMA,
            found_version: Some(M::FROM_SCHEMA),
            action: SessionMigrationAction::RewriteCanonicalEvents,
            reason: format!(
                "canonical event migration {}->{}",
                M::FROM_SCHEMA,
                M::TO_SCHEMA
            ),
            automatic: false,
            backup_policy: SessionMigrationBackupPolicy::Required,
        };
        let backup_dir = self.backup_canonical_events(&[plan_item])?;
        let tmp_path = path.with_extension("events.tmp");
        let mut tmp = fs::File::create(&tmp_path)?;
        let mut migrated_count = 0_usize;
        for mut event in report.events {
            event = migration
                .migrate_event(event)
                .map_err(|error| SessionStoreError::InvalidSessionId(error.to_string()))?;
            event.schema_version = M::TO_SCHEMA;
            write_event_frame(&mut tmp, &event)?;
            migrated_count = migrated_count.saturating_add(1);
        }
        tmp.flush()?;
        drop(tmp);
        let validation = reader::read_events(&tmp_path)?;
        if validation.events.len() != migrated_count
            || validation.min_schema_version != Some(M::TO_SCHEMA)
            || validation.max_schema_version != Some(M::TO_SCHEMA)
            || !validation.issues.is_empty()
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
                    "migrated canonical events {}->{}",
                    M::FROM_SCHEMA,
                    M::TO_SCHEMA
                ),
            }],
        })
    }
}
