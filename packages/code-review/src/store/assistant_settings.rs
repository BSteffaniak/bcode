//! The review app's assistant settings, one record of each per review store (user-wide with
//! the default user state location):
//!
//! * the model and reasoning effort it gives new conversations and Ask AI card sessions;
//! * the assistant panel's size chosen by dragging its divider.
//!
//! Each record is its own row, so saving one never overwrites the other.

use super::db::{CodeReviewDb, int_column, now_ms, text_column};
use super::{CodeReviewConfig, StoreError, resolve_repo_root, with_database};
use bcode_code_review_models::{
    GetReviewAssistantPanelSizeRequest, GetReviewAssistantPanelSizeResponse,
    GetReviewAssistantSettingsRequest, GetReviewAssistantSettingsResponse,
    ReviewAssistantPanelSize, ReviewAssistantSettings, SaveReviewAssistantPanelSizeRequest,
    SaveReviewAssistantPanelSizeResponse, SaveReviewAssistantSettingsRequest,
    SaveReviewAssistantSettingsResponse,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use switchy::database::query::{FilterableQuery as _, where_eq};
use switchy::database::schema::create_table;
use switchy::schema::discovery::code::CodeMigration;

const TABLE: &str = "review_assistant_settings";
/// Key of the model and effort record.
const KEY: &str = "app";
/// Key of the panel size record.
const PANEL_SIZE_KEY: &str = "panel_size";

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
    /// The record under `key`, when a readable one exists.
    async fn settings_record<T: DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, StoreError> {
        let row = self
            .db()
            .select(TABLE)
            .columns(&["settings_json"])
            .filter(Box::new(where_eq("settings_key", key)))
            .execute_first(self.db())
            .await?;
        Ok(row
            .as_ref()
            .and_then(|row| row.get("settings_json"))
            .and_then(|value| value.as_str().map(str::to_owned))
            .and_then(|json| serde_json::from_str(&json).ok()))
    }

    /// Replace the record under `key`, saved at `updated_at_ms`.
    async fn save_settings_record<T: Serialize + Sync>(
        &self,
        key: &str,
        record: &T,
        updated_at_ms: u64,
    ) -> Result<(), StoreError> {
        let json = serde_json::to_string(record)?;
        self.db()
            .upsert(TABLE)
            .unique(&["settings_key"])
            .value("settings_key", key)
            .value("settings_json", json)
            .value(
                "updated_at_ms",
                i64::try_from(updated_at_ms).unwrap_or(i64::MAX),
            )
            .execute(self.db())
            .await?;
        Ok(())
    }

    /// The saved settings, when a readable record exists.
    pub async fn assistant_settings(&self) -> Result<Option<ReviewAssistantSettings>, StoreError> {
        self.settings_record(KEY).await
    }

    /// Replace the settings; returns them with their timestamp.
    pub async fn save_assistant_settings(
        &self,
        mut settings: ReviewAssistantSettings,
    ) -> Result<ReviewAssistantSettings, StoreError> {
        settings.updated_at_ms = now_ms();
        self.save_settings_record(KEY, &settings, settings.updated_at_ms)
            .await?;
        Ok(settings)
    }

    /// The saved panel size, when a readable record exists.
    pub async fn assistant_panel_size(
        &self,
    ) -> Result<Option<ReviewAssistantPanelSize>, StoreError> {
        self.settings_record(PANEL_SIZE_KEY).await
    }

    /// Replace the panel size; returns it with its timestamp.
    pub async fn save_assistant_panel_size(
        &self,
        mut size: ReviewAssistantPanelSize,
    ) -> Result<ReviewAssistantPanelSize, StoreError> {
        size.updated_at_ms = now_ms();
        self.save_settings_record(PANEL_SIZE_KEY, &size, size.updated_at_ms)
            .await?;
        Ok(size)
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

/// Read the assistant panel size.
///
/// # Errors
///
/// * See [`super::ReviewStore`].
pub fn assistant_panel_size_for_request(
    request: &GetReviewAssistantPanelSizeRequest,
    config: &CodeReviewConfig,
) -> Result<GetReviewAssistantPanelSizeResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let size = with_database(&repo_root, config, move |database| {
        Box::pin(async move { CodeReviewDb::new(database).assistant_panel_size().await })
    })?;
    Ok(GetReviewAssistantPanelSizeResponse { size })
}

/// Replace the assistant panel size.
///
/// # Errors
///
/// * See [`super::ReviewStore`].
pub fn save_assistant_panel_size_for_request(
    request: &SaveReviewAssistantPanelSizeRequest,
    config: &CodeReviewConfig,
) -> Result<SaveReviewAssistantPanelSizeResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let size = request.size;
    let size = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .save_assistant_panel_size(size)
                .await
        })
    })?;
    Ok(SaveReviewAssistantPanelSizeResponse { size })
}
