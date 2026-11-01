//! Explicit expiring diagnostic policy from trusted plugin configuration.

use bcode_workflow::{
    WorkflowApplicationActorKind, WorkflowApplicationOperation, WorkflowApplicationOperationFacts,
    WorkflowExecutionObservationPolicyDecision as Decision,
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    version: u32,
    grants: Vec<Grant>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    actor_kind: WorkflowApplicationActorKind,
    actor_id: Option<String>,
    root_run_id: String,
    namespace: String,
    state_location_id: String,
    recursive: bool,
    deep_capture: bool,
    expires_at_unix_ms: u64,
    operations: Vec<WorkflowApplicationOperation>,
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

impl Grant {
    fn valid(&self) -> bool {
        self.actor_kind == WorkflowApplicationActorKind::LocalClient
            && self.actor_id.as_deref().is_none_or(valid_identity)
            && [&self.root_run_id, &self.namespace, &self.state_location_id]
                .into_iter()
                .all(|id| valid_identity(id))
            && self.expires_at_unix_ms > 0
            && !self.operations.is_empty()
            && self.operations.len() <= 4
            && self.operations.iter().enumerate().all(|(index, op)| {
                op.is_execution_observation()
                    && !self.operations[..index].contains(op)
                    && (*op != WorkflowApplicationOperation::ExportExecutionObservation
                        || self.deep_capture)
            })
    }
}

#[must_use]
pub fn evaluate(
    config: &serde_json::Value,
    facts: &WorkflowApplicationOperationFacts,
    now_unix_ms: u64,
) -> Decision {
    let Some(value) = config.get("execution_observation_policy") else {
        return Decision::Deny;
    };
    if value
        .get("grants")
        .and_then(serde_json::Value::as_array)
        .is_none_or(|grants| grants.len() > 256)
    {
        return Decision::Deny;
    }
    let Ok(policy) = serde_json::from_value::<Policy>(value.clone()) else {
        return Decision::Deny;
    };
    if policy.version != 1
        || policy.grants.len() > 256
        || !policy.grants.iter().all(Grant::valid)
        || facts.validate().is_err()
        || !facts.operation.is_execution_observation()
        || facts.actor.kind != WorkflowApplicationActorKind::LocalClient
    {
        return Decision::Deny;
    }
    let Some(scope) = facts.execution_observation.as_ref() else {
        return Decision::Deny;
    };
    policy
        .grants
        .iter()
        .filter(|grant| {
            grant.expires_at_unix_ms > now_unix_ms
                && grant
                    .actor_id
                    .as_ref()
                    .is_none_or(|id| id == &facts.actor.actor_id)
                && grant.root_run_id == scope.root_run_id
                && grant.namespace == scope.namespace
                && grant.state_location_id == scope.state_location_id
                && grant.recursive == scope.recursive
                && grant.deep_capture == scope.deep_capture
                && grant.operations.contains(&facts.operation)
        })
        .map(|grant| grant.expires_at_unix_ms)
        .max()
        .map_or(Decision::Deny, |expires_at_unix_ms| Decision::Allow {
            expires_at_unix_ms,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_workflow::{WorkflowApplicationActor, WorkflowExecutionObservationFacts};
    use serde_json::json;

    fn facts() -> WorkflowApplicationOperationFacts {
        WorkflowApplicationOperationFacts {
            version: bcode_workflow::WORKFLOW_APPLICATION_OPERATION_FACTS_VERSION,
            operation: WorkflowApplicationOperation::ObserveExecution,
            actor: WorkflowApplicationActor {
                kind: WorkflowApplicationActorKind::LocalClient,
                actor_id: "alice".into(),
            },
            execution_observation: Some(WorkflowExecutionObservationFacts {
                root_run_id: "root".into(),
                namespace: "namespace".into(),
                state_location_id: "location".into(),
                recursive: true,
                deep_capture: true,
            }),
            definition_registration: None,
            workflow_id: String::new(),
            draft_id: None,
            revision: None,
            preset_id: None,
            producer: None,
            requirements: bcode_workflow::WorkflowRequirementSummary::default(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
        }
    }

    fn config() -> serde_json::Value {
        json!({"execution_observation_policy":{"version":1,"grants":[{
            "actor_kind":"local_client", "actor_id":"alice", "root_run_id":"root",
            "namespace":"namespace", "state_location_id":"location",
            "recursive":true,"deep_capture":true,"expires_at_unix_ms":1000,
            "operations":["observe_execution","read_execution_observation",
                "finalize_execution_observation","export_execution_observation"]
        }]}})
    }

    #[test]
    fn exact_expiring_local_grants_only_authorize_listed_diagnostics() {
        let config = config();
        let mut facts = facts();
        for operation in [
            WorkflowApplicationOperation::ObserveExecution,
            WorkflowApplicationOperation::ReadExecutionObservation,
            WorkflowApplicationOperation::FinalizeExecutionObservation,
            WorkflowApplicationOperation::ExportExecutionObservation,
        ] {
            facts.operation = operation;
            assert_eq!(
                evaluate(&config, &facts, 999),
                Decision::Allow {
                    expires_at_unix_ms: 1000
                }
            );
            assert_eq!(evaluate(&config, &facts, 1000), Decision::Deny);
        }
        facts.operation = WorkflowApplicationOperation::StartActiveRevision;
        assert_eq!(evaluate(&config, &facts, 1), Decision::Deny);
    }

    #[test]
    fn foreign_scope_actor_and_future_facts_are_denied() {
        let config = config();
        for key in [
            "root_run_id",
            "namespace",
            "state_location_id",
            "recursive",
            "deep_capture",
        ] {
            let mut value = serde_json::to_value(facts()).unwrap();
            let scope = &mut value["execution_observation"];
            scope[key] = if matches!(key, "recursive" | "deep_capture") {
                json!(false)
            } else {
                json!("foreign")
            };
            let foreign = serde_json::from_value(value).unwrap();
            assert_eq!(evaluate(&config, &foreign, 1), Decision::Deny);
        }
        let mut foreign = facts();
        foreign.actor.actor_id = "bob".into();
        assert_eq!(evaluate(&config, &foreign, 1), Decision::Deny);
        foreign.actor.kind = WorkflowApplicationActorKind::Plugin;
        assert_eq!(evaluate(&config, &foreign, 1), Decision::Deny);
        foreign = facts();
        foreign.version += 1;
        assert_eq!(evaluate(&config, &foreign, 1), Decision::Deny);
    }

    #[test]
    fn absent_malformed_future_and_shallow_export_config_fail_closed() {
        assert_eq!(evaluate(&json!({}), &facts(), 1), Decision::Deny);
        for (path, value) in [
            ("version", json!(2)),
            ("unknown", json!(true)),
            ("grants", json!(null)),
        ] {
            let mut config = config();
            config["execution_observation_policy"][path] = value;
            assert_eq!(evaluate(&config, &facts(), 1), Decision::Deny);
        }
        for (key, value) in [
            ("deep_capture", json!(false)),
            ("expires_at_unix_ms", json!(null)),
            ("expires_at_unix_ms", json!(0)),
            ("actor_kind", json!("plugin")),
            ("operations", json!(["publish_and_start"])),
            (
                "operations",
                json!(["observe_execution", "observe_execution"]),
            ),
            ("unknown", json!(true)),
        ] {
            let mut config = config();
            config["execution_observation_policy"]["grants"][0][key] = value;
            assert_eq!(evaluate(&config, &facts(), 1), Decision::Deny);
        }
    }

    #[test]
    fn shallow_grants_and_mixed_invalid_grants_never_enable_export() {
        let mut config = config();
        let grant = &mut config["execution_observation_policy"]["grants"][0];
        grant["deep_capture"] = json!(false);
        grant["operations"] = json!(["observe_execution", "read_execution_observation"]);
        let mut facts = facts();
        facts.execution_observation.as_mut().unwrap().deep_capture = false;
        assert_eq!(
            evaluate(&config, &facts, 1),
            Decision::Allow {
                expires_at_unix_ms: 1000
            }
        );
        facts.operation = WorkflowApplicationOperation::ExportExecutionObservation;
        assert_eq!(evaluate(&config, &facts, 1), Decision::Deny);
        let mut mixed = super::tests::config();
        mixed["execution_observation_policy"]["grants"]
            .as_array_mut()
            .unwrap()
            .push(json!({"unknown":true}));
        assert_eq!(evaluate(&mixed, &super::tests::facts(), 1), Decision::Deny);
    }

    #[test]
    fn service_uses_trusted_config_and_never_payload_supplied_grants() {
        use bcode_plugin_sdk::prelude::*;
        use bcode_plugin_sdk::{PluginConfigContext, ServiceCancellation};
        let mut trusted = config();
        trusted["execution_observation_policy"]["grants"][0]["expires_at_unix_ms"] =
            json!(u64::MAX);
        let invoke = |payload, config| {
            let mut context = NativeServiceContext {
                diagnostics: None,
                plugin_id: "bcode.workflow".into(),
                request: ServiceRequest {
                    interface_id:
                        bcode_workflow::WORKFLOW_EXECUTION_OBSERVATION_POLICY_INTERFACE_ID.into(),
                    operation: bcode_workflow::OP_AUTHORIZE_EXECUTION_OBSERVATION.into(),
                    payload,
                },
                config: PluginConfigContext::default(),
                events: ServiceEventEmitter::default(),
                cancellation: ServiceCancellation::default(),
                bridge: ServiceBridge::default(),
                transient_progress_limits: TransientProgressLimits::default(),
            };
            context.config.config = config;
            crate::WorkflowPlugin
                .invoke_service(context)
                .payload_json::<Decision>()
                .unwrap()
        };
        let payload = serde_json::to_vec(&facts()).unwrap();
        assert_eq!(
            invoke(payload.clone(), trusted.clone()),
            Decision::Allow {
                expires_at_unix_ms: u64::MAX
            }
        );
        assert_eq!(invoke(payload, json!({})), Decision::Deny);
        let mut untrusted = serde_json::to_value(facts()).unwrap();
        untrusted["execution_observation_policy"] = trusted["execution_observation_policy"].clone();
        assert_eq!(
            invoke(serde_json::to_vec(&untrusted).unwrap(), trusted),
            Decision::Deny
        );
    }

    #[test]
    fn optional_actor_is_still_local_and_operation_allowlist_is_exact() {
        let mut config = config();
        config["execution_observation_policy"]["grants"][0]
            .as_object_mut()
            .unwrap()
            .remove("actor_id");
        let mut facts = facts();
        facts.actor.actor_id = "bob".into();
        assert_eq!(
            evaluate(&config, &facts, 1),
            Decision::Allow {
                expires_at_unix_ms: 1000
            }
        );
        config["execution_observation_policy"]["grants"][0]["operations"] =
            json!(["read_execution_observation"]);
        assert_eq!(evaluate(&config, &facts, 1), Decision::Deny);
        facts.actor.kind = WorkflowApplicationActorKind::Service;
        assert_eq!(evaluate(&config, &facts, 1), Decision::Deny);
    }
}
