//! Explicit reporting-index upgrades. Never invoked by normal report reads.
use super::{error, integer, text};
use switchy::database::{
    Database,
    query::{FilterableQuery, SortDirection},
};

/// Reconstruct exact aggregates from existing reporting contributions under the caller's
/// exclusive file lock and transaction. Canonical session stores are never accessed.
pub(super) async fn upgrade_v1(db: &dyn Database) -> Result<(), String> {
    db.query_raw("CREATE TABLE usage_time_nodes (node_key TEXT PRIMARY KEY, tree TEXT NOT NULL, totals_json TEXT NOT NULL)").await.map_err(error)?;
    db.query_raw("CREATE INDEX usage_time_tree ON usage_time_nodes(tree)")
        .await
        .map_err(error)?;
    let mut after = 0_i64;
    loop {
        let rows = db
            .select("usage_rows")
            .where_gt("ordinal", after)
            .sort("ordinal", SortDirection::Asc)
            .limit(128)
            .execute(db)
            .await
            .map_err(error)?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            after = integer(&row, "ordinal")?;
            let id = text(&row, "session_id")?;
            let source = db
                .select("usage_sessions")
                .where_eq("session_id", id.clone())
                .execute_first(db)
                .await
                .map_err(error)?
                .ok_or_else(|| error("missing collection source"))?;
            let collecting = integer(&source, "collecting")?;
            let staged = integer(&row, "staged")?;
            if !matches!(collecting, 0 | 1) || !matches!(staged, 0 | 1) {
                return Err(error("invalid collection state"));
            }
            // A collecting v1 source retains only the staging generation, not its older published
            // generation. The older requests remain preserved but cannot seed this generation.
            if staged != collecting {
                continue;
            }
            let entry = serde_json::from_str(&text(&row, "entry_json")?).map_err(error)?;
            super::index_contribution(db, &id, &text(&source, "generation")?, &entry).await?;
        }
    }
    db.update("usage_meta")
        .value("version", 2)
        .where_eq("id", 1)
        .execute(db)
        .await
        .map_err(error)?;
    Ok(())
}
