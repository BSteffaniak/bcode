//! Structured fallback and explicit maintenance/export CLI.
use bcode_plugin_sdk::{StaticCliFuture, StaticCliOutcome, StaticCliRegistration};
use bcode_session_models::{SessionCostRange, SessionId, SessionUsageQuery};
use bcode_usage_models::{USAGE_VERSION, UsageQuery};
use clap::{CommandFactory, FromArgMatches, Parser};
use std::collections::BTreeSet;

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
    /// Export CSV rather than JSON to stdout; one explicitly requested page.
    #[arg(long)]
    csv: bool,
    /// Continue a report page using its next_after ordinal.
    #[arg(long, requires = "revision")]
    after: Option<u64>,
    /// Index revision required for continuation.
    #[arg(long)]
    revision: Option<u64>,
}

pub fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        requires_daemon: true,
        command: UsageCli::command,
        invoke,
    }
}
fn invoke(matches: clap::ArgMatches) -> StaticCliFuture {
    Box::pin(async move {
        let args = UsageCli::from_arg_matches(&matches).map_err(|error| error.to_string())?;
        let client = bcode_client::BcodeClient::default_endpoint();
        let query = UsageQuery {
            version: USAGE_VERSION,
            range: SessionCostRange {
                from_timestamp_ms: args.from_ms,
                to_timestamp_ms: args.to_ms,
            },
            models: BTreeSet::new(),
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
        let report = client
            .usage_report(query)
            .await
            .map_err(|_| "usage query unavailable or changed; restart query")?;
        if args.csv {
            print!("{}", bcode_usage::export_csv(&report));
        } else {
            println!(
                "{}",
                bcode_usage::export_json(&report).map_err(|_| "usage encoding failed")?
            );
        }
        Ok(StaticCliOutcome::default())
    })
}
