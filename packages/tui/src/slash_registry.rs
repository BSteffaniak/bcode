//! Slash command registry metadata for the TUI.

use bcode_client::BcodeClient;
use bcode_command::{CommandContribution, CommandSurface};
use bcode_skill_models::SkillId;

/// Static metadata for a builtin slash command name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinSlashCommand {
    name: &'static str,
}

impl BuiltinSlashCommand {
    /// Return the command name without a leading slash.
    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
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

const BUILTIN_COMMANDS: &[BuiltinSlashCommand] = &[
    BuiltinSlashCommand { name: "version" },
    BuiltinSlashCommand { name: "sessions" },
    BuiltinSlashCommand { name: "resync" },
    BuiltinSlashCommand {
        name: "rescan-imports",
    },
    BuiltinSlashCommand { name: "new" },
    BuiltinSlashCommand { name: "plan" },
    BuiltinSlashCommand { name: "build" },
    BuiltinSlashCommand { name: "agent" },
    BuiltinSlashCommand { name: "compact" },
    BuiltinSlashCommand { name: "model" },
    BuiltinSlashCommand { name: "models" },
    BuiltinSlashCommand { name: "set-model" },
    BuiltinSlashCommand { name: "provider" },
    BuiltinSlashCommand {
        name: "set-provider",
    },
    BuiltinSlashCommand {
        name: "context-strategy",
    },
    BuiltinSlashCommand { name: "context" },
    BuiltinSlashCommand { name: "cwd" },
    BuiltinSlashCommand { name: "worktree" },
    BuiltinSlashCommand { name: "worktrees" },
    BuiltinSlashCommand { name: "fork" },
    BuiltinSlashCommand { name: "clone" },
    BuiltinSlashCommand { name: "ralph" },
    BuiltinSlashCommand { name: "goal" },
    BuiltinSlashCommand { name: "skills" },
    BuiltinSlashCommand { name: "skill" },
    BuiltinSlashCommand { name: "thinking" },
    BuiltinSlashCommand { name: "timeline" },
    BuiltinSlashCommand { name: "stop" },
    BuiltinSlashCommand {
        name: "cancel-runtime",
    },
    BuiltinSlashCommand { name: "runtime" },
    BuiltinSlashCommand { name: "status" },
];

const STATIC_COMPLETIONS: &[SlashCompletion] = &[
    SlashCompletion {
        command: "/version",
        description: "Show detailed build information",
    },
    SlashCompletion {
        command: "/plan",
        description: "Switch to plan agent",
    },
    SlashCompletion {
        command: "/build",
        description: "Switch to build agent",
    },
    SlashCompletion {
        command: "/sessions",
        description: "Open session picker",
    },
    SlashCompletion {
        command: "/new",
        description: "Create and switch to a new session",
    },
    SlashCompletion {
        command: "/compact",
        description: "Compact current session context",
    },
    SlashCompletion {
        command: "/model",
        description: "Open model picker",
    },
    SlashCompletion {
        command: "/models",
        description: "Open model picker",
    },
    SlashCompletion {
        command: "/set-model ",
        description: "Set model by id",
    },
    SlashCompletion {
        command: "/provider",
        description: "Show current provider",
    },
    SlashCompletion {
        command: "/set-provider ",
        description: "Set provider by id",
    },
    SlashCompletion {
        command: "/thinking",
        description: "Open reasoning output settings",
    },
    SlashCompletion {
        command: "/timeline",
        description: "Browse user messages",
    },
    SlashCompletion {
        command: "/thinking status",
        description: "Show reasoning output status",
    },
    SlashCompletion {
        command: "/thinking capabilities",
        description: "Show model reasoning capabilities",
    },
    SlashCompletion {
        command: "/thinking effort",
        description: "Open reasoning settings focused on effort",
    },
    SlashCompletion {
        command: "/thinking summary",
        description: "Open reasoning settings focused on summary",
    },
    SlashCompletion {
        command: "/fork",
        description: "Fork current session",
    },
    SlashCompletion {
        command: "/clone",
        description: "Clone current session",
    },
    SlashCompletion {
        command: "/worktree",
        description: "Create worktree",
    },
    SlashCompletion {
        command: "/worktrees",
        description: "Create worktree",
    },
    SlashCompletion {
        command: "/worktree list",
        description: "List Git worktrees",
    },
    SlashCompletion {
        command: "/worktree create",
        description: "Open worktree create dialog",
    },
    SlashCompletion {
        command: "/worktree attach ",
        description: "Set session working directory",
    },
    SlashCompletion {
        command: "/ralph",
        description: "Open Ralph UI",
    },
    SlashCompletion {
        command: "/ralph ui",
        description: "Open Ralph UI",
    },
    SlashCompletion {
        command: "/ralph start",
        description: "Start/setup Ralph loop",
    },
    SlashCompletion {
        command: "/ralph run",
        description: "Prepare Ralph run",
    },
    SlashCompletion {
        command: "/ralph approve",
        description: "Approve prepared Ralph run",
    },
    SlashCompletion {
        command: "/ralph status",
        description: "Show Ralph status",
    },
    SlashCompletion {
        command: "/ralph runs",
        description: "List Ralph runs",
    },
    SlashCompletion {
        command: "/ralph iterations",
        description: "List Ralph iterations",
    },
    SlashCompletion {
        command: "/ralph stop",
        description: "Stop active Ralph run",
    },
    SlashCompletion {
        command: "/ralph resume",
        description: "Resume interrupted Ralph run",
    },
    SlashCompletion {
        command: "/ralph audit",
        description: "Build Ralph audit prompt",
    },
    SlashCompletion {
        command: "/ralph replan",
        description: "Build Ralph replan prompt",
    },
    SlashCompletion {
        command: "/ralph open",
        description: "Open Ralph progress doc",
    },
    SlashCompletion {
        command: "/goal",
        description: "Start/continue Ralph goal workflow",
    },
    SlashCompletion {
        command: "/rescan-imports",
        description: "Rescan and open importable sessions",
    },
    SlashCompletion {
        command: "/skills",
        description: "Open skill picker",
    },
    SlashCompletion {
        command: "/agent ",
        description: "Set session agent by id",
    },
    SlashCompletion {
        command: "/skill ",
        description: "Invoke skill by id",
    },
    SlashCompletion {
        command: "/skill describe ",
        description: "Describe skill by id",
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
    PluginCommand(CommandContribution),
    /// Command did not resolve to any known slash command.
    Unknown,
}

impl SlashResolution {}

/// Return static builtin slash command metadata.
#[must_use]
pub const fn builtin_commands() -> &'static [BuiltinSlashCommand] {
    BUILTIN_COMMANDS
}

/// Return static slash completion metadata.
#[must_use]
pub const fn static_completions() -> &'static [SlashCompletion] {
    STATIC_COMPLETIONS
}

/// Return the builtin command matching a command name without a leading slash.
#[must_use]
pub fn builtin_command(command: &str) -> Option<BuiltinSlashCommand> {
    builtin_commands()
        .iter()
        .copied()
        .find(|candidate| candidate.name() == command)
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
        .find(|candidate| {
            candidate.supports_surface(&CommandSurface::Slash) && candidate.id == command
        })
    {
        return Ok(SlashResolution::PluginCommand(contribution));
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
    fn version_is_discoverable() {
        assert!(static_completions().iter().any(|completion| {
            completion.command() == "/version"
                && completion.description().contains("build information")
        }));
    }
}
