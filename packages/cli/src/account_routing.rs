//! Account-first CLI adapters for the shared application routing operations.

use bcode_client::BcodeClient;
use bcode_provider_auth_models::{
    AccountId, AccountPause, AccountRoute, AccountRoutingScope, RenameAccountRequest,
    SetAccountPauseRequest, SetAccountRoutingRequest,
};
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
    /// Inspect accounts and this session's next-turn and new-session routing as JSON.
    Status {
        #[arg(long)]
        session: SessionId,
    },
    /// Select an account without changing the model. Only this account is used unless fallback is explicit.
    Use {
        profile: String,
        #[arg(long)]
        session: SessionId,
        /// Owning provider plugin ID, not a pool name.
        #[arg(long)]
        provider: String,
        #[arg(long, value_enum, default_value = "session")]
        scope: Scope,
        /// Explicit fallback profiles in attempt order. Omit for account-only routing.
        #[arg(long, value_delimiter = ',')]
        fallback: Vec<String>,
    },
    /// Clear the override and follow configuration again.
    FollowConfiguration {
        #[arg(long)]
        session: SessionId,
        #[arg(long, value_enum, default_value = "session")]
        scope: Scope,
    },
    /// Exclude this account across pools and account-only routing in this configuration.
    Pause {
        profile: String,
        #[arg(long)]
        session: SessionId,
        #[arg(long)]
        provider: String,
        #[arg(long, value_enum, default_value = "manual")]
        duration: PauseDuration,
    },
    /// Resume a paused account; does not change routing or select it.
    Resume {
        profile: String,
        #[arg(long)]
        session: SessionId,
        #[arg(long)]
        provider: String,
    },
    /// Change a friendly label only. Omit the label to restore the profile name.
    Rename {
        profile: String,
        label: Option<String>,
        #[arg(long)]
        session: SessionId,
        #[arg(long)]
        provider: String,
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

pub async fn handle(command: AccountCommand) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    match command {
        AccountCommand::Status { session } => {
            crate::print_json(&client.account_routing(session).await?)
        }
        AccountCommand::Use {
            profile,
            session,
            provider,
            scope,
            fallback,
        } => {
            let request = SetAccountRoutingRequest {
                scope: scope.into(),
                route: route(&provider, profile, fallback),
            };
            crate::print_json(&client.set_account_routing(session, request).await?)
        }
        AccountCommand::FollowConfiguration { session, scope } => crate::print_json(
            &client
                .set_account_routing(
                    session,
                    SetAccountRoutingRequest {
                        scope: scope.into(),
                        route: AccountRoute::FollowConfiguration,
                    },
                )
                .await?,
        ),
        AccountCommand::Pause {
            profile,
            session,
            provider,
            duration,
        } => {
            let pause = match duration {
                PauseDuration::Manual => AccountPause::Manual,
                PauseDuration::OneHour => AccountPause::OneHour,
                PauseDuration::FourHours => AccountPause::FourHours,
            };
            crate::print_json(
                &client
                    .set_account_pause(
                        session,
                        SetAccountPauseRequest {
                            account: account(provider, profile),
                            pause,
                        },
                    )
                    .await?,
            )
        }
        AccountCommand::Resume {
            profile,
            session,
            provider,
        } => crate::print_json(
            &client
                .set_account_pause(
                    session,
                    SetAccountPauseRequest {
                        account: account(provider, profile),
                        pause: AccountPause::Resume,
                    },
                )
                .await?,
        ),
        AccountCommand::Rename {
            profile,
            label,
            session,
            provider,
        } => crate::print_json(
            &client
                .rename_account(
                    session,
                    RenameAccountRequest {
                        account: account(provider, profile),
                        label,
                    },
                )
                .await?,
        ),
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
