//! Review entry points: what the app opens first, and turning entries that need
//! service or provider answers (pull requests, stacks, resuming a review) into a workspace.
//!
//! * The command line opens an [`Entry`] directly; a host that opens the review from options
//!   passes it as the `entry` option (JSON). Older options keep
//!   working: `workspace` opens that workspace and `target` a transient review of one target.
//! * Direct entries open a transient workspace ([`transient_workspace`]); the review can save
//!   it later. `resume` opens a durable workspace.
//! * Pull-request and stack entries ask the enabled review-source providers (client-side
//!   routing); without one they fail with a message naming what is missing, and every
//!   other entry keeps working.

use crate::services::ReviewServices;
use bcode_code_review::provider::{
    ProviderCallError, ProviderRoute, discover_providers, list_pulls, resolve_stack,
};
use bcode_code_review_models::{
    GetReviewWorkspaceRequest, GetReviewWorkspaceResponse, ListReviewWorkspacesRequest,
    ListReviewWorkspacesResponse, REVIEW_SOURCE_ERROR_UNAUTHENTICATED, ReviewRepoStateRequest,
    ReviewRepoStateResponse, ReviewSourceKind, ReviewTarget, ReviewWorkspace,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Surface option carrying the JSON [`Entry`].
pub const ENTRY_OPTION: &str = "entry";

pub use bcode_code_review::scope::{TRANSIENT_WORKSPACE_ID, default_title, transient_workspace};

/// What the app opens first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    /// The home route.
    Home,
    /// A transient review of `sources`.
    Sources {
        /// Review title; the first source's label when absent.
        #[serde(default)]
        title: Option<String>,
        /// Source kinds, in order.
        sources: Vec<ReviewSourceKind>,
    },
    /// A transient review of one pull request, by number or URL.
    PullRequest {
        /// Pull request number (`12`, `#12`) or URL.
        reference: String,
    },
    /// A transient whole-stack review; without a reference, the stack of the current branch's
    /// pull request.
    Stack {
        /// Pull request number or URL of any layer.
        #[serde(default)]
        reference: Option<String>,
    },
    /// Resume a durable review; without an id, the most recently updated one.
    Resume {
        /// Workspace id.
        #[serde(default)]
        workspace_id: Option<String>,
    },
    /// Open this workspace.
    Workspace {
        /// The workspace (durable or transient).
        workspace: Box<ReviewWorkspace>,
    },
}

impl Entry {
    /// The entry described by surface `options`: `entry` when given, else `workspace`, else a
    /// transient review of `target`, else home. Option values may be JSON values or JSON
    /// strings.
    ///
    /// # Errors
    ///
    /// Returns a message when a present option does not parse.
    pub fn from_options(options: &serde_json::Value) -> Result<Self, String> {
        fn parse<T: serde::de::DeserializeOwned>(value: &serde_json::Value) -> Result<T, String> {
            match value {
                serde_json::Value::String(text) => serde_json::from_str(text),
                other => serde_json::from_value(other.clone()),
            }
            .map_err(|error| error.to_string())
        }
        let option = |name: &str| options.get(name).filter(|value| !value.is_null());
        if let Some(entry) = option(ENTRY_OPTION) {
            return parse(entry).map_err(|error| format!("invalid review entry: {error}"));
        }
        if let Some(workspace) = option("workspace") {
            let workspace: ReviewWorkspace =
                parse(workspace).map_err(|error| format!("invalid review workspace: {error}"))?;
            return Ok(Self::Workspace {
                workspace: Box::new(workspace),
            });
        }
        if let Some(target) = option("target") {
            let target: ReviewTarget =
                parse(target).map_err(|error| format!("invalid review target: {error}"))?;
            return Ok(Self::Sources {
                title: None,
                sources: vec![ReviewSourceKind::from(target)],
            });
        }
        Ok(Self::Home)
    }

    /// The entry as the `entry` option value.
    #[must_use]
    pub fn to_option(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// What the opening screen says while the entry resolves.
    #[must_use]
    pub fn opening_label(&self) -> String {
        match self {
            Self::Home => "Opening reviews…".to_owned(),
            Self::Sources { .. } | Self::Workspace { .. } => "Opening review…".to_owned(),
            Self::PullRequest { reference } => format!("Finding pull request {reference}…"),
            Self::Stack {
                reference: Some(reference),
            } => {
                format!("Finding the stack of {reference}…")
            }
            Self::Stack { reference: None } => "Finding the current branch's stack…".to_owned(),
            Self::Resume { .. } => "Finding the review to resume…".to_owned(),
        }
    }
}

/// A pull request reference from the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullReference {
    /// Pull request number.
    pub number: u64,
    /// The URL it was given as, if any.
    pub url: Option<String>,
}

/// Parse `12`, `#12`, or a URL whose last numeric path segment is the number.
///
/// A URL such as `https://host/owner/name/pull/12/files` is also kept and matched against the
/// providers' pull request URLs; nothing provider-specific is parsed here.
#[must_use]
pub fn parse_pull_reference(text: &str) -> Option<PullReference> {
    let text = text.trim();
    let digits = text.strip_prefix('#').unwrap_or(text);
    if let Ok(number) = digits.parse::<u64>() {
        return (number > 0).then_some(PullReference { number, url: None });
    }
    let (_, rest) = text.split_once("://")?;
    let path = rest.split(['?', '#']).next().unwrap_or_default();
    let number = path
        .split('/')
        .skip(1)
        .filter_map(|segment| segment.parse::<u64>().ok())
        .last()
        .filter(|number| *number > 0)?;
    Some(PullReference {
        number,
        url: Some(text.to_owned()),
    })
}

/// A provider failure as a message for the opening screen and the wizard.
#[must_use]
pub fn provider_message(provider: &str, error: &ProviderCallError) -> String {
    if error.code == REVIEW_SOURCE_ERROR_UNAUTHENTICATED {
        format!(
            "{provider} is not signed in or its credentials are unavailable: {}",
            error.message
        )
    } else {
        format!("{provider}: {}", error.message)
    }
}

/// Message shown when no review-source provider is enabled.
pub const NO_PROVIDER: &str = "Pull request and stack reviews need a review-source provider plugin, and none is enabled. Every other source type still works.";

/// The enabled providers with `capable` capabilities, first one first.
///
/// # Errors
///
/// Returns a message when the plugin services cannot be listed or no provider qualifies.
pub async fn provider(
    services: &ReviewServices,
    capable: impl Fn(&ProviderRoute) -> bool,
) -> Result<ProviderRoute, String> {
    let transport = services.transport();
    let routes = discover_providers(&transport)
        .await
        .map_err(|error| format!("could not list review-source providers: {}", error.message))?;
    routes
        .into_iter()
        .find(|route| capable(route))
        .ok_or_else(|| NO_PROVIDER.to_owned())
}

/// Resolve an entry into the workspace to open.
///
/// # Errors
///
/// Returns a message when the entry cannot be resolved (no provider, no pull request for the
/// current branch, an unknown or archived review, or a failing call).
pub async fn resolve(
    services: ReviewServices,
    repo: PathBuf,
    entry: Entry,
) -> Result<ReviewWorkspace, String> {
    match entry {
        Entry::Home => Err("home is not a review".to_owned()),
        Entry::Workspace { workspace } => Ok(*workspace),
        Entry::Sources { title, sources } => Ok(transient_workspace(repo, title, sources)),
        Entry::PullRequest { reference } => pull_request(&services, repo, &reference).await,
        Entry::Stack { reference } => stack(&services, repo, reference.as_deref()).await,
        Entry::Resume { workspace_id } => resume(&services, &repo, workspace_id).await,
    }
}

async fn pull_request(
    services: &ReviewServices,
    repo: PathBuf,
    reference: &str,
) -> Result<ReviewWorkspace, String> {
    let wanted = parse_pull_reference(reference)
        .ok_or_else(|| format!("{reference} is not a pull request number or URL"))?;
    let route = provider(services, |route| route.manifest.capabilities.pull_fetch).await?;
    let transport = services.transport();
    let listed = list_pulls(&transport, &route, &repo, None)
        .await
        .map_err(|error| provider_message(&route.manifest.label, &error))?;
    let pull = listed.pulls.iter().find(|pull| {
        pull.number == wanted.number
            || wanted
                .url
                .as_ref()
                .is_some_and(|url| pull.url.as_deref() == Some(url.as_str()))
    });
    let number = pull.map_or(wanted.number, |pull| pull.number);
    let title = pull.map_or_else(
        || format!("Pull request #{number}"),
        |pull| format!("#{number} {}", pull.title),
    );
    Ok(transient_workspace(
        repo,
        Some(title),
        vec![ReviewSourceKind::PullRequest {
            provider: route.manifest.id.clone(),
            repository: listed.repository,
            number,
        }],
    ))
}

async fn stack(
    services: &ReviewServices,
    repo: PathBuf,
    reference: Option<&str>,
) -> Result<ReviewWorkspace, String> {
    let route = provider(services, |route| {
        route.manifest.capabilities.stack_resolve && route.manifest.capabilities.pull_fetch
    })
    .await?;
    let transport = services.transport();
    let label = &route.manifest.label;
    let (number, repository) = if let Some(reference) = reference {
        let wanted = parse_pull_reference(reference)
            .ok_or_else(|| format!("{reference} is not a pull request number or URL"))?;
        (wanted.number, None)
    } else {
        let state: ReviewRepoStateResponse = services
            .call(ReviewRepoStateRequest {
                repo_path: repo.clone(),
            })
            .await
            .map_err(|error| error.message)?;
        let branch = state
            .branch
            .ok_or("HEAD is detached; pass a pull request number")?;
        let listed = list_pulls(&transport, &route, &repo, None)
            .await
            .map_err(|error| provider_message(label, &error))?;
        let pull = listed
            .pulls
            .iter()
            .find(|pull| pull.head_branch == branch)
            .ok_or_else(|| format!("no open pull request has the current branch {branch}"))?;
        (pull.number, Some(listed.repository))
    };
    let resolved = resolve_stack(
        &transport,
        &route,
        &repo,
        repository,
        number,
        services.stack_limit().resolve_limit(),
    )
    .await
    .map_err(|error| provider_message(label, &error))?;
    let top = resolved
        .stack
        .layers
        .last()
        .ok_or_else(|| format!("pull request #{number} has no stack"))?;
    let title = format!(
        "Stack #{} · {} ({} layers)",
        top.pull.number,
        top.pull.title,
        resolved.stack.layers.len()
    );
    Ok(transient_workspace(
        repo,
        Some(title),
        vec![ReviewSourceKind::Stack {
            provider: route.manifest.id.clone(),
            repository: resolved.repository,
            top: top.pull.number,
        }],
    ))
}

async fn resume(
    services: &ReviewServices,
    repo: &Path,
    workspace_id: Option<String>,
) -> Result<ReviewWorkspace, String> {
    let workspace = if let Some(workspace_id) = workspace_id {
        let response: GetReviewWorkspaceResponse = services
            .call(GetReviewWorkspaceRequest {
                repo_path: repo.to_path_buf(),
                workspace_id: workspace_id.clone(),
            })
            .await
            .map_err(|error| error.message)?;
        response
            .workspace
            .ok_or_else(|| format!("no review with id {workspace_id}"))?
    } else {
        let response: ListReviewWorkspacesResponse = services
            .call(ListReviewWorkspacesRequest {
                repo_path: repo.to_path_buf(),
                include_archived: false,
            })
            .await
            .map_err(|error| error.message)?;
        response
            .workspaces
            .into_iter()
            .filter(|workspace| workspace.archived_at_ms.is_none())
            .max_by_key(|workspace| workspace.updated_at_ms.unwrap_or(0))
            .ok_or("there is no review to resume yet")?
    };
    if workspace.archived_at_ms.is_some() {
        return Err(format!(
            "“{}” is archived; restore it from the reviews home first",
            workspace.title
        ));
    }
    Ok(workspace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_references_parse_numbers_and_urls() {
        let number = |text: &str| parse_pull_reference(text).map(|reference| reference.number);
        assert_eq!(number("12"), Some(12));
        assert_eq!(number("#7"), Some(7));
        assert_eq!(number(" 42 "), Some(42));
        assert_eq!(number("https://example.com/o/r/pull/31"), Some(31));
        assert_eq!(
            number("https://example.com/o/r/pull/31/files?w=1"),
            Some(31)
        );
        assert_eq!(
            number("https://example.com/o/r/-/merge_requests/8#note"),
            Some(8)
        );
        assert_eq!(
            parse_pull_reference("https://example.com/o/r/pull/31")
                .and_then(|reference| reference.url),
            Some("https://example.com/o/r/pull/31".to_owned())
        );
        assert_eq!(number("0"), None);
        assert_eq!(number("feature"), None);
        assert_eq!(number("https://example.com/o/r"), None);
    }

    #[test]
    fn options_select_entry_workspace_target_or_home() {
        let entry = Entry::PullRequest {
            reference: "12".to_owned(),
        };
        assert_eq!(
            Entry::from_options(&serde_json::json!({ "entry": entry.to_option() })),
            Ok(entry)
        );
        let target = serde_json::to_string(&ReviewTarget::IndexStaged).unwrap();
        assert_eq!(
            Entry::from_options(&serde_json::json!({ "target": target })),
            Ok(Entry::Sources {
                title: None,
                sources: vec![ReviewSourceKind::IndexStaged],
            })
        );
        assert_eq!(Entry::from_options(&serde_json::json!({})), Ok(Entry::Home));
        assert!(Entry::from_options(&serde_json::json!({ "entry": "{" })).is_err());
    }
}
