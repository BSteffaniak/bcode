//! Composer service routing; errors remain path-free and ordinary input survives.

use bcode_plugin_sdk::composer::{
    COMPOSER_INTERFACE_ID, ComposerSearchRequest, ComposerSnapshot, ComposerSnapshotState,
    OP_COMPOSER_SEARCH,
};
use bcode_plugin_sdk::prelude::*;

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;

/// Disableable path-reference service plugin.
#[derive(Debug, Default)]
pub struct FileReferencesPlugin;

impl RustPlugin for FileReferencesPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != COMPOSER_INTERFACE_ID
            || context.request.operation != OP_COMPOSER_SEARCH
        {
            return ServiceResponse::error(
                "unsupported_service",
                "unsupported file-reference service",
            );
        }
        let Ok(request) = context.request.payload_json::<ComposerSearchRequest>() else {
            return ServiceResponse::error("invalid_request", "invalid composer request");
        };
        if !request.contribution_id.is_empty() && request.contribution_id != crate::CONTRIBUTION_ID
        {
            return ServiceResponse::error(
                "unsupported_contribution",
                "unsupported composer contribution",
            );
        }
        if request.validate().is_err() {
            return ServiceResponse::error("invalid_request", "invalid composer request");
        }
        let snapshot =
            crate::search::search(&context, &request).unwrap_or_else(|code| ComposerSnapshot {
                request_id: request.request_id.clone(),
                document_id: request.document.document_id.clone(),
                revision: request.document.revision,
                state: ComposerSnapshotState::Error { code: code.into() },
                candidates: Vec::new(),
                interaction_state: request.interaction_state.clone(),
                presentation: None,
                incomplete: false,
            });
        ServiceResponse::json(&snapshot).unwrap_or_else(|_| {
            ServiceResponse::error("encode_failed", "could not encode composer snapshot")
        })
    }
}
