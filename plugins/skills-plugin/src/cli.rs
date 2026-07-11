//! Statically bundled skills CLI contribution.

use bcode_client::BcodeClient;
use bcode_plugin_sdk::{StaticCliFuture, StaticCliOutcome, StaticCliRegistration};
use bcode_session_models::SessionId;
use bcode_settings::SettingsStore;
use bcode_skill::{SkillRegistry, SkillRegistryOptions, skill_source_roots_from_config};
use bcode_skill_models::{
    SkillDiagnosticSeverity, SkillSourceKind, SkillToolDecision, SkillToolDecisionEntry,
};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Parser)]
#[command(name = "skill", about = "Manage coding-agent skills")]
struct SkillCli {
    #[command(subcommand)]
    command: SkillCommand,
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Config(#[from] bcode_config::ConfigError),
    #[error(transparent)]
    SkillRegistry(#[from] bcode_skill::SkillRegistryError),
    #[error(transparent)]
    Skill(#[from] bcode_skill_models::SkillError),
    #[error(transparent)]
    Settings(#[from] bcode_settings::SettingsError),
    #[error(transparent)]
    Client(#[from] bcode_client::ClientError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("skill check failed: {warning_count} warnings, {error_count} errors")]
    SkillCheckFailed {
        warning_count: usize,
        error_count: usize,
    },
}

pub(super) fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        command: SkillCli::command,
        invoke,
    }
}

fn invoke(matches: clap::ArgMatches) -> StaticCliFuture {
    Box::pin(async move {
        let cli = SkillCli::from_arg_matches(&matches).map_err(|error| error.to_string())?;
        run(&cli.command).await.map_err(|error| error.to_string())?;
        Ok(StaticCliOutcome::default())
    })
}

#[derive(Debug, Subcommand)]
enum SkillCommand {
    /// Check skill discovery and parsing diagnostics.
    Check {
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
        /// Exit nonzero when any warnings or errors are reported.
        #[arg(long)]
        strict: bool,
    },
    /// List loaded skills.
    List {
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Describe one loaded skill.
    Describe {
        /// Skill ID to describe.
        skill_id: String,
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// List active skills for a live session.
    Active {
        /// Session ID.
        session_id: SessionId,
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Activate a skill for a live session.
    Activate {
        /// Session ID.
        session_id: SessionId,
        /// Skill ID.
        skill_id: String,
    },
    /// Deactivate a skill for a live session.
    Deactivate {
        /// Session ID.
        session_id: SessionId,
        /// Skill ID.
        skill_id: String,
    },
    /// List remembered skill tool decisions.
    Decisions {
        /// Emit JSON instead of tab-separated text.
        #[arg(long)]
        json: bool,
        /// Only include decisions matching this skill ID.
        #[arg(long)]
        skill: Option<String>,
        /// Only include decisions matching this tool name.
        #[arg(long)]
        tool: Option<String>,
    },
    /// Clear remembered skill tool decisions.
    ClearDecisions {
        /// Only clear decisions matching this skill ID.
        #[arg(long)]
        skill: Option<String>,
        /// Only clear decisions matching this tool name.
        #[arg(long)]
        tool: Option<String>,
    },
}

async fn run(command: &SkillCommand) -> Result<(), CliError> {
    let store = SettingsStore::default();
    match command {
        SkillCommand::Check { json, strict } => check_skills(*json, *strict)?,
        SkillCommand::List { json } => list_skills(*json)?,
        SkillCommand::Describe { skill_id, json } => describe_skill(skill_id, *json)?,
        SkillCommand::Active { session_id, json } => active_skills(*session_id, *json).await?,
        SkillCommand::Activate {
            session_id,
            skill_id,
        } => activate_skill(*session_id, skill_id).await?,
        SkillCommand::Deactivate {
            session_id,
            skill_id,
        } => deactivate_skill(*session_id, skill_id).await?,
        SkillCommand::Decisions { json, skill, tool } => {
            let state = store.skill_tool_decisions()?;
            let decisions =
                filtered_skill_tool_decisions(state.decisions, skill.as_deref(), tool.as_deref());
            if *json {
                println!("{}", serde_json::to_string_pretty(&decisions)?);
                return Ok(());
            }
            if decisions.is_empty() {
                println!("no remembered skill tool decisions");
                return Ok(());
            }
            for entry in decisions {
                let decision = match entry.decision {
                    SkillToolDecision::Allow => "allow",
                    SkillToolDecision::Deny => "deny",
                };
                let skills = entry
                    .key
                    .skill_ids
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                println!(
                    "{decision}\t{}\t{}\t{:?}\t{}",
                    skills, entry.key.tool_name, entry.key.scope, entry.remembered_at_ms
                );
            }
        }
        SkillCommand::ClearDecisions { skill, tool } => {
            let mut state = store.skill_tool_decisions()?;
            let before = state.decisions.len();
            state.decisions.retain(|entry| {
                !skill_tool_decision_matches(entry, skill.as_deref(), tool.as_deref())
            });
            let cleared = before.saturating_sub(state.decisions.len());
            store.save_skill_tool_decisions(&state, cli_current_time_ms())?;
            println!("cleared {cleared} remembered skill tool decisions");
        }
    }
    Ok(())
}

fn skill_registry_from_config() -> Result<SkillRegistry, CliError> {
    let config = bcode_config::load_config()?;
    let roots = skill_source_roots_from_config(&config);
    let options = SkillRegistryOptions {
        max_skill_file_bytes: config.skills.max_skill_file_bytes,
        max_context_bytes: config.skills.max_context_bytes,
        follow_symlinks: config.skills.follow_symlinks,
        disabled_ids: config.skills.disabled_skill_ids(),
    };
    Ok(SkillRegistry::discover(&roots, options)?)
}

fn list_skills(json: bool) -> Result<(), CliError> {
    let list = skill_registry_from_config()?.list();
    if json {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(());
    }
    if list.skills.is_empty() {
        println!("no skills loaded");
        return Ok(());
    }
    for skill in list.skills {
        println!(
            "{}\t{}\t{}\t{}",
            skill.id,
            skill.name,
            skill.source.label,
            skill.description.unwrap_or_default()
        );
    }
    Ok(())
}

fn describe_skill(skill_id: &str, json: bool) -> Result<(), CliError> {
    let registry = skill_registry_from_config()?;
    let manifest = registry.describe(&skill_id.parse()?)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&manifest)?);
        return Ok(());
    }
    println!("id: {}", manifest.summary.id);
    println!("name: {}", manifest.summary.name);
    if let Some(description) = manifest.summary.description.as_deref() {
        println!("description: {description}");
    }
    println!("source: {}", manifest.summary.source.label);
    if let Some(path) = manifest.summary.source.path.as_deref() {
        println!("path: {path}");
    }
    if !manifest.summary.activation.keywords.is_empty() {
        println!(
            "keywords: {}",
            manifest.summary.activation.keywords.join(", ")
        );
    }
    if !manifest.permissions.tools.is_empty() {
        println!("tools: {}", manifest.permissions.tools.join(", "));
    }
    print_skill_model_policy(
        manifest.model_policy.preferred.as_ref(),
        manifest.model_policy.required.as_ref(),
    );
    println!("instructions:\n{}", manifest.instructions);
    Ok(())
}

fn print_skill_model_policy(
    preferred: Option<&bcode_skill_models::SkillModelRequest>,
    required: Option<&bcode_skill_models::SkillModelRequest>,
) {
    let summary = skill_model_policy_summary(preferred, required);
    if summary != "-" {
        println!("model policy: {summary}");
    }
}

fn skill_model_policy_summary(
    preferred: Option<&bcode_skill_models::SkillModelRequest>,
    required: Option<&bcode_skill_models::SkillModelRequest>,
) -> String {
    if let Some(request) = required {
        return format_skill_model_request("required", request);
    }
    preferred.map_or_else(
        || "-".to_string(),
        |request| format_skill_model_request("preferred", request),
    )
}

fn format_skill_model_request(
    kind: &str,
    request: &bcode_skill_models::SkillModelRequest,
) -> String {
    let provider = request.provider.as_deref().unwrap_or("auto");
    let effort = request
        .thinking_effort
        .as_ref()
        .map(|effort| format!(", effort={}", effort.source_label))
        .unwrap_or_default();
    format!("{kind}: {provider}/{}{effort}", request.model)
}

async fn active_skills(session_id: SessionId, json: bool) -> Result<(), CliError> {
    let skills = BcodeClient::default_endpoint()
        .active_skills(session_id)
        .await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&skills)?);
        return Ok(());
    }
    if skills.is_empty() {
        println!("no active skills");
        return Ok(());
    }
    for skill in skills {
        let policy = skill_model_policy_summary(
            skill
                .model_policy
                .as_ref()
                .and_then(|policy| policy.preferred.as_ref()),
            skill
                .model_policy
                .as_ref()
                .and_then(|policy| policy.required.as_ref()),
        );
        println!(
            "{}\t{}\t{}\t{}\t{}",
            skill.skill_id, skill.source.label, skill.bytes_loaded, skill.truncated, policy
        );
    }
    Ok(())
}

async fn activate_skill(session_id: SessionId, skill_id: &str) -> Result<(), CliError> {
    BcodeClient::default_endpoint()
        .activate_skill(session_id, skill_id.parse()?)
        .await?;
    println!("activated skill {skill_id} for session {session_id}");
    Ok(())
}

async fn deactivate_skill(session_id: SessionId, skill_id: &str) -> Result<(), CliError> {
    BcodeClient::default_endpoint()
        .deactivate_skill(session_id, skill_id.parse()?)
        .await?;
    println!("deactivated skill {skill_id} for session {session_id}");
    Ok(())
}

#[derive(Debug, Serialize)]
struct SkillCheckReport {
    roots: Vec<SkillCheckRoot>,
    loaded_count: usize,
    diagnostic_count: usize,
    warning_count: usize,
    error_count: usize,
    diagnostics: Vec<bcode_skill_models::SkillDiagnostic>,
}

#[derive(Debug, Serialize)]
struct SkillCheckRoot {
    path: String,
    kind: SkillSourceKind,
    label: String,
    precedence: u16,
}

fn check_skills(json: bool, strict: bool) -> Result<(), CliError> {
    let config = bcode_config::load_config()?;
    let roots = skill_source_roots_from_config(&config);
    let options = SkillRegistryOptions {
        max_skill_file_bytes: config.skills.max_skill_file_bytes,
        max_context_bytes: config.skills.max_context_bytes,
        follow_symlinks: config.skills.follow_symlinks,
        disabled_ids: config.skills.disabled_skill_ids(),
    };
    let registry = SkillRegistry::discover(&roots, options)?;
    let list = registry.list();
    let diagnostics = list.diagnostics;
    let report = SkillCheckReport {
        roots: roots
            .iter()
            .map(|root| SkillCheckRoot {
                path: root.path.to_string_lossy().into_owned(),
                kind: root.kind,
                label: root.label.clone(),
                precedence: root.precedence,
            })
            .collect(),
        loaded_count: list.skills.len(),
        diagnostic_count: diagnostics.len(),
        warning_count: diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == SkillDiagnosticSeverity::Warning)
            .count(),
        error_count: diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == SkillDiagnosticSeverity::Error)
            .count(),
        diagnostics,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        if strict && (report.warning_count > 0 || report.error_count > 0) {
            return Err(CliError::SkillCheckFailed {
                warning_count: report.warning_count,
                error_count: report.error_count,
            });
        }
        return Ok(());
    }

    println!("skill roots: {}", report.roots.len());
    for root in &report.roots {
        println!(
            "  {}\t{}\t{}\t{}",
            root.label,
            root.precedence,
            skill_source_kind_name(root.kind),
            root.path
        );
    }
    println!("loaded skills: {}", report.loaded_count);
    println!("diagnostics: {}", report.diagnostic_count);
    println!("warnings: {}", report.warning_count);
    println!("errors: {}", report.error_count);
    for diagnostic in &report.diagnostics {
        let path = diagnostic.path.as_deref().unwrap_or("<unknown>");
        println!(
            "  {}\t{}\t{}",
            skill_diagnostic_severity_name(diagnostic.severity),
            path,
            diagnostic.message
        );
    }
    if strict && (report.warning_count > 0 || report.error_count > 0) {
        return Err(CliError::SkillCheckFailed {
            warning_count: report.warning_count,
            error_count: report.error_count,
        });
    }
    Ok(())
}

const fn skill_source_kind_name(kind: SkillSourceKind) -> &'static str {
    match kind {
        SkillSourceKind::Repository => "repository",
        SkillSourceKind::Compatibility => "compatibility",
        SkillSourceKind::User => "user",
        SkillSourceKind::Configured => "configured",
        SkillSourceKind::Bundled => "bundled",
        SkillSourceKind::Plugin => "plugin",
    }
}

const fn skill_diagnostic_severity_name(severity: SkillDiagnosticSeverity) -> &'static str {
    match severity {
        SkillDiagnosticSeverity::Info => "info",
        SkillDiagnosticSeverity::Warning => "warning",
        SkillDiagnosticSeverity::Error => "error",
    }
}

fn filtered_skill_tool_decisions(
    decisions: Vec<SkillToolDecisionEntry>,
    skill: Option<&str>,
    tool: Option<&str>,
) -> Vec<SkillToolDecisionEntry> {
    decisions
        .into_iter()
        .filter(|entry| skill_tool_decision_matches(entry, skill, tool))
        .collect()
}

fn skill_tool_decision_matches(
    entry: &SkillToolDecisionEntry,
    skill: Option<&str>,
    tool: Option<&str>,
) -> bool {
    let skill_matches = skill.is_none_or(|skill| {
        entry
            .key
            .skill_ids
            .iter()
            .any(|skill_id| skill_id.as_str() == skill)
    });
    let tool_matches = tool.is_none_or(|tool| entry.key.tool_name == tool);
    skill_matches && tool_matches
}

fn cli_current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}
