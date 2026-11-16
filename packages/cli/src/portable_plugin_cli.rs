//! Caller-local adapter for shared plugin command declarations.
//!
//! This module owns clap, never the plugin ABI. Static and native hosts supply
//! the same inert declarations and dispatch the resulting `CliInvocation` through
//! the activation-scoped `bcode.cli/v1` extension.

use std::collections::{BTreeMap, BTreeSet};

use bcode_command::cli::{CliCommandPolicy, CliInvocation, CliOutcome};
use bmux_plugin_sdk::{PluginCommand, PluginCommandArgument, PluginCommandArgumentKind};
use clap::{Arg, ArgAction, ArgMatches, Command};

/// A declaration bound to a verified owner by the local loader.
#[derive(Debug, Clone)]
pub struct PortableCliContribution {
    /// Logical owner; the dispatching host must also retain its activation handle.
    pub plugin_id: String,
    /// Renderer-neutral shared grammar, identical for either packaging mode.
    pub command: PluginCommand,
    /// Bcode application policy, not BMUX domain behavior.
    pub policy: CliCommandPolicy,
}

/// Invalid or conflicting declarations are rejected before invoking clap.
#[derive(Debug, thiserror::Error)]
#[error("invalid portable plugin CLI declaration: {0}")]
pub struct PortableCliError(String);

/// Composed local grammar with deterministic conflict detection.
#[derive(Debug)]
pub struct PortableCliRegistry {
    contributions: Vec<PortableCliContribution>,
    paths: BTreeMap<Vec<String>, usize>,
}

/// Activation-bound local extension adapter supplied by the plugin host.
/// Implementations must retain the selected instance until the returned future settles.
pub trait PortableCliInvoker: Send + Sync {
    /// Invoke exactly once, using `bcode.cli/v1`; never use model-tool routing.
    fn invoke(
        &self,
        plugin_id: String,
        request: CliInvocation,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<CliOutcome, crate::CliError>> + Send>,
    >;
}

/// Startup-selected grammar and scoped dispatcher for either packaging mode.
pub struct PortableCliRuntime {
    /// Declarations obtained from the verified startup selection.
    pub registry: PortableCliRegistry,
    /// Retained owner-scoped invocation adapter.
    pub invoker: std::sync::Arc<dyn PortableCliInvoker>,
    /// Negotiated response bound, enforced again before any CLI output.
    pub max_output_bytes: usize,
}

impl PortableCliRuntime {
    /// Dispatch leaf-scoped argv after the embedding host has installed config
    /// and state-location guards. Returns the plugin's exact exit status.
    ///
    /// # Errors
    /// Reports argument, invocation, output, or host-action failure without retry.
    pub async fn dispatch(
        &self,
        argv: &[String],
        stdin: Option<Vec<u8>>,
    ) -> Result<u8, crate::CliError> {
        let invoker = std::sync::Arc::clone(&self.invoker);
        dispatch_portable_plugin(
            &self.registry,
            argv,
            stdin,
            self.max_output_bytes,
            move |entry, request| invoker.invoke(entry.plugin_id.clone(), request),
        )
        .await
    }
}

#[derive(Default)]
struct Node {
    command: Option<usize>,
    children: BTreeMap<String, Self>,
}

impl PortableCliRegistry {
    /// Validate shared declarations without loading a daemon or executing code.
    ///
    /// # Errors
    /// Rejects invalid grammar, duplicate paths and inconsistent application policy.
    pub fn new(contributions: Vec<PortableCliContribution>) -> Result<Self, PortableCliError> {
        let mut paths = BTreeMap::new();
        let mut identities = BTreeSet::new();
        for (index, entry) in contributions.iter().enumerate() {
            if entry.plugin_id.trim().is_empty()
                || entry.command.name.is_empty()
                || entry.policy.command_name != entry.command.name
                || !entry.command.expose_in_cli
            {
                return Err(invalid("owner, CLI visibility or policy identity mismatch"));
            }
            if !identities.insert((entry.plugin_id.clone(), entry.command.name.clone())) {
                return Err(invalid("duplicate command identity"));
            }
            validate_arguments(&entry.command.arguments)?;
            for path in entry.command.cli_paths() {
                if path.is_empty()
                    || path
                        .iter()
                        .any(|segment| !valid_name(segment) || segment == "help")
                {
                    return Err(invalid("command path contains an invalid segment"));
                }
                if let Some(previous) = paths.insert(path.clone(), index) {
                    return Err(invalid(&format!(
                        "path {} is claimed by {} and {}",
                        path.join(" "),
                        contributions[previous].plugin_id,
                        entry.plugin_id
                    )));
                }
            }
        }
        Ok(Self {
            contributions,
            paths,
        })
    }

    /// Compose into the host parser. Host-owned command paths cannot be replaced.
    /// The returned command also drives native clap help and completion engines.
    ///
    /// # Errors
    /// Returns an error on a top-level collision with the host grammar.
    pub fn compose(&self, mut root: Command) -> Result<Command, PortableCliError> {
        let mut tree = Node::default();
        for (path, index) in &self.paths {
            let mut node = &mut tree;
            for segment in path {
                node = node.children.entry(segment.clone()).or_default();
            }
            node.command = Some(*index);
        }
        for (name, node) in &tree.children {
            if root.get_subcommands().any(|command| {
                command.get_name() == name || command.get_all_aliases().any(|alias| alias == name)
            }) {
                return Err(invalid(&format!("host command collision at {name}")));
            }
            root = root.subcommand(self.build_node(name, node));
        }
        Ok(root)
    }

    fn build_node(&self, name: &str, node: &Node) -> Command {
        let mut command = Command::new(name.to_owned());
        if let Some(index) = node.command {
            let declaration = &self.contributions[index].command;
            command = command.about(declaration.summary.clone());
            if let Some(description) = &declaration.description {
                command = command.long_about(description.clone());
            }
            for argument in &declaration.arguments {
                command = command.arg(build_arg(argument));
            }
        } else {
            command = command
                .subcommand_required(true)
                .arg_required_else_help(true);
        }
        for (child, nested) in &node.children {
            command = command.subcommand(self.build_node(child, nested));
        }
        command
    }

    /// Resolve a selected leaf from the composed parser without running code.
    #[must_use]
    pub fn selected(&self, matches: &ArgMatches) -> Option<&PortableCliContribution> {
        let (_, index) = self.selected_path(matches)?;
        self.contributions.get(index)
    }

    fn selected_path(&self, matches: &ArgMatches) -> Option<(Vec<String>, usize)> {
        let mut path = Vec::new();
        let mut current = matches;
        while let Some((name, nested)) = current.subcommand() {
            path.push(name.to_owned());
            current = nested;
        }
        self.paths.get(&path).map(|index| (path, *index))
    }

    /// Parse a plugin-only argv (program token, command path, arguments). Host
    /// global options must be consumed by the host first. This preserves trailing
    /// argv, repeated flags and `--` exactly instead of reconstructing clap values.
    /// No stdin is read, output emitted or daemon started here.
    ///
    /// # Errors
    /// Invalid arguments, non-command groups, or unsolicited stdin are rejected.
    pub fn parse(
        &self,
        argv: &[String],
        stdin: Option<Vec<u8>>,
    ) -> Result<(&PortableCliContribution, CliInvocation), PortableCliError> {
        let parser = self.compose(Command::new("bcode"))?;
        let matches = parser.try_get_matches_from(argv).map_err(|_| {
            // Do not echo arbitrary argument values, which may contain secrets.
            invalid("arguments do not match the declared grammar")
        })?;
        let (selected_path, index) = self
            .selected_path(&matches)
            .ok_or_else(|| invalid("no executable plugin command selected"))?;
        let entry = &self.contributions[index];
        if stdin.is_some() && !entry.policy.accepts_stdin {
            return Err(invalid("command does not accept stdin"));
        }
        // This entry point takes leaf-only argv. Parent options require the host
        // parser's explicit scope mapping; never guess token boundaries by searching
        // values for strings that happen to look like subcommand names.
        if argv.get(1..1 + selected_path.len()) != Some(selected_path.as_slice()) {
            return Err(invalid(
                "parent arguments before a nested command are unsupported",
            ));
        }
        Ok((
            entry,
            CliInvocation {
                command_name: entry.command.name.clone(),
                command_path: entry.command.canonical_path(),
                arguments: argv[1 + selected_path.len()..].to_vec(),
                stdin,
            },
        ))
    }
}

fn invalid(message: &str) -> PortableCliError {
    PortableCliError(message.to_owned())
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.chars().any(char::is_whitespace)
        && !value.chars().any(char::is_control)
}

fn validate_arguments(arguments: &[PluginCommandArgument]) -> Result<(), PortableCliError> {
    let mut ids = BTreeSet::new();
    let mut longs = BTreeSet::from(["help".to_owned()]);
    let mut shorts = BTreeSet::from(['h']);
    let mut positions = BTreeSet::new();
    for arg in arguments {
        if !valid_name(&arg.name) || arg.name == "help" || !ids.insert(arg.name.clone()) {
            return Err(invalid("invalid or duplicate argument id"));
        }
        if let Some(long) = &arg.long
            && (!valid_name(long) || !longs.insert(long.clone()))
        {
            return Err(invalid("invalid or duplicate long option"));
        }
        if let Some(short) = arg.short
            && (!short.is_ascii_alphanumeric() || !shorts.insert(short))
        {
            return Err(invalid("invalid or duplicate short option"));
        }
        if let Some(position) = arg.position {
            if arg.long.is_some() || arg.short.is_some() || !positions.insert(position) {
                return Err(invalid("ambiguous positional argument"));
            }
        } else if arg.long.is_none() && arg.short.is_none() {
            return Err(invalid(
                "argument has neither a position nor an option spelling",
            ));
        }
        if matches!(arg.kind, PluginCommandArgumentKind::Choice) && arg.choice_values.is_empty() {
            return Err(invalid("choice argument requires values"));
        }
        if arg.trailing_var_arg && (arg.position.is_none() || !arg.multiple) {
            return Err(invalid("trailing argv must be a variadic positional"));
        }
        if matches!(arg.kind, PluginCommandArgumentKind::Boolean)
            && (arg.position.is_some() || arg.multiple)
        {
            return Err(invalid("boolean flags cannot be positional or variadic"));
        }
    }
    if positions.iter().copied().ne(0..positions.len()) {
        return Err(invalid("positional indices must be contiguous from zero"));
    }
    let mut optional_positional = false;
    for position in 0..positions.len() {
        let arg = arguments
            .iter()
            .find(|arg| arg.position == Some(position))
            .ok_or_else(|| invalid("missing positional argument"))?;
        if arg.required && optional_positional {
            return Err(invalid(
                "required positional cannot follow an optional positional",
            ));
        }
        optional_positional |= !arg.required;
    }
    for arg in arguments {
        if arg.multiple
            && arg
                .position
                .is_some_and(|position| position + 1 != positions.len())
        {
            return Err(invalid("variadic positional must be last"));
        }
    }
    Ok(())
}

fn build_arg(argument: &PluginCommandArgument) -> Arg {
    let mut arg = Arg::new(argument.name.clone()).required(argument.required);
    if let Some(position) = argument.position {
        arg = arg.index(position + 1);
    }
    if let Some(long) = &argument.long {
        arg = arg.long(long.clone());
    }
    if let Some(short) = argument.short {
        arg = arg.short(short);
    }
    if let Some(summary) = &argument.summary {
        arg = arg.help(summary.clone());
    }
    if let Some(name) = &argument.value_name {
        arg = arg.value_name(name.clone());
    }
    if matches!(argument.kind, PluginCommandArgumentKind::Boolean) {
        arg = arg.action(ArgAction::SetTrue);
    } else {
        arg = arg.action(if argument.multiple {
            ArgAction::Append
        } else {
            ArgAction::Set
        });
        if argument.multiple && argument.position.is_some() {
            arg = arg.num_args(1..);
        }
        arg = arg
            .allow_hyphen_values(argument.allow_hyphen_values)
            .trailing_var_arg(argument.trailing_var_arg);
        match argument.kind {
            PluginCommandArgumentKind::Integer => {
                arg = arg.value_parser(clap::value_parser!(i64));
            }
            PluginCommandArgumentKind::Choice => {
                arg = arg.value_parser(clap::builder::PossibleValuesParser::new(
                    argument.choice_values.clone(),
                ));
            }
            PluginCommandArgumentKind::String
            | PluginCommandArgumentKind::Path
            | PluginCommandArgumentKind::Boolean => {}
        }
    }
    arg
}

/// Validate the entire response before any printing or terminal transition.
/// The caller supplies its transport's negotiated bound, not an ABI buffer size.
///
/// # Errors
/// Rejects oversized output, undeclared actions, or mixed terminal/output ownership.
pub fn validate_outcome(
    policy: &CliCommandPolicy,
    outcome: &CliOutcome,
    max_output_bytes: usize,
) -> Result<(), PortableCliError> {
    if outcome.stdout.len().saturating_add(outcome.stderr.len()) > max_output_bytes {
        return Err(invalid("command output exceeds the negotiated limit"));
    }
    if outcome.host_action.is_some()
        && (!policy.allows_host_action
            || outcome.exit_code != 0
            || !outcome.stdout.is_empty()
            || !outcome.stderr.is_empty())
    {
        return Err(invalid(
            "host action conflicts with command policy, status or output",
        ));
    }
    Ok(())
}

/// Execute a caller-local portable command through an activation-bound adapter.
///
/// The embedding startup path must install its config/state guards before calling
/// this function, and must bind `invoke` to the exact verified owner selected by
/// the registry. Native and bundled adapters use this same path. The adapter must
/// enforce cancellation and bound its wire response *before* decoding it here.
/// No stdin is read implicitly. Returned status is not silently converted to success.
///
/// # Errors
/// Reports invalid input, failed daemon admission, invocation failure, invalid
/// output, local output I/O failure, or a failed local host action. Never retry
/// this function automatically: the invocation may already have committed effects.
pub async fn dispatch_portable_plugin<F, Fut>(
    registry: &PortableCliRegistry,
    argv: &[String],
    stdin: Option<Vec<u8>>,
    max_output_bytes: usize,
    invoke: F,
) -> Result<u8, crate::CliError>
where
    F: FnOnce(&PortableCliContribution, CliInvocation) -> Fut,
    Fut: std::future::Future<Output = Result<CliOutcome, crate::CliError>>,
{
    use std::io::Write as _;

    let (entry, request) = registry
        .parse(argv, stdin)
        .map_err(|error| crate::CliError::InvalidArguments(error.to_string()))?;
    if entry.policy.requires_daemon {
        crate::ensure_server_running().await?;
    }
    let outcome = invoke(entry, request).await?;
    validate_outcome(&entry.policy, &outcome, max_output_bytes)
        .map_err(|error| crate::CliError::InvalidArguments(error.to_string()))?;
    if let Some(action) = outcome.host_action {
        match action {
            bcode_command::cli::CliHostAction::OpenTuiSurface {
                surface_kind,
                repo_path,
                options,
            } => {
                let repo_path = crate::resolve_surface_repo_path(repo_path)?;
                crate::run_plugin_surface_in_shell(surface_kind, repo_path, options).await?;
            }
            bcode_command::cli::CliHostAction::AttachSession { session_id } => {
                crate::ensure_server_running().await?;
                crate::attach_session(session_id).await?;
            }
        }
    } else {
        // Locks are acquired only after the complete outcome is validated, and
        // released before returning or entering any subsequent terminal surface.
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&outcome.stdout)
            .and_then(|()| stdout.flush())
            .map_err(|_| {
                crate::CliError::InvalidArguments("plugin CLI stdout write failed".to_owned())
            })?;
        let mut stderr = std::io::stderr().lock();
        stderr
            .write_all(&outcome.stderr)
            .and_then(|()| stderr.flush())
            .map_err(|_| {
                crate::CliError::InvalidArguments("plugin CLI stderr write failed".to_owned())
            })?;
    }
    Ok(outcome.exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contribution() -> PortableCliContribution {
        PortableCliContribution {
            plugin_id: "example.plugin".to_owned(),
            command: PluginCommand::new("inspect", "Inspect an item")
                .path(["example", "inspect"])
                .alias(["ex", "show"])
                .expose_in_cli(true)
                .argument(
                    PluginCommandArgument::option("format", PluginCommandArgumentKind::Choice)
                        .choice_values(["json", "text"]),
                ),
            policy: CliCommandPolicy {
                command_name: "inspect".to_owned(),
                requires_daemon: false,
                accepts_stdin: false,
                allows_host_action: false,
            },
        }
    }

    #[test]
    fn nested_alias_preserves_canonical_identity_and_original_arguments() {
        let registry = PortableCliRegistry::new(vec![contribution()]).unwrap();
        let argv = ["bcode", "ex", "show", "--format", "json"].map(str::to_owned);
        let (entry, request) = registry.parse(&argv, None).unwrap();
        assert!(!entry.policy.requires_daemon);
        assert_eq!(request.command_name, "inspect");
        assert_eq!(request.command_path, ["example", "inspect"]);
        assert_eq!(request.arguments, ["--format", "json"]);
        assert!(registry.parse(&argv, Some(vec![1])).is_err());
    }

    #[test]
    fn choice_validation_and_help_come_from_composed_grammar() {
        let registry = PortableCliRegistry::new(vec![contribution()]).unwrap();
        let argv = ["bcode", "example", "inspect", "--format", "unknown"].map(str::to_owned);
        assert!(registry.parse(&argv, None).is_err());
        let mut root = registry.compose(Command::new("bcode")).unwrap();
        let leaf = root
            .find_subcommand_mut("example")
            .unwrap()
            .find_subcommand_mut("inspect")
            .unwrap();
        let help = leaf.render_long_help().to_string();
        assert!(help.contains("Inspect an item"));
        assert!(help.contains("--format"));
        assert!(help.contains("json"));
    }

    #[test]
    fn collisions_do_not_replace_host_or_other_plugin_commands() {
        assert!(PortableCliRegistry::new(vec![contribution(), contribution()]).is_err());
        let registry = PortableCliRegistry::new(vec![contribution()]).unwrap();
        assert!(
            registry
                .compose(Command::new("bcode").subcommand(Command::new("host").alias("example")))
                .is_err()
        );
    }

    #[test]
    fn trailing_arguments_preserve_separator_order_and_option_like_values() {
        let mut entry = contribution();
        entry.command.arguments = vec![
            PluginCommandArgument::positional("argv", PluginCommandArgumentKind::String)
                .multiple(true)
                .trailing_var_arg(true)
                .allow_hyphen_values(true),
        ];
        let registry = PortableCliRegistry::new(vec![entry]).unwrap();
        let argv = [
            "bcode",
            "example",
            "inspect",
            "--",
            "--format",
            "secret-value",
        ]
        .map(str::to_owned);
        let (_, request) = registry.parse(&argv, None).unwrap();
        assert_eq!(request.arguments, ["--", "--format", "secret-value"]);
    }

    #[test]
    fn daemon_classification_is_selected_leaf_policy() {
        let mut remote = contribution();
        remote.command = PluginCommand::new("publish", "Publish")
            .path(["example", "publish"])
            .expose_in_cli(true);
        remote.policy.command_name = "publish".to_owned();
        remote.policy.requires_daemon = true;
        let registry = PortableCliRegistry::new(vec![contribution(), remote]).unwrap();
        for (leaf, requires_daemon) in [("inspect", false), ("publish", true)] {
            let argv = ["bcode", "example", leaf].map(str::to_owned);
            let (entry, _) = registry.parse(&argv, None).unwrap();
            assert_eq!(entry.policy.requires_daemon, requires_daemon);
        }
    }

    #[test]
    fn output_is_validated_before_terminal_ownership_changes() {
        let mut policy = contribution().policy;
        policy.allows_host_action = true;
        let mut outcome = CliOutcome {
            host_action: Some(bcode_command::cli::CliHostAction::OpenTuiSurface {
                surface_kind: "example".to_owned(),
                repo_path: None,
                options: BTreeMap::new(),
            }),
            ..CliOutcome::default()
        };
        assert!(validate_outcome(&policy, &outcome, 8).is_ok());
        outcome.stdout.push(1);
        assert!(validate_outcome(&policy, &outcome, 8).is_err());
        outcome.host_action = None;
        assert!(validate_outcome(&policy, &outcome, 0).is_err());
        outcome.stdout.clear();
        outcome.exit_code = 23;
        assert!(validate_outcome(&policy, &outcome, 0).is_ok());
    }
}
