#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Feedback command line shared by `bcode feedback` and the standalone `bcode-feedback`.
//!
//! Every command goes through the daemon's `bcode.feedback/v1` service, acting for the
//! person. `submit` records the item together with the decision to triage it, so neither is
//! lost when triage cannot run now, then delivers it to the single intake session as a
//! follow-up turn whose context blocks carry the item id, application and facets. What cannot
//! be delivered now stays queued and the daemon delivers it later; `show` and `inbox` say
//! where each item's triage stands, and `triage` asks again for a blocked one.
//!
//! Applications that do not link Bcode shell out to `bcode-feedback submit`, passing a
//! [`RecordFeedbackRequest`] as JSON on stdin with `--request-json`.

mod render;

use std::collections::BTreeSet;
use std::io::Read as _;
use std::time::Duration;

use bcode_client::BcodeClient;
use bcode_feedback_client as feedback;
use bcode_feedback_models::{
    Actor, Facet, FeedbackId, FeedbackItem, FeedbackQuery, FeedbackStatus, GetFeedbackRequest,
    LinkFeedbackRequest, LinkKind, RecordFeedbackRequest, RecordForTriageRequest,
    SetFeedbackStatusRequest, TriageDispatchState, TriageIntent, TriageState,
};
use clap::{Args, Parser, Subcommand, ValueEnum};

/// Collect, search and resolve feedback about any application.
#[derive(Debug, Parser)]
#[command(name = "feedback")]
pub struct FeedbackCli {
    /// What to do.
    #[command(subcommand)]
    pub command: FeedbackCommand,
}

/// Feedback subcommands.
#[derive(Debug, Subcommand)]
pub enum FeedbackCommand {
    /// Record feedback and hand it to the intake agent for triage.
    Submit(SubmitArgs),
    /// List feedback, newest first.
    List(ListArgs),
    /// Show one item.
    Show {
        /// Item id, such as fb-12.
        id: FeedbackId,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Items an agent resolved that await your verification, and items with open questions.
    Inbox {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Confirm an item is fixed.
    Verify(StatusArgs),
    /// Decide not to act on an item.
    Dismiss(StatusArgs),
    /// Reopen an item.
    Reopen(StatusArgs),
    /// Link one item to another.
    Link {
        /// Source item.
        from: FeedbackId,
        /// Target item.
        to: FeedbackId,
        /// Relationship from source to target.
        #[arg(long, value_enum, default_value = "related")]
        kind: LinkKindArg,
    },
    /// Send untriaged items to the intake agent: again when triage is blocked, or for the first
    /// time when they were recorded without it.
    Triage {
        /// Item ids, such as fb-12.
        #[arg(required = true)]
        ids: Vec<FeedbackId>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the intake session id, creating the session if needed.
    Session,
}

/// Arguments of `feedback submit`.
#[derive(Debug, Args)]
pub struct SubmitArgs {
    /// Application the feedback is about, such as bdata or bmux.
    #[arg(long, required_unless_present = "request_json")]
    pub app: Option<String>,
    /// Context as KEY=VALUE, such as bdata.connection=prod (repeatable).
    #[arg(long = "facet", value_name = "KEY=VALUE", value_parser = parse_facet)]
    pub facets: Vec<(String, String)>,
    /// Context the model must not read, as KEY=VALUE (repeatable).
    #[arg(long = "sensitive-facet", value_name = "KEY=VALUE", value_parser = parse_facet)]
    pub sensitive_facets: Vec<(String, String)>,
    /// Read the whole request as JSON from stdin: {"app", "text", "facets": [{"key", "value", "sensitive"}]}.
    #[arg(long, conflicts_with_all = ["app", "facets", "sensitive_facets", "text"])]
    pub request_json: bool,
    /// Record only; do not send the item to the intake agent.
    #[arg(long, conflicts_with = "wait")]
    pub no_triage: bool,
    /// Wait for the intake agent and print its reply, for up to SECS seconds (default 600).
    #[arg(
        long,
        value_name = "SECS",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "600"
    )]
    pub wait: Option<u64>,
    /// Print JSON.
    #[arg(long)]
    pub json: bool,
    /// The feedback; read from stdin when absent.
    #[arg(trailing_var_arg = true)]
    pub text: Vec<String>,
}

/// Arguments of `feedback list`.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Only this application.
    #[arg(long)]
    pub app: Option<String>,
    /// Only these statuses (repeatable).
    #[arg(long = "status", value_enum)]
    pub statuses: Vec<StatusArg>,
    /// Only items not yet triaged.
    #[arg(long)]
    pub pending: bool,
    /// Only this category.
    #[arg(long)]
    pub category: Option<String>,
    /// Only items with every one of these tags (repeatable).
    #[arg(long = "tag")]
    pub tags: Vec<String>,
    /// Only items with these exact facets, as KEY=VALUE (repeatable).
    #[arg(long = "facet", value_name = "KEY=VALUE", value_parser = parse_facet)]
    pub facets: Vec<(String, String)>,
    /// Case-insensitive text in the feedback, title or summary.
    #[arg(long)]
    pub search: Option<String>,
    /// Continue after this id, from a previous page.
    #[arg(long)]
    pub before: Option<FeedbackId>,
    /// Maximum items.
    #[arg(long, default_value_t = 25)]
    pub limit: usize,
    /// Print JSON.
    #[arg(long)]
    pub json: bool,
}

/// Arguments of status changes.
#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Item id, such as fb-12.
    pub id: FeedbackId,
    /// Why.
    #[arg(long)]
    pub note: Option<String>,
}

/// Status filter values.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum StatusArg {
    /// Not yet resolved.
    Open,
    /// Resolved by an agent, awaiting verification.
    AgentResolved,
    /// Verified by a person.
    Verified,
    /// Dismissed by a person.
    Dismissed,
}

impl From<StatusArg> for FeedbackStatus {
    fn from(value: StatusArg) -> Self {
        match value {
            StatusArg::Open => Self::Open,
            StatusArg::AgentResolved => Self::AgentResolved,
            StatusArg::Verified => Self::Verified,
            StatusArg::Dismissed => Self::Dismissed,
        }
    }
}

/// Link kinds.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum LinkKindArg {
    /// Same problem.
    Duplicate,
    /// Shared area or likely fix.
    Related,
    /// Consequence of the target.
    CausedBy,
}

impl From<LinkKindArg> for LinkKind {
    fn from(value: LinkKindArg) -> Self {
        match value {
            LinkKindArg::Duplicate => Self::Duplicate,
            LinkKindArg::Related => Self::Related,
            LinkKindArg::CausedBy => Self::CausedBy,
        }
    }
}

fn parse_facet(value: &str) -> Result<(String, String), String> {
    let (key, value) = value.split_once('=').ok_or("facets are KEY=VALUE")?;
    Ok((key.trim().to_owned(), value.to_owned()))
}

/// Run one command through `client`.
///
/// `bcode feedback` passes the client of its own daemon; `bcode-feedback` passes the
/// installed Bcode's ([`BcodeClient::for_installed_bcode`]).
///
/// # Errors
/// Returns a printable message when the daemon, the feedback service, or the intake session
/// cannot complete the command.
pub async fn run(cli: FeedbackCli, client: &BcodeClient) -> Result<(), String> {
    match cli.command {
        FeedbackCommand::Submit(args) => submit(client, args).await,
        FeedbackCommand::List(args) => {
            let json = args.json;
            let page = feedback::list(client, &list_query(args))
                .await
                .map_err(text)?;
            render::page(&page, json)
        }
        FeedbackCommand::Show { id, json } => {
            let item = feedback::get(client, &GetFeedbackRequest { id })
                .await
                .map_err(text)?;
            render::item(&item, json)
        }
        FeedbackCommand::Inbox { json } => inbox(client, json).await,
        FeedbackCommand::Verify(args) => set_status(client, args, FeedbackStatus::Verified).await,
        FeedbackCommand::Dismiss(args) => set_status(client, args, FeedbackStatus::Dismissed).await,
        FeedbackCommand::Reopen(args) => set_status(client, args, FeedbackStatus::Open).await,
        FeedbackCommand::Link { from, to, kind } => {
            let item = feedback::link(
                client,
                &LinkFeedbackRequest {
                    from,
                    to,
                    kind: kind.into(),
                },
            )
            .await
            .map_err(text)?;
            render::item(&item, false)
        }
        FeedbackCommand::Triage { ids, json } => retriage(client, &ids, json).await,
        FeedbackCommand::Session => {
            let session = feedback::ensure_intake_session(client)
                .await
                .map_err(text)?;
            println!("{session}");
            Ok(())
        }
    }
}

async fn submit(client: &BcodeClient, args: SubmitArgs) -> Result<(), String> {
    let request = if args.request_json {
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .map_err(|error| format!("reading stdin: {error}"))?;
        serde_json::from_str::<RecordFeedbackRequest>(&input)
            .map_err(|error| format!("invalid request JSON: {error}"))?
    } else {
        let text = if args.text.is_empty() {
            let mut input = String::new();
            std::io::stdin()
                .read_to_string(&mut input)
                .map_err(|error| format!("reading stdin: {error}"))?;
            input
        } else {
            args.text.join(" ")
        };
        RecordFeedbackRequest {
            app: args.app.unwrap_or_default(),
            text: text.trim().to_owned(),
            facets: args
                .facets
                .into_iter()
                .map(|(key, value)| Facet::new(key, value))
                .chain(args.sensitive_facets.into_iter().map(|(key, value)| Facet {
                    key,
                    value,
                    sensitive: true,
                }))
                .collect(),
        }
    };
    let triage = if args.no_triage {
        TriageIntent::Skipped
    } else {
        TriageIntent::Requested { instructions: None }
    };
    let item = feedback::record_for_triage(
        client,
        &RecordForTriageRequest {
            item: request,
            triage,
        },
    )
    .await
    .map_err(text)?;
    if args.no_triage {
        return render::submitted(&item, None, args.json);
    }
    // The item and its triage request are saved: a failed delivery is retried by the daemon.
    let (item, reply) = deliver(client, item, args.wait.map(Duration::from_secs)).await;
    render::submitted(&item, reply.as_deref(), args.json)
}

/// Deliver a queued item to the intake session now, waiting for the reply when asked.
///
/// Returns the item as delivery left it; a delivery failure is recorded on the item.
async fn deliver(
    client: &BcodeClient,
    item: FeedbackItem,
    wait: Option<Duration>,
) -> (FeedbackItem, Option<String>) {
    // Watch before delivering so the turn's outcome cannot be missed.
    let mut watcher = match wait {
        Some(_) => match feedback::ensure_intake_session(client).await {
            Ok(session_id) => client.watch_session(session_id, 0).await.ok(),
            Err(_) => None,
        },
        None => None,
    };
    let holder = feedback::holder("cli");
    let delivery = match feedback::deliver(client, item.id, &holder, None).await {
        Ok(delivery) => delivery,
        Err(error) => {
            eprintln!("{}: triage stays queued: {error}", item.id);
            return (item, None);
        }
    };
    let reply = match (wait, watcher.as_mut(), &delivery.intake) {
        (Some(timeout), Some(watcher), Some(receipt)) => {
            match feedback::wait_for_reply(client, watcher, receipt, timeout).await {
                Ok(reply) => Some(reply),
                Err(error) => {
                    eprintln!("{}: {error}", item.id);
                    None
                }
            }
        }
        _ => None,
    };
    (delivery.item, reply)
}

/// `feedback triage`: queue each item again and deliver it now.
async fn retriage(client: &BcodeClient, ids: &[FeedbackId], json: bool) -> Result<(), String> {
    let mut items = Vec::with_capacity(ids.len());
    let mut failed = Vec::new();
    for id in ids {
        match feedback::request_triage(client, *id).await {
            Ok(item) => items.push(deliver(client, item, None).await.0),
            Err(error) => failed.push(format!("{id}: {error}")),
        }
    }
    render::triaged(&items, json)?;
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed.join("\n"))
    }
}

fn list_query(args: ListArgs) -> FeedbackQuery {
    FeedbackQuery {
        app: args.app,
        statuses: args.statuses.into_iter().map(Into::into).collect(),
        triage_state: args.pending.then_some(TriageState::Pending),
        category: args.category,
        tags: args.tags.into_iter().collect(),
        facets: args.facets,
        text: args.search,
        before: args.before,
        limit: args.limit,
    }
}

async fn inbox(client: &BcodeClient, json: bool) -> Result<(), String> {
    let resolved = feedback::list(
        client,
        &FeedbackQuery {
            statuses: BTreeSet::from([FeedbackStatus::AgentResolved]),
            limit: bcode_feedback_models::MAX_PAGE_LIMIT,
            ..FeedbackQuery::default()
        },
    )
    .await
    .map_err(text)?;
    let open = feedback::list(
        client,
        &FeedbackQuery {
            statuses: BTreeSet::from([FeedbackStatus::Open]),
            triage_state: Some(TriageState::Triaged),
            limit: bcode_feedback_models::MAX_PAGE_LIMIT,
            ..FeedbackQuery::default()
        },
    )
    .await
    .map_err(text)?;
    let questions: Vec<FeedbackItem> = open
        .items
        .into_iter()
        .filter(|item| {
            item.triage
                .as_ref()
                .is_some_and(|triage| !triage.open_questions.is_empty())
        })
        .collect();
    let untriaged = feedback::list(
        client,
        &FeedbackQuery {
            statuses: BTreeSet::from([FeedbackStatus::Open]),
            triage_state: Some(TriageState::Pending),
            limit: bcode_feedback_models::MAX_PAGE_LIMIT,
            ..FeedbackQuery::default()
        },
    )
    .await
    .map_err(text)?;
    let stalled: Vec<FeedbackItem> = untriaged
        .items
        .into_iter()
        .filter(needs_attention)
        .collect();
    render::inbox(&resolved.items, &questions, &stalled, json)
}

/// An untriaged item whose triage is blocked, retrying after failures, or was never asked for.
fn needs_attention(item: &FeedbackItem) -> bool {
    item.triage_dispatch.as_ref().is_none_or(|dispatch| {
        dispatch.state == TriageDispatchState::Blocked || dispatch.is_retrying()
    })
}

async fn set_status(
    client: &BcodeClient,
    args: StatusArgs,
    status: FeedbackStatus,
) -> Result<(), String> {
    let item = feedback::set_status(
        client,
        &SetFeedbackStatusRequest {
            id: args.id,
            status,
            actor: Actor::User,
            note: args.note,
            session_id: None,
            commits: Vec::new(),
        },
    )
    .await
    .map_err(text)?;
    render::item(&item, false)
}

#[allow(clippy::needless_pass_by_value)] // Used as a `map_err` adapter.
fn text(error: feedback::FeedbackClientError) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests;
