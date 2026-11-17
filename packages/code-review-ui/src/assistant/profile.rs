//! The review assistant's agent profile and the tools the app provides.
//!
//! * **Profile.** `bcode.code-review.assistant` ("Review assistant") is contributed by the app's
//!   tool provider connection, based on Bcode's `build` agent and restrict-only: its tool
//!   selection is the union of the enabled [`Category`] tool lists plus `extra`, intersected by
//!   Bcode with what `build` exposes; every call is still decided by `build`'s permission rules.
//! * **Categories.** Configured under `[plugins.config."bcode.code_review".assistant.tools]`,
//!   every one on by default: review reads, review writes, publish, navigation, file reads, file
//!   writes, shell. A category set to `false` removes its tools; an unknown key or a non-boolean
//!   value is reported and the default is used.
//! * **Client tools.** The tools the app itself answers, under its provider id, with their policy
//!   class fixed at registration.

use super::actions;
use bcode_agent_profile::client_profile::ClientAgentProfile;
use bcode_tool_models::client_tool::{ClientToolDescriptor, ClientToolPolicy};
use std::collections::BTreeMap;

/// Provider id of the app's tools, and the app id of its sessions' origin.
pub const PROVIDER_ID: &str = crate::host::REVIEW_SESSION_APP_ID;
/// Id of the assistant's agent profile.
pub const PROFILE_ID: &str = "bcode.code-review.assistant";
/// Agent the profile narrows.
pub const BASE_AGENT: &str = "build";
/// How long a client tool call may run, in milliseconds.
const TOOL_DEADLINE_MS: u32 = 30_000;

/// A group of tools the user can turn off for the assistant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    /// Reading reviews: plugin read tools and the app's view of the screen.
    ReviewReads,
    /// Changing reviews: comments, replies, resolution, viewed marks, suggestions.
    ReviewWrites,
    /// Publishing review comments.
    Publish,
    /// Moving the user within the app.
    Navigation,
    /// Reading files.
    FileReads,
    /// Writing files.
    FileWrites,
    /// Running commands.
    Shell,
}

impl Category {
    /// Every category, in configuration order.
    pub const ALL: [Self; 7] = [
        Self::ReviewReads,
        Self::ReviewWrites,
        Self::Publish,
        Self::Navigation,
        Self::FileReads,
        Self::FileWrites,
        Self::Shell,
    ];

    /// Configuration key.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::ReviewReads => "review_reads",
            Self::ReviewWrites => "review_writes",
            Self::Publish => "publish",
            Self::Navigation => "navigation",
            Self::FileReads => "file_reads",
            Self::FileWrites => "file_writes",
            Self::Shell => "shell",
        }
    }

    /// The tools of the category.
    #[must_use]
    pub const fn tools(self) -> &'static [&'static str] {
        match self {
            Self::ReviewReads => &[
                "code_review.list_reviews",
                "code_review.find_paths",
                "code_review.summary",
                "code_review.list_files",
                "code_review.file_diff",
                "code_review.file_content",
                "code_review.list_threads",
                "code_review.get_thread",
                "code_review.pull_request",
                "code_review.search_code",
                "code_review.diff_hunks",
                "code_review.related_files",
                "code_review.guide.get",
                "code_review.guide.check",
                "code_review.guide.plan",
                "code_review.guide.export",
                CURRENT_VIEW,
                actions::FIND_PULL_REQUESTS,
            ],
            Self::ReviewWrites => &[
                "code_review.add_comment",
                "code_review.reply",
                "code_review.resolve_thread",
                "code_review.suggest_change",
                "code_review.mark_viewed",
                "code_review.add_comments",
                "code_review.resolve_threads",
                "code_review.create_review",
                "code_review.update_review",
                "code_review.delete_review",
                actions::COMPLETE_PASS,
            ],
            Self::Publish => &[
                "code_review.publish",
                actions::PUBLISH,
                actions::PULL_REQUEST_REVIEW,
            ],
            Self::Navigation => &[
                actions::GO_TO,
                actions::SET_COMPARISON,
                actions::SET_VIEW,
                actions::REFRESH,
                actions::OPEN_REVIEW,
            ],
            Self::FileReads => &[
                "filesystem.read",
                "filesystem.list",
                "filesystem.find",
                "filesystem.grep",
                "filesystem.stat",
                "filesystem.exists",
            ],
            Self::FileWrites => &[
                "filesystem.write",
                "filesystem.edit",
                "filesystem.multi_edit",
            ],
            Self::Shell => &["shell.run"],
        }
    }
}

/// Name of the client tool that returns the screen.
pub const CURRENT_VIEW: &str = "bcode.code-review.current_view";

/// The assistant's configuration (`[plugins.config."bcode.code_review".assistant]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantConfig {
    /// Whether each category is on.
    pub categories: BTreeMap<Category, bool>,
    /// Exact names of other Bcode tools to expose.
    pub extra: Vec<String>,
    /// The model new conversations and card sessions get until one is chosen in the app:
    /// `model` or `provider/model`.
    pub model: Option<String>,
    /// The reasoning effort they get until one is chosen in the app.
    pub effort: Option<String>,
    /// The right dock's width until one is dragged.
    pub width: DockLength,
    /// The bottom dock's height until one is dragged.
    pub height: DockLength,
}

/// A docked panel's default size along its axis (`assistant.width` or `assistant.height`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockLength {
    /// A number of columns or rows.
    Cells(u16),
    /// A percentage (1 to 100) of the app's width or height.
    Percent(u16),
}

impl Default for DockLength {
    fn default() -> Self {
        Self::Percent(40)
    }
}

impl DockLength {
    /// Read a configured value: a positive integer counts cells, a string such as `"40%"` is a
    /// percentage from 1 to 100. `None` when it is neither.
    #[must_use]
    pub fn parse(value: &toml::Value) -> Option<Self> {
        match value {
            toml::Value::Integer(cells) => u16::try_from(*cells)
                .ok()
                .filter(|cells| *cells > 0)
                .map(Self::Cells),
            toml::Value::String(text) => text
                .trim()
                .strip_suffix('%')
                .and_then(|percent| percent.trim().parse::<u16>().ok())
                .filter(|percent| (1..=100).contains(percent))
                .map(Self::Percent),
            _ => None,
        }
    }

    /// The cells this size takes of `length` cells, before the dock's limits apply.
    #[must_use]
    pub fn cells_of(self, length: u16) -> u16 {
        match self {
            Self::Cells(cells) => cells,
            Self::Percent(percent) => {
                u16::try_from(u32::from(length) * u32::from(percent) / 100).unwrap_or(length)
            }
        }
    }
}

impl std::fmt::Display for DockLength {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cells(cells) => write!(formatter, "{cells}"),
            Self::Percent(percent) => write!(formatter, "\"{percent}%\""),
        }
    }
}

impl Default for AssistantConfig {
    fn default() -> Self {
        Self {
            categories: Category::ALL
                .into_iter()
                .map(|category| (category, true))
                .collect(),
            extra: Vec::new(),
            model: None,
            effort: None,
            width: DockLength::default(),
            height: DockLength::default(),
        }
    }
}

impl AssistantConfig {
    /// Read the plugin table's `assistant` table; problems are reported and defaults used.
    #[must_use]
    pub fn from_plugin_table(table: &toml::Table) -> (Self, Vec<String>) {
        let mut config = Self::default();
        let mut problems = Vec::new();
        let prefix = format!("plugins.config.\"{}\".assistant", crate::config::PLUGIN_ID);
        let Some(assistant) = table.get("assistant") else {
            return (config, problems);
        };
        let Some(assistant) = assistant.as_table() else {
            problems.push(format!("{prefix} must be a table"));
            return (config, problems);
        };
        for (key, slot) in [("model", &mut config.model), ("effort", &mut config.effort)] {
            match assistant.get(key) {
                None => {}
                Some(toml::Value::String(text)) if !text.trim().is_empty() => {
                    *slot = Some(text.trim().to_owned());
                }
                Some(_) => problems.push(format!("{prefix}.{key} must be a non-empty string")),
            }
        }
        for (key, slot, unit) in [
            ("width", &mut config.width, "columns"),
            ("height", &mut config.height, "rows"),
        ] {
            let Some(value) = assistant.get(key) else {
                continue;
            };
            match DockLength::parse(value) {
                Some(length) => *slot = length,
                None => problems.push(format!(
                    "{prefix}.{key} must be a number of {unit} or a percentage such as \"40%\"; \
                     using {}",
                    DockLength::default()
                )),
            }
        }
        let Some(tools) = assistant.get("tools") else {
            return (config, problems);
        };
        let Some(tools) = tools.as_table() else {
            problems.push(format!("{prefix}.tools must be a table"));
            return (config, problems);
        };
        for (key, value) in tools {
            if key == "extra" {
                match value.as_array().map(|names| {
                    names
                        .iter()
                        .map(|name| name.as_str().map(str::to_owned))
                        .collect::<Option<Vec<String>>>()
                }) {
                    Some(Some(names)) => config.extra = names,
                    _ => {
                        problems.push(format!("{prefix}.tools.extra must be a list of tool names"));
                    }
                }
                continue;
            }
            let Some(category) = Category::ALL
                .into_iter()
                .find(|category| category.key() == key)
            else {
                problems.push(format!("{prefix}.tools.{key} is not a tool category"));
                continue;
            };
            match value.as_bool() {
                Some(on) => {
                    config.categories.insert(category, on);
                }
                None => problems.push(format!("{prefix}.tools.{key} must be true or false")),
            }
        }
        (config, problems)
    }

    /// The configured app default: provider (when `model` names one), model and effort.
    #[must_use]
    pub fn default_settings(&self) -> bcode_code_review_models::ReviewAssistantSettings {
        let (provider, model) = match self.model.as_deref().map(|model| model.split_once('/')) {
            Some(Some((provider, model))) => (Some(provider.to_owned()), Some(model.to_owned())),
            Some(None) => (None, self.model.clone()),
            None => (None, None),
        };
        bcode_code_review_models::ReviewAssistantSettings {
            provider_plugin_id: provider,
            model_id: model,
            effort: self.effort.clone(),
            updated_at_ms: 0,
        }
    }

    /// The tool selection of the profile: the enabled categories' tools and `extra`, sorted.
    #[must_use]
    pub fn tools(&self) -> Vec<String> {
        let mut tools: Vec<String> = Category::ALL
            .into_iter()
            .filter(|category| self.categories.get(category).copied().unwrap_or(true))
            .flat_map(|category| category.tools().iter().map(|tool| (*tool).to_owned()))
            .chain(self.extra.iter().cloned())
            .collect();
        tools.sort();
        tools.dedup();
        tools
    }

    /// The assistant's agent profile.
    #[must_use]
    pub fn profile(&self) -> ClientAgentProfile {
        ClientAgentProfile {
            id: PROFILE_ID.to_owned(),
            name: "Review assistant".to_owned(),
            description: "Reviews code in the code review app: sees the screen, reads and \
                          changes reviews, and moves the user, under the build agent's \
                          permission rules."
                .to_owned(),
            base_agent: BASE_AGENT.to_owned(),
            instructions: INSTRUCTIONS.to_owned(),
            tools: self.tools(),
        }
    }
}

/// The profile's instructions.
const INSTRUCTIONS: &str = "You are the review assistant of the Bcode code review app. \
Each user message comes with a `review app view` block describing the screen the user is on \
(home, the new-review wizard, or a review: its id, kind, sources, stack layers and the current \
one, the active comparison, the file, cursor and selection) and, for that turn only, a `review \
app detail` block with the selected lines, threads and per-layer changes. Earlier view blocks \
describe where the user was when they wrote earlier messages; the screen may have changed since. \
Call bcode.code-review.current_view to read the screen as it is now, in full.

The code_review tools act on the review on screen when called without `review`; on home or in \
the wizard, or for another review, pass `review` with an id from code_review.list_reviews. \
The bcode.code-review tools act on the app itself: go_to, set_comparison and set_view change \
what the review on screen shows; open_review takes the user to another review, home or the \
wizard; refresh reloads. They answer with what is on screen afterwards.

Recipes:
* \"Create a review for my most recent PR stack\": bcode.code-review.find_pull_requests with \
filter mine (limit 1), then code_review.create_review with that stack's create_review_source, \
then bcode.code-review.open_review with the new id.
* \"Create a review for my uncommitted changes in <dir>\": code_review.create_review with \
sources [{kind: working_tree}] and directory <dir>, then open_review. When the result says \
other_repository, tell the user to run `bcode review` in that repository to see it.
* \"Review PR #N\" / a URL / a branch / a range / commits: code_review.create_review with that \
kind (find_pull_requests first when you need the stack of a pull request), then open_review.
* \"Create a review of X\", \"a review of X as it is now\", \"all of X\", \"review the X folder\" \
(X a folder, package, crate, plugin or file): code_review.find_paths with X, then \
code_review.create_review with sources [{kind: files, paths: [<the best match's path>]}], then \
open_review. Ask only when find_paths says ambiguous (which one), says weak (whether its best \
match is what the user meant), or finds nothing; never ask about commits or ranges for such a \
review.
* \"X's changes\", \"what changed in X\": a diff review (working_tree with paths, or the branch, \
range or pull request the user names), not a files review.
* \"The whole repository\": code_review.create_review with sources [{kind: repository}].
* \"Mark these files viewed\": one code_review.mark_viewed call with paths, a glob, layers, all, \
or unchanged_since_look; never one call per file.
* Comparisons (\"what changed since I looked\", \"since the last push\", \"v2 to v3 of #12\", \
\"between pushes 2 and 4\", \"the stack as it was yesterday\"): bcode.code-review.set_comparison; \
\"show v2 of #12\": bcode.code-review.go_to with layer and version.
* \"Set up a new review of X but let me check it first\": bcode.code-review.open_review with \
screen new_review, a title and sources (the wizard opens filled in for the user to confirm).
* Archive, rename, delete reviews: code_review.update_review / code_review.delete_review \
(delete only when asked; prefer archiving).

Never run the bcode command line through the shell to create or open reviews: the tools above \
create every kind of review the app can. When a tool cannot do what was asked, say what is \
missing instead.

Do only what the user asked; ask before acting on anything else. Changes to reviews, files, \
publishing, commands and moving the user to another screen may ask the user for permission; \
publishing and pull request verdicts open a dialog the user confirms.";

/// The tools the app answers itself.
#[must_use]
pub fn client_tools() -> Vec<ClientToolDescriptor> {
    let mut tools = vec![ClientToolDescriptor {
        name: CURRENT_VIEW.to_owned(),
        description: "The screen the user is looking at in the code review app, live: which \
                      screen (home, wizard, review), the review (id, title, kind, sources with \
                      pull request numbers and heads), its stack layers and the current one, \
                      the active comparison (baseline, view, toggles, each layer's or other \
                      source's result and changed files, the stack's recent pushes), the file, \
                      cursor and selected lines, threads on the file and the focused thread, \
                      and loaded pull request state. Pass `sections` to read only some of it \
                      and `max_files` to list fewer files per layer; a shortened list has a \
                      count of what it left out beside it (`files_omitted`, `omitted`). A \
                      `truncated` object says what was left out to fit."
            .to_owned(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "sections": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": [
                            "screen", "review", "stack", "comparison", "file", "selection",
                            "threads", "pull_request", "home", "wizard"
                        ]
                    },
                    "description": "Sections to return; all when omitted."
                },
                "max_files": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Files listed per layer or source at most (default 100)."
                }
            },
            "additionalProperties": false
        }),
        policy: ClientToolPolicy::ReadOnly,
        max_duration_ms: TOOL_DEADLINE_MS,
    }];
    tools.extend(actions::descriptors());
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (AssistantConfig, Vec<String>) {
        AssistantConfig::from_plugin_table(&toml::from_str(text).expect("toml"))
    }

    #[test]
    fn every_category_is_on_by_default_and_each_can_be_turned_off() {
        let (config, problems) = parse("");
        assert_eq!(problems, Vec::<String>::new());
        let tools = config.tools();
        for category in Category::ALL {
            for tool in category.tools() {
                assert!(tools.iter().any(|name| name == tool), "{tool} by default");
            }
        }
        let (config, problems) = parse(
            "[assistant.tools]\nshell = false\nfile_writes = false\nextra = [\"web.fetch\"]\n",
        );
        assert_eq!(problems, Vec::<String>::new());
        let tools = config.tools();
        assert!(!tools.iter().any(|tool| tool.starts_with("shell.")));
        assert!(!tools.iter().any(|tool| tool == "filesystem.write"));
        assert!(tools.iter().any(|tool| tool == "filesystem.read"));
        assert!(tools.iter().any(|tool| tool == "web.fetch"));
        let profile = config.profile();
        assert_eq!(profile.id, PROFILE_ID);
        assert_eq!(profile.base_agent, BASE_AGENT);
        assert_eq!(profile.tools, tools);
    }

    #[test]
    fn the_profile_finds_paths_and_creates_reviews_of_them_as_they_are_now() {
        let (config, _) = parse("");
        let profile = config.profile();
        for tool in ["code_review.find_paths", "code_review.create_review"] {
            assert!(profile.tools.iter().any(|name| name == tool), "{tool}");
        }
        assert!(
            Category::ReviewReads
                .tools()
                .contains(&"code_review.find_paths")
        );
        let (config, _) = parse("[assistant.tools]\nreview_reads = false\n");
        assert!(
            !config
                .tools()
                .iter()
                .any(|tool| tool == "code_review.find_paths")
        );
    }

    #[test]
    fn model_and_effort_seed_the_app_default() {
        let (config, problems) =
            parse("[assistant]\nmodel = \"bcode.fake-provider/fake-echo\"\neffort = \"high\"\n");
        assert_eq!(problems, Vec::<String>::new());
        let settings = config.default_settings();
        assert_eq!(
            settings.provider_plugin_id.as_deref(),
            Some("bcode.fake-provider")
        );
        assert_eq!(settings.model_id.as_deref(), Some("fake-echo"));
        assert_eq!(settings.effort.as_deref(), Some("high"));
        let (config, _) = parse("[assistant]\nmodel = \"fake-echo\"\n");
        assert_eq!(config.default_settings().provider_plugin_id, None);
        let (config, problems) = parse("[assistant]\nmodel = 3\n");
        assert_eq!(config.model, None);
        assert_eq!(problems.len(), 1, "{problems:?}");
    }

    #[test]
    fn misspelled_or_mistyped_settings_are_reported_and_the_default_kept() {
        let (config, problems) = parse("[assistant.tools]\nshel = false\npublish = \"no\"\n");
        assert_eq!(config, AssistantConfig::default());
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems[0].contains("publish must be true or false")
                || problems[1].contains("publish")
        );
    }

    #[test]
    fn the_profile_is_a_valid_contribution_and_its_tools_are_ones_build_exposes() {
        let config = AssistantConfig::default();
        let profile = config.profile();
        assert_eq!(
            bcode_agent_profile::client_profile::validate_client_agent_profiles(
                PROVIDER_ID,
                std::slice::from_ref(&profile),
            ),
            Ok(())
        );
        // Every plugin tool the categories name is a tool Bcode's build agent is offered, so a
        // renamed tool fails here instead of silently leaving the assistant.
        let defaults = bcode_default_agents_plugin::default_config();
        let build = bcode_agent_policy::active_tools_for(&bcode_agent_policy::agent_config(
            &defaults, BASE_AGENT,
        ));
        let client: Vec<String> = client_tools().into_iter().map(|tool| tool.name).collect();
        for tool in profile.tools {
            if tool.starts_with(&format!("{PROVIDER_ID}.")) {
                assert!(client.contains(&tool), "{tool} is a client tool of the app");
            } else {
                assert!(
                    build.contains(&tool),
                    "the build agent is offered {tool}: {build:?}"
                );
            }
        }
    }
}
