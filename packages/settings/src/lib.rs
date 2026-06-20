#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Settings orchestration and runtime control-state persistence for Bcode.
//!
//! User-editable Bcode configuration should remain in TOML/config layers so it
//! can be hand-edited, merged, templated, and overridden the same way existing
//! `bcode.toml` configuration works. This crate may grow service APIs that
//! safely write those TOML-backed overrides through the config domain.
//!
//! The database in this crate is for runtime/control-center state: onboarding
//! progress, visited setup-map sections, dismissed recommendations, sanitized
//! detection cache entries, and similar durable state that is not intended to be
//! hand-edited. It intentionally does not store secrets; credential values
//! belong in the configured secure auth store.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
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

/// Durable Bcode runtime settings/control-state database location.
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

    /// Persist a control-center JSON state value.
    ///
    /// This is for durable runtime/UI state that should not live in user-edited
    /// TOML config. User-configurable settings should be saved through
    /// dedicated config mutation APIs instead.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings database cannot be opened, migrated,
    /// serialized, or written.
    pub fn put_control_state(
        &self,
        key: &str,
        value: &Value,
        updated_at_ms: u64,
    ) -> SettingsResult<()> {
        let key = key.to_owned();
        let value_json = serde_json::to_string(value)?;
        self.with_database(move |database| {
            Box::pin(async move {
                database
                    .upsert("control_state_kv")
                    .value("key", key)
                    .value("value_json", value_json)
                    .value("updated_at_ms", u64_to_i64(updated_at_ms))
                    .execute(database.as_ref())
                    .await?;
                Ok(())
            })
        })
    }

    /// Return a control-center JSON state value.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings database cannot be opened, migrated,
    /// read, or decoded.
    pub fn control_state(&self, key: &str) -> SettingsResult<Option<ControlStateValue>> {
        let key = key.to_owned();
        self.with_database(move |database| {
            Box::pin(async move {
                let rows = database
                    .select("control_state_kv")
                    .columns(&["key", "value_json", "updated_at_ms"])
                    .filter(Box::new(where_eq("key", key)))
                    .execute(database.as_ref())
                    .await?;
                rows.into_iter()
                    .next()
                    .map(|row| control_state_value_from_row(&row))
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

/// Stored control-center JSON state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlStateValue {
    /// State key.
    pub key: String,
    /// State JSON value.
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

/// Stable setup-map sections used by first-run onboarding and Settings / Control Center.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupSectionId {
    /// First impression and setup overview.
    Welcome,
    /// Environment/config detection summary.
    Detection,
    /// Secure credential storage and device sealing.
    SecureVault,
    /// Provider authentication and profile setup.
    Providers,
    /// Model profile/default setup.
    Models,
    /// Permission preset/rule review.
    Permissions,
    /// Session import review.
    Imports,
    /// Plugin review/customization.
    Plugins,
    /// Final review and launch.
    Launch,
}

impl SetupSectionId {
    /// Return the stable storage identifier for this setup section.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Welcome => "welcome",
            Self::Detection => "detection",
            Self::SecureVault => "secure_vault",
            Self::Providers => "providers",
            Self::Models => "models",
            Self::Permissions => "permissions",
            Self::Imports => "imports",
            Self::Plugins => "plugins",
            Self::Launch => "launch",
        }
    }

    /// Return all setup sections in the default map order.
    #[must_use]
    pub const fn all() -> [Self; 9] {
        [
            Self::Welcome,
            Self::Detection,
            Self::SecureVault,
            Self::Providers,
            Self::Models,
            Self::Permissions,
            Self::Imports,
            Self::Plugins,
            Self::Launch,
        ]
    }
}

/// Reconciled setup section status for onboarding rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupSectionStatus {
    /// The user has not visited this section.
    Unvisited,
    /// The user has visited this section.
    Visited,
    /// This section is currently focused.
    Current,
    /// Real product/config state indicates this section is complete.
    Complete,
    /// This section has an active recommendation.
    Recommended,
    /// This section is optional for the current setup.
    Optional,
    /// The user explicitly skipped this section.
    Skipped,
    /// This section cannot be completed until another issue is resolved.
    Blocked,
    /// This section is complete and tied to secure storage state.
    Secured,
    /// This section needs attention.
    NeedsAttention,
}

impl SetupSectionStatus {
    /// Return the stable storage identifier for this status.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unvisited => "unvisited",
            Self::Visited => "visited",
            Self::Current => "current",
            Self::Complete => "complete",
            Self::Recommended => "recommended",
            Self::Optional => "optional",
            Self::Skipped => "skipped",
            Self::Blocked => "blocked",
            Self::Secured => "secured",
            Self::NeedsAttention => "needs_attention",
        }
    }
}

/// External real-state summary used to reconcile onboarding display state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupReconciliationInput {
    /// Currently focused setup section.
    pub current_section: Option<SetupSectionId>,
    /// Sections whose real product/config state is complete.
    pub configured_sections: BTreeSet<SetupSectionId>,
    /// Sections whose real product/config state is secured.
    pub secured_sections: BTreeSet<SetupSectionId>,
    /// Sections blocked by missing prerequisites or degraded state.
    pub blocked_sections: BTreeSet<SetupSectionId>,
    /// Sections with active recommendations.
    pub recommended_sections: BTreeSet<SetupSectionId>,
    /// Sections that are optional in the current setup.
    pub optional_sections: BTreeSet<SetupSectionId>,
}

/// Reconciled setup-map section snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciledSetupSection {
    /// Stable setup section identifier.
    pub section_id: SetupSectionId,
    /// Display status after reconciling persisted UX state with real state.
    pub status: SetupSectionStatus,
    /// Whether the user has visited this section.
    pub visited: bool,
}

/// Reconcile setup-map sections from persisted UX state and real product/config state.
#[must_use]
pub fn reconcile_setup_sections(
    persisted_sections: &[OnboardingSection],
    input: &SetupReconciliationInput,
) -> Vec<ReconciledSetupSection> {
    let visited_sections = persisted_sections
        .iter()
        .filter(|section| section.visited)
        .map(|section| section.section_id.as_str())
        .collect::<BTreeSet<_>>();
    let skipped_sections = persisted_sections
        .iter()
        .filter(|section| section.status == SetupSectionStatus::Skipped.as_str())
        .map(|section| section.section_id.as_str())
        .collect::<BTreeSet<_>>();

    SetupSectionId::all()
        .into_iter()
        .map(|section_id| {
            let section_key = section_id.as_str();
            let visited = visited_sections.contains(section_key);
            let status = if input.current_section == Some(section_id) {
                SetupSectionStatus::Current
            } else if input.secured_sections.contains(&section_id) {
                SetupSectionStatus::Secured
            } else if input.configured_sections.contains(&section_id) {
                SetupSectionStatus::Complete
            } else if input.blocked_sections.contains(&section_id) {
                SetupSectionStatus::Blocked
            } else if input.recommended_sections.contains(&section_id) {
                SetupSectionStatus::Recommended
            } else if skipped_sections.contains(section_key) {
                SetupSectionStatus::Skipped
            } else if input.optional_sections.contains(&section_id) {
                SetupSectionStatus::Optional
            } else if visited {
                SetupSectionStatus::Visited
            } else {
                SetupSectionStatus::Unvisited
            };
            ReconciledSetupSection {
                section_id,
                status,
                visited,
            }
        })
        .collect()
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
    source.add_migration(control_state_kv_table_migration());
    source.add_migration(onboarding_progress_table_migration());
    source.add_migration(onboarding_sections_table_migration());
    source.add_migration(detection_cache_table_migration());
    source.add_migration(setup_recommendations_table_migration());
    source
}

fn control_state_kv_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "001_control_state_kv".to_owned(),
        Box::new(
            create_table("control_state_kv")
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

fn control_state_value_from_row(row: &Row) -> SettingsResult<ControlStateValue> {
    Ok(ControlStateValue {
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
    fn control_state_kv_round_trips() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let store = SettingsStore::from_settings_db_path(temp.path().join("settings.db"));

        store
            .put_control_state("setup.theme", &json!({"name": "default"}), 100)
            .expect("control state should be written");
        let value = store
            .control_state("setup.theme")
            .expect("control state should be read")
            .expect("control state should exist");

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

    #[test]
    fn reconciliation_prefers_real_state_over_persisted_ux_state() {
        let persisted = vec![
            OnboardingSection {
                section_id: SetupSectionId::Providers.as_str().to_owned(),
                status: SetupSectionStatus::Visited.as_str().to_owned(),
                visited: true,
                visited_at_ms: Some(10),
                completed_at_ms: None,
                skipped_at_ms: None,
                dismissed: false,
            },
            OnboardingSection {
                section_id: SetupSectionId::Imports.as_str().to_owned(),
                status: SetupSectionStatus::Skipped.as_str().to_owned(),
                visited: true,
                visited_at_ms: Some(11),
                completed_at_ms: None,
                skipped_at_ms: Some(12),
                dismissed: false,
            },
        ];
        let input = SetupReconciliationInput {
            current_section: Some(SetupSectionId::Models),
            configured_sections: BTreeSet::from([SetupSectionId::Providers]),
            secured_sections: BTreeSet::from([SetupSectionId::SecureVault]),
            blocked_sections: BTreeSet::from([SetupSectionId::Permissions]),
            recommended_sections: BTreeSet::from([SetupSectionId::Plugins]),
            optional_sections: BTreeSet::from([SetupSectionId::Imports]),
        };

        let sections = reconcile_setup_sections(&persisted, &input);
        let status_for = |section_id| {
            sections
                .iter()
                .find(|section| section.section_id == section_id)
                .expect("section should exist")
                .status
        };

        assert_eq!(
            status_for(SetupSectionId::Models),
            SetupSectionStatus::Current
        );
        assert_eq!(
            status_for(SetupSectionId::SecureVault),
            SetupSectionStatus::Secured
        );
        assert_eq!(
            status_for(SetupSectionId::Providers),
            SetupSectionStatus::Complete
        );
        assert_eq!(
            status_for(SetupSectionId::Permissions),
            SetupSectionStatus::Blocked
        );
        assert_eq!(
            status_for(SetupSectionId::Plugins),
            SetupSectionStatus::Recommended
        );
        assert_eq!(
            status_for(SetupSectionId::Imports),
            SetupSectionStatus::Skipped
        );
        assert_eq!(
            status_for(SetupSectionId::Welcome),
            SetupSectionStatus::Unvisited
        );
    }
}
