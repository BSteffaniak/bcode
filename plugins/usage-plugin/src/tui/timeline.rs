//! BMUX chart adaptation for bounded usage reports.
use bcode_usage_models::{UsageModel, UsageReport};
use bmux_tui::{
    component::{Component, Constraints, LayoutCx},
    geometry::{Rect, Size},
    paint::{LocalRect, PaintCx},
};
use bmux_tui_components::chart::{
    Chart, ChartAxes, ChartAxis, ChartBounds, ChartDataset, ChartPoint,
};

/// Paint independent bucket observations, never joining missing coverage with invented zeros.
pub(super) fn paint(report: &UsageReport, area: Rect, frame: &mut PaintCx<'_, '_>) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // Separate currencies rather than plotting incomparable amounts on one scale.
    let Some(currency) = report.totals.cost_micros.keys().next() else {
        return;
    };
    if report.totals.cost_micros.len() != 1 {
        return;
    }
    let Some(first) = report.buckets.first() else {
        return;
    };
    let origin = first.timestamp_ms;
    let series: Vec<_> = report
        .models
        .iter()
        .take(5)
        .map(|model| {
            let points = points(report, &model.model, currency, origin);
            let label = format!(
                "{}/{} ({currency})",
                model.model.provider.as_deref().unwrap_or("?"),
                model.model.model.as_deref().unwrap_or("?")
            );
            (label, points)
        })
        .collect();
    let datasets: Vec<_> = series
        .iter()
        .map(|(label, points)| ChartDataset::scatter(label, points))
        .collect();
    let x_max = series
        .iter()
        .flat_map(|(_, points)| points)
        .map(|point| point.x)
        .fold(1.0_f64, f64::max);
    let y_max = series
        .iter()
        .flat_map(|(_, points)| points)
        .map(|point| point.y)
        .fold(1.0_f64, f64::max);
    let chart = Chart::new(&datasets, ChartBounds::new(0.0, x_max, 0.0, y_max)).axes(
        ChartAxes::empty()
            .x(ChartAxis::empty().title("Page bucket observations; first 5 models"))
            .y(ChartAxis::empty().title(currency))
            .legend(true),
    );
    let layout = chart.layout(
        Constraints::tight(Size::new(area.width, area.height)),
        &mut LayoutCx::new(),
    );
    frame.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| chart.paint(&layout, cx),
    );
}

fn points(
    report: &UsageReport,
    model: &UsageModel,
    currency: &str,
    origin: u64,
) -> Vec<ChartPoint> {
    report
        .buckets
        .iter()
        .filter_map(|bucket| {
            let row = bucket.models.iter().find(|row| &row.model == model)?;
            let micros = *row.totals.cost_micros.get(currency)?;
            // Conversion is presentation-only; financial accounting remains integer micros.
            let amount = format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000)
                .parse::<f64>()
                .ok()?;
            let seconds = bucket
                .timestamp_ms
                .checked_sub(origin)?
                .to_string()
                .parse::<f64>()
                .ok()?
                / 1000.0;
            Some(ChartPoint::new(seconds, amount))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_usage_models::{USAGE_VERSION, UsageBucket, UsageModelRow, UsageTotals};
    #[test]
    fn chart_is_confined_at_tiny_sizes_and_unicode_labels() {
        use bmux_tui::{buffer::Buffer, frame::Frame};
        let identity = UsageModel {
            provider: Some("供給者".into()),
            model: Some("模型👩‍💻e\u{301}".into()),
        };
        let totals = UsageTotals {
            cost_micros: std::collections::BTreeMap::from([("USD".into(), 100)]),
            ..UsageTotals::default()
        };
        let report = UsageReport {
            revision: 1,
            version: USAGE_VERSION,
            totals: totals.clone(),
            models: vec![UsageModelRow {
                model: identity.clone(),
                totals: totals.clone(),
            }],
            requests: Vec::new(),
            next_after: None,
            coverage: "partial".into(),
            buckets: vec![UsageBucket {
                timestamp_ms: 1000,
                models: vec![UsageModelRow {
                    model: identity,
                    totals,
                }],
            }],
        };
        for width in [0, 1, 2, 8, 20] {
            for height in [0, 1, 2, 6] {
                let bounds = Rect::new(0, 0, 24, 10);
                let mut buffer = Buffer::empty(bounds);
                let area = Rect::new(2, 2, width, height);
                paint(
                    &report,
                    area,
                    &mut PaintCx::new(&mut Frame::new(&mut buffer)),
                );
                for y in 0..10 {
                    let row = buffer.row_symbols(y).unwrap();
                    if y < 2 || y >= 2 + height {
                        assert!(row.trim().is_empty());
                    }
                }
            }
        }
    }

    #[test]
    fn unknown_buckets_are_not_zero_and_currency_is_separate() {
        let model = UsageModel::default();
        let report = UsageReport {
            revision: 1,
            version: USAGE_VERSION,
            totals: UsageTotals::default(),
            models: Vec::new(),
            requests: Vec::new(),
            next_after: None,
            coverage: "partial".into(),
            buckets: vec![
                UsageBucket {
                    timestamp_ms: 1000,
                    models: vec![UsageModelRow {
                        model: model.clone(),
                        totals: UsageTotals {
                            cost_micros: std::collections::BTreeMap::from([(
                                "USD".into(),
                                1_500_000,
                            )]),
                            ..UsageTotals::default()
                        },
                    }],
                },
                UsageBucket {
                    timestamp_ms: 2000,
                    models: vec![UsageModelRow {
                        model: model.clone(),
                        totals: UsageTotals::default(),
                    }],
                },
            ],
        };
        assert_eq!(
            points(&report, &model, "USD", 1000),
            vec![ChartPoint::new(0.0, 1.5)]
        );
        assert!(points(&report, &model, "EUR", 1000).is_empty());
    }
}
