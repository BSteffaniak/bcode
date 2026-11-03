//! Guided presentation only; all domain reads and writes use the application client.

use std::io::Write as _;

use bcode_client::BcodeClient;
use bcode_provider_auth_models::{
    AccountPause, AccountRoute, AccountSummary, RenameAccountRequest, SetAccountPauseRequest,
    SetAccountRoutingRequest,
};
use bcode_session_models::{SessionCatalogQuery, SessionDiscoveryScope, SessionId};

use super::{AccountCommand, PauseDuration, Scope, account, route};
use crate::CliError;
use crate::auth_login::{Prompter, terminal};

type Terminal = Prompter<std::io::Stdin, std::io::Stderr>;

#[derive(Debug)]
pub(super) enum Prepared {
    Status(SessionId),
    Routing(SessionId, SetAccountRoutingRequest),
    Pause(SessionId, SetAccountPauseRequest),
    Rename(SessionId, RenameAccountRequest),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Status,
    Use,
    Follow,
    Pause,
    Resume,
    Rename,
}

struct Draft {
    action: Action,
    session: Option<SessionId>,
    provider: Option<String>,
    profile: Option<String>,
    scope: Option<Scope>,
    duration: Option<PauseDuration>,
    fallback: Vec<String>,
    label: Option<String>,
}

impl Draft {
    fn from_command(command: AccountCommand) -> Option<Self> {
        let mut draft = Self {
            action: Action::Status,
            session: None,
            provider: None,
            profile: None,
            scope: None,
            duration: None,
            fallback: vec![],
            label: None,
        };
        match command {
            AccountCommand::Manage => return None,
            AccountCommand::Remove { .. } => {
                unreachable!("account removal is dispatched to auth_management before routing")
            }
            AccountCommand::Status { session } => draft.session = session,
            AccountCommand::Use {
                profile,
                session,
                provider,
                scope,
                fallback,
            } => {
                draft.action = Action::Use;
                draft.profile = profile;
                draft.session = session;
                draft.provider = provider;
                draft.scope = scope;
                draft.fallback = fallback;
            }
            AccountCommand::FollowConfiguration { session, scope } => {
                draft.action = Action::Follow;
                draft.session = session;
                draft.scope = scope;
            }
            AccountCommand::Pause {
                profile,
                session,
                provider,
                duration,
            } => {
                draft.action = Action::Pause;
                draft.profile = profile;
                draft.session = session;
                draft.provider = provider;
                draft.duration = duration;
            }
            AccountCommand::Resume {
                profile,
                session,
                provider,
            } => {
                draft.action = Action::Resume;
                draft.profile = profile;
                draft.session = session;
                draft.provider = provider;
            }
            AccountCommand::Rename {
                profile,
                label,
                session,
                provider,
            } => {
                draft.action = Action::Rename;
                draft.profile = profile;
                draft.label = label;
                draft.session = session;
                draft.provider = provider;
            }
        }
        Some(draft)
    }

    const fn needs_account(&self) -> bool {
        !matches!(self.action, Action::Status | Action::Follow)
    }

    const fn explicit(&self) -> bool {
        self.session.is_some()
            && (!self.needs_account() || (self.provider.is_some() && self.profile.is_some()))
    }

    fn prepare(self) -> Result<Prepared, CliError> {
        let session = self.session.ok_or_else(missing)?;
        if self.action == Action::Status {
            return Ok(Prepared::Status(session));
        }
        if self.action == Action::Follow {
            return Ok(Prepared::Routing(
                session,
                SetAccountRoutingRequest {
                    scope: self.scope.unwrap_or(Scope::Session).into(),
                    route: AccountRoute::FollowConfiguration,
                },
            ));
        }
        let provider = self.provider.ok_or_else(missing)?;
        let profile = self.profile.ok_or_else(missing)?;
        Ok(match self.action {
            Action::Use => Prepared::Routing(
                session,
                SetAccountRoutingRequest {
                    scope: self.scope.unwrap_or(Scope::Session).into(),
                    route: route(&provider, profile, self.fallback),
                },
            ),
            Action::Rename => Prepared::Rename(
                session,
                RenameAccountRequest {
                    account: account(provider, profile),
                    label: self.label,
                },
            ),
            _ => Prepared::Pause(
                session,
                SetAccountPauseRequest {
                    account: account(provider, profile),
                    pause: if self.action == Action::Resume {
                        AccountPause::Resume
                    } else {
                        match self.duration.unwrap_or(PauseDuration::Manual) {
                            PauseDuration::Manual => AccountPause::Manual,
                            PauseDuration::OneHour => AccountPause::OneHour,
                            PauseDuration::FourHours => AccountPause::FourHours,
                        }
                    },
                },
            ),
        })
    }
}

fn missing() -> CliError {
    CliError::InvalidArguments("Missing account selection. Use an interactive terminal, or pass --session <id> and, for account actions, <profile> --provider <plugin-id>. Use `bcode account --help` for explicit forms.".into())
}

fn choose<R: std::io::Read, W: std::io::Write>(
    prompts: &mut Prompter<R, W>,
    title: &str,
    labels: &[String],
) -> Result<usize, CliError> {
    prompts.back_requested = false;
    if labels.is_empty() {
        return Err(CliError::InvalidArguments(
            "No matching choices are visible. Check the provider/profile filters or choose a directory containing a native session.".into(),
        ));
    }
    let mut choices = labels.to_vec();
    choices.extend(options(&["Back", "Cancel"]));
    let selected = prompts.choose(
        title,
        &choices,
        "Select an entry, Back, or Cancel; nothing changes until review is confirmed.",
    )?;
    if selected < labels.len() {
        return Ok(selected);
    }
    prompts.back_requested = selected == labels.len();
    Err(CliError::InvalidArguments(
        "Account selection stopped before changes were applied.".into(),
    ))
}

async fn session(client: &BcodeClient, prompts: &mut Terminal) -> Result<SessionId, CliError> {
    let scope = SessionDiscoveryScope::WorkingDirectory(std::env::current_dir()?);
    let mut cursor = None;
    loop {
        let page = client
            .list_sessions_page_with_scope(
                scope.clone(),
                SessionCatalogQuery::default(),
                cursor,
                30,
            )
            .await?;
        let sessions = page
            .sessions
            .iter()
            .filter(|entry| entry.is_picker_visible() && entry.import.is_none())
            .collect::<Vec<_>>();
        let mut labels = sessions
            .iter()
            .map(|entry| format!("{} ({})", entry.display_title(), entry.id))
            .collect::<Vec<_>>();
        if page.next_cursor.is_some() {
            labels.push("More sessions".into());
        }
        if sessions.is_empty() && page.next_cursor.is_none() {
            return Err(CliError::InvalidArguments(format!(
                "No selectable native sessions in this directory (catalog: {:?}). Pass --session <id> for an existing session, or create a session first.",
                page.catalog_status
            )));
        }
        let selected = choose(
            prompts,
            &format!(
                "Choose a session in this directory (catalog: {:?})",
                page.catalog_status
            ),
            &labels,
        )?;
        if let Some(entry) = sessions.get(selected) {
            return Ok(entry.id);
        }
        if selected == sessions.len() && page.next_cursor.is_some() {
            cursor = page.next_cursor;
        } else {
            return Err(CliError::InvalidArguments(
                "Account selection cancelled; nothing changed.".into(),
            ));
        }
    }
}

fn account_choices<'a>(draft: &Draft, accounts: &'a [AccountSummary]) -> Vec<&'a AccountSummary> {
    accounts
        .iter()
        .filter(|entry| {
            draft
                .provider
                .as_ref()
                .is_none_or(|provider| *provider == entry.account.provider_id)
                && draft
                    .profile
                    .as_ref()
                    .is_none_or(|profile| *profile == entry.account.profile)
        })
        .collect()
}

fn options(labels: &[&str]) -> Vec<String> {
    labels.iter().map(|label| (*label).to_owned()).collect()
}

pub(super) async fn complete(
    client: &BcodeClient,
    command: AccountCommand,
) -> Result<Option<Prepared>, CliError> {
    let mut prompts = terminal();
    let draft = Draft::from_command(command);
    if let Some(draft) = draft {
        if draft.explicit() {
            return draft.prepare().map(Some);
        }
        if !prompts.interactive {
            return Err(missing());
        }
        return guide(client, &mut prompts, draft).await;
    }
    if !prompts.interactive {
        return Err(missing());
    }
    loop {
        let selected = choose(
            &mut prompts,
            "Manage account routing",
            &options(&[
                "Status",
                "Use an account",
                "Follow configuration",
                "Pause an account",
                "Resume an account",
                "Rename an account",
            ]),
        )?;
        let command = match selected {
            0 => AccountCommand::Status { session: None },
            1 => AccountCommand::Use {
                profile: None,
                session: None,
                provider: None,
                scope: None,
                fallback: vec![],
            },
            2 => AccountCommand::FollowConfiguration {
                session: None,
                scope: None,
            },
            3 => AccountCommand::Pause {
                profile: None,
                session: None,
                provider: None,
                duration: None,
            },
            4 => AccountCommand::Resume {
                profile: None,
                session: None,
                provider: None,
            },
            5 => AccountCommand::Rename {
                profile: None,
                label: None,
                session: None,
                provider: None,
            },
            _ => return Ok(None),
        };
        let Some(draft) = Draft::from_command(command) else {
            unreachable!()
        };
        match guide(client, &mut prompts, draft).await {
            Err(_) if prompts.back_requested => {}
            result => return result,
        }
    }
}

fn pick_label(prompts: &mut Terminal) -> Result<Option<String>, CliError> {
    if choose(
        prompts,
        "Friendly label (not credentials)",
        &options(&["Enter a label", "Restore profile name"]),
    )? != 0
    {
        return Ok(None);
    }
    write!(prompts.writer, "Label (type /back or /cancel to leave): ")?;
    prompts.writer.flush()?;
    let label =
        crate::read_auth_input_line(std::io::BufReader::with_capacity(1, &mut prompts.reader))?;
    if label == "/back" {
        prompts.back_requested = true;
        return Err(missing());
    }
    if label == "/cancel" {
        return Err(CliError::InvalidArguments(
            "Account rename cancelled; nothing changed.".into(),
        ));
    }
    if label.trim().is_empty() {
        return Err(CliError::InvalidArguments(
            "Label must not be empty; choose Restore profile name instead.".into(),
        ));
    }
    Ok(Some(label))
}

async fn pick_fallback(
    client: &BcodeClient,
    prompts: &mut Terminal,
    draft: &mut Draft,
) -> Result<(), CliError> {
    // Explicitly select alternatives in attempt order, never infer a pool.
    let routing = client
        .account_routing(draft.session.ok_or_else(missing)?)
        .await?;
    draft.fallback.clear();
    loop {
        let entries = routing
            .accounts
            .iter()
            .filter(|entry| {
                Some(&entry.account.provider_id) == draft.provider.as_ref()
                    && Some(&entry.account.profile) != draft.profile.as_ref()
                    && !draft.fallback.contains(&entry.account.profile)
            })
            .collect::<Vec<_>>();
        let mut labels = vec![if draft.fallback.is_empty() {
            "Only this account (no fallback)".into()
        } else {
            "Done adding fallback accounts".into()
        }];
        labels.extend(
            entries
                .iter()
                .map(|entry| format!("Add {} ({})", entry.label, entry.account.profile)),
        );
        let selected = choose(prompts, "Fallback in attempt order", &labels)?;
        if selected == 0 {
            return Ok(());
        }
        draft
            .fallback
            .push(entries[selected - 1].account.profile.clone());
    }
}

fn review(draft: &Draft, prompts: &mut Terminal) -> Result<usize, CliError> {
    let action = match draft.action {
        Action::Status => "Inspect status",
        Action::Use => "Use account (model unchanged)",
        Action::Follow => "Follow configuration",
        Action::Pause => "Pause account across this configuration",
        Action::Resume => "Resume account across this configuration (routing unchanged)",
        Action::Rename => "Rename account across this configuration (credentials unchanged)",
    };
    let summary = format!(
        "Review — {action}: session {}, account {:?}/{:?}, scope {:?}, duration {:?}, fallback in attempt order {:?}, label {}. Routing affects next turns, not an already running turn. Confirm?",
        draft.session.ok_or_else(missing)?,
        draft.provider,
        draft.profile,
        draft.scope.unwrap_or(Scope::Session),
        draft.duration.unwrap_or(PauseDuration::Manual),
        draft.fallback,
        draft.label.as_deref().unwrap_or("restore profile name")
    );
    choose(prompts, &summary, &options(&["Confirm"]))
}

fn pick_scope(prompts: &mut Terminal) -> Result<Scope, CliError> {
    Ok(
        match choose(
            prompts,
            "Where should routing apply?",
            &options(&["This session's next turn", "New sessions", "Both"]),
        )? {
            0 => Scope::Session,
            1 => Scope::NewSessions,
            _ => Scope::Both,
        },
    )
}

fn pick_duration(prompts: &mut Terminal) -> Result<PauseDuration, CliError> {
    Ok(
        match choose(
            prompts,
            "Pause across this configuration for how long?",
            &options(&["Until manually resumed", "One hour", "Four hours"]),
        )? {
            0 => PauseDuration::Manual,
            1 => PauseDuration::OneHour,
            _ => PauseDuration::FourHours,
        },
    )
}

async fn guide(
    client: &BcodeClient,
    prompts: &mut Terminal,
    mut draft: Draft,
) -> Result<Option<Prepared>, CliError> {
    let pick_session = draft.session.is_none();
    let pick_account =
        draft.needs_account() && (draft.profile.is_none() || draft.provider.is_none());
    let original_provider = draft.provider.clone();
    let original_profile = draft.profile.clone();
    let pick_scope = matches!(draft.action, Action::Use | Action::Follow) && draft.scope.is_none();
    let pick_duration = draft.action == Action::Pause && draft.duration.is_none();
    let pick_label = draft.action == Action::Rename && draft.label.is_none();
    let pick_fallback = draft.action == Action::Use && draft.fallback.is_empty();
    let steps = [
        pick_session,
        pick_account,
        pick_scope || pick_duration || pick_label,
        pick_fallback,
        true,
    ]
    .into_iter()
    .enumerate()
    .filter_map(|(step, enabled)| enabled.then_some(step))
    .collect::<Vec<_>>();
    let mut position = 0_usize;
    loop {
        let step = steps[position];
        prompts.back_requested = false;
        let result: Result<(), CliError> = async {
            match step {
                0 if pick_session => draft.session = Some(session(client, prompts).await?),
                1 if pick_account => {
                    let routing = client
                        .account_routing(draft.session.ok_or_else(missing)?)
                        .await?;
                    draft.provider.clone_from(&original_provider);
                    draft.profile.clone_from(&original_profile);
                    let entries = account_choices(&draft, &routing.accounts);
                    let labels = entries
                        .iter()
                        .map(|entry| {
                            format!(
                                "{} — {} / {} (pause: {:?}, readiness: {:?})",
                                entry.label,
                                entry.account.provider_id,
                                entry.account.profile,
                                entry.pause,
                                entry.readiness
                            )
                        })
                        .collect::<Vec<_>>();
                    let selected = choose(prompts, "Choose an account", &labels)?;
                    draft.provider = Some(entries[selected].account.provider_id.clone());
                    draft.profile = Some(entries[selected].account.profile.clone());
                }
                2 if pick_scope => draft.scope = Some(self::pick_scope(prompts)?),
                2 if pick_duration => draft.duration = Some(self::pick_duration(prompts)?),
                2 if pick_label => draft.label = self::pick_label(prompts)?,
                3 if pick_fallback => self::pick_fallback(client, prompts, &mut draft).await?,
                _ => {}
            }
            Ok(())
        }
        .await;
        match result {
            Err(_) if prompts.back_requested && position > 0 => {
                position -= 1;
                continue;
            }
            Err(error) => return Err(error),
            Ok(()) => {}
        }
        if step < 4 {
            position += 1;
            continue;
        }
        if draft.action == Action::Status {
            return draft.prepare().map(Some);
        }
        match review(&draft, prompts) {
            Ok(_) => return draft.prepare().map(Some),
            Err(_) if prompts.back_requested => position = position.saturating_sub(1),
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_provider_auth_models::AccountRoutingScope;
    use clap::Parser;
    use std::io::Cursor;

    #[derive(Parser)]
    struct Args {
        #[command(subcommand)]
        command: AccountCommand,
    }

    fn parse(args: &[&str]) -> Draft {
        Draft::from_command(Args::try_parse_from(args).unwrap().command).unwrap()
    }

    #[test]
    fn remove_parses_lifecycle_options_and_rejects_all_with_profile() {
        let args = Args::try_parse_from([
            "account",
            "remove",
            "work",
            "--provider",
            "provider",
            "--yes",
            "--revoke",
        ])
        .unwrap();
        let AccountCommand::Remove {
            provider,
            profile,
            all,
            yes,
            revoke,
        } = args.command
        else {
            panic!("wrong command");
        };
        assert_eq!(provider.as_deref(), Some("provider"));
        assert_eq!(profile.as_deref(), Some("work"));
        assert!(!all);
        assert!(yes);
        assert!(revoke);
        assert!(matches!(
            Args::try_parse_from(["account", "remove", "--all"])
                .unwrap()
                .command,
            AccountCommand::Remove {
                provider: None,
                profile: None,
                all: true,
                yes: false,
                revoke: false
            }
        ));
        assert!(Args::try_parse_from(["account", "remove", "work", "--all"]).is_err());
    }

    #[test]
    fn omitted_selections_parse_for_every_action() {
        for action in [
            "status",
            "use",
            "follow-configuration",
            "pause",
            "resume",
            "rename",
        ] {
            assert!(!parse(&["account", action]).explicit());
        }
        assert!(matches!(
            Args::try_parse_from(["account", "manage"]).unwrap().command,
            AccountCommand::Manage
        ));
    }

    fn test_session() -> &'static str {
        "00000000-0000-4000-8000-000000000001"
    }

    #[test]
    fn explicit_use_keeps_exclusive_default_scope_and_fallback_order() {
        let id = test_session();
        let draft = parse(&[
            "account",
            "use",
            "work",
            "--session",
            id,
            "--provider",
            "provider",
        ]);
        assert!(draft.explicit());
        let Prepared::Routing(_, request) = draft.prepare().unwrap() else {
            panic!("wrong request")
        };
        assert_eq!(request.scope, AccountRoutingScope::Session);
        assert_eq!(request.route, route("provider", "work".into(), vec![]));
        let draft = parse(&[
            "account",
            "use",
            "work",
            "--session",
            id,
            "--provider",
            "provider",
            "--scope",
            "both",
            "--fallback",
            "personal,other",
        ]);
        let Prepared::Routing(_, request) = draft.prepare().unwrap() else {
            panic!("wrong request")
        };
        assert_eq!(request.scope, AccountRoutingScope::Both);
        assert_eq!(
            request.route,
            route(
                "provider",
                "work".into(),
                vec!["personal".into(), "other".into()]
            )
        );
    }

    #[test]
    fn explicit_rename_without_label_still_resets_and_pause_defaults_to_manual() {
        let id = test_session();
        let draft = parse(&[
            "account",
            "rename",
            "work",
            "--session",
            id,
            "--provider",
            "provider",
        ]);
        assert!(draft.explicit());
        let Prepared::Rename(_, request) = draft.prepare().unwrap() else {
            panic!("wrong request")
        };
        assert_eq!(request.label, None);
        let draft = parse(&[
            "account",
            "pause",
            "work",
            "--session",
            id,
            "--provider",
            "provider",
        ]);
        let Prepared::Pause(_, request) = draft.prepare().unwrap() else {
            panic!("wrong request")
        };
        assert_eq!(request.pause, AccountPause::Manual);
    }

    fn prompts(input: &[u8], interactive: bool) -> Prompter<Cursor<Vec<u8>>, Vec<u8>> {
        Prompter {
            reader: Cursor::new(input.to_vec()),
            writer: vec![],
            interactive,
            rich: false,
            back_requested: false,
        }
    }

    #[test]
    fn choices_are_constrained_by_summary_provider_and_profile() {
        use bcode_provider_auth_models::{AccountPauseState, AccountReadiness};
        let entries = [
            ("provider", "work"),
            ("provider", "personal"),
            ("other", "work"),
        ]
        .into_iter()
        .map(|(provider, profile)| AccountSummary {
            account: account(provider.into(), profile.into()),
            label: profile.into(),
            pools: vec![],
            pause: AccountPauseState::Eligible,
            readiness: AccountReadiness::Unknown,
            last_used_unix: None,
        })
        .collect::<Vec<_>>();
        let draft = parse(&["account", "use", "work", "--provider", "provider"]);
        let choices = account_choices(&draft, &entries);
        assert_eq!(choices.len(), 1);
        assert_eq!(
            choices[0].account,
            account("provider".into(), "work".into())
        );
        let draft = parse(&["account", "use", "missing"]);
        assert_eq!(
            account_choices(&draft, &entries),
            [] as [&AccountSummary; 0]
        );
    }

    #[test]
    fn back_cancel_and_non_tty_never_confirm() {
        let labels = options(&["Confirm"]);
        let mut back = prompts(b"2\n", true);
        assert!(choose(&mut back, "Review", &labels).is_err());
        assert!(back.back_requested);
        let mut cancel = prompts(b"3\n", true);
        assert!(choose(&mut cancel, "Review", &labels).is_err());
        assert!(!cancel.back_requested);
        let mut non_tty = prompts(b"1\n", false);
        assert!(choose(&mut non_tty, "Review", &labels).is_err());
        assert_eq!(non_tty.reader.position(), 0);
        assert_eq!(
            choose(&mut prompts(b"1\n", true), "Review", &labels).unwrap(),
            0
        );
    }
}
