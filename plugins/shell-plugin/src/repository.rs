//! Repository checks reuse workspace-owned Git exports and shell-owned execution.
use crate::contracts::ShellWorkflowCommandPlan;
use bcode_plugin_sdk::prelude::NativeServiceContext;
use bcode_shell_models::{RepositoryDelivery, RepositoryVerification};
use bcode_workflow::{ArtifactReference, WorkflowBlockInvocation};
use bcode_worktree::repository_target::RepositoryExport;
use sha2::Digest;
use std::path::Path;

pub struct PreparedRepository {
    export: RepositoryExport,
    pub directory: tempfile::TempDir,
    delivery: RepositoryDelivery,
    pub artifact: ArtifactReference,
}
impl PreparedRepository {
    pub fn prepare(
        context: &NativeServiceContext,
        invocation: &WorkflowBlockInvocation,
        plan: &ShellWorkflowCommandPlan,
        cwd: &Path,
    ) -> Result<Option<Self>, String> {
        let Some(target) = &plan.repository_target else {
            return Ok(None);
        };
        if !target.valid()
            || plan.delivered_snapshot.is_some()
            || !plan.observe_files.is_empty()
            || plan.expected_content.is_some()
            || plan.commands.is_empty()
        {
            return Err("invalid repository target or conflicting observation mode".into());
        }
        let export =
            bcode_worktree::repository_target::export(cwd, &target.commit, &context.cancellation)?;
        let directory =
            tempfile::tempdir().map_err(|_| "repository target directory unavailable")?;
        export.materialize(directory.path())?;
        let bytes = serde_json::to_vec(&export).map_err(|_| "repository export encoding failed")?;
        let sha256 = hex::encode(sha2::Sha256::digest(&bytes));
        let artifact = super::write_workflow_output_artifact(
            context,
            invocation,
            0,
            "repository",
            "bcode.repository.export",
            bytes,
        )?;
        let delivery = RepositoryDelivery {
            target: target.clone(),
            artifact: artifact.reference_key.clone(),
            sha256,
        };
        Ok(Some(Self {
            export,
            directory,
            delivery,
            artifact,
        }))
    }
    pub fn matches(&self) -> bool {
        self.export.matches(self.directory.path())
    }
    pub fn verification(self, passed: bool, invalidated: bool) -> RepositoryVerification {
        let sources_unchanged = !invalidated && self.matches();
        RepositoryVerification {
            version: 1,
            delivery: self.delivery,
            commands_passed: passed,
            sources_unchanged,
            environment: RepositoryVerification::ENVIRONMENT.into(),
        }
    }
}
