//! `[plugins.config."bcode.code_review".conversation]`: how a pull request's conversation shows
//! and stays current. See `docs/review-forge-conversation.md`.
//!
//! ```toml
//! [plugins.config."bcode.code_review".conversation]
//! replies = "send_now"     # or "with_review"
//! overview = "expanded"    # the pull request cards: or "auto", "collapsed"
//! notify = "reviews_and_replies"  # or "all", "reviews", "none"
//! fold_resolved = true
//! outdated_on_file = true
//!
//! # Threads (yours and pull requests'); see `docs/review-thread-cards.md`.
//! [plugins.config."bcode.code_review".conversation.threads]
//! fold_viewed = true
//! resolve_marks_viewed = true
//! new_comments = "reopen"  # or "keep_folded": a folded thread stays folded, marked new
//! card = "compact"         # header and buttons in the frame; or "roomy": rows of their own
//! width = 110              # widest a thread card gets; 0 for the whole width
//!
//! [plugins.config."bcode.code_review".conversation.signals]
//! provider = true          # ask providers that can say when conversations change
//! min_gap_ms = 2000        # least time between two reads of the same conversations
//! pause_unfocused = true   # stop asking while the terminal is not focused
//!
//! [plugins.config."bcode.code_review".conversation.signals.providers.<provider id>]
//! # passed to that provider's watch as is (for example `interval_ms`)
//! ```

use serde::Deserialize;
use std::collections::BTreeMap;

/// Where a reply to a remote thread goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyMode {
    /// Posted at once; *Add to review* holds it instead.
    #[default]
    SendNow,
    /// Held for the next review; *Send now* posts it at once instead.
    WithReview,
}

/// How a pull request's card shows before its files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverviewMode {
    /// The status line only, unless something is new.
    Auto,
    /// The status line only.
    Collapsed,
    /// Open: the description, reviews, checks, and conversation.
    #[default]
    Expanded,
}

/// What a new comment does to a folded thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewCommentsMode {
    /// It opens again.
    #[default]
    Reopen,
    /// It stays folded, with a *new* chip.
    KeepFolded,
}

/// How a thread card is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadCardMode {
    /// The header in the top border and the buttons in the bottom one: no rows for the frame.
    #[default]
    Compact,
    /// The header and the buttons on rows of their own inside the frame.
    Roomy,
}

/// Which new activity makes a toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyMode {
    /// Reviews, and replies in threads the viewer took part in (or on their pull request).
    #[default]
    ReviewsAndReplies,
    /// Every review and comment.
    All,
    /// Reviews only.
    Reviews,
    /// Nothing.
    None,
}

/// `conversation.signals`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SignalsConfig {
    /// Ask providers that can say when conversations change.
    pub provider: bool,
    /// Least time between two reads of the same conversations, in milliseconds.
    pub min_gap_ms: u64,
    /// Stop asking providers while the terminal is not focused.
    pub pause_unfocused: bool,
    /// Settings passed to each provider's watch, by provider id.
    pub providers: BTreeMap<String, toml::Value>,
}

impl Default for SignalsConfig {
    fn default() -> Self {
        Self {
            provider: true,
            min_gap_ms: 2_000,
            pause_unfocused: true,
            providers: BTreeMap::new(),
        }
    }
}

impl SignalsConfig {
    /// The settings for `provider`'s watch, as JSON.
    #[must_use]
    pub fn provider_settings(&self, provider: &str) -> serde_json::Value {
        self.providers
            .get(provider)
            .and_then(|value| serde_json::to_value(value).ok())
            .unwrap_or(serde_json::Value::Null)
    }
}

/// The `conversation` table.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConversationConfig {
    /// Show pull request conversations at all.
    pub enabled: bool,
    /// Where replies to remote threads go.
    pub replies: ReplyMode,
    /// How the overview shows.
    pub overview: OverviewMode,
    /// Which new activity makes a toast.
    pub notify: NotifyMode,
    /// Resolved threads fold by themselves.
    pub fold_resolved: bool,
    /// Thread cards and their viewed and folded states.
    pub threads: ThreadsConfig,
    /// Threads whose code could not be followed show on their file (else only in the threads
    /// panel).
    pub outdated_on_file: bool,
    /// When conversations are read again.
    pub signals: SignalsConfig,
}

/// `conversation.threads`: thread cards and their viewed and folded states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThreadsConfig {
    /// Viewed threads fold by themselves.
    pub fold_viewed: bool,
    /// Resolving a thread yourself marks it viewed.
    pub resolve_marks_viewed: bool,
    /// What a new comment does to a folded thread.
    pub new_comments: NewCommentsMode,
    /// How thread cards are laid out.
    pub card: ThreadCardMode,
    /// Widest a thread card gets, in cells; 0 for the whole width.
    pub width: u16,
}

impl Default for ThreadsConfig {
    fn default() -> Self {
        Self {
            fold_viewed: true,
            resolve_marks_viewed: true,
            new_comments: NewCommentsMode::default(),
            card: ThreadCardMode::default(),
            width: 110,
        }
    }
}

impl ConversationConfig {
    /// What folds a thread by itself.
    #[must_use]
    pub const fn fold_rules(&self) -> bcode_code_review::threads::FoldRules {
        bcode_code_review::threads::FoldRules {
            fold_viewed: self.threads.fold_viewed,
            fold_resolved: self.fold_resolved,
            new_comments: match self.threads.new_comments {
                NewCommentsMode::Reopen => bcode_code_review::threads::NewComments::Reopen,
                NewCommentsMode::KeepFolded => bcode_code_review::threads::NewComments::KeepFolded,
            },
        }
    }
}

impl Default for ConversationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            replies: ReplyMode::default(),
            overview: OverviewMode::default(),
            notify: NotifyMode::default(),
            fold_resolved: true,
            threads: ThreadsConfig::default(),
            outdated_on_file: true,
            signals: SignalsConfig::default(),
        }
    }
}

impl ConversationConfig {
    /// The `conversation` table of the plugin `table`, with a diagnostic when it is unreadable
    /// (and the defaults then).
    #[must_use]
    pub fn from_plugin_table(table: &toml::Table) -> (Self, Option<String>) {
        let Some(value) = table.get("conversation") else {
            return (Self::default(), None);
        };
        match value.clone().try_into::<Self>() {
            Ok(config) => (config, None),
            Err(error) => (
                Self::default(),
                Some(format!(
                    "plugins.config.\"{}\".conversation: {}",
                    super::PLUGIN_ID,
                    error.to_string().lines().next().unwrap_or_default()
                )),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (ConversationConfig, Option<String>) {
        let table: toml::Table = toml::from_str(text).expect("toml");
        ConversationConfig::from_plugin_table(&table)
    }

    #[test]
    fn every_setting_has_a_default_and_can_be_changed() {
        assert_eq!(parse(""), (ConversationConfig::default(), None));
        let (config, problem) = parse(
            "[conversation]\nreplies = \"with_review\"\noverview = \"collapsed\"\nnotify = \"none\"\n\
             [conversation.signals]\nmin_gap_ms = 500\n\
             [conversation.signals.providers.forge]\ninterval_ms = 60000\n",
        );
        assert_eq!(problem, None);
        assert_eq!(config.replies, ReplyMode::WithReview);
        assert_eq!(config.overview, OverviewMode::Collapsed);
        assert_eq!(config.notify, NotifyMode::None);
        assert_eq!(config.signals.min_gap_ms, 500);
        assert!(config.signals.provider);
        assert_eq!(
            config.signals.provider_settings("forge"),
            serde_json::json!({ "interval_ms": 60000 })
        );
        assert_eq!(
            config.signals.provider_settings("other"),
            serde_json::Value::Null
        );
    }

    #[test]
    fn a_bad_value_is_reported_and_the_defaults_used() {
        let (config, problem) = parse("[conversation]\nreplies = \"later\"\n");
        assert_eq!(config, ConversationConfig::default());
        assert!(problem.is_some_and(|problem| problem.contains("conversation")));
    }
}
