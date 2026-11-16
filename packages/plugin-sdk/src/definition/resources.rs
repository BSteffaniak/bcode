//! Portable resource operations over the existing renderer-neutral controllers.
//!
//! Resources belong to one selected frontend instance; IDs never confer host
//! authority. Shared runtime admission/activation owns execution and teardown.

use super::{
    Deserialize, Mutex, PluginCallContext, PluginCommandError, PluginResult, Serialize,
    encode_extension,
};
use std::collections::BTreeMap;

/// Exact portable interaction resource contract.
pub const INTERACTION_RESOURCE_CONTRACT: &str = "bcode.interaction-resources/v1";

/// Per-instance resource policy, not a global controller count or lifetime limit.
#[derive(Debug, Clone, Copy)]
pub struct InteractionResourcePolicy {
    /// Simultaneously open controllers in this frontend instance.
    pub max_open: usize,
}
impl Default for InteractionResourcePolicy {
    fn default() -> Self {
        Self { max_open: 256 }
    }
}

/// Frontend resource operations. Host admission, not these IDs, authenticates callers.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum InteractionResourceRequest {
    /// Open an existing registered controller factory.
    Open {
        kind: String,
        request: serde_json::Value,
    },
    /// Read the current renderer-neutral state without mutation.
    Snapshot { resource: uuid::Uuid },
    /// Apply input once against the observed resource revision.
    Input {
        resource: uuid::Uuid,
        expected_revision: u64,
        input: crate::interaction::InteractionInput,
    },
    /// Explicitly relinquish a resource at its observed revision.
    Destroy {
        resource: uuid::Uuid,
        expected_revision: u64,
    },
}

/// Resource state; submitted output is a proposal, never an authoritative commit.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum InteractionResourceResponse {
    /// Current controller snapshot at one revision.
    State {
        resource: uuid::Uuid,
        revision: u64,
        snapshot: serde_json::Value,
        output: crate::interaction::InteractionOutput,
        terminal: bool,
    },
    /// Resource was explicitly closed.
    Destroyed { resource: uuid::Uuid },
}

struct ControllerResource {
    controller: crate::interaction::BoxedPluginInteractionController,
    revision: u64,
    terminal: bool,
}

pub(super) struct InteractionResources {
    factory: Option<fn() -> crate::interaction::PluginInteractionRegistry>,
    policy: InteractionResourcePolicy,
    controllers: Mutex<BTreeMap<uuid::Uuid, ControllerResource>>,
}
impl InteractionResources {
    pub(super) fn new(
        factory: Option<fn() -> crate::interaction::PluginInteractionRegistry>,
        policy: InteractionResourcePolicy,
    ) -> Self {
        Self {
            factory,
            policy,
            controllers: Mutex::new(BTreeMap::new()),
        }
    }
    pub(super) fn clear(&self) -> PluginResult<()> {
        let mut controllers = self.controllers.try_lock().map_err(|_| unavailable())?;
        controllers.clear();
        drop(controllers);
        Ok(())
    }
    pub(super) fn invoke(&self, call: &PluginCallContext, payload: &[u8]) -> PluginResult<Vec<u8>> {
        if call.cancellation.is_cancelled() {
            return Err(unavailable());
        }
        let request: InteractionResourceRequest =
            serde_json::from_slice(payload).map_err(|_| {
                PluginCommandError::invalid_arguments("invalid interaction resource operation")
            })?;
        let response = self.execute(call, request)?;
        encode_extension(&response)
    }
    pub(super) fn execute(
        &self,
        call: &PluginCallContext,
        request: InteractionResourceRequest,
    ) -> PluginResult<InteractionResourceResponse> {
        if call.cancellation.is_cancelled() {
            return Err(unavailable());
        }
        let mut resources = self.controllers.try_lock().map_err(|_| unavailable())?;
        match request {
            InteractionResourceRequest::Open { kind, request } => {
                if resources.len() >= self.policy.max_open {
                    return Err(unavailable());
                }
                let factory = self.factory.ok_or_else(unavailable)?;
                let controller = factory().open(&kind, request).map_err(|_| unavailable())?;
                let snapshot = controller.snapshot_json().map_err(|_| unavailable())?;
                let resource = uuid::Uuid::new_v4();
                let response = InteractionResourceResponse::State {
                    resource,
                    revision: 0,
                    snapshot,
                    output: crate::interaction::InteractionOutput::None,
                    terminal: false,
                };
                encode_extension(&response)?;
                resources.insert(
                    resource,
                    ControllerResource {
                        controller,
                        revision: 0,
                        terminal: false,
                    },
                );
                drop(resources);
                Ok(response)
            }
            InteractionResourceRequest::Snapshot { resource } => {
                let state = resources.get(&resource).ok_or_else(unavailable)?;
                let response = InteractionResourceResponse::State {
                    resource,
                    revision: state.revision,
                    snapshot: state
                        .controller
                        .snapshot_json()
                        .map_err(|_| unavailable())?,
                    output: crate::interaction::InteractionOutput::None,
                    terminal: state.terminal,
                };
                drop(resources);
                Ok(response)
            }
            InteractionResourceRequest::Input {
                resource,
                expected_revision,
                input,
            } => {
                let state = resources.get_mut(&resource).ok_or_else(unavailable)?;
                if state.revision != expected_revision || state.terminal {
                    return Err(stale());
                }
                // Advance before mutation. A failed snapshot/response never permits
                // re-executing an input whose effects may already have occurred.
                state.revision = state.revision.checked_add(1).ok_or_else(unavailable)?;
                let output = state.controller.handle_input(input);
                state.terminal = matches!(
                    output,
                    crate::interaction::InteractionOutput::Submitted { .. }
                        | crate::interaction::InteractionOutput::Cancelled
                );
                let response = InteractionResourceResponse::State {
                    resource,
                    revision: state.revision,
                    snapshot: state
                        .controller
                        .snapshot_json()
                        .map_err(|_| unavailable())?,
                    output,
                    terminal: state.terminal,
                };
                drop(resources);
                Ok(response)
            }
            InteractionResourceRequest::Destroy {
                resource,
                expected_revision,
            } => {
                let state = resources.get(&resource).ok_or_else(unavailable)?;
                if state.revision != expected_revision {
                    return Err(stale());
                }
                resources.remove(&resource);
                drop(resources);
                Ok(InteractionResourceResponse::Destroyed { resource })
            }
        }
    }
}
fn unavailable() -> PluginCommandError {
    PluginCommandError::unavailable("interaction resource unavailable")
}
fn stale() -> PluginCommandError {
    PluginCommandError::invalid_arguments("stale or terminal interaction resource")
}

#[cfg(test)]
mod tests {
    use super::{
        InteractionResourcePolicy, InteractionResourceRequest, InteractionResourceResponse,
        InteractionResources, PluginCallContext,
    };
    struct Controller(u64);
    impl crate::interaction::PluginInteractionController for Controller {
        fn kind(&self) -> &'static str {
            "example"
        }
        fn snapshot_json(
            &self,
        ) -> Result<serde_json::Value, crate::interaction::PluginInteractionSnapshotError> {
            Ok(serde_json::json!({"count": self.0}))
        }
        fn handle_input(
            &mut self,
            input: crate::interaction::InteractionInput,
        ) -> crate::interaction::InteractionOutput {
            self.0 += 1;
            if matches!(input, crate::interaction::InteractionInput::Submit) {
                crate::interaction::InteractionOutput::Submitted {
                    payload: serde_json::json!({"count": self.0}),
                }
            } else {
                crate::interaction::InteractionOutput::Redraw
            }
        }
    }
    struct Factory;
    impl crate::interaction::PluginInteractionControllerFactory for Factory {
        fn interaction_kind(&self) -> &'static str {
            "example"
        }
        fn open(
            &self,
            _request: serde_json::Value,
        ) -> Result<
            crate::interaction::BoxedPluginInteractionController,
            crate::interaction::PluginInteractionError,
        > {
            Ok(Box::new(Controller(0)))
        }
    }
    fn registry() -> crate::interaction::PluginInteractionRegistry {
        let mut registry = crate::interaction::PluginInteractionRegistry::default();
        registry.try_register_factory(Box::new(Factory)).unwrap();
        registry
    }
    fn call() -> PluginCallContext {
        PluginCallContext {
            invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
            cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
            host_services: None,
            callback_execution: None,
        }
    }
    #[test]
    fn same_native_and_portable_resource_rejects_replay_and_foreign_handles() {
        let resources =
            InteractionResources::new(Some(registry), InteractionResourcePolicy { max_open: 1 });
        let other = InteractionResources::new(Some(registry), InteractionResourcePolicy::default());
        let call = call();
        let InteractionResourceResponse::State { resource, .. } = resources
            .execute(
                &call,
                InteractionResourceRequest::Open {
                    kind: "example".to_owned(),
                    request: serde_json::Value::Null,
                },
            )
            .unwrap()
        else {
            panic!("expected state");
        };
        assert!(
            other
                .execute(&call, InteractionResourceRequest::Snapshot { resource })
                .is_err()
        );
        assert!(
            resources
                .execute(
                    &call,
                    InteractionResourceRequest::Open {
                        kind: "example".to_owned(),
                        request: serde_json::Value::Null
                    }
                )
                .is_err()
        );
        let payload = serde_json::to_vec(&InteractionResourceRequest::Input {
            resource,
            expected_revision: 0,
            input: crate::interaction::InteractionInput::Submit,
        })
        .unwrap();
        let reply: InteractionResourceResponse =
            serde_json::from_slice(&resources.invoke(&call, &payload).unwrap()).unwrap();
        assert!(matches!(
            reply,
            InteractionResourceResponse::State {
                revision: 1,
                terminal: true,
                ..
            }
        ));
        assert!(resources.invoke(&call, &payload).is_err());
        resources
            .execute(
                &call,
                InteractionResourceRequest::Destroy {
                    resource,
                    expected_revision: 1,
                },
            )
            .unwrap();
        assert!(
            resources
                .execute(&call, InteractionResourceRequest::Snapshot { resource })
                .is_err()
        );
    }
    #[test]
    fn cancelled_resources_never_open() {
        let resources =
            InteractionResources::new(Some(registry), InteractionResourcePolicy::default());
        let call = call();
        call.cancellation.cancel();
        assert!(
            resources
                .execute(
                    &call,
                    InteractionResourceRequest::Open {
                        kind: "example".to_owned(),
                        request: serde_json::Value::Null
                    }
                )
                .is_err()
        );
    }
}
