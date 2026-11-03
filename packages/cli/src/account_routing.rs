//! Account-first CLI adapters for the shared application routing operations.

mod guided;

use bcode_client::BcodeClient;
use bcode_provider_auth_models::{AccountId, AccountRoute, AccountRoutingScope};
use bcode_session_models::SessionId;
use clap::{Subcommand, ValueEnum};

use crate::CliError;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Scope {
    Session,
    NewSessions,
    Both,
}

impl From<Scope> for AccountRoutingScope {
    fn from(scope: Scope) -> Self {
        match scope {
            Scope::Session => Self::Session,
            Scope::NewSessions => Self::NewSessions,
            Scope::Both => Self::Both,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PauseDuration {
    Manual,
    OneHour,
    FourHours,
}

#[derive(Debug, Subcommand)]
pub enum AccountCommand {
    /// Open the guided account-routing menu.
    Manage,
    /// Remove a saved account through the reviewed account lifecycle flow.
    Remove {
        #[arg(long)]
        provider: Option<String>,
        profile: Option<String>,
        #[arg(long, conflicts_with = "profile")]
        all: bool,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        revoke: bool,
    },
    /// Inspect accounts and this session's next-turn and new-session routing as JSON.
    Status {
        #[arg(long)]
        session: Option<SessionId>,
    },
    /// Select an account without changing the model. Fallback is always explicit.
    Use {
        profile: Option<String>,
        #[arg(long)]
        session: Option<SessionId>,
        /// Owning provider plugin ID, not a pool name.
        #[arg(long)]
        provider: Option<String>,
        /// Defaults to session for fully specified commands.
        #[arg(long, value_enum)]
        scope: Option<Scope>,
        #[arg(long, value_delimiter = ',')]
        fallback: Vec<String>,
    },
    /// Clear the override and follow configuration again.
    FollowConfiguration {
        #[arg(long)]
        session: Option<SessionId>,
        /// Defaults to session for fully specified commands.
        #[arg(long, value_enum)]
        scope: Option<Scope>,
    },
    /// Exclude this account across pools and account-only routing in this configuration.
    Pause {
        profile: Option<String>,
        #[arg(long)]
        session: Option<SessionId>,
        #[arg(long)]
        provider: Option<String>,
        /// Defaults to manual for fully specified commands.
        #[arg(long, value_enum)]
        duration: Option<PauseDuration>,
    },
    /// Resume a paused account; does not change routing or select it.
    Resume {
        profile: Option<String>,
        #[arg(long)]
        session: Option<SessionId>,
        #[arg(long)]
        provider: Option<String>,
    },
    /// Change a friendly label only. Omit the label in an explicit command to restore the profile name.
    Rename {
        profile: Option<String>,
        label: Option<String>,
        #[arg(long)]
        session: Option<SessionId>,
        #[arg(long)]
        provider: Option<String>,
    },
}

const fn account(provider: String, profile: String) -> AccountId {
    AccountId {
        provider_id: provider,
        profile,
    }
}

fn route(provider: &str, profile: String, fallback: Vec<String>) -> AccountRoute {
    let selected = account(provider.to_owned(), profile);
    if fallback.is_empty() {
        AccountRoute::Only { account: selected }
    } else {
        AccountRoute::Prefer {
            account: selected,
            alternatives: fallback
                .into_iter()
                .map(|profile| account(provider.to_owned(), profile))
                .collect(),
        }
    }
}

/// Open the guided account-routing menu.
///
/// # Errors
/// Returns application, terminal, or missing-interactive-input errors.
pub async fn manage() -> Result<(), CliError> {
    handle(AccountCommand::Manage).await
}

pub async fn handle(command: AccountCommand) -> Result<(), CliError> {
    if let AccountCommand::Remove {
        provider,
        profile,
        all,
        yes,
        revoke,
    } = command
    {
        return super::auth_management::lifecycle(super::auth_management::LifecycleOptions {
            provider,
            profile,
            all,
            yes,
            revoke,
            action: bcode_provider_auth_models::account_lifecycle::AccountLifecycleAction::Remove,
        })
        .await;
    }
    let client = BcodeClient::default_endpoint();
    let Some(prepared) = guided::complete(&client, command).await? else {
        return Ok(());
    };
    match prepared {
        guided::Prepared::Status(session) => {
            crate::print_json(&client.account_routing(session).await?)
        }
        guided::Prepared::Routing(session, request) => {
            crate::print_json(&client.set_account_routing(session, request).await?)
        }
        guided::Prepared::Pause(session, request) => {
            crate::print_json(&client.set_account_pause(session, request).await?)
        }
        guided::Prepared::Rename(session, request) => {
            crate::print_json(&client.rename_account(session, request).await?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_is_exclusive_unless_fallback_is_explicit() {
        assert_eq!(
            route("provider", "work".into(), vec![]),
            AccountRoute::Only {
                account: account("provider".into(), "work".into())
            }
        );
        assert_eq!(
            route(
                "provider",
                "work".into(),
                vec!["personal".into(), "other".into()]
            ),
            AccountRoute::Prefer {
                account: account("provider".into(), "work".into()),
                alternatives: vec![
                    account("provider".into(), "personal".into()),
                    account("provider".into(), "other".into())
                ],
            }
        );
    }
}
