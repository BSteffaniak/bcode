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
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
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

    /// Check whether the settings DB can be opened and migrated.
    ///
    /// This is intended for Settings / Control Center degraded-state handling.
    #[must_use]
    pub fn health(&self) -> SettingsDbHealth {
        match self.with_database(|_| Box::pin(async { Ok(()) })) {
            Ok(()) => SettingsDbHealth::Available,
            Err(error) => SettingsDbHealth::Unavailable {
                message: error.to_string(),
            },
        }
    }

    /// Reset the settings database and `SQLite` sidecar files.
    ///
    /// This is intended for explicit repair/reset flows only. User-editable TOML
    /// configuration and secure auth vaults are not touched.
    ///
    /// # Errors
    ///
    /// Returns an error when any existing DB file or sidecar cannot be removed.
    pub fn reset_database(&self) -> SettingsResult<()> {
        remove_file_if_exists(&self.settings_db_path)?;
        remove_file_if_exists(&sqlite_sidecar_path(&self.settings_db_path, "wal"))?;
        remove_file_if_exists(&sqlite_sidecar_path(&self.settings_db_path, "shm"))?;
        Ok(())
    }

    /// Persist a setup readiness report into control state.
    ///
    /// # Errors
    ///
    /// Returns an error when the report cannot be serialized or persisted.
    pub fn save_readiness_report(
        &self,
        report: &SetupReadinessReport,
        updated_at_ms: u64,
    ) -> SettingsResult<()> {
        self.put_control_state(
            "setup.readiness_report",
            &serde_json::to_value(report)?,
            updated_at_ms,
        )
    }

    /// Return the persisted onboarding draft setup choices.
    ///
    /// # Errors
    ///
    /// Returns an error when draft state cannot be decoded.
    pub fn onboarding_draft_setup(&self) -> SettingsResult<OnboardingDraftSetup> {
        self.control_state("onboarding.draft_setup")?
            .map(|value| serde_json::from_value(value.value).map_err(Into::into))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    /// Persist onboarding draft setup choices.
    ///
    /// # Errors
    ///
    /// Returns an error when draft state cannot be written.
    pub fn save_onboarding_draft_setup(
        &self,
        draft: &OnboardingDraftSetup,
        saved_at_ms: u64,
    ) -> SettingsResult<()> {
        self.put_control_state(
            "onboarding.draft_setup",
            &serde_json::to_value(draft)?,
            saved_at_ms,
        )
    }

    /// Toggle a draft provider selection.
    ///
    /// # Errors
    ///
    /// Returns an error when draft state cannot be read or written.
    pub fn toggle_draft_provider(
        &self,
        provider: &str,
        at_ms: u64,
    ) -> SettingsResult<OnboardingDraftSetup> {
        let mut draft = self.onboarding_draft_setup()?;
        draft.toggle_provider(provider);
        self.save_onboarding_draft_setup(&draft, at_ms)?;
        Ok(draft)
    }

    /// Toggle a draft auth profile selection.
    ///
    /// # Errors
    ///
    /// Returns an error when draft state cannot be read or written.
    pub fn toggle_draft_auth_profile(
        &self,
        profile: &str,
        at_ms: u64,
    ) -> SettingsResult<OnboardingDraftSetup> {
        let mut draft = self.onboarding_draft_setup()?;
        draft.toggle_auth_profile(profile);
        self.save_onboarding_draft_setup(&draft, at_ms)?;
        Ok(draft)
    }

    /// Select a draft model profile.
    ///
    /// # Errors
    ///
    /// Returns an error when draft state cannot be read or written.
    pub fn select_draft_model_profile(
        &self,
        profile: &str,
        at_ms: u64,
    ) -> SettingsResult<OnboardingDraftSetup> {
        let mut draft = self.onboarding_draft_setup()?;
        draft.set_model_profile(profile);
        self.save_onboarding_draft_setup(&draft, at_ms)?;
        Ok(draft)
    }

    /// Cycle the draft permission preset.
    ///
    /// # Errors
    ///
    /// Returns an error when draft state cannot be read or written.
    pub fn cycle_draft_permission_preset(
        &self,
        at_ms: u64,
    ) -> SettingsResult<OnboardingDraftSetup> {
        let mut draft = self.onboarding_draft_setup()?;
        draft.cycle_permission_preset();
        self.save_onboarding_draft_setup(&draft, at_ms)?;
        Ok(draft)
    }

    /// Return the persisted setup readiness report if one exists.
    ///
    /// # Errors
    ///
    /// Returns an error when the report cannot be read or decoded.
    pub fn readiness_report(&self) -> SettingsResult<Option<SetupReadinessReport>> {
        self.control_state("setup.readiness_report")?
            .map(|value| serde_json::from_value(value.value).map_err(Into::into))
            .transpose()
    }

    /// Reconcile applied onboarding plan state against actual config/auth state.
    ///
    /// # Errors
    ///
    /// Returns an error when detection state cannot be read.
    pub fn reconcile_setup_apply(
        &self,
        config: &bcode_config::BcodeConfig,
    ) -> SettingsResult<SetupApplyReconciliation> {
        let draft = self.onboarding_draft_setup()?;
        let detection_entries = self.detection_cache_entries()?;
        let secure_import_plans = secure_import_plans_from_detection(&detection_entries);
        let auth_detection = detect_auth_security_from_config(config);
        Ok(SetupApplyReconciliation {
            draft_present: draft != OnboardingDraftSetup::default(),
            config_summary: SetupConfigSummary::from_config(config),
            secure_import_reconciliation: reconcile_secure_import_plans(
                &secure_import_plans,
                &auth_detection,
            ),
        })
    }

    /// Apply a generated setup plan through available settings/domain services.
    ///
    /// Current mutating plan actions are represented as persisted
    /// recommendations/control-state transitions. Secret import plans are still
    /// explicit preview objects and must be applied by the secure auth domain so
    /// this method never handles raw secret values.
    ///
    /// # Errors
    ///
    /// Returns an error when recommendation/control-state updates cannot be
    /// persisted.
    pub fn apply_setup_plan(
        &self,
        plan: &SetupPlanReview,
        applied_at_ms: u64,
    ) -> SettingsResult<AppliedSetupPlan> {
        let mut applied_actions = Vec::new();
        let mut skipped_actions = Vec::new();
        for action in &plan.actions {
            if action.kind == "launch" {
                skipped_actions.push(action.clone());
                continue;
            }
            if action.mutating {
                self.put_control_state(
                    &format!("setup.applied.{}", action.kind),
                    &json!({
                        "section_id": action.section_id.as_str(),
                        "applied_at_ms": applied_at_ms,
                    }),
                    applied_at_ms,
                )?;
                applied_actions.push(action.clone());
            } else {
                skipped_actions.push(action.clone());
            }
        }
        let result = AppliedSetupPlan {
            applied_actions,
            skipped_actions,
        };
        self.put_control_state(
            "setup.last_applied_plan",
            &serde_json::to_value(&result)?,
            applied_at_ms,
        )?;
        Ok(result)
    }

    /// Return the current settings/onboarding experience mode.
    ///
    /// # Errors
    ///
    /// Returns an error when onboarding progress cannot be read.
    pub fn experience_mode(&self) -> SettingsResult<SettingsExperienceMode> {
        let progress = self.onboarding_progress()?;
        Ok(settings_experience_mode(progress.as_ref()))
    }

    /// Add a model to the runtime TOML-backed ignore state.
    ///
    /// This is a user-visible settings mutation, so it writes through the
    /// config package instead of storing the ignore rule in this database.
    ///
    /// # Errors
    ///
    /// Returns an error when the TOML-backed model ignore state cannot be read
    /// or written.
    pub fn ignore_model(
        &self,
        provider_plugin_id: &str,
        model_id: String,
    ) -> SettingsResult<PathBuf> {
        bcode_config::ignore_model_in_state(provider_plugin_id, model_id).map_err(Into::into)
    }

    /// Remove a model from the runtime TOML-backed ignore state.
    ///
    /// # Errors
    ///
    /// Returns an error when the TOML-backed model ignore state cannot be read
    /// or written.
    pub fn unignore_model(
        &self,
        provider_plugin_id: &str,
        model_id: &str,
    ) -> SettingsResult<PathBuf> {
        bcode_config::unignore_model_in_state(provider_plugin_id, model_id).map_err(Into::into)
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
        let sanitized_value = redact_secret_like_json(value.clone());
        let value_json = serde_json::to_string(&sanitized_value)?;
        self.with_database(move |database| {
            Box::pin(async move {
                database
                    .upsert("control_state_kv")
                    .unique(&["key"])
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
                    .unique(&["id"])
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

    /// Persist the current onboarding section and mark it visited.
    ///
    /// # Errors
    ///
    /// Returns an error when onboarding progress or section state cannot be
    /// written.
    pub fn visit_onboarding_section(
        &self,
        section_id: SetupSectionId,
        visited_at_ms: u64,
    ) -> SettingsResult<()> {
        self.save_onboarding_progress(&OnboardingProgress {
            mode: "first_run".to_owned(),
            first_run_completed: false,
            last_section: Some(section_id.as_str().to_owned()),
            last_opened_at_ms: Some(visited_at_ms),
            completed_at_ms: None,
        })?;
        self.save_onboarding_section(&OnboardingSection {
            section_id: section_id.as_str().to_owned(),
            status: SetupSectionStatus::Visited.as_str().to_owned(),
            visited: true,
            visited_at_ms: Some(visited_at_ms),
            completed_at_ms: None,
            skipped_at_ms: None,
            dismissed: false,
        })
    }

    /// Persist onboarding as completed.
    ///
    /// # Errors
    ///
    /// Returns an error when onboarding progress cannot be written.
    pub fn complete_onboarding(&self, completed_at_ms: u64) -> SettingsResult<()> {
        self.save_onboarding_progress(&OnboardingProgress {
            mode: "first_run".to_owned(),
            first_run_completed: true,
            last_section: Some(SetupSectionId::Launch.as_str().to_owned()),
            last_opened_at_ms: Some(completed_at_ms),
            completed_at_ms: Some(completed_at_ms),
        })
    }

    /// Mark an onboarding section skipped.
    ///
    /// # Errors
    ///
    /// Returns an error when section state cannot be written.
    pub fn skip_onboarding_section(
        &self,
        section_id: SetupSectionId,
        skipped_at_ms: u64,
    ) -> SettingsResult<()> {
        self.save_onboarding_section(&OnboardingSection {
            section_id: section_id.as_str().to_owned(),
            status: SetupSectionStatus::Skipped.as_str().to_owned(),
            visited: true,
            visited_at_ms: Some(skipped_at_ms),
            completed_at_ms: None,
            skipped_at_ms: Some(skipped_at_ms),
            dismissed: false,
        })
    }

    /// Mark an onboarding section complete.
    ///
    /// # Errors
    ///
    /// Returns an error when section state cannot be written.
    pub fn complete_onboarding_section(
        &self,
        section_id: SetupSectionId,
        completed_at_ms: u64,
    ) -> SettingsResult<()> {
        self.save_onboarding_section(&OnboardingSection {
            section_id: section_id.as_str().to_owned(),
            status: SetupSectionStatus::Complete.as_str().to_owned(),
            visited: true,
            visited_at_ms: Some(completed_at_ms),
            completed_at_ms: Some(completed_at_ms),
            skipped_at_ms: None,
            dismissed: false,
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
                    .delete("onboarding_sections")
                    .filter(Box::new(where_eq("section_id", section.section_id.clone())))
                    .execute(database.as_ref())
                    .await?;
                database
                    .insert("onboarding_sections")
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
                    .unique(&["detection_id"])
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

    /// Persist a setup detection snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when any sanitized detection entry or recommendation
    /// cannot be written.
    pub fn save_setup_detection_snapshot(
        &self,
        snapshot: &SetupDetectionSnapshot,
    ) -> SettingsResult<()> {
        for entry in &snapshot.entries {
            self.save_detection_cache_entry(entry)?;
        }
        for recommendation in &snapshot.recommendations {
            self.save_setup_recommendation(recommendation)?;
        }
        Ok(())
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
                    .delete("setup_recommendations")
                    .filter(Box::new(where_eq(
                        "recommendation_id",
                        recommendation.recommendation_id.clone(),
                    )))
                    .execute(database.as_ref())
                    .await?;
                database
                    .insert("setup_recommendations")
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

    /// Dismiss a setup recommendation.
    ///
    /// # Errors
    ///
    /// Returns an error when recommendation state cannot be read or written.
    pub fn dismiss_setup_recommendation(
        &self,
        recommendation_id: &str,
        dismissed_at_ms: u64,
    ) -> SettingsResult<Option<SetupRecommendation>> {
        let mut recommendations = self.setup_recommendations()?;
        let Some(recommendation) = recommendations
            .iter_mut()
            .find(|recommendation| recommendation.recommendation_id == recommendation_id)
        else {
            return Ok(None);
        };
        "dismissed".clone_into(&mut recommendation.status);
        recommendation.dismissed_at_ms = Some(dismissed_at_ms);
        let updated = recommendation.clone();
        self.save_setup_recommendation(&updated)?;
        Ok(Some(updated))
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

/// Secret-safety audit report for persisted/rendered setup state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretAuditReport {
    /// Whether the audited payload appears secret-safe.
    pub safe: bool,
    /// Labels describing suspected secret-bearing fields or values.
    pub findings: Vec<String>,
}

/// Audit JSON/text intended for settings DB, logs, or TUI snapshots for obvious secret material.
#[must_use]
pub fn audit_no_secret_material(label: &str, payload: &str) -> SecretAuditReport {
    let lower = payload.to_ascii_lowercase();
    let mut findings = Vec::new();
    for marker in [
        "sk-",
        "api_key=",
        "apikey=",
        "access_token=",
        "secret_access_key=",
        "password=",
        "bearer ",
    ] {
        if lower.contains(marker) {
            findings.push(format!("{label} contains marker {marker}"));
        }
    }
    SecretAuditReport {
        safe: findings.is_empty(),
        findings,
    }
}

/// Recursively redact secret-like JSON keys before storing diagnostic state.
#[must_use]
pub fn redact_secret_like_json(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    let redacted = if is_secret_like_key(&key) {
                        json!({ "redacted": true })
                    } else {
                        redact_secret_like_json(value)
                    };
                    (key, redacted)
                })
                .collect(),
        ),
        Value::Array(values) => {
            Value::Array(values.into_iter().map(redact_secret_like_json).collect())
        }
        other => other,
    }
}

fn is_secret_like_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower.contains("secret")
        || lower.contains("token")
        || lower.contains("password")
        || lower.contains("api_key")
        || lower.contains("apikey")
}

/// Settings database health state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SettingsDbHealth {
    /// The database opened and migrations completed successfully.
    Available,
    /// The database is unavailable or corrupt and settings UI should degrade.
    Unavailable {
        /// User-facing diagnostic message.
        message: String,
    },
}

/// Settings/control-center degraded-state panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsDegradedPanel {
    /// Whether normal control-center state is available.
    pub available: bool,
    /// User-facing diagnostic copy.
    pub message: String,
    /// Whether a reset/repair action should be offered.
    pub reset_available: bool,
}

/// Build degraded-state copy for the settings database health state.
#[must_use]
pub fn settings_degraded_panel(health: &SettingsDbHealth) -> SettingsDegradedPanel {
    match health {
        SettingsDbHealth::Available => SettingsDegradedPanel {
            available: true,
            message: "Settings state is available.".to_owned(),
            reset_available: false,
        },
        SettingsDbHealth::Unavailable { message } => SettingsDegradedPanel {
            available: false,
            message: format!(
                "Settings state is unavailable. Bcode can keep running with degraded onboarding/control-center state. Details: {message}"
            ),
            reset_available: true,
        },
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

/// User-visible config capability detected for setup/onboarding reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupConfigCapability {
    /// Config currently has a provider selection.
    ProviderSelection,
    /// Config currently has a model selection.
    ModelSelection,
    /// Config currently has auth profiles or legacy auth config.
    AuthConfiguration,
    /// Permission/agent config is present.
    PermissionConfiguration,
    /// Session import config is present.
    SessionImportConfiguration,
}

/// User-visible config summary for setup/onboarding reconciliation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupConfigSummary {
    /// Detected config capabilities.
    pub capabilities: BTreeSet<SetupConfigCapability>,
    /// Enabled plugin IDs.
    pub enabled_plugins: BTreeSet<String>,
    /// Disabled plugin IDs.
    pub disabled_plugins: BTreeSet<String>,
}

impl SetupConfigSummary {
    /// Build a setup summary from loaded Bcode config.
    #[must_use]
    pub fn from_config(config: &bcode_config::BcodeConfig) -> Self {
        let selection = config.resolved_model_selection();
        let mut capabilities = BTreeSet::new();
        if selection.provider_plugin_id.is_some() {
            capabilities.insert(SetupConfigCapability::ProviderSelection);
        }
        if selection.model_id.is_some() || selection.model_profile.is_some() {
            capabilities.insert(SetupConfigCapability::ModelSelection);
        }
        if config.auth.openai.is_some()
            || !config.auth.profiles.is_empty()
            || !config.auth.pools.is_empty()
        {
            capabilities.insert(SetupConfigCapability::AuthConfiguration);
        }
        if !config.agent.is_empty() {
            capabilities.insert(SetupConfigCapability::PermissionConfiguration);
        }
        if config.session_import != bcode_config::SessionImportConfig::default() {
            capabilities.insert(SetupConfigCapability::SessionImportConfiguration);
        }
        Self {
            capabilities,
            enabled_plugins: config.plugins.enabled.clone(),
            disabled_plugins: config.plugins.disabled.clone(),
        }
    }

    /// Convert this config summary into setup reconciliation facts.
    #[must_use]
    pub fn reconciliation_input(&self) -> SetupReconciliationInput {
        let mut input = SetupReconciliationInput::default();
        if self
            .capabilities
            .contains(&SetupConfigCapability::AuthConfiguration)
        {
            input.secured_sections.insert(SetupSectionId::SecureVault);
        }
        if self
            .capabilities
            .contains(&SetupConfigCapability::ProviderSelection)
            || self
                .capabilities
                .contains(&SetupConfigCapability::AuthConfiguration)
        {
            input.configured_sections.insert(SetupSectionId::Providers);
        }
        if self
            .capabilities
            .contains(&SetupConfigCapability::ModelSelection)
        {
            input.configured_sections.insert(SetupSectionId::Models);
        }
        if self
            .capabilities
            .contains(&SetupConfigCapability::PermissionConfiguration)
        {
            input
                .configured_sections
                .insert(SetupSectionId::Permissions);
        }
        if self
            .capabilities
            .contains(&SetupConfigCapability::SessionImportConfiguration)
        {
            input.configured_sections.insert(SetupSectionId::Imports);
        } else {
            input.optional_sections.insert(SetupSectionId::Imports);
        }
        if !self.enabled_plugins.is_empty() || !self.disabled_plugins.is_empty() {
            input.configured_sections.insert(SetupSectionId::Plugins);
        } else {
            input.optional_sections.insert(SetupSectionId::Plugins);
        }
        input
    }
}

/// Provider setup card view model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSetupCard {
    /// Provider/plugin identifier.
    pub provider_plugin_id: String,
    /// Whether the provider is selected/configured.
    pub configured: bool,
    /// Whether auth information exists for this provider.
    pub auth_configured: bool,
    /// Optional story copy explaining the next action.
    pub story: String,
}

/// Build a provider setup card.
#[must_use]
pub fn provider_setup_card(
    provider_plugin_id: impl Into<String>,
    configured: bool,
    auth_configured: bool,
) -> ProviderSetupCard {
    let provider_plugin_id = provider_plugin_id.into();
    let story = if configured && auth_configured {
        "This provider is ready to use with configured authentication.".to_owned()
    } else if configured {
        "This provider is selected, but Bcode still needs a secure auth path.".to_owned()
    } else {
        "Choose this provider if it matches the models and credentials you want Bcode to use."
            .to_owned()
    };
    ProviderSetupCard {
        provider_plugin_id,
        configured,
        auth_configured,
        story,
    }
}

/// Security trust panel view model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityTrustPanel {
    /// Whether sshenv/secure storage appears available.
    pub secure_storage_available: bool,
    /// Whether device sealing appears available.
    pub device_seal_available: bool,
    /// Storytelling copy for the UI.
    pub story: String,
}

/// Build security storytelling copy for onboarding.
#[must_use]
pub fn security_trust_panel(
    secure_storage_available: bool,
    device_seal_available: bool,
) -> SecurityTrustPanel {
    let story = match (secure_storage_available, device_seal_available) {
        (true, true) => "Secure storage and device sealing are available. Bcode can keep credentials out of plaintext config and bind encrypted secrets to this machine.",
        (true, false) => "Secure storage is available. Bcode can keep credentials out of plaintext shell files and config, even though device sealing was not detected.",
        (false, _) => "Secure storage is not ready yet. Bcode should guide you to a safer credential path before launch.",
    }
    .to_owned();
    SecurityTrustPanel {
        secure_storage_available,
        device_seal_available,
        story,
    }
}

/// Authentication security detection summary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthSecurityDetection {
    /// Whether any configured auth profile uses sshenv.
    pub sshenv_available: bool,
    /// Whether any configured auth profile requests device sealing.
    pub device_seal_requested: bool,
    /// Profiles backed by sshenv.
    pub sshenv_profiles: BTreeSet<String>,
}

/// Detect configured sshenv/device-seal auth state without opening or mutating vaults.
#[must_use]
pub fn detect_auth_security_from_config(
    config: &bcode_config::BcodeConfig,
) -> AuthSecurityDetection {
    let mut detection = AuthSecurityDetection::default();
    for (profile_name, profile) in &config.auth.profiles {
        if profile.backend == "sshenv" {
            detection.sshenv_available = true;
            detection.sshenv_profiles.insert(profile_name.clone());
            if profile
                .settings
                .get("device_seal")
                .is_none_or(|value| !matches!(value.as_str(), "off" | "false" | "disabled"))
            {
                detection.device_seal_requested = true;
            }
        }
    }
    if config
        .auth
        .openai
        .as_ref()
        .is_some_and(|auth| auth.backend == "sshenv")
    {
        detection.sshenv_available = true;
        detection.sshenv_profiles.insert("openai".to_owned());
        detection.device_seal_requested = true;
    }
    detection
}

/// Planned secure credential import sourced from an environment variable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecureCredentialImportPlan {
    /// Source environment variable name. The value is intentionally not stored.
    pub env_var: String,
    /// Target auth profile.
    pub auth_profile: String,
    /// Canonical credential key such as `api_key`.
    pub credential_key: String,
    /// Whether this plan requires secure storage before apply.
    pub requires_secure_storage: bool,
}

/// Build secure-import plans from sanitized detection entries.
#[must_use]
pub fn secure_import_plans_from_detection(
    entries: &[DetectionCacheEntry],
) -> Vec<SecureCredentialImportPlan> {
    entries
        .iter()
        .filter(|entry| {
            entry.detector == "environment"
                && matches!(
                    entry.key.as_str(),
                    "OPENAI_API_KEY" | "XAI_API_KEY" | "OPENROUTER_API_KEY" | "GITHUB_TOKEN"
                )
        })
        .map(|entry| SecureCredentialImportPlan {
            env_var: entry.key.clone(),
            auth_profile: auth_profile_for_env_var(&entry.key).to_owned(),
            credential_key: "api_key".to_owned(),
            requires_secure_storage: true,
        })
        .collect()
}

fn auth_profile_for_env_var(env_var: &str) -> &'static str {
    match env_var {
        "OPENAI_API_KEY" => "openai",
        "XAI_API_KEY" => "xai",
        "OPENROUTER_API_KEY" => "openrouter",
        "GITHUB_TOKEN" => "github",
        _ => "default",
    }
}

/// Security story panel for detected secure credential import opportunities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecureCredentialStoryPanel {
    /// User-facing headline.
    pub headline: String,
    /// Security story bullets.
    pub bullets: Vec<String>,
    /// Detected credential source names; values are never included.
    pub detected_sources: Vec<String>,
    /// Device-seal explanation.
    pub device_seal_message: String,
    /// Recommended next actions.
    pub recommended_actions: Vec<String>,
}

/// Build user-facing security story copy from sanitized secure-import plans.
#[must_use]
pub fn secure_credential_story_panel(
    plans: &[SecureCredentialImportPlan],
    auth_detection: &AuthSecurityDetection,
) -> SecureCredentialStoryPanel {
    let detected_sources = plans.iter().map(|plan| plan.env_var.clone()).collect();
    let device_seal_message = if auth_detection.device_seal_requested {
        "Device sealing is requested for configured sshenv profiles, so encrypted secrets can be bound to this machine where supported."
    } else {
        "Device sealing is not configured yet; onboarding can explain and enable it where supported."
    }
    .to_owned();
    SecureCredentialStoryPanel {
        headline: "Secure detected provider credentials".to_owned(),
        bullets: vec![
            "Bcode keeps provider secrets out of plaintext config.".to_owned(),
            "Bcode avoids scattering tokens across shell startup files.".to_owned(),
            "sshenv-backed storage is the guided secure path for loaded credentials.".to_owned(),
            "Secret values are never displayed, logged, or persisted in the settings database."
                .to_owned(),
        ],
        detected_sources,
        device_seal_message,
        recommended_actions: vec![
            "Import detected credentials into sshenv-backed secure storage.".to_owned(),
            "Reconcile auth profiles after import before marking setup complete.".to_owned(),
            "Remove plaintext environment exports after secure import if appropriate.".to_owned(),
        ],
    }
}

/// Post-import reconciliation summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecureImportReconciliation {
    /// Plans that now have matching configured sshenv auth profiles.
    pub imported_profiles: BTreeSet<String>,
    /// Plans that still need secure auth config.
    pub pending_profiles: BTreeSet<String>,
}

/// Reconcile secure-import plans against current config/auth state.
#[must_use]
pub fn reconcile_secure_import_plans(
    plans: &[SecureCredentialImportPlan],
    auth_detection: &AuthSecurityDetection,
) -> SecureImportReconciliation {
    let mut imported_profiles = BTreeSet::new();
    let mut pending_profiles = BTreeSet::new();
    for plan in plans {
        if auth_detection.sshenv_profiles.contains(&plan.auth_profile) {
            imported_profiles.insert(plan.auth_profile.clone());
        } else {
            pending_profiles.insert(plan.auth_profile.clone());
        }
    }
    SecureImportReconciliation {
        imported_profiles,
        pending_profiles,
    }
}

/// Model setup card view model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSetupCard {
    /// Selected model profile, if any.
    pub model_profile: Option<String>,
    /// Selected model identifier, if any.
    pub model_id: Option<String>,
    /// Whether a usable model selection exists.
    pub configured: bool,
    /// User-facing story copy.
    pub story: String,
}

/// Build a model setup card.
#[must_use]
pub fn model_setup_card(model_profile: Option<String>, model_id: Option<String>) -> ModelSetupCard {
    let configured = model_profile.is_some() || model_id.is_some();
    let story = if configured {
        "Bcode has a model path selected, so onboarding can focus on quality, cost, and context preferences."
            .to_owned()
    } else {
        "Choose a default model so Bcode can start quickly while still letting you switch models later."
            .to_owned()
    };
    ModelSetupCard {
        model_profile,
        model_id,
        configured,
        story,
    }
}

/// Permission setup card view model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionSetupCard {
    /// Whether explicit permission configuration exists.
    pub configured: bool,
    /// User-facing story copy.
    pub story: String,
}

/// Build a permission setup card.
#[must_use]
pub fn permission_setup_card(configured: bool) -> PermissionSetupCard {
    let story = if configured {
        "Permission rules are configured, so Bcode can explain and enforce your preferred safety boundaries."
            .to_owned()
    } else {
        "Review a permission preset so Bcode knows when to ask, when to proceed, and what should stay blocked."
            .to_owned()
    };
    PermissionSetupCard { configured, story }
}

/// Import setup card view model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportSetupCard {
    /// Whether session import configuration exists.
    pub configured: bool,
    /// Whether this section is optional.
    pub optional: bool,
    /// User-facing story copy.
    pub story: String,
}

/// Build an import setup card.
#[must_use]
pub fn import_setup_card(configured: bool) -> ImportSetupCard {
    let story = if configured {
        "Import settings are configured, so Bcode can help bring useful history forward without replaying sessions during normal setup."
            .to_owned()
    } else {
        "Session import is optional. Review it if you want Bcode to discover supported history sources later."
            .to_owned()
    };
    ImportSetupCard {
        configured,
        optional: !configured,
        story,
    }
}

/// Plugin setup card view model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSetupCard {
    /// Enabled plugin IDs.
    pub enabled_plugins: BTreeSet<String>,
    /// Disabled plugin IDs.
    pub disabled_plugins: BTreeSet<String>,
    /// User-facing story copy.
    pub story: String,
}

/// Build a plugin setup card.
#[must_use]
pub fn plugin_setup_card(
    enabled_plugins: BTreeSet<String>,
    disabled_plugins: BTreeSet<String>,
) -> PluginSetupCard {
    let story = if enabled_plugins.is_empty() && disabled_plugins.is_empty() {
        "Bundled plugins are ready by default, and every capability remains reviewable and disableable."
            .to_owned()
    } else {
        "Plugin choices are customized. Bcode will respect enabled and disabled plugin state during launch."
            .to_owned()
    };
    PluginSetupCard {
        enabled_plugins,
        disabled_plugins,
        story,
    }
}

/// Setup plan action generated before mutating configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupPlanAction {
    /// Related setup-map section.
    pub section_id: SetupSectionId,
    /// Stable action kind.
    pub kind: String,
    /// User-facing action title.
    pub title: String,
    /// User-facing action explanation.
    pub body: String,
    /// Whether this action mutates user config/state when applied.
    pub mutating: bool,
}

/// Onboarding startup command classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnboardingStartupCommand {
    /// Normal TUI startup with no explicit subcommand.
    NormalTui,
    /// Explicit onboarding command or flag.
    ExplicitOnboard,
    /// New-session startup.
    NewSession,
    /// Server/daemon command.
    Server,
    /// Session/history/import/repair command.
    SessionManagement,
    /// Scriptable non-interactive command.
    Scripting,
}

/// Decision for automatic first-run onboarding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnboardingAutoTriggerDecision {
    /// Whether onboarding should start automatically.
    pub should_start: bool,
    /// User/developer readable reason.
    pub reason: String,
}

/// Decide whether first-run onboarding should auto-start before the normal TUI.
#[must_use]
pub fn should_auto_start_onboarding(
    command: OnboardingStartupCommand,
    terminal_interactive: bool,
    progress: Option<&OnboardingProgress>,
    config_summary: &SetupConfigSummary,
) -> OnboardingAutoTriggerDecision {
    if command == OnboardingStartupCommand::ExplicitOnboard {
        return OnboardingAutoTriggerDecision {
            should_start: true,
            reason: "explicit onboarding requested".to_owned(),
        };
    }
    if command != OnboardingStartupCommand::NormalTui {
        return OnboardingAutoTriggerDecision {
            should_start: false,
            reason: "command is not normal TUI startup".to_owned(),
        };
    }
    if !terminal_interactive {
        return OnboardingAutoTriggerDecision {
            should_start: false,
            reason: "terminal is not interactive".to_owned(),
        };
    }
    if progress.is_some_and(|progress| progress.first_run_completed) {
        return OnboardingAutoTriggerDecision {
            should_start: false,
            reason: "first-run onboarding already completed".to_owned(),
        };
    }
    let has_provider = config_summary
        .capabilities
        .contains(&SetupConfigCapability::ProviderSelection);
    let has_model = config_summary
        .capabilities
        .contains(&SetupConfigCapability::ModelSelection);
    let has_auth = config_summary
        .capabilities
        .contains(&SetupConfigCapability::AuthConfiguration);
    if has_provider && has_model && has_auth {
        return OnboardingAutoTriggerDecision {
            should_start: false,
            reason: "usable provider/model/auth setup already exists".to_owned(),
        };
    }
    OnboardingAutoTriggerDecision {
        should_start: true,
        reason: "normal interactive startup has incomplete setup".to_owned(),
    }
}

/// Draft setup choices collected by onboarding before canonical config apply.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnboardingDraftSetup {
    /// Selected provider identifiers.
    pub providers: BTreeSet<String>,
    /// Selected auth profile names/subscriptions.
    pub auth_profiles: BTreeSet<String>,
    /// Selected default model profile.
    pub model_profile: Option<String>,
    /// Selected permission preset.
    pub permission_preset: Option<String>,
    /// Whether session import has been reviewed.
    pub session_import_reviewed: bool,
    /// Whether plugin setup has been reviewed.
    pub plugins_reviewed: bool,
}

impl OnboardingDraftSetup {
    /// Toggle a provider selection.
    pub fn toggle_provider(&mut self, provider: &str) {
        if !self.providers.remove(provider) {
            self.providers.insert(provider.to_owned());
        }
    }

    /// Toggle an auth profile/subscription selection.
    pub fn toggle_auth_profile(&mut self, profile: &str) {
        if !self.auth_profiles.remove(profile) {
            self.auth_profiles.insert(profile.to_owned());
        }
    }

    /// Set the selected default model profile.
    pub fn set_model_profile(&mut self, profile: &str) {
        self.model_profile = Some(profile.to_owned());
    }

    /// Cycle the deterministic onboarding permission preset.
    pub fn cycle_permission_preset(&mut self) {
        let next = if self.permission_preset.is_none()
            || self.permission_preset.as_deref() == Some("autonomous")
        {
            "cautious"
        } else if self.permission_preset.as_deref() == Some("balanced") {
            "autonomous"
        } else {
            "balanced"
        };
        self.permission_preset = Some(next.to_owned());
    }
}

/// Reconciliation result after setup apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupApplyReconciliation {
    /// Whether draft setup state exists and was considered.
    pub draft_present: bool,
    /// Config summary after apply.
    pub config_summary: SetupConfigSummary,
    /// Secure import reconciliation based on actual auth config.
    pub secure_import_reconciliation: SecureImportReconciliation,
}

/// Generate setup-plan review actions from draft choices and secure import plans.
#[must_use]
pub fn generate_setup_plan_from_draft(
    draft: &OnboardingDraftSetup,
    secure_import_plans: &[SecureCredentialImportPlan],
    config_summary: &SetupConfigSummary,
) -> SetupPlanReview {
    let mut actions = Vec::new();
    for provider in &draft.providers {
        actions.push(SetupPlanAction {
            section_id: SetupSectionId::Providers,
            kind: format!("provider:{provider}"),
            title: format!("Configure provider {provider}"),
            body: "Persist provider selection through config/domain services when applying setup."
                .to_owned(),
            mutating: true,
        });
    }
    for profile in &draft.auth_profiles {
        actions.push(SetupPlanAction {
            section_id: SetupSectionId::SecureVault,
            kind: format!("auth-profile:{profile}"),
            title: format!("Configure auth profile {profile}"),
            body: "Persist secure auth profile references without storing raw secret values."
                .to_owned(),
            mutating: true,
        });
    }
    if let Some(profile) = &draft.model_profile {
        actions.push(SetupPlanAction {
            section_id: SetupSectionId::Models,
            kind: format!("model-profile:{profile}"),
            title: format!("Select model profile {profile}"),
            body: "Persist model profile/default through TOML-backed config APIs.".to_owned(),
            mutating: true,
        });
    }
    if let Some(preset) = &draft.permission_preset {
        actions.push(SetupPlanAction {
            section_id: SetupSectionId::Permissions,
            kind: format!("permission-preset:{preset}"),
            title: format!("Use {preset} permission preset"),
            body: "Persist the selected permission behavior through permission/config services."
                .to_owned(),
            mutating: true,
        });
    }
    if draft.session_import_reviewed {
        actions.push(SetupPlanAction {
            section_id: SetupSectionId::Imports,
            kind: "session-import-reviewed".to_owned(),
            title: "Record session import review".to_owned(),
            body:
                "Record that import sources were reviewed without replaying or repairing sessions."
                    .to_owned(),
            mutating: true,
        });
    }
    if draft.plugins_reviewed {
        actions.push(SetupPlanAction {
            section_id: SetupSectionId::Plugins,
            kind: "plugins-reviewed".to_owned(),
            title: "Record plugin review".to_owned(),
            body: "Record plugin review while leaving canonical plugin state in config/plugin services."
                .to_owned(),
            mutating: true,
        });
    }
    for plan in secure_import_plans {
        actions.push(SetupPlanAction {
            section_id: SetupSectionId::SecureVault,
            kind: format!("secure-import-preview:{}", plan.auth_profile),
            title: format!("Securely import {}", plan.env_var),
            body: "Preview secure import into sshenv; raw secret values are never stored here."
                .to_owned(),
            mutating: false,
        });
    }
    let launch_ready = config_summary
        .capabilities
        .contains(&SetupConfigCapability::ProviderSelection)
        || !draft.providers.is_empty();
    SetupPlanReview {
        actions,
        launch_ready,
    }
}

/// Setup readiness severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupReadinessSeverity {
    /// Informational item.
    Info,
    /// Recommended but not blocking.
    Recommended,
    /// Blocks launch/readiness.
    Blocking,
}

/// Setup readiness item surfaced before launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupReadinessItem {
    /// Related setup section.
    pub section_id: SetupSectionId,
    /// Item severity.
    pub severity: SetupReadinessSeverity,
    /// User-facing title.
    pub title: String,
    /// User-facing body.
    pub body: String,
}

/// Aggregate setup readiness report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupReadinessReport {
    /// Readiness items.
    pub items: Vec<SetupReadinessItem>,
    /// Whether setup is launch-ready.
    pub launch_ready: bool,
}

/// Build a readiness report from reconciled setup sections and recommendations.
#[must_use]
pub fn setup_readiness_report(
    sections: &[ReconciledSetupSection],
    recommendations: &[SetupRecommendation],
) -> SetupReadinessReport {
    let mut items = Vec::new();
    for section in sections {
        match section.status {
            SetupSectionStatus::Blocked => items.push(SetupReadinessItem {
                section_id: section.section_id,
                severity: SetupReadinessSeverity::Blocking,
                title: format!("{} is blocked", section.section_id.as_str()),
                body: "Resolve this setup section before launch.".to_owned(),
            }),
            SetupSectionStatus::Recommended | SetupSectionStatus::NeedsAttention => {
                items.push(SetupReadinessItem {
                    section_id: section.section_id,
                    severity: SetupReadinessSeverity::Recommended,
                    title: format!("Review {}", section.section_id.as_str()),
                    body: "This setup section has recommended follow-up work.".to_owned(),
                });
            }
            _ => {}
        }
    }
    for recommendation in recommendations
        .iter()
        .filter(|recommendation| recommendation.dismissed_at_ms.is_none())
    {
        items.push(SetupReadinessItem {
            section_id: setup_section_id_from_str(&recommendation.section_id)
                .unwrap_or(SetupSectionId::Welcome),
            severity: SetupReadinessSeverity::Recommended,
            title: recommendation.title.clone(),
            body: recommendation.body.clone(),
        });
    }
    let launch_ready = !items
        .iter()
        .any(|item| item.severity == SetupReadinessSeverity::Blocking);
    SetupReadinessReport {
        items,
        launch_ready,
    }
}

/// Final setup plan review model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupPlanReview {
    /// Actions to show before apply/launch.
    pub actions: Vec<SetupPlanAction>,
    /// Whether launch is available without more required setup.
    pub launch_ready: bool,
}

impl SetupPlanReview {
    /// Return a launch/start-session action when setup is ready enough.
    #[must_use]
    pub fn launch_action(&self) -> Option<SetupPlanAction> {
        self.launch_ready.then(|| SetupPlanAction {
            section_id: SetupSectionId::Launch,
            kind: "launch".to_owned(),
            title: "Launch Bcode".to_owned(),
            body: "Your setup is ready enough to start using Bcode.".to_owned(),
            mutating: false,
        })
    }
}

/// Result of applying a setup plan through settings/domain services.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedSetupPlan {
    /// Applied actions.
    pub applied_actions: Vec<SetupPlanAction>,
    /// Skipped non-mutating actions such as launch prompts.
    pub skipped_actions: Vec<SetupPlanAction>,
}

/// Onboarding/settings shell mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsExperienceMode {
    /// First-run onboarding should be shown.
    FirstRunOnboarding,
    /// The settings/control-center experience should be shown.
    ControlCenter,
}

/// Determine whether to show first-run onboarding or return to Control Center.
#[must_use]
pub fn settings_experience_mode(progress: Option<&OnboardingProgress>) -> SettingsExperienceMode {
    if progress.is_some_and(|progress| progress.first_run_completed) {
        SettingsExperienceMode::ControlCenter
    } else {
        SettingsExperienceMode::FirstRunOnboarding
    }
}

/// Generate a conservative setup plan from the reconciled setup state and recommendations.
#[must_use]
pub fn generate_setup_plan(
    sections: &[ReconciledSetupSection],
    recommendations: &[SetupRecommendation],
) -> SetupPlanReview {
    let blocked = sections
        .iter()
        .any(|section| section.status == SetupSectionStatus::Blocked);
    let readiness = setup_readiness_report(sections, recommendations);
    let mut actions = recommendations
        .iter()
        .filter(|recommendation| recommendation.dismissed_at_ms.is_none())
        .map(|recommendation| SetupPlanAction {
            section_id: setup_section_id_from_str(&recommendation.section_id)
                .unwrap_or(SetupSectionId::Welcome),
            kind: recommendation.kind.clone(),
            title: recommendation.title.clone(),
            body: recommendation.body.clone(),
            mutating: true,
        })
        .collect::<Vec<_>>();
    if !blocked && actions.is_empty() {
        actions.push(SetupPlanAction {
            section_id: SetupSectionId::Launch,
            kind: "launch".to_owned(),
            title: "Launch Bcode".to_owned(),
            body: "Your setup is ready enough to start using Bcode.".to_owned(),
            mutating: false,
        });
    }
    SetupPlanReview {
        actions,
        launch_ready: !blocked && readiness.launch_ready,
    }
}

fn setup_section_id_from_str(value: &str) -> Option<SetupSectionId> {
    SetupSectionId::all()
        .into_iter()
        .find(|section| section.as_str() == value)
}

/// Bounded, sanitized onboarding/environment detection result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupDetectionSnapshot {
    /// Sanitized detection entries suitable for settings DB persistence.
    pub entries: Vec<DetectionCacheEntry>,
    /// Recommended setup actions derived from detection.
    pub recommendations: Vec<SetupRecommendation>,
}

/// Build a sanitized detection snapshot from environment variables.
#[must_use]
pub fn detect_setup_environment_from_vars(
    vars: &BTreeMap<String, String>,
    detected_at_ms: u64,
) -> SetupDetectionSnapshot {
    let secret_names = [
        "OPENAI_API_KEY",
        "XAI_API_KEY",
        "OPENROUTER_API_KEY",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "GITHUB_TOKEN",
    ];
    let metadata_names = ["AWS_PROFILE", "AWS_REGION", "AWS_DEFAULT_REGION"];

    let mut entries = Vec::new();
    let mut recommendations = Vec::new();

    for name in secret_names {
        if vars.get(name).is_some_and(|value| !value.is_empty()) {
            entries.push(DetectionCacheEntry {
                detector: "environment".to_owned(),
                key: name.to_owned(),
                value: json!({ "present": true, "secret_value_stored": false }),
                confidence: "high".to_owned(),
                source: "environment".to_owned(),
                detected_at_ms,
                expires_at_ms: None,
            });
            recommendations.push(SetupRecommendation {
                recommendation_id: format!("secure-env-{}", name.to_ascii_lowercase()),
                section_id: SetupSectionId::SecureVault.as_str().to_owned(),
                kind: "secure_env_secret".to_owned(),
                status: SetupSectionStatus::Recommended.as_str().to_owned(),
                priority: 100,
                title: format!("Secure detected {name}"),
                body: "Move this detected environment credential into secure storage.".to_owned(),
                created_at_ms: detected_at_ms,
                dismissed_at_ms: None,
            });
        }
    }

    for name in ["SSHENV_PROFILE", "SSHENV_VAULT", "SSHENV_DEVICE_SEAL"] {
        if vars.get(name).is_some_and(|value| !value.is_empty()) {
            entries.push(DetectionCacheEntry {
                detector: "environment".to_owned(),
                key: name.to_owned(),
                value: json!({ "present": true, "value_stored": false }),
                confidence: "medium".to_owned(),
                source: "environment".to_owned(),
                detected_at_ms,
                expires_at_ms: None,
            });
        }
    }

    for name in metadata_names {
        if let Some(value) = vars.get(name).filter(|value| !value.is_empty()) {
            entries.push(DetectionCacheEntry {
                detector: "environment".to_owned(),
                key: name.to_owned(),
                value: json!({ "present": true, "value": value }),
                confidence: "high".to_owned(),
                source: "environment".to_owned(),
                detected_at_ms,
                expires_at_ms: None,
            });
        }
    }

    // Detect Bcode-prefixed environment metadata without storing secret-like values.
    for (name, value) in vars
        .iter()
        .filter(|(name, value)| name.starts_with("BCODE_") && !value.is_empty())
    {
        let looks_secret = name.contains("KEY")
            || name.contains("TOKEN")
            || name.contains("SECRET")
            || name.contains("PASSWORD");
        entries.push(DetectionCacheEntry {
            detector: "environment".to_owned(),
            key: name.clone(),
            value: if looks_secret {
                json!({ "present": true, "secret_value_stored": false })
            } else {
                json!({ "present": true, "value": value })
            },
            confidence: "high".to_owned(),
            source: "environment".to_owned(),
            detected_at_ms,
            expires_at_ms: None,
        });
    }

    SetupDetectionSnapshot {
        entries,
        recommendations,
    }
}

/// Detect setup-relevant environment state from the current process environment.
#[must_use]
pub fn detect_setup_environment(detected_at_ms: u64) -> SetupDetectionSnapshot {
    detect_setup_environment_from_vars(&std::env::vars().collect(), detected_at_ms)
}

/// Setup-map view model for the first TUI vertical slice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupMapSnapshot {
    /// Reconciled setup sections.
    pub sections: Vec<ReconciledSetupSection>,
}

impl SetupMapSnapshot {
    /// Build a setup-map snapshot from persisted and real state.
    #[must_use]
    pub fn from_reconciliation(
        persisted_sections: &[OnboardingSection],
        input: &SetupReconciliationInput,
    ) -> Self {
        Self {
            sections: reconcile_setup_sections(persisted_sections, input),
        }
    }

    /// Render a compact text setup map for smoke tests and early TUI wiring.
    #[must_use]
    pub fn render_text_map(&self) -> String {
        self.sections
            .iter()
            .map(|section| {
                format!(
                    "{}:{}{}",
                    section.section_id.as_str(),
                    section.status.as_str(),
                    if section.visited { ":visited" } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join(" -> ")
    }
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
    /// A TOML-backed config/state operation failed.
    #[error("settings config operation failed: {0}")]
    Config(#[from] bcode_config::ConfigError),
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

fn remove_file_if_exists(path: &Path) -> SettingsResult<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn sqlite_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}-{suffix}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn health_reports_available_and_reset_removes_database() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let db_path = temp.path().join("settings.db");
        let store = SettingsStore::from_settings_db_path(&db_path);

        assert_eq!(store.health(), SettingsDbHealth::Available);
        assert!(db_path.exists());
        store
            .reset_database()
            .expect("database reset should succeed");
        assert!(!db_path.exists());
    }

    #[test]
    fn health_reports_unavailable_for_directory_database_path() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let store = SettingsStore::from_settings_db_path(temp.path());

        assert!(matches!(
            store.health(),
            SettingsDbHealth::Unavailable { .. }
        ));
    }

    #[test]
    fn control_state_redacts_secret_like_json_keys() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let store = SettingsStore::from_settings_db_path(temp.path().join("settings.db"));

        store
            .put_control_state(
                "setup.secret-test",
                &json!({
                    "api_key": "sk-secret-value",
                    "nested": { "access_token": "token-secret" },
                    "safe": "visible",
                }),
                5,
            )
            .expect("state should write");
        let value = store
            .control_state("setup.secret-test")
            .expect("state should load")
            .expect("state should exist");
        let encoded = serde_json::to_string(&value).expect("state should encode");

        assert!(encoded.contains("visible"));
        assert!(!encoded.contains("sk-secret-value"));
        assert!(!encoded.contains("token-secret"));
    }

    #[test]
    fn secret_audit_flags_obvious_secret_markers() {
        let safe = audit_no_secret_material("safe", "OPENAI_API_KEY present but value hidden");
        let unsafe_report = audit_no_secret_material("unsafe", "api_key=sk-secret-value");

        assert!(safe.safe);
        assert!(!unsafe_report.safe);
        assert!(!unsafe_report.findings.is_empty());
    }

    #[test]
    fn degraded_panel_explains_health_state() {
        let panel = settings_degraded_panel(&SettingsDbHealth::Unavailable {
            message: "cannot open".to_owned(),
        });

        assert!(!panel.available);
        assert!(panel.reset_available);
        assert!(panel.message.contains("degraded"));
    }

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
    fn config_summary_maps_config_to_reconciliation_input() {
        let mut config = bcode_config::BcodeConfig::default();
        config.model.profile = Some("fast".to_owned());
        config.model.profiles.insert(
            "fast".to_owned(),
            bcode_config::ModelProfileConfig {
                provider_plugin_id: "bcode.openai-compatible".to_owned(),
                model_id: Some("gpt-test".to_owned()),
                ..bcode_config::ModelProfileConfig::default()
            },
        );
        config.plugins.enabled.insert("bcode.filesystem".to_owned());

        let summary = SetupConfigSummary::from_config(&config);
        let input = summary.reconciliation_input();

        assert!(
            summary
                .capabilities
                .contains(&SetupConfigCapability::ProviderSelection)
        );
        assert!(
            summary
                .capabilities
                .contains(&SetupConfigCapability::ModelSelection)
        );
        assert!(
            input
                .configured_sections
                .contains(&SetupSectionId::Providers)
        );
        assert!(input.configured_sections.contains(&SetupSectionId::Models));
        assert!(input.configured_sections.contains(&SetupSectionId::Plugins));
    }

    #[test]
    fn auto_start_onboarding_only_for_interactive_incomplete_normal_tui() {
        let summary = SetupConfigSummary::default();
        let decision =
            should_auto_start_onboarding(OnboardingStartupCommand::NormalTui, true, None, &summary);

        assert!(decision.should_start);
        assert!(
            !should_auto_start_onboarding(OnboardingStartupCommand::Server, true, None, &summary,)
                .should_start
        );
        assert!(
            !should_auto_start_onboarding(
                OnboardingStartupCommand::NormalTui,
                false,
                None,
                &summary,
            )
            .should_start
        );
    }

    #[test]
    fn auto_start_onboarding_skips_completed_or_usable_setup() {
        let progress = OnboardingProgress {
            mode: "first_run".to_owned(),
            first_run_completed: true,
            last_section: None,
            last_opened_at_ms: None,
            completed_at_ms: Some(1),
        };
        let completed_decision = should_auto_start_onboarding(
            OnboardingStartupCommand::NormalTui,
            true,
            Some(&progress),
            &SetupConfigSummary::default(),
        );
        let usable_summary = SetupConfigSummary {
            capabilities: BTreeSet::from([
                SetupConfigCapability::ProviderSelection,
                SetupConfigCapability::ModelSelection,
                SetupConfigCapability::AuthConfiguration,
            ]),
            enabled_plugins: BTreeSet::new(),
            disabled_plugins: BTreeSet::new(),
        };
        let usable_decision = should_auto_start_onboarding(
            OnboardingStartupCommand::NormalTui,
            true,
            None,
            &usable_summary,
        );

        assert!(!completed_decision.should_start);
        assert!(!usable_decision.should_start);
    }

    #[test]
    fn auth_security_detection_finds_sshenv_and_device_seal_config() {
        let mut config = bcode_config::BcodeConfig::default();
        config.auth.profiles.insert(
            "openai".to_owned(),
            bcode_config::AuthProfileConfig {
                backend: "sshenv".to_owned(),
                settings: BTreeMap::from([("device_seal".to_owned(), "required".to_owned())]),
                ..bcode_config::AuthProfileConfig::default()
            },
        );

        let detection = detect_auth_security_from_config(&config);

        assert!(detection.sshenv_available);
        assert!(detection.device_seal_requested);
        assert!(detection.sshenv_profiles.contains("openai"));
    }

    #[test]
    fn setup_plan_from_draft_applies_and_reconciles_state() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let store = SettingsStore::from_settings_db_path(temp.path().join("settings.db"));
        let draft = OnboardingDraftSetup {
            providers: BTreeSet::from(["openai-compatible".to_owned()]),
            auth_profiles: BTreeSet::from(["default".to_owned()]),
            model_profile: Some("default".to_owned()),
            permission_preset: Some("balanced".to_owned()),
            session_import_reviewed: true,
            plugins_reviewed: true,
        };
        let config = bcode_config::BcodeConfig::default();
        let plan =
            generate_setup_plan_from_draft(&draft, &[], &SetupConfigSummary::from_config(&config));
        let applied = store
            .apply_setup_plan(&plan, 900)
            .expect("plan should apply");
        let reconciliation = store
            .reconcile_setup_apply(&config)
            .expect("reconciliation should load");

        assert!(plan.launch_ready);
        assert_eq!(applied.applied_actions.len(), plan.actions.len());
        assert!(!reconciliation.draft_present);
        assert!(
            store
                .control_state("setup.last_applied_plan")
                .expect("control state should load")
                .is_some()
        );
    }

    #[test]
    fn onboarding_draft_setup_persists_provider_auth_model_and_permissions() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let store = SettingsStore::from_settings_db_path(temp.path().join("settings.db"));

        let draft = store
            .toggle_draft_provider("openai-compatible", 10)
            .expect("provider should toggle");
        assert!(draft.providers.contains("openai-compatible"));
        let draft = store
            .toggle_draft_auth_profile("default", 11)
            .expect("auth profile should toggle");
        assert!(draft.auth_profiles.contains("default"));
        let draft = store
            .select_draft_model_profile("default", 12)
            .expect("model profile should persist");
        assert_eq!(draft.model_profile.as_deref(), Some("default"));
        let draft = store
            .cycle_draft_permission_preset(13)
            .expect("permission preset should cycle");
        assert_eq!(draft.permission_preset.as_deref(), Some("cautious"));

        let reloaded = store.onboarding_draft_setup().expect("draft should reload");
        assert_eq!(reloaded, draft);
    }

    #[test]
    fn secure_credential_story_panel_never_contains_secret_values() {
        let entries = vec![DetectionCacheEntry {
            detector: "environment".to_owned(),
            key: "OPENAI_API_KEY".to_owned(),
            value: json!({ "present": true, "secret_value_stored": false }),
            confidence: "high".to_owned(),
            source: "environment".to_owned(),
            detected_at_ms: 70,
            expires_at_ms: None,
        }];
        let plans = secure_import_plans_from_detection(&entries);
        let panel = secure_credential_story_panel(
            &plans,
            &AuthSecurityDetection {
                sshenv_available: true,
                device_seal_requested: true,
                sshenv_profiles: BTreeSet::from(["openai".to_owned()]),
            },
        );
        let encoded = serde_json::to_string(&panel).expect("panel should encode");

        assert!(encoded.contains("OPENAI_API_KEY"));
        assert!(encoded.contains("sshenv"));
        assert!(encoded.contains("Device sealing"));
        assert!(!encoded.contains("sk-secret-value"));
    }

    #[test]
    fn secure_import_plans_never_include_secret_values_and_reconcile_post_import() {
        let vars = BTreeMap::from([("OPENAI_API_KEY".to_owned(), "sk-secret-value".to_owned())]);
        let snapshot = detect_setup_environment_from_vars(&vars, 950);
        let plans = secure_import_plans_from_detection(&snapshot.entries);
        let encoded = serde_json::to_string(&plans).expect("plans should encode");
        let auth_detection = AuthSecurityDetection {
            sshenv_available: true,
            device_seal_requested: true,
            sshenv_profiles: BTreeSet::from(["openai".to_owned()]),
        };
        let reconciliation = reconcile_secure_import_plans(&plans, &auth_detection);

        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].env_var, "OPENAI_API_KEY");
        assert!(!encoded.contains("sk-secret-value"));
        assert!(reconciliation.imported_profiles.contains("openai"));
        assert!(reconciliation.pending_profiles.is_empty());
    }

    #[test]
    fn section_visit_and_completion_persist_resume_state() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let store = SettingsStore::from_settings_db_path(temp.path().join("settings.db"));

        store
            .visit_onboarding_section(SetupSectionId::Providers, 700)
            .expect("visit should persist");
        let progress = store
            .onboarding_progress()
            .expect("progress should load")
            .expect("progress should exist");
        assert_eq!(progress.last_section.as_deref(), Some("providers"));
        assert!(!progress.first_run_completed);
        assert_eq!(
            store
                .onboarding_sections()
                .expect("sections should load")
                .first()
                .expect("section should exist")
                .section_id,
            "providers"
        );

        store
            .complete_onboarding(701)
            .expect("completion should persist");
        let progress = store
            .onboarding_progress()
            .expect("progress should reload")
            .expect("progress should still exist");
        assert!(progress.first_run_completed);
        assert_eq!(progress.completed_at_ms, Some(701));
    }

    #[test]
    fn section_completion_skip_and_recommendation_dismissal_persist() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let store = SettingsStore::from_settings_db_path(temp.path().join("settings.db"));
        let recommendation = SetupRecommendation {
            recommendation_id: "review-imports".to_owned(),
            section_id: SetupSectionId::Imports.as_str().to_owned(),
            kind: "review_optional_section".to_owned(),
            status: SetupSectionStatus::Recommended.as_str().to_owned(),
            priority: 10,
            title: "Review imports".to_owned(),
            body: "Review optional import sources.".to_owned(),
            created_at_ms: 900,
            dismissed_at_ms: None,
        };

        store
            .complete_onboarding_section(SetupSectionId::Models, 901)
            .expect("section completion should persist");
        store
            .skip_onboarding_section(SetupSectionId::Imports, 902)
            .expect("section skip should persist");
        store
            .save_setup_recommendation(&recommendation)
            .expect("recommendation should persist");
        let dismissed = store
            .dismiss_setup_recommendation("review-imports", 903)
            .expect("dismissal should persist")
            .expect("recommendation should exist");

        let sections = store.onboarding_sections().expect("sections should load");
        assert!(sections.iter().any(|section| {
            section.section_id == "models"
                && section.status == SetupSectionStatus::Complete.as_str()
        }));
        assert!(sections.iter().any(|section| {
            section.section_id == "imports"
                && section.status == SetupSectionStatus::Skipped.as_str()
        }));
        assert_eq!(dismissed.dismissed_at_ms, Some(903));
    }

    #[test]
    fn setup_plan_apply_records_mutating_actions_and_skips_launch() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let store = SettingsStore::from_settings_db_path(temp.path().join("settings.db"));
        let plan = SetupPlanReview {
            actions: vec![
                SetupPlanAction {
                    section_id: SetupSectionId::SecureVault,
                    kind: "secure_env_secret".to_owned(),
                    title: "Secure detected key".to_owned(),
                    body: "Move it to secure storage.".to_owned(),
                    mutating: true,
                },
                SetupPlanAction {
                    section_id: SetupSectionId::Launch,
                    kind: "launch".to_owned(),
                    title: "Launch Bcode".to_owned(),
                    body: "Start using Bcode.".to_owned(),
                    mutating: false,
                },
            ],
            launch_ready: true,
        };

        let applied = store
            .apply_setup_plan(&plan, 1_000)
            .expect("plan should apply");
        let state = store
            .control_state("setup.applied.secure_env_secret")
            .expect("control state should load")
            .expect("control state should exist");

        assert_eq!(applied.applied_actions.len(), 1);
        assert_eq!(applied.skipped_actions.len(), 1);
        assert_eq!(state.value["section_id"], "secure_vault");
    }

    #[test]
    fn settings_experience_mode_switches_after_completion() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let store = SettingsStore::from_settings_db_path(temp.path().join("settings.db"));

        assert_eq!(
            store.experience_mode().expect("mode should load"),
            SettingsExperienceMode::FirstRunOnboarding
        );
        store
            .complete_onboarding(1_001)
            .expect("onboarding should complete");
        assert_eq!(
            store.experience_mode().expect("mode should reload"),
            SettingsExperienceMode::ControlCenter
        );
    }

    #[test]
    fn readiness_report_identifies_blocking_and_recommended_items() {
        let sections = vec![
            ReconciledSetupSection {
                section_id: SetupSectionId::SecureVault,
                status: SetupSectionStatus::Blocked,
                visited: true,
            },
            ReconciledSetupSection {
                section_id: SetupSectionId::Plugins,
                status: SetupSectionStatus::Recommended,
                visited: false,
            },
        ];
        let recommendation = SetupRecommendation {
            recommendation_id: "secure-openai-env".to_owned(),
            section_id: SetupSectionId::SecureVault.as_str().to_owned(),
            kind: "secure_env_secret".to_owned(),
            status: SetupSectionStatus::Recommended.as_str().to_owned(),
            priority: 100,
            title: "Secure detected OpenAI key".to_owned(),
            body: "Move this detected key into secure storage.".to_owned(),
            created_at_ms: 1,
            dismissed_at_ms: None,
        };

        let report = setup_readiness_report(&sections, &[recommendation]);

        assert!(!report.launch_ready);
        assert!(
            report
                .items
                .iter()
                .any(|item| item.severity == SetupReadinessSeverity::Blocking)
        );
        assert!(
            report
                .items
                .iter()
                .any(|item| item.severity == SetupReadinessSeverity::Recommended)
        );
    }

    #[test]
    fn readiness_report_round_trips_through_control_state() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let store = SettingsStore::from_settings_db_path(temp.path().join("settings.db"));
        let report = SetupReadinessReport {
            items: vec![SetupReadinessItem {
                section_id: SetupSectionId::Plugins,
                severity: SetupReadinessSeverity::Recommended,
                title: "Review plugins".to_owned(),
                body: "Plugin settings are customizable.".to_owned(),
            }],
            launch_ready: true,
        };

        store
            .save_readiness_report(&report, 55)
            .expect("report should save");

        assert_eq!(
            store
                .readiness_report()
                .expect("report should load")
                .expect("report should exist"),
            report
        );
    }

    #[test]
    fn setup_plan_review_exposes_launch_action() {
        let plan = SetupPlanReview {
            actions: Vec::new(),
            launch_ready: true,
        };

        let action = plan.launch_action().expect("launch action should exist");

        assert_eq!(action.kind, "launch");
        assert!(!action.mutating);
    }

    #[test]
    fn setup_plan_review_uses_recommendations_and_launch_state() {
        let blocked_sections = vec![ReconciledSetupSection {
            section_id: SetupSectionId::SecureVault,
            status: SetupSectionStatus::Blocked,
            visited: true,
        }];
        let recommendation = SetupRecommendation {
            recommendation_id: "secure-openai-env".to_owned(),
            section_id: SetupSectionId::SecureVault.as_str().to_owned(),
            kind: "secure_env_secret".to_owned(),
            status: SetupSectionStatus::Recommended.as_str().to_owned(),
            priority: 100,
            title: "Secure detected OpenAI key".to_owned(),
            body: "Move this detected key into secure storage.".to_owned(),
            created_at_ms: 800,
            dismissed_at_ms: None,
        };

        let blocked_plan = generate_setup_plan(&blocked_sections, &[recommendation]);
        assert!(!blocked_plan.launch_ready);
        assert_eq!(blocked_plan.actions[0].kind, "secure_env_secret");

        let launch_plan = generate_setup_plan(&[], &[]);
        assert!(launch_plan.launch_ready);
        assert_eq!(launch_plan.actions[0].kind, "launch");
    }

    #[test]
    fn setup_view_models_include_story_copy() {
        let provider = provider_setup_card("bcode.openai-compatible", true, false);
        let detection = AuthSecurityDetection {
            sshenv_available: true,
            device_seal_requested: true,
            sshenv_profiles: BTreeSet::from(["openai".to_owned()]),
        };
        let security =
            security_trust_panel(detection.sshenv_available, detection.device_seal_requested);
        let model = model_setup_card(Some("fast".to_owned()), None);
        let permission = permission_setup_card(false);
        let import = import_setup_card(false);
        let plugin = plugin_setup_card(BTreeSet::new(), BTreeSet::new());

        assert!(provider.story.contains("secure auth path"));
        assert!(security.story.contains("device sealing"));
        assert!(model.story.contains("model path"));
        assert!(permission.story.contains("permission preset"));
        assert!(import.story.contains("Session import is optional"));
        assert!(plugin.story.contains("Bundled plugins"));
    }

    #[test]
    fn environment_detection_sanitizes_secret_values() {
        let vars = BTreeMap::from([
            ("OPENAI_API_KEY".to_owned(), "sk-secret-value".to_owned()),
            ("AWS_PROFILE".to_owned(), "work".to_owned()),
            ("BCODE_AUTH_TOKEN".to_owned(), "bcode-secret".to_owned()),
            ("BCODE_MODEL_PROFILE".to_owned(), "fast".to_owned()),
            ("SSHENV_VAULT".to_owned(), "/tmp/secret/path".to_owned()),
        ]);

        let snapshot = detect_setup_environment_from_vars(&vars, 500);
        let encoded = serde_json::to_string(&snapshot).expect("snapshot should encode");

        assert!(encoded.contains("OPENAI_API_KEY"));
        assert!(encoded.contains("work"));
        assert!(!encoded.contains("sk-secret-value"));
        assert!(!encoded.contains("bcode-secret"));
        assert!(!encoded.contains("/tmp/secret/path"));
        assert!(encoded.contains("fast"));
        assert_eq!(snapshot.recommendations.len(), 1);
    }

    #[test]
    fn setup_map_snapshot_renders_compact_text_map() {
        let persisted = vec![OnboardingSection {
            section_id: SetupSectionId::Welcome.as_str().to_owned(),
            status: SetupSectionStatus::Visited.as_str().to_owned(),
            visited: true,
            visited_at_ms: Some(1),
            completed_at_ms: None,
            skipped_at_ms: None,
            dismissed: false,
        }];
        let input = SetupReconciliationInput {
            current_section: Some(SetupSectionId::Welcome),
            ..SetupReconciliationInput::default()
        };

        let snapshot = SetupMapSnapshot::from_reconciliation(&persisted, &input);
        let rendered = snapshot.render_text_map();

        assert!(rendered.starts_with("welcome:current:visited -> detection:unvisited"));
        assert!(rendered.ends_with("launch:unvisited"));
    }

    #[test]
    fn setup_detection_snapshot_persists_entries_and_recommendations() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let store = SettingsStore::from_settings_db_path(temp.path().join("settings.db"));
        let vars = BTreeMap::from([(
            "OPENROUTER_API_KEY".to_owned(),
            "secret-openrouter-value".to_owned(),
        )]);
        let snapshot = detect_setup_environment_from_vars(&vars, 600);

        store
            .save_setup_detection_snapshot(&snapshot)
            .expect("snapshot should persist");
        let persisted_entries = store
            .detection_cache_entries()
            .expect("entries should load");
        let persisted_recommendations = store
            .setup_recommendations()
            .expect("recommendations should load");
        let encoded_entries = serde_json::to_string(&persisted_entries).expect("entries encode");

        assert_eq!(persisted_entries.len(), 1);
        assert_eq!(persisted_recommendations.len(), 1);
        assert!(!encoded_entries.contains("secret-openrouter-value"));
    }

    #[test]
    fn settings_service_model_ignore_writes_toml_state() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let state_path = temp.path().join("model-ignores.toml");
        unsafe {
            std::env::set_var("BCODE_MODEL_IGNORES_STATE", &state_path);
        }
        let store = SettingsStore::from_settings_db_path(temp.path().join("settings.db"));

        store
            .ignore_model("bcode.openai-compatible", "gpt-hidden".to_owned())
            .expect("model ignore should be written");
        let rules = bcode_config::load_model_ignores_state_from(&state_path)
            .expect("model ignore state should load");
        assert!(
            rules
                .get("bcode.openai-compatible")
                .expect("provider ignore entry should exist")
                .models
                .contains("gpt-hidden")
        );

        store
            .unignore_model("bcode.openai-compatible", "gpt-hidden")
            .expect("model ignore should be removed");
        let rules = bcode_config::load_model_ignores_state_from(&state_path)
            .expect("model ignore state should reload");
        assert!(
            !rules
                .get("bcode.openai-compatible")
                .expect("provider ignore entry should still exist")
                .models
                .contains("gpt-hidden")
        );
        unsafe {
            std::env::remove_var("BCODE_MODEL_IGNORES_STATE");
        }
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
