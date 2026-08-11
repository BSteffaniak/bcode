//! Dedicated root-owned session search interaction state.
//!
//! This module owns only TUI interaction and presentation choices. Portable query semantics,
//! provider eligibility, hydration, coverage, and maintenance policy remain server-owned.

use bcode_session_search::{
    SearchContentKind, SearchCursor, SearchField, SessionSearchExecutionClass,
    SessionSearchFilters, SessionSearchPlanPolicy, SessionSearchQuery, SessionSearchRequest,
    SessionSearchSort, TextMatchMode,
};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Maximum search hits requested/rendered by the dedicated root surface.
pub const TUI_SEARCH_PAGE_SIZE: usize = 20;

/// Parsed dedicated-search input after extracting optional textual power-user controls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSessionSearchInput {
    pub query: String,
    pub controls: SessionSearchControls,
}

/// Parse optional validated textual controls while retaining ordinary text as the query.
///
/// Supported controls are `mode:`, `deep:`, `content:`, `cwd:`, `after:`, `before:`,
/// `provider:`, `model:`, `agent:`, `tool:`, `status:`, `field:`, and `sort:`.
/// Repeating a set-valued control adds another value.
///
/// # Errors
///
/// Returns a concise error for unknown values, invalid timestamps, or a query containing only
/// controls. Portable request validation remains authoritative after parsing.
pub fn parse_search_input(
    input: &str,
    mut controls: SessionSearchControls,
) -> Result<ParsedSessionSearchInput, String> {
    let mut query = Vec::new();
    for token in input.split_whitespace() {
        let Some((name, value)) = token.split_once(':') else {
            query.push(token);
            continue;
        };
        if value.is_empty() {
            return Err(format!("{name}: requires a value"));
        }
        match name {
            "mode" => controls.match_mode = parse_match_mode(value)?,
            "deep" => {
                controls.depth = match value {
                    "true" | "on" | "yes" => SessionSearchDepth::Deep,
                    "false" | "off" | "no" => SessionSearchDepth::Ordinary,
                    _ => return Err("deep: expects on/off".to_owned()),
                };
            }
            "content" => {
                controls
                    .filters
                    .content_kinds
                    .insert(parse_content_kind(value)?);
            }
            "cwd" => controls.filters.working_directory = Some(PathBuf::from(value)),
            "after" => controls.filters.after_timestamp_ms = Some(parse_timestamp(name, value)?),
            "before" => {
                controls.filters.before_timestamp_ms = Some(parse_timestamp(name, value)?);
            }
            "provider" => {
                controls.filters.providers.insert(value.to_owned());
            }
            "model" => {
                controls.filters.models.insert(value.to_owned());
            }
            "agent" => {
                controls.filters.agents.insert(value.to_owned());
            }
            "tool" => {
                controls.filters.tool_names.insert(value.to_owned());
            }
            "status" | "tool-status" => {
                controls.filters.tool_statuses.insert(value.to_owned());
            }
            "field" => {
                controls.fields.insert(parse_field(value)?);
            }
            "sort" => controls.sort = parse_sort(value)?,
            _ => query.push(token),
        }
    }
    let query = query.join(" ");
    if query.trim().is_empty() {
        return Err("search query must include text in addition to controls".to_owned());
    }
    let request = controls.request(query.clone(), None);
    request.validate().map_err(|error| error.to_string())?;
    controls
        .depth
        .policy()
        .validate(&request)
        .map_err(|error| error.to_string())?;
    Ok(ParsedSessionSearchInput { query, controls })
}

fn parse_timestamp(name: &str, value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("{name}: expects Unix milliseconds"))
}

fn parse_match_mode(value: &str) -> Result<TextMatchMode, String> {
    match value {
        "terms" => Ok(TextMatchMode::Terms),
        "phrase" => Ok(TextMatchMode::Phrase),
        "prefix" => Ok(TextMatchMode::Prefix),
        "fuzzy" => Ok(TextMatchMode::Fuzzy),
        "regex" => Ok(TextMatchMode::Regex),
        _ => Err(format!("unknown match mode '{value}'")),
    }
}

fn parse_sort(value: &str) -> Result<SessionSearchSort, String> {
    match value {
        "relevance" => Ok(SessionSearchSort::ProviderRelevance),
        "newest" => Ok(SessionSearchSort::NewestFirst),
        "oldest" => Ok(SessionSearchSort::OldestFirst),
        "session" => Ok(SessionSearchSort::SessionThenSequence),
        _ => Err(format!("unknown sort '{value}'")),
    }
}

fn parse_content_kind(value: &str) -> Result<SearchContentKind, String> {
    match value {
        "title" => Ok(SearchContentKind::SessionTitle),
        "user" => Ok(SearchContentKind::UserMessage),
        "assistant" => Ok(SearchContentKind::AssistantMessage),
        "reasoning" => Ok(SearchContentKind::AssistantReasoning),
        "system" => Ok(SearchContentKind::SystemMessage),
        "shell-command" => Ok(SearchContentKind::ShellCommand),
        "shell-output" => Ok(SearchContentKind::ShellOutput),
        "tool-arguments" => Ok(SearchContentKind::ToolArguments),
        "tool-output" => Ok(SearchContentKind::ToolOutput),
        "tool-error" => Ok(SearchContentKind::ToolError),
        "permission" => Ok(SearchContentKind::Permission),
        "diagnostic" => Ok(SearchContentKind::RuntimeDiagnostic),
        "compaction" => Ok(SearchContentKind::Compaction),
        "trace" => Ok(SearchContentKind::TraceMetadata),
        "artifact" => Ok(SearchContentKind::ArtifactMetadata),
        _ => Err(format!("unknown content kind '{value}'")),
    }
}

fn parse_field(value: &str) -> Result<SearchField, String> {
    match value {
        "title" => Ok(SearchField::Title),
        "text" => Ok(SearchField::Text),
        "command" => Ok(SearchField::Command),
        "stdout" => Ok(SearchField::StandardOutput),
        "stderr" => Ok(SearchField::StandardError),
        "tool" => Ok(SearchField::ToolName),
        "arguments" => Ok(SearchField::ToolArguments),
        "error" => Ok(SearchField::ErrorMessage),
        "cwd" => Ok(SearchField::WorkingDirectory),
        "provider" => Ok(SearchField::Provider),
        "model" => Ok(SearchField::Model),
        "agent" => Ok(SearchField::Agent),
        "source" => Ok(SearchField::Source),
        _ => Err(format!("unknown search field '{value}'")),
    }
}

/// Discoverable textual filter/help summary for renderer surfaces.
pub const SEARCH_CONTROL_HELP: &str = "controls: mode:terms|phrase|prefix|fuzzy|regex deep:on|off content:<kind> cwd:<path> after:<ms> before:<ms> provider:<id> model:<id> agent:<id> tool:<name> status:<status> field:<field> sort:relevance|newest|oldest|session";

/// Explicit ordinary versus deep search intent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SessionSearchDepth {
    #[default]
    Ordinary,
    /// Explicitly permit eligible high-volume local scan providers.
    Deep,
}

impl SessionSearchDepth {
    /// Return server-owned planning policy for this explicit depth choice.
    #[must_use]
    pub const fn policy(self) -> SessionSearchPlanPolicy {
        SessionSearchPlanPolicy {
            execution_class: if matches!(self, Self::Deep) {
                SessionSearchExecutionClass::Deep
            } else {
                SessionSearchExecutionClass::Ordinary
            },
            maximum_staleness_sequences: Some(0),
            allow_remote: false,
            per_provider_deadline_ms: 2_000,
        }
    }

    /// Concise privacy/storage explanation for the current choice.
    #[must_use]
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::Ordinary => {
                "ordinary: indexed local transcript search; compressed scans excluded"
            }
            Self::Deep => {
                "deep: explicitly scans eligible retained compressed shell/tool output locally"
            }
        }
    }
}

/// Discoverable renderer-neutral search controls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSearchControls {
    pub match_mode: TextMatchMode,
    pub fields: BTreeSet<SearchField>,
    pub filters: SessionSearchFilters,
    pub sort: SessionSearchSort,
    pub depth: SessionSearchDepth,
}

impl Default for SessionSearchControls {
    fn default() -> Self {
        Self {
            match_mode: TextMatchMode::Terms,
            fields: BTreeSet::new(),
            filters: SessionSearchFilters::default(),
            sort: SessionSearchSort::ProviderRelevance,
            depth: SessionSearchDepth::Ordinary,
        }
    }
}

impl SessionSearchControls {
    /// Cycle terms, phrase, prefix, fuzzy, and regex match behavior.
    pub const fn cycle_match_mode(&mut self) {
        self.match_mode = match self.match_mode {
            TextMatchMode::Terms => TextMatchMode::Phrase,
            TextMatchMode::Phrase => TextMatchMode::Prefix,
            TextMatchMode::Prefix => TextMatchMode::Fuzzy,
            TextMatchMode::Fuzzy => TextMatchMode::Regex,
            TextMatchMode::Regex => TextMatchMode::Terms,
        };
    }

    /// Toggle explicit ordinary/deep intent.
    pub const fn toggle_depth(&mut self) {
        self.depth = match self.depth {
            SessionSearchDepth::Ordinary => SessionSearchDepth::Deep,
            SessionSearchDepth::Deep => SessionSearchDepth::Ordinary,
        };
    }

    /// Cycle deterministic result ordering.
    pub const fn cycle_sort(&mut self) {
        self.sort = match self.sort {
            SessionSearchSort::ProviderRelevance => SessionSearchSort::NewestFirst,
            SessionSearchSort::NewestFirst => SessionSearchSort::OldestFirst,
            SessionSearchSort::OldestFirst => SessionSearchSort::SessionThenSequence,
            SessionSearchSort::SessionThenSequence => SessionSearchSort::ProviderRelevance,
        };
    }

    /// Set discoverable content filters.
    pub fn set_content_kinds(&mut self, content: impl IntoIterator<Item = SearchContentKind>) {
        self.filters.content_kinds = content.into_iter().collect();
    }

    /// Set exact normalized working-directory scope.
    pub fn set_working_directory(&mut self, directory: Option<PathBuf>) {
        self.filters.working_directory = directory;
    }

    /// Set inclusive timestamp bounds.
    pub const fn set_timestamp_range(&mut self, after: Option<u64>, before: Option<u64>) {
        self.filters.after_timestamp_ms = after;
        self.filters.before_timestamp_ms = before;
    }

    /// Set provider/model/agent filters.
    pub fn set_runtime_filters(
        &mut self,
        providers: BTreeSet<String>,
        models: BTreeSet<String>,
        agents: BTreeSet<String>,
    ) {
        self.filters.providers = providers;
        self.filters.models = models;
        self.filters.agents = agents;
    }

    /// Set tool name and tool-status filters.
    pub fn set_tool_filters(&mut self, tools: BTreeSet<String>, statuses: BTreeSet<String>) {
        self.filters.tool_names = tools;
        self.filters.tool_statuses = statuses;
    }

    /// Build one portable bounded request. Validation remains at the portable/server boundary.
    #[must_use]
    pub fn request(&self, text: String, cursor: Option<SearchCursor>) -> SessionSearchRequest {
        SessionSearchRequest {
            query: SessionSearchQuery::Text {
                text,
                mode: self.match_mode,
                fields: self.fields.clone(),
            },
            filters: self.filters.clone(),
            sort: self.sort,
            limit: TUI_SEARCH_PAGE_SIZE,
            cursor,
            deadline_ms: Some(5_000),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn textual_power_controls_cover_discoverable_filters_and_reject_invalid_values() {
        let parsed = parse_search_input(
            "mode:regex deep:on content:tool-output cwd:/work after:10 before:20 provider:p model:m agent:a tool:shell status:failed field:error sort:newest needle",
            SessionSearchControls::default(),
        )
        .expect("power controls");
        assert_eq!(parsed.query, "needle");
        assert_eq!(parsed.controls.match_mode, TextMatchMode::Regex);
        assert_eq!(parsed.controls.depth, SessionSearchDepth::Deep);
        assert!(
            parsed
                .controls
                .filters
                .content_kinds
                .contains(&SearchContentKind::ToolOutput)
        );
        assert_eq!(
            parsed.controls.filters.working_directory,
            Some(PathBuf::from("/work"))
        );
        assert_eq!(parsed.controls.filters.after_timestamp_ms, Some(10));
        assert_eq!(parsed.controls.filters.before_timestamp_ms, Some(20));
        assert!(parsed.controls.filters.providers.contains("p"));
        assert!(parsed.controls.filters.models.contains("m"));
        assert!(parsed.controls.filters.agents.contains("a"));
        assert!(parsed.controls.filters.tool_names.contains("shell"));
        assert!(parsed.controls.filters.tool_statuses.contains("failed"));
        assert!(parsed.controls.fields.contains(&SearchField::ErrorMessage));
        assert_eq!(parsed.controls.sort, SessionSearchSort::NewestFirst);
        assert!(parse_search_input("mode:nope needle", SessionSearchControls::default()).is_err());
        assert!(parse_search_input("deep:on", SessionSearchControls::default()).is_err());
    }

    #[test]
    fn dedicated_controls_cover_modes_depth_sort_and_portable_filters() {
        let mut controls = SessionSearchControls::default();
        let modes = [
            TextMatchMode::Terms,
            TextMatchMode::Phrase,
            TextMatchMode::Prefix,
            TextMatchMode::Fuzzy,
            TextMatchMode::Regex,
        ];
        for expected in modes {
            assert_eq!(controls.match_mode, expected);
            controls.cycle_match_mode();
        }
        assert_eq!(controls.match_mode, TextMatchMode::Terms);
        controls.toggle_depth();
        assert_eq!(
            controls.depth.policy().execution_class,
            SessionSearchExecutionClass::Deep
        );
        assert!(controls.depth.explanation().contains("compressed"));
        controls.cycle_sort();
        assert_eq!(controls.sort, SessionSearchSort::NewestFirst);
        controls.set_working_directory(Some(PathBuf::from("/workspace")));
        controls.set_timestamp_range(Some(10), Some(20));
        controls.set_runtime_filters(
            BTreeSet::from(["provider".to_owned()]),
            BTreeSet::from(["model".to_owned()]),
            BTreeSet::from(["agent".to_owned()]),
        );
        controls.set_tool_filters(
            BTreeSet::from(["shell".to_owned()]),
            BTreeSet::from(["failed".to_owned()]),
        );
        let request = controls.request("needle".to_owned(), None);
        assert_eq!(request.limit, TUI_SEARCH_PAGE_SIZE);
        request.validate().expect("portable request");
    }
}
