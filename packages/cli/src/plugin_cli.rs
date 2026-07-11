//! Composition and dispatch for statically linked plugin CLI contributions.

use bcode_plugin::StaticBundledPlugin;
use bcode_plugin_sdk::StaticCliFuture;
use clap::Command;

#[derive(Debug, thiserror::Error)]
pub enum CompositionError {
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
    invoke: fn(clap::ArgMatches) -> StaticCliFuture,
    command: Command,
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
            invoke: registration.invoke,
            command,
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

impl Contribution {
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
            invoke: unused_invoke,
            command,
        }
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
