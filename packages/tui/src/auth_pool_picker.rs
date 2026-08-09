//! TUI auth-pool profile picker state.

use bcode_provider_auth_models::AuthPoolSummary;
use bmux_tui::list::{ListItem, ListState};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;

/// Terminal auth-pool profile picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthPoolPickerApp {
    pools: Vec<AuthPoolSummary>,
    rows: Vec<(usize, usize)>,
    list: ListState,
}

impl AuthPoolPickerApp {
    /// Create a picker for portable auth-pool summaries.
    #[must_use]
    pub fn new(pools: Vec<AuthPoolSummary>) -> Self {
        let rows = pools
            .iter()
            .enumerate()
            .flat_map(|(pool_index, pool)| {
                (0..pool.profiles.len()).map(move |profile_index| (pool_index, profile_index))
            })
            .collect::<Vec<_>>();
        let mut list = ListState::default();
        if !rows.is_empty() {
            list.select(Some(0));
        }
        Self { pools, rows, list }
    }

    /// Return mutable list state.
    pub const fn list_state_mut(&mut self) -> &mut ListState {
        &mut self.list
    }

    /// Renderable profile rows.
    #[must_use]
    pub fn list_items(&self, muted: Style, accent: Style) -> Vec<ListItem> {
        if self.rows.is_empty() {
            return vec![ListItem::new(Line::from("No auth pools configured."))];
        }
        self.rows
            .iter()
            .map(|(pool_index, profile_index)| {
                let pool = &self.pools[*pool_index];
                let profile = &pool.profiles[*profile_index];
                let marker = if profile.preferred { "preferred" } else { "" };
                let availability = if profile.cooldown {
                    "cooldown"
                } else {
                    "available"
                };
                ListItem::new(Line::from_spans(vec![
                    Span::styled(pool.pool.clone(), Style::new().add_modifier(Modifier::BOLD)),
                    Span::raw("  "),
                    Span::styled(profile.profile.clone(), accent),
                    Span::raw("  "),
                    Span::styled(marker.to_owned(), muted),
                    Span::raw("  "),
                    Span::styled(availability.to_owned(), muted),
                ]))
            })
            .collect()
    }

    /// Move selection down.
    pub fn select_next(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let next = self
            .list
            .selected
            .map_or(0, |index| (index + 1) % self.rows.len());
        self.list.select(Some(next));
    }

    /// Move selection up.
    pub fn select_previous(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let previous = self.list.selected.map_or(0, |index| {
            if index == 0 {
                self.rows.len() - 1
            } else {
                index - 1
            }
        });
        self.list.select(Some(previous));
    }

    /// Select a visible row.
    pub const fn select_visible(&mut self, row: usize) -> bool {
        if row >= self.rows.len() {
            return false;
        }
        self.list.select(Some(row));
        true
    }

    /// Return selected pool and profile.
    #[must_use]
    pub fn selected(&self) -> Option<(String, String)> {
        let (pool_index, profile_index) = *self.rows.get(self.list.selected?)?;
        let pool = &self.pools[pool_index];
        Some((
            pool.pool.clone(),
            pool.profiles[profile_index].profile.clone(),
        ))
    }

    /// Return the selected pool.
    #[must_use]
    pub fn selected_pool(&self) -> Option<String> {
        self.selected().map(|(pool, _)| pool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_provider_auth_models::{
        AUTH_POOL_SCHEMA_VERSION, AuthPoolPreferenceSource, AuthPoolProfileSummary,
    };

    fn pool() -> AuthPoolSummary {
        AuthPoolSummary {
            schema_version: AUTH_POOL_SCHEMA_VERSION,
            pool: "provider".to_owned(),
            provider_plugin_id: Some("plugin".to_owned()),
            strategy: "failover".to_owned(),
            preferred_profile: Some("one".to_owned()),
            preference_source: Some(AuthPoolPreferenceSource::PoolOrder),
            profiles: vec![
                AuthPoolProfileSummary {
                    profile: "one".to_owned(),
                    preferred: true,
                    cooldown: false,
                    cooldown_until_unix: None,
                },
                AuthPoolProfileSummary {
                    profile: "two".to_owned(),
                    preferred: false,
                    cooldown: false,
                    cooldown_until_unix: None,
                },
            ],
            degraded_reason: None,
        }
    }

    #[test]
    fn picker_selects_any_profile_in_an_n_profile_pool() {
        let mut picker = AuthPoolPickerApp::new(vec![pool()]);
        assert_eq!(
            picker.selected(),
            Some(("provider".to_owned(), "one".to_owned()))
        );
        picker.select_next();
        assert_eq!(
            picker.selected(),
            Some(("provider".to_owned(), "two".to_owned()))
        );
        assert_eq!(picker.selected_pool().as_deref(), Some("provider"));
    }
}
