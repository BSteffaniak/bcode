//! Portable Blims CLI entry; the author parser remains the sole grammar.

use super::BlimsCli;
use bcode_command::cli::{CliDescriptor, CliInvocation, CliOutcome};
use clap::CommandFactory as _;

/// Full portable grammar, retaining defaults, nested commands and constraints.
///
/// # Errors
/// Returns an error if the author grammar cannot be represented portably.
pub fn descriptor() -> Result<CliDescriptor, String> {
    CliDescriptor::extract(BlimsCli::command()).map_err(|error| error.to_string())
}

/// Parse original leaf tokens and invoke with structured output. No process
/// output is captured, and a rejected request cannot execute domain operations.
pub async fn invoke_portable(invocation: CliInvocation) -> CliOutcome {
    if invocation.stdin.is_some()
        || invocation.command_path.first().map(String::as_str) != Some("blims")
        || invocation.command_path.join(".") != invocation.command_name
    {
        return failure("invalid Blims invocation");
    }
    let args = invocation
        .command_path
        .into_iter()
        .chain(invocation.arguments);
    let Ok(matches) = BlimsCli::command().try_get_matches_from(args) else {
        return failure("invalid Blims arguments");
    };
    super::invoke_structured(matches)
        .await
        .unwrap_or_else(|_| failure("Blims command failed; inspect pending work before retrying"))
}

fn failure(message: &str) -> CliOutcome {
    CliOutcome {
        exit_code: 1,
        stderr: format!("{message}\n").into_bytes(),
        ..CliOutcome::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_command::cli::CliHostAction;

    #[test]
    fn portable_office_is_local_and_input_is_rejected() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let mut invocation = CliInvocation {
            command_name: "blims.enter".into(),
            command_path: vec!["blims".into(), "enter".into()],
            arguments: Vec::new(),
            stdin: None,
        };
        let outcome = runtime.block_on(invoke_portable(invocation.clone()));
        assert!(matches!(
            outcome.host_action,
            Some(CliHostAction::OpenTuiSurface { .. })
        ));
        assert!(outcome.stdout.is_empty() && outcome.stderr.is_empty());
        invocation.stdin = Some(vec![1]);
        let rejected = runtime.block_on(invoke_portable(invocation));
        assert_ne!(rejected.exit_code, 0);
        assert!(rejected.host_action.is_none());
    }

    #[test]
    fn descriptor_preserves_nested_grammar_and_round_trips() {
        let grammar = descriptor().expect("grammar");
        let initiative = grammar
            .subcommands
            .iter()
            .find(|command| command.name == "initiative")
            .expect("initiative");
        assert!(
            initiative
                .subcommands
                .iter()
                .any(|command| command.name == "create")
        );
        let encoded = serde_json::to_vec(&grammar).expect("encode");
        assert_eq!(
            serde_json::from_slice::<CliDescriptor>(&encoded).expect("decode"),
            grammar
        );
    }

    #[test]
    fn output_is_bounded_and_owned_by_each_invocation() {
        let mut first = super::super::CliOutput::default();
        let mut second = super::super::CliOutput::default();
        first.line(format_args!("first"));
        second.line(format_args!("second"));
        assert_eq!(first.stdout, "first\n");
        assert_eq!(second.stdout, "second\n");
        first.line(format_args!(
            "{}",
            "x".repeat(super::super::CliOutput::LIMIT + 1)
        ));
        assert!(first.overflowed);
        assert!(first.stdout.len() <= super::super::CliOutput::LIMIT);
        assert!(!second.overflowed);
    }
}
