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
    connection.prepare("SELECT run_id, after_dispatch, after_child, attempts_complete, complete FROM workflow_quiescence LIMIT 0")?;
    for name in [
        "quiescence_blocks_attempt_insert",
        "quiescence_blocks_attempt_update",
        "quiescence_blocks_attempt_delete",
        "quiescence_blocks_link_insert",
        "quiescence_blocks_link_update",
        "quiescence_blocks_link_delete",
        "quiescence_blocks_reopen",
    ] {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='trigger' AND name=?1)",
            [name],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(WorkflowStoreError::InvalidData(
                "quiescence guard missing; maintenance required".into(),
            ));
        }
    }
    connection.prepare("SELECT child_run_id FROM workflow_run_links INDEXED BY workflow_quiescence_children WHERE parent_run_id=?1 AND child_run_id>?2 ORDER BY child_run_id LIMIT 1")?;
    connection.prepare("SELECT dispatch_identity FROM workflow_attempts INDEXED BY workflow_quiescence_attempts WHERE run_id=?1 AND dispatch_identity>?2 ORDER BY dispatch_identity LIMIT 1")?;
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
        CREATE TABLE IF NOT EXISTS workflow_quiescence (
            run_id TEXT PRIMARY KEY NOT NULL REFERENCES workflow_runs(run_id),
            after_dispatch TEXT NOT NULL DEFAULT '',
            after_child TEXT NOT NULL DEFAULT '',
            attempts_complete INTEGER NOT NULL DEFAULT 0,
            complete INTEGER NOT NULL DEFAULT 0
        );
        CREATE TRIGGER IF NOT EXISTS quiescence_blocks_attempt_insert BEFORE INSERT ON workflow_attempts
        WHEN EXISTS(SELECT 1 FROM workflow_quiescence WHERE run_id=NEW.run_id)
        BEGIN SELECT RAISE(ABORT, 'quiescence verification freezes attempts'); END;
        CREATE TRIGGER IF NOT EXISTS quiescence_blocks_attempt_update BEFORE UPDATE ON workflow_attempts
        WHEN EXISTS(SELECT 1 FROM workflow_quiescence WHERE run_id=OLD.run_id OR run_id=NEW.run_id)
        BEGIN SELECT RAISE(ABORT, 'quiescence verification freezes attempts'); END;
        CREATE TRIGGER IF NOT EXISTS quiescence_blocks_attempt_delete BEFORE DELETE ON workflow_attempts
        WHEN EXISTS(SELECT 1 FROM workflow_quiescence WHERE run_id=OLD.run_id)
        BEGIN SELECT RAISE(ABORT, 'quiescence verification freezes attempts'); END;
        CREATE TRIGGER IF NOT EXISTS quiescence_blocks_link_insert BEFORE INSERT ON workflow_run_links
        WHEN EXISTS(SELECT 1 FROM workflow_quiescence WHERE run_id=NEW.parent_run_id)
        BEGIN SELECT RAISE(ABORT, 'quiescence verification freezes children'); END;
        CREATE TRIGGER IF NOT EXISTS quiescence_blocks_link_update BEFORE UPDATE ON workflow_run_links
        WHEN EXISTS(SELECT 1 FROM workflow_quiescence WHERE run_id=OLD.parent_run_id OR run_id=NEW.parent_run_id)
        BEGIN SELECT RAISE(ABORT, 'quiescence verification freezes children'); END;
        CREATE TRIGGER IF NOT EXISTS quiescence_blocks_link_delete BEFORE DELETE ON workflow_run_links
        WHEN EXISTS(SELECT 1 FROM workflow_quiescence WHERE run_id=OLD.parent_run_id)
        BEGIN SELECT RAISE(ABORT, 'quiescence verification freezes children'); END;
        CREATE TRIGGER IF NOT EXISTS quiescence_blocks_reopen BEFORE UPDATE OF status ON workflow_runs
        WHEN NEW.status != OLD.status AND EXISTS(SELECT 1 FROM workflow_quiescence WHERE run_id=OLD.run_id)
        BEGIN SELECT RAISE(ABORT, 'quiescent terminal outcome is stable'); END;
        CREATE INDEX IF NOT EXISTS workflow_quiescence_children ON workflow_run_links(parent_run_id,child_run_id);
        CREATE INDEX IF NOT EXISTS workflow_quiescence_attempts ON workflow_attempts(run_id,dispatch_identity);
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
