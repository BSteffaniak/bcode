#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Durable feedback store.
//!
//! One `SQLite` database per state location, at `<state-root>/feedback/feedback.db`, shared by
//! every daemon serving that location. Writes are short transactions; concurrent daemons
//! serialize through `SQLite` locking. The store is the record of truth for feedback: an intake
//! session's transcript may be compacted, but items, triage and status live here.
//!
//! Items recorded with [`FeedbackStore::record_for_triage`] carry a durable triage request,
//! written in the same transaction, that dispatchers lease and deliver to the intake session
//! until a turn triages the item (see [`dispatch`]).
//!
//! Compatibility: schema version 1 gained the `feedback_triage_requests` table additively. It
//! is created on open when missing; builds that predate it ignore it, and items they record
//! have no triage request.
//!
//! Status rules: an [`Actor::Agent`] may only move an item between
//! [`FeedbackStatus::Open`] and [`FeedbackStatus::AgentResolved`], and never touches an item
//! a person has verified or dismissed. Only an [`Actor::User`] verifies or dismisses.

mod attachments;
mod database;
pub mod dispatch;

use std::path::{Path, PathBuf};
use std::time::Duration;

use bcode_feedback_models::{
    Actor, ClaimIntakeSessionRequest, Facet, FeedbackId, FeedbackItem, FeedbackLink, FeedbackPage,
    FeedbackQuery, FeedbackStatus, FeedbackValidationError, IntakeSession, LinkFeedbackRequest,
    LinkKind, RecordFeedbackRequest, SetFeedbackStatusRequest, StatusChange, Triage,
    TriageFeedbackRequest, TriageState,
};
use switchy_database::query::{FilterableQuery as _, SortDirection, where_in};
use switchy_database::{Database, DatabaseValue, Row, TransactionMode};

/// Current on-disk schema version.
pub const SCHEMA_VERSION: i64 = 1;
/// Database file name inside the feedback directory.
pub const DATABASE_FILE: &str = "feedback.db";
/// Rows read per scan step while listing.
const SCAN_CHUNK: usize = 200;
/// Rows a single list call may scan before returning a partial page.
const MAX_SCAN: usize = 5_000;

/// Failure of a store operation.
#[derive(Debug, thiserror::Error)]
pub enum FeedbackStoreError {
    /// The request violates a bound.
    #[error(transparent)]
    Invalid(#[from] FeedbackValidationError),
    /// No item has this id.
    #[error("{0} does not exist")]
    NotFound(FeedbackId),
    /// The actor may not make this status change.
    #[error("{0}")]
    Forbidden(&'static str),
    /// An attachment source is missing, a symbolic link, too large, or the item is full.
    #[error("attachment rejected: {0}")]
    AttachmentRejected(String),
    /// The item changed concurrently; read it again and retry.
    #[error("{0} changed concurrently; read it again")]
    Conflict(FeedbackId),
    /// The database was written by a newer or unknown schema; it is left untouched.
    #[error("feedback store schema {0} is not supported by this build")]
    UnsupportedSchema(i64),
    /// The database path is not a regular file or its directory cannot be created.
    #[error("feedback store path {0} is unusable: {1}")]
    Path(PathBuf, String),
    /// A stored row does not decode.
    #[error("feedback store holds invalid data: {0}")]
    Corrupt(String),
    /// The database rejected an operation.
    #[error("feedback store database error: {0}")]
    Database(String),
}

impl From<switchy_database::DatabaseError> for FeedbackStoreError {
    fn from(error: switchy_database::DatabaseError) -> Self {
        Self::Database(error.to_string())
    }
}

impl From<switchy_database_connection::InitSqliteRusqliteError> for FeedbackStoreError {
    fn from(error: switchy_database_connection::InitSqliteRusqliteError) -> Self {
        Self::Database(error.to_string())
    }
}

/// Return the feedback database path for a resolved state location.
#[must_use]
pub fn database_path(state_dir: &Path) -> PathBuf {
    state_dir.join("feedback").join(DATABASE_FILE)
}

/// Open handle to the feedback store.
pub struct FeedbackStore {
    database: Box<dyn Database>,
    /// Directory holding the database and the `attachments` tree.
    root: PathBuf,
}

impl std::fmt::Debug for FeedbackStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeedbackStore").finish_non_exhaustive()
    }
}

impl FeedbackStore {
    /// Open, creating if needed, the store of a resolved state location.
    ///
    /// # Errors
    /// See [`Self::open_at`].
    pub fn open_in_state_dir(state_dir: &Path) -> Result<Self, FeedbackStoreError> {
        Self::open_at(&database_path(state_dir))
    }

    /// Open, creating if needed, the store at an explicit database path.
    ///
    /// # Errors
    /// * [`FeedbackStoreError::Path`] when the path is not a regular file or its directory
    ///   cannot be created.
    /// * [`FeedbackStoreError::UnsupportedSchema`] when the database has an unknown version.
    /// * [`FeedbackStoreError::Database`] when `SQLite` fails.
    pub fn open_at(path: &Path) -> Result<Self, FeedbackStoreError> {
        let parent = path
            .parent()
            .ok_or_else(|| FeedbackStoreError::Path(path.to_owned(), "no parent".into()))?;
        std::fs::create_dir_all(parent)
            .map_err(|error| FeedbackStoreError::Path(parent.to_owned(), error.to_string()))?;
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if !metadata.is_file() => {
                return Err(FeedbackStoreError::Path(
                    path.to_owned(),
                    "not a regular file".into(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(FeedbackStoreError::Path(path.to_owned(), error.to_string()));
            }
        }
        let database = database::open(path, Duration::from_secs(5))?;
        database::block_on(database::initialize(&database))?;
        Ok(Self {
            root: parent.to_owned(),
            database: Box::new(database),
        })
    }

    /// Record a new item, untriaged and open.
    ///
    /// # Errors
    /// [`FeedbackStoreError::Invalid`] for an out-of-bounds request, or a database error.
    pub fn record(
        &self,
        request: &RecordFeedbackRequest,
        now_ms: u64,
    ) -> Result<FeedbackItem, FeedbackStoreError> {
        request.validate()?;
        database::block_on(async {
            let tx = self
                .database
                .begin_transaction_with_mode(TransactionMode::Immediate)
                .await?;
            let id = insert_item(&*tx, request, now_ms).await?;
            tx.commit().await?;
            Ok::<_, FeedbackStoreError>(id)
        })
        .and_then(|id| self.get(id))
    }

    /// Read one item.
    ///
    /// # Errors
    /// [`FeedbackStoreError::NotFound`] for an unknown id, or a database error.
    pub fn get(&self, id: FeedbackId) -> Result<FeedbackItem, FeedbackStoreError> {
        database::block_on(async {
            let rows = self
                .database
                .select("feedback_items")
                .where_eq("id", int(id.0)?)
                .execute(&*self.database)
                .await?;
            let row = rows.first().ok_or(FeedbackStoreError::NotFound(id))?;
            let mut items = self.hydrate(std::slice::from_ref(row)).await?;
            items.pop().ok_or(FeedbackStoreError::NotFound(id))
        })
    }

    /// List matching items, newest first.
    ///
    /// A call scans a bounded number of rows. When the scan budget runs out before `limit`
    /// matches are found, the page holds fewer items and `next_before` continues the scan.
    ///
    /// # Errors
    /// [`FeedbackStoreError::Invalid`] for a bad limit, or a database error.
    pub fn list(&self, query: &FeedbackQuery) -> Result<FeedbackPage, FeedbackStoreError> {
        query.validate()?;
        let needle = query.text.as_ref().map(|text| text.to_lowercase());
        database::block_on(async {
            let mut page = FeedbackPage::default();
            let mut cursor = query.before;
            let mut scanned = 0;
            loop {
                let mut select = self
                    .database
                    .select("feedback_items")
                    .sort("id", SortDirection::Desc)
                    .limit(SCAN_CHUNK);
                if let Some(app) = &query.app {
                    select = select.where_eq("app", app.as_str());
                }
                if let Some(state) = query.triage_state {
                    select = select.where_eq("triage_state", triage_state_name(state));
                }
                if !query.statuses.is_empty() {
                    let names: Vec<DatabaseValue> = query
                        .statuses
                        .iter()
                        .map(|status| DatabaseValue::from(status_name(*status)))
                        .collect();
                    select = select.filter(Box::new(where_in("status", names)));
                }
                if let Some(before) = cursor {
                    select = select.where_lt("id", int(before.0)?);
                }
                let rows = select.execute(&*self.database).await?;
                let exhausted = rows.len() < SCAN_CHUNK;
                scanned += rows.len();
                let items = self.hydrate(&rows).await?;
                cursor = items.last().map(|item| item.id);
                for item in items {
                    if matches(&item, query, needle.as_deref()) {
                        page.items.push(item);
                        if page.items.len() == query.limit {
                            page.next_before = page.items.last().map(|item| item.id);
                            return Ok(page);
                        }
                    }
                }
                if exhausted {
                    return Ok(page);
                }
                if scanned >= MAX_SCAN {
                    page.next_before = cursor;
                    return Ok(page);
                }
            }
        })
    }

    /// Replace an item's triage and mark it triaged, completing its triage request.
    ///
    /// # Errors
    /// [`FeedbackStoreError::Invalid`], [`FeedbackStoreError::NotFound`], or a database error.
    pub fn triage(
        &self,
        request: &TriageFeedbackRequest,
        now_ms: u64,
    ) -> Result<FeedbackItem, FeedbackStoreError> {
        request.validate()?;
        let triage = serde_json::to_string(&request.triage)
            .map_err(|error| FeedbackStoreError::Corrupt(error.to_string()))?;
        database::block_on(async {
            let tx = self
                .database
                .begin_transaction_with_mode(TransactionMode::Immediate)
                .await?;
            let changed = tx
                .update("feedback_items")
                .where_eq("id", int(request.id.0)?)
                .value("triage_json", triage)
                .value("triage_state", triage_state_name(TriageState::Triaged))
                .value("updated_at_ms", int(now_ms)?)
                .execute(&*tx)
                .await?;
            if changed.is_empty() {
                tx.rollback().await?;
                return Err(FeedbackStoreError::NotFound(request.id));
            }
            dispatch::complete(&*tx, request.id, now_ms).await?;
            tx.commit().await?;
            Ok(())
        })?;
        self.get(request.id)
    }

    /// Change an item's status.
    ///
    /// # Errors
    /// * [`FeedbackStoreError::Forbidden`] when an agent sets a status reserved for people, or
    ///   changes an item a person verified or dismissed.
    /// * [`FeedbackStoreError::Conflict`] when the status changed since it was read.
    /// * [`FeedbackStoreError::Invalid`], [`FeedbackStoreError::NotFound`], or a database error.
    pub fn set_status(
        &self,
        request: &SetFeedbackStatusRequest,
        now_ms: u64,
    ) -> Result<FeedbackItem, FeedbackStoreError> {
        request.validate()?;
        let current = self.get(request.id)?;
        check_transition(current.status, request.status, request.actor)?;
        let change = StatusChange {
            by: request.actor,
            at_ms: now_ms,
            note: request.note.clone(),
            session_id: request.session_id.clone(),
            commits: request.commits.clone(),
        };
        let change = serde_json::to_string(&change)
            .map_err(|error| FeedbackStoreError::Corrupt(error.to_string()))?;
        let changed = database::block_on(
            self.database
                .update("feedback_items")
                .where_eq("id", int(request.id.0)?)
                .where_eq("status", status_name(current.status))
                .value("status", status_name(request.status))
                .value("status_change_json", change)
                .value("updated_at_ms", int(now_ms)?)
                .execute(&*self.database),
        )?;
        if changed.is_empty() {
            return Err(FeedbackStoreError::Conflict(request.id));
        }
        self.get(request.id)
    }

    /// Link one item to another. Linking twice with the same kind is a no-op.
    ///
    /// # Errors
    /// [`FeedbackStoreError::Invalid`] for a self-link, [`FeedbackStoreError::NotFound`] when
    /// either item is missing, or a database error.
    pub fn link(
        &self,
        request: &LinkFeedbackRequest,
        now_ms: u64,
    ) -> Result<FeedbackItem, FeedbackStoreError> {
        if request.from == request.to {
            return Err(FeedbackValidationError::SelfLink.into());
        }
        self.get(request.to)?;
        self.get(request.from)?;
        database::block_on(async {
            let tx = self
                .database
                .begin_transaction_with_mode(TransactionMode::Immediate)
                .await?;
            tx.insert("feedback_links")
                .value("from_id", int(request.from.0)?)
                .value("to_id", int(request.to.0)?)
                .value("kind", link_kind_name(request.kind))
                .or_ignore()
                .execute_count(&*tx)
                .await?;
            tx.update("feedback_items")
                .where_eq("id", int(request.from.0)?)
                .value("updated_at_ms", int(now_ms)?)
                .execute(&*tx)
                .await?;
            tx.commit().await?;
            Ok::<(), FeedbackStoreError>(())
        })?;
        self.get(request.from)
    }

    /// Read the intake session.
    ///
    /// # Errors
    /// A database error.
    pub fn intake_session(&self) -> Result<IntakeSession, FeedbackStoreError> {
        database::block_on(async {
            let row = self
                .database
                .select("feedback_meta")
                .where_eq("id", 1)
                .execute_first(&*self.database)
                .await?
                .ok_or_else(|| FeedbackStoreError::Corrupt("missing meta row".into()))?;
            Ok(IntakeSession {
                session_id: optional_text(&row, "intake_session_id")?,
            })
        })
    }

    /// Install `session_id` as the intake session when the current one equals `expected`.
    ///
    /// Returns the intake session after the attempt, which is the caller's session when the
    /// claim won and the concurrent winner otherwise.
    ///
    /// # Errors
    /// [`FeedbackStoreError::Invalid`] for malformed ids, or a database error.
    pub fn claim_intake_session(
        &self,
        request: &ClaimIntakeSessionRequest,
    ) -> Result<IntakeSession, FeedbackStoreError> {
        request.validate()?;
        database::block_on(async {
            let tx = self
                .database
                .begin_transaction_with_mode(TransactionMode::Immediate)
                .await?;
            let row = tx
                .select("feedback_meta")
                .where_eq("id", 1)
                .execute_first(&*tx)
                .await?
                .ok_or_else(|| FeedbackStoreError::Corrupt("missing meta row".into()))?;
            let current = optional_text(&row, "intake_session_id")?;
            let winner = if current == request.expected {
                tx.update("feedback_meta")
                    .where_eq("id", 1)
                    .value("intake_session_id", request.session_id.as_str())
                    .execute(&*tx)
                    .await?;
                Some(request.session_id.clone())
            } else {
                current
            };
            tx.commit().await?;
            Ok(IntakeSession { session_id: winner })
        })
    }

    async fn hydrate(&self, rows: &[Row]) -> Result<Vec<FeedbackItem>, FeedbackStoreError> {
        let mut items = rows
            .iter()
            .map(item_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        if items.is_empty() {
            return Ok(items);
        }
        let ids: Vec<DatabaseValue> = items
            .iter()
            .map(|item| int(item.id.0))
            .collect::<Result<_, _>>()?;
        let facets = self
            .database
            .select("feedback_facets")
            .filter(Box::new(where_in("item_id", ids.clone())))
            .sort("key", SortDirection::Asc)
            .execute(&*self.database)
            .await?;
        let links = self
            .database
            .select("feedback_links")
            .filter(Box::new(where_in("from_id", ids.clone())))
            .sort("to_id", SortDirection::Asc)
            .execute(&*self.database)
            .await?;
        let attachments = self
            .database
            .select("feedback_attachments")
            .filter(Box::new(where_in("item_id", ids.clone())))
            .sort("ordinal", SortDirection::Asc)
            .execute(&*self.database)
            .await?;
        let dispatches = dispatch::dispatches(
            &self
                .database
                .select(dispatch::TABLE)
                .filter(Box::new(where_in("item_id", ids)))
                .execute(&*self.database)
                .await?,
        )?;
        for item in &mut items {
            let id = item.id.0;
            for row in &facets {
                if unsigned(row, "item_id")? == id {
                    item.facets.push(Facet {
                        key: text(row, "key")?,
                        value: text(row, "value")?,
                        sensitive: unsigned(row, "sensitive")? != 0,
                    });
                }
            }
            for row in &links {
                if unsigned(row, "from_id")? == id {
                    item.links.push(FeedbackLink {
                        kind: parse_link_kind(&text(row, "kind")?)?,
                        target: FeedbackId(unsigned(row, "to_id")?),
                    });
                }
            }
            for row in &attachments {
                if unsigned(row, "item_id")? == id {
                    item.attachments.push(attachments::from_row(row)?);
                }
            }
            item.triage_dispatch = dispatches
                .iter()
                .find(|(item_id, _)| *item_id == id)
                .map(|(_, dispatch)| dispatch.clone());
        }
        Ok(items)
    }
}

fn check_transition(
    from: FeedbackStatus,
    to: FeedbackStatus,
    actor: Actor,
) -> Result<(), FeedbackStoreError> {
    if actor == Actor::User {
        return Ok(());
    }
    if matches!(from, FeedbackStatus::Verified | FeedbackStatus::Dismissed) {
        return Err(FeedbackStoreError::Forbidden(
            "a person verified or dismissed this item; agents cannot change it",
        ));
    }
    if matches!(to, FeedbackStatus::Verified | FeedbackStatus::Dismissed) {
        return Err(FeedbackStoreError::Forbidden(
            "only a person can verify or dismiss feedback; use agent_resolved",
        ));
    }
    Ok(())
}

fn matches(item: &FeedbackItem, query: &FeedbackQuery, needle: Option<&str>) -> bool {
    let triage = item.triage.as_ref();
    if let Some(category) = &query.category
        && triage.and_then(|triage| triage.category.as_ref()) != Some(category)
    {
        return false;
    }
    if !query.tags.is_empty() && !triage.is_some_and(|triage| query.tags.is_subset(&triage.tags)) {
        return false;
    }
    if !query.facets.iter().all(|(key, value)| {
        item.facets
            .iter()
            .any(|facet| &facet.key == key && &facet.value == value)
    }) {
        return false;
    }
    needle.is_none_or(|needle| {
        item.text.to_lowercase().contains(needle)
            || triage.is_some_and(|triage| {
                triage.title.to_lowercase().contains(needle)
                    || triage
                        .summary
                        .as_ref()
                        .is_some_and(|summary| summary.to_lowercase().contains(needle))
            })
    })
}

fn item_from_row(row: &Row) -> Result<FeedbackItem, FeedbackStoreError> {
    let triage = optional_text(row, "triage_json")?
        .map(|json| serde_json::from_str::<Triage>(&json))
        .transpose()
        .map_err(|error| FeedbackStoreError::Corrupt(error.to_string()))?;
    let status_change = optional_text(row, "status_change_json")?
        .map(|json| serde_json::from_str::<StatusChange>(&json))
        .transpose()
        .map_err(|error| FeedbackStoreError::Corrupt(error.to_string()))?;
    Ok(FeedbackItem {
        id: FeedbackId(unsigned(row, "id")?),
        app: text(row, "app")?,
        text: text(row, "text")?,
        facets: Vec::new(),
        created_at_ms: unsigned(row, "created_at_ms")?,
        updated_at_ms: unsigned(row, "updated_at_ms")?,
        triage_state: parse_triage_state(&text(row, "triage_state")?)?,
        triage,
        status: parse_status(&text(row, "status")?)?,
        status_change,
        links: Vec::new(),
        attachments: Vec::new(),
        triage_dispatch: None,
    })
}

/// Insert a new open, untriaged item and its facets inside `database`'s transaction.
async fn insert_item(
    database: &dyn Database,
    request: &RecordFeedbackRequest,
    now_ms: u64,
) -> Result<FeedbackId, FeedbackStoreError> {
    let row = database
        .insert("feedback_items")
        .value("app", request.app.as_str())
        .value("text", request.text.as_str())
        .value("created_at_ms", int(now_ms)?)
        .value("updated_at_ms", int(now_ms)?)
        .value("triage_state", triage_state_name(TriageState::Pending))
        .value("status", status_name(FeedbackStatus::Open))
        .execute(database)
        .await?;
    let id = FeedbackId(unsigned(&row, "id")?);
    for facet in &request.facets {
        database
            .insert("feedback_facets")
            .value("item_id", int(id.0)?)
            .value("key", facet.key.as_str())
            .value("value", facet.value.as_str())
            .value("sensitive", i64::from(facet.sensitive))
            .execute(database)
            .await?;
    }
    Ok(id)
}

const fn status_name(status: FeedbackStatus) -> &'static str {
    match status {
        FeedbackStatus::Open => "open",
        FeedbackStatus::AgentResolved => "agent_resolved",
        FeedbackStatus::Verified => "verified",
        FeedbackStatus::Dismissed => "dismissed",
    }
}

fn parse_status(value: &str) -> Result<FeedbackStatus, FeedbackStoreError> {
    Ok(match value {
        "open" => FeedbackStatus::Open,
        "agent_resolved" => FeedbackStatus::AgentResolved,
        "verified" => FeedbackStatus::Verified,
        "dismissed" => FeedbackStatus::Dismissed,
        other => return Err(FeedbackStoreError::Corrupt(format!("status {other:?}"))),
    })
}

const fn triage_state_name(state: TriageState) -> &'static str {
    match state {
        TriageState::Pending => "pending",
        TriageState::Triaged => "triaged",
    }
}

fn parse_triage_state(value: &str) -> Result<TriageState, FeedbackStoreError> {
    match value {
        "pending" => Ok(TriageState::Pending),
        "triaged" => Ok(TriageState::Triaged),
        other => Err(FeedbackStoreError::Corrupt(format!(
            "triage state {other:?}"
        ))),
    }
}

const fn link_kind_name(kind: LinkKind) -> &'static str {
    match kind {
        LinkKind::Duplicate => "duplicate",
        LinkKind::Related => "related",
        LinkKind::CausedBy => "caused_by",
    }
}

fn parse_link_kind(value: &str) -> Result<LinkKind, FeedbackStoreError> {
    match value {
        "duplicate" => Ok(LinkKind::Duplicate),
        "related" => Ok(LinkKind::Related),
        "caused_by" => Ok(LinkKind::CausedBy),
        other => Err(FeedbackStoreError::Corrupt(format!("link kind {other:?}"))),
    }
}

fn int(value: u64) -> Result<DatabaseValue, FeedbackStoreError> {
    i64::try_from(value)
        .map(DatabaseValue::Int64)
        .map_err(|_| FeedbackStoreError::Corrupt("integer exceeds storage range".into()))
}

fn unsigned(row: &Row, column: &str) -> Result<u64, FeedbackStoreError> {
    row.get(column)
        .and_then(|value| value.as_i64())
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| FeedbackStoreError::Corrupt(format!("column {column}")))
}

fn text(row: &Row, column: &str) -> Result<String, FeedbackStoreError> {
    optional_text(row, column)?
        .ok_or_else(|| FeedbackStoreError::Corrupt(format!("column {column}")))
}

fn optional_text(row: &Row, column: &str) -> Result<Option<String>, FeedbackStoreError> {
    match row.get(column) {
        None | Some(DatabaseValue::Null | DatabaseValue::StringOpt(None)) => Ok(None),
        Some(DatabaseValue::String(value) | DatabaseValue::StringOpt(Some(value))) => {
            Ok(Some(value))
        }
        Some(_) => Err(FeedbackStoreError::Corrupt(format!("column {column}"))),
    }
}

#[cfg(test)]
mod tests;
