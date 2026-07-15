#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Example native Bcode plugin.

use bcode_plugin_sdk::prelude::*;

/// Example plugin used by smoke tests.
#[derive(Default)]
pub struct HelloPlugin {
    event_count: usize,
}

impl RustPlugin for HelloPlugin {
    fn activate(&mut self) -> Result<(), PluginError> {
        Ok(())
    }

    fn deactivate(&mut self) -> Result<(), PluginError> {
        Ok(())
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != "example-hello/v1" {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported hello service interface",
            );
        }
        match context.request.operation.as_str() {
            "echo" => ServiceResponse::ok(context.request.payload),
            "bridge-exchange" => {
                let request = ServiceBridgeRequest::Exchange(bcode_tool::ToolExchangeRequest {
                    invocation_id: "hello-invocation".to_string(),
                    exchange_id: "hello-exchange".to_string(),
                    producer_id: "example.hello".to_string(),
                    schema: "example.hello.exchange".to_string(),
                    schema_version: 1,
                    payload: serde_json::Value::Null,
                    response_policy: bcode_tool::ToolExchangeResponsePolicy::Required,
                });
                match context.bridge.request(&request) {
                    Ok(response) => ServiceResponse::json(&response).unwrap_or_else(|error| {
                        ServiceResponse::error("bridge_response_encode_failed", error.to_string())
                    }),
                    Err(error) => ServiceResponse::error("bridge_failed", error.to_string()),
                }
            }
            "emit-event" => {
                context.events.emit(b"hello-event");
                ServiceResponse::text("event-emitted")
            }
            "bridge-all" => {
                let requests = [
                    ServiceBridgeRequest::Exchange(bcode_tool::ToolExchangeRequest {
                        invocation_id: "hello-invocation".to_string(),
                        exchange_id: "hello-exchange".to_string(),
                        producer_id: "example.hello".to_string(),
                        schema: "example.hello.exchange".to_string(),
                        schema_version: 1,
                        payload: serde_json::Value::Null,
                        response_policy: bcode_tool::ToolExchangeResponsePolicy::Required,
                    }),
                    ServiceBridgeRequest::ReceiveInput {
                        invocation_id: "hello-invocation".to_string(),
                    },
                    ServiceBridgeRequest::InvokeService(bcode_tool::ToolInvocationServiceRequest {
                        invocation_id: "hello-invocation".to_string(),
                        request_id: "hello-service".to_string(),
                        interface_id: "example.nested/v1".to_string(),
                        operation: "run".to_string(),
                        payload: serde_json::Value::Null,
                    }),
                    ServiceBridgeRequest::WriteArtifact(bcode_tool::ToolArtifactWriteRequest {
                        invocation_id: "hello-invocation".to_string(),
                        artifact_id: "hello-artifact".to_string(),
                        content_type: "text/plain".to_string(),
                        bytes: b"hello".to_vec(),
                        metadata: serde_json::Value::Null,
                    }),
                ];
                let responses = requests
                    .iter()
                    .map(|request| context.bridge.request(request))
                    .collect::<Result<Vec<_>, _>>();
                match responses {
                    Ok(responses) => ServiceResponse::json(&responses).unwrap_or_else(|error| {
                        ServiceResponse::error("bridge_response_encode_failed", error.to_string())
                    }),
                    Err(error) => ServiceResponse::error("bridge_failed", error.to_string()),
                }
            }
            "wait-cancelled" => {
                if context
                    .cancellation
                    .wait_cancelled(std::time::Duration::from_secs(5))
                {
                    ServiceResponse::text("cancelled")
                } else {
                    ServiceResponse::text("timeout")
                }
            }
            "event-count" => ServiceResponse::text(self.event_count.to_string()),
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported hello service operation",
            ),
        }
    }

    fn handle_event(&mut self, context: NativeEventContext) -> Result<(), PluginError> {
        if context.event.topic == "example.event" || context.event.topic == "bcode.session.event" {
            self.event_count += 1;
        }
        Ok(())
    }
}

/// Return the statically linked hello plugin vtable.
#[must_use]
pub fn static_plugin() -> StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(HelloPlugin, include_str!("../bcode-plugin.toml"))
}

bcode_plugin_sdk::export_plugin!(HelloPlugin, include_str!("../bcode-plugin.toml"));
