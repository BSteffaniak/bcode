//! Guided account management. Domain decisions and mutations use application operations.

use bcode_client::{AccountLifecycleClient, BcodeClient};
use bcode_provider_auth_models::{
    AuthMethodContribution,
    account_lifecycle::{AccountLifecycleAction, AccountLifecycleEffect, AccountLifecycleRequest},
    application::{
        AccountLifecycleCatalog, AccountLifecyclePlan, AccountLifecycleProvider,
        AccountLifecycleRevocation,
    },
};

use super::{CliError, auth_prompts};

type Terminal = auth_prompts::Prompter<std::io::Stdin, std::io::Stderr>;

/// Explicit lifecycle selection and confirmation policy.
pub struct LifecycleOptions {
    /// Registered provider ID, or a guided selection.
    pub provider: Option<String>,
    /// Owned profile name, or a guided selection.
    pub profile: Option<String>,
    /// Select every account belonging to the selected provider.
    pub all: bool,
    /// Confirm the explicit operation without a review prompt.
    pub yes: bool,
    /// Explicitly request provider-owned remote revocation.
    pub revoke: bool,
    /// Local lifecycle action, independent of remote revocation.
    pub action: AccountLifecycleAction,
}

impl LifecycleOptions {
    /// Start a guided lifecycle action with no accounts preselected.
    #[must_use]
    pub const fn guided(action: AccountLifecycleAction) -> Self {
        Self {
            provider: None,
            profile: None,
            all: false,
            yes: false,
            revoke: false,
            action,
        }
    }

    fn validate(&self, interactive: bool) -> Result<(), CliError> {
        if self.all && self.profile.is_some() {
            return Err(CliError::LoginProfile(
                "Choose a profile or --all, not both.".to_owned(),
            ));
        }
        if !interactive
            && (!self.yes || self.provider.is_none() || (self.profile.is_none() && !self.all))
        {
            return Err(CliError::LoginProfile("Noninteractive sign-out/removal requires a provider, --profile <name> or --all, and --yes.".to_owned()));
        }
        Ok(())
    }
}

pub async fn manage() -> Result<(), CliError> {
    let mut prompts = auth_prompts::terminal();
    loop {
        let labels = [
            "Sign in / add account",
            "View accounts and authentication status",
            "Sign out — keep accounts",
            "Remove accounts from this computer",
            "Manage account selection, labels, and pause state",
            "Manage pool preferences",
            "Done",
        ]
        .map(str::to_owned);
        let selected = prompts.choose(
            "Authentication",
            &labels,
            "Use an explicit auth subcommand when not in a terminal.",
        )?;
        let result = match selected {
            0 => login(None, None).await,
            1 => inspect().await,
            2 => lifecycle(LifecycleOptions::guided(AccountLifecycleAction::Logout)).await,
            3 => lifecycle(LifecycleOptions::guided(AccountLifecycleAction::Remove)).await,
            4 => super::account_routing::manage().await,
            5 => pool_preferences().await,
            _ => return Ok(()),
        };
        if let Err(error) = result {
            // Returning to the menu does not undo partial effects.
            eprintln!("{error}");
        }
    }
}

async fn login(provider: Option<String>, profile: Option<String>) -> Result<(), CliError> {
    Box::pin(super::handle_auth_command(super::AuthCommand::Login {
        provider,
        profile,
        vault: None,
        recipient_key: None,
        no_device_seal: false,
        pool: None,
        standalone: false,
        method: None,
        verify: false,
    }))
    .await
}

pub async fn pool_preferences() -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let mut prompts = auth_prompts::terminal();
    loop {
        let pools = client.auth_pool_list().await?;
        let labels = pools
            .iter()
            .map(|pool| pool.pool.clone())
            .collect::<Vec<_>>();
        let index = prompts.choose(
            "Choose an authentication pool",
            &labels,
            "Use auth pool promote <pool> <profile>.",
        )?;
        let pool = &pools[index];
        let mut labels = vec!["Follow configuration (clear interactive preference)".to_owned()];
        labels.extend(pool.profiles.iter().map(|profile| {
            format!(
                "Prefer {}{}",
                profile.profile,
                if profile.cooldown {
                    " (cooling down)"
                } else {
                    ""
                }
            )
        }));
        let selection = match prompts.choose(
            "Pool preference",
            &labels,
            "Use auth pool promote or clear-preference.",
        ) {
            Ok(selection) => selection,
            Err(_) if prompts.back_requested => continue,
            Err(error) => return Err(error),
        };
        let profile = selection
            .checked_sub(1)
            .map(|index| pool.profiles[index].profile.clone());
        let title = format!(
            "Pool {}: {}",
            pool.pool,
            profile.as_deref().unwrap_or("follow configuration")
        );
        match prompts.choose(
            &title,
            &["Confirm preference change".to_owned(), "Cancel".to_owned()],
            "Use explicit pool commands.",
        ) {
            Ok(0) => {
                client
                    .set_auth_pool_preference(pool.pool.clone(), profile)
                    .await?;
                return Ok(());
            }
            Ok(_) => return Ok(()),
            Err(_) if prompts.back_requested => {}
            Err(error) => return Err(error),
        }
    }
}

async fn inspect() -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let catalog = client.account_lifecycle_catalog().await?;
    let mut prompts = auth_prompts::terminal();
    loop {
        let labels = catalog
            .providers
            .iter()
            .map(|provider| format!("{} ({})", provider.display_name, provider.provider_id))
            .collect::<Vec<_>>();
        let index = prompts.choose(
            "Inspect authentication provider",
            &labels,
            "Use auth status <provider>.",
        )?;
        let provider = &catalog.providers[index];
        let labels = account_labels(provider);
        let index = match prompts.choose(
            "Inspect account",
            &labels,
            "Use auth profile show <profile>.",
        ) {
            Ok(index) => index,
            Err(_) if prompts.back_requested => continue,
            Err(error) => return Err(error),
        };
        let account = &provider.accounts[index];
        super::print_json(account)?;
        match prompts.choose(
            "Account actions",
            &[
                "Done".to_owned(),
                "Sign in again".to_owned(),
                "Sign out — keep account".to_owned(),
                "Remove account from this computer".to_owned(),
            ],
            "Use auth login, logout or account remove.",
        ) {
            Ok(1) => {
                return login(
                    Some(provider.provider_id.clone()),
                    Some(account.profile.clone()),
                )
                .await;
            }
            Ok(action @ (2 | 3)) => {
                return lifecycle(LifecycleOptions {
                    provider: Some(provider.provider_id.clone()),
                    profile: Some(account.profile.clone()),
                    action: if action == 3 {
                        AccountLifecycleAction::Remove
                    } else {
                        AccountLifecycleAction::Logout
                    },
                    ..LifecycleOptions::guided(AccountLifecycleAction::Logout)
                })
                .await;
            }
            Ok(_) => return Ok(()),
            Err(_) if prompts.back_requested => {}
            Err(error) => return Err(error),
        }
    }
}

fn account_labels(provider: &AccountLifecycleProvider) -> Vec<String> {
    provider
        .accounts
        .iter()
        .map(|account| {
            format!(
                "{}{}",
                account.profile,
                if account.declared {
                    " (configuration-defined)"
                } else {
                    ""
                }
            )
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Provider,
    Accounts,
    Revocation,
    Review,
}

/// Pure navigation policy: only revisit steps that actually offered a choice.
fn previous_step(options: &LifecycleOptions, step: Step, revocation_offered: bool) -> Option<Step> {
    if step == Step::Review && revocation_offered {
        return Some(Step::Revocation);
    }
    if matches!(step, Step::Review | Step::Revocation) && options.profile.is_none() && !options.all
    {
        return Some(Step::Accounts);
    }
    if step != Step::Provider && options.provider.is_none() {
        return Some(Step::Provider);
    }
    None
}

struct LifecycleFlow {
    options: LifecycleOptions,
    catalog: AccountLifecycleCatalog,
    provider_index: usize,
    selected: Vec<usize>,
    revoke: bool,
    revocation_offered: bool,
    plans: Vec<AccountLifecyclePlan>,
}

impl LifecycleFlow {
    fn provider(&self) -> &AccountLifecycleProvider {
        &self.catalog.providers[self.provider_index]
    }

    fn select_provider(&mut self, prompts: &mut Terminal) -> Result<(), CliError> {
        self.provider_index = if let Some(id) = &self.options.provider {
            self.catalog
                .providers
                .iter()
                .position(|provider| &provider.provider_id == id)
                .ok_or_else(|| {
                    CliError::LoginProfile(format!("Unknown authentication provider '{id}'."))
                })?
        } else {
            let labels = self
                .catalog
                .providers
                .iter()
                .map(|provider| format!("{} ({})", provider.display_name, provider.provider_id))
                .collect::<Vec<_>>();
            prompts.choose(
                "Choose an authentication provider",
                &labels,
                "Pass a provider ID.",
            )?
        };
        Ok(())
    }

    fn select_accounts(&mut self, prompts: &mut Terminal) -> Result<(), CliError> {
        let provider = self.provider();
        self.selected = if let Some(profile) = &self.options.profile {
            vec![
                provider
                    .accounts
                    .iter()
                    .position(|account| &account.profile == profile)
                    .ok_or_else(|| {
                        CliError::LoginProfile(format!(
                            "Account '{profile}' is not owned by this provider."
                        ))
                    })?,
            ]
        } else if self.options.all {
            (0..provider.accounts.len()).collect()
        } else {
            prompts.choose_many(
                "Select accounts (none selected by default)",
                &account_labels(provider),
                "Pass --profile <name> or --all.",
            )?
        };
        if self.selected.is_empty() {
            return Err(CliError::LoginProfile("No accounts selected.".to_owned()));
        }
        Ok(())
    }

    fn select_revocation(&mut self, prompts: &mut Terminal) -> Result<(), CliError> {
        let provider = self.provider();
        let supports_revocation = self.selected.iter().all(|index| {
            let account = &provider.accounts[*index];
            provider
                .methods
                .iter()
                .find(|method| Some(method.method_id()) == account.method_id.as_deref())
                .is_some_and(|method| {
                    let (AuthMethodContribution::Interactive {
                        supports_revocation,
                        ..
                    }
                    | AuthMethodContribution::SecretFields {
                        supports_revocation,
                        ..
                    }) = method;
                    *supports_revocation
                })
        });
        self.revocation_offered =
            !self.options.revoke && supports_revocation && prompts.interactive;
        self.revoke = self.options.revoke
            || (self.revocation_offered
                && prompts.choose(
                    "Credential deletion",
                    &[
                        "Delete locally only (other computers unaffected)".to_owned(),
                        "Also revoke remotely (may affect other computers)".to_owned(),
                    ],
                    "Use --revoke to explicitly request remote revocation.",
                )? == 1);
        Ok(())
    }

    async fn preview(&mut self, client: &AccountLifecycleClient) -> Result<(), CliError> {
        self.plans.clear();
        for index in &self.selected {
            let provider = self.provider();
            let plan = client
                .preview_account_lifecycle(
                    AccountLifecycleRequest {
                        schema_version: 1,
                        provider_id: provider.provider_id.clone(),
                        profile: provider.accounts[*index].profile.clone(),
                        action: self.options.action,
                    },
                    self.revoke,
                )
                .await?;
            self.plans.push(plan);
        }
        Ok(())
    }

    fn review(&self, prompts: &mut Terminal) -> Result<bool, CliError> {
        for plan in &self.plans {
            present_plan(plan);
        }
        if self.options.yes {
            return Ok(true);
        }
        let action = match self.options.action {
            AccountLifecycleAction::Remove => "removal of",
            AccountLifecycleAction::Logout => "sign-out of",
        };
        Ok(prompts.choose(
            "Review account operation",
            &[
                format!("Confirm {action} {} account(s)", self.plans.len()),
                "Cancel".to_owned(),
            ],
            "Pass --yes only after selecting explicit accounts.",
        )? == 0)
    }

    /// Selection and previews only. Execution is deliberately outside this navigation loop.
    async fn navigate(
        &mut self,
        client: &AccountLifecycleClient,
        prompts: &mut Terminal,
    ) -> Result<bool, CliError> {
        let mut step = Step::Provider;
        loop {
            prompts.back_requested = false;
            let result = match step {
                Step::Provider => self.select_provider(prompts).map(|()| Step::Accounts),
                Step::Accounts => self.select_accounts(prompts).map(|()| Step::Revocation),
                Step::Revocation => self.select_revocation(prompts).map(|()| Step::Review),
                Step::Review => {
                    self.preview(client).await?;
                    match self.review(prompts) {
                        Ok(confirmed) => return Ok(confirmed),
                        Err(error) => Err(error),
                    }
                }
            };
            match result {
                Ok(next) => step = next,
                Err(error) => {
                    let previous = if prompts.back_requested {
                        previous_step(&self.options, step, self.revocation_offered)
                    } else {
                        None
                    };
                    if let Some(previous) = previous {
                        step = previous;
                        self.plans.clear();
                    } else {
                        return Err(error);
                    }
                }
            }
        }
    }
}

fn present_plan(plan: &AccountLifecyclePlan) {
    let remove = plan.local.request.action == AccountLifecycleAction::Remove;
    eprintln!(
        "{}: {}",
        if remove { "Remove" } else { "Sign out" },
        plan.local.request.profile
    );
    if remove {
        eprintln!(
            "  Delete owned credentials; remove runtime enrollment and memberships: {}",
            plan.local.runtime_pools.join(", ")
        );
        if plan.local.retains_declarative_profile {
            eprintln!(
                "  Configuration-defined profile remains. Bcode will not edit generated/Nix configuration."
            );
        }
    } else {
        eprintln!(
            "  Delete owned credentials only. Profile, memberships and configuration remain."
        );
    }
    if plan.revoke {
        eprintln!("  Remote revocation requested; effects may extend beyond this computer.");
    }
}

/// Review selected account operations, then execute each confirmed application plan once.
///
/// # Errors
/// Returns selection, terminal, application, or incomplete-operation errors. An execution
/// error may follow partial effects; callers must not automatically replay a plan.
pub async fn lifecycle(options: LifecycleOptions) -> Result<(), CliError> {
    let mut prompts = auth_prompts::terminal();
    options.validate(prompts.interactive)?;
    let client = BcodeClient::default_endpoint().account_lifecycle().await?;
    let mut flow = LifecycleFlow {
        options,
        catalog: client.account_lifecycle_catalog().await?,
        provider_index: 0,
        selected: vec![],
        revoke: false,
        revocation_offered: false,
        plans: vec![],
    };
    if !flow.navigate(&client, &mut prompts).await? {
        return Ok(());
    }
    execute_plans(&client, &flow.plans).await?;
    if flow.options.action == AccountLifecycleAction::Remove
        && prompts.interactive
        && !flow.options.yes
        && prompts.choose(
            "Account removal finished",
            &["Done".to_owned(), "Sign in with another account".to_owned()],
            "Run auth login to enroll another account.",
        )? == 1
    {
        return login(Some(flow.provider().provider_id.clone()), None).await;
    }
    Ok(())
}

async fn execute_plans(
    client: &AccountLifecycleClient,
    plans: &[AccountLifecyclePlan],
) -> Result<(), CliError> {
    // Each reviewed account has its own result. Never replay an ambiguous result.
    let mut failed = false;
    for plan in plans {
        let result = client
            .execute_account_lifecycle(plan.plan_id.clone())
            .await?;
        println!("Account: {}", plan.local.request.profile);
        super::print_json(&result)?;
        failed |= result.local.error.is_some()
            || result.local.credentials != AccountLifecycleEffect::Complete
            || result.revocation == AccountLifecycleRevocation::Failed;
    }
    if failed {
        return Err(CliError::LoginProfile("One or more account operations did not complete fully. Inspect the separate local and remote results before retrying.".to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_navigation_revisits_only_presented_choices() {
        let mut options = LifecycleOptions::guided(AccountLifecycleAction::Remove);
        assert_eq!(previous_step(&options, Step::Provider, true), None);
        assert_eq!(
            previous_step(&options, Step::Accounts, true),
            Some(Step::Provider)
        );
        assert_eq!(
            previous_step(&options, Step::Revocation, true),
            Some(Step::Accounts)
        );
        assert_eq!(
            previous_step(&options, Step::Review, true),
            Some(Step::Revocation)
        );
        assert_eq!(
            previous_step(&options, Step::Review, false),
            Some(Step::Accounts)
        );
        options.all = true;
        assert_eq!(
            previous_step(&options, Step::Revocation, false),
            Some(Step::Provider)
        );
        assert_eq!(
            previous_step(&options, Step::Review, false),
            Some(Step::Provider)
        );
        options.provider = Some("provider".into());
        assert_eq!(previous_step(&options, Step::Review, false), None);
        assert_eq!(
            previous_step(&options, Step::Review, true),
            Some(Step::Revocation)
        );
        options.all = false;
        options.profile = Some("work".into());
        assert_eq!(previous_step(&options, Step::Revocation, false), None);
    }

    #[test]
    fn noninteractive_selection_requires_explicit_confirmation_and_accounts() {
        let mut options = LifecycleOptions::guided(AccountLifecycleAction::Logout);
        assert!(options.validate(false).is_err());
        options.yes = true;
        options.provider = Some("provider".into());
        assert!(options.validate(false).is_err());
        options.profile = Some("work".into());
        assert!(options.validate(false).is_ok());
        options.all = true;
        assert!(options.validate(false).is_err());
        options.profile = None;
        assert!(options.validate(false).is_ok());
    }
}
