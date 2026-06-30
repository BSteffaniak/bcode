//! Slash command registry metadata for the TUI.

use bcode_client::BcodeClient;
use bcode_skill_models::SkillId;

/// Static metadata for a builtin slash command name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinSlashCommand {
    name: &'static str,
    draft_safe: bool,
}

impl BuiltinSlashCommand {
    /// Return the command name without a leading slash.
    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }

    /// Return whether the command can run before a persisted session exists.
    #[must_use]
    pub const fn draft_safe(self) -> bool {
        self.draft_safe
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
    BuiltinSlashCommand {
        name: "sessions",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "resync",
        draft_safe: false,
    },
    BuiltinSlashCommand {
        name: "rescan-imports",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "new",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "plan",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "build",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "agent",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "compact",
        draft_safe: false,
    },
    BuiltinSlashCommand {
        name: "model",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "models",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "set-model",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "provider",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "set-provider",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "context-strategy",
        draft_safe: false,
    },
    BuiltinSlashCommand {
        name: "context",
        draft_safe: false,
    },
    BuiltinSlashCommand {
        name: "diff",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "cwd",
        draft_safe: false,
    },
    BuiltinSlashCommand {
        name: "worktree",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "worktrees",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "fork",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "clone",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "ralph",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "goal",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "skills",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "skill",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "thinking",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "timeline",
        draft_safe: true,
    },
    BuiltinSlashCommand {
        name: "stop",
        draft_safe: false,
    },
    BuiltinSlashCommand {
        name: "cancel-runtime",
        draft_safe: false,
    },
    BuiltinSlashCommand {
        name: "runtime",
        draft_safe: false,
    },
    BuiltinSlashCommand {
        name: "status",
        draft_safe: false,
    },
];

const STATIC_COMPLETIONS: &[SlashCompletion] = &[
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
        command: "/diff",
        description: "Toggle diff panel",
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
    /// Command did not resolve to any known slash command.
    Unknown,
}

impl SlashResolution {
    /// Return true when the resolution is a known slash command.
    #[must_use]
    pub const fn is_known(&self) -> bool {
        matches!(self, Self::Builtin(_) | Self::SkillAlias { .. })
    }

    /// Return true when the command can run before a persisted session exists.
    #[must_use]
    pub const fn is_draft_safe(&self) -> bool {
        match self {
            Self::Builtin(command) => command.draft_safe(),
            Self::SkillAlias { .. } => true,
            Self::Unknown => false,
        }
    }
}

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
