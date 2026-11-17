//! Guided-review guides (see `docs/code-review-guide.md`): guides, their versions, fan-out
//! areas, ask threads, and reviewed-section marks, each in a table of its own, kept apart from
//! the review workspace so review saves and guide writes never overwrite each other.
//!
//! Guide, version, and area rows carry a revision every write increments. A write names the
//! revision it read and replaces the row only while it is still that revision; otherwise it
//! fails with [`StoreError::RevisionConflict`] naming the current revision.

use super::db::{
    CodeReviewDb, i64_to_u64, int_column, now_ms, nullable_int_column, nullable_text_column,
    optional_i64, optional_text, required_i64, required_text, review_key_for_scope, text_column,
    u64_to_i64,
};
use super::{
    CodeReviewConfig, MAX_WORKSPACE_WRITE_ATTEMPTS, StoreError, resolve_repo_root, with_database,
};
use crate::guide::document::{GuideDocumentError, check_area, check_thread, check_version};
use bcode_code_review_models::{
    ClearGuideSectionMarkRequest, ClearGuideSectionMarkResponse, CreateReviewGuideRequest,
    CreateReviewGuideResponse, DeleteGuideThreadRequest, DeleteGuideThreadResponse,
    DeleteReviewGuideRequest, DeleteReviewGuideResponse, ForkGuideVersionRequest,
    ForkGuideVersionResponse, GetGuideVersionRequest, GetGuideVersionResponse,
    GetReviewGuideRequest, GetReviewGuideResponse, GuideArea, GuideAreaRecord, GuideAreaStatus,
    GuideDocument, GuideRecord, GuideSectionMark, GuideStatus, GuideThread, GuideThreadStatus,
    GuideVersion, GuideVersionRecord, GuideVersionSummary, ListGuideAreasRequest,
    ListGuideAreasResponse, ListGuideSectionMarksRequest, ListGuideSectionMarksResponse,
    ListGuideThreadsRequest, ListGuideThreadsResponse, ListGuideVersionsRequest,
    ListGuideVersionsResponse, ListReviewGuidesRequest, ListReviewGuidesResponse,
    REVIEW_GUIDE_SCHEMA_VERSION, RekeyReviewGuidesRequest, RekeyReviewGuidesResponse, ReviewGuide,
    SaveGuideAreaRequest, SaveGuideAreaResponse, SaveGuideThreadRequest, SaveGuideThreadResponse,
    SaveGuideVersionRequest, SaveGuideVersionResponse, SetGuideSectionMarkRequest,
    SetGuideSectionMarkResponse, UpdateReviewGuideRequest, UpdateReviewGuideResponse,
};
use std::path::Path;
use switchy::database::query::{FilterableQuery as _, where_eq};
use switchy::database::schema::create_table;
use switchy::database::{DatabaseValue, Row};
use switchy::schema::discovery::code::CodeMigration;

const GUIDES: &str = "review_guides";
const VERSIONS: &str = "review_guide_versions";
const AREAS: &str = "review_guide_areas";
const THREADS: &str = "review_guide_threads";
const MARKS: &str = "review_guide_marks";

/// The table of guides.
pub fn review_guides_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "031_review_guides_table".to_string(),
        Box::new(
            create_table(GUIDES)
                .if_not_exists(true)
                .column(text_column("guide_id"))
                .column(text_column("review_key"))
                .column(text_column("scope_json"))
                .column(nullable_text_column("parent_guide_id"))
                .column(text_column("title"))
                .column(int_column("current_version"))
                .column(int_column("revision"))
                .column(int_column("created_at_ms"))
                .column(int_column("updated_at_ms"))
                .column(nullable_int_column("archived_at_ms"))
                .primary_key("guide_id"),
        ),
        None,
    )
}

/// The table of guide versions.
pub fn review_guide_versions_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "032_review_guide_versions_table".to_string(),
        Box::new(
            create_table(VERSIONS)
                .if_not_exists(true)
                .column(text_column("guide_id"))
                .column(int_column("version"))
                .column(int_column("schema_version"))
                .column(text_column("heads_json"))
                .column(nullable_int_column("based_on"))
                .column(text_column("origin_json"))
                .column(text_column("status"))
                .column(nullable_text_column("status_message"))
                .column(nullable_text_column("generation_json"))
                .column(text_column("doc_json"))
                .column(int_column("revision"))
                .column(int_column("created_at_ms"))
                .column(int_column("updated_at_ms"))
                .primary_key_columns(["guide_id", "version"]),
        ),
        None,
    )
}

/// The table of fan-out areas: each child writes only its own row.
pub fn review_guide_areas_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "033_review_guide_areas_table".to_string(),
        Box::new(
            create_table(AREAS)
                .if_not_exists(true)
                .column(text_column("guide_id"))
                .column(int_column("version"))
                .column(text_column("area_id"))
                .column(nullable_text_column("session_id"))
                .column(text_column("status"))
                .column(nullable_text_column("status_message"))
                .column(text_column("doc_json"))
                .column(int_column("revision"))
                .column(int_column("created_at_ms"))
                .column(int_column("updated_at_ms"))
                .primary_key_columns(["guide_id", "version", "area_id"]),
        ),
        None,
    )
}

/// The table of ask threads.
pub fn review_guide_threads_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "034_review_guide_threads_table".to_string(),
        Box::new(
            create_table(THREADS)
                .if_not_exists(true)
                .column(text_column("thread_id"))
                .column(text_column("guide_id"))
                .column(int_column("version"))
                .column(text_column("section_id"))
                .column(nullable_text_column("session_id"))
                .column(text_column("status"))
                .column(text_column("question"))
                .column(int_column("created_at_ms"))
                .column(int_column("updated_at_ms"))
                .primary_key("thread_id"),
        ),
        None,
    )
}

/// The table of reviewed-section marks.
pub fn review_guide_marks_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "035_review_guide_marks_table".to_string(),
        Box::new(
            create_table(MARKS)
                .if_not_exists(true)
                .column(text_column("guide_id"))
                .column(text_column("section_id"))
                .column(int_column("reviewed_at_ms"))
                .column(text_column("fingerprint"))
                .primary_key_columns(["guide_id", "section_id"]),
        ),
        None,
    )
}

fn optional_string(value: Option<&String>) -> DatabaseValue {
    value.map_or(DatabaseValue::Null, |value| {
        DatabaseValue::String(value.clone())
    })
}

fn optional_int(value: Option<u64>) -> DatabaseValue {
    value.map_or(DatabaseValue::Null, |value| {
        DatabaseValue::Int64(u64_to_i64(value))
    })
}

fn u32_of(row: &Row, column: &'static str) -> Result<u32, StoreError> {
    u32::try_from(required_i64(row, column)?).map_err(|_| {
        StoreError::InvalidRequest(format!("stored guide column {column} is out of range"))
    })
}

fn revision_of(row: &Row) -> u64 {
    i64_to_u64(optional_i64(row, "revision").unwrap_or(0))
}

fn guide_from_row(row: &Row) -> Result<GuideRecord, StoreError> {
    Ok(GuideRecord {
        guide: ReviewGuide {
            guide_id: required_text(row, "guide_id")?,
            review_key: required_text(row, "review_key")?,
            scope: serde_json::from_str(&required_text(row, "scope_json")?)?,
            parent_guide_id: optional_text(row, "parent_guide_id"),
            title: required_text(row, "title")?,
            current_version: u32_of(row, "current_version")?,
            created_at_ms: i64_to_u64(required_i64(row, "created_at_ms")?),
            updated_at_ms: i64_to_u64(required_i64(row, "updated_at_ms")?),
            archived_at_ms: optional_i64(row, "archived_at_ms").map(i64_to_u64),
        },
        revision: revision_of(row),
    })
}

fn schema_of(row: &Row) -> Result<u16, StoreError> {
    u16::try_from(required_i64(row, "schema_version")?).map_err(|_| {
        StoreError::InvalidRequest("stored review guide schema version is out of range".to_owned())
    })
}

fn version_from_row(row: &Row) -> Result<GuideVersionRecord, StoreError> {
    let schema_version = schema_of(row)?;
    if schema_version != REVIEW_GUIDE_SCHEMA_VERSION {
        return Err(GuideDocumentError::UnsupportedSchema {
            found: schema_version,
        }
        .into());
    }
    let status = required_text(row, "status")?;
    let status = GuideStatus::parse(&status, optional_text(row, "status_message"))
        .ok_or(GuideDocumentError::UnknownStatus(status))?;
    let document: GuideDocument = serde_json::from_str(&required_text(row, "doc_json")?)?;
    Ok(GuideVersionRecord {
        version: GuideVersion {
            guide_id: required_text(row, "guide_id")?,
            version: u32_of(row, "version")?,
            schema_version,
            heads: serde_json::from_str(&required_text(row, "heads_json")?)?,
            based_on: optional_i64(row, "based_on")
                .map(|based_on| u32::try_from(based_on).unwrap_or(0)),
            origin: serde_json::from_str(&required_text(row, "origin_json")?)?,
            status,
            generation: optional_text(row, "generation_json")
                .map(|json| serde_json::from_str(&json))
                .transpose()?,
            outline: document.outline,
            citations: document.citations,
            retired_sections: document.retired_sections,
            created_at_ms: i64_to_u64(required_i64(row, "created_at_ms")?),
            updated_at_ms: i64_to_u64(required_i64(row, "updated_at_ms")?),
        },
        revision: revision_of(row),
    })
}

fn version_summary_from_row(row: &Row) -> Result<GuideVersionSummary, StoreError> {
    Ok(GuideVersionSummary {
        guide_id: required_text(row, "guide_id")?,
        version: u32_of(row, "version")?,
        schema_version: schema_of(row)?,
        status: GuideStatus::parse(
            &required_text(row, "status")?,
            optional_text(row, "status_message"),
        ),
        based_on: optional_i64(row, "based_on")
            .map(|based_on| u32::try_from(based_on).unwrap_or(0)),
        revision: revision_of(row),
        created_at_ms: i64_to_u64(required_i64(row, "created_at_ms")?),
        updated_at_ms: i64_to_u64(required_i64(row, "updated_at_ms")?),
    })
}

fn area_from_row(row: &Row) -> Result<GuideAreaRecord, StoreError> {
    let status = required_text(row, "status")?;
    let status =
        GuideAreaStatus::parse(&status).ok_or(GuideDocumentError::UnknownStatus(status))?;
    let document: GuideDocument = serde_json::from_str(&required_text(row, "doc_json")?)?;
    Ok(GuideAreaRecord {
        area: GuideArea {
            guide_id: required_text(row, "guide_id")?,
            version: u32_of(row, "version")?,
            area_id: required_text(row, "area_id")?,
            session_id: optional_text(row, "session_id"),
            status,
            message: optional_text(row, "status_message"),
            outline: document.outline,
            citations: document.citations,
            created_at_ms: i64_to_u64(required_i64(row, "created_at_ms")?),
            updated_at_ms: i64_to_u64(required_i64(row, "updated_at_ms")?),
        },
        revision: revision_of(row),
    })
}

fn thread_from_row(row: &Row) -> Result<GuideThread, StoreError> {
    let status = required_text(row, "status")?;
    let status =
        GuideThreadStatus::parse(&status).ok_or(GuideDocumentError::UnknownStatus(status))?;
    Ok(GuideThread {
        thread_id: required_text(row, "thread_id")?,
        guide_id: required_text(row, "guide_id")?,
        version: u32_of(row, "version")?,
        section_id: required_text(row, "section_id")?,
        session_id: optional_text(row, "session_id"),
        status,
        question: required_text(row, "question")?,
        created_at_ms: i64_to_u64(required_i64(row, "created_at_ms")?),
        updated_at_ms: i64_to_u64(required_i64(row, "updated_at_ms")?),
    })
}

fn mark_from_row(row: &Row) -> Result<GuideSectionMark, StoreError> {
    Ok(GuideSectionMark {
        guide_id: required_text(row, "guide_id")?,
        section_id: required_text(row, "section_id")?,
        reviewed_at_ms: i64_to_u64(required_i64(row, "reviewed_at_ms")?),
        fingerprint: required_text(row, "fingerprint")?,
    })
}

/// A version's JSON columns.
struct VersionColumns {
    heads: String,
    origin: String,
    generation: Option<String>,
    document: String,
}

impl VersionColumns {
    fn of(version: &GuideVersion) -> Result<Self, StoreError> {
        Ok(Self {
            heads: serde_json::to_string(&version.heads)?,
            origin: serde_json::to_string(&version.origin)?,
            generation: version
                .generation
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            document: serde_json::to_string(&version.document())?,
        })
    }
}

const fn conflict(record: String, current: Option<u64>) -> StoreError {
    StoreError::RevisionConflict { record, current }
}

impl CodeReviewDb<'_> {
    /// Create `guide` under `review_key` with its first version.
    pub async fn create_guide(
        &self,
        review_key: &str,
        guide: &ReviewGuide,
        version: &GuideVersion,
    ) -> Result<(GuideRecord, GuideVersionRecord), StoreError> {
        let now = now_ms();
        let mut guide = guide.clone();
        guide.review_key = review_key.to_owned();
        guide.current_version = 1;
        guide.created_at_ms = now;
        guide.updated_at_ms = now;
        let mut version = version.clone();
        version.guide_id.clone_from(&guide.guide_id);
        version.version = 1;
        version.created_at_ms = now;
        version.updated_at_ms = now;
        check_version(&version)?;
        if let Some(existing) = self.get_guide(&guide.guide_id).await? {
            return Err(conflict(
                format!("guide {}", guide.guide_id),
                Some(existing.revision),
            ));
        }
        self.db()
            .insert(GUIDES)
            .value("guide_id", guide.guide_id.clone())
            .value("review_key", guide.review_key.clone())
            .value("scope_json", serde_json::to_string(&guide.scope)?)
            .value(
                "parent_guide_id",
                optional_string(guide.parent_guide_id.as_ref()),
            )
            .value("title", guide.title.clone())
            .value("current_version", i64::from(guide.current_version))
            .value("revision", 0_i64)
            .value("created_at_ms", u64_to_i64(now))
            .value("updated_at_ms", u64_to_i64(now))
            .value("archived_at_ms", optional_int(guide.archived_at_ms))
            .execute(self.db())
            .await?;
        if let Err(error) = self.insert_version(&version).await {
            // No guide without its first version.
            self.delete_guide(&guide.guide_id).await?;
            return Err(error);
        }
        Ok((
            GuideRecord { guide, revision: 0 },
            GuideVersionRecord {
                version,
                revision: 0,
            },
        ))
    }

    /// The guide `guide_id`.
    pub async fn get_guide(&self, guide_id: &str) -> Result<Option<GuideRecord>, StoreError> {
        self.db()
            .select(GUIDES)
            .filter(Box::new(where_eq("guide_id", guide_id)))
            .execute_first(self.db())
            .await?
            .as_ref()
            .map(guide_from_row)
            .transpose()
    }

    /// The guides under `review_key`, most recently changed first.
    pub async fn list_guides(
        &self,
        review_key: &str,
        include_archived: bool,
    ) -> Result<Vec<GuideRecord>, StoreError> {
        let rows = self
            .db()
            .select(GUIDES)
            .filter(Box::new(where_eq("review_key", review_key)))
            .execute(self.db())
            .await?;
        let mut guides = rows
            .iter()
            .map(guide_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        guides.retain(|record| include_archived || record.guide.archived_at_ms.is_none());
        guides.sort_by(|a, b| {
            (b.guide.updated_at_ms, &a.guide.guide_id)
                .cmp(&(a.guide.updated_at_ms, &b.guide.guide_id))
        });
        Ok(guides)
    }

    /// Replace the row of `guide` (keeping its review key) when its revision is still
    /// `expected`. Returns the new revision.
    pub async fn update_guide_if(
        &self,
        guide: &ReviewGuide,
        expected: u64,
    ) -> Result<u64, StoreError> {
        let record = format!("guide {}", guide.guide_id);
        let updated = self
            .db()
            .update(GUIDES)
            .value("scope_json", serde_json::to_string(&guide.scope)?)
            .value(
                "parent_guide_id",
                optional_string(guide.parent_guide_id.as_ref()),
            )
            .value("title", guide.title.clone())
            .value("current_version", i64::from(guide.current_version))
            .value("updated_at_ms", u64_to_i64(now_ms()))
            .value("archived_at_ms", optional_int(guide.archived_at_ms))
            .value("revision", u64_to_i64(expected.wrapping_add(1)))
            .filter(Box::new(where_eq("guide_id", guide.guide_id.clone())))
            .filter(Box::new(where_eq("revision", u64_to_i64(expected))))
            .execute(self.db())
            .await?;
        if updated.is_empty() {
            let current = self.get_guide(&guide.guide_id).await?;
            return Err(conflict(record, current.map(|record| record.revision)));
        }
        Ok(expected.wrapping_add(1))
    }

    /// Delete the guide `guide_id` with its versions, areas, threads, and marks. Returns
    /// whether it existed.
    pub async fn delete_guide(&self, guide_id: &str) -> Result<bool, StoreError> {
        for table in [VERSIONS, AREAS, THREADS, MARKS] {
            self.db()
                .delete(table)
                .filter(Box::new(where_eq("guide_id", guide_id)))
                .execute(self.db())
                .await?;
        }
        let deleted = self
            .db()
            .delete(GUIDES)
            .filter(Box::new(where_eq("guide_id", guide_id)))
            .execute(self.db())
            .await?;
        Ok(!deleted.is_empty())
    }

    /// Move every guide under `from` to `to`. Returns how many moved.
    pub async fn rekey_guides(&self, from: &str, to: &str) -> Result<usize, StoreError> {
        let mut moved = 0;
        for record in self.list_guides(from, true).await? {
            let mut revision = record.revision;
            for _ in 0..MAX_WORKSPACE_WRITE_ATTEMPTS {
                let updated = self
                    .db()
                    .update(GUIDES)
                    .value("review_key", to.to_owned())
                    .value("revision", u64_to_i64(revision.wrapping_add(1)))
                    .filter(Box::new(where_eq(
                        "guide_id",
                        record.guide.guide_id.clone(),
                    )))
                    .filter(Box::new(where_eq("revision", u64_to_i64(revision))))
                    .execute(self.db())
                    .await?;
                if !updated.is_empty() {
                    moved += 1;
                    break;
                }
                match self.get_guide(&record.guide.guide_id).await? {
                    Some(current) if current.guide.review_key == from => {
                        revision = current.revision;
                    }
                    _ => break,
                }
            }
        }
        Ok(moved)
    }

    /// Version `version` of `guide_id`, or its current version.
    ///
    /// A version of another schema version fails with [`StoreError::Guide`] rather than being
    /// read as a known form.
    pub async fn get_guide_version(
        &self,
        guide_id: &str,
        version: Option<u32>,
    ) -> Result<Option<GuideVersionRecord>, StoreError> {
        let number = match version {
            Some(number) => number,
            None => match self.get_guide(guide_id).await? {
                Some(record) => record.guide.current_version,
                None => return Ok(None),
            },
        };
        self.db()
            .select(VERSIONS)
            .filter(Box::new(where_eq("guide_id", guide_id)))
            .filter(Box::new(where_eq("version", i64::from(number))))
            .execute_first(self.db())
            .await?
            .as_ref()
            .map(version_from_row)
            .transpose()
    }

    /// The versions of `guide_id`, oldest first, without their documents.
    pub async fn list_guide_versions(
        &self,
        guide_id: &str,
    ) -> Result<Vec<GuideVersionSummary>, StoreError> {
        let rows = self
            .db()
            .select(VERSIONS)
            .columns(&[
                "guide_id",
                "version",
                "schema_version",
                "status",
                "status_message",
                "based_on",
                "revision",
                "created_at_ms",
                "updated_at_ms",
            ])
            .filter(Box::new(where_eq("guide_id", guide_id)))
            .execute(self.db())
            .await?;
        let mut versions = rows
            .iter()
            .map(version_summary_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        versions.sort_by_key(|version| version.version);
        Ok(versions)
    }

    async fn insert_version(&self, version: &GuideVersion) -> Result<(), StoreError> {
        let columns = VersionColumns::of(version)?;
        self.db()
            .insert(VERSIONS)
            .value("guide_id", version.guide_id.clone())
            .value("version", i64::from(version.version))
            .value("schema_version", i64::from(version.schema_version))
            .value("heads_json", columns.heads)
            .value("based_on", optional_int(version.based_on.map(u64::from)))
            .value("origin_json", columns.origin)
            .value("status", version.status.as_str())
            .value(
                "status_message",
                optional_string(version.status.message().map(str::to_owned).as_ref()),
            )
            .value("generation_json", columns.generation)
            .value("doc_json", columns.document)
            .value("revision", 0_i64)
            .value("created_at_ms", u64_to_i64(version.created_at_ms))
            .value("updated_at_ms", u64_to_i64(version.updated_at_ms))
            .execute(self.db())
            .await?;
        Ok(())
    }

    /// Replace `version` when its row's revision is still `expected`. Returns the new revision.
    ///
    /// The version is checked first: another schema version, a bound exceeded, or an
    /// inconsistent document fails with [`StoreError::Guide`].
    pub async fn save_guide_version_if(
        &self,
        version: &GuideVersion,
        expected: u64,
    ) -> Result<u64, StoreError> {
        check_version(version)?;
        let columns = VersionColumns::of(version)?;
        let updated = self
            .db()
            .update(VERSIONS)
            .value("heads_json", columns.heads)
            .value("based_on", optional_int(version.based_on.map(u64::from)))
            .value("origin_json", columns.origin)
            .value("status", version.status.as_str())
            .value(
                "status_message",
                optional_string(version.status.message().map(str::to_owned).as_ref()),
            )
            .value("generation_json", columns.generation)
            .value("doc_json", columns.document)
            .value("updated_at_ms", u64_to_i64(now_ms()))
            .value("revision", u64_to_i64(expected.wrapping_add(1)))
            .filter(Box::new(where_eq("guide_id", version.guide_id.clone())))
            .filter(Box::new(where_eq("version", i64::from(version.version))))
            .filter(Box::new(where_eq("revision", u64_to_i64(expected))))
            .execute(self.db())
            .await?;
        if updated.is_empty() {
            let current = self
                .get_version_revision(&version.guide_id, version.version)
                .await?;
            return Err(conflict(
                format!("guide {} version {}", version.guide_id, version.version),
                current,
            ));
        }
        Ok(expected.wrapping_add(1))
    }

    async fn get_version_revision(
        &self,
        guide_id: &str,
        version: u32,
    ) -> Result<Option<u64>, StoreError> {
        Ok(self
            .db()
            .select(VERSIONS)
            .columns(&["revision"])
            .filter(Box::new(where_eq("guide_id", guide_id)))
            .filter(Box::new(where_eq("version", i64::from(version))))
            .execute_first(self.db())
            .await?
            .as_ref()
            .map(revision_of))
    }

    /// Apply `change` to a version and write it.
    ///
    /// With `expected`, the version must still be at that revision, and a write that loses a
    /// race fails with [`StoreError::RevisionConflict`]. Without it, a lost race reads the
    /// version again and applies `change` again, a bounded number of times. When `change`
    /// fails, nothing is written and its error is returned inside the result.
    pub async fn update_guide_version<T, E>(
        &self,
        guide_id: &str,
        version: Option<u32>,
        expected: Option<u64>,
        change: &mut (dyn FnMut(&mut GuideVersion) -> Result<T, E> + Send),
    ) -> Result<Result<(GuideVersionRecord, T), E>, StoreError> {
        for _ in 0..MAX_WORKSPACE_WRITE_ATTEMPTS {
            let Some(record) = self.get_guide_version(guide_id, version).await? else {
                return Err(StoreError::InvalidRequest(format!(
                    "guide {guide_id} has no version {}",
                    version.map_or_else(|| "current".to_owned(), |number| number.to_string())
                )));
            };
            if let Some(expected) = expected
                && expected != record.revision
            {
                return Err(conflict(
                    format!("guide {guide_id} version {}", record.version.version),
                    Some(record.revision),
                ));
            }
            let mut changed = record.version.clone();
            let output = match change(&mut changed) {
                Ok(output) => output,
                Err(error) => return Ok(Err(error)),
            };
            changed.guide_id = record.version.guide_id.clone();
            changed.version = record.version.version;
            match self.save_guide_version_if(&changed, record.revision).await {
                Ok(revision) => {
                    changed.updated_at_ms = now_ms();
                    return Ok(Ok((
                        GuideVersionRecord {
                            version: changed,
                            revision,
                        },
                        output,
                    )));
                }
                Err(StoreError::RevisionConflict { .. }) if expected.is_none() => {}
                Err(error) => return Err(error),
            }
        }
        Err(conflict(format!("guide {guide_id}"), None))
    }

    /// Add `draft` as the next version of its guide (racing forks take the next free number),
    /// making it current when `make_current`. Returns the guide as it is now and the version.
    pub async fn fork_guide_version(
        &self,
        draft: &GuideVersion,
        make_current: bool,
    ) -> Result<(GuideRecord, GuideVersionRecord), StoreError> {
        let guide_id = draft.guide_id.clone();
        check_version(draft)?;
        let mut version = draft.clone();
        let now = now_ms();
        version.created_at_ms = now;
        version.updated_at_ms = now;
        let mut inserted = false;
        for _ in 0..MAX_WORKSPACE_WRITE_ATTEMPTS {
            let Some(guide) = self.get_guide(&guide_id).await? else {
                return Err(StoreError::InvalidRequest(format!(
                    "there is no guide {guide_id}"
                )));
            };
            let newest = self
                .list_guide_versions(&guide_id)
                .await?
                .last()
                .map_or(0, |summary| summary.version);
            version.version = newest.max(guide.guide.current_version) + 1;
            match self.insert_version(&version).await {
                Ok(()) => {
                    inserted = true;
                    break;
                }
                // Another fork took the number meanwhile: take the next one.
                Err(_)
                    if self
                        .get_version_revision(&guide_id, version.version)
                        .await?
                        .is_some() => {}
                Err(error) => return Err(error),
            }
        }
        if !inserted {
            return Err(conflict(format!("guide {guide_id}"), None));
        }
        for _ in 0..MAX_WORKSPACE_WRITE_ATTEMPTS {
            let Some(mut guide) = self.get_guide(&guide_id).await? else {
                return Err(StoreError::InvalidRequest(format!(
                    "there is no guide {guide_id}"
                )));
            };
            if make_current && guide.guide.current_version < version.version {
                guide.guide.current_version = version.version;
            }
            match self.update_guide_if(&guide.guide, guide.revision).await {
                Ok(revision) => {
                    guide.revision = revision;
                    return Ok((
                        guide,
                        GuideVersionRecord {
                            version,
                            revision: 0,
                        },
                    ));
                }
                Err(StoreError::RevisionConflict { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Err(conflict(format!("guide {guide_id}"), None))
    }

    /// Write `area`: create it when `expected` is `None` (failing when it exists), or replace it
    /// when its revision is still `expected`. Returns the new revision.
    pub async fn save_guide_area_if(
        &self,
        area: &GuideArea,
        expected: Option<u64>,
    ) -> Result<u64, StoreError> {
        check_area(area)?;
        let record = format!(
            "guide {} version {} area {}",
            area.guide_id, area.version, area.area_id
        );
        let document = serde_json::to_string(&GuideDocument {
            outline: area.outline.clone(),
            citations: area.citations.clone(),
            retired_sections: Vec::new(),
        })?;
        let now = now_ms();
        let Some(expected) = expected else {
            if let Some(current) = self.get_area_revision(area).await? {
                return Err(conflict(record, Some(current)));
            }
            self.db()
                .insert(AREAS)
                .value("guide_id", area.guide_id.clone())
                .value("version", i64::from(area.version))
                .value("area_id", area.area_id.clone())
                .value("session_id", optional_string(area.session_id.as_ref()))
                .value("status", area.status.as_str())
                .value("status_message", optional_string(area.message.as_ref()))
                .value("doc_json", document)
                .value("revision", 0_i64)
                .value("created_at_ms", u64_to_i64(now))
                .value("updated_at_ms", u64_to_i64(now))
                .execute(self.db())
                .await?;
            return Ok(0);
        };
        let updated = self
            .db()
            .update(AREAS)
            .value("session_id", optional_string(area.session_id.as_ref()))
            .value("status", area.status.as_str())
            .value("status_message", optional_string(area.message.as_ref()))
            .value("doc_json", document)
            .value("updated_at_ms", u64_to_i64(now))
            .value("revision", u64_to_i64(expected.wrapping_add(1)))
            .filter(Box::new(where_eq("guide_id", area.guide_id.clone())))
            .filter(Box::new(where_eq("version", i64::from(area.version))))
            .filter(Box::new(where_eq("area_id", area.area_id.clone())))
            .filter(Box::new(where_eq("revision", u64_to_i64(expected))))
            .execute(self.db())
            .await?;
        if updated.is_empty() {
            return Err(conflict(record, self.get_area_revision(area).await?));
        }
        Ok(expected.wrapping_add(1))
    }

    async fn get_area_revision(&self, area: &GuideArea) -> Result<Option<u64>, StoreError> {
        Ok(self
            .db()
            .select(AREAS)
            .columns(&["revision"])
            .filter(Box::new(where_eq("guide_id", area.guide_id.clone())))
            .filter(Box::new(where_eq("version", i64::from(area.version))))
            .filter(Box::new(where_eq("area_id", area.area_id.clone())))
            .execute_first(self.db())
            .await?
            .as_ref()
            .map(revision_of))
    }

    /// The fan-out areas of a version, by area id.
    pub async fn list_guide_areas(
        &self,
        guide_id: &str,
        version: u32,
    ) -> Result<Vec<GuideAreaRecord>, StoreError> {
        let rows = self
            .db()
            .select(AREAS)
            .filter(Box::new(where_eq("guide_id", guide_id)))
            .filter(Box::new(where_eq("version", i64::from(version))))
            .execute(self.db())
            .await?;
        let mut areas = rows
            .iter()
            .map(area_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        areas.sort_by(|a, b| a.area.area_id.cmp(&b.area.area_id));
        Ok(areas)
    }

    /// Create or replace an ask thread.
    pub async fn save_guide_thread(&self, thread: &GuideThread) -> Result<GuideThread, StoreError> {
        check_thread(thread)?;
        let mut thread = thread.clone();
        thread.updated_at_ms = now_ms();
        if thread.created_at_ms == 0 {
            thread.created_at_ms = thread.updated_at_ms;
        }
        self.db()
            .upsert(THREADS)
            .unique(&["thread_id"])
            .value("thread_id", thread.thread_id.clone())
            .value("guide_id", thread.guide_id.clone())
            .value("version", i64::from(thread.version))
            .value("section_id", thread.section_id.clone())
            .value("session_id", optional_string(thread.session_id.as_ref()))
            .value("status", thread.status.as_str())
            .value("question", thread.question.clone())
            .value("created_at_ms", u64_to_i64(thread.created_at_ms))
            .value("updated_at_ms", u64_to_i64(thread.updated_at_ms))
            .execute(self.db())
            .await?;
        Ok(thread)
    }

    /// The ask threads of a guide (of one version, when given), oldest first.
    pub async fn list_guide_threads(
        &self,
        guide_id: &str,
        version: Option<u32>,
    ) -> Result<Vec<GuideThread>, StoreError> {
        let mut query = self
            .db()
            .select(THREADS)
            .filter(Box::new(where_eq("guide_id", guide_id)));
        if let Some(version) = version {
            query = query.filter(Box::new(where_eq("version", i64::from(version))));
        }
        let rows = query.execute(self.db()).await?;
        let mut threads = rows
            .iter()
            .map(thread_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        threads
            .sort_by(|a, b| (a.created_at_ms, &a.thread_id).cmp(&(b.created_at_ms, &b.thread_id)));
        Ok(threads)
    }

    /// Delete an ask thread. Returns whether it existed.
    pub async fn delete_guide_thread(&self, thread_id: &str) -> Result<bool, StoreError> {
        let deleted = self
            .db()
            .delete(THREADS)
            .filter(Box::new(where_eq("thread_id", thread_id)))
            .execute(self.db())
            .await?;
        Ok(!deleted.is_empty())
    }

    /// Mark a section reviewed, replacing an earlier mark.
    pub async fn set_guide_section_mark(&self, mark: &GuideSectionMark) -> Result<(), StoreError> {
        self.db()
            .upsert(MARKS)
            .unique(&["guide_id", "section_id"])
            .value("guide_id", mark.guide_id.clone())
            .value("section_id", mark.section_id.clone())
            .value("reviewed_at_ms", u64_to_i64(mark.reviewed_at_ms))
            .value("fingerprint", mark.fingerprint.clone())
            .execute(self.db())
            .await?;
        Ok(())
    }

    /// Un-review a section. Returns whether it was marked.
    pub async fn clear_guide_section_mark(
        &self,
        guide_id: &str,
        section_id: &str,
    ) -> Result<bool, StoreError> {
        let deleted = self
            .db()
            .delete(MARKS)
            .filter(Box::new(where_eq("guide_id", guide_id)))
            .filter(Box::new(where_eq("section_id", section_id)))
            .execute(self.db())
            .await?;
        Ok(!deleted.is_empty())
    }

    /// The reviewed sections of a guide, by section id.
    pub async fn list_guide_section_marks(
        &self,
        guide_id: &str,
    ) -> Result<Vec<GuideSectionMark>, StoreError> {
        let rows = self
            .db()
            .select(MARKS)
            .filter(Box::new(where_eq("guide_id", guide_id)))
            .execute(self.db())
            .await?;
        let mut marks = rows
            .iter()
            .map(mark_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        marks.sort_by(|a, b| a.section_id.cmp(&b.section_id));
        Ok(marks)
    }
}

/// Run `operation` on the store of the repository at `repo_path`.
fn on_store<T: Send + 'static>(
    repo_path: &Path,
    config: &CodeReviewConfig,
    operation: impl for<'a> FnOnce(
        CodeReviewDb<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, StoreError>> + 'a>,
    > + Send
    + 'static,
) -> Result<T, StoreError> {
    let repo_root = resolve_repo_root(repo_path)?;
    with_database(&repo_root, config, move |database| {
        operation(CodeReviewDb::new(database))
    })
}

pub fn create_guide_for_request(
    request: CreateReviewGuideRequest,
    config: &CodeReviewConfig,
) -> Result<CreateReviewGuideResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let review_key = review_key_for_scope(&repo_root, &request.target, request.scope.as_ref())?;
    let (guide, version) = on_store(&repo_root, config, move |db| {
        Box::pin(async move {
            db.create_guide(&review_key, &request.guide, &request.version)
                .await
        })
    })?;
    Ok(CreateReviewGuideResponse { guide, version })
}

pub fn list_guides_for_request(
    request: &ListReviewGuidesRequest,
    config: &CodeReviewConfig,
) -> Result<ListReviewGuidesResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let review_key = review_key_for_scope(&repo_root, &request.target, request.scope.as_ref())?;
    let include_archived = request.include_archived;
    let guides = on_store(&repo_root, config, move |db| {
        Box::pin(async move { db.list_guides(&review_key, include_archived).await })
    })?;
    Ok(ListReviewGuidesResponse { guides })
}

pub fn get_guide_for_request(
    request: &GetReviewGuideRequest,
    config: &CodeReviewConfig,
) -> Result<GetReviewGuideResponse, StoreError> {
    let guide_id = request.guide_id.clone();
    let guide = on_store(&request.repo_path, config, move |db| {
        Box::pin(async move { db.get_guide(&guide_id).await })
    })?;
    Ok(GetReviewGuideResponse { guide })
}

pub fn update_guide_for_request(
    request: UpdateReviewGuideRequest,
    config: &CodeReviewConfig,
) -> Result<UpdateReviewGuideResponse, StoreError> {
    let revision = on_store(&request.repo_path, config, move |db| {
        Box::pin(async move {
            db.update_guide_if(&request.guide, request.expected_revision)
                .await
        })
    })?;
    Ok(UpdateReviewGuideResponse { revision })
}

pub fn delete_guide_for_request(
    request: &DeleteReviewGuideRequest,
    config: &CodeReviewConfig,
) -> Result<DeleteReviewGuideResponse, StoreError> {
    let guide_id = request.guide_id.clone();
    let deleted = on_store(&request.repo_path, config, move |db| {
        Box::pin(async move { db.delete_guide(&guide_id).await })
    })?;
    Ok(DeleteReviewGuideResponse { deleted })
}

pub fn rekey_guides_for_request(
    request: &RekeyReviewGuidesRequest,
    config: &CodeReviewConfig,
) -> Result<RekeyReviewGuidesResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let from = review_key_for_scope(&repo_root, request.from.target(), Some(&request.from))?;
    let to = review_key_for_scope(&repo_root, request.to.target(), Some(&request.to))?;
    if from == to {
        return Ok(RekeyReviewGuidesResponse { moved: 0 });
    }
    let moved = on_store(&repo_root, config, move |db| {
        Box::pin(async move { db.rekey_guides(&from, &to).await })
    })?;
    Ok(RekeyReviewGuidesResponse { moved })
}

pub fn get_guide_version_for_request(
    request: &GetGuideVersionRequest,
    config: &CodeReviewConfig,
) -> Result<GetGuideVersionResponse, StoreError> {
    let (guide_id, version) = (request.guide_id.clone(), request.version);
    let version = on_store(&request.repo_path, config, move |db| {
        Box::pin(async move { db.get_guide_version(&guide_id, version).await })
    })?;
    Ok(GetGuideVersionResponse { version })
}

pub fn list_guide_versions_for_request(
    request: &ListGuideVersionsRequest,
    config: &CodeReviewConfig,
) -> Result<ListGuideVersionsResponse, StoreError> {
    let guide_id = request.guide_id.clone();
    let versions = on_store(&request.repo_path, config, move |db| {
        Box::pin(async move { db.list_guide_versions(&guide_id).await })
    })?;
    Ok(ListGuideVersionsResponse { versions })
}

pub fn save_guide_version_for_request(
    request: SaveGuideVersionRequest,
    config: &CodeReviewConfig,
) -> Result<SaveGuideVersionResponse, StoreError> {
    let revision = on_store(&request.repo_path, config, move |db| {
        Box::pin(async move {
            db.save_guide_version_if(&request.version, request.expected_revision)
                .await
        })
    })?;
    Ok(SaveGuideVersionResponse { revision })
}

pub fn fork_guide_version_for_request(
    request: ForkGuideVersionRequest,
    config: &CodeReviewConfig,
) -> Result<ForkGuideVersionResponse, StoreError> {
    let (guide, version) = on_store(&request.repo_path, config, move |db| {
        Box::pin(async move {
            db.fork_guide_version(&request.version, request.make_current)
                .await
        })
    })?;
    Ok(ForkGuideVersionResponse { guide, version })
}

pub fn save_guide_area_for_request(
    request: SaveGuideAreaRequest,
    config: &CodeReviewConfig,
) -> Result<SaveGuideAreaResponse, StoreError> {
    let revision = on_store(&request.repo_path, config, move |db| {
        Box::pin(async move {
            db.save_guide_area_if(&request.area, request.expected_revision)
                .await
        })
    })?;
    Ok(SaveGuideAreaResponse { revision })
}

pub fn list_guide_areas_for_request(
    request: &ListGuideAreasRequest,
    config: &CodeReviewConfig,
) -> Result<ListGuideAreasResponse, StoreError> {
    let (guide_id, version) = (request.guide_id.clone(), request.version);
    let areas = on_store(&request.repo_path, config, move |db| {
        Box::pin(async move { db.list_guide_areas(&guide_id, version).await })
    })?;
    Ok(ListGuideAreasResponse { areas })
}

pub fn save_guide_thread_for_request(
    request: SaveGuideThreadRequest,
    config: &CodeReviewConfig,
) -> Result<SaveGuideThreadResponse, StoreError> {
    let thread = on_store(&request.repo_path, config, move |db| {
        Box::pin(async move { db.save_guide_thread(&request.thread).await })
    })?;
    Ok(SaveGuideThreadResponse { thread })
}

pub fn list_guide_threads_for_request(
    request: &ListGuideThreadsRequest,
    config: &CodeReviewConfig,
) -> Result<ListGuideThreadsResponse, StoreError> {
    let (guide_id, version) = (request.guide_id.clone(), request.version);
    let threads = on_store(&request.repo_path, config, move |db| {
        Box::pin(async move { db.list_guide_threads(&guide_id, version).await })
    })?;
    Ok(ListGuideThreadsResponse { threads })
}

pub fn delete_guide_thread_for_request(
    request: &DeleteGuideThreadRequest,
    config: &CodeReviewConfig,
) -> Result<DeleteGuideThreadResponse, StoreError> {
    let thread_id = request.thread_id.clone();
    let deleted = on_store(&request.repo_path, config, move |db| {
        Box::pin(async move { db.delete_guide_thread(&thread_id).await })
    })?;
    Ok(DeleteGuideThreadResponse { deleted })
}

pub fn set_guide_section_mark_for_request(
    request: SetGuideSectionMarkRequest,
    config: &CodeReviewConfig,
) -> Result<SetGuideSectionMarkResponse, StoreError> {
    on_store(&request.repo_path, config, move |db| {
        Box::pin(async move { db.set_guide_section_mark(&request.mark).await })
    })?;
    Ok(SetGuideSectionMarkResponse {})
}

pub fn clear_guide_section_mark_for_request(
    request: &ClearGuideSectionMarkRequest,
    config: &CodeReviewConfig,
) -> Result<ClearGuideSectionMarkResponse, StoreError> {
    let (guide_id, section_id) = (request.guide_id.clone(), request.section_id.clone());
    let cleared = on_store(&request.repo_path, config, move |db| {
        Box::pin(async move { db.clear_guide_section_mark(&guide_id, &section_id).await })
    })?;
    Ok(ClearGuideSectionMarkResponse { cleared })
}

pub fn list_guide_section_marks_for_request(
    request: &ListGuideSectionMarksRequest,
    config: &CodeReviewConfig,
) -> Result<ListGuideSectionMarksResponse, StoreError> {
    let guide_id = request.guide_id.clone();
    let marks = on_store(&request.repo_path, config, move |db| {
        Box::pin(async move { db.list_guide_section_marks(&guide_id).await })
    })?;
    Ok(ListGuideSectionMarksResponse { marks })
}

/// Apply `change` to a version and write it in one turn at the database (see
/// [`CodeReviewDb::update_guide_version`]).
pub fn update_guide_version_for_request<T, E>(
    repo_path: &Path,
    guide_id: String,
    version: Option<u32>,
    expected: Option<u64>,
    mut change: impl FnMut(&mut GuideVersion) -> Result<T, E> + Send + 'static,
    config: &CodeReviewConfig,
) -> Result<Result<(GuideVersionRecord, T), E>, StoreError>
where
    T: Send + 'static,
    E: Send + 'static,
{
    on_store(repo_path, config, move |db| {
        Box::pin(async move {
            db.update_guide_version(&guide_id, version, expected, &mut change)
                .await
        })
    })
}

#[cfg(test)]
mod tests;
