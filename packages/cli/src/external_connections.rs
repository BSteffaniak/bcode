//! CLI consent adapters for external credential connections.
//!
//! Discovery, source reads and durable approval belong to provider-auth behind the daemon.
//! These adapters only present secret-free metadata and return an explicit user decision.

use super::CliError;
use bcode_provider_auth_models::external::{
    ExternalAuthApplication as _, ExternalAuthCandidate, ExternalAuthConnection, ExternalAuthGrant,
    ExternalConnectionStatus,
};
use std::io::{BufRead as _, IsTerminal as _, Read as _, Write as _};

/// Discover, review and approve a connection through the daemon, then offer model selection.
pub async fn connect_command() -> Result<(), CliError> {
    require_terminal()?;
    let config = bcode_config::load_config()?;
    if !config
        .onboarding
        .credential_discovery_enabled(false, &bcode_config::ProcessConfigEnvironment)
    {
        println!("External credential discovery is disabled. No connection was changed.");
        return Ok(());
    }
    let client = bcode_client::BcodeClient::default_endpoint();
    let candidates = client
        .discover_external_auth()
        .await
        .map_err(external_error)?;
    let Some(candidate) = connect(&candidates)? else {
        return Ok(());
    };
    client
        .approve_external_auth(candidate.clone())
        .await
        .map_err(external_error)?;
    println!("Connection approved. Bcode will read it in place; nothing was copied.");
    match super::startup::save_recommended_external_model(&candidate)? {
        Some(model) => println!(
            "Bcode will start with {model} using {}. Change the model anytime with /model.",
            candidate.application
        ),
        None => println!(
            "Your explicit provider/model selection is unchanged; choose this connection with /model in a session."
        ),
    }
    Ok(())
}

/// List approvals with their current local status, optionally with usage windows.
pub async fn list_command(usage: bool) -> Result<(), CliError> {
    let connections = bcode_client::BcodeClient::default_endpoint()
        .external_auth_status()
        .await
        .map_err(external_error)?;
    status_with_io(&connections, &mut std::io::stdout().lock())?;
    if !usage {
        return Ok(());
    }
    let mut host = super::load_cli_plugin_host()?;
    let location = super::external_execution::ExecutionLocation::current();
    let mut output = std::io::stdout().lock();
    for connection in connections
        .iter()
        .filter(|connection| matches!(connection.status, ExternalConnectionStatus::Ready { .. }))
    {
        writeln!(output, "\n{} usage:", connection.grant.application)?;
        match super::external_execution::usage(&host, connection, &location) {
            Ok(report) => usage_with_io(&report, &mut output)?,
            Err(error) => writeln!(output, "  unavailable: {error}")?,
        }
    }
    host.deactivate_all()?;
    Ok(())
}

fn usage_with_io(
    report: &bcode_model::AuthUsageResponse,
    output: &mut impl std::io::Write,
) -> Result<(), CliError> {
    if !report.supported || report.meters.is_empty() {
        writeln!(
            output,
            "  not reported{}",
            report
                .degraded_reason
                .as_deref()
                .map(|reason| format!(" ({reason})"))
                .unwrap_or_default()
        )?;
        return Ok(());
    }
    for meter in &report.meters {
        let name = meter.meter_name.as_deref().unwrap_or(&meter.meter_id);
        for window in &meter.windows {
            let used = window.used_percent.map_or_else(
                || "usage unknown".to_owned(),
                |percent| format!("{percent}% used"),
            );
            let resets = window
                .resets_at_unix
                .map(|reset| format!(", resets at Unix {reset}"))
                .unwrap_or_default();
            writeln!(output, "  {name} {}: {used}{resets}", window.window_id)?;
        }
    }
    Ok(())
}

/// What a status means for the user, with the one action that fixes it.
#[must_use]
pub fn status_label(connection: &ExternalAuthConnection) -> String {
    let application = &connection.grant.application;
    match connection.status {
        ExternalConnectionStatus::Ready { .. } => "ready".into(),
        ExternalConnectionStatus::Expired => format!("expired — open {application} to renew it"),
        ExternalConnectionStatus::AccountChanged => {
            format!(
                "{application} is signed in to a different account — run `bcode auth connect` to approve it"
            )
        }
        ExternalConnectionStatus::SourceUnavailable => {
            format!("not found — sign in to {application} again")
        }
        ExternalConnectionStatus::DeclarationChanged => {
            "no longer offered by an enabled provider — run `bcode auth disconnect`".into()
        }
    }
}

fn status_with_io(
    connections: &[ExternalAuthConnection],
    output: &mut impl std::io::Write,
) -> Result<(), CliError> {
    if connections.is_empty() {
        writeln!(
            output,
            "No external connections approved. Run `bcode auth connect` to add one."
        )?;
    }
    for (index, connection) in connections.iter().enumerate() {
        writeln!(
            output,
            "{}. {} -> {} · {}",
            index + 1,
            connection.grant.application,
            describe(&connection.grant.target),
            status_label(connection)
        )?;
    }
    Ok(())
}

/// Revoke one reviewed approval without reading or modifying its source.
pub async fn disconnect_command() -> Result<(), CliError> {
    require_terminal()?;
    let client = bcode_client::BcodeClient::default_endpoint();
    let grants = client.list_external_auth().await.map_err(external_error)?;
    if let Some(grant) = disconnect(&grants)? {
        let removed = client
            .revoke_external_auth(grant.binding_id)
            .await
            .map_err(external_error)?;
        println!(
            "{} {} itself was not modified or signed out.",
            if removed {
                "Bcode approval removed."
            } else {
                "Approval was already absent."
            },
            grant.application
        );
    }
    Ok(())
}

fn external_error(error: bcode_provider_auth_models::external::ExternalAuthError) -> CliError {
    CliError::LoginProfile(format!("External connection failed: {error}."))
}

fn describe(target: &bcode_provider_auth_models::external::ExternalAuthTarget) -> String {
    format!("{} ({})", target.provider_id, target.method_id)
}

/// Review one discovered connection before the caller persists consent.
pub fn connect(
    candidates: &[ExternalAuthCandidate],
) -> Result<Option<ExternalAuthCandidate>, CliError> {
    require_terminal()?;
    let mut output = std::io::stdout().lock();
    let mut input = std::io::stdin().lock();
    connect_with_io(candidates, &mut input, &mut output)
}

fn connect_with_io(
    candidates: &[ExternalAuthCandidate],
    input: &mut impl std::io::BufRead,
    output: &mut impl std::io::Write,
) -> Result<Option<ExternalAuthCandidate>, CliError> {
    let available: Vec<_> = candidates.iter().filter(|item| item.available).collect();
    if available.is_empty() {
        writeln!(
            output,
            "No credentials from other coding agents were found. Nothing was changed."
        )?;
        return Ok(None);
    }
    writeln!(
        output,
        "Use credentials another coding agent already keeps on this computer. Bcode reads them in place when it makes requests; it never copies, refreshes, or modifies them."
    )?;
    for (index, item) in available.iter().enumerate() {
        writeln!(
            output,
            "{}. {} -> {}",
            index + 1,
            item.application,
            describe(&item.target)
        )?;
    }
    write!(output, "Choose a connection number (blank cancels): ")?;
    output.flush()?;
    let Some(index) = read_index(input, available.len())? else {
        return Ok(None);
    };
    let candidate = available[index];
    writeln!(
        output,
        "Allow every local Bcode application to use {} credentials for {}? Requests use that account's quota or billing. Disconnecting later only removes Bcode's approval.",
        candidate.application,
        describe(&candidate.target)
    )?;
    write!(output, "Type connect to approve: ")?;
    output.flush()?;
    if !confirmed(input, "connect")? {
        return Ok(None);
    }
    Ok(Some(candidate.clone()))
}

fn list_with_io(
    grants: &[ExternalAuthGrant],
    output: &mut impl std::io::Write,
) -> Result<(), CliError> {
    if grants.is_empty() {
        writeln!(output, "No external connections approved.")?;
    }
    for (index, grant) in grants.iter().enumerate() {
        writeln!(
            output,
            "{}. {} -> {}",
            index + 1,
            grant.application,
            describe(&grant.target)
        )?;
    }
    Ok(())
}

/// Return the exact approved binding to revoke after an explicit confirmation.
/// The caller owns durable revocation; this adapter never touches source credentials.
pub fn disconnect(grants: &[ExternalAuthGrant]) -> Result<Option<ExternalAuthGrant>, CliError> {
    require_terminal()?;
    disconnect_with_io(
        grants,
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
    )
}

fn disconnect_with_io(
    grants: &[ExternalAuthGrant],
    input: &mut impl std::io::BufRead,
    output: &mut impl std::io::Write,
) -> Result<Option<ExternalAuthGrant>, CliError> {
    list_with_io(grants, output)?;
    if grants.is_empty() {
        return Ok(None);
    }
    write!(output, "Connection number to disconnect (blank cancels): ")?;
    output.flush()?;
    let Some(index) = read_index(input, grants.len())? else {
        return Ok(None);
    };
    writeln!(
        output,
        "Only Bcode's approval will be removed. {} is not modified or signed out.",
        grants[index].application
    )?;
    write!(output, "Type disconnect to confirm: ")?;
    output.flush()?;
    Ok(confirmed(input, "disconnect")?.then(|| grants[index].clone()))
}

fn require_terminal() -> Result<(), CliError> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        Ok(())
    } else {
        Err(CliError::InvalidArguments("External connection consent requires an interactive terminal; no approval was changed.".into()))
    }
}

// Bound input before parsing so an accidental paste cannot retain an unbounded
// credential-bearing line. Never include supplied input in errors.
fn read_answer(input: &mut impl std::io::BufRead) -> Result<String, CliError> {
    let mut answer = String::new();
    (&mut *input).take(258).read_line(&mut answer)?;
    if answer.len() > 256 || (!answer.ends_with('\n') && !input.fill_buf()?.is_empty()) {
        return Err(CliError::InvalidArguments(
            "Consent answer is too long; nothing was changed.".into(),
        ));
    }
    Ok(answer.trim().to_owned())
}

fn confirmed(input: &mut impl std::io::BufRead, expected: &str) -> Result<bool, CliError> {
    Ok(read_answer(input)? == expected)
}

fn read_index(input: &mut impl std::io::BufRead, count: usize) -> Result<Option<usize>, CliError> {
    let answer = read_answer(input)?;
    if answer.is_empty() {
        return Ok(None);
    }
    answer
        .parse::<usize>()
        .ok()
        .and_then(|number| number.checked_sub(1))
        .filter(|index| *index < count)
        .map(Some)
        .ok_or_else(|| {
            CliError::InvalidArguments(
                "Connection number is unavailable; nothing was changed.".into(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_provider_auth_models::external::ExternalAuthTarget;

    fn candidate(available: bool) -> ExternalAuthCandidate {
        ExternalAuthCandidate {
            binding_id: "fixture-binding".into(),
            target: ExternalAuthTarget {
                plugin_id: "fixture.plugin".into(),
                provider_id: "fixture.provider".into(),
                method_id: "api_key".into(),
                source_id: "fixture-source".into(),
            },
            application: "Fixture".into(),
            available,
        }
    }

    #[test]
    fn consent_is_exact_and_unavailable_sources_cannot_be_selected() {
        let candidates = [candidate(false), candidate(true)];
        let mut output = Vec::new();
        let result = connect_with_io(&candidates, &mut &b"1\nconnect\n"[..], &mut output)
            .unwrap()
            .unwrap();
        assert_eq!(result, candidates[1]);
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("never copies"));
        assert!(text.contains("every local Bcode application"));
        assert!(
            connect_with_io(&candidates, &mut &b"1\nyes\n"[..], &mut Vec::new())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cancel_and_invalid_selection_never_approve() {
        let candidates = [candidate(true)];
        assert!(
            connect_with_io(&candidates, &mut &b"\n"[..], &mut Vec::new())
                .unwrap()
                .is_none()
        );
        assert!(connect_with_io(&candidates, &mut &b"0\nconnect\n"[..], &mut Vec::new()).is_err());
    }

    #[test]
    fn disconnect_returns_only_the_selected_grant_and_requires_confirmation() {
        let candidate = candidate(true);
        let grant = ExternalAuthGrant {
            binding_id: candidate.binding_id,
            target: candidate.target,
            application: candidate.application,
            account_fingerprint: None,
            approved_at_unix: 1,
        };
        let grants = [grant.clone()];
        assert!(
            disconnect_with_io(&grants, &mut &b"1\nyes\n"[..], &mut Vec::new())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            disconnect_with_io(&grants, &mut &b"1\ndisconnect\n"[..], &mut Vec::new()).unwrap(),
            Some(grant)
        );
    }

    #[test]
    fn usage_lists_each_window_and_explains_missing_reports() {
        let report = bcode_model::AuthUsageResponse {
            supported: true,
            meters: vec![bcode_model::AuthUsageMeterSnapshot {
                meter_id: "codex".into(),
                meter_name: Some("Codex".into()),
                windows: vec![bcode_model::AuthUsageWindowSnapshot {
                    window_id: "5h".into(),
                    used_percent: Some(42),
                    resets_at_unix: Some(100),
                    ..Default::default()
                }],
            }],
            ..Default::default()
        };
        let mut output = Vec::new();
        usage_with_io(&report, &mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "  Codex 5h: 42% used, resets at Unix 100\n"
        );
        let mut output = Vec::new();
        usage_with_io(
            &bcode_model::AuthUsageResponse {
                degraded_reason: Some("not a subscription".into()),
                ..Default::default()
            },
            &mut output,
        )
        .unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("not reported (not a subscription)")
        );
    }

    #[test]
    fn oversized_answers_are_rejected_without_echoing_input() {
        let answer = "s".repeat(300);
        let error = read_answer(&mut answer.as_bytes()).unwrap_err().to_string();
        assert!(!error.contains(&answer));
    }
}
