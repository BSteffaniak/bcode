#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! The Bcode side of the session bridge.
//!
//! `bcode session-bridge --stdio` runs [`serve`] on its stdin and stdout with an ordinary,
//! exact-match [`BcodeClient`] of its own daemon. [`serve`] speaks the versioned bridge protocol
//! of [`bcode_session_bridge_models`] and turns every request into a daemon application
//! operation; the bridge has no session behavior of its own. Observation snapshots are projected
//! into the protocol's stable view ([`view::project`]).
//!
//! The bridge lives as long as its input: at end of input it stops its observations and closes
//! its tool provider connection, which drops the application's registrations and claims. Turns
//! already admitted keep running.

pub mod view;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bcode_client::client_tools::{ClientToolHandle, ClientToolRequest};
use bcode_client::{BcodeClient, ClientError};
use bcode_session_bridge_models::{
    AgentInfo, BridgeError, BridgeEvent, BridgeFrame, BridgeRequest, BridgeResponse, BridgeResult,
    ClientFrame, ContextBlock, Hello, MAX_LIST_LIMIT, MAX_VIEW_ITEMS, ModelChoice, ModelInfo,
    Origin, SessionPage, SessionSummary, ToolInvocation, ToolOutcome, ToolPolicy, VersionRange,
    negotiate,
};
use bcode_session_models::{
    ProjectionWindowAnchor, ProjectionWindowDirection, ProjectionWindowLimits,
    ProjectionWindowRequest, ProjectionWindowTarget, SessionCatalogQuery, SessionDiscoveryScope,
    SessionId, SessionOrigin, SessionOriginFilter, SessionProjectionKind, SessionVisibility,
    TurnAdmission, TurnAdmissionMetadata, TurnContext, TurnContextBlock, TurnExecutionOptions,
    TurnReasoningOptions,
};
use bcode_session_view::follow::{
    SessionViewFollow, SessionViewFollowUpdate, SessionViewSink, follow_session_view,
};
use bcode_session_view_models::{ReasoningPresentationPolicy, SessionViewSnapshot};
use bcode_tool::client_tool::{ClientToolDescriptor, ClientToolOutcome, ClientToolPolicy};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};

/// Frames buffered before request handlers wait for the output to drain.
const OUTPUT_BUFFER: usize = 256;
/// Shortest interval between two snapshots of one observation; changes in between coalesce.
const OBSERVATION_INTERVAL: Duration = Duration::from_millis(50);
/// Longest wait for catalog discovery before an empty first page is returned as degraded.
const CATALOG_LOAD_WAIT: Duration = Duration::from_secs(5);
/// Pause between catalog reads while discovery loads.
const CATALOG_LOAD_POLL: Duration = Duration::from_millis(50);
/// Daemon connection name, shown in daemon diagnostics.
const CLIENT_NAME: &str = "session-bridge";

/// How the bridge identifies itself and what it accepts.
#[derive(Debug, Clone)]
pub struct BridgeOptions {
    /// Protocol versions the bridge accepts.
    pub versions: VersionRange,
    /// The Bcode version reported by `hello`.
    pub bcode_version: String,
    /// The executable `open_command` names, normally the running Bcode.
    pub executable: PathBuf,
}

/// The application's tool provider: a concurrent daemon connection and the task forwarding its
/// invocations as events.
struct ToolProvider {
    handle: ClientToolHandle,
    pump: JoinHandle<()>,
}

impl Drop for ToolProvider {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

struct Bridge {
    client: BcodeClient,
    options: BridgeOptions,
    out: mpsc::Sender<BridgeFrame>,
    tools: Mutex<Option<ToolProvider>>,
    observations: std::sync::Mutex<BTreeMap<String, JoinHandle<()>>>,
    next_observation: AtomicU64,
}

/// Serve the bridge protocol on `reader`/`writer` until `reader` ends.
///
/// # Errors
///
/// Returns an error when reading the input fails. Output failures end serving quietly, since the
/// application is gone.
pub async fn serve<R, W>(
    reader: R,
    writer: W,
    client: BcodeClient,
    options: BridgeOptions,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out, outgoing) = mpsc::channel(OUTPUT_BUFFER);
    let writer = tokio::spawn(write_frames(writer, outgoing));
    let bridge = Arc::new(Bridge {
        client,
        options,
        out,
        tools: Mutex::new(None),
        observations: std::sync::Mutex::default(),
        next_observation: AtomicU64::new(1),
    });
    let mut handlers = JoinSet::new();
    let mut negotiated = false;
    let mut lines = BufReader::new(reader).lines();
    let result = loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        };
        if line.trim().is_empty() {
            continue;
        }
        let frame = match decode_frame(&line) {
            Ok(frame) => frame,
            Err((id, error)) => {
                bridge.respond(id, Err(error)).await;
                continue;
            }
        };
        match frame.request {
            BridgeRequest::Hello {
                client_name: _,
                versions,
            } if !negotiated => {
                let result = bridge.hello(versions);
                negotiated = result.is_ok();
                bridge.respond(frame.id, result).await;
            }
            BridgeRequest::Hello { .. } => {
                bridge
                    .respond(
                        frame.id,
                        Err(error("already_negotiated", "hello was already answered")),
                    )
                    .await;
            }
            _ if !negotiated => {
                bridge
                    .respond(
                        frame.id,
                        Err(error(
                            "hello_required",
                            "negotiate the protocol version with hello first",
                        )),
                    )
                    .await;
            }
            request => {
                // Finished handlers are reaped here so a long-lived bridge keeps no results.
                while handlers.try_join_next().is_some() {}
                let bridge = Arc::clone(&bridge);
                handlers.spawn(async move {
                    let result = bridge.handle(request).await;
                    bridge.respond(frame.id, result).await;
                });
            }
        }
    };
    handlers.shutdown().await;
    bridge.stop();
    drop(bridge);
    let _ = writer.await;
    result
}

/// Decode one line; a failure carries the request id when it could be read.
fn decode_frame(line: &str) -> Result<ClientFrame, (u64, BridgeError)> {
    serde_json::from_str::<ClientFrame>(line).map_err(|decode| {
        let id = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|value| value.get("id").and_then(serde_json::Value::as_u64))
            .unwrap_or(0);
        (id, error("invalid_request", &decode.to_string()))
    })
}

async fn write_frames<W>(mut writer: W, mut outgoing: mpsc::Receiver<BridgeFrame>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = outgoing.recv().await {
        let Ok(mut line) = serde_json::to_vec(&frame) else {
            continue;
        };
        line.push(b'\n');
        if writer.write_all(&line).await.is_err() || writer.flush().await.is_err() {
            return;
        }
    }
    let _ = writer.shutdown().await;
}

fn error(code: &str, message: &str) -> BridgeError {
    BridgeError {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

/// A daemon failure keeps its stable code; transport failures become `bcode_unavailable`.
fn client_error(failure: ClientError) -> BridgeError {
    match failure {
        ClientError::Server { code, message } => BridgeError { code, message },
        other => error("bcode_unavailable", &other.to_string()),
    }
}

fn session_id(text: &str) -> Result<SessionId, BridgeError> {
    text.parse()
        .map_err(|_| error("invalid_session_id", "session id is not a Bcode session id"))
}

fn absolute(path: &str) -> Result<PathBuf, BridgeError> {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(error(
            "invalid_request",
            "working directories and paths must be absolute",
        ))
    }
}

type Handled = Result<BridgeResponse, BridgeError>;

impl Bridge {
    async fn respond(&self, id: u64, result: Handled) {
        let result = match result {
            Ok(response) => BridgeResult::Ok(Box::new(response)),
            Err(error) => BridgeResult::Error(error),
        };
        let _ = self.out.send(BridgeFrame::Response { id, result }).await;
    }

    async fn emit(&self, event: BridgeEvent) {
        let _ = self.out.send(BridgeFrame::Event { event }).await;
    }

    fn hello(&self, versions: VersionRange) -> Handled {
        let bridge_versions = self.options.versions;
        negotiate(versions, bridge_versions)
            .map(|version| {
                BridgeResponse::Hello(Hello {
                    version,
                    bridge_versions,
                    bcode_version: self.options.bcode_version.clone(),
                })
            })
            .map_err(|mismatch| {
                error(
                    "version_mismatch",
                    &mismatch.message(versions, bridge_versions),
                )
            })
    }

    /// Stop every observation and drop the tool provider connection.
    fn stop(&self) {
        let observations =
            std::mem::take(&mut *self.observations.lock().expect("observations poisoned"));
        for task in observations.into_values() {
            task.abort();
        }
        if let Ok(mut tools) = self.tools.try_lock() {
            tools.take();
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn handle(self: &Arc<Self>, request: BridgeRequest) -> Handled {
        let client = &self.client;
        match request {
            BridgeRequest::Hello { .. } => Err(error("already_negotiated", "hello was answered")),
            BridgeRequest::CreateSession {
                name,
                working_directory,
                origin,
                agent,
                model,
            } => {
                self.create_session(name, &working_directory, origin, agent, model)
                    .await
            }
            BridgeRequest::ListSessions {
                text,
                origin,
                working_directory,
                cursor,
                limit,
            } => {
                let scope = match working_directory {
                    Some(directory) => {
                        SessionDiscoveryScope::WorkingDirectory(absolute(&directory)?)
                    }
                    None => SessionDiscoveryScope::AllWorkingDirectories,
                };
                let cursor = cursor
                    .map(|cursor| serde_json::from_str(&cursor))
                    .transpose()
                    .map_err(|_| {
                        error("invalid_cursor", "the cursor is not one this bridge issued")
                    })?;
                let query = SessionCatalogQuery {
                    text,
                    origin: origin.map(|filter| SessionOriginFilter {
                        app_id: filter.app_id,
                        context: filter.context,
                    }),
                };
                let page = list_page(
                    client,
                    scope,
                    query,
                    cursor,
                    limit.clamp(1, MAX_LIST_LIMIT) as usize,
                )
                .await?;
                Ok(BridgeResponse::Sessions(SessionPage {
                    sessions: page.sessions.iter().map(summary).collect(),
                    next_cursor: page
                        .next_cursor
                        .and_then(|cursor| serde_json::to_string(&cursor).ok()),
                    unclassified: page.unclassified_origin_count,
                    degraded: !matches!(
                        page.catalog_status,
                        bcode_session_models::SessionCatalogStatus::Loaded
                    ),
                }))
            }
            BridgeRequest::Summary { session_id: id } => client
                .session_summary(session_id(&id)?)
                .await
                .map(|session| BridgeResponse::Session {
                    session: summary(&session),
                })
                .map_err(client_error),
            BridgeRequest::HistoryPage {
                session_id: id,
                before_sequence,
                max_items,
            } => {
                let anchor = before_sequence.map_or(
                    ProjectionWindowAnchor::Latest,
                    ProjectionWindowAnchor::BeforeSequence,
                );
                let max_items = max_items.clamp(1, MAX_VIEW_ITEMS) as usize;
                let snapshot = self.snapshot(session_id(&id)?, anchor, max_items).await?;
                Ok(BridgeResponse::History {
                    view: view::project(&snapshot, max_items),
                })
            }
            BridgeRequest::Observe {
                session_id: id,
                max_items,
            } => Ok(self.observe(
                session_id(&id)?,
                max_items.clamp(1, MAX_VIEW_ITEMS) as usize,
            )),
            BridgeRequest::StopObserving { observation_id } => {
                let task = self
                    .observations
                    .lock()
                    .expect("observations poisoned")
                    .remove(&observation_id);
                if let Some(task) = task {
                    task.abort();
                    self.emit(BridgeEvent::ObservationEnded {
                        observation_id,
                        reason: "stopped".to_owned(),
                    })
                    .await;
                }
                Ok(BridgeResponse::Done)
            }
            BridgeRequest::Submit {
                session_id: id,
                text,
                context,
                model,
                effort,
                agent,
            } => {
                self.submit(session_id(&id)?, text, context, model, effort, agent)
                    .await
            }
            BridgeRequest::CancelTurn { session_id: id } => client
                .cancel_session_turn(session_id(&id)?)
                .await
                .map(|_| BridgeResponse::Done)
                .map_err(client_error),
            BridgeRequest::Rename {
                session_id: id,
                name,
            } => client
                .rename_session(session_id(&id)?, Some(name))
                .await
                .map(|session| BridgeResponse::Session {
                    session: summary(&session),
                })
                .map_err(client_error),
            BridgeRequest::Delete { session_id: id } => client
                .delete_session(session_id(&id)?)
                .await
                .map(|_| BridgeResponse::Done)
                .map_err(client_error),
            BridgeRequest::PendingPermissions { session_id: only } => {
                let only = only.as_deref().map(session_id).transpose()?;
                let permissions = client.list_permissions().await.map_err(client_error)?;
                Ok(BridgeResponse::Permissions {
                    permissions: permissions
                        .into_iter()
                        .filter(|permission| only.is_none_or(|id| permission.session_id == id))
                        .map(
                            |permission| bcode_session_bridge_models::PendingPermission {
                                permission_id: permission.permission_id,
                                session_id: Some(permission.session_id.to_string()),
                                tool_name: permission.tool_name,
                                arguments: bcode_session_bridge_models::bounded_text(
                                    &permission.arguments_json,
                                    bcode_session_bridge_models::MAX_TOOL_TEXT_BYTES,
                                ),
                                title: permission.policy_reason,
                            },
                        )
                        .collect(),
                })
            }
            BridgeRequest::ResolvePermission {
                permission_id,
                approve,
                remember,
            } => {
                let resolved = client
                    .resolve_permission_with_remember(permission_id, approve, remember)
                    .await
                    .map_err(client_error)?;
                if resolved {
                    Ok(BridgeResponse::Done)
                } else {
                    Err(error(
                        "permission_not_found",
                        "the permission request was already answered or does not exist",
                    ))
                }
            }
            BridgeRequest::RegisterTools {
                provider_id,
                tools,
                profiles,
            } => self.register_tools(provider_id, tools, profiles).await,
            BridgeRequest::ClaimSession { session_id: id } => {
                let id = session_id(&id)?;
                self.with_tools(|handle| async move { handle.claim_session(id).await })
                    .await
            }
            BridgeRequest::ReleaseSession { session_id: id } => {
                let id = session_id(&id)?;
                self.with_tools(|handle| async move { handle.release_session(id).await })
                    .await
            }
            BridgeRequest::ToolResult {
                invocation_id,
                outcome,
            } => {
                let outcome = match outcome {
                    ToolOutcome::Completed { content } => ClientToolOutcome::Completed { content },
                    ToolOutcome::Failed { code, message } => {
                        ClientToolOutcome::Failed { code, message }
                    }
                };
                self.with_tools(
                    |handle| async move { handle.respond(invocation_id, outcome).await },
                )
                .await
            }
            BridgeRequest::ListAgents => client
                .list_agents()
                .await
                .map(|agents| BridgeResponse::Agents {
                    agents: agents
                        .into_iter()
                        .map(|agent| AgentInfo {
                            id: agent.id,
                            name: agent.name,
                            description: agent.description,
                            is_default: agent.is_default,
                        })
                        .collect(),
                })
                .map_err(client_error),
            BridgeRequest::ListModels => self.list_models().await,
            BridgeRequest::Usage { session_id: id } => {
                let snapshot = self
                    .snapshot(session_id(&id)?, ProjectionWindowAnchor::Latest, 1)
                    .await?;
                Ok(BridgeResponse::Usage(view::project(&snapshot, 0).usage))
            }
            BridgeRequest::ImportSdkSession {
                path,
                working_directory,
                origin,
            } => client
                .import_session_file(
                    "sdk-json",
                    absolute(&path)?,
                    absolute(&working_directory)?,
                    origin.map(session_origin),
                )
                .await
                .map(|(session, warnings)| BridgeResponse::Imported {
                    session: summary(&session),
                    warnings: warnings
                        .into_iter()
                        .map(|warning| warning.message)
                        .collect(),
                })
                .map_err(client_error),
            BridgeRequest::OpenCommand { session_id: id } => {
                session_id(&id)?;
                Ok(BridgeResponse::Command {
                    argv: vec![
                        self.options.executable.display().to_string(),
                        "tui".to_owned(),
                        id,
                    ],
                })
            }
        }
    }

    async fn create_session(
        &self,
        name: Option<String>,
        working_directory: &str,
        origin: Option<Origin>,
        agent: Option<String>,
        model: Option<ModelChoice>,
    ) -> Handled {
        let working_directory = absolute(working_directory)?;
        let origin = origin.map(session_origin);
        let tools = self.tools.lock().await;
        // A session the application's own provider creates is claimed for its tools.
        let created = match (tools.as_ref(), &origin) {
            (Some(provider), Some(origin)) if origin.app_id == provider.handle.provider_id() => {
                let handle = provider.handle.clone();
                drop(tools);
                handle
                    .create_session(name, working_directory, Some(origin.clone()))
                    .await
            }
            _ => {
                drop(tools);
                self.client
                    .create_session_with_origin(name, working_directory, origin)
                    .await
            }
        }
        .map_err(client_error)?;
        if let Some(agent) = agent {
            self.client
                .set_session_agent(created.id, agent)
                .await
                .map_err(client_error)?;
        }
        if let Some(model) = model {
            self.client
                .set_session_model(created.id, model.provider, model.model)
                .await
                .map_err(client_error)?;
        }
        Ok(BridgeResponse::Session {
            session: summary(&created),
        })
    }

    async fn submit(
        &self,
        session_id: SessionId,
        text: String,
        context: Vec<ContextBlock>,
        model: Option<ModelChoice>,
        effort: Option<String>,
        agent: Option<String>,
    ) -> Handled {
        let mut turn_context = TurnContext::default();
        for block in context {
            let target = if block.request_only {
                &mut turn_context.request_only
            } else {
                &mut turn_context.blocks
            };
            target.push(TurnContextBlock::new(block.label, block.text));
        }
        let (provider_plugin_id, model_id) =
            model.map_or((None, None), |model| (model.provider, Some(model.model)));
        let admission = TurnAdmissionMetadata {
            execution: TurnExecutionOptions {
                agent_profile: agent,
                provider_plugin_id,
                model_id,
                reasoning: effort.map(|effort| {
                    Box::new(TurnReasoningOptions {
                        effort: Some(effort),
                        summary: None,
                    })
                }),
                ..TurnExecutionOptions::default()
            },
            context: turn_context,
            ..TurnAdmissionMetadata::default()
        };
        match self
            .client
            .submit_turn(session_id, text, admission)
            .await
            .map_err(client_error)?
        {
            TurnAdmission::Accepted(receipt) | TurnAdmission::Existing(receipt) => {
                Ok(BridgeResponse::Submitted {
                    turn_id: receipt.turn_id.0,
                    queued: false,
                })
            }
            TurnAdmission::Deferred(receipt) => Ok(BridgeResponse::Submitted {
                turn_id: receipt.turn_id.0,
                queued: true,
            }),
            TurnAdmission::Rejected(reason) => Err(error(
                "turn_rejected",
                &format!("the turn was not admitted: {reason:?}"),
            )),
            TurnAdmission::CancelledBeforeStart(_) => Err(error(
                "turn_cancelled",
                "the turn was cancelled before it started",
            )),
        }
    }

    async fn list_models(&self) -> Handled {
        let provider = self
            .client
            .default_model_status()
            .await
            .map_err(client_error)?
            .provider_plugin_id
            .unwrap_or_default();
        let models = self
            .client
            .session_model_list(None)
            .await
            .map_err(client_error)?;
        let per_million = |price: Option<&bcode_model::ModelTokenPrice>,
                           pricing: &bcode_model::ModelPricingInfo| {
            (pricing.currency == "USD"
                && pricing.unit == bcode_model::ModelPricingUnit::PerMillionTokens)
                .then_some(price)
                .flatten()
                .map(|price| view_dollars(price.micros))
        };
        Ok(BridgeResponse::Models {
            models: models
                .models
                .into_iter()
                .map(|model| ModelInfo {
                    provider: provider.clone(),
                    efforts: model
                        .reasoning
                        .as_ref()
                        .map(|reasoning| reasoning.effort_values.clone())
                        .unwrap_or_default(),
                    input_usd_per_million: model
                        .pricing
                        .as_ref()
                        .and_then(|pricing| per_million(pricing.input.as_ref(), pricing)),
                    output_usd_per_million: model
                        .pricing
                        .as_ref()
                        .and_then(|pricing| per_million(pricing.output.as_ref(), pricing)),
                    display_name: Some(model.display_name),
                    model: model.model_id,
                })
                .collect(),
        })
    }

    async fn register_tools(
        self: &Arc<Self>,
        provider_id: String,
        tools: Vec<bcode_session_bridge_models::ToolDescriptor>,
        profiles: Vec<bcode_session_bridge_models::AgentProfile>,
    ) -> Handled {
        let tools = tools
            .into_iter()
            .map(|tool| ClientToolDescriptor {
                name: tool.name,
                description: tool.description,
                input_schema: tool.input_schema,
                policy: match tool.policy {
                    ToolPolicy::ReadOnly => ClientToolPolicy::ReadOnly,
                    ToolPolicy::Mutating => ClientToolPolicy::Mutating,
                },
                max_duration_ms: tool.max_duration_ms,
            })
            .collect::<Vec<_>>();
        let profiles = profiles
            .into_iter()
            .map(
                |profile| bcode_agent_profile::client_profile::ClientAgentProfile {
                    id: profile.id,
                    name: profile.name,
                    description: profile.description,
                    base_agent: profile.base_agent,
                    instructions: profile.instructions,
                    tools: profile.tools,
                },
            )
            .collect::<Vec<_>>();
        let tool_count = tools.len();
        let profile_count = profiles.len();
        let mut current = self.tools.lock().await;
        let live = current
            .as_ref()
            .is_some_and(|provider| !provider.pump.is_finished());
        match current.as_ref() {
            Some(provider) if live && provider.handle.provider_id() == provider_id => {
                provider
                    .handle
                    .register(tools)
                    .await
                    .map_err(client_error)?;
            }
            Some(provider) if live => {
                return Err(error(
                    "provider_mismatch",
                    &format!(
                        "this bridge already provides tools as {:?}",
                        provider.handle.provider_id()
                    ),
                ));
            }
            _ => {
                let provider = self
                    .client
                    .client_tool_provider(provider_id, tools)
                    .await
                    .map_err(client_error)?;
                let (handle, requests) = provider.into_concurrent();
                let pump = tokio::spawn(pump_tool_requests(Arc::downgrade(self), requests));
                *current = Some(ToolProvider { handle, pump });
            }
        }
        let handle = current
            .as_ref()
            .map(|provider| provider.handle.clone())
            .ok_or_else(|| error("tools_not_registered", "registration failed"))?;
        drop(current);
        handle
            .register_agent_profiles(profiles)
            .await
            .map_err(client_error)?;
        Ok(BridgeResponse::ToolsRegistered {
            tool_count,
            profile_count,
        })
    }

    async fn with_tools<F, Fut>(&self, operation: F) -> Handled
    where
        F: FnOnce(ClientToolHandle) -> Fut,
        Fut: std::future::Future<Output = Result<(), ClientError>>,
    {
        let handle = self
            .tools
            .lock()
            .await
            .as_ref()
            .filter(|provider| !provider.pump.is_finished())
            .map(|provider| provider.handle.clone())
            .ok_or_else(|| {
                error(
                    "tools_not_registered",
                    "register tools before claiming sessions or answering invocations",
                )
            })?;
        operation(handle).await.map_err(client_error)?;
        Ok(BridgeResponse::Done)
    }

    /// One snapshot of a session's view.
    async fn snapshot(
        &self,
        session_id: SessionId,
        anchor: ProjectionWindowAnchor,
        max_items: usize,
    ) -> Result<SessionViewSnapshot, BridgeError> {
        let (sender, mut receiver) = mpsc::channel(1);
        let client = self.client.clone();
        let follow = follow_request(session_id, anchor, max_items);
        let task = tokio::spawn(async move {
            let mut sender = sender;
            follow_session_view(client, follow, &mut sender).await;
        });
        let first = receiver.recv().await;
        task.abort();
        match first {
            Some(SessionViewFollowUpdate::Snapshot(snapshot)) => Ok(*snapshot),
            Some(SessionViewFollowUpdate::Disconnected { message }) => {
                Err(error("session_unavailable", &message))
            }
            None => Err(error("session_unavailable", "the session view ended")),
        }
    }

    fn observe(self: &Arc<Self>, session_id: SessionId, max_items: usize) -> BridgeResponse {
        let observation_id = format!(
            "observation-{}",
            self.next_observation.fetch_add(1, Ordering::Relaxed)
        );
        let (latest, changes) = watch::channel(None);
        let follow = follow_request(session_id, ProjectionWindowAnchor::Latest, max_items);
        let client = self.client.clone();
        let bridge = Arc::downgrade(self);
        let id = observation_id.clone();
        let task = tokio::spawn(async move {
            let mut sink = LatestSnapshot(latest);
            let follower = follow_session_view(client, follow, &mut sink);
            let emitter = emit_observations(bridge.clone(), id.clone(), changes, max_items);
            let reason = tokio::select! {
                () = follower => "the session view ended".to_owned(),
                reason = emitter => reason,
            };
            if let Some(bridge) = bridge.upgrade() {
                bridge
                    .observations
                    .lock()
                    .expect("observations poisoned")
                    .remove(&id);
                bridge
                    .emit(BridgeEvent::ObservationEnded {
                        observation_id: id,
                        reason,
                    })
                    .await;
            }
        });
        self.observations
            .lock()
            .expect("observations poisoned")
            .insert(observation_id.clone(), task);
        BridgeResponse::Observing { observation_id }
    }
}

/// Keeps only the newest update; the emitter samples it at most every
/// [`OBSERVATION_INTERVAL`].
struct LatestSnapshot(watch::Sender<Option<SessionViewFollowUpdate>>);

impl SessionViewSink for LatestSnapshot {
    fn deliver(&mut self, update: SessionViewFollowUpdate) -> impl Future<Output = bool> + Send {
        let ends = matches!(update, SessionViewFollowUpdate::Disconnected { .. });
        std::future::ready(self.0.send(Some(update)).is_ok() && !ends)
    }

    fn is_closed(&self) -> bool {
        self.0.is_closed()
    }
}

/// Send projected snapshots until the follow ends; returns why it ended.
async fn emit_observations(
    bridge: std::sync::Weak<Bridge>,
    observation_id: String,
    mut changes: watch::Receiver<Option<SessionViewFollowUpdate>>,
    max_items: usize,
) -> String {
    let mut sequence = 0;
    loop {
        if changes.changed().await.is_err() {
            return "the session view ended".to_owned();
        }
        let update = changes.borrow_and_update().clone();
        let Some(bridge) = bridge.upgrade() else {
            return "the bridge closed".to_owned();
        };
        match update {
            Some(SessionViewFollowUpdate::Snapshot(snapshot)) => {
                bridge
                    .emit(BridgeEvent::Observation {
                        observation_id: observation_id.clone(),
                        sequence,
                        view: Box::new(view::project(&snapshot, max_items)),
                    })
                    .await;
                sequence += 1;
            }
            Some(SessionViewFollowUpdate::Disconnected { message }) => return message,
            None => {}
        }
        drop(bridge);
        tokio::time::sleep(OBSERVATION_INTERVAL).await;
    }
}

/// Forward the daemon's requests to the application's tools as events.
async fn pump_tool_requests(
    bridge: std::sync::Weak<Bridge>,
    mut requests: bcode_client::client_tools::ClientToolRequests,
) {
    while let Some(request) = requests.recv().await {
        let Some(bridge) = bridge.upgrade() else {
            return;
        };
        let event = match request {
            ClientToolRequest::Invoke(invocation) => BridgeEvent::ToolInvoke(ToolInvocation {
                invocation_id: invocation.invocation_id,
                session_id: invocation.session_id,
                tool_call_id: invocation.tool_call_id,
                tool_name: invocation.tool_name,
                arguments_json: invocation.arguments_json,
                deadline_ms: invocation.deadline_ms,
            }),
            ClientToolRequest::Cancel { invocation_id } => {
                BridgeEvent::ToolCancel { invocation_id }
            }
            ClientToolRequest::ClaimLost { session_id } => BridgeEvent::ClaimLost {
                session_id: session_id.to_string(),
            },
        };
        bridge.emit(event).await;
    }
    if let Some(bridge) = bridge.upgrade() {
        bridge
            .emit(BridgeEvent::ToolsLost {
                reason: "the daemon connection closed; register tools again and re-claim sessions"
                    .to_owned(),
            })
            .await;
    }
}

/// One catalog page. While discovery is still loading and nothing was found yet, wait a bounded
/// time for it, so an application's first listing is not mistaken for "no sessions".
async fn list_page(
    client: &BcodeClient,
    scope: SessionDiscoveryScope,
    query: SessionCatalogQuery,
    cursor: Option<bcode_session_models::SessionCatalogCursor>,
    limit: usize,
) -> Result<bcode_session_models::SessionCatalogPage, BridgeError> {
    let deadline = tokio::time::Instant::now() + CATALOG_LOAD_WAIT;
    loop {
        let page = client
            .list_sessions_page_with_scope(scope.clone(), query.clone(), cursor.clone(), limit)
            .await
            .map_err(client_error)?;
        let loading = matches!(
            page.catalog_status,
            bcode_session_models::SessionCatalogStatus::NotStarted
                | bcode_session_models::SessionCatalogStatus::Loading
        );
        if !(loading && page.sessions.is_empty()) || tokio::time::Instant::now() >= deadline {
            return Ok(page);
        }
        tokio::time::sleep(CATALOG_LOAD_POLL).await;
    }
}

fn follow_request(
    session_id: SessionId,
    anchor: ProjectionWindowAnchor,
    max_items: usize,
) -> SessionViewFollow {
    SessionViewFollow {
        session_id,
        projection: ProjectionWindowRequest {
            projection: SessionProjectionKind::Transcript,
            anchor,
            direction: ProjectionWindowDirection::Backward,
            target: ProjectionWindowTarget {
                min_items: Some(max_items.max(1)),
                min_estimated_rows: None,
                min_bytes: None,
                width_columns: None,
            },
            limits: ProjectionWindowLimits {
                max_items: max_items.max(1),
                max_events_scanned: 4_096,
                max_bytes: 1024 * 1024,
            },
        },
        reasoning_policy: ReasoningPresentationPolicy::Summary,
        client_name: CLIENT_NAME.to_owned(),
        interaction_kind: None,
    }
}

fn session_origin(origin: Origin) -> SessionOrigin {
    let mut converted = SessionOrigin::new(origin.app_id);
    if let Some(name) = origin.display_name {
        converted = converted.with_display_name(name);
    }
    for (key, value) in origin.context {
        converted = converted.with_context(key, value);
    }
    if origin.background {
        converted = converted.with_visibility(SessionVisibility::Background);
    }
    converted
}

fn summary(session: &bcode_session_models::SessionSummary) -> SessionSummary {
    SessionSummary {
        id: session.id.to_string(),
        title: session.title().map(str::to_owned),
        working_directory: session.working_directory.display().to_string(),
        created_at_ms: session.created_at_ms,
        updated_at_ms: session.updated_at_ms,
        client_count: session.client_count,
        origin: session.origin.as_ref().map(|origin| Origin {
            app_id: origin.app_id.clone(),
            display_name: origin.app_display_name.clone(),
            context: origin.context.clone(),
            background: origin.visibility == SessionVisibility::Background,
        }),
    }
}

#[allow(clippy::cast_precision_loss)]
fn view_dollars(micros: u64) -> f64 {
    micros as f64 / 1_000_000.0
}
