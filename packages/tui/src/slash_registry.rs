//! Slash command registry metadata for the TUI.

use bcode_client::BcodeClient;
use bcode_command::CommandContribution;
use bcode_skill_models::SkillId;

/// Stable identity for a host-owned slash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinCommandId {
    Version,
    Sessions,
    Search,
    Resync,
    RescanImports,
    New,
    Agent,
    Compact,
    Theme,
    Streaming,
    Model,
    AuthPool,
    Provider,
    Context,
    Cwd,
    Worktree,
    Ralph,
    Goal,
    Skills,
    Skill,
    Thinking,
    Timeline,
    Stop,
    CancelRuntime,
    Runtime,
}

/// Resolved host-owned slash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinSlashCommand {
    id: BuiltinCommandId,
}

impl BuiltinSlashCommand {
    /// Return the stable command identity.
    #[must_use]
    pub const fn id(self) -> BuiltinCommandId {
        self.id
    }
}

/// Static metadata for a slash completion item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCompletion {
    command: &'static str,
    description: &'static str,
}

impl SlashCompletion {
    /// Return replacement command text.
    #[must_use]
    pub const fn command(self) -> &'static str {
        self.command
    }

    /// Return completion description.
    #[must_use]
    pub const fn description(self) -> &'static str {
        self.description
    }
}

#[derive(Debug, Clone, Copy)]
struct BuiltinCommandSpec {
    id: BuiltinCommandId,
    names: &'static [&'static str],
    completions: &'static [SlashCompletion],
}

macro_rules! completion {
    ($command:literal, $description:literal) => {
        SlashCompletion {
            command: $command,
            description: $description,
        }
    };
}

const BUILTIN_COMMANDS: &[BuiltinCommandSpec] = &[
    BuiltinCommandSpec {
        id: BuiltinCommandId::Version,
        names: &["version"],
        completions: &[completion!("/version", "Show detailed build information")],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Sessions,
        names: &["sessions"],
        completions: &[completion!("/sessions", "Open session picker")],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Search,
        names: &["search"],
        completions: &[completion!("/search", "Search session transcripts")],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Resync,
        names: &["resync"],
        completions: &[completion!("/resync", "Resynchronize the active session")],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::RescanImports,
        names: &["rescan-imports"],
        completions: &[completion!(
            "/rescan-imports",
            "Rescan and open importable sessions"
        )],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::New,
        names: &["new"],
        completions: &[completion!("/new", "Create and switch to a new session")],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Agent,
        names: &["plan", "build", "agent"],
        completions: &[
            completion!("/plan", "Switch to plan agent"),
            completion!("/build", "Switch to build agent"),
            completion!("/agent ", "Set session agent by id"),
        ],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Compact,
        names: &["compact"],
        completions: &[completion!("/compact", "Compact current session context")],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Theme,
        names: &["theme"],
        completions: &[
            completion!("/theme", "Open interactive theme picker"),
            completion!("/theme preview ", "Preview bundled theme"),
            completion!("/theme apply ", "Persist bundled theme"),
            completion!("/theme cancel", "Cancel theme preview"),
        ],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Streaming,
        names: &["streaming"],
        completions: &[completion!(
            "/streaming",
            "Compare and tune streaming presentation"
        )],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Model,
        names: &["model", "models", "set-model"],
        completions: &[
            completion!("/model", "Open model picker"),
            completion!("/models", "Open model picker"),
            completion!("/set-model ", "Set model by id"),
        ],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::AuthPool,
        names: &["auth-pool", "subscriptions"],
        completions: &[
            completion!("/auth-pool", "Choose preferred provider subscription"),
            completion!("/subscriptions", "Choose preferred provider subscription"),
        ],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Provider,
        names: &["provider", "set-provider"],
        completions: &[
            completion!("/provider", "Show current provider"),
            completion!("/set-provider ", "Set provider by id"),
        ],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Context,
        names: &["context-strategy", "context"],
        completions: &[
            completion!("/context", "Show session context strategy"),
            completion!("/context-strategy", "Show session context strategy"),
        ],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Cwd,
        names: &["cwd"],
        completions: &[completion!("/cwd ", "Set session working directory")],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Worktree,
        names: &["worktree", "worktrees"],
        completions: &[
            completion!("/worktree", "Create worktree"),
            completion!("/worktrees", "Create worktree"),
            completion!("/worktree list", "List Git worktrees"),
            completion!("/worktree create", "Open worktree create dialog"),
            completion!("/worktree attach ", "Set session working directory"),
        ],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Ralph,
        names: &["ralph"],
        completions: &[
            completion!("/ralph", "Open Ralph UI"),
            completion!("/ralph ui", "Open Ralph UI"),
            completion!("/ralph start", "Start/setup Ralph loop"),
            completion!("/ralph run", "Prepare Ralph run"),
            completion!("/ralph approve", "Approve prepared Ralph run"),
            completion!("/ralph status", "Show Ralph status"),
            completion!("/ralph runs", "List Ralph runs"),
            completion!("/ralph iterations", "List Ralph iterations"),
            completion!("/ralph stop", "Stop active Ralph run"),
            completion!("/ralph resume", "Resume interrupted Ralph run"),
            completion!("/ralph audit", "Build Ralph audit prompt"),
            completion!("/ralph replan", "Build Ralph replan prompt"),
            completion!("/ralph open", "Open Ralph progress doc"),
        ],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Goal,
        names: &["goal"],
        completions: &[completion!("/goal", "Start/continue Ralph goal workflow")],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Skills,
        names: &["skills"],
        completions: &[completion!("/skills", "Open skill picker")],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Skill,
        names: &["skill"],
        completions: &[
            completion!("/skill ", "Invoke skill by id"),
            completion!("/skill describe ", "Describe skill by id"),
        ],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Thinking,
        names: &["thinking"],
        completions: &[
            completion!("/thinking", "Open reasoning output settings"),
            completion!("/thinking status", "Show reasoning output status"),
            completion!(
                "/thinking capabilities",
                "Show model reasoning capabilities"
            ),
            completion!(
                "/thinking effort",
                "Open reasoning settings focused on effort"
            ),
            completion!(
                "/thinking summary",
                "Open reasoning settings focused on summary"
            ),
        ],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Timeline,
        names: &["timeline"],
        completions: &[completion!("/timeline", "Browse user messages")],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Stop,
        names: &["stop"],
        completions: &[completion!("/stop", "Stop the active turn")],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::CancelRuntime,
        names: &["cancel-runtime"],
        completions: &[completion!("/cancel-runtime ", "Cancel runtime work by id")],
    },
    BuiltinCommandSpec {
        id: BuiltinCommandId::Runtime,
        names: &["runtime", "status"],
        completions: &[
            completion!("/runtime", "Show runtime status"),
            completion!("/status", "Show runtime status"),
        ],
    },
];

/// Resolution for a submitted slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashResolution {
    /// Command resolved to a builtin slash command.
    Builtin(BuiltinSlashCommand),
    /// Command resolved to a non-conflicting skill alias.
    SkillAlias {
        skill_id: SkillId,
        arguments: String,
    },
    /// Command resolved to a plugin-owned slash contribution.
    PluginCommand(Box<CommandContribution>),
    /// Command did not resolve to any known slash command.
    Unknown,
}

impl SlashResolution {}

/// Return slash completion metadata derived from the builtin command catalog.
pub fn static_completions() -> impl Iterator<Item = SlashCompletion> {
    BUILTIN_COMMANDS
        .iter()
        .flat_map(|command| command.completions.iter().copied())
}

/// Return the builtin command matching a command name without a leading slash.
#[must_use]
pub fn builtin_command(command: &str) -> Option<BuiltinSlashCommand> {
    BUILTIN_COMMANDS
        .iter()
        .find(|candidate| candidate.names.contains(&command))
        .map(|candidate| BuiltinSlashCommand { id: candidate.id })
}

/// Return true when the command name is a builtin slash command.
#[must_use]
pub fn is_builtin_command_name(command: &str) -> bool {
    builtin_command(command).is_some()
}

/// Return the first slash command token without its leading slash.
#[must_use]
pub fn slash_command_name(message: &str) -> Option<&str> {
    message
        .strip_prefix('/')
        .and_then(|command| command.split_whitespace().next())
}

fn slash_command_arguments(message: &str) -> String {
    message
        .strip_prefix('/')
        .and_then(|command| command.split_once(char::is_whitespace))
        .map_or_else(String::new, |(_command, arguments)| {
            arguments.trim_start().to_owned()
        })
}

/// Return true when a skill ID can be exposed as a top-level slash alias.
#[must_use]
pub fn is_non_conflicting_skill_alias(skill_id: &SkillId) -> bool {
    !is_builtin_command_name(skill_id.as_str())
}

/// Resolve a submitted slash command without executing side effects.
///
/// # Errors
///
/// Returns an error when dynamic skill discovery fails.
pub async fn resolve(
    client: &BcodeClient,
    message: &str,
) -> Result<SlashResolution, bcode_client::ClientError> {
    let Some(command) = slash_command_name(message) else {
        return Ok(SlashResolution::Unknown);
    };
    if let Some(builtin) = builtin_command(command) {
        return Ok(SlashResolution::Builtin(builtin));
    }
    let contributions = client.plugin_contributions().await?;
    if let Some(contribution) = contributions
        .command_contributions
        .into_iter()
        .find(|candidate| candidate.matches_slash_name(command))
    {
        return Ok(SlashResolution::PluginCommand(Box::new(contribution)));
    }
    let skills = client.list_skills().await?;
    let Some(skill) = skills
        .skills
        .into_iter()
        .find(|skill| skill.id.as_str() == command && is_non_conflicting_skill_alias(&skill.id))
    else {
        return Ok(SlashResolution::Unknown);
    };
    Ok(SlashResolution::SkillAlias {
        skill_id: skill.id,
        arguments: slash_command_arguments(message),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_is_discoverable_and_reserved_as_a_builtin() {
        assert!(static_completions().any(|completion| {
            completion.command() == "/search"
                && completion.description().contains("session transcripts")
        }));
        assert!(is_builtin_command_name("search"));
        assert!(!is_non_conflicting_skill_alias(&SkillId::new("search")));
    }

    #[test]
    fn fork_and_clone_are_not_host_builtins() {
        assert!(!is_builtin_command_name("fork"));
        assert!(!is_builtin_command_name("clone"));
        assert!(
            static_completions()
                .all(|completion| !matches!(completion.command(), "/fork" | "/clone"))
        );
    }

    #[test]
    fn every_builtin_name_is_discoverable_and_unique() {
        let mut names = std::collections::BTreeSet::new();
        for spec in BUILTIN_COMMANDS {
            assert!(
                !spec.completions.is_empty(),
                "builtin {:?} has no slash completion",
                spec.id
            );
            for name in spec.names {
                assert!(names.insert(*name), "duplicate builtin slash name: {name}");
                assert!(
                    spec.completions.iter().any(|completion| {
                        completion
                            .command()
                            .trim_start_matches('/')
                            .split_whitespace()
                            .next()
                            == Some(*name)
                    }),
                    "builtin slash name is not discoverable: {name}"
                );
                assert_eq!(
                    builtin_command(name).map(BuiltinSlashCommand::id),
                    Some(spec.id)
                );
            }
        }
    }

    #[test]
    fn cwd_is_discoverable_with_a_path_argument() {
        assert!(static_completions().any(|completion| {
            completion.command() == "/cwd "
                && completion.description().contains("working directory")
        }));
        assert_eq!(
            builtin_command("cwd").map(BuiltinSlashCommand::id),
            Some(BuiltinCommandId::Cwd)
        );
    }

    #[test]
    fn version_is_discoverable() {
        assert!(static_completions().any(|completion| {
            completion.command() == "/version"
                && completion.description().contains("build information")
        }));
    }
}
