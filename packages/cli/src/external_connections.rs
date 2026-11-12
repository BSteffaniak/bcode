//! Secret-free CLI consent adapters for provider-owned external authentication.
//!
//! Discovery, source reads and durable grant mutation belong to provider-auth. These
//! adapters only present supplied metadata and return an explicit user decision.

use super::CliError;
use bcode_provider_auth_models::external::{
    ExternalAuthApplication as _, ExternalAuthCandidate, ExternalAuthGrant, ExternalAuthScope,
};
use std::io::{BufRead as _, IsTerminal as _, Read as _};

/// Discover and approve through the authenticated application boundary.
/// An omitted scope grants all local Bcode applications, after explicit review.
pub async fn connect_command(scope: Option<ExternalAuthScope>) -> Result<(), CliError> {
    if matches!(scope, Some(ExternalAuthScope::Application { .. })) {
        return Err(CliError::InvalidArguments(
            "Application-specific consent is unavailable until caller identity is authenticated; omit --application for explicitly reviewed local Bcode consent.".into(),
        ));
    }
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
    let Some((candidate, scope)) = connect(
        &candidates,
        scope.unwrap_or(ExternalAuthScope::AllLocalApplications),
    )?
    else {
        return Ok(());
    };
    client
        .approve_external_auth(candidate.clone(), Some(scope))
        .await
        .map_err(external_error)?;
    println!(
        "External connection approved in place. No credential was copied or account verified. Model selection is a separate optional step."
    );
    let mut shell = bcode_tui::TerminalShell::enter()?;
    let result = Box::pin(super::startup::select_external_model_in_shell(
        &mut shell, &candidate,
    ))
    .await;
    shell.finish()?;
    if result? {
        println!("Exact external binding and model saved for startup. No session was started.");
    } else {
        println!(
            "No model selection changed. The approval remains available to list or disconnect; explicit provider/model selections still take precedence."
        );
    }
    Ok(())
}

/// List non-secret approvals without discovering or reading external sources.
pub async fn list_command() -> Result<(), CliError> {
    list(
        &bcode_client::BcodeClient::default_endpoint()
            .list_external_auth()
            .await
            .map_err(external_error)?,
    )
}

/// Revoke one reviewed approval without reading or modifying its source.
pub async fn disconnect_command() -> Result<(), CliError> {
    require_terminal()?;
    let client = bcode_client::BcodeClient::default_endpoint();
    let grants = client.list_external_auth().await.map_err(external_error)?;
    if let Some(grant) = disconnect(&grants)? {
        let removed = client
            .revoke_external_auth(grant)
            .await
            .map_err(external_error)?;
        println!(
            "{} The source application was not modified.",
            if removed {
                "Bcode approval removed."
            } else {
                "Approval was already absent."
            }
        );
    }
    Ok(())
}

fn external_error(error: bcode_provider_auth_models::external::ExternalAuthError) -> CliError {
    CliError::LoginProfile(format!(
        "External connection operation failed ({error:?}). Existing state was preserved where possible; inspect connections before retrying. No source logout or provider fallback was attempted."
    ))
}

/// Review one exact source binding before the caller persists consent.
/// No source is read and no execution or model selection is changed here.
pub fn connect(
    candidates: &[ExternalAuthCandidate],
    scope: ExternalAuthScope,
) -> Result<Option<(ExternalAuthCandidate, ExternalAuthScope)>, CliError> {
    require_terminal()?;
    let mut output = std::io::stdout().lock();
    let mut input = std::io::stdin().lock();
    connect_with_io(candidates, scope, &mut input, &mut output)
}

fn connect_with_io(
    candidates: &[ExternalAuthCandidate],
    scope: ExternalAuthScope,
    input: &mut impl std::io::BufRead,
    output: &mut impl std::io::Write,
) -> Result<Option<(ExternalAuthCandidate, ExternalAuthScope)>, CliError> {
    let scope_label = scope_label(&scope)?;
    let available: Vec<_> = candidates.iter().filter(|item| item.available).collect();
    if available.is_empty() {
        writeln!(
            output,
            "No compatible external connections are available. Nothing was changed."
        )?;
        return Ok(None);
    }
    writeln!(
        output,
        "Reuse an external credential in place (no vault or credential copy)."
    )?;
    writeln!(output, "Consent scope: {scope_label}")?;
    writeln!(
        output,
        "Bcode-native execution is preferred only when supported safely. This does not launch a native agent, verify an account, or change your selected model."
    )?;
    for (index, item) in available.iter().enumerate() {
        writeln!(
            output,
            "{}. {} -> {} / {} / {}",
            index + 1,
            item.source.as_str(),
            item.target.provider_id,
            item.target.method_id,
            item.target.credential_id
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
        "Approve {} for {} ({scope_label}). The original application retains its credentials. Disconnecting only removes Bcode consent; it does not log out the source application.",
        candidate.source.as_str(),
        candidate.target.provider_id
    )?;
    write!(output, "Type connect to consent: ")?;
    output.flush()?;
    if !confirmed(input, "connect")? {
        return Ok(None);
    }
    Ok(Some((candidate.clone(), scope)))
}

/// Print approved bindings, not credential values or private source paths.
pub fn list(grants: &[ExternalAuthGrant]) -> Result<(), CliError> {
    list_with_io(grants, &mut std::io::stdout().lock())
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
            "{}. {} -> {} / {} / {} [{}]",
            index + 1,
            grant.source.as_str(),
            grant.target.provider_id,
            grant.target.method_id,
            grant.target.credential_id,
            scope_label(&grant.scope)?
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
        "Only Bcode's approval will be removed. The source application is not modified or logged out."
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

fn scope_label(scope: &ExternalAuthScope) -> Result<String, CliError> {
    match scope {
        ExternalAuthScope::AllLocalApplications => Ok("all local Bcode applications".into()),
        ExternalAuthScope::Application { application_id }
            if !application_id.trim().is_empty()
                && application_id.len() <= 256
                && !application_id.chars().any(char::is_control) =>
        {
            Ok(format!("local application {application_id}"))
        }
        ExternalAuthScope::Application { .. } => Err(CliError::InvalidArguments(
            "External connection application scope is invalid.".into(),
        )),
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
    use bcode_provider_auth_models::external::{ExternalAuthSourceKind, ExternalAuthTarget};

    fn candidate(available: bool) -> ExternalAuthCandidate {
        ExternalAuthCandidate {
            source: ExternalAuthSourceKind::Codex,
            binding_id: "fixture-binding".into(),
            target: ExternalAuthTarget {
                plugin_id: "fixture.plugin".into(),
                provider_id: "fixture.provider".into(),
                method_id: "api_key".into(),
                credential_id: "api_key".into(),
            },
            label: "fixture".into(),
            available,
        }
    }

    #[test]
    fn consent_is_exact_and_unavailable_sources_cannot_be_selected() {
        let candidates = [candidate(false), candidate(true)];
        let mut output = Vec::new();
        let result = connect_with_io(
            &candidates,
            ExternalAuthScope::AllLocalApplications,
            &mut &b"1\nconnect\n"[..],
            &mut output,
        )
        .unwrap()
        .unwrap();
        assert_eq!(result.0, candidates[1]);
        assert_eq!(result.1, ExternalAuthScope::AllLocalApplications);
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("no vault"));
        assert!(text.contains("all local Bcode applications"));
        assert!(
            connect_with_io(
                &candidates,
                result.1,
                &mut &b"1\nyes\n"[..],
                &mut Vec::new()
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn cancel_invalid_selection_and_invalid_scope_never_approve() {
        let candidates = [candidate(true)];
        assert!(
            connect_with_io(
                &candidates,
                ExternalAuthScope::AllLocalApplications,
                &mut &b"\n"[..],
                &mut Vec::new()
            )
            .unwrap()
            .is_none()
        );
        assert!(
            connect_with_io(
                &candidates,
                ExternalAuthScope::AllLocalApplications,
                &mut &b"0\nconnect\n"[..],
                &mut Vec::new()
            )
            .is_err()
        );
        assert!(
            connect_with_io(
                &candidates,
                ExternalAuthScope::Application {
                    application_id: "\n".into()
                },
                &mut &b"1\nconnect\n"[..],
                &mut Vec::new()
            )
            .is_err()
        );
    }

    #[test]
    fn disconnect_returns_only_the_selected_scope_and_requires_confirmation() {
        let candidate = candidate(true);
        let grant = ExternalAuthGrant {
            source: candidate.source,
            binding_id: candidate.binding_id,
            target: candidate.target,
            scope: ExternalAuthScope::Application {
                application_id: "fixture-app".into(),
            },
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
    fn oversized_answers_are_rejected_without_echoing_input() {
        let answer = "s".repeat(300);
        let error = read_answer(&mut answer.as_bytes()).unwrap_err().to_string();
        assert!(!error.contains(&answer));
    }
}
