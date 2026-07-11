//! Statically bundled provider configuration CLI contribution.

use bcode_plugin_sdk::path::display_from_current_dir;
use bcode_plugin_sdk::{StaticCliFuture, StaticCliOutcome, StaticCliRegistration};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "provider", about = "Configure model providers")]
struct ProviderCli {
    #[command(subcommand)]
    command: ProviderCommand,
}

#[derive(Debug, Subcommand)]
enum ProviderCommand {
    Configure {
        #[command(subcommand)]
        command: ConfigureCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigureCommand {
    /// Configure an Amazon Bedrock model profile.
    Bedrock {
        #[arg(long, default_value = "bedrock")]
        profile: String,
        #[arg(long)]
        aws_profile: Option<String>,
        #[arg(long)]
        region: Option<String>,
        #[arg(long)]
        endpoint_url: Option<String>,
        #[arg(long, default_value = "anthropic.claude-sonnet-4-5-20250929-v1:0")]
        model: String,
        #[arg(long = "model-id")]
        model_ids: Vec<String>,
    },
}

pub(super) fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        requires_daemon: false,
        command: ProviderCli::command,
        invoke,
    }
}

fn invoke(matches: clap::ArgMatches) -> StaticCliFuture {
    Box::pin(async move {
        let cli = ProviderCli::from_arg_matches(&matches).map_err(|error| error.to_string())?;
        run(cli.command)?;
        Ok(StaticCliOutcome::default())
    })
}

fn run(command: ProviderCommand) -> Result<(), String> {
    let ProviderCommand::Configure {
        command:
            ConfigureCommand::Bedrock {
                profile,
                aws_profile,
                region,
                endpoint_url,
                model,
                mut model_ids,
            },
    } = command;
    if !model_ids.contains(&model) {
        model_ids.insert(0, model.clone());
    }
    let config_path = bcode_config::set_bedrock_model_profile(
        &profile,
        model,
        aws_profile,
        region,
        endpoint_url.as_deref(),
        &model_ids,
    )
    .map_err(|error| error.to_string())?;
    println!(
        "Bedrock provider profile '{profile}' configured; config updated: {}",
        display_from_current_dir(&config_path)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bedrock_configuration() {
        let matches = ProviderCli::command()
            .try_get_matches_from([
                "provider",
                "configure",
                "bedrock",
                "--profile",
                "production",
                "--region",
                "us-east-1",
            ])
            .expect("provider command should parse");
        let cli = ProviderCli::from_arg_matches(&matches).expect("matches should decode");

        assert!(matches!(
            cli.command,
            ProviderCommand::Configure {
                command: ConfigureCommand::Bedrock { profile, region, .. }
            } if profile == "production" && region.as_deref() == Some("us-east-1")
        ));
    }
}
