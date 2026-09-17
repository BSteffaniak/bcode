//! Durable recovery-only dispatch barrier. Absence means ordinary execution.
use crate::WorkflowStoreError;
use rusqlite::Connection;

pub fn verify_barriers(connection: &Connection) -> Result<(), WorkflowStoreError> {
    connection.prepare(
        "SELECT run_id, source_artifact_id, created_at_ms FROM workflow_recovery_barriers LIMIT 0",
    )?;
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

pub fn verify(connection: &Connection) -> Result<(), WorkflowStoreError> {
    verify_barriers(connection)?;
    connection.prepare(
        "SELECT dispatch_identity FROM workflow_attempts INDEXED BY workflow_receipt_recovery_page
        WHERE run_id = ?1 AND dispatch_identity > ?2
        AND status IN ('admitted', 'running', 'cancelling', 'sibling_cancelling')
        AND receipt_json IS NOT NULL ORDER BY dispatch_identity LIMIT 1",
    )?;
    connection
        .prepare("SELECT run_id, after_dispatch_identity FROM workflow_receipt_cursors LIMIT 0")?;
    connection.prepare("SELECT old_run_id, successor_run_id, successor_json, requested_at_ms FROM workflow_replacement_intents LIMIT 0")?;
    connection
        .prepare("SELECT artifact_id, after_run_id FROM workflow_discovery_cursors LIMIT 0")?;
    Ok(())
}

pub fn require_execution(connection: &Connection, run_id: &str) -> Result<(), WorkflowStoreError> {
    let recovering: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM workflow_recovery_barriers WHERE run_id = ?1)",
        [run_id],
        |row| row.get(0),
    )?;
    if recovering {
        return Err(WorkflowStoreError::InvalidData(
            "workflow recovery prohibits execution admission".into(),
        ));
    }
    Ok(())
}

pub fn initialize(connection: &Connection) -> Result<(), WorkflowStoreError> {
    connection.execute_batch(
        "CREATE INDEX IF NOT EXISTS workflow_receipt_recovery_page
            ON workflow_attempts(run_id, dispatch_identity)
            WHERE status IN ('admitted', 'running', 'cancelling', 'sibling_cancelling')
            AND receipt_json IS NOT NULL;
        CREATE TABLE IF NOT EXISTS workflow_replacement_intents (
            old_run_id TEXT PRIMARY KEY NOT NULL REFERENCES workflow_runs(run_id),
            successor_run_id TEXT UNIQUE NOT NULL,
            successor_json TEXT NOT NULL,
            requested_at_ms INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS workflow_discovery_cursors (
            artifact_id TEXT PRIMARY KEY NOT NULL,
            after_run_id TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS workflow_receipt_cursors (
            run_id TEXT PRIMARY KEY NOT NULL REFERENCES workflow_runs(run_id),
            after_dispatch_identity TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS workflow_recovery_barriers (
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
