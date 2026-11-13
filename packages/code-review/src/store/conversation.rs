//! Pull request thread snapshots: each review's pull request threads as the review app last
//! read and placed them, one row per review scope, replaced whenever the app reads them again.
//!
//! The review app is the only writer; agents' review tools read it, so they see the threads the
//! reviewer sees without reaching the forge themselves. A snapshot is bounded when saved (see
//! [`ReviewConversationSnapshot::bounded`]).

use super::db::{CodeReviewDb, int_column, review_key_for_scope, text_column};
use super::{CodeReviewConfig, StoreError, resolve_repo_root, with_database};
use bcode_code_review_models::{
    GetReviewConversationRequest, GetReviewConversationResponse, ReviewConversationSnapshot,
    SaveReviewConversationRequest, SaveReviewConversationResponse,
};
use switchy::database::query::{FilterableQuery as _, where_eq};
use switchy::database::schema::create_table;
use switchy::schema::discovery::code::CodeMigration;

const TABLE: &str = "review_conversation_snapshots";

/// The table of pull request thread snapshots.
pub fn conversation_snapshots_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "026_review_conversation_snapshots_table".to_string(),
        Box::new(
            create_table(TABLE)
                .if_not_exists(true)
                .column(text_column("review_key"))
                .column(text_column("snapshot_json"))
                .column(int_column("read_at_ms"))
                .primary_key("review_key"),
        ),
        None,
    )
}

impl CodeReviewDb<'_> {
    /// Replace the snapshot of `review_key`.
    pub async fn save_conversation(
        &self,
        review_key: &str,
        snapshot: &ReviewConversationSnapshot,
    ) -> Result<(), StoreError> {
        self.db()
            .upsert(TABLE)
            .unique(&["review_key"])
            .value("review_key", review_key.to_owned())
            .value("snapshot_json", serde_json::to_string(snapshot)?)
            .value(
                "read_at_ms",
                i64::try_from(snapshot.read_at_ms).unwrap_or(i64::MAX),
            )
            .execute(self.db())
            .await?;
        Ok(())
    }

    /// The snapshot of `review_key`, when one was saved.
    pub async fn conversation(
        &self,
        review_key: &str,
    ) -> Result<Option<ReviewConversationSnapshot>, StoreError> {
        let row = self
            .db()
            .select(TABLE)
            .columns(&["snapshot_json"])
            .filter(Box::new(where_eq("review_key", review_key)))
            .execute_first(self.db())
            .await?;
        let Some(json) = row
            .as_ref()
            .and_then(|row| row.get("snapshot_json"))
            .and_then(|value| value.as_str().map(str::to_owned))
        else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_str(&json)?))
    }
}

/// Replace a review's pull request thread snapshot (bounded first).
///
/// # Errors
///
/// * See [`super::ReviewStore`].
pub fn save_conversation_for_request(
    request: SaveReviewConversationRequest,
    config: &CodeReviewConfig,
) -> Result<SaveReviewConversationResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let review_key = review_key_for_scope(&repo_root, &request.target, request.scope.as_ref())?;
    let snapshot = request.snapshot.bounded();
    let saved = snapshot.threads.len();
    with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .save_conversation(&review_key, &snapshot)
                .await
        })
    })?;
    Ok(SaveReviewConversationResponse { saved })
}

/// A review's pull request thread snapshot.
///
/// # Errors
///
/// * See [`super::ReviewStore`].
pub fn conversation_for_request(
    request: &GetReviewConversationRequest,
    config: &CodeReviewConfig,
) -> Result<GetReviewConversationResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let review_key = review_key_for_scope(&repo_root, &request.target, request.scope.as_ref())?;
    let snapshot = with_database(&repo_root, config, move |database| {
        Box::pin(async move { CodeReviewDb::new(database).conversation(&review_key).await })
    })?;
    Ok(GetReviewConversationResponse { snapshot })
}
