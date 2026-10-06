//! Descriptive execution timing from observed attempt intervals, not causal profiling.
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub fn describe(inspection: &Value) -> Value {
    let mut intervals = Vec::new();
    let mut stages = BTreeMap::<String, (u64, usize)>::new();
    let mut missing = 0;
    for attempt in inspection["attempts"].as_array().into_iter().flatten() {
        let pair = attempt["admitted_at_ms"]
            .as_u64()
            .zip(attempt["terminal_at_ms"].as_u64());
        let Some((start, end)) = pair.filter(|(start, end)| end >= start) else {
            missing += 1;
            continue;
        };
        let node = attempt["node_id"].as_str().unwrap_or("unknown");
        let stage = stages.entry(node.into()).or_default();
        stage.0 = stage.0.saturating_add(end - start);
        stage.1 += 1;
        intervals.push((start, end));
    }
    intervals.sort_unstable();
    let mut union = 0_u64;
    let mut current: Option<(u64, u64)> = None;
    let mut overlap = false;
    for (start, end) in &intervals {
        match current {
            Some((left, right)) if *start < right => {
                overlap = true;
                current = Some((left, right.max(*end)));
            }
            Some((left, right)) => {
                union = union.saturating_add(right - left);
                current = Some((*start, *end));
            }
            None => current = Some((*start, *end)),
        }
    }
    if let Some((start, end)) = current {
        union = union.saturating_add(end - start);
    }
    let span = intervals
        .first()
        .zip(intervals.iter().map(|(_, end)| end).max())
        .map(|((start, _), end)| end - start);
    json!({"schema_version":1,"stages":stages.into_iter().map(|(node,(elapsed,count))|json!({"node_id":node,"attempts":count,"summed_attempt_elapsed_ms":elapsed})).collect::<Vec<_>>(),"observed_attempts":intervals.len(),"missing_intervals":missing,"overlap_observed":overlap,"active_union_ms":union,"observed_span_ms":span,"between_attempt_gap_ms":span.map(|span|span.saturating_sub(union)),"complete":missing==0 && inspection["attempts"].as_array().is_some_and(|v|!v.is_empty() && v.len()<100),"interpretation":"Attempt elapsed includes provider, tools and waiting; overlap does not establish useful parallel work. Prefix observation is not a full critical path."})
}

#[cfg(test)]
mod tests {
    #[test]
    fn distinguishes_serial_and_overlapping_intervals_without_double_counting() {
        let inspect = |intervals: &[(u64, u64)]| serde_json::json!({"attempts":intervals.iter().map(|(start,end)|serde_json::json!({"node_id":"worker","admitted_at_ms":start,"terminal_at_ms":end})).collect::<Vec<_>>()});
        let serial = super::describe(&inspect(&[(10, 20), (25, 35)]));
        assert_eq!(serial["active_union_ms"], 20);
        assert_eq!(serial["between_attempt_gap_ms"], 5);
        assert_eq!(serial["overlap_observed"], false);
        let parallel = super::describe(&inspect(&[(10, 30), (20, 40)]));
        assert_eq!(parallel["active_union_ms"], 30);
        assert_eq!(parallel["overlap_observed"], true);
        assert_eq!(parallel["stages"][0]["summed_attempt_elapsed_ms"], 40);
        assert_eq!(
            super::describe(&serde_json::json!({"attempts":[{}]}))["complete"],
            false
        );
    }
}
