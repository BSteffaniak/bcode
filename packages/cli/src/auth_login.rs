//! Terminal-only presentation of provider-registered login choices.

use super::{CliError, read_auth_input_line, selected_auth_method};
use bcode_config::{BcodeConfig, RuntimeAuthSubscriptions};
use bcode_plugin::RegisteredAuthProvider;
use bcode_provider_auth::enrollment::{EnrollmentDestination, EnrollmentPlan, PoolEnrollment};
use bcode_provider_auth_models::{AuthMethodContribution, AuthPoolCompatibility};
use std::collections::BTreeSet;
use std::io::{IsTerminal as _, Write};

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

pub fn resolve_provider(explicit_profile: Option<&str>) -> Result<Option<String>, CliError> {
    let config = bcode_config::load_config()?;
    let runtime = bcode_config::try_load_runtime_auth_subscriptions()?;
    if let Some(name) = explicit_profile {
        if let Some(profile) = config.auth.profiles.get(name) {
            if let Some(provider) = &profile.provider_id {
                return Ok(Some(provider.clone()));
            }
            // Preserve explicit legacy API-key profile enrollment, never assume it for bare login.
            return Ok(None);
        }
        if config.active_context.is_none()
            && let Some(profile) = runtime.profiles.get(name)
        {
            return Ok(Some(profile.provider_id.clone()));
        }
    }
    let mut host = super::load_cli_plugin_host()?;
    let result = terminal().provider(&host.auth_provider_registry().providers());
    let cleanup = host.deactivate_all().map_err(CliError::from);
    result.and_then(|provider| cleanup.map(|()| Some(provider)))
}

pub struct Prompter<R, W> {
    pub reader: R,
    pub writer: W,
    pub interactive: bool,
    pub rich: bool,
    pub back_requested: bool,
}

pub fn terminal() -> Prompter<std::io::Stdin, std::io::Stderr> {
    Prompter {
        reader: std::io::stdin(),
        writer: std::io::stderr(),
        interactive: interactive(),
        rich: bcode_tui::auth_inline::supported(),
        back_requested: false,
    }
}

impl<R: std::io::Read, W: Write> Prompter<R, W> {
    pub fn choose(
        &mut self,
        title: &str,
        labels: &[String],
        hint: &str,
    ) -> Result<usize, CliError> {
        if !self.interactive {
            return Err(CliError::LoginProfile(format!("{title}. {hint}")));
        }
        if labels.is_empty() {
            return Err(CliError::LoginProfile(format!(
                "{title}: no available choices. {hint}"
            )));
        }
        if self.rich {
            let config = bcode_config::load_config()?;
            return match bcode_tui::auth_inline::choose(title, labels, &config.tui)? {
                bcode_tui::auth_inline::AuthChoiceOutcome::Selected(index) => Ok(index),
                bcode_tui::auth_inline::AuthChoiceOutcome::Back => {
                    self.back_requested = true;
                    Err(CliError::LoginProfile(
                        "Return to previous authentication step.".to_owned(),
                    ))
                }
                bcode_tui::auth_inline::AuthChoiceOutcome::Cancelled => {
                    Err(CliError::LoginProfile(
                        "Authentication cancelled before provider side effects.".to_owned(),
                    ))
                }
            };
        }
        writeln!(self.writer, "{title}")?;
        for (index, label) in labels.iter().enumerate() {
            writeln!(self.writer, "  {}) {label}", index + 1)?;
        }
        loop {
            write!(self.writer, "Select 1-{} (q to cancel): ", labels.len())?;
            self.writer.flush()?;
            // Avoid reading ahead into a subsequent password or provider-owned prompt.
            let answer =
                read_auth_input_line(std::io::BufReader::with_capacity(1, &mut self.reader))?;
            if answer.trim().eq_ignore_ascii_case("q") {
                return Err(CliError::LoginProfile(
                    "Authentication cancelled; credentials were not changed.".to_owned(),
                ));
            }
            if let Ok(number) = answer.trim().parse::<usize>()
                && let Some(index) = number.checked_sub(1).filter(|index| *index < labels.len())
            {
                return Ok(index);
            }
            writeln!(self.writer, "Enter a listed number, or q to cancel.")?;
        }
    }

    pub fn provider(&mut self, providers: &[&RegisteredAuthProvider]) -> Result<String, CliError> {
        let labels = providers
            .iter()
            .map(|provider| {
                format!(
                    "{} ({})",
                    provider.contribution.display_name, provider.contribution.provider_id
                )
            })
            .collect::<Vec<_>>();
        let index = self.choose(
            "Choose an authentication provider",
            &labels,
            "Pass a provider ID: bcode auth login <provider>. Run `bcode auth providers` to list enabled providers.",
        )?;
        Ok(providers[index].contribution.provider_id.clone())
    }

    pub fn method<'a>(
        &mut self,
        provider: &'a RegisteredAuthProvider,
        requested: Option<&str>,
    ) -> Result<&'a AuthMethodContribution, CliError> {
        if requested.is_some() || provider.contribution.methods.len() <= 1 || !self.interactive {
            return selected_auth_method(provider, requested);
        }
        let labels = provider
            .contribution
            .methods
            .iter()
            .map(|method| {
                let (AuthMethodContribution::SecretFields { display_name, .. }
                | AuthMethodContribution::Interactive { display_name, .. }) = method;
                format!("{display_name} ({})", method.method_id())
            })
            .collect::<Vec<_>>();
        let index = self.choose(
            "Choose an authentication method",
            &labels,
            "Pass --method <method>.",
        )?;
        Ok(&provider.contribution.methods[index])
    }

    pub fn profile(
        &mut self,
        planning: &Planning<'_>,
        explicit: Option<&str>,
        hint: Option<String>,
    ) -> Result<Option<String>, CliError> {
        if explicit.is_some() || !self.interactive {
            return Ok(explicit.map(str::to_owned).or(hint));
        }
        let mut names = planning
            .config
            .auth
            .profiles
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        if planning.config.active_context.is_none() {
            names.extend(planning.runtime.profiles.keys().cloned());
            names.extend(planning.runtime.pools.values().flat_map(|pool| {
                pool.profiles
                    .iter()
                    .map(|member| member.auth_profile.clone())
            }));
        }
        let mut profiles = names
            .into_iter()
            .filter(|name| {
                bcode_provider_auth::resolve_auth_provider_profile(
                    planning.config,
                    &planning.provider.contribution.provider_id,
                    &planning.provider.plugin_id,
                    Some(name),
                    planning.runtime,
                )
                .is_ok()
            })
            .collect::<Vec<_>>();
        let mut labels = profiles
            .iter()
            .map(|name| {
                format!(
                    "Reauthenticate {name}{}",
                    if hint.as_ref() == Some(name) {
                        " (current account)"
                    } else {
                        ""
                    }
                )
            })
            .collect::<Vec<_>>();
        if planning.config.active_context.is_none() {
            let name = bcode_provider_auth::enrollment::new_profile_name(
                planning.config,
                planning.runtime,
                &planning.provider.contribution.provider_id,
            );
            labels.push(format!("Create account {name}"));
            profiles.push(name);
        }
        let index = self.choose(
            "Choose an authentication account", &labels,
            "Pass --profile <name>. In a named context, declare a compatible account in that context first.",
        )?;
        Ok(Some(profiles[index].clone()))
    }

    pub fn plan(
        &mut self,
        planning: &Planning<'_>,
        destination: EnrollmentDestination,
        pooling: PoolEnrollment,
    ) -> Result<EnrollmentPlan, CliError> {
        if pooling == PoolEnrollment::Automatic
            && bcode_provider_auth::resolve_auth_provider_profile(
                planning.config,
                &planning.provider.contribution.provider_id,
                &planning.provider.plugin_id,
                destination.profile.as_deref(),
                planning.runtime,
            )
            .is_ok()
        {
            writeln!(
                self.writer,
                "Reauthenticating existing account; pool memberships will be preserved."
            )?;
            return bcode_provider_auth::enrollment::plan_intent(
                planning.config,
                planning.runtime,
                &planning.provider.contribution,
                (&planning.provider.plugin_id, planning.method.method_id()),
                destination,
                bcode_provider_auth::enrollment::EnrollmentIntent::Reauthenticate,
                planning.compatibility,
            )
            .map_err(|error| CliError::LoginProfile(error.to_string()));
        }
        let eligible = matches!(planning.method, AuthMethodContribution::Interactive { .. })
            && planning.compatibility.is_some_and(|capability| {
                capability
                    .methods
                    .iter()
                    .any(|id| id == planning.method.method_id())
            });
        if pooling != PoolEnrollment::Automatic
            || !self.interactive
            || !eligible
            || planning.config.active_context.is_some()
        {
            return planning.plan(destination, pooling).map_err(|error| CliError::LoginProfile(format!(
                "{error}. To select pool enrollment explicitly, use --pool <name> or --standalone (preserve existing memberships)."
            )));
        }
        let mut plans = Vec::new();
        let mut labels = Vec::new();
        // Reauthentication must not require changing pool metadata, even if unrelated pools conflict.
        plans.push(
            planning
                .plan(destination.clone(), PoolEnrollment::Standalone)
                .map_err(|error| CliError::LoginProfile(error.to_string()))?,
        );
        labels.push("Authenticate only; preserve existing pool memberships".to_owned());
        let names = planning
            .config
            .auth
            .pools
            .keys()
            .chain(planning.runtime.pools.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        for name in names {
            match planning.plan(destination.clone(), PoolEnrollment::Explicit(name.clone())) {
                Ok(plan) => {
                    plans.push(plan);
                    labels.push(format!("Join pool {name}"));
                }
                Err(error) => writeln!(self.writer, "Pool {name} is unavailable: {error}")?,
            }
        }
        match planning.plan(destination, PoolEnrollment::Automatic) {
            Ok(plan) => {
                plans.push(plan);
                labels.push("Use automatic/default pool enrollment".to_owned());
            }
            Err(error) => writeln!(
                self.writer,
                "Automatic pool enrollment is unavailable: {error}"
            )?,
        }
        if plans.len() == 1 {
            writeln!(
                self.writer,
                "Authenticating only; existing pool memberships preserved."
            )?;
            return Ok(plans.remove(0));
        }
        let index = self.choose(
            "Choose authentication pool enrollment",
            &labels,
            "Pass --pool <name> or --standalone.",
        )?;
        Ok(plans.swap_remove(index))
    }
}

pub struct Planning<'a> {
    pub config: &'a BcodeConfig,
    pub runtime: &'a RuntimeAuthSubscriptions,
    pub provider: &'a RegisteredAuthProvider,
    pub method: &'a AuthMethodContribution,
    pub compatibility: Option<&'a AuthPoolCompatibility>,
}

impl Planning<'_> {
    fn plan(
        &self,
        destination: EnrollmentDestination,
        pooling: PoolEnrollment,
    ) -> Result<EnrollmentPlan, bcode_provider_auth::enrollment::EnrollmentError> {
        bcode_provider_auth::enrollment::plan(
            self.config,
            self.runtime,
            &self.provider.contribution,
            &self.provider.plugin_id,
            self.method.method_id(),
            destination,
            pooling,
            self.compatibility,
        )
    }
}

#[cfg(test)]
mod tests;

pub fn preflight<'a, R: std::io::Read, W: Write>(
    prompts: &mut Prompter<R, W>,
    provider: &'a RegisteredAuthProvider,
    config: &BcodeConfig,
    runtime: &RuntimeAuthSubscriptions,
    compatibility: Option<&AuthPoolCompatibility>,
    options: &super::AuthProviderLoginOptions<'_>,
) -> Result<(EnrollmentPlan, &'a AuthMethodContribution), CliError> {
    use bcode_provider_auth::enrollment::{AuthenticationJourney, AuthenticationStep};
    let mut journey = AuthenticationJourney::new();
    let mut prepared = None;
    let mut inferred = None;
    let pooling = if options.standalone {
        PoolEnrollment::Standalone
    } else if let Some(pool) = options.pool {
        PoolEnrollment::Explicit(pool.to_owned())
    } else {
        PoolEnrollment::Automatic
    };
    let mut membership_locked = options.standalone || options.pool.is_some();
    loop {
        prompts.back_requested = false;
        let result = (|| -> Result<(), CliError> {
            match journey.step() {
                AuthenticationStep::Account => {
                    let first_method = initial_method(provider)?;
                    let (profile, method, locked) = choose_account(
                        prompts,
                        &Planning {
                            config,
                            runtime,
                            provider,
                            method: first_method,
                            compatibility,
                        },
                        options,
                    )?;
                    inferred = method;
                    membership_locked = locked;
                    prepared = None;
                    journey.select_account(profile);
                }
                AuthenticationStep::Method => {
                    let method = prompts
                        .method(provider, options.requested_method.or(inferred.as_deref()))?;
                    prepared = None;
                    journey.select_method(method.method_id().to_owned());
                }
                AuthenticationStep::Membership => {
                    let method = selected_auth_method(provider, journey.method())?;
                    prepared = Some(prompts.plan(
                        &Planning {
                            config,
                            runtime,
                            provider,
                            method,
                            compatibility,
                        },
                        EnrollmentDestination {
                            profile: journey.account().map(str::to_owned),
                            vault: options.explicit_vault.clone(),
                            recipient_key: options.recipient_key.map(str::to_owned),
                        },
                        pooling.clone(),
                    )?);
                    journey.review();
                }
                AuthenticationStep::Review => {}
            }
            Ok(())
        })();
        if let Err(error) = result {
            if prompts.back_requested
                && journey.back(
                    options.explicit_profile.is_some(),
                    options.requested_method.is_some() || inferred.is_some(),
                    membership_locked,
                )
            {
                continue;
            }
            return Err(error);
        }
        // Review is entered by membership planning; only a completed review admits effects.
        if journey.step() == AuthenticationStep::Review && prepared.is_some() {
            // Run review once after entering it, not in the membership transition.
            let plan = prepared
                .take()
                .ok_or_else(|| CliError::LoginProfile("Authentication plan missing.".to_owned()))?;
            let method = selected_auth_method(provider, journey.method())?;
            if prompts.interactive
                && let Err(error) = review(prompts, provider, &plan, method)
            {
                if prompts.back_requested
                    && journey.back(
                        options.explicit_profile.is_some(),
                        options.requested_method.is_some() || inferred.is_some(),
                        membership_locked,
                    )
                {
                    continue;
                }
                return Err(error);
            }
            return Ok((plan, method));
        }
    }
}

fn choose_account<R: std::io::Read, W: Write>(
    prompts: &mut Prompter<R, W>,
    planning: &Planning<'_>,
    options: &super::AuthProviderLoginOptions<'_>,
) -> Result<(Option<String>, Option<String>, bool), CliError> {
    let Planning {
        config,
        runtime,
        provider,
        compatibility,
        ..
    } = *planning;
    let first_method = provider.contribution.methods.first().ok_or_else(|| {
        CliError::LoginProfile("Provider has no registered authentication methods.".to_owned())
    })?;
    let profile = prompts.profile(
        &Planning {
            config,
            runtime,
            provider,
            method: first_method,
            compatibility,
        },
        options.explicit_profile,
        super::registered_auth_profile_hint(config, runtime, provider, options.explicit_profile),
    )?;
    let existing = bcode_provider_auth::resolve_auth_provider_profile(
        config,
        &provider.contribution.provider_id,
        &provider.plugin_id,
        profile.as_deref(),
        runtime,
    )
    .ok();
    let inferred = existing
        .as_ref()
        .and_then(|resolved| resolved.profile.scheme.clone());
    let membership_locked = options.standalone || options.pool.is_some() || existing.is_some();
    if options.no_device_seal && existing.is_some() && options.pool.is_none() && !options.standalone
    {
        return Err(CliError::LoginProfile("Reauthentication preserves the account sealing policy. Use explicit --standalone enrollment to change that policy.".to_owned()));
    }
    Ok((profile, inferred, membership_locked))
}

fn initial_method(provider: &RegisteredAuthProvider) -> Result<&AuthMethodContribution, CliError> {
    provider.contribution.methods.first().ok_or_else(|| {
        CliError::LoginProfile("Provider has no registered authentication methods.".to_owned())
    })
}
fn review<R: std::io::Read, W: Write>(
    prompts: &mut Prompter<R, W>,
    provider: &RegisteredAuthProvider,
    plan: &EnrollmentPlan,
    method: &AuthMethodContribution,
) -> Result<(), CliError> {
    let summary = format!(
        "Continue: {} · {} · {}",
        provider.contribution.display_name,
        plan.prepared.resolved.profile_name,
        method.method_id()
    );
    prompts.choose(
        "Review authentication",
        &[summary],
        "Supply explicit choices in noninteractive usage.",
    )?;
    Ok(())
}
