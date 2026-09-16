//! Durable recovery-only dispatch barrier. Absence means ordinary execution.
use crate::WorkflowStoreError;
use rusqlite::Connection;

pub fn verify(connection: &Connection) -> Result<(), WorkflowStoreError> {
    for name in [
        "recovery_blocks_attempt_insert",
        "recovery_blocks_resume",
        "recovery_blocks_handoff",
    ] {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'trigger' AND name = ?1)",
            [name],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(WorkflowStoreError::InvalidData(
                "workflow recovery dispatch barrier is missing; maintenance required".into(),
            ));
        }
    }
    Ok(())
}

pub fn initialize(connection: &Connection) -> Result<(), WorkflowStoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS workflow_recovery_barriers (
            run_id TEXT PRIMARY KEY NOT NULL REFERENCES workflow_runs(run_id),
            source_artifact_id TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL
        );
        CREATE TRIGGER IF NOT EXISTS recovery_blocks_attempt_insert BEFORE INSERT ON workflow_attempts
        WHEN EXISTS(SELECT 1 FROM workflow_recovery_barriers WHERE run_id = NEW.run_id)
        BEGIN SELECT RAISE(ABORT, 'workflow recovery prohibits new attempts'); END;
        CREATE TRIGGER IF NOT EXISTS recovery_blocks_resume BEFORE UPDATE OF status ON workflow_runs
        WHEN NEW.status = 'running' AND EXISTS(
            SELECT 1 FROM workflow_recovery_barriers WHERE run_id = NEW.run_id)
        BEGIN SELECT RAISE(ABORT, 'workflow recovery prohibits resume'); END;
        CREATE TRIGGER IF NOT EXISTS recovery_blocks_handoff BEFORE UPDATE OF handed_off ON workflow_dispatch_handoffs
        WHEN NEW.handed_off = 1 AND EXISTS(
            SELECT 1 FROM workflow_attempts a JOIN workflow_recovery_barriers b USING(run_id)
            WHERE a.dispatch_identity = NEW.dispatch_identity)
        BEGIN SELECT RAISE(ABORT, 'workflow recovery prohibits dispatch'); END;",
    )?;
    Ok(())
}
