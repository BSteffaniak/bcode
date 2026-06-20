#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Settings database persistence for Bcode.
//!
//! The settings database stores durable local settings and control-center
//! metadata that should survive across Bcode sessions. It intentionally does
//! not store secrets; credential values belong in the configured secure auth
//! store.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;
use switchy::database::query::{FilterableQuery as _, where_eq};
use switchy::database::schema::{Column, DataType, create_table};
use switchy::database::{Database, DatabaseError, Row};
use switchy::schema::discovery::code::{CodeMigration, CodeMigrationSource};
use switchy::schema::runner::MigrationRunner;

const DATABASE_FILE_NAME: &str = "settings.db";
const MIGRATIONS_TABLE: &str = "settings_schema_migrations";
const DATABASE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DATABASE_OPEN_INITIAL_RETRY_DELAY: Duration = Duration::from_millis(20);
const DATABASE_OPEN_MAX_RETRY_DELAY: Duration = Duration::from_millis(250);
const DATABASE_OPEN_RETRY_ATTEMPTS: u32 = 5;
const ONBOARDING_PROGRESS_ID: i64 = 1;

/// Durable Bcode settings database location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsStore {
    settings_db_path: PathBuf,
}

impl Default for SettingsStore {
    fn default() -> Self {
        Self::from_settings_db_path(bcode_config::default_state_dir().join(DATABASE_FILE_NAME))
    }
}

impl SettingsStore {
    /// Create a settings store at `settings_db_path`.
    #[must_use]
    pub fn from_settings_db_path(settings_db_path: impl Into<PathBuf>) -> Self {
        Self {
            settings_db_path: settings_db_path.into(),
        }
    }

    /// Return the settings database path.
    #[must_use]
    pub fn settings_db_path(&self) -> &Path {
        &self.settings_db_path
    }

    /// Persist a generic JSON setting.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings database cannot be opened, migrated,
    /// serialized, or written.
    pub fn put_setting(&self, key: &str, value: &Value, updated_at_ms: u64) -> SettingsResult<()> {
        let key = key.to_owned();
        let value_json = serde_json::to_string(value)?;
        self.with_database(move |database| {
            Box::pin(async move {
                database
                    .upsert("settings_kv")
                    .value("key", key)
                    .value("value_json", value_json)
                    .value("updated_at_ms", u64_to_i64(updated_at_ms))
                    .execute(database.as_ref())
                    .await?;
                Ok(())
            })
        })
    }

    /// Return a generic JSON setting.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings database cannot be opened, migrated,
    /// read, or decoded.
    pub fn get_setting(&self, key: &str) -> SettingsResult<Option<SettingsValue>> {
        let key = key.to_owned();
        self.with_database(move |database| {
            Box::pin(async move {
                let rows = database
                    .select("settings_kv")
                    .columns(&["key", "value_json", "updated_at_ms"])
                    .filter(Box::new(where_eq("key", key)))
                    .execute(database.as_ref())
                    .await?;
                rows.into_iter()
                    .next()
                    .map(|row| setting_value_from_row(&row))
                    .transpose()
            })
        })
    }

    /// Persist onboarding progress metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings database cannot be opened, migrated,
    /// or written.
    pub fn save_onboarding_progress(&self, progress: &OnboardingProgress) -> SettingsResult<()> {
        let progress = progress.clone();
        self.with_database(move |database| {
            Box::pin(async move {
                database
                    .upsert("onboarding_progress")
                    .value("id", ONBOARDING_PROGRESS_ID)
                    .value("mode", progress.mode)
                    .value(
                        "first_run_completed",
                        bool_to_i64(progress.first_run_completed),
                    )
                    .value("last_section", progress.last_section)
                    .value(
                        "last_opened_at_ms",
                        progress.last_opened_at_ms.map(u64_to_i64),
                    )
                    .value("completed_at_ms", progress.completed_at_ms.map(u64_to_i64))
                    .execute(database.as_ref())
                    .await?;
                Ok(())
            })
        })
    }

    /// Return onboarding progress metadata if it has been persisted.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings database cannot be opened, migrated,
    /// or read.
    pub fn onboarding_progress(&self) -> SettingsResult<Option<OnboardingProgress>> {
        self.with_database(move |database| {
            Box::pin(async move {
                let rows = database
                    .select("onboarding_progress")
                    .columns(&[
                        "mode",
                        "first_run_completed",
                        "last_section",
                        "last_opened_at_ms",
                        "completed_at_ms",
                    ])
                    .filter(Box::new(where_eq("id", ONBOARDING_PROGRESS_ID)))
                    .execute(database.as_ref())
                    .await?;
                rows.into_iter()
                    .next()
                    .map(|row| onboarding_progress_from_row(&row))
                    .transpose()
            })
        })
    }

    /// Persist one onboarding setup-map section state.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings database cannot be opened, migrated,
    /// or written.
    pub fn save_onboarding_section(&self, section: &OnboardingSection) -> SettingsResult<()> {
        let section = section.clone();
        self.with_database(move |database| {
            Box::pin(async move {
                database
                    .upsert("onboarding_sections")
                    .value("section_id", section.section_id)
                    .value("status", section.status)
                    .value("visited", bool_to_i64(section.visited))
                    .value("visited_at_ms", section.visited_at_ms.map(u64_to_i64))
                    .value("completed_at_ms", section.completed_at_ms.map(u64_to_i64))
                    .value("skipped_at_ms", section.skipped_at_ms.map(u64_to_i64))
                    .value("dismissed", bool_to_i64(section.dismissed))
                    .execute(database.as_ref())
                    .await?;
                Ok(())
            })
        })
    }

    /// Return all persisted onboarding setup-map section states.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings database cannot be opened, migrated,
    /// or read.
    pub fn onboarding_sections(&self) -> SettingsResult<Vec<OnboardingSection>> {
        self.with_database(move |database| {
            Box::pin(async move {
                database
                    .select("onboarding_sections")
                    .columns(&[
                        "section_id",
                        "status",
                        "visited",
                        "visited_at_ms",
                        "completed_at_ms",
                        "skipped_at_ms",
                        "dismissed",
                    ])
                    .execute(database.as_ref())
                    .await?
                    .into_iter()
                    .map(|row| onboarding_section_from_row(&row))
                    .collect()
            })
        })
    }

    /// Persist one sanitized detection cache entry.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings database cannot be opened, migrated,
    /// serialized, or written.
    pub fn save_detection_cache_entry(&self, entry: &DetectionCacheEntry) -> SettingsResult<()> {
        let entry = entry.clone();
        let value_json = serde_json::to_string(&entry.value)?;
        self.with_database(move |database| {
            Box::pin(async move {
                database
                    .upsert("detection_cache")
                    .value("detection_id", detection_id(&entry.detector, &entry.key))
                    .value("detector", entry.detector)
                    .value("key", entry.key)
                    .value("value_json", value_json)
                    .value("confidence", entry.confidence)
                    .value("source", entry.source)
                    .value("detected_at_ms", u64_to_i64(entry.detected_at_ms))
                    .value("expires_at_ms", entry.expires_at_ms.map(u64_to_i64))
                    .execute(database.as_ref())
                    .await?;
                Ok(())
            })
        })
    }

    /// Return all sanitized detection cache entries.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings database cannot be opened, migrated,
    /// read, or decoded.
    pub fn detection_cache_entries(&self) -> SettingsResult<Vec<DetectionCacheEntry>> {
        self.with_database(move |database| {
            Box::pin(async move {
                database
                    .select("detection_cache")
                    .columns(&[
                        "detector",
                        "key",
                        "value_json",
                        "confidence",
                        "source",
                        "detected_at_ms",
                        "expires_at_ms",
                    ])
                    .execute(database.as_ref())
                    .await?
                    .into_iter()
                    .map(|row| detection_cache_entry_from_row(&row))
                    .collect()
            })
        })
    }

    /// Persist one setup recommendation.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings database cannot be opened, migrated,
    /// or written.
    pub fn save_setup_recommendation(
        &self,
        recommendation: &SetupRecommendation,
    ) -> SettingsResult<()> {
        let recommendation = recommendation.clone();
        self.with_database(move |database| {
            Box::pin(async move {
                database
                    .upsert("setup_recommendations")
                    .value("recommendation_id", recommendation.recommendation_id)
                    .value("section_id", recommendation.section_id)
                    .value("kind", recommendation.kind)
                    .value("status", recommendation.status)
                    .value("priority", i64::from(recommendation.priority))
                    .value("title", recommendation.title)
                    .value("body", recommendation.body)
                    .value("created_at_ms", u64_to_i64(recommendation.created_at_ms))
                    .value(
                        "dismissed_at_ms",
                        recommendation.dismissed_at_ms.map(u64_to_i64),
                    )
                    .execute(database.as_ref())
                    .await?;
                Ok(())
            })
        })
    }

    /// Return all setup recommendations.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings database cannot be opened, migrated,
    /// or read.
    pub fn setup_recommendations(&self) -> SettingsResult<Vec<SetupRecommendation>> {
        self.with_database(move |database| {
            Box::pin(async move {
                database
                    .select("setup_recommendations")
                    .columns(&[
                        "recommendation_id",
                        "section_id",
                        "kind",
                        "status",
                        "priority",
                        "title",
                        "body",
                        "created_at_ms",
                        "dismissed_at_ms",
                    ])
                    .execute(database.as_ref())
                    .await?
                    .into_iter()
                    .map(|row| setup_recommendation_from_row(&row))
                    .collect()
            })
        })
    }

    fn with_database<T>(
        &self,
        operation: impl FnOnce(
            Box<dyn Database>,
        )
            -> Pin<Box<dyn Future<Output = SettingsResult<T>> + Send + 'static>>
        + Send
        + 'static,
    ) -> SettingsResult<T>
    where
        T: Send + 'static,
    {
        let database_path = self.settings_db_path.clone();
        if let Some(parent) = database_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread Tokio runtime should build");
            runtime.block_on(async {
                let database = open_database(&database_path).await?;
                run_migrations(database.as_ref()).await?;
                operation(database).await
            })
        })
        .join()
        .map_err(|_| SettingsError::DatabaseWorkerPanicked)?
    }
}

/// Stored generic JSON setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsValue {
    /// Setting key.
    pub key: String,
    /// Setting JSON value.
    pub value: Value,
    /// Last update timestamp in milliseconds since Unix epoch.
    pub updated_at_ms: u64,
}

/// Persisted onboarding lifecycle metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnboardingProgress {
    /// Onboarding/control-center mode name.
    pub mode: String,
    /// Whether first-run onboarding has completed.
    pub first_run_completed: bool,
    /// Last focused setup-map section, if known.
    pub last_section: Option<String>,
    /// Last opened timestamp in milliseconds since Unix epoch.
    pub last_opened_at_ms: Option<u64>,
    /// Completion timestamp in milliseconds since Unix epoch.
    pub completed_at_ms: Option<u64>,
}

/// Persisted setup-map section metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnboardingSection {
    /// Stable setup-map section identifier.
    pub section_id: String,
    /// Persisted UX status hint.
    pub status: String,
    /// Whether the user has visited this section.
    pub visited: bool,
    /// Visit timestamp in milliseconds since Unix epoch.
    pub visited_at_ms: Option<u64>,
    /// Completion timestamp in milliseconds since Unix epoch.
    pub completed_at_ms: Option<u64>,
    /// Skip timestamp in milliseconds since Unix epoch.
    pub skipped_at_ms: Option<u64>,
    /// Whether the user dismissed this section's recommendation.
    pub dismissed: bool,
}

/// Sanitized non-secret detection cache entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectionCacheEntry {
    /// Detector name.
    pub detector: String,
    /// Detector-local key.
    pub key: String,
    /// Sanitized JSON value.
    pub value: Value,
    /// Confidence label.
    pub confidence: String,
    /// Source label.
    pub source: String,
    /// Detection timestamp in milliseconds since Unix epoch.
    pub detected_at_ms: u64,
    /// Optional expiry timestamp in milliseconds since Unix epoch.
    pub expires_at_ms: Option<u64>,
}

/// Persisted setup recommendation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupRecommendation {
    /// Stable recommendation identifier.
    pub recommendation_id: String,
    /// Related setup-map section identifier.
    pub section_id: String,
    /// Recommendation kind.
    pub kind: String,
    /// Recommendation status.
    pub status: String,
    /// Sort priority. Higher values can be surfaced first.
    pub priority: i32,
    /// User-facing title.
    pub title: String,
    /// User-facing body.
    pub body: String,
    /// Creation timestamp in milliseconds since Unix epoch.
    pub created_at_ms: u64,
    /// Dismissal timestamp in milliseconds since Unix epoch.
    pub dismissed_at_ms: Option<u64>,
}

/// Settings persistence result type.
pub type SettingsResult<T> = Result<T, SettingsError>;

/// Errors returned by settings persistence operations.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// An I/O operation failed.
    #[error("settings I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// The database could not be opened.
    #[error("failed to open settings database: {0}")]
    DatabaseOpen(String),
    /// A database operation failed.
    #[error("settings database operation failed: {0}")]
    Database(#[from] DatabaseError),
    /// A schema migration failed.
    #[error("settings database migration failed: {0}")]
    Migration(String),
    /// JSON encoding or decoding failed.
    #[error("settings JSON operation failed: {0}")]
    Json(#[from] serde_json::Error),
    /// A required database column was missing or had an unexpected type.
    #[error("settings database row is missing required column {0}")]
    MissingColumn(&'static str),
    /// The database worker panicked.
    #[error("settings database worker panicked")]
    DatabaseWorkerPanicked,
}

async fn open_database(path: &Path) -> SettingsResult<Box<dyn Database>> {
    let mut attempt = 0_u32;
    let mut delay = DATABASE_OPEN_INITIAL_RETRY_DELAY;
    loop {
        match switchy::database_connection::builder()
            .turso()
            .with_path(path)
            .with_busy_timeout(DATABASE_BUSY_TIMEOUT)
            .with_multiprocess_wal(false)
            .build()
            .await
        {
            Ok(database) => return Ok(database),
            Err(error)
                if is_database_lock_error(&error) && attempt < DATABASE_OPEN_RETRY_ATTEMPTS =>
            {
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2).min(DATABASE_OPEN_MAX_RETRY_DELAY);
            }
            Err(error) => return Err(SettingsError::DatabaseOpen(error.to_string())),
        }
    }
}

fn is_database_lock_error(error: &impl std::fmt::Display) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("database is locked") || message.contains("busy")
}

async fn run_migrations(database: &dyn Database) -> SettingsResult<()> {
    let runner = MigrationRunner::new(Box::new(settings_migrations()))
        .with_table_name(MIGRATIONS_TABLE.to_owned());
    runner
        .run(database)
        .await
        .map_err(|error| SettingsError::Migration(error.to_string()))?;
    Ok(())
}

fn settings_migrations() -> CodeMigrationSource<'static> {
    let mut source = CodeMigrationSource::new();
    source.add_migration(settings_kv_table_migration());
    source.add_migration(onboarding_progress_table_migration());
    source.add_migration(onboarding_sections_table_migration());
    source.add_migration(detection_cache_table_migration());
    source.add_migration(setup_recommendations_table_migration());
    source
}

fn settings_kv_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "001_settings_kv".to_owned(),
        Box::new(
            create_table("settings_kv")
                .if_not_exists(true)
                .column(text_column("key"))
                .column(text_column("value_json"))
                .column(int_column("updated_at_ms"))
                .primary_key("key"),
        ),
        None,
    )
}

fn onboarding_progress_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "002_onboarding_progress".to_owned(),
        Box::new(
            create_table("onboarding_progress")
                .if_not_exists(true)
                .column(int_column("id"))
                .column(text_column("mode"))
                .column(int_column("first_run_completed"))
                .column(nullable_text_column("last_section"))
                .column(nullable_int_column("last_opened_at_ms"))
                .column(nullable_int_column("completed_at_ms"))
                .primary_key("id"),
        ),
        None,
    )
}

fn onboarding_sections_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "003_onboarding_sections".to_owned(),
        Box::new(
            create_table("onboarding_sections")
                .if_not_exists(true)
                .column(text_column("section_id"))
                .column(text_column("status"))
                .column(int_column("visited"))
                .column(nullable_int_column("visited_at_ms"))
                .column(nullable_int_column("completed_at_ms"))
                .column(nullable_int_column("skipped_at_ms"))
                .column(int_column("dismissed"))
                .primary_key("section_id"),
        ),
        None,
    )
}

fn detection_cache_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "004_detection_cache".to_owned(),
        Box::new(
            create_table("detection_cache")
                .if_not_exists(true)
                .column(text_column("detection_id"))
                .column(text_column("detector"))
                .column(text_column("key"))
                .column(text_column("value_json"))
                .column(text_column("confidence"))
                .column(text_column("source"))
                .column(int_column("detected_at_ms"))
                .column(nullable_int_column("expires_at_ms"))
                .primary_key("detection_id"),
        ),
        None,
    )
}

fn setup_recommendations_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "005_setup_recommendations".to_owned(),
        Box::new(
            create_table("setup_recommendations")
                .if_not_exists(true)
                .column(text_column("recommendation_id"))
                .column(text_column("section_id"))
                .column(text_column("kind"))
                .column(text_column("status"))
                .column(int_column("priority"))
                .column(text_column("title"))
                .column(text_column("body"))
                .column(int_column("created_at_ms"))
                .column(nullable_int_column("dismissed_at_ms"))
                .primary_key("recommendation_id"),
        ),
        None,
    )
}

fn text_column(name: &str) -> Column {
    Column {
        name: name.to_owned(),
        nullable: false,
        auto_increment: false,
        data_type: DataType::Text,
        default: None,
    }
}

fn nullable_text_column(name: &str) -> Column {
    Column {
        name: name.to_owned(),
        nullable: true,
        auto_increment: false,
        data_type: DataType::Text,
        default: None,
    }
}

fn int_column(name: &str) -> Column {
    Column {
        name: name.to_owned(),
        nullable: false,
        auto_increment: false,
        data_type: DataType::BigInt,
        default: None,
    }
}

fn nullable_int_column(name: &str) -> Column {
    Column {
        name: name.to_owned(),
        nullable: true,
        auto_increment: false,
        data_type: DataType::BigInt,
        default: None,
    }
}

fn setting_value_from_row(row: &Row) -> SettingsResult<SettingsValue> {
    Ok(SettingsValue {
        key: required_text(row, "key")?,
        value: serde_json::from_str(&required_text(row, "value_json")?)?,
        updated_at_ms: i64_to_u64(required_i64(row, "updated_at_ms")?),
    })
}

fn onboarding_progress_from_row(row: &Row) -> SettingsResult<OnboardingProgress> {
    Ok(OnboardingProgress {
        mode: required_text(row, "mode")?,
        first_run_completed: required_bool(row, "first_run_completed")?,
        last_section: optional_text(row, "last_section"),
        last_opened_at_ms: optional_i64(row, "last_opened_at_ms").map(i64_to_u64),
        completed_at_ms: optional_i64(row, "completed_at_ms").map(i64_to_u64),
    })
}

fn onboarding_section_from_row(row: &Row) -> SettingsResult<OnboardingSection> {
    Ok(OnboardingSection {
        section_id: required_text(row, "section_id")?,
        status: required_text(row, "status")?,
        visited: required_bool(row, "visited")?,
        visited_at_ms: optional_i64(row, "visited_at_ms").map(i64_to_u64),
        completed_at_ms: optional_i64(row, "completed_at_ms").map(i64_to_u64),
        skipped_at_ms: optional_i64(row, "skipped_at_ms").map(i64_to_u64),
        dismissed: required_bool(row, "dismissed")?,
    })
}

fn detection_cache_entry_from_row(row: &Row) -> SettingsResult<DetectionCacheEntry> {
    Ok(DetectionCacheEntry {
        detector: required_text(row, "detector")?,
        key: required_text(row, "key")?,
        value: serde_json::from_str(&required_text(row, "value_json")?)?,
        confidence: required_text(row, "confidence")?,
        source: required_text(row, "source")?,
        detected_at_ms: i64_to_u64(required_i64(row, "detected_at_ms")?),
        expires_at_ms: optional_i64(row, "expires_at_ms").map(i64_to_u64),
    })
}

fn setup_recommendation_from_row(row: &Row) -> SettingsResult<SetupRecommendation> {
    Ok(SetupRecommendation {
        recommendation_id: required_text(row, "recommendation_id")?,
        section_id: required_text(row, "section_id")?,
        kind: required_text(row, "kind")?,
        status: required_text(row, "status")?,
        priority: i32::try_from(required_i64(row, "priority")?).unwrap_or_default(),
        title: required_text(row, "title")?,
        body: required_text(row, "body")?,
        created_at_ms: i64_to_u64(required_i64(row, "created_at_ms")?),
        dismissed_at_ms: optional_i64(row, "dismissed_at_ms").map(i64_to_u64),
    })
}

fn required_text(row: &Row, column: &'static str) -> SettingsResult<String> {
    row.get(column)
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .ok_or(SettingsError::MissingColumn(column))
}

fn optional_text(row: &Row, column: &'static str) -> Option<String> {
    row.get(column)
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
}

fn required_i64(row: &Row, column: &'static str) -> SettingsResult<i64> {
    row.get(column)
        .and_then(|value| value.as_i64())
        .ok_or(SettingsError::MissingColumn(column))
}

fn optional_i64(row: &Row, column: &'static str) -> Option<i64> {
    row.get(column).and_then(|value| value.as_i64())
}

fn required_bool(row: &Row, column: &'static str) -> SettingsResult<bool> {
    Ok(required_i64(row, column)? != 0)
}

fn bool_to_i64(value: bool) -> i64 {
    i64::from(value)
}

fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn i64_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

fn detection_id(detector: &str, key: &str) -> String {
    format!("{detector}\u{1f}{key}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn settings_kv_round_trips() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let store = SettingsStore::from_settings_db_path(temp.path().join("settings.db"));

        store
            .put_setting("setup.theme", &json!({"name": "default"}), 100)
            .expect("setting should be written");
        let value = store
            .get_setting("setup.theme")
            .expect("setting should be read")
            .expect("setting should exist");

        assert_eq!(value.key, "setup.theme");
        assert_eq!(value.value, json!({"name": "default"}));
        assert_eq!(value.updated_at_ms, 100);
    }

    #[test]
    fn onboarding_progress_and_sections_round_trip() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let store = SettingsStore::from_settings_db_path(temp.path().join("settings.db"));
        let progress = OnboardingProgress {
            mode: "first_run".to_owned(),
            first_run_completed: false,
            last_section: Some("providers".to_owned()),
            last_opened_at_ms: Some(200),
            completed_at_ms: None,
        };
        let section = OnboardingSection {
            section_id: "providers".to_owned(),
            status: "visited".to_owned(),
            visited: true,
            visited_at_ms: Some(201),
            completed_at_ms: None,
            skipped_at_ms: None,
            dismissed: false,
        };

        store
            .save_onboarding_progress(&progress)
            .expect("progress should be saved");
        store
            .save_onboarding_section(&section)
            .expect("section should be saved");

        assert_eq!(
            store.onboarding_progress().expect("progress should load"),
            Some(progress)
        );
        assert_eq!(
            store.onboarding_sections().expect("sections should load"),
            vec![section]
        );
    }

    #[test]
    fn detection_cache_and_recommendations_round_trip_without_secret_values() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let store = SettingsStore::from_settings_db_path(temp.path().join("settings.db"));
        let detection = DetectionCacheEntry {
            detector: "environment".to_owned(),
            key: "OPENAI_API_KEY".to_owned(),
            value: json!({"present": true, "value_stored": false}),
            confidence: "high".to_owned(),
            source: "environment".to_owned(),
            detected_at_ms: 300,
            expires_at_ms: Some(400),
        };
        let recommendation = SetupRecommendation {
            recommendation_id: "secure-openai-env".to_owned(),
            section_id: "secure_vault".to_owned(),
            kind: "secure_env_secret".to_owned(),
            status: "recommended".to_owned(),
            priority: 100,
            title: "Secure detected OpenAI key".to_owned(),
            body: "Move this detected key into secure storage.".to_owned(),
            created_at_ms: 301,
            dismissed_at_ms: None,
        };

        store
            .save_detection_cache_entry(&detection)
            .expect("detection should be saved");
        store
            .save_setup_recommendation(&recommendation)
            .expect("recommendation should be saved");

        assert_eq!(
            store
                .detection_cache_entries()
                .expect("detections should load"),
            vec![detection]
        );
        assert_eq!(
            store
                .setup_recommendations()
                .expect("recommendations should load"),
            vec![recommendation]
        );
    }
}
