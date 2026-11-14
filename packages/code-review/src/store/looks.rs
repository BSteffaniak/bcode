//! Looks: which versions of each pull request the reviewer saw, and how (see
//! `docs/review-stack-history.md`). Per user, in the review store, shared by every review of the
//! pull request; a comparison's "since you last looked" baseline is chosen from them.

use super::db::{CodeReviewDb, int_column, nullable_text_column, text_column};
use super::{CodeReviewConfig, StoreError, resolve_repo_root, with_database};
use bcode_code_review_models::{
    LayerLook, ListLayerLooksRequest, ListLayerLooksResponse, LookKind, MAX_LOOK_HEADS_PER_PULL,
    RecordLayerLooksRequest, RecordLayerLooksResponse,
};
use std::collections::{BTreeMap, BTreeSet};
use switchy::database::query::{FilterableQuery as _, where_eq, where_in};
use switchy::database::schema::create_table;
use switchy::database::{DatabaseValue, Row};
use switchy::schema::discovery::code::CodeMigration;

const TABLE: &str = "layer_looks";

/// The table of looks.
pub fn layer_looks_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "027_layer_looks_table".to_string(),
        Box::new(
            create_table(TABLE)
                .if_not_exists(true)
                .column(text_column("look_id"))
                .column(text_column("provider"))
                .column(text_column("repository"))
                .column(int_column("number"))
                .column(text_column("head_oid"))
                .column(nullable_text_column("fork_oid"))
                .column(text_column("kind"))
                .column(int_column("looked_at_ms"))
                .column(nullable_text_column("review_id"))
                .primary_key("look_id"),
        ),
        None,
    )
}

/// A look's row id: its pull request, head, and kind.
fn look_id(look: &LayerLook) -> String {
    format!(
        "{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
        look.provider,
        look.repository,
        look.number,
        look.head_oid,
        look.kind.as_str()
    )
}

/// `known` with what `new` adds: the later time (and its review), and what it knows that
/// `known` does not.
fn merged(mut known: LayerLook, new: &LayerLook) -> LayerLook {
    if new.looked_at_ms > known.looked_at_ms {
        known.looked_at_ms = new.looked_at_ms;
        if new.review_id.is_some() {
            known.review_id.clone_from(&new.review_id);
        }
    }
    if known.fork_oid.is_none() {
        known.fork_oid.clone_from(&new.fork_oid);
    }
    if known.review_id.is_none() {
        known.review_id.clone_from(&new.review_id);
    }
    known
}

fn look_from_row(row: &Row) -> Option<LayerLook> {
    let text = |column: &str| {
        row.get(column)
            .and_then(|value| value.as_str().map(str::to_owned))
    };
    let int = |column: &str| row.get(column).and_then(|value| value.as_i64());
    Some(LayerLook {
        provider: text("provider")?,
        repository: text("repository")?,
        number: u64::try_from(int("number")?).ok()?,
        head_oid: text("head_oid")?,
        fork_oid: text("fork_oid"),
        kind: LookKind::parse(&text("kind")?)?,
        looked_at_ms: u64::try_from(int("looked_at_ms")?).unwrap_or(0),
        review_id: text("review_id"),
    })
}

fn optional(value: Option<&String>) -> DatabaseValue {
    value.map_or(DatabaseValue::Null, |value| {
        DatabaseValue::String(value.clone())
    })
}

fn int(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// The heads of `looks` (one pull request's) past the newest [`MAX_LOOK_HEADS_PER_PULL`], by
/// when each head was last looked at.
fn heads_to_prune(looks: &[LayerLook]) -> Vec<String> {
    let mut latest: BTreeMap<&str, u64> = BTreeMap::new();
    for look in looks {
        let at = latest.entry(look.head_oid.as_str()).or_default();
        *at = (*at).max(look.looked_at_ms);
    }
    let mut heads: Vec<(u64, &str)> = latest.into_iter().map(|(head, at)| (at, head)).collect();
    heads.sort_unstable_by(|a, b| b.cmp(a));
    heads
        .into_iter()
        .skip(MAX_LOOK_HEADS_PER_PULL)
        .map(|(_, head)| head.to_owned())
        .collect()
}

impl CodeReviewDb<'_> {
    /// Record `looks`, merging each into what is known of it, then prune each touched pull
    /// request to its newest heads. Returns how many were added or changed, and the heads
    /// pruned.
    pub async fn record_looks(
        &self,
        looks: &[LayerLook],
    ) -> Result<(usize, Vec<String>), StoreError> {
        let mut recorded = 0;
        let mut touched: BTreeSet<(String, String, u64)> = BTreeSet::new();
        for look in looks {
            let id = look_id(look);
            let known = self
                .db()
                .select(TABLE)
                .filter(Box::new(where_eq("look_id", id.clone())))
                .execute_first(self.db())
                .await?
                .as_ref()
                .and_then(look_from_row);
            let look = match known {
                Some(known) => {
                    let merged = merged(known.clone(), look);
                    if merged == known {
                        continue;
                    }
                    merged
                }
                None => look.clone(),
            };
            recorded += 1;
            touched.insert((look.provider.clone(), look.repository.clone(), look.number));
            self.db()
                .upsert(TABLE)
                .unique(&["look_id"])
                .value("look_id", id)
                .value("provider", look.provider.clone())
                .value("repository", look.repository.clone())
                .value("number", int(look.number))
                .value("head_oid", look.head_oid.clone())
                .value("fork_oid", optional(look.fork_oid.as_ref()))
                .value("kind", look.kind.as_str())
                .value("looked_at_ms", int(look.looked_at_ms))
                .value("review_id", optional(look.review_id.as_ref()))
                .execute(self.db())
                .await?;
        }
        let mut pruned = Vec::new();
        for (provider, repository, number) in touched {
            let looks = self.list_looks(&provider, &repository, &[number]).await?;
            for head in heads_to_prune(&looks) {
                self.db()
                    .delete(TABLE)
                    .filter(Box::new(where_eq("provider", provider.clone())))
                    .filter(Box::new(where_eq("repository", repository.clone())))
                    .filter(Box::new(where_eq("number", int(number))))
                    .filter(Box::new(where_eq("head_oid", head.clone())))
                    .execute(self.db())
                    .await?;
                pruned.push(head);
            }
        }
        Ok((recorded, pruned))
    }

    /// The looks of pull requests `numbers` of `provider`'s `repository`, oldest first per pull
    /// request.
    pub async fn list_looks(
        &self,
        provider: &str,
        repository: &str,
        numbers: &[u64],
    ) -> Result<Vec<LayerLook>, StoreError> {
        if numbers.is_empty() {
            return Ok(Vec::new());
        }
        let numbers: Vec<DatabaseValue> = numbers
            .iter()
            .map(|number| DatabaseValue::Int64(int(*number)))
            .collect();
        let rows = self
            .db()
            .select(TABLE)
            .filter(Box::new(where_eq("provider", provider)))
            .filter(Box::new(where_eq("repository", repository)))
            .filter(Box::new(where_in("number", numbers)))
            .execute(self.db())
            .await?;
        let mut looks: Vec<LayerLook> = rows.iter().filter_map(look_from_row).collect();
        looks.sort_by(|a, b| {
            (a.number, a.looked_at_ms, &a.head_oid, a.kind).cmp(&(
                b.number,
                b.looked_at_ms,
                &b.head_oid,
                b.kind,
            ))
        });
        Ok(looks)
    }
}

/// Record looks in the repository's store.
///
/// # Errors
///
/// * See [`super::ReviewStore`].
pub fn record_layer_looks_for_request(
    request: RecordLayerLooksRequest,
    config: &CodeReviewConfig,
) -> Result<RecordLayerLooksResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let looks = request.looks;
    let (recorded, pruned_heads) = with_database(&repo_root, config, move |database| {
        Box::pin(async move { CodeReviewDb::new(database).record_looks(&looks).await })
    })?;
    Ok(RecordLayerLooksResponse {
        recorded,
        pruned_heads,
    })
}

/// List looks from the repository's store.
///
/// # Errors
///
/// * See [`super::ReviewStore`].
pub fn list_layer_looks_for_request(
    request: &ListLayerLooksRequest,
    config: &CodeReviewConfig,
) -> Result<ListLayerLooksResponse, StoreError> {
    let repo_root = resolve_repo_root(&request.repo_path)?;
    let (provider, repository, numbers) = (
        request.provider.clone(),
        request.repository.clone(),
        request.numbers.clone(),
    );
    let looks = with_database(&repo_root, config, move |database| {
        Box::pin(async move {
            CodeReviewDb::new(database)
                .list_looks(&provider, &repository, &numbers)
                .await
        })
    })?;
    Ok(ListLayerLooksResponse { looks })
}
