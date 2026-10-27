//! Account-first subscription presentation and confirmation state.

use bcode_provider_auth_models::{
    AccountId, AccountPause, AccountPauseState, AccountReadiness, AccountRoute, AccountRouting,
    AccountRoutingScope, RenameAccountRequest, SetAccountPauseRequest, SetAccountRoutingRequest,
};
use bmux_text_edit::TextEditBuffer;
use bmux_tui::event::Event;
use bmux_tui::prelude::Line;
use bmux_tui_components::text_input::{TextInputControl, TextInputState};

/// Confirmed intent sent to the account application boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountAction {
    /// Set or reset next-turn / future-session routing.
    Route(SetAccountRoutingRequest),
    /// Pause or resume configuration-wide eligibility.
    Pause(SetAccountPauseRequest),
    /// Change friendly display metadata only.
    Rename(RenameAccountRequest),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Accounts,
    Use,
    Details,
    Pause,
    Rename,
    Confirm,
}

/// Ephemeral form state. All durable state comes from the application boundary.
pub struct AuthPoolPickerApp {
    pub snapshot: AccountRouting,
    pub page: Page,
    pub label: TextInputState,
    pub field: usize,
    pub scroll: usize,
    pub submitting: bool,
    routing: usize,
    scope: usize,
    duration: usize,
    fallbacks: std::collections::BTreeSet<AccountId>,
    pending: Option<AccountAction>,
    list: bmux_tui_components::selectable_list::SelectableListState,
}

pub enum PickerOutcome {
    Continue,
    Cancel,
    Apply(AccountAction),
}

impl AuthPoolPickerApp {
    /// Create presentation state from a secret-free authoritative snapshot.
    #[must_use]
    pub fn new(mut snapshot: AccountRouting) -> Self {
        // Identity is provider/profile, never pool membership or a friendly label.
        snapshot.accounts.sort_by(|a, b| a.account.cmp(&b.account));
        snapshot.accounts.dedup_by(|a, b| a.account == b.account);
        let list = bmux_tui_components::selectable_list::SelectableListState::new(
            (!snapshot.accounts.is_empty()).then_some(0),
        );
        Self {
            snapshot,
            page: Page::Accounts,
            label: super::text_input_flow::empty_state(),
            field: 0,
            scroll: 0,
            submitting: false,
            routing: 0,
            scope: 0,
            duration: 0,
            fallbacks: std::collections::BTreeSet::new(),
            pending: None,
            list,
        }
    }

    /// Currently highlighted account, not a routing prediction.
    pub fn selected(&self) -> Option<&bcode_provider_auth_models::AccountSummary> {
        self.snapshot.accounts.get(self.list.selected()?)
    }

    fn name(&self, id: &AccountId) -> String {
        self.snapshot
            .accounts
            .iter()
            .find(|item| &item.account == id)
            .map_or_else(
                || id.profile.clone(),
                |item| {
                    if item.label.is_empty() {
                        item.account.profile.clone()
                    } else {
                        item.label.clone()
                    }
                },
            )
    }

    #[must_use]
    /// Human-readable selection policy, distinct from usage and readiness.
    pub fn route_label(&self, route: &AccountRoute) -> String {
        match route {
            AccountRoute::FollowConfiguration => "Follow configuration".to_owned(),
            AccountRoute::Only { account } => format!("{} · only account", self.name(account)),
            AccountRoute::Prefer {
                account,
                alternatives,
            } => format!(
                "{} · fallback: {}",
                self.name(account),
                self.alternative_names(alternatives)
            ),
        }
    }

    fn alternative_names(&self, alternatives: &[AccountId]) -> String {
        if alternatives.is_empty() {
            return "none".to_owned();
        }
        alternatives
            .iter()
            .map(|id| format!("{} ({} / {})", self.name(id), id.provider_id, id.profile))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn alternatives(&self) -> Vec<AccountId> {
        let selected = self.selected().map(|item| &item.account);
        self.snapshot
            .accounts
            .iter()
            .filter(|item| {
                Some(&item.account) != selected
                    && selected
                        .is_some_and(|account| account.provider_id == item.account.provider_id)
            })
            .map(|item| item.account.clone())
            .collect()
    }

    const fn scope(&self) -> AccountRoutingScope {
        match self.scope {
            1 => AccountRoutingScope::NewSessions,
            2 => AccountRoutingScope::Both,
            _ => AccountRoutingScope::Session,
        }
    }

    fn route_request(&self) -> Option<AccountAction> {
        let account = self.selected()?.account.clone();
        let route = match self.routing {
            1 => AccountRoute::Prefer {
                account,
                alternatives: self
                    .alternatives()
                    .into_iter()
                    .filter(|id| self.fallbacks.contains(id))
                    .collect(),
            },
            2 => AccountRoute::FollowConfiguration,
            _ => AccountRoute::Only { account },
        };
        Some(AccountAction::Route(SetAccountRoutingRequest {
            scope: self.scope(),
            route,
        }))
    }

    fn submit(&mut self, action: AccountAction) -> PickerOutcome {
        if self.snapshot.running_turn
            && matches!(&action, AccountAction::Route(request) if request.scope != AccountRoutingScope::NewSessions)
        {
            self.pending = Some(action);
            self.page = Page::Confirm;
            self.scroll = 0;
            PickerOutcome::Continue
        } else {
            PickerOutcome::Apply(action)
        }
    }

    /// Interpret form actions using the user's effective keymap.
    pub fn handle_event(
        &mut self,
        event: &Event,
        keymap: &super::keymap::BmuxKeyMap,
    ) -> PickerOutcome {
        if self.submitting {
            return PickerOutcome::Continue;
        }
        let action = match event {
            Event::Key(key) => keymap.action("auth_pool_picker", *key),
            _ => None,
        };
        if action == Some("tui.control.cancel") {
            if self.page == Page::Accounts {
                return PickerOutcome::Cancel;
            }
            self.page = Page::Accounts;
            self.pending = None;
            self.scroll = 0;
            return PickerOutcome::Continue;
        }
        if action == Some("tui.control.page_up") {
            self.scroll = self.scroll.saturating_sub(1);
            return PickerOutcome::Continue;
        }
        if action == Some("tui.control.page_down") {
            self.scroll = self.scroll.saturating_add(1);
            return PickerOutcome::Continue;
        }
        if self.page == Page::Accounts {
            self.scroll = 0;
        }
        if self.page == Page::Rename {
            return self.handle_rename(event, action, keymap);
        }
        match self.page {
            Page::Accounts => return self.handle_accounts(action),
            Page::Use => return self.handle_use(action),
            Page::Pause => return self.handle_pause(action),
            Page::Confirm => {
                if action == Some("tui.control.submit")
                    && let Some(action) = self.pending.clone()
                {
                    return PickerOutcome::Apply(action);
                }
            }
            Page::Details | Page::Rename => {}
        }
        PickerOutcome::Continue
    }

    fn handle_rename(
        &mut self,
        event: &Event,
        action: Option<&str>,
        keymap: &super::keymap::BmuxKeyMap,
    ) -> PickerOutcome {
        if action == Some("tui.control.submit") {
            if let Some(item) = self.selected() {
                let label = self.label.buffer().text().trim().to_owned();
                return PickerOutcome::Apply(AccountAction::Rename(RenameAccountRequest {
                    account: item.account.clone(),
                    label: (!label.is_empty()).then_some(label),
                }));
            }
        } else {
            match event {
                Event::Key(key) => {
                    let _ = super::text_input_flow::handle_key_in_scope(
                        &mut self.label,
                        keymap,
                        "session_rename",
                        *key,
                    );
                }
                Event::Paste(text) => {
                    let _ = TextInputControl::new(&super::text_input_flow::single_line_policy())
                        .handle_paste(&mut self.label, text);
                }
                _ => {}
            }
        }
        PickerOutcome::Continue
    }

    fn handle_accounts(&mut self, action: Option<&str>) -> PickerOutcome {
        match action {
            Some("tui.control.up") => self
                .list
                .set_selected(self.list.selected().map(|index| index.saturating_sub(1))),
            Some("tui.control.down") => self.list.set_selected(
                self.list
                    .selected()
                    .map(|index| (index + 1).min(self.snapshot.accounts.len().saturating_sub(1))),
            ),
            Some("tui.control.submit" | "tui.auth.clear") if self.selected().is_some() => {
                self.routing = if action == Some("tui.auth.clear") {
                    2
                } else {
                    0
                };
                self.scope = 0;
                self.field = 0;
                self.fallbacks.clear();
                self.page = Page::Use;
            }
            Some("tui.auth.details") if self.selected().is_some() => self.page = Page::Details,
            Some("tui.auth.pause") if self.selected().is_some() => {
                self.duration = 0;
                self.page = Page::Pause;
            }
            Some("tui.auth.rename") => {
                if let Some(item) = self.selected() {
                    self.label = TextInputState::new(TextEditBuffer::from_text(&item.label));
                    self.page = Page::Rename;
                }
            }
            _ => {}
        }
        PickerOutcome::Continue
    }

    fn handle_use(&mut self, action: Option<&str>) -> PickerOutcome {
        match action {
            Some("tui.control.focus_next" | "tui.control.focus_previous") => {
                let fields = if self.routing == 1 {
                    2 + self.alternatives().len()
                } else {
                    2
                };
                self.field = if action == Some("tui.control.focus_previous") {
                    (self.field + fields - 1) % fields
                } else {
                    (self.field + 1) % fields
                };
            }
            Some(
                "tui.control.left" | "tui.control.right" | "tui.control.up" | "tui.control.down",
            ) => {
                let delta = if matches!(action, Some("tui.control.left" | "tui.control.up")) {
                    2
                } else {
                    1
                };
                if self.field < 2 {
                    let value = if self.field == 0 {
                        &mut self.routing
                    } else {
                        &mut self.scope
                    };
                    *value = (*value + delta) % 3;
                } else if let Some(id) = self.alternatives().get(self.field - 2).cloned()
                    && !self.fallbacks.remove(&id)
                {
                    self.fallbacks.insert(id);
                }
            }
            Some("tui.control.submit") => {
                if let Some(request) = self.route_request() {
                    return self.submit(request);
                }
            }
            _ => {}
        }
        PickerOutcome::Continue
    }

    fn handle_pause(&mut self, action: Option<&str>) -> PickerOutcome {
        match action {
            Some("tui.control.left" | "tui.control.up") => self.duration = (self.duration + 2) % 3,
            Some("tui.control.right" | "tui.control.down") => {
                self.duration = (self.duration + 1) % 3;
            }
            Some("tui.control.submit") => {
                if let Some(item) = self.selected() {
                    let pause = if item.pause == AccountPauseState::Eligible {
                        match self.duration {
                            1 => AccountPause::OneHour,
                            2 => AccountPause::FourHours,
                            _ => AccountPause::Manual,
                        }
                    } else {
                        AccountPause::Resume
                    };
                    return PickerOutcome::Apply(AccountAction::Pause(SetAccountPauseRequest {
                        account: item.account.clone(),
                        pause,
                    }));
                }
            }
            _ => {}
        }
        PickerOutcome::Continue
    }

    fn use_lines(&self) -> Vec<String> {
        let Some(item) = self.selected() else {
            return Vec::new();
        };
        let name = self.name(&item.account);
        let mut lines = vec![
            format!("Use {name} ({})", item.account.profile),
            format!(
                "{} Routing: {}",
                if self.field == 0 { "›" } else { " " },
                ["Only account", "Prefer account", "Follow configuration"][self.routing]
            ),
            format!(
                "Fallback alternatives: {}",
                if self.routing == 1 {
                    self.alternative_names(
                        &self
                            .alternatives()
                            .into_iter()
                            .filter(|id| self.fallbacks.contains(id))
                            .collect::<Vec<_>>(),
                    )
                } else {
                    "none".to_owned()
                }
            ),
            format!(
                "{} Apply scope: {}",
                if self.field == 1 { "›" } else { " " },
                ["This session", "New sessions using selected preset", "Both"][self.scope]
            ),
            format!(
                "Model unchanged · preset {}",
                self.snapshot
                    .model_profile
                    .as_deref()
                    .unwrap_or("none selected")
            ),
            "Tab field · arrows choice · Enter Apply · Esc Cancel".to_owned(),
        ];
        if self.routing == 1 {
            for (index, id) in self.alternatives().iter().enumerate() {
                lines.push(format!(
                    "{} [{}] {} ({}): fallback",
                    if self.field == index + 2 { "›" } else { " " },
                    if self.fallbacks.contains(id) {
                        "x"
                    } else {
                        " "
                    },
                    self.name(id),
                    id.profile
                ));
            }
            lines.push("Tab to an alternative; arrows include/exclude. Only checked alternatives are used.".to_owned());
        }
        lines
    }

    #[must_use]
    /// Semantic form content; terminal projection is renderer-owned.
    pub fn form_lines(&self) -> Vec<Line> {
        let Some(item) = self.selected() else {
            return vec![Line::from("No accounts configured.")];
        };
        let name = self.name(&item.account);
        let lines = match self.page {
            Page::Use => self.use_lines(),
            Page::Pause => vec![
                format!(
                    "{} {name}?",
                    if item.pause == AccountPauseState::Eligible {
                        "Pause"
                    } else {
                        "Resume"
                    }
                ),
                "Applies across sessions and pools in this configuration.".to_owned(),
                if item.pause == AccountPauseState::Eligible {
                    format!(
                        "Duration: {}",
                        ["Until manually resumed", "1 hour", "4 hours"][self.duration]
                    )
                } else {
                    "Resume separately; does not select this account.".to_owned()
                },
                "Running turn continues unchanged.".to_owned(),
                "Arrows duration · Enter Confirm · Esc Cancel".to_owned(),
            ],
            Page::Details => {
                let mut details = vec![
                    format!("{name} · {}", item.account.profile),
                    format!("Provider: {}", item.account.provider_id),
                    format!("Pools: {}", item.pools.join(", ")),
                    format!("Pause: {:?}", item.pause),
                    format!("Readiness: {:?}", item.readiness),
                    format!(
                        "Last successful dispatch: {}",
                        item.last_used_unix
                            .map_or_else(|| "unknown".to_owned(), |time| time.to_string())
                    ),
                    "Esc Back".to_owned(),
                ];
                for pool in self
                    .snapshot
                    .pools
                    .iter()
                    .filter(|pool| item.pools.contains(&pool.pool))
                {
                    details.push(format!(
                        "Pool {} · strategy {} · preferred {} · {:?}",
                        pool.pool,
                        pool.strategy,
                        pool.preferred_profile.as_deref().unwrap_or("none"),
                        pool.preference_source
                    ));
                    if let Some(reason) = &pool.degraded_reason {
                        details.push(format!("Degraded: {reason}"));
                    }
                }
                details
            }
            Page::Rename => vec![
                format!("Rename {name}"),
                "Friendly label only; profile identity is unchanged.".to_owned(),
                "Empty restores profile ID · Enter Save · Esc Cancel".to_owned(),
            ],
            Page::Confirm => vec![
                "Apply starting next turn?".to_owned(),
                "The current turn continues with its frozen routing.".to_owned(),
                "Model unchanged · Enter Confirm · Esc Cancel".to_owned(),
            ],
            Page::Accounts => Vec::new(),
        };
        lines.into_iter().map(Line::from).collect()
    }

    /// Paint account rows using BMUX-owned measured list geometry.
    pub fn render_list(
        &mut self,
        area: bmux_tui::geometry::Rect,
        frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
        theme: super::render::TuiTheme,
    ) {
        let items = self
            .snapshot
            .accounts
            .iter()
            .map(|item| {
                let status = match item.pause {
                    AccountPauseState::Eligible => "Not paused".to_owned(),
                    AccountPauseState::Manual => "Paused · manual resume".to_owned(),
                    AccountPauseState::Until { unix_seconds } => {
                        format!("Paused until {unix_seconds}")
                    }
                };
                let readiness = match item.readiness {
                    AccountReadiness::Unknown => "Readiness unverified".to_owned(),
                    AccountReadiness::Cooldown { until_unix } => {
                        format!("Cooldown until {until_unix}")
                    }
                };
                let last = if self.snapshot.last_used.as_ref() == Some(&item.account) {
                    " · last used"
                } else {
                    ""
                };
                let identity = format!(
                    "{} ({} / {})",
                    self.name(&item.account),
                    item.account.provider_id,
                    item.account.profile
                );
                let status = format!("{status} · {readiness}{last}");
                if area.width < 60 {
                    vec![Line::from(identity), Line::from(status)]
                } else {
                    vec![Line::from(format!("{identity} · {status}"))]
                }
            })
            .collect::<Vec<_>>();
        let rows = items
            .into_iter()
            .enumerate()
            .map(|(index, lines)| {
                bmux_tui_components::selectable_list::SelectableListItem::multiline(
                    index.to_string(),
                    lines,
                )
            })
            .collect::<Vec<_>>();
        let list = bmux_tui_components::selectable_list::SelectableList::new(&rows)
            .policy(bmux_tui_components::selectable_list::SelectableListPolicy {
                highlight: bmux_tui_components::selectable_list::SelectableListHighlightPolicy::new(
                    "", false,
                ),
                ..Default::default()
            })
            .styles(bmux_tui_components::selectable_list::SelectableListStyles {
                normal: theme.text,
                focused: theme.selection,
                selected: theme.selection,
                hovered: theme.focused,
                pressed: theme.selection,
                disabled: theme.muted,
                background: theme.raised,
                scrollbar: bmux_tui_components::scrollbar::ScrollbarStyles::default(),
            });
        let state = &mut self.list;
        state.set_focused(state.selected());
        let layout = list.layout(&bmux_tui::component::LayoutId::new("accounts"), area);
        if let Some(viewport) = layout.children.first().map(|child| &child.node)
            && let Some(index) = state.selected()
            && let Some(content) = viewport.children.first()
            && let Some(row) = content.node.children.get(index)
        {
            list.scroll_view().ensure_visible(
                viewport,
                &mut state.scroll,
                row.y,
                row.node.size.height.min(viewport.size.height),
            );
        }
        list.clamp_state(area, state);
        list.paint(area, state, theme.text, frame);
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use bmux_keyboard::{KeyCode, KeyStroke};

    pub fn snapshot() -> AccountRouting {
        let account = |profile: &str, label: &str| bcode_provider_auth_models::AccountSummary {
            account: AccountId {
                provider_id: "provider.example".to_owned(),
                profile: profile.to_owned(),
            },
            label: label.to_owned(),
            pools: vec!["pool-one".to_owned(), "pool-two".to_owned()],
            readiness: AccountReadiness::Unknown,
            pause: AccountPauseState::Eligible,
            last_used_unix: None,
        };
        AccountRouting {
            accounts: vec![
                account("personal", "個人 👩‍💻 e\u{301}"),
                account("work", "Work"),
            ],
            pools: Vec::new(),
            model_profile: Some("sol".to_owned()),
            session_route: AccountRoute::FollowConfiguration,
            new_session_route: AccountRoute::FollowConfiguration,
            last_used: None,
            running_turn: false,
        }
    }

    fn press(app: &mut AuthPoolPickerApp, key: KeyCode) -> PickerOutcome {
        app.handle_event(
            &Event::Key(KeyStroke::simple(key)),
            &super::super::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
        )
    }

    #[test]
    fn pools_do_not_duplicate_accounts_or_imply_usage() {
        let mut data = snapshot();
        data.accounts.push(data.accounts[0].clone());
        let app = AuthPoolPickerApp::new(data);
        assert_eq!(app.snapshot.accounts.len(), 2);
        assert_eq!(app.snapshot.last_used, None);
        assert_eq!(
            app.route_label(&app.snapshot.session_route),
            "Follow configuration"
        );
    }

    #[test]
    fn use_requires_apply_and_preserves_explicit_fallbacks_and_scope() {
        let mut app = AuthPoolPickerApp::new(snapshot());
        assert!(matches!(
            press(&mut app, KeyCode::Enter),
            PickerOutcome::Continue
        ));
        assert_eq!(app.page, Page::Use);
        assert!(matches!(
            press(&mut app, KeyCode::Right),
            PickerOutcome::Continue
        ));
        assert!(matches!(
            press(&mut app, KeyCode::Tab),
            PickerOutcome::Continue
        ));
        assert!(matches!(
            press(&mut app, KeyCode::Right),
            PickerOutcome::Continue
        ));
        let _ = press(&mut app, KeyCode::Tab);
        let _ = press(&mut app, KeyCode::Right);
        let PickerOutcome::Apply(AccountAction::Route(request)) = press(&mut app, KeyCode::Enter)
        else {
            panic!("applied route");
        };
        assert_eq!(request.scope, AccountRoutingScope::NewSessions);
        let AccountRoute::Prefer {
            account,
            alternatives,
        } = request.route
        else {
            panic!("explicit fallback route");
        };
        assert_eq!(account.profile, "personal");
        assert_eq!(alternatives, vec![app.snapshot.accounts[1].account.clone()]);
        assert_eq!(app.snapshot.model_profile.as_deref(), Some("sol"));
        assert_eq!(
            app.snapshot.session_route,
            AccountRoute::FollowConfiguration
        );
    }

    #[test]
    fn running_turn_needs_second_confirmation_but_future_defaults_do_not() {
        let mut data = snapshot();
        data.running_turn = true;
        let mut app = AuthPoolPickerApp::new(data);
        let _ = press(&mut app, KeyCode::Enter);
        assert!(matches!(
            press(&mut app, KeyCode::Enter),
            PickerOutcome::Continue
        ));
        assert_eq!(app.page, Page::Confirm);
        assert!(matches!(
            press(&mut app, KeyCode::Escape),
            PickerOutcome::Continue
        ));
        assert_eq!(app.page, Page::Accounts);
        let _ = press(&mut app, KeyCode::Enter);
        let _ = press(&mut app, KeyCode::Tab);
        let _ = press(&mut app, KeyCode::Right);
        assert!(matches!(
            press(&mut app, KeyCode::Enter),
            PickerOutcome::Apply(AccountAction::Route(_))
        ));
    }

    #[test]
    fn pause_is_separate_from_routing_and_resume_never_selects() {
        for (steps, expected) in [
            (0, AccountPause::Manual),
            (1, AccountPause::OneHour),
            (2, AccountPause::FourHours),
        ] {
            let mut app = AuthPoolPickerApp::new(snapshot());
            assert!(matches!(
                press(&mut app, KeyCode::Char('p')),
                PickerOutcome::Continue
            ));
            for _ in 0..steps {
                let _ = press(&mut app, KeyCode::Right);
            }
            let PickerOutcome::Apply(AccountAction::Pause(request)) =
                press(&mut app, KeyCode::Enter)
            else {
                panic!("pause confirmation");
            };
            assert_eq!(request.pause, expected);
            assert_eq!(
                app.snapshot.session_route,
                AccountRoute::FollowConfiguration
            );
        }
        let mut data = snapshot();
        data.accounts[0].pause = AccountPauseState::Manual;
        let mut app = AuthPoolPickerApp::new(data);
        let _ = press(&mut app, KeyCode::Char('p'));
        let PickerOutcome::Apply(AccountAction::Pause(request)) = press(&mut app, KeyCode::Enter)
        else {
            panic!("resume confirmation");
        };
        assert_eq!(request.pause, AccountPause::Resume);
    }

    #[test]
    fn rename_edits_friendly_label_without_changing_identity() {
        let mut app = AuthPoolPickerApp::new(snapshot());
        let _ = press(&mut app, KeyCode::Char('r'));
        app.label = super::super::text_input_flow::state_with_text("Work 🦀", false);
        let PickerOutcome::Apply(AccountAction::Rename(request)) = press(&mut app, KeyCode::Enter)
        else {
            panic!("rename");
        };
        assert_eq!(request.account.profile, "personal");
        assert_eq!(request.label.as_deref(), Some("Work 🦀"));
    }

    #[test]
    fn rename_accepts_action_letters_and_spaces_as_text_and_empty_restores_id() {
        let mut app = AuthPoolPickerApp::new(snapshot());
        let _ = press(&mut app, KeyCode::Char('r'));
        app.label = super::super::text_input_flow::empty_state();
        for character in "p r d c 🦀".chars() {
            let _ = press(&mut app, KeyCode::Char(character));
        }
        assert_eq!(app.label.buffer().text(), "p r d c 🦀");
        app.label = super::super::text_input_flow::empty_state();
        let PickerOutcome::Apply(AccountAction::Rename(request)) = press(&mut app, KeyCode::Enter)
        else {
            panic!("rename reset");
        };
        assert_eq!(request.label, None);
    }

    #[test]
    fn submitting_form_does_not_emit_duplicate_mutations() {
        let mut app = AuthPoolPickerApp::new(snapshot());
        let _ = press(&mut app, KeyCode::Enter);
        app.submitting = true;
        assert!(matches!(
            press(&mut app, KeyCode::Enter),
            PickerOutcome::Continue
        ));
        assert_eq!(
            app.snapshot.session_route,
            AccountRoute::FollowConfiguration
        );
    }

    #[test]
    fn reset_is_scope_explicit_and_does_not_immediately_mutate() {
        let mut app = AuthPoolPickerApp::new(snapshot());
        assert!(matches!(
            press(&mut app, KeyCode::Char('c')),
            PickerOutcome::Continue
        ));
        assert_eq!(app.page, Page::Use);
        let PickerOutcome::Apply(AccountAction::Route(request)) = press(&mut app, KeyCode::Enter)
        else {
            panic!("reset route");
        };
        assert_eq!(request.route, AccountRoute::FollowConfiguration);
        assert_eq!(request.scope, AccountRoutingScope::Session);
    }
}
