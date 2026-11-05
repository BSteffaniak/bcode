//! Caller-bound delegation for portable discovery, model, plugin, and worktree contracts.

use std::{path::PathBuf, sync::Arc};

use bcode_agent_profile::application::{AgentApplication, AgentApplicationFailure};
use bcode_model_catalog::application::{
    ApplicationModelList, ModelApplication, ModelApplicationFailure,
};
use bcode_plugin_sdk::application::{
    PluginApplication, PluginApplicationFailure, PluginServiceContext, PluginServiceResponse,
    PluginServiceSummary,
};
use bcode_session_models::{ClientId, SessionId};
use bcode_skill_models::application::{SkillApplication, SkillApplicationFailure};
use bcode_worktree_models::application::{WorktreeApplication, WorktreeApplicationFailure};

use crate::{ServerState, plugin_operations, session_operations, worktree_operations};

/// Application access supplied by the owning host to a verified caller.
///
/// This borrowed handle neither constructs canonical state nor selects a caller's authority.
/// The host registers the caller's runtime context before supplying access and releases it
/// when access ends. Contextual plugin invocation cancellation remains caller-scoped.
pub struct ServicesApplicationHandle<'a> {
    state: &'a Arc<ServerState>,
    client_id: ClientId,
}

impl<'a> ServicesApplicationHandle<'a> {
    /// Bind access after the owning host has verified and registered this caller.
    #[must_use]
    pub const fn new(state: &'a Arc<ServerState>, client_id: ClientId) -> Self {
        Self { state, client_id }
    }

    fn require_running(&self) -> Result<(), (&'static str, &'static str)> {
        if self
            .state
            .shutdown_requested
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err((
                "services_application_stopped",
                "services application is shutting down",
            ));
        }
        Ok(())
    }

    async fn selected_session(&self, session_id: SessionId) -> Result<(), (String, String)> {
        self.require_running()
            .map_err(|(code, message)| (code.into(), message.into()))?;
        session_operations::require_selected_location(self.state, session_id)
            .await
            .map_err(|error| (error.code, error.message))?;
        if self
            .state
            .active_session_namespace_mismatch(session_id, self.client_id)
            .await
            .is_some()
        {
            return Err(("session_incompatible_active_client".into(), "session is active for an incompatible client; reconnect with a matching client or wait until the session is inactive".into()));
        }
        self.require_running()
            .map_err(|(code, message)| (code.into(), message.into()))
    }
}

fn plugin_failure(error: &plugin_operations::PublicPluginError) -> PluginApplicationFailure {
    PluginApplicationFailure {
        code: error.code.into(),
        message: error.message.into(),
    }
}

impl AgentApplication for ServicesApplicationHandle<'_> {
    async fn list_agents(
        &self,
    ) -> Result<Vec<bcode_agent_profile::AgentInfo>, AgentApplicationFailure> {
        self.require_running()
            .map_err(|(code, message)| AgentApplicationFailure {
                code: code.into(),
                message: message.into(),
            })?;
        Ok(session_operations::list_agents(self.state, self.client_id).await)
    }

    async fn agent_policy_status(
        &self,
    ) -> Result<bcode_agent_profile::PolicyStatusResponse, AgentApplicationFailure> {
        self.require_running()
            .map_err(|(code, message)| AgentApplicationFailure {
                code: code.into(),
                message: message.into(),
            })?;
        Ok(session_operations::agent_policy_status(self.state).await)
    }
}

impl SkillApplication for ServicesApplicationHandle<'_> {
    fn list_skills(
        &self,
    ) -> impl std::future::Future<
        Output = Result<bcode_skill_models::SkillList, SkillApplicationFailure>,
    > + Send {
        std::future::ready((|| {
            self.require_running()
                .map_err(|(code, message)| SkillApplicationFailure {
                    code: code.into(),
                    message: message.into(),
                })?;
            Ok(session_operations::list_skills(self.state))
        })())
    }

    fn describe_skill(
        &self,
        skill_id: bcode_skill_models::SkillId,
    ) -> impl std::future::Future<
        Output = Result<bcode_skill_models::SkillManifest, SkillApplicationFailure>,
    > + Send {
        std::future::ready((|| {
            self.require_running()
                .map_err(|(code, message)| SkillApplicationFailure {
                    code: code.into(),
                    message: message.into(),
                })?;
            session_operations::describe_skill(self.state, &skill_id).map_err(|error| {
                SkillApplicationFailure {
                    code: error.code().into(),
                    message: error.message().into(),
                }
            })
        })())
    }
}

impl ModelApplication for ServicesApplicationHandle<'_> {
    async fn model_catalog_snapshot(
        &self,
    ) -> Result<bcode_model_catalog_models::CatalogDocument, ModelApplicationFailure> {
        self.require_running()
            .map_err(|(code, message)| ModelApplicationFailure {
                code: code.into(),
                message: message.into(),
            })?;
        Ok(self
            .state
            .model_catalog
            .catalog_snapshot()
            .await
            .document()
            .clone())
    }

    async fn refresh_model_catalog(
        &self,
    ) -> Result<bcode_model_catalog_models::ModelCatalogDiagnostics, ModelApplicationFailure> {
        self.require_running()
            .map_err(|(code, message)| ModelApplicationFailure {
                code: code.into(),
                message: message.into(),
            })?;
        self.state.model_catalog.refresh_now().await;
        Ok(self.state.model_catalog.diagnostics().await.into_public())
    }
    async fn list_models(
        &self,
        provider_plugin_id: Option<String>,
    ) -> Result<ApplicationModelList, ModelApplicationFailure> {
        self.require_running()
            .map_err(|(code, message)| ModelApplicationFailure {
                code: code.into(),
                message: message.into(),
            })?;
        let (provider_plugin_id, models) =
            session_operations::list_models(self.state, self.client_id, provider_plugin_id)
                .await
                .map_err(|error| ModelApplicationFailure {
                    code: error.code().into(),
                    message: error.message().into(),
                })?;
        Ok(ApplicationModelList {
            provider_plugin_id,
            models,
        })
    }

    async fn session_model_list(
        &self,
        session_id: SessionId,
        provider_plugin_id: Option<String>,
    ) -> Result<ApplicationModelList, ModelApplicationFailure> {
        self.selected_session(session_id)
            .await
            .map_err(|(code, message)| ModelApplicationFailure { code, message })?;
        crate::session_operations::list_models_for_session(
            self.state,
            self.client_id,
            session_id,
            provider_plugin_id,
        )
        .await
    }

    async fn check_models(
        &self,
        request: bcode_model_catalog::application::ApplicationModelCheck,
    ) -> Result<bcode_model::ModelCheckReport, ModelApplicationFailure> {
        self.require_running()
            .map_err(|(code, message)| ModelApplicationFailure {
                code: code.into(),
                message: message.into(),
            })?;
        if let Some(session_id) = request.session_id {
            self.selected_session(session_id)
                .await
                .map_err(|(code, message)| ModelApplicationFailure { code, message })?;
        }
        Box::pin(crate::model_check::run(
            self.state,
            self.client_id,
            crate::model_check::CheckInput {
                check_id: request.check_id,
                session_id: request.session_id,
                provider: request.provider_plugin_id,
                model_ids: request.model_ids,
                timeout_seconds: request.timeout_seconds,
                consent: request.allow_provider_requests,
            },
        ))
        .await
        .map_err(|message| ModelApplicationFailure {
            code: "model_check_rejected".into(),
            message: message.into(),
        })
    }

    async fn cancel_model_check(&self, check_id: String) -> Result<bool, ModelApplicationFailure> {
        self.require_running()
            .map_err(|(code, message)| ModelApplicationFailure {
                code: code.into(),
                message: message.into(),
            })?;
        Ok(crate::model_check::cancel(self.state, self.client_id, &check_id).await)
    }

    async fn session_model_status(
        &self,
        session_id: SessionId,
    ) -> Result<bcode_model::SessionModelStatus, ModelApplicationFailure> {
        self.selected_session(session_id)
            .await
            .map_err(|(code, message)| ModelApplicationFailure { code, message })?;
        Ok(session_operations::model_status(self.state, self.client_id, session_id).await)
    }

    async fn default_model_status(
        &self,
    ) -> Result<bcode_model::SessionModelStatus, ModelApplicationFailure> {
        self.require_running()
            .map_err(|(code, message)| ModelApplicationFailure {
                code: code.into(),
                message: message.into(),
            })?;
        Ok(session_operations::default_model_status(self.state, self.client_id).await)
    }

    async fn model_catalog_diagnostics(
        &self,
    ) -> Result<bcode_model_catalog_models::ModelCatalogDiagnostics, ModelApplicationFailure> {
        self.require_running()
            .map_err(|(code, message)| ModelApplicationFailure {
                code: code.into(),
                message: message.into(),
            })?;
        Ok(crate::server_operations::model_catalog_diagnostics(self.state).await)
    }
}

impl PluginApplication for ServicesApplicationHandle<'_> {
    fn list_plugin_services(
        &self,
    ) -> impl std::future::Future<
        Output = Result<Vec<PluginServiceSummary>, PluginApplicationFailure>,
    > + Send {
        std::future::ready((|| {
            self.require_running()
                .map_err(|(code, message)| PluginApplicationFailure {
                    code: code.into(),
                    message: message.into(),
                })?;
            Ok(plugin_operations::list_services(self.state))
        })())
    }

    fn list_plugin_contributions(
        &self,
    ) -> impl std::future::Future<
        Output = Result<bcode_plugin_sdk::PluginContributions, PluginApplicationFailure>,
    > + Send {
        std::future::ready((|| {
            self.require_running()
                .map_err(|(code, message)| PluginApplicationFailure {
                    code: code.into(),
                    message: message.into(),
                })?;
            Ok(plugin_operations::list_contributions(self.state))
        })())
    }

    async fn invoke_plugin_service(
        &self,
        plugin_id: String,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    ) -> Result<PluginServiceResponse, PluginApplicationFailure> {
        self.require_running()
            .map_err(|(code, message)| PluginApplicationFailure {
                code: code.into(),
                message: message.into(),
            })?;
        let _admission =
            self.state
                .shutdown_admission
                .admit()
                .map_err(|_| PluginApplicationFailure {
                    code: "daemon_stopping".into(),
                    message: "daemon is not admitting plugin work".into(),
                })?;
        Box::pin(plugin_operations::invoke_service(
            self.state,
            &plugin_id,
            &interface_id,
            operation,
            payload,
        ))
        .await
        .map_err(|error| plugin_failure(&error))
    }

    async fn call_plugin_service(
        &self,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    ) -> Result<PluginServiceResponse, PluginApplicationFailure> {
        self.require_running()
            .map_err(|(code, message)| PluginApplicationFailure {
                code: code.into(),
                message: message.into(),
            })?;
        let _admission =
            self.state
                .shutdown_admission
                .admit()
                .map_err(|_| PluginApplicationFailure {
                    code: "daemon_stopping".into(),
                    message: "daemon is not admitting plugin work".into(),
                })?;
        let result = if interface_id == bcode_session_import::SESSION_IMPORT_INTERFACE_ID {
            crate::session_import::call_import_service(self.state, &operation, &payload).await
        } else {
            Box::pin(plugin_operations::call_service(
                self.state,
                &interface_id,
                operation,
                payload,
            ))
            .await
        };
        result.map_err(|error| plugin_failure(&error))
    }

    async fn invoke_plugin_service_contextual(
        &self,
        context: PluginServiceContext,
        plugin_id: String,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    ) -> Result<PluginServiceResponse, PluginApplicationFailure> {
        self.require_running()
            .map_err(|(code, message)| PluginApplicationFailure {
                code: code.into(),
                message: message.into(),
            })?;
        let _admission =
            self.state
                .shutdown_admission
                .admit()
                .map_err(|_| PluginApplicationFailure {
                    code: "daemon_stopping".into(),
                    message: "daemon is not admitting plugin work".into(),
                })?;
        Box::pin(crate::plugin_context::invoke(
            self.state,
            self.client_id,
            context,
            plugin_id,
            interface_id,
            operation,
            payload,
        ))
        .await
        .map_err(|error| plugin_failure(&error))
    }

    fn cancel_plugin_service_invocation(
        &self,
        invocation_id: String,
    ) -> impl std::future::Future<Output = Result<bool, PluginApplicationFailure>> + Send {
        std::future::ready((|| {
            self.require_running()
                .map_err(|(code, message)| PluginApplicationFailure {
                    code: code.into(),
                    message: message.into(),
                })?;
            Ok(crate::plugin_context::cancel(
                self.state,
                self.client_id,
                &invocation_id,
            ))
        })())
    }

    async fn publish_plugin_event(
        &self,
        topic: String,
        payload: Vec<u8>,
    ) -> Result<usize, PluginApplicationFailure> {
        self.require_running()
            .map_err(|(code, message)| PluginApplicationFailure {
                code: code.into(),
                message: message.into(),
            })?;
        plugin_operations::publish_event(self.state, &topic, &payload)
            .await
            .map_err(|error| plugin_failure(&error))
    }

    async fn send_plugin_invocation_input(
        &self,
        session_id: SessionId,
        input: bcode_tool::ToolInvocationInput,
    ) -> Result<(), PluginApplicationFailure> {
        self.selected_session(session_id)
            .await
            .map_err(|(code, message)| PluginApplicationFailure { code, message })?;
        plugin_operations::route_controlled_invocation_input(
            self.state,
            session_id,
            self.client_id,
            input,
        )
        .map_err(|error| PluginApplicationFailure {
            code: error.code().into(),
            message: error.message().into(),
        })
    }
}

impl WorktreeApplication for ServicesApplicationHandle<'_> {
    fn list_worktrees(
        &self,
        cwd: PathBuf,
    ) -> impl std::future::Future<
        Output = Result<bcode_worktree_models::WorktreeListResponse, WorktreeApplicationFailure>,
    > + Send {
        std::future::ready((|| {
            self.require_running()
                .map_err(|(code, message)| WorktreeApplicationFailure {
                    code: code.into(),
                    message: message.into(),
                })?;
            worktree_operations::list(&cwd)
                .map_err(|error| worktree_operations::normalize_error(&error))
        })())
    }

    async fn remove_worktree(
        &self,
        cwd: PathBuf,
        path: PathBuf,
        force: bool,
    ) -> Result<bcode_worktree_models::WorktreeRemoveResponse, WorktreeApplicationFailure> {
        self.require_running()
            .map_err(|(code, message)| WorktreeApplicationFailure {
                code: code.into(),
                message: message.into(),
            })?;
        worktree_operations::remove(self.state, &cwd, &path, force)
            .await
            .map_err(worktree_operations::normalize_remove_error)
    }

    async fn start_worktree_creation(
        &self,
        operation_id: String,
        request: bcode_worktree_models::WorktreeCreateRequest,
    ) -> Result<bcode_worktree_models::WorktreeCreateOperationStatus, WorktreeApplicationFailure>
    {
        self.require_running()
            .map_err(|(code, message)| WorktreeApplicationFailure {
                code: code.into(),
                message: message.into(),
            })?;
        crate::worktree_creation::start(self.state, operation_id, request).await
    }

    async fn worktree_creation_status(
        &self,
        operation_id: String,
    ) -> Result<bcode_worktree_models::WorktreeCreateOperationStatus, WorktreeApplicationFailure>
    {
        self.require_running()
            .map_err(|(code, message)| WorktreeApplicationFailure {
                code: code.into(),
                message: message.into(),
            })?;
        crate::worktree_creation::status(self.state, &operation_id).await
    }

    async fn wait_worktree_creation(
        &self,
        operation_id: String,
        after_revision: u64,
        timeout_ms: u64,
    ) -> Result<bcode_worktree_models::WorktreeCreateOperationStatus, WorktreeApplicationFailure>
    {
        self.require_running()
            .map_err(|(code, message)| WorktreeApplicationFailure {
                code: code.into(),
                message: message.into(),
            })?;
        crate::worktree_creation::wait(self.state, &operation_id, after_revision, timeout_ms).await
    }
}

impl bcode_usage_models::application::UsageApplication for ServicesApplicationHandle<'_> {
    async fn usage_report(
        &self,
        query: bcode_usage_models::UsageQuery,
    ) -> Result<
        bcode_usage_models::UsageReport,
        bcode_usage_models::application::UsageApplicationFailure,
    > {
        self.require_running().map_err(|(code, message)| {
            bcode_usage_models::application::UsageApplicationFailure {
                code: code.into(),
                message: message.into(),
            }
        })?;
        if let Some(session_id) = query.session_id {
            self.selected_session(session_id)
                .await
                .map_err(|(code, message)| {
                    bcode_usage_models::application::UsageApplicationFailure { code, message }
                })?;
        }
        self.state
            .usage_index
            .lock()
            .await
            .query(&query)
            .await
            .map_err(
                |_| bcode_usage_models::application::UsageApplicationFailure {
                    code: "usage_unavailable".into(),
                    message: "Usage query invalid or snapshot changed; restart reporting.".into(),
                },
            )
    }
}

impl bcode_workflow_view_models::application::SessionProgressApplication
    for ServicesApplicationHandle<'_>
{
    async fn session_progress(
        &self,
        session_id: SessionId,
    ) -> Result<
        bcode_workflow_view_models::SessionProgressSnapshot,
        bcode_workflow_view_models::application::SessionProgressApplicationFailure,
    > {
        self.selected_session(session_id)
            .await
            .map_err(|(code, message)| {
                bcode_workflow_view_models::application::SessionProgressApplicationFailure {
                    code,
                    message,
                }
            })?;
        let mut snapshot = self
            .state
            .progress_observations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot(session_id);
        if !self.state.sessions.session_is_owned(session_id).await {
            snapshot.observation = bcode_workflow_view_models::SessionProgressObservation::Unavailable {
                reason: bcode_workflow_view_models::SessionProgressUnavailableReason::CoverageUnavailable,
            };
        }
        Ok(snapshot)
    }
}

impl bcode_provider_auth_models::application::AuthPoolApplication
    for ServicesApplicationHandle<'_>
{
    fn auth_pool_list(
        &self,
    ) -> impl std::future::Future<
        Output = Result<
            Vec<bcode_provider_auth_models::AuthPoolSummary>,
            bcode_provider_auth_models::application::AuthPoolApplicationFailure,
        >,
    > + Send {
        std::future::ready((|| {
            self.require_running().map_err(|(code, message)| {
                bcode_provider_auth_models::application::AuthPoolApplicationFailure {
                    code: code.into(),
                    message: message.into(),
                }
            })?;
            bcode_provider_auth::list_auth_pools().map_err(|error| {
                bcode_provider_auth_models::application::AuthPoolApplicationFailure {
                    code: error.code().into(),
                    message: error.to_string(),
                }
            })
        })())
    }

    async fn auth_pool_preference(
        &self,
        pool: String,
    ) -> Result<
        bcode_provider_auth_models::application::AuthPoolPreference,
        bcode_provider_auth_models::application::AuthPoolApplicationFailure,
    > {
        let summary = self
            .auth_pool_list()
            .await?
            .into_iter()
            .find(|summary| summary.pool == pool)
            .ok_or_else(
                || bcode_provider_auth_models::application::AuthPoolApplicationFailure {
                    code: "auth_pool_not_found".into(),
                    message: "auth pool is unavailable".into(),
                },
            )?;
        Ok(
            bcode_provider_auth_models::application::AuthPoolPreference {
                pool,
                profile: summary.preferred_profile,
            },
        )
    }

    fn apply_auth_pool_preference(
        &self,
        request: bcode_provider_auth_models::SetAuthPoolPreferenceRequest,
    ) -> impl std::future::Future<
        Output = Result<(), bcode_provider_auth_models::application::AuthPoolApplicationFailure>,
    > + Send {
        std::future::ready((|| {
            self.require_running().map_err(|(code, message)| {
                bcode_provider_auth_models::application::AuthPoolApplicationFailure {
                    code: code.into(),
                    message: message.into(),
                }
            })?;
            bcode_provider_auth::apply_auth_pool_preference(&request).map_err(|error| {
                bcode_provider_auth_models::application::AuthPoolApplicationFailure {
                    code: error.code().into(),
                    message: error.to_string(),
                }
            })
        })())
    }
}

/// Single-use implementation of the domain shutdown operation; host state stays private.
/// Dropping this borrowed operation does not request shutdown.
pub struct PreparedRuntimeShutdown<'a> {
    state: &'a ServerState,
    mode: bcode_daemon_lifecycle::application::RuntimeShutdownMode,
    reservation: crate::server_operations::shutdown_admission::ShutdownReservation<'a>,
}

impl bcode_daemon_lifecycle::application::RuntimeShutdownOperation for PreparedRuntimeShutdown<'_> {
    fn admission(&self) -> bcode_daemon_lifecycle::application::RuntimeShutdownAccepted {
        bcode_daemon_lifecycle::application::RuntimeShutdownAccepted { mode: self.mode }
    }

    fn commit(self) -> bcode_daemon_lifecycle::application::RuntimeShutdownAccepted {
        self.reservation.commit();
        crate::server_operations::request_shutdown(self.state);
        bcode_daemon_lifecycle::application::RuntimeShutdownAccepted { mode: self.mode }
    }
}

impl bcode_daemon_lifecycle::application::RuntimeShutdownPreparationApplication
    for ServicesApplicationHandle<'_>
{
    type ShutdownOperation<'a>
        = PreparedRuntimeShutdown<'a>
    where
        Self: 'a;

    async fn prepare_runtime_shutdown(
        &self,
        mode: bcode_daemon_lifecycle::application::RuntimeShutdownMode,
    ) -> Result<
        Self::ShutdownOperation<'_>,
        bcode_daemon_lifecycle::application::RuntimeApplicationFailure,
    > {
        use bcode_daemon_lifecycle::application::{RuntimeApplicationFailure, RuntimeShutdownMode};
        self.require_running()
            .map_err(|(code, message)| RuntimeApplicationFailure {
                code: code.into(),
                message: message.into(),
            })?;
        let policy = match mode {
            RuntimeShutdownMode::Force => bcode_ipc::ServerStopMode::Force,
            RuntimeShutdownMode::IfIdle => bcode_ipc::ServerStopMode::IfIdle,
        };
        let reservation = crate::server_operations::prepare_stop(self.state, policy)
            .await
            .map_err(|_| RuntimeApplicationFailure {
                code: crate::server_operations::StopBlocked::code().into(),
                message: crate::server_operations::StopBlocked::message().into(),
            })?;
        Ok(PreparedRuntimeShutdown {
            state: self.state,
            mode,
            reservation,
        })
    }
}

impl bcode_daemon_lifecycle::application::RuntimeQuiescenceApplication
    for ServicesApplicationHandle<'_>
{
    async fn runtime_quiescence(
        &self,
        expected_instance: bcode_ipc::RuntimeInstanceIdentity,
        after: Option<bcode_ipc::RuntimeQuiescence>,
        wait_ms: u32,
    ) -> Result<
        bcode_ipc::RuntimeQuiescence,
        bcode_daemon_lifecycle::application::RuntimeApplicationFailure,
    > {
        crate::server_operations::wait_quiescence(
            self.state,
            &expected_instance,
            after.as_ref(),
            wait_ms,
        )
        .await
    }

    async fn stop_quiescent_runtime(
        &self,
        expected: bcode_ipc::RuntimeQuiescence,
    ) -> Result<
        bcode_ipc::RuntimeStopReceipt,
        bcode_daemon_lifecycle::application::RuntimeApplicationFailure,
    > {
        let reservation =
            crate::server_operations::prepare_quiescent_stop(self.state, &expected).await?;
        let receipt = bcode_ipc::RuntimeStopReceipt {
            version: 1,
            instance: expected.instance,
            admission_generation: expected.admission_generation,
        };
        reservation.commit();
        crate::server_operations::request_shutdown(self.state);
        Ok(receipt)
    }
}

impl bcode_daemon_lifecycle::application::RuntimeApplication for ServicesApplicationHandle<'_> {
    async fn runtime_status(
        &self,
    ) -> Result<
        bcode_daemon_lifecycle::application::RuntimeStatus,
        bcode_daemon_lifecycle::application::RuntimeApplicationFailure,
    > {
        self.require_running().map_err(|(code, message)| {
            bcode_daemon_lifecycle::application::RuntimeApplicationFailure {
                code: code.into(),
                message: message.into(),
            }
        })?;
        let status = crate::server_operations::status(self.state, None).await;
        Ok(bcode_daemon_lifecycle::application::RuntimeStatus {
            connected_client_count: status.connected_client_count,
            selected_provider_plugin_id: status.selected_provider_plugin_id,
            selected_model_id: status.selected_model_id,
            idle_shutdown_blocker: status.idle_shutdown_blocker,
            workflow_unavailable_reason: status.workflow_unavailable_reason,
        })
    }

    async fn request_runtime_shutdown(
        &self,
        mode: bcode_daemon_lifecycle::application::RuntimeShutdownMode,
    ) -> Result<
        bcode_daemon_lifecycle::application::RuntimeShutdownAccepted,
        bcode_daemon_lifecycle::application::RuntimeApplicationFailure,
    > {
        use bcode_daemon_lifecycle::application::{
            RuntimeShutdownOperation as _, RuntimeShutdownPreparationApplication as _,
        };
        Ok(self.prepare_runtime_shutdown(mode).await?.commit())
    }
}

impl bcode_provider_auth_models::application::AccountLifecycleApplication
    for ServicesApplicationHandle<'_>
{
    async fn account_lifecycle_catalog(
        &self,
    ) -> Result<
        bcode_provider_auth_models::application::AccountLifecycleCatalog,
        bcode_provider_auth_models::application::AuthPoolApplicationFailure,
    > {
        self.require_running().map_err(account_lifecycle_stopped)?;
        crate::account_lifecycle::catalog(self.state, self.client_id).await
    }

    async fn preview_account_lifecycle(
        &self,
        request: bcode_provider_auth_models::account_lifecycle::AccountLifecycleRequest,
        revoke: bool,
    ) -> Result<
        bcode_provider_auth_models::application::AccountLifecyclePlan,
        bcode_provider_auth_models::application::AuthPoolApplicationFailure,
    > {
        self.require_running().map_err(account_lifecycle_stopped)?;
        crate::account_lifecycle::preview(self.state, self.client_id, request, revoke).await
    }

    async fn execute_account_lifecycle(
        &self,
        plan_id: String,
    ) -> Result<
        bcode_provider_auth_models::application::AccountLifecycleExecution,
        bcode_provider_auth_models::application::AuthPoolApplicationFailure,
    > {
        self.require_running().map_err(account_lifecycle_stopped)?;
        Box::pin(crate::account_lifecycle::execute(
            self.state,
            self.client_id,
            &plan_id,
        ))
        .await
    }
}

fn account_lifecycle_stopped(
    (code, message): (&str, &str),
) -> bcode_provider_auth_models::application::AuthPoolApplicationFailure {
    bcode_provider_auth_models::application::AuthPoolApplicationFailure {
        code: code.into(),
        message: message.into(),
    }
}

impl bcode_provider_auth_models::application::AccountRoutingApplication
    for ServicesApplicationHandle<'_>
{
    async fn account_routing(
        &self,
        session_id: String,
    ) -> Result<
        bcode_provider_auth_models::AccountRouting,
        bcode_provider_auth_models::application::AuthPoolApplicationFailure,
    > {
        let fail =
            |message: String| bcode_provider_auth_models::application::AuthPoolApplicationFailure {
                code: "account_routing_unavailable".into(),
                message,
            };
        let session_id = session_id
            .parse()
            .map_err(|_| fail("invalid session ID".into()))?;
        self.selected_session(session_id)
            .await
            .map_err(|(_, message)| fail(message))?;
        if !self
            .state
            .session_configs
            .lock()
            .await
            .contains_key(&session_id)
        {
            let runtime = self.state.client_runtime_context(self.client_id).await;
            self.state
                .set_session_config_from_runtime_context(session_id, runtime.as_ref())
                .await;
        }
        crate::account_routing::read(self.state, session_id)
            .await
            .map_err(fail)
    }
    async fn set_account_routing(
        &self,
        session_id: String,
        request: bcode_provider_auth_models::SetAccountRoutingRequest,
    ) -> Result<
        bcode_provider_auth_models::AccountRouting,
        bcode_provider_auth_models::application::AuthPoolApplicationFailure,
    > {
        let fail =
            |message: String| bcode_provider_auth_models::application::AuthPoolApplicationFailure {
                code: "account_routing_unavailable".into(),
                message,
            };
        let session_id = session_id
            .parse()
            .map_err(|_| fail("invalid session ID".into()))?;
        self.selected_session(session_id)
            .await
            .map_err(|(_, message)| fail(message))?;
        if !self
            .state
            .session_configs
            .lock()
            .await
            .contains_key(&session_id)
        {
            let runtime = self.state.client_runtime_context(self.client_id).await;
            self.state
                .set_session_config_from_runtime_context(session_id, runtime.as_ref())
                .await;
        }
        crate::account_routing::set_route(self.state, session_id, request)
            .await
            .map_err(fail)
    }
    async fn set_account_pause(
        &self,
        session_id: String,
        request: bcode_provider_auth_models::SetAccountPauseRequest,
    ) -> Result<
        bcode_provider_auth_models::AccountRouting,
        bcode_provider_auth_models::application::AuthPoolApplicationFailure,
    > {
        let fail =
            |message: String| bcode_provider_auth_models::application::AuthPoolApplicationFailure {
                code: "account_routing_unavailable".into(),
                message,
            };
        let session_id = session_id
            .parse()
            .map_err(|_| fail("invalid session ID".into()))?;
        self.selected_session(session_id)
            .await
            .map_err(|(_, message)| fail(message))?;
        if !self
            .state
            .session_configs
            .lock()
            .await
            .contains_key(&session_id)
        {
            let runtime = self.state.client_runtime_context(self.client_id).await;
            self.state
                .set_session_config_from_runtime_context(session_id, runtime.as_ref())
                .await;
        }
        crate::account_routing::pause(self.state, session_id, request)
            .await
            .map_err(fail)
    }
    async fn rename_account(
        &self,
        session_id: String,
        request: bcode_provider_auth_models::RenameAccountRequest,
    ) -> Result<
        bcode_provider_auth_models::AccountRouting,
        bcode_provider_auth_models::application::AuthPoolApplicationFailure,
    > {
        let fail =
            |message: String| bcode_provider_auth_models::application::AuthPoolApplicationFailure {
                code: "account_routing_unavailable".into(),
                message,
            };
        let session_id = session_id
            .parse()
            .map_err(|_| fail("invalid session ID".into()))?;
        self.selected_session(session_id)
            .await
            .map_err(|(_, message)| fail(message))?;
        if !self
            .state
            .session_configs
            .lock()
            .await
            .contains_key(&session_id)
        {
            let runtime = self.state.client_runtime_context(self.client_id).await;
            self.state
                .set_session_config_from_runtime_context(session_id, runtime.as_ref())
                .await;
        }
        crate::account_routing::rename(self.state, session_id, request)
            .await
            .map_err(fail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> Arc<ServerState> {
        Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ))
    }

    #[tokio::test]
    async fn account_lifecycle_rejects_stopped_access_and_unknown_plans() {
        use bcode_provider_auth_models::application::AccountLifecycleApplication as _;
        let state = state();
        let handle = ServicesApplicationHandle::new(&state, ClientId::new());
        let error = handle
            .execute_account_lifecycle("unreviewed".into())
            .await
            .unwrap_err();
        assert_eq!(error.code, "account_lifecycle_plan_unavailable");
        assert!(state.account_lifecycle_plans.lock().await.is_empty());
        state
            .shutdown_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            handle.account_lifecycle_catalog().await.unwrap_err().code,
            "services_application_stopped"
        );
        assert_eq!(handle.preview_account_lifecycle(
            bcode_provider_auth_models::account_lifecycle::AccountLifecycleRequest {
                schema_version: 1,
                provider_id: "provider".into(),
                profile: "account".into(),
                action: bcode_provider_auth_models::account_lifecycle::AccountLifecycleAction::Remove,
            }, false,
        ).await.unwrap_err().code, "services_application_stopped");
        assert_eq!(
            handle
                .execute_account_lifecycle("unreviewed".into())
                .await
                .unwrap_err()
                .code,
            "services_application_stopped"
        );
        drop(state);
    }

    #[tokio::test]
    async fn stopped_application_rejects_queries_and_side_effects() {
        let state = state();
        state
            .shutdown_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let final_error = {
            let app = ServicesApplicationHandle::new(&state, ClientId::new());
            assert_eq!(
                app.list_agents().await.unwrap_err().code,
                "services_application_stopped"
            );
            assert_eq!(
                app.list_skills().await.unwrap_err().code,
                "services_application_stopped"
            );
            assert_eq!(
                app.list_models(None).await.unwrap_err().code,
                "services_application_stopped"
            );
            assert_eq!(
                app.default_model_status().await.unwrap_err().code,
                "services_application_stopped"
            );
            assert_eq!(
                app.model_catalog_diagnostics().await.unwrap_err().code,
                "services_application_stopped"
            );
            assert_eq!(
                app.list_plugin_services().await.unwrap_err().code,
                "services_application_stopped"
            );
            assert_eq!(
                app.publish_plugin_event("event".into(), vec![])
                    .await
                    .unwrap_err()
                    .code,
                "services_application_stopped"
            );
            assert_eq!(
                app.cancel_plugin_service_invocation("one".into())
                    .await
                    .unwrap_err()
                    .code,
                "services_application_stopped"
            );
            assert_eq!(
                app.list_worktrees(PathBuf::from("unavailable"))
                    .await
                    .unwrap_err()
                    .code,
                "services_application_stopped"
            );
            app.worktree_creation_status("one".into())
                .await
                .unwrap_err()
                .code
        };
        assert_eq!(final_error, "services_application_stopped");
        drop(state);
    }

    #[tokio::test]
    async fn quiescence_fences_instance_and_generation_without_endpoint_inference() {
        let mut state = state();
        let daemon = &mut Arc::get_mut(&mut state).unwrap().daemon_status;
        daemon.instance_id = "quiescence-instance".into();
        daemon.artifact_id = Some(bcode_ipc::ArtifactId::parse("quiescence-artifact").unwrap());
        daemon.state_location_id = Some("quiescence-state".into());
        let identity = crate::server_operations::runtime_identity(&state).unwrap();
        let observed = crate::server_operations::wait_quiescence(&state, &identity, None, 0)
            .await
            .unwrap();
        assert_eq!(observed.blockers, [] as [bcode_ipc::RuntimeIdleBlocker; 0]);
        let mut replacement = identity.clone();
        replacement.instance_id = "replacement".into();
        assert_eq!(
            crate::server_operations::wait_quiescence(&state, &replacement, None, 0)
                .await
                .unwrap_err()
                .code,
            "runtime_instance_changed"
        );
        drop(state.shutdown_admission.admit().unwrap());
        assert!(
            crate::server_operations::prepare_quiescent_stop(&state, &observed)
                .await
                .is_err()
        );
        let current =
            crate::server_operations::wait_quiescence(&state, &identity, Some(&observed), 0)
                .await
                .unwrap();
        assert!(current.admission_generation > observed.admission_generation);
        let reservation = crate::server_operations::prepare_quiescent_stop(&state, &current)
            .await
            .unwrap();
        assert!(state.shutdown_admission.admit().is_err());
        drop(reservation);
        drop(state.shutdown_admission.admit().unwrap());
        drop(state);
    }

    #[tokio::test]
    async fn prepared_shutdown_excludes_direct_graph_publication_and_background_restoration() {
        use bcode_daemon_lifecycle::application::{
            RuntimeShutdownMode, RuntimeShutdownPreparationApplication as _,
        };
        let state = state();
        let app = ServicesApplicationHandle::new(&state, ClientId::new());
        let reservation = app
            .prepare_runtime_shutdown(RuntimeShutdownMode::IfIdle)
            .await
            .unwrap();
        let before = state.shutdown_admission.snapshot().unwrap();
        let edit = bcode_workflow::WorkflowRunGraphEditBatch {
            version: 1,
            run_id: "shutdown-barrier-run".into(),
            expected_revision: 1,
            mutation_id: "shutdown-barrier-edit".into(),
            edits: Vec::new(),
            reconciliation: Vec::new(),
        };
        for error in [
            state
                .publish_workflow_run_graph_edit(ClientId::new(), edit.clone())
                .await
                .unwrap_err(),
            state
                .accept_workflow_run_graph_publication(ClientId::new(), edit.clone())
                .await
                .unwrap_err(),
            state
                .publish_workflow_run_graph_edit_from_invocation(
                    SessionId::new(),
                    "plugin",
                    edit.clone(),
                    &crate::TurnCancelState::default(),
                )
                .await
                .unwrap_err(),
            state
                .accept_workflow_run_graph_publication_from_invocation(
                    SessionId::new(),
                    "plugin",
                    edit,
                    &crate::TurnCancelState::default(),
                )
                .await
                .unwrap_err(),
        ] {
            assert!(
                matches!(error, crate::ServerError::WorkflowApplicationOperationUnauthorized(message) if message == "daemon is not admitting workflow publication")
            );
        }
        // Exercise the production background roots while the reservation is retained.
        crate::restore_workflow_runtime_work(&state).await;
        crate::settle_restored_quiescent_workflow_runtime_work(&state).await;
        assert!(state.workflow_driver_sender.get().is_none());
        assert_eq!(state.runtime_work.active_count().await, 0);
        assert_eq!(state.shutdown_admission.snapshot().unwrap(), before);
        drop(reservation);
        drop(state.shutdown_admission.admit().unwrap());
        drop(state);
    }

    #[tokio::test]
    async fn prepared_shutdown_excludes_admission_until_failed_acknowledgment_releases_it() {
        use bcode_daemon_lifecycle::application::{
            RuntimeShutdownMode, RuntimeShutdownOperation as _,
            RuntimeShutdownPreparationApplication as _,
        };
        let state = state();
        let app = ServicesApplicationHandle::new(&state, ClientId::new());
        let initial = state.shutdown_admission.snapshot().unwrap();
        let prepared = app
            .prepare_runtime_shutdown(RuntimeShutdownMode::IfIdle)
            .await
            .unwrap();
        assert!(state.shutdown_admission.admit().is_err());
        assert!(
            app.prepare_runtime_shutdown(RuntimeShutdownMode::Force)
                .await
                .is_err()
        );
        // Failed transport acknowledgment drops rather than commits the operation.
        drop(prepared);
        let admitted = state.shutdown_admission.admit().unwrap();
        assert!(
            app.prepare_runtime_shutdown(RuntimeShutdownMode::IfIdle)
                .await
                .is_err()
        );
        assert!(state.shutdown_admission.snapshot().unwrap().generation > initial.generation);
        drop(admitted);
        let _receipt = app
            .prepare_runtime_shutdown(RuntimeShutdownMode::IfIdle)
            .await
            .unwrap()
            .commit();
        assert!(state.shutdown_admission.admit().is_err());
        drop(state);
    }

    #[tokio::test]
    async fn cancelling_runtime_work_remains_an_idle_shutdown_blocker_until_finish() {
        use bcode_daemon_lifecycle::application::{
            RuntimeShutdownMode, RuntimeShutdownOperation as _,
            RuntimeShutdownPreparationApplication as _,
        };
        let state = state();
        let app = ServicesApplicationHandle::new(&state, ClientId::new());
        let session_id = SessionId::new();
        let work_id = bcode_session_models::WorkId::new("shutdown-cancellation-barrier");
        let cancellation_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        state
            .runtime_work
            .start(
                session_id,
                crate::RuntimeWorkSpec::new(
                    work_id.clone(),
                    bcode_session_models::RuntimeWorkKind::Tool,
                    "shutdown cancellation barrier".into(),
                    crate::CancellationHandle::Test(cancellation_count),
                ),
            )
            .await;
        assert!(
            app.prepare_runtime_shutdown(RuntimeShutdownMode::IfIdle)
                .await
                .is_err()
        );
        let cancelled = state
            .runtime_work
            .cancel_with_children(session_id, &work_id)
            .await;
        assert_eq!(cancelled, vec![work_id.clone()]);
        assert_eq!(
            state.runtime_work.active_for_session(session_id).await[0].status,
            bcode_session_models::RuntimeWorkStatus::Cancelling
        );
        assert!(
            app.prepare_runtime_shutdown(RuntimeShutdownMode::IfIdle)
                .await
                .is_err()
        );
        state.runtime_work.finish(session_id, &work_id).await;
        let _receipt = app
            .prepare_runtime_shutdown(RuntimeShutdownMode::IfIdle)
            .await
            .unwrap()
            .commit();
        drop(state);
    }

    #[tokio::test]
    async fn rejected_shutdown_policy_reopens_admission() {
        use bcode_daemon_lifecycle::application::{
            RuntimeShutdownMode, RuntimeShutdownPreparationApplication as _,
        };
        let state = state();
        let app = ServicesApplicationHandle::new(&state, ClientId::new());
        state.register_client(ClientId::new()).await;
        state.register_client(ClientId::new()).await;
        assert!(
            app.prepare_runtime_shutdown(RuntimeShutdownMode::IfIdle)
                .await
                .is_err()
        );
        drop(state.shutdown_admission.admit().unwrap());
        assert!(
            !state
                .shutdown_requested
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        drop(state);
    }

    #[tokio::test]
    async fn prepared_shutdown_does_not_signal_until_commit_and_drop_has_no_effect() {
        use bcode_daemon_lifecycle::application::{
            RuntimeShutdownMode, RuntimeShutdownOperation as _,
            RuntimeShutdownPreparationApplication as _,
        };
        let state = state();
        {
            let app = ServicesApplicationHandle::new(&state, ClientId::new());
            let abandoned_admission = {
                let abandoned = app
                    .prepare_runtime_shutdown(RuntimeShutdownMode::Force)
                    .await
                    .unwrap();
                abandoned.admission()
            };
            assert_eq!(abandoned_admission.mode, RuntimeShutdownMode::Force);
            assert!(
                !state
                    .shutdown_requested
                    .load(std::sync::atomic::Ordering::SeqCst)
            );
            let receipt = {
                let prepared = app
                    .prepare_runtime_shutdown(RuntimeShutdownMode::IfIdle)
                    .await
                    .unwrap();
                assert_eq!(prepared.admission().mode, RuntimeShutdownMode::IfIdle);
                assert!(
                    !state
                        .shutdown_requested
                        .load(std::sync::atomic::Ordering::SeqCst)
                );
                prepared.commit()
            };
            assert_eq!(receipt.mode, RuntimeShutdownMode::IfIdle);
            assert!(
                state
                    .shutdown_requested
                    .load(std::sync::atomic::Ordering::SeqCst)
            );
            assert!(
                app.prepare_runtime_shutdown(RuntimeShutdownMode::Force)
                    .await
                    .is_err()
            );
        }
        drop(state);
    }

    #[tokio::test]
    async fn portable_runtime_admission_and_full_catalog_use_existing_owners() {
        use bcode_daemon_lifecycle::application::{RuntimeApplication as _, RuntimeShutdownMode};
        async fn exercise(state: &Arc<ServerState>, app: &ServicesApplicationHandle<'_>) -> bool {
            let status = app.runtime_status().await.unwrap();
            let existing = crate::server_operations::status(state, None).await;
            assert_eq!(
                status.connected_client_count,
                existing.connected_client_count
            );
            assert_eq!(status.idle_shutdown_blocker, existing.idle_shutdown_blocker);
            drop(existing);
            drop(status);
            let catalog = app.model_catalog_snapshot().await.unwrap();
            assert_eq!(
                serde_json::to_value(catalog).unwrap(),
                serde_json::to_value(state.model_catalog.catalog_snapshot().await.document())
                    .unwrap()
            );
            let receipt = app
                .request_runtime_shutdown(RuntimeShutdownMode::Force)
                .await
                .unwrap();
            assert_eq!(receipt.mode, RuntimeShutdownMode::Force);
            assert!(
                state
                    .shutdown_requested
                    .load(std::sync::atomic::Ordering::SeqCst)
            );
            assert!(app.runtime_status().await.is_err());
            assert!(app.model_catalog_snapshot().await.is_err());
            assert!(app.refresh_model_catalog().await.is_err());
            bcode_provider_auth_models::application::AuthPoolApplication::auth_pool_list(app)
                .await
                .is_err()
        }
        let state = state();
        let auth_rejected = exercise(
            &state,
            &ServicesApplicationHandle::new(&state, ClientId::new()),
        )
        .await;
        assert!(auth_rejected);
        drop(state);
    }

    #[tokio::test]
    async fn portable_discovery_matches_existing_domain_owners() {
        async fn exercise(
            state: &Arc<ServerState>,
            caller: ClientId,
            app: &ServicesApplicationHandle<'_>,
        ) -> serde_json::Value {
            assert_eq!(
                app.list_agents().await.unwrap(),
                session_operations::list_agents(state, caller).await
            );
            assert_eq!(
                app.list_plugin_services().await.unwrap(),
                plugin_operations::list_services(state)
            );
            assert_eq!(
                app.list_plugin_contributions().await.unwrap(),
                plugin_operations::list_contributions(state)
            );
            let discovered = app.list_skills().await.unwrap();
            let existing = session_operations::list_skills(state);
            assert_eq!(
                serde_json::to_value(discovered).unwrap(),
                serde_json::to_value(existing).unwrap()
            );
            let diagnostics = app.model_catalog_diagnostics().await.unwrap();
            serde_json::to_value(diagnostics).unwrap()
        }
        let state = state();
        let caller = ClientId::new();
        let diagnostics = exercise(
            &state,
            caller,
            &ServicesApplicationHandle::new(&state, caller),
        )
        .await;
        let existing = crate::server_operations::model_catalog_diagnostics(&state).await;
        assert_eq!(diagnostics, serde_json::to_value(existing).unwrap());
        drop(state);
    }

    #[tokio::test]
    async fn model_checks_require_consent_before_admission_and_cannot_cancel_other_callers() {
        async fn exercise(
            state: &Arc<ServerState>,
            caller: ClientId,
            app: &ServicesApplicationHandle<'_>,
        ) -> bcode_agent_runtime::CancellationToken {
            let error = app
                .check_models(bcode_model_catalog::application::ApplicationModelCheck {
                    check_id: "probe".into(),
                    session_id: None,
                    provider_plugin_id: None,
                    model_ids: vec!["test-model".into()],
                    timeout_seconds: 1,
                    allow_provider_requests: false,
                })
                .await
                .unwrap_err();
            assert_eq!(error.code, "model_check_rejected");
            assert!(state.model_checks.lock().await.is_empty());
            let cancellation = bcode_agent_runtime::CancellationToken::new();
            state.model_checks.lock().await.insert(
                "owned".into(),
                crate::model_check::ActiveCheck {
                    owner: caller,
                    cancellation: cancellation.clone(),
                },
            );
            let stranger_cancelled = ServicesApplicationHandle::new(state, ClientId::new())
                .cancel_model_check("owned".into())
                .await
                .unwrap();
            assert!(!stranger_cancelled);
            assert!(!cancellation.is_cancelled());
            assert!(app.cancel_model_check("owned".into()).await.unwrap());
            cancellation
        }
        let state = state();
        let caller = ClientId::new();
        let cancellation = exercise(
            &state,
            caller,
            &ServicesApplicationHandle::new(&state, caller),
        )
        .await;
        assert!(cancellation.is_cancelled());
        drop(cancellation);
        drop(state);
    }

    #[tokio::test]
    async fn invocation_cancellation_and_transient_status_do_not_invent_work() {
        let state = state();
        let missing_operation_error = {
            let app = ServicesApplicationHandle::new(&state, ClientId::new());
            assert!(
                !app.cancel_plugin_service_invocation("missing".into())
                    .await
                    .unwrap()
            );
            app.worktree_creation_status("missing".into())
                .await
                .unwrap_err()
                .code
        };
        assert_eq!(
            missing_operation_error,
            "worktree_create_operation_not_found"
        );
        assert!(state.worktree_creations.lock().await.is_empty());
        drop(state);
    }
}
