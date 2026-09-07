//! Incremental accounting over canonical request observations.

use bcode_session_models::{SessionCostEstimate, SessionTokenUsage, SessionUsageSummary};

/// Decide whether a request observation replaces its current contribution.
/// Identical and stale deliveries are harmless; conflicting duplicates are not.
pub fn accepts(
    current: Option<&SessionTokenUsage>,
    next: &SessionTokenUsage,
) -> Result<bool, String> {
    let Some(current) = current else {
        return Ok(true);
    };
    if next == current || next.observation_ordinal < current.observation_ordinal {
        return Ok(false);
    }
    if let (Some(current_request), Some(next_request)) = (&current.request, &next.request)
        && current_request != next_request
    {
        return Err("request attribution changed within one billing attempt".to_owned());
    }
    if current.terminal || next.observation_ordinal == current.observation_ordinal {
        return Err("conflicting usage observation for an existing request".to_owned());
    }
    if let Some(SessionCostEstimate::Estimated {
        currency,
        total_micros,
        ..
    }) = &current.cost
        && !matches!(&next.cost, Some(SessionCostEstimate::Estimated { currency: next_currency, total_micros: next_total, .. })
            if next_currency == currency && next_total >= total_micros)
    {
        return Err(
            "a recorded cost cannot be reduced or removed by a later usage observation".to_owned(),
        );
    }
    Ok(true)
}

/// Replace exactly one accepted request contribution, without replaying other requests.
pub fn replace(
    summary: &mut SessionUsageSummary,
    previous: Option<&SessionTokenUsage>,
    next: &SessionTokenUsage,
) -> Result<(), String> {
    let mut updated = summary.clone();
    if let Some(previous) = previous {
        contribution(&mut updated, previous, false)?;
    }
    contribution(&mut updated, next, true)?;
    updated.latest_usage = Some(next.clone());
    *summary = updated;
    Ok(())
}

fn adjust(value: &mut u64, amount: u64, add: bool) -> Result<(), String> {
    *value = if add {
        value.checked_add(amount)
    } else {
        value.checked_sub(amount)
    }
    .ok_or_else(|| "session usage arithmetic is inconsistent or exceeds capacity".to_owned())?;
    Ok(())
}

fn contribution(
    summary: &mut SessionUsageSummary,
    usage: &SessionTokenUsage,
    add: bool,
) -> Result<(), String> {
    adjust(&mut summary.observed_usage_count, 1, add)?;
    adjust(
        &mut summary.cumulative_metered_tokens,
        u64::from(usage.metered_total_tokens().unwrap_or_default()),
        add,
    )?;
    match &usage.cost {
        Some(SessionCostEstimate::Estimated {
            currency,
            total_micros,
            ..
        }) => {
            adjust(
                summary.totals_micros.entry(currency.clone()).or_default(),
                *total_micros,
                add,
            )?;
            adjust(&mut summary.estimated_usage_count, 1, add)?;
        }
        Some(SessionCostEstimate::Unavailable { .. }) => {
            adjust(&mut summary.unavailable_usage_count, 1, add)?;
        }
        None => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_cannot_reduce_a_recorded_nonterminal_cost() {
        let mut current = SessionTokenUsage {
            request_id: Some("request".into()),
            observation_id: Some("request:0".into()),
            cost: Some(SessionCostEstimate::Estimated {
                currency: "USD".into(),
                total_micros: 10,
                components: Vec::new(),
                source: "fixture".into(),
                revision: None,
            }),
            ..Default::default()
        };
        let mut next = current.clone();
        next.observation_ordinal = 1;
        next.cost = None;
        assert!(accepts(Some(&current), &next).is_err());
        current.terminal = true;
        next.cost = current.cost.clone();
        assert!(accepts(Some(&current), &next).is_err());
    }

    #[test]
    fn observations_are_idempotent_and_conflicts_fail_closed() {
        let initial = SessionTokenUsage {
            request_id: Some("request".into()),
            observation_id: Some("request:0".into()),
            ..SessionTokenUsage::default()
        };
        let mut final_usage = initial.clone();
        final_usage.observation_ordinal = 1;
        final_usage.terminal = true;
        assert_eq!(accepts(Some(&initial), &initial), Ok(false));
        assert_eq!(accepts(Some(&initial), &final_usage), Ok(true));
        assert_eq!(accepts(Some(&final_usage), &initial), Ok(false));
        let mut conflict = final_usage.clone();
        conflict.output_tokens = Some(1);
        assert!(accepts(Some(&final_usage), &conflict).is_err());
    }
}
