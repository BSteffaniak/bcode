//! Explicit, bounded target selection for the browser X-ray adapter.
//!
//! This module neither owns the gateway nor infers a session from the working directory.

use std::io::{BufRead, IsTerminal as _, Write as _};

use bcode_client::BcodeClient;
use bcode_session_models::{
    SessionCatalogQuery, SessionCatalogStatus, SessionDiscoveryScope, SessionId,
    SessionLocationSummary,
};
use clap::Args;

use crate::{CliError, xray_web};

const PAGE_SIZE: usize = 25;
const MAX_CHOICE_BYTES: u8 = 64;

/// Open the browser inspector for an explicit session or an explicitly chosen catalog entry.
#[derive(Debug, Args)]
pub struct XrayLaunchArgs {
    /// Session to observe. Omit to choose from bounded catalog pages.
    pub session_id: Option<SessionId>,
    /// Launch isolated synthetic evidence without a session or provider.
    #[arg(long, conflicts_with = "session_id", action = clap::ArgAction::SetTrue)]
    pub demo: Option<bool>,
    /// Print the local URL without opening a browser.
    #[arg(long)]
    pub no_open: bool,
    /// Open on serving host even through SSH.
    #[arg(long)]
    pub local_browser: bool,
    /// Explicitly authorize sensitive capture.
    #[arg(long, conflicts_with = "demo")]
    pub deep: bool,
    /// Observation grant lifetime, in seconds; does not limit session execution.
    #[arg(long, default_value_t = 3600, value_parser = clap::value_parser!(u64).range(1..=86400))]
    pub lifetime_seconds: u64,
}

pub async fn run(args: &XrayLaunchArgs) -> Result<(), CliError> {
    if args.demo.unwrap_or(false) {
        return xray_web::run_demo(&web_args(args, SessionId::default())).await;
    }
    let session_id = match args.session_id {
        Some(id) => id,
        None => match pick_session().await? {
            Some(id) => id,
            None => return Ok(()),
        },
    };
    // Discovery is not authority. Resolve canonical metadata again after selection,
    // including duplicate-location detection, before provisioning any observation grant.
    let client = BcodeClient::default_endpoint();
    let summary = client.session_summary(session_id).await?;
    validate_location(summary.location.as_ref())?;
    let owner = crate::session_read_client(session_id).await?;
    let status = owner.verified_server_status().await?;
    validate_identity(
        status.daemon.artifact_id.as_ref(),
        status.daemon.state_location_id.as_deref(),
    )?;
    xray_web::run(&web_args(args, session_id)).await
}

const fn web_args(args: &XrayLaunchArgs, session_id: SessionId) -> xray_web::XrayWebArgs {
    xray_web::XrayWebArgs {
        session_id,
        asset_dir: None,
        no_open: args.no_open,
        local_browser: args.local_browser,
        deep: args.deep,
        lifetime_seconds: args.lifetime_seconds,
    }
}

fn invalid(message: &str) -> CliError {
    CliError::InvalidArguments(message.to_owned())
}

fn validate_location(location: Option<&SessionLocationSummary>) -> Result<(), CliError> {
    match location {
        Some(location) if location.ambiguous => Err(invalid(
            "session has conflicting state-location claims; resolve them explicitly before opening X-ray",
        )),
        Some(location)
            if !location.primary || location.location_id != bcode_ipc::state_location_id() =>
        {
            Err(invalid(
                "session belongs to another state location; rerun with its explicit --state-root or --state-profile",
            ))
        }
        Some(_) | None => Ok(()),
    }
}

fn validate_identity(
    artifact: Option<&bcode_ipc::ArtifactId>,
    state_location: Option<&str>,
) -> Result<(), CliError> {
    if artifact != Some(&bcode_ipc::ArtifactId::current()) {
        return Err(invalid(
            "session owner targets another or unverifiable artifact; use its exact Bcode executable explicitly",
        ));
    }
    if state_location != Some(bcode_ipc::state_location_id().as_str()) {
        return Err(invalid(
            "session owner state location does not match the explicitly resolved launch location",
        ));
    }
    Ok(())
}

async fn pick_session() -> Result<Option<SessionId>, CliError> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(invalid(
            "bcode xray requires SESSION_ID without an interactive terminal",
        ));
    }
    let client = BcodeClient::default_endpoint();
    let mut cursor = None;
    loop {
        let page = client
            .list_sessions_page_with_scope(
                SessionDiscoveryScope::AllWorkingDirectories,
                SessionCatalogQuery::default(),
                cursor,
                PAGE_SIZE,
            )
            .await?;
        {
            let mut output = std::io::stderr().lock();
            writeln!(
                output,
                "Choose a session for read-only X-ray (no automatic selection):"
            )?;
            if !matches!(page.catalog_status, SessionCatalogStatus::Loaded)
                || page
                    .catalog_sources
                    .iter()
                    .any(|source| !matches!(source.status, SessionCatalogStatus::Loaded))
            {
                writeln!(
                    output,
                    "Catalog is incomplete or degraded; run session list for diagnostics."
                )?;
            }
            for (index, session) in page.sessions.iter().enumerate() {
                let location = if validate_location(session.location.as_ref()).is_ok() {
                    ""
                } else {
                    " (requires explicit location selection or conflict resolution)"
                };
                // IDs are sufficient for exact selection. Never print untrusted titles
                // or paths as terminal control text.
                writeln!(output, "{}. {}{location}", index + 1, session.id)?;
            }
            if page.sessions.is_empty() {
                writeln!(output, "No sessions in this catalog window.")?;
            }
            writeln!(
                output,
                "Enter a displayed number, n for next page, or q to cancel:"
            )?;
            output.flush()?;
        }
        let choice = read_choice(&mut std::io::stdin().lock())?;
        match parse_choice(&choice, page.sessions.len(), page.next_cursor.is_some())? {
            Choice::Select(index) => {
                let session = &page.sessions[index];
                validate_location(session.location.as_ref())?;
                return Ok(Some(session.id));
            }
            Choice::Next => cursor = page.next_cursor,
            Choice::Cancel => return Ok(None),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Choice {
    Select(usize),
    Next,
    Cancel,
}

fn read_choice(input: &mut impl BufRead) -> Result<String, CliError> {
    let mut line = String::new();
    std::io::Read::take(input, u64::from(MAX_CHOICE_BYTES) + 1).read_line(&mut line)?;
    if line.len() > usize::from(MAX_CHOICE_BYTES) {
        return Err(invalid("X-ray picker input exceeds 64 bytes"));
    }
    Ok(line)
}

fn parse_choice(input: &str, count: usize, has_next: bool) -> Result<Choice, CliError> {
    let input = input.trim();
    match input {
        "" | "q" => Ok(Choice::Cancel),
        "n" if has_next => Ok(Choice::Next),
        _ => input
            .parse::<usize>()
            .ok()
            .filter(|number| (1..=count).contains(number))
            .map(|number| Choice::Select(number - 1))
            .ok_or_else(|| {
                invalid("choose a displayed session number, an available next page, or q")
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Launch {
        #[command(flatten)]
        args: XrayLaunchArgs,
    }

    #[test]
    fn top_level_route_and_existing_web_alias_parse() {
        let id = "00000000-0000-0000-0000-000000000001";
        let cli = crate::Cli::try_parse_from(["bcode", "xray", id]).unwrap();
        assert!(matches!(cli.command, Some(crate::Commands::Xray(_))));
        let cli = crate::Cli::try_parse_from(["bcode", "session", "xray-web", id]).unwrap();
        assert!(matches!(
            cli.command,
            Some(crate::Commands::Session {
                command: crate::SessionCommand::XrayWeb(_)
            })
        ));
    }

    #[test]
    fn explicit_target_and_safe_gateway_defaults() {
        let id: SessionId = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let parsed = Launch::try_parse_from(["xray", &id.to_string(), "--no-open"]).unwrap();
        assert_eq!(parsed.args.session_id, Some(id));
        let adapted = web_args(&parsed.args, id);
        assert_eq!(adapted.session_id, id);
        assert!(adapted.asset_dir.is_none());
        assert!(!adapted.deep);
        assert!(adapted.no_open);
        assert_eq!(adapted.lifetime_seconds, 3600);
    }

    #[test]
    fn omitted_target_is_not_inferred_and_grant_is_bounded() {
        assert!(
            Launch::try_parse_from(["xray"])
                .unwrap()
                .args
                .session_id
                .is_none()
        );
        for value in ["0", "86401"] {
            assert!(Launch::try_parse_from(["xray", "--lifetime-seconds", value]).is_err());
        }
        assert!(
            Launch::try_parse_from(["xray", "--deep"])
                .unwrap()
                .args
                .deep
        );
    }

    #[test]
    fn picker_requires_a_displayed_choice_even_for_one_result() {
        assert_eq!(parse_choice("", 1, false).unwrap(), Choice::Cancel);
        assert_eq!(parse_choice("1\n", 1, false).unwrap(), Choice::Select(0));
        assert_eq!(
            parse_choice("25", PAGE_SIZE, true).unwrap(),
            Choice::Select(24)
        );
        for input in ["0", "2", "-1", "current", "n"] {
            assert!(parse_choice(input, 1, false).is_err());
        }
        assert_eq!(parse_choice("n", 0, true).unwrap(), Choice::Next);
        assert_eq!(parse_choice("q", 0, false).unwrap(), Choice::Cancel);
    }

    #[test]
    fn picker_input_is_bounded_and_eof_cancels() {
        let mut eof = std::io::Cursor::new(b"");
        assert_eq!(
            parse_choice(&read_choice(&mut eof).unwrap(), 1, false).unwrap(),
            Choice::Cancel
        );
        let mut oversized = std::io::Cursor::new(vec![b'1'; 1024]);
        assert!(read_choice(&mut oversized).is_err());
        assert_eq!(oversized.position(), u64::from(MAX_CHOICE_BYTES) + 1);
    }

    #[test]
    fn discovery_does_not_authorize_foreign_or_ambiguous_locations() {
        let mut location = SessionLocationSummary {
            primary: true,
            location_id: bcode_ipc::state_location_id(),
            ..SessionLocationSummary::default()
        };
        assert!(validate_location(Some(&location)).is_ok());
        location.ambiguous = true;
        assert!(validate_location(Some(&location)).is_err());
        location.ambiguous = false;
        location.primary = false;
        assert!(validate_location(Some(&location)).is_err());
        location.primary = true;
        location.location_id = "another-location".to_owned();
        assert!(validate_location(Some(&location)).is_err());
    }

    #[test]
    fn owner_identity_must_match_both_artifact_and_location() {
        let artifact = bcode_ipc::ArtifactId::current();
        let location = bcode_ipc::state_location_id();
        assert!(validate_identity(Some(&artifact), Some(&location)).is_ok());
        assert!(validate_identity(None, Some(&location)).is_err());
        assert!(validate_identity(Some(&artifact), None).is_err());
        assert!(validate_identity(Some(&artifact), Some("another-location")).is_err());
    }
}
