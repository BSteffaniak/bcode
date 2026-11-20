#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Feedback plugin: owns the feedback store inside the daemon.
//!
//! * `bcode.feedback/v1` serves typed store operations to clients such as `bcode feedback`
//!   and `bcode-feedback`, acting as [`Actor::User`].
//! * `bcode.tool/v1` gives every session `feedback.*` tools acting as [`Actor::Agent`]. Tool
//!   output redacts sensitive facet values.
//!
//! The store opens lazily on first use, so a damaged or newer feedback database fails only
//! feedback operations and never the daemon.

mod tools;

use std::path::PathBuf;

use bcode_feedback::{FeedbackStore, FeedbackStoreError};
use bcode_feedback_models::{
    Actor, AttachFeedbackRequest, ClaimIntakeSessionRequest, FEEDBACK_SERVICE_INTERFACE_ID,
    FeedbackQuery, GetFeedbackRequest, LinkFeedbackRequest, OP_ATTACH, OP_CLAIM_INTAKE_SESSION,
    OP_GET, OP_INTAKE_SESSION, OP_LINK, OP_LIST, OP_RECORD, OP_SET_STATUS, OP_TRIAGE,
    RecordFeedbackRequest, SetFeedbackStatusRequest, TriageFeedbackRequest,
};
use bcode_plugin_sdk::prelude::*;

pub use bcode_feedback_models::FEEDBACK_PLUGIN_ID as PLUGIN_ID;

/// Daemon-side owner of the feedback store.
#[derive(Default)]
pub struct FeedbackPlugin {
    state_dir: Option<PathBuf>,
    store: Option<FeedbackStore>,
}

impl FeedbackPlugin {
    /// Use an explicit state location instead of the process default.
    #[must_use]
    pub const fn with_state_dir(state_dir: PathBuf) -> Self {
        Self {
            state_dir: Some(state_dir),
            store: None,
        }
    }

    fn store(&mut self) -> Result<&FeedbackStore, FeedbackStoreError> {
        if self.store.is_none() {
            let state_dir = self
                .state_dir
                .clone()
                .unwrap_or_else(bcode_config::default_state_dir);
            self.store = Some(FeedbackStore::open_in_state_dir(&state_dir)?);
        }
        Ok(self.store.as_ref().expect("store opened above"))
    }

    fn invoke_store(&mut self, request: &ServiceRequest) -> ServiceResponse {
        let now = now_ms();
        let store = match self.store() {
            Ok(store) => store,
            Err(error) => return store_error(&error),
        };
        let result = match request.operation.as_str() {
            OP_RECORD => decode::<RecordFeedbackRequest>(request)
                .and_then(|payload| encode(store.record(&payload, now))),
            OP_GET => decode::<GetFeedbackRequest>(request)
                .and_then(|payload| encode(store.get(payload.id))),
            OP_LIST => {
                decode::<FeedbackQuery>(request).and_then(|payload| encode(store.list(&payload)))
            }
            OP_TRIAGE => decode::<TriageFeedbackRequest>(request)
                .and_then(|payload| encode(store.triage(&payload, now))),
            OP_SET_STATUS => decode::<SetFeedbackStatusRequest>(request).and_then(|payload| {
                // Clients of this interface act for the person; agents use feedback tools.
                let payload = SetFeedbackStatusRequest {
                    actor: Actor::User,
                    ..payload
                };
                encode(store.set_status(&payload, now))
            }),
            OP_LINK => decode::<LinkFeedbackRequest>(request)
                .and_then(|payload| encode(store.link(&payload, now))),
            OP_ATTACH => decode::<AttachFeedbackRequest>(request)
                .and_then(|payload| encode(store.attach(&payload, now))),
            OP_INTAKE_SESSION => encode(store.intake_session()),
            OP_CLAIM_INTAKE_SESSION => decode::<ClaimIntakeSessionRequest>(request)
                .and_then(|payload| encode(store.claim_intake_session(&payload))),
            _ => Err(ServiceResponse::error(
                "unsupported_operation",
                "unsupported feedback operation",
            )),
        };
        result.unwrap_or_else(|response| response)
    }
}

impl RustPlugin for FeedbackPlugin {
    fn component_registrations() -> bcode_plugin_sdk::definition::ProductRegistrations {
        product_registrations()
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match context.request.interface_id.as_str() {
            FEEDBACK_SERVICE_INTERFACE_ID => self.invoke_store(&context.request),
            bcode_tool::TOOL_SERVICE_INTERFACE_ID => tools::invoke(self, &context),
            _ => ServiceResponse::error("unsupported_interface", "unsupported feedback interface"),
        }
    }
}

fn decode<T: serde::de::DeserializeOwned>(request: &ServiceRequest) -> Result<T, ServiceResponse> {
    request
        .payload_json::<T>()
        .map_err(|error| ServiceResponse::error("invalid_request", error.to_string()))
}

fn encode<T: serde::Serialize>(
    result: Result<T, FeedbackStoreError>,
) -> Result<ServiceResponse, ServiceResponse> {
    let value = result.map_err(|error| store_error(&error))?;
    ServiceResponse::json(&value)
        .map_err(|_| ServiceResponse::error("encode_failed", "feedback response encoding failed"))
}

/// Map a store failure to a stable service error code.
fn store_error(error: &FeedbackStoreError) -> ServiceResponse {
    let code = match error {
        FeedbackStoreError::Invalid(_) => "invalid_request",
        FeedbackStoreError::NotFound(_) => "not_found",
        FeedbackStoreError::Forbidden(_) => "forbidden",
        FeedbackStoreError::Conflict(_) => "conflict",
        FeedbackStoreError::AttachmentRejected(_) => "attachment_rejected",
        FeedbackStoreError::UnsupportedSchema(_) => "maintenance_required",
        FeedbackStoreError::Path(..)
        | FeedbackStoreError::Corrupt(_)
        | FeedbackStoreError::Database(_) => "store_unavailable",
    };
    ServiceResponse::error(code, error.to_string())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// `bcode feedback`: the shared feedback command line, run against the daemon.
#[must_use]
pub fn cli_registration() -> bcode_plugin_sdk::StaticCliRegistration {
    use clap::{CommandFactory as _, FromArgMatches as _};
    bcode_plugin_sdk::StaticCliRegistration {
        requires_daemon: |_| true,
        command: bcode_feedback_cli::FeedbackCli::command,
        invoke: |matches| {
            Box::pin(async move {
                let cli = bcode_feedback_cli::FeedbackCli::from_arg_matches(&matches)
                    .map_err(|error| error.to_string())?;
                bcode_feedback_cli::run(cli, &bcode_client::BcodeClient::default_endpoint())
                    .await?;
                Ok(bcode_plugin_sdk::StaticCliOutcome::default())
            })
        },
        invoke_with_execution: None,
    }
}

/// Capabilities registered with the shared plugin definition.
#[must_use]
pub fn product_registrations() -> bcode_plugin_sdk::definition::ProductRegistrations {
    bcode_plugin_sdk::definition::ProductRegistrations {
        cli: Some(cli_registration()),
        ..bcode_plugin_sdk::definition::ProductRegistrations::default()
    }
}

bcode_plugin_sdk::define_plugin!(
    serial FeedbackPlugin, include_str!("../bcode-plugin.toml"),
    registrations = product_registrations(),
);

#[cfg(test)]
mod tests;
