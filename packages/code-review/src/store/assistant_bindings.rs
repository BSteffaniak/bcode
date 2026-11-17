//! Compaction of recorded review assistant bindings.
//!
//! Bindings once recorded every stack of a review in full (a stepped-through stack of `n`
//! layers recorded `n` stacks of up to `n` layers each) and a durable workspace's whole viewed
//! and presentation state. A binding now records each stack layer once and a durable
//! workspace by what names it (see [`crate::assistant::bound_workspace`]); the migration here
//! rewrites earlier records the same way, one row at a time.

use super::db::optional_text;
use crate::assistant::bound_workspace;
use bcode_code_review_models::{REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION, ReviewAssistantBinding};
use switchy::database::query::{FilterableQuery as _, where_eq};
use switchy::database::{Database, DatabaseError, Executable};
use switchy::schema::discovery::code::CodeMigration;

const TABLE: &str = "review_assistant_sessions";

/// Rewrites every readable assistant binding in its compact form.
pub fn compact_assistant_bindings_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "030_compact_review_assistant_bindings".to_string(),
        Box::new(CompactBindings),
        None,
    )
}

/// `binding` as recorded: its durable workspace reduced to what names it.
pub fn recorded(mut binding: ReviewAssistantBinding) -> ReviewAssistantBinding {
    binding.workspace = bound_workspace(&binding.workspace);
    binding
}

/// The compact form of the stored binding `json`, when it is a binding of the current schema
/// and the compact form is smaller. Unreadable records and other schema versions are left as
/// they are.
pub fn compacted(json: &str) -> Option<String> {
    let binding = serde_json::from_str::<ReviewAssistantBinding>(json)
        .ok()
        .filter(|binding| binding.schema_version == REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION)?;
    let compact = serde_json::to_string(&recorded(binding)).ok()?;
    (compact.len() < json.len()).then_some(compact)
}

/// The data step of [`compact_assistant_bindings_migration`]. Each row is rewritten on its own
/// and rewriting is idempotent, so an interrupted run is completed by the next one.
struct CompactBindings;

#[async_trait::async_trait]
impl Executable for CompactBindings {
    async fn execute(&self, db: &dyn Database) -> Result<(), DatabaseError> {
        // The table exists (an earlier migration creates it). Migrations are also run against a
        // checksum database that reads no rows and answers no schema questions.
        //
        // Ids first, then one binding at a time: records may be megabytes each.
        let sessions: Vec<String> = db
            .select(TABLE)
            .columns(&["session_id"])
            .execute(db)
            .await?
            .iter()
            .filter_map(|row| optional_text(row, "session_id"))
            .collect();
        for session_id in sessions {
            let json = db
                .select(TABLE)
                .columns(&["binding_json"])
                .filter(Box::new(where_eq("session_id", session_id.clone())))
                .execute_first(db)
                .await?
                .as_ref()
                .and_then(|row| optional_text(row, "binding_json"));
            let Some(compact) = json.as_deref().and_then(compacted) else {
                continue;
            };
            db.update(TABLE)
                .value("binding_json", compact)
                .filter(Box::new(where_eq("session_id", session_id)))
                .execute(db)
                .await?;
        }
        Ok(())
    }
}
