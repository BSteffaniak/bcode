//! Portable projection of author-local clap declarations.
//!
//! Only this local adapter sees clap types; the extension wire carries serializable records.
//! The original parser remains authoritative for custom value parsers and hidden
//! cross-argument constraints. Classification reparses before any daemon admission.

use clap::{Arg, ArgAction, Command};
use serde::{Deserialize, Serialize};

/// Original plugin-local argv, excluding the plugin's top-level command token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliArgvRequest {
    /// Exact ordered tokens, including any `--` boundary. No stdin is inferred.
    pub arguments: Vec<String>,
}

/// Result of authoritative author-local parsing without executing the handler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliClassification {
    /// Argument-dependent daemon requirement from the registration callback.
    pub requires_daemon: bool,
}

/// Portable recursive presentation grammar; versioned by its extension contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // Independent declarative clap flags, not runtime state.
pub struct CliDescriptor {
    /// Command token.
    pub name: String,
    /// Accepted aliases, including hidden aliases.
    pub aliases: Vec<String>,
    /// Aliases shown in help/completion.
    pub visible_aliases: Vec<String>,
    /// Short description.
    pub about: Option<String>,
    /// Extended description.
    pub long_about: Option<String>,
    /// Hidden from parent help/completion.
    pub hidden: bool,
    /// Require a child command.
    pub subcommand_required: bool,
    /// Show help when no arguments are present.
    pub arg_required_else_help: bool,
    /// Child commands negate parent required arguments.
    pub subcommand_negates_reqs: bool,
    /// Parent args and child commands conflict.
    pub args_conflicts_with_subcommands: bool,
    /// Later flags override earlier occurrences.
    pub args_override_self: bool,
    /// Allow unknown child commands as trailing argv.
    pub external_subcommands: bool,
    /// Declared argument grammar.
    pub arguments: Vec<CliArgumentDescriptor>,
    /// Recursive child commands.
    pub subcommands: Vec<Self>,
    /// Author-rendered help preserves formatting and conditional constraint text.
    pub help: String,
    /// Author-rendered extended help.
    pub long_help: String,
}

/// Portable action; never a clap enum or callback on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliArgumentAction {
    /// Replace a value.
    Set,
    /// Collect repeated values.
    Append,
    /// Boolean on.
    SetTrue,
    /// Boolean off.
    SetFalse,
    /// Count occurrences.
    Count,
}

/// Portable argument presentation and parsing facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // Independent argument grammar facts on the wire.
pub struct CliArgumentDescriptor {
    /// Stable argument identity.
    pub id: String,
    /// Long spelling, without dashes.
    pub long: Option<String>,
    /// Short spelling.
    pub short: Option<char>,
    /// Accepted long aliases.
    pub aliases: Vec<String>,
    /// Visible long aliases.
    pub visible_aliases: Vec<String>,
    /// Accepted short aliases.
    pub short_aliases: Vec<char>,
    /// Visible short aliases.
    pub visible_short_aliases: Vec<char>,
    /// Positional index, one-based.
    pub index: Option<usize>,
    /// Required by the author grammar.
    pub required: bool,
    /// Inherited by descendants.
    pub global: bool,
    /// Hidden from help/completion.
    pub hidden: bool,
    /// Short help.
    pub help: Option<String>,
    /// Extended help.
    pub long_help: Option<String>,
    /// Help heading.
    pub help_heading: Option<String>,
    /// Value display names.
    pub value_names: Vec<String>,
    /// Action.
    pub action: CliArgumentAction,
    /// Minimum values per occurrence.
    pub min_values: usize,
    /// Maximum values; none is unbounded.
    pub max_values: Option<usize>,
    /// Completion choices, including hidden values for truthful parsing.
    pub possible_values: Vec<String>,
    /// Declared default values; no environment lookup is performed by extraction.
    pub default_values: Vec<String>,
    /// Default-missing values are interpreted by the authoritative author parser.
    /// Clap does not expose a public extraction API for them.
    pub default_missing_values: Vec<String>,
    /// Delimiter for multiple values.
    pub value_delimiter: Option<char>,
    /// End marker for a variadic value list.
    pub value_terminator: Option<String>,
    /// Trailing passthrough arguments.
    pub trailing_var_arg: bool,
    /// Positional requires `--`.
    pub last: bool,
    /// Option values may begin with dashes.
    pub allow_hyphen_values: bool,
    /// Option requires equals syntax.
    pub require_equals: bool,
    /// Completion hint in portable spelling.
    pub value_hint: String,
}

/// Extraction/reconstruction refuses lossy non-UTF8 defaults and future actions.
#[derive(Debug, thiserror::Error)]
#[error("unsupported CLI declaration: {0}")]
pub struct CliDescriptorError(pub String);

fn text(value: Option<&clap::builder::StyledStr>) -> Option<String> {
    value.map(ToString::to_string)
}

impl CliDescriptor {
    /// Validate portable grammar before giving untrusted records to clap.
    /// Transport byte limits bound total allocation; recursion is bounded by those
    /// same records, rather than an independently imposed plugin topology limit.
    ///
    /// # Errors
    /// Rejects ambiguous names, invalid ranges and positional/action conflicts.
    pub fn validate(&self) -> Result<(), CliDescriptorError> {
        use std::collections::BTreeSet;
        let valid = |name: &str| {
            !name.is_empty()
                && !name.starts_with('-')
                && !name.chars().any(|ch| ch.is_whitespace() || ch.is_control())
        };
        if !valid(&self.name) {
            return Err(CliDescriptorError("invalid command name".to_owned()));
        }
        let mut children = BTreeSet::new();
        for child in &self.subcommands {
            child.validate()?;
            for name in std::iter::once(&child.name).chain(&child.aliases) {
                if !valid(name) || !children.insert(name.clone()) {
                    return Err(CliDescriptorError("ambiguous subcommand name".to_owned()));
                }
            }
        }
        let mut ids = BTreeSet::new();
        let mut longs = BTreeSet::from(["help".to_owned()]);
        let mut shorts = BTreeSet::from(['h']);
        let mut positions = BTreeSet::new();
        for arg in &self.arguments {
            if !valid(&arg.id)
                || arg.id == "help"
                || !ids.insert(arg.id.clone())
                || arg.max_values.is_some_and(|max| max < arg.min_values)
            {
                return Err(CliDescriptorError(
                    "invalid argument identity or arity".to_owned(),
                ));
            }
            for long in arg.long.iter().chain(&arg.aliases) {
                if !valid(long) || !longs.insert(long.clone()) {
                    return Err(CliDescriptorError("ambiguous long option".to_owned()));
                }
            }
            for short in arg.short.iter().chain(&arg.short_aliases) {
                if !short.is_ascii_alphanumeric() || !shorts.insert(*short) {
                    return Err(CliDescriptorError("ambiguous short option".to_owned()));
                }
            }
            if let Some(position) = arg.index {
                if position == 0
                    || !positions.insert(position)
                    || arg.long.is_some()
                    || arg.short.is_some()
                    || !matches!(
                        arg.action,
                        CliArgumentAction::Set | CliArgumentAction::Append
                    )
                {
                    return Err(CliDescriptorError("invalid positional argument".to_owned()));
                }
            } else if arg.long.is_none() && arg.short.is_none() {
                return Err(CliDescriptorError("argument lacks a spelling".to_owned()));
            }
            if arg.trailing_var_arg
                && (arg.index.is_none() || arg.max_values.is_some_and(|max| max <= 1))
            {
                return Err(CliDescriptorError("invalid trailing argv".to_owned()));
            }
        }
        if positions.iter().copied().ne(1..=positions.len()) {
            return Err(CliDescriptorError(
                "noncontiguous positional indices".to_owned(),
            ));
        }
        Ok(())
    }

    /// Extract from a freshly constructed local command without invoking handlers.
    ///
    /// # Errors
    /// Unsupported actions and non-UTF8 default values fail closed.
    pub fn extract(mut command: Command) -> Result<Self, CliDescriptorError> {
        command.build();
        Self::extract_built(&mut command)
    }

    fn extract_built(command: &mut Command) -> Result<Self, CliDescriptorError> {
        let arguments = command
            .get_arguments()
            .filter(|arg| {
                !matches!(
                    arg.get_action(),
                    ArgAction::Help
                        | ArgAction::HelpShort
                        | ArgAction::HelpLong
                        | ArgAction::Version
                )
            })
            .map(CliArgumentDescriptor::extract)
            .collect::<Result<Vec<_>, _>>()?;
        let inherited = arguments
            .iter()
            .filter(|arg| arg.global)
            .map(|arg| arg.id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let subcommands = command
            .get_subcommands_mut()
            // clap's synthesized help command is regenerated by the local adapter.
            .filter(|child| child.get_name() != "help")
            .map(|child| {
                let mut descriptor = Self::extract_built(child)?;
                // Built clap trees propagate globals to children. The receiving
                // parser performs that propagation itself; don't declare twice.
                descriptor
                    .arguments
                    .retain(|arg| !(arg.global && inherited.contains(&arg.id)));
                Ok(descriptor)
            })
            .collect::<Result<Vec<_>, CliDescriptorError>>()?;
        Ok(Self {
            name: command.get_name().to_owned(),
            aliases: command.get_all_aliases().map(str::to_owned).collect(),
            visible_aliases: command.get_visible_aliases().map(str::to_owned).collect(),
            about: text(command.get_about()),
            long_about: text(command.get_long_about()),
            hidden: command.is_hide_set(),
            subcommand_required: command.is_subcommand_required_set(),
            arg_required_else_help: command.is_arg_required_else_help_set(),
            subcommand_negates_reqs: command.is_subcommand_negates_reqs_set(),
            args_conflicts_with_subcommands: command.is_args_conflicts_with_subcommands_set(),
            args_override_self: command.is_args_override_self(),
            external_subcommands: command.is_allow_external_subcommands_set(),
            arguments,
            subcommands,
            help: command.render_help().to_string(),
            long_help: command.render_long_help().to_string(),
        })
    }

    /// Reconstruct local presentation/completion grammar. It never recreates
    /// executable value-parser callbacks. Authoritative classification is required
    /// before executing a dynamic command, even after this parser accepts argv.
    #[must_use]
    pub fn command(&self) -> Command {
        let mut command = Command::new(self.name.clone())
            .hide(self.hidden)
            .subcommand_required(self.subcommand_required)
            .arg_required_else_help(self.arg_required_else_help)
            .subcommand_negates_reqs(self.subcommand_negates_reqs)
            .args_conflicts_with_subcommands(self.args_conflicts_with_subcommands)
            .args_override_self(self.args_override_self)
            .allow_external_subcommands(self.external_subcommands)
            .override_help(self.long_help.clone());
        if let Some(about) = &self.about {
            command = command.about(about.clone());
        }
        if let Some(about) = &self.long_about {
            command = command.long_about(about.clone());
        }
        for alias in &self.aliases {
            command = if self.visible_aliases.contains(alias) {
                command.visible_alias(alias.clone())
            } else {
                command.alias(alias.clone())
            };
        }
        for argument in &self.arguments {
            command = command.arg(argument.argument());
        }
        for child in &self.subcommands {
            command = command.subcommand(child.command());
        }
        command
    }
}

impl CliArgumentDescriptor {
    fn extract(arg: &Arg) -> Result<Self, CliDescriptorError> {
        let action = match arg.get_action() {
            ArgAction::Set => CliArgumentAction::Set,
            ArgAction::Append => CliArgumentAction::Append,
            ArgAction::SetTrue => CliArgumentAction::SetTrue,
            ArgAction::SetFalse => CliArgumentAction::SetFalse,
            ArgAction::Count => CliArgumentAction::Count,
            _ => return Err(CliDescriptorError("unsupported argument action".to_owned())),
        };
        let defaults = |values: &[clap::builder::OsStr]| {
            values
                .iter()
                .map(|value| {
                    value
                        .to_str()
                        .map(str::to_owned)
                        .ok_or_else(|| CliDescriptorError("non-UTF8 default".to_owned()))
                })
                .collect::<Result<Vec<_>, _>>()
        };
        let range = arg.get_num_args();
        let max = range.map_or(1, |range| range.max_values());
        Ok(Self {
            id: arg.get_id().to_string(),
            long: arg.get_long().map(str::to_owned),
            short: arg.get_short(),
            aliases: arg
                .get_all_aliases()
                .unwrap_or_default()
                .into_iter()
                .map(str::to_owned)
                .collect(),
            visible_aliases: arg
                .get_visible_aliases()
                .unwrap_or_default()
                .into_iter()
                .map(str::to_owned)
                .collect(),
            short_aliases: arg.get_all_short_aliases().unwrap_or_default(),
            visible_short_aliases: arg.get_visible_short_aliases().unwrap_or_default(),
            index: arg.get_index(),
            required: arg.is_required_set(),
            global: arg.is_global_set(),
            hidden: arg.is_hide_set(),
            help: text(arg.get_help()),
            long_help: text(arg.get_long_help()),
            help_heading: arg.get_help_heading().map(str::to_owned),
            value_names: arg
                .get_value_names()
                .unwrap_or_default()
                .iter()
                .map(ToString::to_string)
                .collect(),
            action,
            min_values: range.map_or(1, |range| range.min_values()),
            max_values: (max != usize::MAX).then_some(max),
            possible_values: arg
                .get_possible_values()
                .iter()
                .map(|value| value.get_name().to_owned())
                .collect(),
            default_values: defaults(arg.get_default_values())?,
            default_missing_values: Vec::new(),
            value_delimiter: arg.get_value_delimiter(),
            value_terminator: arg.get_value_terminator().map(ToString::to_string),
            trailing_var_arg: arg.is_trailing_var_arg_set(),
            last: arg.is_last_set(),
            allow_hyphen_values: arg.is_allow_hyphen_values_set(),
            require_equals: arg.is_require_equals_set(),
            value_hint: format!("{:?}", arg.get_value_hint()),
        })
    }

    fn argument(&self) -> Arg {
        let action = match self.action {
            CliArgumentAction::Set => ArgAction::Set,
            CliArgumentAction::Append => ArgAction::Append,
            CliArgumentAction::SetTrue => ArgAction::SetTrue,
            CliArgumentAction::SetFalse => ArgAction::SetFalse,
            CliArgumentAction::Count => ArgAction::Count,
        };
        let mut arg = Arg::new(self.id.clone())
            .action(action)
            .required(self.required)
            .global(self.global)
            .hide(self.hidden)
            .num_args(self.min_values..=self.max_values.unwrap_or(usize::MAX))
            .allow_hyphen_values(self.allow_hyphen_values)
            .require_equals(self.require_equals)
            .trailing_var_arg(self.trailing_var_arg)
            .last(self.last);
        if let Some(long) = &self.long {
            arg = arg.long(long.clone());
        }
        if let Some(short) = self.short {
            arg = arg.short(short);
        }
        if let Some(index) = self.index {
            arg = arg.index(index);
        }
        if let Some(help) = &self.help {
            arg = arg.help(help.clone());
        }
        if let Some(help) = &self.long_help {
            arg = arg.long_help(help.clone());
        }
        if let Some(heading) = &self.help_heading {
            arg = arg.help_heading(heading.clone());
        }
        if !self.value_names.is_empty() {
            arg = arg.value_names(self.value_names.clone());
        }
        for alias in &self.aliases {
            arg = if self.visible_aliases.contains(alias) {
                arg.visible_alias(alias.clone())
            } else {
                arg.alias(alias.clone())
            };
        }
        for alias in &self.short_aliases {
            arg = if self.visible_short_aliases.contains(alias) {
                arg.visible_short_alias(*alias)
            } else {
                arg.short_alias(*alias)
            };
        }
        if matches!(
            self.action,
            CliArgumentAction::Set | CliArgumentAction::Append
        ) {
            if !self.possible_values.is_empty() {
                arg = arg.value_parser(clap::builder::PossibleValuesParser::new(
                    self.possible_values.clone(),
                ));
            }
            if !self.default_values.is_empty() {
                arg = arg.default_values(self.default_values.clone());
            }
            if !self.default_missing_values.is_empty() {
                arg = arg.default_missing_values(self.default_missing_values.clone());
            }
            if let Some(delimiter) = self.value_delimiter {
                arg = arg.value_delimiter(delimiter);
            }
            if let Some(terminator) = &self.value_terminator {
                arg = arg.value_terminator(terminator.clone());
            }
        }
        arg.value_hint(match self.value_hint.as_str() {
            "AnyPath" => clap::ValueHint::AnyPath,
            "FilePath" => clap::ValueHint::FilePath,
            "DirPath" => clap::ValueHint::DirPath,
            "ExecutablePath" => clap::ValueHint::ExecutablePath,
            "CommandName" => clap::ValueHint::CommandName,
            "CommandString" => clap::ValueHint::CommandString,
            "CommandWithArguments" => clap::ValueHint::CommandWithArguments,
            "Username" => clap::ValueHint::Username,
            "Hostname" => clap::ValueHint::Hostname,
            "Url" => clap::ValueHint::Url,
            "EmailAddress" => clap::ValueHint::EmailAddress,
            "Other" => clap::ValueHint::Other,
            _ => clap::ValueHint::Unknown,
        })
    }
}

/// Parse with the original author-local parser. No matches cross the ABI.
///
/// # Errors
/// Returns clap's local diagnostic; public hosts must bound/redact as appropriate.
pub fn parse_local(
    command: Command,
    request: &CliArgvRequest,
) -> Result<clap::ArgMatches, clap::Error> {
    let program = command.get_name().to_owned();
    command.try_get_matches_from(std::iter::once(program).chain(request.arguments.iter().cloned()))
}

/// Invoke an author-local handler once after reparsing its original command.
///
/// No parser or matches are serialized and errors do not echo arbitrary argv.
/// The caller converts its local result into `CliOutcome` before transport.
///
/// # Errors
/// Returns a fixed parse diagnostic, or the handler's explicit error value.
pub async fn invoke_local<T, F, Fut>(
    command: Command,
    request: &CliArgvRequest,
    invoke: F,
) -> Result<T, String>
where
    F: FnOnce(clap::ArgMatches) -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let matches = parse_local(command, request)
        .map_err(|_| "arguments do not match the plugin's author grammar".to_owned())?;
    invoke(matches).await
}

/// Classify using the original typed parser and registration callback.
///
/// # Errors
/// Invalid arguments fail before the daemon or handler is entered.
pub fn classify_local(
    command: Command,
    classify: fn(&clap::ArgMatches) -> bool,
    request: &CliArgvRequest,
) -> Result<CliClassification, clap::Error> {
    let matches = parse_local(command, request)?;
    Ok(CliClassification {
        requires_daemon: classify(&matches),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn author_command() -> Command {
        Command::new("example")
            .visible_alias("ex")
            .about("Example plugin commands")
            .subcommand_required(true)
            .subcommand(
                Command::new("inspect")
                    .visible_alias("show")
                    .about("Inspect an item")
                    .arg(
                        Arg::new("format")
                            .long("format")
                            .visible_alias("output")
                            .value_parser(["json", "text"])
                            .default_value("text"),
                    )
                    .arg(
                        Arg::new("count")
                            .long("count")
                            .value_parser(clap::value_parser!(u32)),
                    )
                    .arg(Arg::new("verbose").short('v').action(ArgAction::Count)),
            )
            .subcommand(
                Command::new("publish").arg(
                    Arg::new("argv")
                        .num_args(0..)
                        .trailing_var_arg(true)
                        .allow_hyphen_values(true),
                ),
            )
    }

    #[test]
    fn descriptor_round_trip_preserves_nested_aliases_help_and_completion_choices() {
        let descriptor = CliDescriptor::extract(author_command()).unwrap();
        descriptor.validate().unwrap();
        let bytes = serde_json::to_vec(&descriptor).unwrap();
        let decoded: CliDescriptor = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(descriptor, decoded);
        let command = decoded.command();
        let matches = command
            .clone()
            .try_get_matches_from(["example", "show", "--output", "json", "-vv", "--count", "8"])
            .unwrap();
        let (_, leaf) = matches.subcommand().unwrap();
        assert_eq!(leaf.get_count("verbose"), 2);
        assert_eq!(
            leaf.get_one::<String>("format").map(String::as_str),
            Some("json")
        );
        let inspect = command.find_subcommand("inspect").unwrap();
        let format = inspect
            .get_arguments()
            .find(|arg| arg.get_id() == "format")
            .unwrap();
        let choices = format.get_possible_values();
        assert_eq!(
            choices
                .iter()
                .map(clap::builder::PossibleValue::get_name)
                .collect::<Vec<_>>(),
            ["json", "text"]
        );
        assert!(descriptor.long_help.contains("Example plugin commands"));
        assert!(
            descriptor
                .subcommands
                .iter()
                .find(|child| child.name == "inspect")
                .unwrap()
                .long_help
                .contains("--format")
        );
    }

    #[test]
    fn descriptor_rejects_ambiguous_native_grammar_before_clap() {
        let mut descriptor = CliDescriptor::extract(author_command()).unwrap();
        let inspect = descriptor
            .subcommands
            .iter_mut()
            .find(|child| child.name == "inspect")
            .unwrap();
        let duplicate = inspect.arguments[0].clone();
        inspect.arguments.push(duplicate);
        assert!(descriptor.validate().is_err());
    }

    #[test]
    fn authoritative_parser_retains_typed_values_and_argument_dependent_classification() {
        let request = CliArgvRequest {
            arguments: ["inspect", "--count", "8"].map(str::to_owned).to_vec(),
        };
        let matches = parse_local(author_command(), &request).unwrap();
        let (_, leaf) = matches.subcommand().unwrap();
        assert_eq!(leaf.get_one::<u32>("count"), Some(&8));
        let classification = classify_local(
            author_command(),
            |matches| matches.subcommand_name() == Some("publish"),
            &request,
        )
        .unwrap();
        assert!(!classification.requires_daemon);
        let invalid = CliArgvRequest {
            arguments: ["inspect", "--count", "not-an-integer"]
                .map(str::to_owned)
                .to_vec(),
        };
        assert!(classify_local(author_command(), |_| true, &invalid).is_err());
        let publish = CliArgvRequest {
            arguments: ["publish", "--", "--secret-looking-token"]
                .map(str::to_owned)
                .to_vec(),
        };
        assert!(
            classify_local(author_command(), |_| true, &publish)
                .unwrap()
                .requires_daemon
        );
    }
}
