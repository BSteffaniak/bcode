//! Statically bundled Worktree CLI contribution.

use bcode_plugin_sdk::path::display_from_current_dir;
use bcode_plugin_sdk::{StaticCliFuture, StaticCliRegistration};
use bcode_worktree_models::{
    WorktreeBaseRef, WorktreeCreateRequest, WorktreeListRequest, WorktreeRemoveRequest,
};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "worktree", about = "Manage Git worktrees")]
struct WorktreeCli {
    #[command(subcommand)]
    command: WorktreeCliCommand,
}

#[derive(Debug, Subcommand)]
enum WorktreeCliCommand {
    List(WorktreeListArgs),
    Create(WorktreeCreateArgs),
    Attach {
        session_id: bcode_session_models::SessionId,
        path: PathBuf,
    },
    Remove(WorktreeRemoveArgs),
}

#[derive(Debug, Args)]
struct WorktreeListArgs {
    #[arg(long)]
    repo: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)]
struct WorktreeCreateArgs {
    name: String,
    #[arg(long)]
    repo: Option<PathBuf>,
    #[arg(long)]
    path: Option<PathBuf>,
    #[arg(long)]
    session: Option<bcode_session_models::SessionId>,
    #[arg(long)]
    new_session: bool,
    #[arg(long)]
    branch: Option<String>,
    #[arg(long)]
    new_branch: Option<String>,
    #[arg(long, value_enum)]
    base: Option<WorktreeCliBaseRef>,
    #[arg(long)]
    detach: bool,
    #[arg(long)]
    force: bool,
    #[arg(long)]
    no_setup: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct WorktreeRemoveArgs {
    path: PathBuf,
    #[arg(long)]
    repo: Option<PathBuf>,
    #[arg(long)]
    force: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum WorktreeCliBaseRef {
    Auto,
    DefaultBranch,
    Head,
}

pub(super) fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        requires_daemon: true,
        command: WorktreeCli::command,
        invoke,
    }
}

fn invoke(matches: clap::ArgMatches) -> StaticCliFuture {
    Box::pin(async move {
        let cli = WorktreeCli::from_arg_matches(&matches).map_err(|error| error.to_string())?;
        run(cli.command).await?;
        Ok(bcode_plugin_sdk::StaticCliOutcome::default())
    })
}

async fn run(command: WorktreeCliCommand) -> Result<(), String> {
    let client = bcode_client::BcodeClient::default_endpoint();
    match command {
        WorktreeCliCommand::List(args) => list(&client, args).await?,
        WorktreeCliCommand::Create(args) => create(&client, args).await?,
        WorktreeCliCommand::Attach { session_id, path } => {
            let session = client
                .change_session_working_directory(session_id, path)
                .await
                .map_err(|error| error.to_string())?;
            println!(
                "{}\t{}",
                session.id,
                display_from_current_dir(&session.working_directory)
            );
        }
        WorktreeCliCommand::Remove(args) => remove(&client, args).await?,
    }
    Ok(())
}

async fn list(client: &bcode_client::BcodeClient, args: WorktreeListArgs) -> Result<(), String> {
    let response = client
        .list_worktrees(WorktreeListRequest { cwd: args.repo })
        .await
        .map_err(|error| error.to_string())?;
    if args.json {
        println!("{}", json(&response)?);
    } else {
        println!("repo\t{}", display_from_current_dir(&response.repo_root));
        for worktree in response.worktrees {
            let marker = if worktree.is_main { "main" } else { "linked" };
            let branch = worktree.branch.unwrap_or_else(|| "<detached>".to_owned());
            let commit = worktree.commit.unwrap_or_else(|| "-".to_owned());
            println!(
                "{}\t{}\t{}\t{}",
                marker,
                branch,
                commit,
                display_from_current_dir(&worktree.path)
            );
        }
    }
    Ok(())
}

async fn create(
    client: &bcode_client::BcodeClient,
    args: WorktreeCreateArgs,
) -> Result<(), String> {
    let response = client
        .create_worktree(WorktreeCreateRequest {
            name: args.name,
            cwd: args.repo,
            path: args.path,
            branch: args.branch,
            new_branch: args.new_branch,
            base_ref: args.base.map(WorktreeCliBaseRef::into_model),
            detach: args.detach,
            force: args.force,
            attach_session_id: args.session,
            new_session: args.new_session,
            no_setup: args.no_setup,
        })
        .await
        .map_err(|error| error.to_string())?;
    if args.json {
        println!("{}", json(&response)?);
    } else {
        println!("created\t{}", display_from_current_dir(&response.path));
        if let Some(branch) = response.branch {
            println!("branch\t{branch}");
        }
        if let Some(session) = response.session {
            println!("session\t{}", session.id);
        }
    }
    Ok(())
}

async fn remove(
    client: &bcode_client::BcodeClient,
    args: WorktreeRemoveArgs,
) -> Result<(), String> {
    let response = client
        .remove_worktree(WorktreeRemoveRequest {
            cwd: args.repo,
            path: args.path,
            force: args.force,
        })
        .await
        .map_err(|error| error.to_string())?;
    if args.json {
        println!("{}", json(&response)?);
    } else {
        println!("removed\t{}", display_from_current_dir(&response.path));
    }
    Ok(())
}

impl WorktreeCliBaseRef {
    const fn into_model(self) -> WorktreeBaseRef {
        match self {
            Self::Auto => WorktreeBaseRef::Auto,
            Self::DefaultBranch => WorktreeBaseRef::DefaultBranch,
            Self::Head => WorktreeBaseRef::Head,
        }
    }
}

fn json(value: &impl serde::Serialize) -> Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_typed_subcommands() {
        let matches = WorktreeCli::command()
            .try_get_matches_from(["worktree", "create", "feature", "--new-session"])
            .expect("worktree command should parse");
        let cli = WorktreeCli::from_arg_matches(&matches).expect("matches should decode");

        assert!(matches!(
            cli.command,
            WorktreeCliCommand::Create(WorktreeCreateArgs {
                name,
                new_session: true,
                ..
            }) if name == "feature"
        ));
    }
}
