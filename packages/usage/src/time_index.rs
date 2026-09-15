//! Exact dyadic time aggregates: bounded range reads independent of request history length.
use bcode_usage_models::UsageTotals;
use switchy::database::{Database, query::FilterableQuery};

/// Disjoint power-of-two intervals covering a half-open millisecond range.
fn cover(mut from: u64, to: u64) -> Vec<(u32, u64)> {
    let mut nodes = Vec::new();
    while from < to {
        let remaining = to - from;
        let level = from.trailing_zeros().min(remaining.ilog2());
        nodes.push((level, from >> level));
        from += 1_u64 << level;
    }
    nodes
}

/// Add one contribution to ancestors in a session/model-owned tree inside the caller transaction.
pub async fn add(
    db: &dyn Database,
    tree: &str,
    timestamp: u64,
    contribution: &UsageTotals,
) -> Result<(), String> {
    if timestamp >= i64::MAX.cast_unsigned() {
        return Err("invalid usage timestamp".into());
    }
    for level in 0..63_u32 {
        let key = format!("{tree}/{level}/{}", timestamp >> level);
        let current = db
            .select("usage_time_nodes")
            .where_eq("node_key", key.clone())
            .execute_first(db)
            .await
            .map_err(super::index::error)?;
        let mut totals = current
            .as_ref()
            .map(|row| {
                super::index::text(row, "totals_json").and_then(|json| {
                    serde_json::from_str::<UsageTotals>(&json).map_err(super::index::error)
                })
            })
            .transpose()?
            .unwrap_or_default();
        crate::summary::merge(&mut totals, contribution)?;
        db.upsert("usage_time_nodes")
            .unique(&["node_key"])
            .value("node_key", key)
            .value("tree", tree)
            .value(
                "totals_json",
                serde_json::to_string(&totals).map_err(super::index::error)?,
            )
            .execute(db)
            .await
            .map_err(super::index::error)?;
    }
    Ok(())
}

/// Read at most 126 nodes for one exact range; never scans leaf requests.
pub async fn range(
    db: &dyn Database,
    tree: &str,
    from: u64,
    to: u64,
) -> Result<UsageTotals, String> {
    if from >= to || to > i64::MAX.cast_unsigned() {
        return Err("invalid usage aggregate range".into());
    }
    let mut totals = UsageTotals::default();
    for (level, offset) in cover(from, to) {
        let key = format!("{tree}/{level}/{offset}");
        if let Some(row) = db
            .select("usage_time_nodes")
            .where_eq("node_key", key)
            .execute_first(db)
            .await
            .map_err(super::index::error)?
        {
            let contribution: UsageTotals =
                serde_json::from_str(&super::index::text(&row, "totals_json")?)
                    .map_err(super::index::error)?;
            crate::summary::merge(&mut totals, &contribution)?;
        }
    }
    Ok(totals)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cover_is_exact_disjoint_and_bounded() {
        for from in 0..128 {
            for to in from + 1..256 {
                let nodes = cover(from, to);
                let mut cursor = from;
                for (level, offset) in nodes {
                    assert_eq!(offset << level, cursor);
                    cursor += 1_u64 << level;
                }
                assert_eq!(cursor, to);
            }
        }
        assert!(cover(1, i64::MAX.cast_unsigned()).len() <= 126);
    }
}
