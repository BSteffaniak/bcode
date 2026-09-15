//! Structured fallback and explicit maintenance/export CLI.
use bcode_plugin_sdk::{StaticCliFuture, StaticCliOutcome, StaticCliRegistration};
use bcode_session_models::{SessionCostRange, SessionId, SessionUsageQuery};
use bcode_usage_models::{USAGE_VERSION, UsageModel, UsageQuery};
use clap::{CommandFactory, FromArgMatches, Parser};

#[derive(Debug, Parser)]
#[command(
    name = "usage",
    about = "Query estimated usage snapshots; explicit collection never reads canonical history"
)]
struct UsageCli {
    /// Explicitly collect all bounded accounting pages for this session.
    #[arg(long)]
    collect: Option<SessionId>,
    /// Inclusive UTC timestamp in milliseconds.
    #[arg(long, default_value_t = 0)]
    from_ms: u64,
    /// Exclusive UTC timestamp in milliseconds.
    #[arg(long)]
    to_ms: u64,
    /// Filter one session.
    #[arg(long)]
    session: Option<SessionId>,
    /// Filter provider IDs (repeatable).
    #[arg(long)]
    provider: Vec<String>,
    /// Exact provider/model identity (repeatable; split at the first slash).
    #[arg(long, value_parser = parse_model)]
    model: Vec<UsageModel>,
    /// Stream every report page as JSON Lines, or CSV with one header.
    /// Output is partial on error; only exit status zero means export completed.
    #[arg(long, conflicts_with = "collect")]
    all_pages: bool,
    /// Export CSV rather than JSON to stdout.
    #[arg(long)]
    csv: bool,
    /// Continue a report page using its next_after ordinal.
    #[arg(long, requires = "revision")]
    after: Option<u64>,
    /// Index revision required for continuation.
    #[arg(long)]
    revision: Option<u64>,
}

fn parse_model(value: &str) -> Result<UsageModel, String> {
    let (provider, model) = value
        .split_once('/')
        .ok_or("model must be provider/model")?;
    if provider.is_empty() || model.is_empty() || provider.len() > 4096 || model.len() > 4096 {
        return Err("model must contain nonempty bounded provider and model IDs".into());
    }
    Ok(UsageModel {
        provider: Some(provider.into()),
        model: Some(model.into()),
    })
}

pub fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        requires_daemon: true,
        command: UsageCli::command,
        invoke,
    }
}
fn write_page(
    report: &bcode_usage_models::UsageReport,
    csv: bool,
    stream: bool,
    first: bool,
) -> Result<(), String> {
    use std::io::Write;
    let output = if csv {
        let csv = bcode_usage::export_csv(report);
        if first {
            csv
        } else {
            csv.split_once("\r\n")
                .map_or(String::new(), |(_, rows)| rows.to_owned())
        }
    } else if stream {
        format!(
            "{}\n",
            serde_json::to_string(report).map_err(|_| "usage encoding failed")?
        )
    } else {
        format!(
            "{}\n",
            bcode_usage::export_json(report).map_err(|_| "usage encoding failed")?
        )
    };
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(output.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|_| "usage export output failed; export is incomplete".into())
}

fn invoke(matches: clap::ArgMatches) -> StaticCliFuture {
    Box::pin(async move {
        let args = UsageCli::from_arg_matches(&matches).map_err(|error| error.to_string())?;
        let client = bcode_client::BcodeClient::default_endpoint();
        let mut query = UsageQuery {
            version: USAGE_VERSION,
            range: SessionCostRange {
                from_timestamp_ms: args.from_ms,
                to_timestamp_ms: args.to_ms,
            },
            models: args.model.into_iter().collect(),
            providers: args.provider.into_iter().collect(),
            session_id: args.session,
            bucket_ms: args.to_ms.saturating_sub(args.from_ms).div_ceil(365).max(1),
            after: args.after,
            revision: args.revision,
            limit: 256,
        };
        query.validate()?;
        if let Some(session_id) = args.collect {
            let mut source = SessionUsageQuery {
                range: SessionCostRange {
                    from_timestamp_ms: 0,
                    to_timestamp_ms: i64::MAX.cast_unsigned(),
                },
                after: None,
                generation: None,
                limit: 256,
            };
            loop {
                let page = client
                    .collect_usage(session_id, source.clone())
                    .await
                    .map_err(|_| "collection unavailable or changed; retry explicitly")?;
                source.generation = Some(page.generation);
                source.after = page.next_after;
                if source.after.is_none() {
                    break;
                }
            }
        }
        let mut first_page = true;
        loop {
            let report = client.usage_report(query.clone()).await.map_err(
                |_| "usage query unavailable or changed; export is incomplete; restart query",
            )?;
            write_page(&report, args.csv, args.all_pages, first_page)?;
            if !args.all_pages {
                break;
            }
            let Some(after) = report.next_after else {
                break;
            };
            if after <= query.after.unwrap_or_default() {
                return Err("nonadvancing usage cursor; export is incomplete".into());
            }
            query.after = Some(after);
            query.revision = Some(report.revision);
            first_page = false;
        }
        Ok(StaticCliOutcome::default())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_filters_preserve_exact_identity() {
        let model = parse_model("provider/family/model:revision").unwrap();
        assert_eq!(model.provider.as_deref(), Some("provider"));
        assert_eq!(model.model.as_deref(), Some("family/model:revision"));
        for value in ["model", "/model", "provider/"] {
            assert!(parse_model(value).is_err());
        }
    }
    #[test]
    fn streaming_export_is_explicit_and_generation_fenced() {
        let matches = UsageCli::command()
            .try_get_matches_from([
                "usage",
                "--to-ms",
                "100",
                "--all-pages",
                "--csv",
                "--model",
                "p/a",
                "--model",
                "p/b",
            ])
            .unwrap();
        let args = UsageCli::from_arg_matches(&matches).unwrap();
        assert!(args.all_pages && args.csv);
        assert_eq!(args.model.len(), 2);
        assert!(
            UsageCli::command()
                .try_get_matches_from(["usage", "--to-ms", "100", "--after", "1"])
                .is_err()
        );
    }
}
