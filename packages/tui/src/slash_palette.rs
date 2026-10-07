//! Slash completion state for the TUI.

use super::slash_registry;
use bcode_client::BcodeClient;
use bcode_command::{CommandContribution, CommandSurface};
use bcode_session_models::SessionId;

use bcode_command::completion::{
    CompletionMatchMode, completion_match_rank, normalize_completion as normalize,
};
use bcode_config::{CompletionConfig, CompletionFilteredSortOrder};
use std::collections::BTreeMap;

/// Discovery scope; argument edits reuse candidates without another RPC.
pub fn discovery_scope(query: &str) -> String {
    let parts = query
        .trim_start_matches('/')
        .split_whitespace()
        .collect::<Vec<_>>();
    match parts.first().copied() {
        Some("agent" | "skill" | "model" | "set-model" | "provider" | "set-provider") => {
            parts[0].to_owned()
        }
        Some("thinking") if matches!(parts.get(1), Some(&"effort" | &"summary")) => {
            format!("thinking {}", parts[1])
        }
        _ => String::new(),
    }
}

/// Facts that affect candidate discovery, without presentation-derived identities.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SlashCompletionContext {
    pub session_id: Option<SessionId>,
    pub working_directory: Option<std::path::PathBuf>,
    pub provider_plugin_id: Option<String>,
    pub model_id: Option<String>,
}

/// Composer-local discovery cache and stale-result fence.
///
/// Only the finite built-in discovery scopes are retained. Successful discovery stays
/// fresh for 60 seconds; the next composer refresh renews expired discovery atomically.
/// Reload and catalog-change notifications can explicitly invalidate it sooner. Failed
/// discovery is never retained as a successful cache entry.
#[derive(Debug, Default)]
pub struct SlashCompletionState {
    context: SlashCompletionContext,
    candidates: BTreeMap<String, (std::time::Instant, SlashPalette)>,
    pending: Option<(u64, String)>,
    generation: u64,
    dismissed_query: Option<String>,
    pub settings: CompletionConfig,
}

impl SlashCompletionState {
    pub fn set_context(&mut self, context: SlashCompletionContext) -> bool {
        if self.context != context {
            self.context = context;
            self.candidates.clear();
            self.pending = None;
            self.dismissed_query = None;
            self.generation = self.generation.wrapping_add(1);
            return true;
        }
        false
    }

    pub fn invalidate(&mut self) {
        self.candidates.clear();
        self.pending = None;
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn dismiss(&mut self, query: &str) {
        self.generation = self.generation.wrapping_add(1);
        self.pending = None;
        self.dismissed_query = Some(query.to_owned());
    }

    pub fn is_dismissed(&mut self, query: &str) -> bool {
        if self.dismissed_query.as_deref() == Some(query) {
            true
        } else {
            self.dismissed_query = None;
            false
        }
    }

    pub fn snapshot(&self, query: &str) -> Option<SlashPalette> {
        let (_, cached) = self.candidates.get(&discovery_scope(query))?;
        let mut palette = cached.clone();
        palette.refilter(query, self.settings);
        Some(palette)
    }

    pub fn cached(&self, query: &str) -> Option<SlashPalette> {
        let (discovered_at, cached) = self.candidates.get(&discovery_scope(query))?;
        if discovered_at.elapsed() >= std::time::Duration::from_mins(1) {
            return None;
        }
        let mut palette = cached.clone();
        palette.refilter(query, self.settings);
        Some(palette)
    }

    pub fn request(&mut self, query: &str) -> Option<u64> {
        let scope = discovery_scope(query);
        if self
            .pending
            .as_ref()
            .is_some_and(|(_, pending)| *pending == scope)
        {
            return None;
        }
        self.generation = self.generation.wrapping_add(1);
        self.pending = Some((self.generation, scope));
        Some(self.generation)
    }

    pub fn accept(&mut self, generation: u64, query: &str, palette: SlashPalette) -> bool {
        if self.pending.as_ref() != Some(&(generation, discovery_scope(query))) {
            return false;
        }
        self.pending = None;
        if palette.discovery_complete {
            self.candidates
                .insert(discovery_scope(query), (std::time::Instant::now(), palette));
        }
        true
    }
}

/// Slash completion picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashPalette {
    discovery_complete: bool,
    candidates: Vec<SlashItem>,
    items: Vec<SlashItem>,
    explicitly_selected: bool,
    selected: usize,
    query: String,
}

/// One slash completion item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashItem {
    skill: bool,
    command: String,
    description: String,
}

/// Visible slash completion item with its source index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleSlashItem<'a> {
    /// Source item index.
    pub source_index: usize,
    /// Completion item.
    pub item: &'a SlashItem,
}

impl SlashPalette {
    /// Discover candidates for the active provider and session.
    pub(super) async fn discover(
        client: &BcodeClient,
        session_id: Option<SessionId>,
        provider_plugin_id: Option<String>,
        query: &str,
    ) -> Self {
        let discovery = slash_items(client, session_id, provider_plugin_id, query).await;
        let discovery_complete = discovery.is_some();
        let candidates = discovery.unwrap_or_else(|| Self::fallback_candidates(query));
        let mut palette = Self {
            discovery_complete,
            candidates,
            items: Vec::new(),
            explicitly_selected: false,
            selected: 0,
            query: String::new(),
        };
        palette.refilter(query, CompletionConfig::default());
        palette
    }

    fn fallback_candidates(query: &str) -> Vec<SlashItem> {
        if discovery_scope(query).is_empty() {
            static_items()
        } else {
            Vec::new()
        }
    }

    pub(super) fn immediate(query: &str, settings: CompletionConfig) -> Self {
        let mut palette = Self {
            discovery_complete: false,
            candidates: Self::fallback_candidates(query),
            items: Vec::new(),
            explicitly_selected: false,
            selected: 0,
            query: String::new(),
        };
        palette.refilter(query, settings);
        palette
    }

    /// Refilter cached discovery synchronously, retaining only intentional navigation.
    pub fn refilter(&mut self, query: &str, settings: CompletionConfig) {
        let previous = self.explicit_selection().map(str::to_owned);
        self.items = filter_items_with_settings(self.candidates.clone(), query, settings);
        query.clone_into(&mut self.query);
        self.selected = 0;
        self.explicitly_selected = false;
        if let Some(previous) = previous {
            self.select_command(&previous);
        }
    }

    pub(super) fn explicit_selection(&self) -> Option<&str> {
        if self.explicitly_selected {
            self.selected_command()
        } else {
            None
        }
    }

    /// Return true if there are no completions.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Return the selected source item index.
    #[must_use]
    pub fn selected_index(&self) -> usize {
        self.selected.min(self.items.len().saturating_sub(1))
    }

    /// Return the query text used to build the completions.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Return the number of completion items.
    #[must_use]
    pub const fn item_count(&self) -> usize {
        self.items.len()
    }

    /// Return visible items for the popup height.
    pub fn visible_items(&self, height: usize) -> impl Iterator<Item = VisibleSlashItem<'_>> {
        let selected = self.selected_index();
        let start = selected.saturating_sub(height.saturating_sub(1));
        self.items
            .iter()
            .enumerate()
            .skip(start)
            .take(height)
            .map(|(source_index, item)| VisibleSlashItem { source_index, item })
    }

    /// Return the currently selected command.
    #[must_use]
    pub fn selected_command(&self) -> Option<&str> {
        self.items
            .get(self.selected_index())
            .map(|item| item.command.as_str())
    }

    /// Select a command if it exists in the current completion list.
    pub fn select_command(&mut self, command: &str) {
        if let Some(index) = self.items.iter().position(|item| item.command == command) {
            self.selected = index;
            self.explicitly_selected = true;
        }
    }

    /// Select an item by visible row, returning the selected command.
    pub fn select_visible_row(&mut self, row: usize, height: usize) -> Option<&str> {
        let selected = self.selected_index();
        let start = selected.saturating_sub(height.saturating_sub(1));
        self.explicitly_selected = true;
        self.selected = start
            .saturating_add(row)
            .min(self.items.len().saturating_sub(1));
        self.selected_command()
    }

    /// Move selection to previous item.
    pub const fn move_previous(&mut self) {
        self.explicitly_selected = true;
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move selection to next item.
    pub fn move_next(&mut self) {
        self.explicitly_selected = true;
        self.selected = self
            .selected
            .saturating_add(1)
            .min(self.items.len().saturating_sub(1));
    }

    /// Return whether the selected command exactly matches current composer text.
    #[must_use]
    pub fn selected_matches(&self, text: &str) -> bool {
        self.selected_command()
            .is_some_and(|command| text == command.trim_end())
    }

    #[cfg(test)]
    pub fn from_items(items: Vec<(&str, &str)>) -> Self {
        let items: Vec<_> = items
            .into_iter()
            .map(|(command, description)| item(command, description))
            .collect();
        Self {
            candidates: items.clone(),
            items,
            discovery_complete: true,
            explicitly_selected: false,
            selected: 0,
            query: String::new(),
        }
    }
}

impl SlashItem {
    /// Whether this completion invokes a discovered skill.
    #[must_use]
    pub const fn is_skill(&self) -> bool {
        self.skill
    }

    /// Return replacement command text.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }

    /// Return item description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }
}

async fn slash_items(
    client: &BcodeClient,
    session_id: Option<SessionId>,
    provider_plugin_id: Option<String>,
    query: &str,
) -> Option<Vec<SlashItem>> {
    let trimmed = query.trim_start_matches('/');
    let parts = trimmed.split_whitespace().collect::<Vec<_>>();
    let candidates = if parts.first() == Some(&"agent") {
        let agents = client.list_agents().await.ok()?;
        agents
            .into_iter()
            .map(|agent| item(format!("/agent {}", agent.id), agent.name))
            .collect()
    } else if matches!(parts.first(), Some(&"skill")) {
        let skills = client.list_skills().await.ok()?;
        skills
            .skills
            .into_iter()
            .flat_map(|skill| {
                let description = skill
                    .description
                    .unwrap_or_else(|| "invoke skill".to_owned());
                [
                    skill_item(format!("/skill {}", skill.id), description),
                    skill_item(format!("/skill describe {}", skill.id), "describe skill"),
                ]
            })
            .collect()
    } else if matches!(parts.first(), Some(&"model" | &"set-model")) {
        let models = client.session_model_list(provider_plugin_id).await.ok()?;
        models
            .models
            .into_iter()
            .map(|model| {
                item(
                    format!("/{} {}", parts[0], model.model_id),
                    model.display_name,
                )
            })
            .collect()
    } else if matches!(parts.first(), Some(&"provider" | &"set-provider")) {
        let services = client.plugin_services().await.ok()?;
        services
            .into_iter()
            .filter(|service| service.interface_id == bcode_model::MODEL_PROVIDER_INTERFACE_ID)
            .map(|service| {
                item(
                    format!("/{} {}", parts[0], service.plugin_id),
                    service.name.unwrap_or_else(|| "model provider".to_owned()),
                )
            })
            .collect()
    } else if matches!(parts.first(), Some(&"thinking"))
        && matches!(parts.get(1), Some(&"effort" | &"summary"))
    {
        let status = client.session_model_status(session_id?).await.ok()?;
        thinking_items(parts[1], status.reasoning.as_ref())
    } else {
        let mut items = static_items();
        {
            let contributions = client.plugin_contributions().await.ok()?;
            items.extend(plugin_slash_items(contributions.command_contributions));
        }
        {
            let skills = client.list_skills().await.ok()?;
            items.extend(skills.skills.into_iter().filter_map(|skill| {
                slash_registry::is_non_conflicting_skill_alias(&skill.id).then(|| {
                    let description = skill
                        .description
                        .unwrap_or_else(|| format!("Invoke skill {}", skill.name));
                    skill_item(format!("/{}", skill.id), description)
                })
            }));
        }
        items
    };
    Some(candidates)
}

fn thinking_items(
    subcommand: &str,
    reasoning: Option<&bcode_model::ModelReasoningInfo>,
) -> Vec<SlashItem> {
    let values = match subcommand {
        "effort" => reasoning.map_or(&[][..], |reasoning| reasoning.effort_values.as_slice()),
        "summary" => reasoning.map_or(&[][..], |reasoning| reasoning.summary_values.as_slice()),
        _ => &[],
    };
    values
        .iter()
        .map(|value| {
            item(
                format!("/thinking {subcommand} {value}"),
                "provider-supported reasoning value",
            )
        })
        .collect()
}

#[cfg(test)]
fn filter_items(items: Vec<SlashItem>, query: &str) -> Vec<SlashItem> {
    filter_items_with_settings(items, query, CompletionConfig::default())
}

fn filter_items_with_settings(
    items: Vec<SlashItem>,
    query: &str,
    settings: CompletionConfig,
) -> Vec<SlashItem> {
    let mode = match settings.match_mode {
        bcode_config::CompletionMatchMode::Fuzzy => CompletionMatchMode::Fuzzy,
        bcode_config::CompletionMatchMode::Prefix => CompletionMatchMode::Prefix,
        bcode_config::CompletionMatchMode::Substring => CompletionMatchMode::Substring,
    };
    let relevance = !normalize(query).is_empty()
        && settings.filtered_sort_order == CompletionFilteredSortOrder::Relevance;
    let mut matches = items
        .into_iter()
        .filter_map(|item| {
            completion_match_rank(&item.command, &item.description, query, mode)
                .map(|rank| (rank, normalize(&item.command), item))
        })
        .collect::<Vec<_>>();
    matches.sort_by(
        |(left_rank, left_name, left), (right_rank, right_name, right)| {
            (if relevance {
                left_rank.cmp(right_rank)
            } else {
                std::cmp::Ordering::Equal
            })
            .then_with(|| left_name.cmp(right_name))
            .then_with(|| left.command.cmp(&right.command))
            .then_with(|| left.description.cmp(&right.description))
        },
    );
    matches.into_iter().map(|(_, _, item)| item).collect()
}

fn plugin_slash_items(
    contributions: impl IntoIterator<Item = CommandContribution>,
) -> Vec<SlashItem> {
    contributions
        .into_iter()
        .filter(|contribution| contribution.supports_surface(&CommandSurface::Slash))
        .filter_map(|contribution| {
            let slash_name = contribution.slash_name()?.to_owned();
            if slash_registry::is_builtin_command_name(&slash_name) {
                return None;
            }
            let description = contribution
                .description
                .unwrap_or_else(|| contribution.title.clone());
            Some(item(format!("/{slash_name}"), description))
        })
        .collect()
}

fn static_items() -> Vec<SlashItem> {
    slash_registry::static_completions()
        .map(|completion| item(completion.command(), completion.description()))
        .collect()
}

fn skill_item(command: impl Into<String>, description: impl Into<String>) -> SlashItem {
    let mut result = item(command, description);
    result.skill = true;
    result
}

fn item(command: impl Into<String>, description: impl Into<String>) -> SlashItem {
    SlashItem {
        skill: false,
        command: command.into(),
        description: description.into(),
    }
}

#[cfg(test)]
mod tests {
    fn palette(commands: &[&str]) -> super::SlashPalette {
        let candidates = commands
            .iter()
            .map(|command| super::item(*command, ""))
            .collect();
        let mut palette = super::SlashPalette {
            discovery_complete: true,
            candidates,
            items: Vec::new(),
            explicitly_selected: false,
            selected: 0,
            query: String::new(),
        };
        palette.refilter("/", bcode_config::CompletionConfig::default());
        palette
    }

    #[test]
    fn typing_refilters_and_only_preserves_explicit_navigation() {
        let mut palette = palette(&["/other", "/resume-progress-doc", "/workflow.resume"]);
        palette.refilter("/resume", bcode_config::CompletionConfig::default());
        assert_eq!(palette.selected_command(), Some("/resume-progress-doc"));
        palette.move_next();
        palette.refilter("/resum", bcode_config::CompletionConfig::default());
        assert_eq!(palette.selected_command(), Some("/workflow.resume"));
        palette.refilter(
            "/resume-progress",
            bcode_config::CompletionConfig::default(),
        );
        assert_eq!(palette.selected_command(), Some("/resume-progress-doc"));
    }

    #[test]
    fn discovery_cache_refilters_without_reloading() {
        let mut state = super::SlashCompletionState::default();
        let generation = state.request("/r").unwrap();
        assert!(state.request("/re").is_none());
        assert!(state.accept(generation, "/r", palette(&["/resume", "/other"])));
        assert_eq!(
            state.cached("/resume").unwrap().selected_command(),
            Some("/resume")
        );
        assert!(state.cached("/model x").is_none());
    }

    #[test]
    fn dismissal_and_context_changes_fence_late_results() {
        let mut state = super::SlashCompletionState::default();
        state.set_context(super::SlashCompletionContext {
            model_id: Some("a".to_owned()),
            ..Default::default()
        });
        let generation = state.request("/r").unwrap();
        state.dismiss("/r");
        assert!(!state.accept(generation, "/r", palette(&["/resume"])));
        assert!(state.is_dismissed("/r"));
        assert!(!state.is_dismissed("/re"));
        let generation = state.request("/re").unwrap();
        state.set_context(super::SlashCompletionContext {
            model_id: Some("b".to_owned()),
            ..Default::default()
        });
        assert!(!state.accept(generation, "/re", palette(&["/resume"])));
        assert!(state.cached("/re").is_none());
    }

    #[test]
    fn deterministic_ties_ignore_discovery_order() {
        let first = super::filter_items(
            vec![
                super::skill_item("/zresume", ""),
                super::item("/aresume", ""),
            ],
            "resume",
        );
        let second = super::filter_items(
            vec![
                super::item("/aresume", ""),
                super::skill_item("/zresume", ""),
            ],
            "resume",
        );
        assert_eq!(first, second);
        assert_eq!(first[0].command(), "/aresume");
    }

    #[test]
    fn alphabetical_and_inherit_override_filtered_relevance() {
        for order in [
            bcode_config::CompletionFilteredSortOrder::Alphabetical,
            bcode_config::CompletionFilteredSortOrder::Inherit,
        ] {
            let mut palette = palette(&["/resume", "/aresume"]);
            palette.refilter(
                "/resume",
                bcode_config::CompletionConfig {
                    filtered_sort_order: order,
                    ..Default::default()
                },
            );
            assert_eq!(palette.selected_command(), Some("/aresume"));
        }
    }

    #[test]
    fn static_candidates_are_immediate_and_failed_discovery_is_retryable() {
        let immediate =
            super::SlashPalette::immediate("/think", bcode_config::CompletionConfig::default());
        assert!(!immediate.is_empty());
        let mut state = super::SlashCompletionState::default();
        let generation = state.request("/think").unwrap();
        assert!(state.accept(generation, "/think", immediate));
        assert!(state.cached("/think").is_none());
        assert!(state.request("/think").is_some());
    }

    #[test]
    fn invalidation_and_expiry_refresh_same_context() {
        let mut state = super::SlashCompletionState::default();
        let generation = state.request("/r").unwrap();
        state.accept(generation, "/r", palette(&["/resume"]));
        state.candidates.get_mut("").unwrap().0 = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(61))
            .expect("test clock supports cache expiry");
        assert!(state.cached("/r").is_none());
        assert_eq!(
            state.snapshot("/resume").unwrap().selected_command(),
            Some("/resume")
        );
        let generation = state.request("/r").unwrap();
        state.invalidate();
        assert!(!state.accept(generation, "/r", palette(&["/resume"])));
        let generation = state.request("/r").unwrap();
        assert!(state.accept(generation, "/r", palette(&["/replacement"])));
        assert_eq!(
            state.cached("/r").unwrap().selected_command(),
            Some("/replacement")
        );
    }

    #[test]
    fn dynamic_scopes_separate_aliases_and_thinking_values() {
        assert_eq!(
            super::discovery_scope("/model foo"),
            super::discovery_scope("/model bar")
        );
        assert_ne!(
            super::discovery_scope("/model foo"),
            super::discovery_scope("/set-model foo")
        );
        assert_ne!(
            super::discovery_scope("/thinking effort high"),
            super::discovery_scope("/thinking summary auto")
        );
    }

    #[test]
    fn skills_retain_their_source_and_are_searchable() {
        let items = super::filter_items(
            vec![
                super::item("/model", "Select model"),
                super::skill_item("/code-review", "Review code"),
            ],
            "code-review",
        );
        assert_eq!(items.len(), 1);
        assert!(items[0].is_skill());
        assert_eq!(items[0].command(), "/code-review");
    }

    use super::{filter_items, plugin_slash_items, static_items};
    use bcode_command::{
        CommandAction, CommandContribution, CommandExecution, CommandOwner, CommandSurface,
        SlashCommandContribution,
    };
    use std::collections::BTreeSet;

    fn contribution(id: &str, surfaces: BTreeSet<CommandSurface>) -> CommandContribution {
        CommandContribution {
            id: id.to_owned(),
            title: "Loop".to_owned(),
            description: Some("Start a deterministic prompt loop".to_owned()),
            category: Some("automation".to_owned()),
            slash: surfaces
                .contains(&CommandSurface::Slash)
                .then(|| SlashCommandContribution {
                    name: id.to_owned(),
                    aliases: BTreeSet::new(),
                }),
            surfaces,
            arguments: Vec::new(),
            session: bcode_command::CommandSessionRequirement::Optional,
            execution: CommandExecution::Immediate,
            owner: CommandOwner::Plugin {
                plugin_id: "bcode.loop".to_owned(),
            },
            action: CommandAction::Plugin {
                plugin_id: "bcode.loop".to_owned(),
                command_id: id.to_owned(),
            },
        }
    }

    #[test]
    fn plugin_slash_contributions_are_visible_and_selectable() {
        let mut contribution = contribution(
            "bcode.loop.start",
            BTreeSet::from([CommandSurface::Slash, CommandSurface::Palette]),
        );
        contribution.slash = Some(SlashCommandContribution {
            name: "loop".to_owned(),
            aliases: BTreeSet::new(),
        });
        let items = plugin_slash_items([contribution]);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].command(), "/loop");
        assert_eq!(items[0].description(), "Start a deterministic prompt loop");
    }

    #[test]
    fn non_slash_and_conflicting_plugin_contributions_are_hidden() {
        let items = plugin_slash_items([
            contribution("loop", BTreeSet::from([CommandSurface::Palette])),
            contribution("new", BTreeSet::from([CommandSurface::Slash])),
        ]);

        assert!(items.is_empty());
    }

    #[test]
    fn ranks_exact_matches_before_prefix_and_fuzzy_matches() {
        let items = filter_items(
            vec![
                super::item("/agent", "a thin agent"),
                super::item("/plan", "Switch to plan agent"),
                super::item("/thinking", "Open reasoning output settings"),
                super::item("/thin", "Short thinking alias"),
            ],
            "thin",
        );

        assert_eq!(items[0].command(), "/thin");
        assert_eq!(items[1].command(), "/thinking");
        assert_eq!(items[2].command(), "/agent");
    }

    #[test]
    fn ranks_prefix_matches_before_fuzzy_matches() {
        let items = filter_items(
            vec![
                super::item("/agent", "Set session agent by id"),
                super::item("/thinking", "Open reasoning output settings"),
            ],
            "t",
        );

        assert_eq!(items[0].command(), "/thinking");
        assert_eq!(items[1].command(), "/agent");
    }

    #[test]
    fn excludes_host_session_fork_commands() {
        let commands = static_items()
            .into_iter()
            .map(|item| item.command().to_owned())
            .collect::<Vec<_>>();

        assert!(commands.iter().all(|command| command != "/fork"));
        assert!(commands.iter().all(|command| command != "/clone"));
    }

    #[test]
    fn fuzzy_matches_non_contiguous_characters() {
        let items = filter_items(static_items(), "tm");

        assert!(items.iter().any(|item| item.command() == "/set-model "));
    }
}
