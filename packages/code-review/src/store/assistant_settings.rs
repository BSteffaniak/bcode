//! The review app's assistant settings: the model and reasoning effort it gives new
//! conversations and Ask AI card sessions. One record per review store (user-wide with the
//! default user state location).

use super::db::{CodeReviewDb, int_column, now_ms, text_column};
use super::{CodeReviewConfig, StoreError, resolve_repo_root, with_database};
use bcode_code_review_models::{
    GetReviewAssistantSettingsRequest, GetReviewAssistantSettingsResponse, ReviewAssistantSettings,
    SaveReviewAssistantSettingsRequest, SaveReviewAssistantSettingsResponse,
};
use switchy::database::query::{FilterableQuery as _, where_eq};
use switchy::database::schema::create_table;
use switchy::schema::discovery::code::CodeMigration;

const TABLE: &str = "review_assistant_settings";
/// Key of the one record.
const KEY: &str = "app";

/// The table of assistant settings.
pub fn assistant_settings_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "029_review_assistant_settings_table".to_string(),
        Box::new(
            create_table(TABLE)
                .if_not_exists(true)
                .column(text_column("settings_key"))
                .column(text_column("settings_json"))
                .column(int_column("updated_at_ms"))
                .primary_key("settings_key"),
        ),
        None,
    )
}

impl CodeReviewDb<'_> {
    /// The saved settings, when a readable record exists.
    pub async fn assistant_settings(&self) -> Result<Option<ReviewAssistantSettings>, StoreError> {
        let row = self
            .db()
            .select(TABLE)
            .columns(&["settings_json"])
            .filter(Box::new(where_eq("settings_key", KEY)))
            .execute_first(self.db())
            .await?;
        Ok(row
            .as_ref()
            .and_then(|row| row.get("settings_json"))
            .and_then(|value| value.as_str().map(str::to_owned))
            .and_then(|json| serde_json::from_str(&json).ok()))
    }

    /// Replace the settings; returns them with their timestamp.
    pub async fn save_assistant_settings(
        &self,
        mut settings: ReviewAssistantSettings,
    ) -> Result<ReviewAssistantSettings, StoreError> {
        settings.updated_at_ms = now_ms();
        let json = serde_json::to_string(&settings)?;
        self.db()
            .upsert(TABLE)
            .unique(&["settings_key"])
            .value("settings_key", KEY)
            .value("settings_json", json)
            .value(
                "updated_at_ms",
                i64::try_from(settings.updated_at_ms).unwrap_or(i64::MAX),
            )
            .execute(self.db())
            .await?;
        Ok(settings)
    }
}

/// Read the assistant settings.
///
/// # Errors
///
/// * See [`super::ReviewStore`].
pub fn assistant_settings_for_request(
    request: &GetReviewAssistantSettingsRequest,
    config: &CodeReviewConfig,
) -> Result<GetReviewAssistantSettingsResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let settings = with_database(&repo_root, config, move |database| {
        Box::pin(async move { CodeReviewDb::new(database).assistant_settings().await })
    })?;
    Ok(GetReviewAssistantSettingsResponse { settings })
}

/// Replace the assistant settings.
///
/// # Errors
///
/// * See [`super::ReviewStore`].
pub fn save_assistant_settings_for_request(
    request: SaveReviewAssistantSettingsRequest,
    config: &CodeReviewConfig,
) -> Result<SaveReviewAssistantSettingsResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let settings = request.settings;
    let settings = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .save_assistant_settings(settings)
                .await
        })
    })?;
    Ok(SaveReviewAssistantSettingsResponse { settings })
}
