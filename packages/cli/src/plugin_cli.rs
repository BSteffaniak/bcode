//! Composition and dispatch for statically linked plugin CLI contributions.

use bcode_plugin::StaticBundledPlugin;
use bcode_plugin_sdk::StaticCliFuture;
use clap::Command;

#[derive(Debug, thiserror::Error)]
pub enum CompositionError {
    #[error("selected plugin CLI declaration or invocation failed: {0}")]
    Extension(String),
    #[error(
        "plugin CLI command `{command}` is contributed by both `{first_plugin}` and `{second_plugin}`"
    )]
    Conflict {
        command: String,
        first_plugin: String,
        second_plugin: String,
    },
}

pub struct Contribution {
    plugin_id: String,
    command_name: String,
    requires_daemon: fn(&clap::ArgMatches) -> bool,
    invoke: fn(clap::ArgMatches) -> StaticCliFuture,
    command: Command,
    extension: Option<crate::plugin_cli_extension::SelectedCliExtension>,
}

pub fn registrations(
    plugins: &[StaticBundledPlugin],
    plugin_ids: &[String],
) -> Result<Vec<Contribution>, CompositionError> {
    let mut contributions = Vec::new();
    for (plugin, plugin_id) in plugins.iter().zip(plugin_ids) {
        let Some(registration) = plugin.cli_registration() else {
            continue;
        };
        let command = (registration.command)();
        contributions.push(Contribution {
            plugin_id: plugin_id.clone(),
            command_name: command.get_name().to_owned(),
            requires_daemon: registration.requires_daemon,
            invoke: registration.invoke,
            command,
            extension: None,
        });
    }
    contributions.sort_by(|left, right| {
        left.command_name
            .cmp(&right.command_name)
            .then_with(|| left.plugin_id.cmp(&right.plugin_id))
    });
    validate(&contributions)?;
    Ok(contributions)
}

fn validate(contributions: &[Contribution]) -> Result<(), CompositionError> {
    let mut claimed = std::collections::BTreeMap::new();
    for entry in contributions {
        for name in std::iter::once(entry.command.get_name()).chain(entry.command.get_all_aliases())
        {
            if let Some(previous) = claimed.insert(name.to_owned(), entry.plugin_id.clone()) {
                return Err(CompositionError::Conflict {
                    command: name.to_owned(),
                    first_plugin: previous,
                    second_plugin: entry.plugin_id.clone(),
                });
            }
        }
    }
    for pair in contributions.windows(2) {
        if pair[0].command_name == pair[1].command_name {
            return Err(CompositionError::Conflict {
                command: pair[0].command_name.clone(),
                first_plugin: pair[0].plugin_id.clone(),
                second_plugin: pair[1].plugin_id.clone(),
            });
        }
    }
    Ok(())
}

pub fn compose(root: Command, contributions: &[Contribution]) -> Command {
    let mut root = root;
    for contribution in contributions {
        if root
            .get_subcommands()
            .any(|command| command.get_name() == contribution.command_name)
        {
            root =
                root.mut_subcommand(&contribution.command_name, |_| contribution.command.clone());
        } else {
            root = root.subcommand(contribution.command.clone());
        }
    }
    root
}

pub fn matched<'a>(
    matches: &clap::ArgMatches,
    contributions: &'a [Contribution],
) -> Option<&'a Contribution> {
    let (name, _) = matches.subcommand()?;
    contributions
        .iter()
        .find(|contribution| contribution.command_name == name)
}

/// Extract host-global selection options without interpreting plugin argv. Host
/// globals are reserved by the root parser even after subcommands; `--` ends them.
pub fn selection_matches(
    argv: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<clap::ArgMatches, clap::Error> {
    let argv = argv.into_iter().collect::<Vec<_>>();
    let mut command = crate::root_command();
    command.build();
    let ids = ["context", "profile", "request_timeout_secs", "config_only"];
    let arguments = command
        .get_arguments()
        .filter(|arg| ids.contains(&arg.get_id().as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let mut selected = vec![std::ffi::OsString::from("bcode")];
    let mut index = 1;
    while index < argv.len() {
        let Some(token) = argv[index].to_str() else {
            index += 1;
            continue;
        };
        if token == "--" {
            break;
        }
        let spelling = token.split_once('=').map_or(token, |(name, _)| name);
        if !token.starts_with('-') {
            break;
        }
        let host_argument = command.get_arguments().find(|arg| {
            arg.get_long()
                .is_some_and(|long| spelling == format!("--{long}"))
                || arg
                    .get_short()
                    .is_some_and(|short| spelling == format!("-{short}"))
        });
        if arguments.iter().any(|arg| {
            arg.get_long()
                .is_some_and(|long| spelling == format!("--{long}"))
        }) {
            selected.push(argv[index].clone());
            if !token.contains('=') {
                index += 1;
                if let Some(value) = argv.get(index) {
                    selected.push(value.clone());
                }
            }
        } else if host_argument.is_some_and(|arg| arg.get_action().takes_values())
            && !token.contains('=')
        {
            index += 1;
        }
        index += 1;
    }
    Command::new("bcode")
        .args(arguments)
        .try_get_matches_from(selected)
}

/// Preserve leaf argv while stripping reserved host globals. Reject non-UTF8
/// extension argv rather than silently replacing bytes before author parsing.
pub fn extension_arguments(command: &Command) -> Result<Vec<String>, String> {
    let argv = std::env::args_os().collect::<Vec<_>>();
    let mut root = crate::root_command_with_build_info(crate::build_info());
    root.build();
    let mut index = 1;
    let mut found = false;
    let mut arguments = Vec::new();
    while index < argv.len() {
        let token = argv[index]
            .to_str()
            .ok_or("non-UTF8 plugin CLI arguments are unsupported")?;
        if found && token == "--" {
            for value in &argv[index..] {
                arguments.push(
                    value
                        .to_str()
                        .ok_or("non-UTF8 plugin CLI arguments are unsupported")?
                        .to_owned(),
                );
            }
            break;
        }
        let (spelling, equals) = token
            .split_once('=')
            .map_or((token, false), |(name, _)| (name, true));
        let global = root.get_arguments().find(|arg| {
            arg.is_global_set()
                && (arg
                    .get_long()
                    .is_some_and(|long| spelling == format!("--{long}"))
                    || arg
                        .get_short()
                        .is_some_and(|short| spelling == format!("-{short}")))
        });
        if let Some(global) = global {
            if found {
                // Without clap source-scope indices, a token can also be a
                // hyphen-accepting plugin value. Never strip it and invoke a
                // different operation; require explicit unambiguous placement.
                return Err("native CLI host-global options must precede the plugin command (or follow -- as plugin data)".to_owned());
            }
            if global.get_action().takes_values() && !equals {
                index += 1;
            }
        } else if !found {
            if token == command.get_name() || command.get_all_aliases().any(|alias| alias == token)
            {
                found = true;
            } else if token.starts_with('-') {
                let host = root.get_arguments().find(|arg| {
                    arg.get_long()
                        .is_some_and(|long| spelling == format!("--{long}"))
                });
                if host.is_some_and(|arg| arg.get_action().takes_values()) && !equals {
                    index += 1;
                }
            } else {
                return Err("cannot determine plugin CLI argv boundary".to_owned());
            }
        } else {
            arguments.push(token.to_owned());
        }
        index += 1;
    }
    if !found {
        return Err("cannot determine plugin CLI command token".to_owned());
    }
    Ok(arguments)
}

pub async fn add_extensions(
    contributions: &mut Vec<Contribution>,
    extensions: &[crate::plugin_cli_extension::SelectedCliExtension],
) -> Result<(), CompositionError> {
    for extension in extensions {
        let descriptor = match extension.describe().await {
            Ok(Some(descriptor)) => descriptor,
            Ok(None) => continue,
            Err(_) => {
                // Optional plugin damage must not prevent unrelated host commands.
                // Its known legacy command stays unavailable; never fall back.
                tracing::warn!(plugin_id = %extension.plugin_id, "selected plugin CLI descriptor unavailable");
                continue;
            }
        };
        descriptor.validate().map_err(|_| {
            CompositionError::Extension("invalid portable CLI descriptor".to_owned())
        })?;
        let command = descriptor.command();
        if contributions
            .iter()
            .any(|entry| entry.plugin_id == extension.plugin_id)
        {
            return Err(CompositionError::Extension(
                "duplicate selected plugin packaging".to_owned(),
            ));
        }
        contributions.push(Contribution {
            plugin_id: extension.plugin_id.clone(),
            command_name: descriptor.name,
            requires_daemon: |_| false,
            invoke: |_| {
                Box::pin(async { Err("native CLI requires extension dispatch".to_owned()) })
            },
            command,
            extension: Some(extension.clone()),
        });
    }
    contributions.sort_by(|a, b| a.command_name.cmp(&b.command_name));
    validate(contributions)
}

impl Contribution {
    pub const fn extension(&self) -> Option<&crate::plugin_cli_extension::SelectedCliExtension> {
        self.extension.as_ref()
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }
    pub const fn command(&self) -> &Command {
        &self.command
    }

    /// Classify the plugin command before dispatch without owning plugin policy.
    pub fn requires_daemon(&self, matches: &clap::ArgMatches) -> bool {
        (self.requires_daemon)(matches)
    }

    pub fn invoke(&self, matches: clap::ArgMatches) -> StaticCliFuture {
        (self.invoke)(matches)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unused_invoke(_: clap::ArgMatches) -> StaticCliFuture {
        Box::pin(async { Ok(bcode_plugin_sdk::StaticCliOutcome::default()) })
    }

    fn contribution(plugin_id: &str, command: Command) -> Contribution {
        Contribution {
            plugin_id: plugin_id.to_owned(),
            command_name: command.get_name().to_owned(),
            requires_daemon: |_| false,
            invoke: unused_invoke,
            command,
            extension: None,
        }
    }

    #[test]
    fn startup_selection_preserves_exclusive_config_profile_and_context() {
        let matches = selection_matches(
            [
                "bcode",
                "--config-only",
                "/tmp/selected.toml",
                "--context",
                "work",
                "--profile=sol",
                "example",
                "inspect",
                "--",
                "--profile=plugin-data",
            ]
            .map(std::ffi::OsString::from),
        )
        .unwrap();
        assert_eq!(
            matches.get_one::<String>("profile").map(String::as_str),
            Some("sol")
        );
        assert_eq!(
            matches.get_one::<String>("context").map(String::as_str),
            Some("work")
        );
        assert_eq!(
            matches
                .get_one::<std::path::PathBuf>("config_only")
                .map(std::path::PathBuf::as_path),
            Some(std::path::Path::new("/tmp/selected.toml"))
        );
    }

    #[test]
    fn plugin_command_replaces_matching_core_command() {
        let root = Command::new("bcode")
            .subcommand(Command::new("web").arg(clap::Arg::new("bind").long("bind")));
        let contributions = [contribution(
            "bcode.plugin-web",
            Command::new("web").arg(clap::Arg::new("plugin-option").long("plugin-option")),
        )];
        let root = compose(root, &contributions);
        let web = root
            .find_subcommand("web")
            .expect("web command should exist");

        assert!(
            web.get_arguments()
                .any(|argument| argument.get_id() == "plugin-option")
        );
        assert!(
            !web.get_arguments()
                .any(|argument| argument.get_id() == "bind")
        );
    }

    #[test]
    fn daemon_requirement_uses_plugin_subcommand_matches() {
        let mut contribution = contribution(
            "bcode.example",
            Command::new("example")
                .subcommand(Command::new("view"))
                .subcommand(Command::new("publish")),
        );
        contribution.requires_daemon = |matches| matches.subcommand_name() == Some("publish");
        for (subcommand, expected) in [("view", false), ("publish", true)] {
            let root = compose(Command::new("bcode"), std::slice::from_ref(&contribution));
            let matches = root
                .try_get_matches_from(["bcode", "example", subcommand])
                .unwrap();
            let selected = matched(&matches, std::slice::from_ref(&contribution)).unwrap();
            let (_, plugin_matches) = matches.subcommand().unwrap();
            assert_eq!(selected.requires_daemon(plugin_matches), expected);
        }
    }

    #[test]
    fn plugin_command_can_extend_root() {
        let contributions = [contribution("bcode.example", Command::new("example"))];
        let root = compose(Command::new("bcode"), &contributions);

        assert!(root.find_subcommand("example").is_some());
    }

    #[test]
    fn duplicate_plugin_commands_are_rejected() {
        let contributions = [
            contribution("bcode.first", Command::new("example")),
            contribution("bcode.second", Command::new("example")),
        ];
        let error = validate(&contributions).expect_err("duplicates should fail");

        assert!(matches!(
            error,
            CompositionError::Conflict {
                command,
                first_plugin,
                second_plugin,
            } if command == "example"
                && first_plugin == "bcode.first"
                && second_plugin == "bcode.second"
        ));
    }
}
