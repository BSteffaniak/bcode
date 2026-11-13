//! The version log: every version of every pull request a review saw or was told about (see
//! `docs/review-stack-history.md`). Per user, in the review store: history cannot be recomputed
//! once a force push drops a version's commits from its branch.

use super::db::{CodeReviewDb, int_column, nullable_text_column, text_column};
use super::{CodeReviewConfig, StoreError, resolve_repo_root, with_database};
use bcode_code_review_models::{
    LayerVersion, ListLayerVersionsRequest, ListLayerVersionsResponse, RecordLayerVersionsRequest,
    RecordLayerVersionsResponse,
};
use switchy::database::query::{FilterableQuery as _, where_eq, where_in};
use switchy::database::schema::create_table;
use switchy::database::{DatabaseValue, Row};
use switchy::schema::discovery::code::CodeMigration;

const TABLE: &str = "layer_versions";

/// The table of the version log.
pub fn layer_versions_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "023_layer_versions_table".to_string(),
        Box::new(
            create_table(TABLE)
                .if_not_exists(true)
                .column(text_column("version_id"))
                .column(text_column("provider"))
                .column(text_column("repository"))
                .column(int_column("number"))
                .column(text_column("head_oid"))
                .column(nullable_text_column("base_oid"))
                .column(nullable_text_column("base_branch"))
                .column(int_column("seen_at_ms"))
                .column(nullable_text_column("actor"))
                .column(text_column("sources_json"))
                .primary_key("version_id"),
        ),
        None,
    )
}

/// A version's row id: its pull request and head.
fn version_id(version: &LayerVersion) -> String {
    format!(
        "{}\u{1}{}\u{1}{}\u{1}{}",
        version.provider, version.repository, version.number, version.head_oid
    )
}

/// `known` with what `new` adds: its sources, what it knows that `known` does not, and the
/// earlier time.
fn merged(mut known: LayerVersion, new: &LayerVersion) -> LayerVersion {
    known.sources.extend(new.sources.iter().copied());
    if known.base_oid.is_none() {
        known.base_oid.clone_from(&new.base_oid);
    }
    if known.base_branch.is_none() {
        known.base_branch.clone_from(&new.base_branch);
    }
    if known.actor.is_none() {
        known.actor.clone_from(&new.actor);
    }
    known.seen_at_ms = known.seen_at_ms.min(new.seen_at_ms);
    known
}

fn version_from_row(row: &Row) -> Option<LayerVersion> {
    let text = |column: &str| {
        row.get(column)
            .and_then(|value| value.as_str().map(str::to_owned))
    };
    let int = |column: &str| row.get(column).and_then(|value| value.as_i64());
    Some(LayerVersion {
        provider: text("provider")?,
        repository: text("repository")?,
        number: u64::try_from(int("number")?).ok()?,
        head_oid: text("head_oid")?,
        base_oid: text("base_oid"),
        base_branch: text("base_branch"),
        seen_at_ms: u64::try_from(int("seen_at_ms")?).unwrap_or(0),
        actor: text("actor"),
        sources: text("sources_json")
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default(),
    })
}

fn optional(value: Option<&String>) -> DatabaseValue {
    value.map_or(DatabaseValue::Null, |value| {
        DatabaseValue::String(value.clone())
    })
}

impl CodeReviewDb<'_> {
    /// Record `versions`, merging each into what is known of it. Returns how many were new.
    pub async fn record_versions(&self, versions: &[LayerVersion]) -> Result<usize, StoreError> {
        let mut added = 0;
        for version in versions {
            let id = version_id(version);
            let known = self
                .db()
                .select(TABLE)
                .filter(Box::new(where_eq("version_id", id.clone())))
                .execute_first(self.db())
                .await?
                .as_ref()
                .and_then(version_from_row);
            let version = if let Some(known) = known {
                let merged = merged(known.clone(), version);
                if merged == known {
                    continue;
                }
                merged
            } else {
                added += 1;
                version.clone()
            };
            self.db()
                .upsert(TABLE)
                .unique(&["version_id"])
                .value("version_id", id)
                .value("provider", version.provider.clone())
                .value("repository", version.repository.clone())
                .value("number", i64::try_from(version.number).unwrap_or(i64::MAX))
                .value("head_oid", version.head_oid.clone())
                .value("base_oid", optional(version.base_oid.as_ref()))
                .value("base_branch", optional(version.base_branch.as_ref()))
                .value(
                    "seen_at_ms",
                    i64::try_from(version.seen_at_ms).unwrap_or(i64::MAX),
                )
                .value("actor", optional(version.actor.as_ref()))
                .value("sources_json", serde_json::to_string(&version.sources)?)
                .execute(self.db())
                .await?;
        }
        Ok(added)
    }

    /// The versions of pull requests `numbers` of `provider`'s `repository`, oldest first per
    /// pull request.
    pub async fn list_versions(
        &self,
        provider: &str,
        repository: &str,
        numbers: &[u64],
    ) -> Result<Vec<LayerVersion>, StoreError> {
        if numbers.is_empty() {
            return Ok(Vec::new());
        }
        let numbers: Vec<DatabaseValue> = numbers
            .iter()
            .map(|number| DatabaseValue::Int64(i64::try_from(*number).unwrap_or(i64::MAX)))
            .collect();
        let rows = self
            .db()
            .select(TABLE)
            .filter(Box::new(where_eq("provider", provider)))
            .filter(Box::new(where_eq("repository", repository)))
            .filter(Box::new(where_in("number", numbers)))
            .execute(self.db())
            .await?;
        let mut versions: Vec<LayerVersion> = rows.iter().filter_map(version_from_row).collect();
        versions.sort_by(|a, b| {
            (a.number, a.seen_at_ms, &a.head_oid).cmp(&(b.number, b.seen_at_ms, &b.head_oid))
        });
        Ok(versions)
    }
}

/// Record layer versions in the repository's store.
///
/// # Errors
///
/// * See [`super::ReviewStore`].
pub fn record_layer_versions_for_request(
    request: RecordLayerVersionsRequest,
    config: &CodeReviewConfig,
) -> Result<RecordLayerVersionsResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let versions = request.versions;
    let added = with_database(&repo_root, config, move |database| {
        Box::pin(async move { CodeReviewDb::new(database).record_versions(&versions).await })
    })?;
    Ok(RecordLayerVersionsResponse { added })
}

/// List layer versions from the repository's store.
///
/// # Errors
///
/// * See [`super::ReviewStore`].
pub fn list_layer_versions_for_request(
    request: &ListLayerVersionsRequest,
    config: &CodeReviewConfig,
) -> Result<ListLayerVersionsResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let (provider, repository, numbers) = (
        request.provider.clone(),
        request.repository.clone(),
        request.numbers.clone(),
    );
    let versions = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .list_versions(&provider, &repository, &numbers)
                .await
        })
    })?;
    Ok(ListLayerVersionsResponse { versions })
}
