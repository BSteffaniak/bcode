//! Optional model selection extension; legacy harness Open is unchanged.
use crate::{
    HARNESS_SCHEMA_VERSION, HarnessContractError, HarnessFailure, HarnessInstance,
    HarnessOpenRequest, HarnessRequest, HarnessRequestKind, HarnessServiceResponse,
    HarnessServiceResponseKind, HarnessSessionTarget, MAX_HARNESS_ID_BYTES, MAX_HARNESS_WIRE_BYTES,
    id, text, wire,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Explicit opt-in compatibility boundary. Unsupported plugins must not receive selected opens.
pub const HARNESS_MODELS_INTERFACE_ID: &str = "bcode.harness.models/v1";
/// Discovery is one complete bounded native snapshot, not an unbounded catalog scan.
pub const MAX_HARNESS_MODELS: usize = 128;

/// Native-advertised metadata, not proof of account entitlement or successful inference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessModel {
    /// Opaque exact native selection key. Alias/provider resolution remains host-owned.
    pub model_id: String,
    pub display_name: String,
    pub description: Option<String>,
}
impl HarnessModel {
    /// Validate bounded metadata.
    /// # Errors
    /// Rejects invalid identity or excessive text.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        id(&self.model_id)?;
        text(&self.display_name)?;
        if self.display_name.trim().is_empty() {
            return Err(HarnessContractError::InvalidShape);
        }
        if let Some(description) = &self.description {
            text(description)?;
        }
        Ok(())
    }
}

/// No native default or alias fallback is permitted for this exact selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessModelSelection {
    pub model_id: String,
}

/// Correlated native control acknowledgment is not an effective-model observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessModelEvidence {
    pub requested_model_id: String,
    /// Present only when the native protocol explicitly reports the effective selection.
    /// It may be a concrete version of the requested alias, but only with plugin-validated
    /// native mapping evidence correlated to that exact selection; never guessed substitution.
    pub effective_model_id: Option<String>,
}

/// Separate domain request avoids mandatory fields on existing Open literals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", content = "request", rename_all = "snake_case")]
pub enum HarnessModelsRequestKind {
    DiscoverModels,
    OpenSelected {
        open: Box<HarnessOpenRequest>,
        selection: HarnessModelSelection,
    },
}
impl HarnessModelsRequestKind {
    /// Stable extension operation name.
    #[must_use]
    pub const fn operation(&self) -> &'static str {
        match self {
            Self::DiscoverModels => "discover_models",
            Self::OpenSelected { .. } => "open_selected",
        }
    }
}

/// Instance-scoped extension envelope; identity is not admission authority.
#[derive(Debug, Serialize, Deserialize)]
pub struct HarnessModelsRequest {
    pub schema_version: u16,
    pub request_id: String,
    pub instance: HarnessInstance,
    pub kind: HarnessModelsRequestKind,
}
impl HarnessModelsRequest {
    /// Validate before launching or mutating a native process.
    /// # Errors
    /// Rejects unknown versions, invalid identity, shape or excessive payloads.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        if self.schema_version != HARNESS_SCHEMA_VERSION {
            return Err(HarnessContractError::UnsupportedVersion);
        }
        id(&self.request_id)?;
        self.instance.validate()?;
        if let HarnessModelsRequestKind::OpenSelected { open, selection } = &self.kind {
            crate::service::validate_open_request(open, &self.instance)?;
            id(&selection.model_id)?;
        }
        wire(self, MAX_HARNESS_WIRE_BYTES)
    }
}

/// Explicitly bounded discovery or ordinary prepared/confirmed open with selection evidence.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "response", rename_all = "snake_case")]
pub enum HarnessModelsResponseKind {
    Models {
        native_version: String,
        models: Vec<HarnessModel>,
    },
    Selected {
        target: HarnessSessionTarget,
        evidence: HarnessModelEvidence,
    },
    Failure(HarnessFailure),
}
/// Exact request correlation must be checked before using discovery or launch evidence.
#[derive(Debug, Serialize, Deserialize)]
pub struct HarnessModelsResponse {
    pub schema_version: u16,
    pub request_id: String,
    pub instance_id: String,
    pub kind: HarnessModelsResponseKind,
}
impl HarnessModelsResponse {
    /// Validate shape, bounds and exact request ownership/selection correlation.
    /// # Errors
    /// Rejects incompatible versions, mismatches, malformed metadata and oversized responses.
    pub fn validate_for(&self, request: &HarnessModelsRequest) -> Result<(), HarnessContractError> {
        request.validate()?;
        if self.schema_version != HARNESS_SCHEMA_VERSION {
            return Err(HarnessContractError::UnsupportedVersion);
        }
        if self.request_id != request.request_id || self.instance_id != request.instance.instance_id
        {
            return Err(HarnessContractError::WrongOwner);
        }
        match (&request.kind, &self.kind) {
            (_, HarnessModelsResponseKind::Failure(_)) => {}
            (
                HarnessModelsRequestKind::DiscoverModels,
                HarnessModelsResponseKind::Models {
                    native_version,
                    models,
                },
            ) => {
                id(native_version)?;
                if native_version.len() > MAX_HARNESS_ID_BYTES || models.len() > MAX_HARNESS_MODELS
                {
                    return Err(HarnessContractError::BoundsExceeded);
                }
                let mut unique = BTreeSet::new();
                for model in models {
                    model.validate()?;
                    if !unique.insert(&model.model_id) {
                        return Err(HarnessContractError::InvalidShape);
                    }
                }
            }
            (
                HarnessModelsRequestKind::OpenSelected { open, selection },
                HarnessModelsResponseKind::Selected { target, evidence },
            ) => {
                id(&evidence.requested_model_id)?;
                if evidence.requested_model_id != selection.model_id {
                    return Err(HarnessContractError::WrongOwner);
                }
                if let Some(effective) = &evidence.effective_model_id {
                    // Native alias mapping is validated by the owning plugin. Concrete
                    // identity need not equal the requested alias; correlation still must.
                    id(effective)?;
                }
                let legacy = HarnessRequest {
                    schema_version: request.schema_version,
                    request_id: request.request_id.clone(),
                    instance: request.instance.clone(),
                    kind: HarnessRequestKind::Open(open.as_ref().clone()),
                };
                let kind = match target {
                    HarnessSessionTarget::Prepared(value) => {
                        HarnessServiceResponseKind::Prepared(value.clone())
                    }
                    HarnessSessionTarget::Confirmed(value) => {
                        HarnessServiceResponseKind::Opened(value.clone())
                    }
                };
                HarnessServiceResponse {
                    schema_version: self.schema_version,
                    request_id: self.request_id.clone(),
                    operation: crate::HarnessOperation::Open,
                    kind,
                }
                .validate_for(&legacy)?;
            }
            _ => return Err(HarnessContractError::OperationMismatch),
        }
        wire(self, MAX_HARNESS_WIRE_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_metadata_rejects_invalid_identity_and_empty_name() {
        let mut model = HarnessModel {
            model_id: "native-model".into(),
            display_name: "Native model".into(),
            description: None,
        };
        assert!(model.validate().is_ok());
        model.model_id = " native-model".into();
        assert!(model.validate().is_err());
        model.model_id = "native-model".into();
        model.display_name.clear();
        assert!(model.validate().is_err());
    }

    fn discovery_request() -> HarnessModelsRequest {
        HarnessModelsRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: "discover-1".into(),
            instance: HarnessInstance {
                instance_id: "native".into(),
                plugin_id: "plugin".into(),
                executable: "/configured/native".into(),
                config_namespace: "/configured/account".into(),
                auth: crate::HarnessAuthBinding {
                    provider_id: "native".into(),
                    profile: "account".into(),
                    credential_namespace: "account".into(),
                    custody: crate::HarnessAuthCustody::NativeManaged,
                },
            },
            kind: HarnessModelsRequestKind::DiscoverModels,
        }
    }

    #[test]
    fn discovery_response_requires_exact_correlation_and_unique_models() {
        let request = discovery_request();
        let model = HarnessModel {
            model_id: "native-key".into(),
            display_name: "Native".into(),
            description: None,
        };
        let mut response = HarnessModelsResponse {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            instance_id: request.instance.instance_id.clone(),
            kind: HarnessModelsResponseKind::Models {
                native_version: "exact-version".into(),
                models: vec![model.clone()],
            },
        };
        assert!(response.validate_for(&request).is_ok());
        response.request_id = "another-request".into();
        assert!(response.validate_for(&request).is_err());
        response.request_id.clone_from(&request.request_id);
        response.kind = HarnessModelsResponseKind::Models {
            native_version: "exact-version".into(),
            models: vec![model.clone(), model],
        };
        assert!(response.validate_for(&request).is_err());
        response.schema_version += 1;
        assert!(response.validate_for(&request).is_err());
    }

    #[test]
    fn selected_open_preserves_prepared_identity_and_exact_model_evidence() {
        let mut request = discovery_request();
        request.kind = HarnessModelsRequestKind::OpenSelected {
            open: Box::new(HarnessOpenRequest {
                open_id: "open-1".into(),
                session_id: "session-1".into(),
                generation: 7,
                working_directory: "/workspace".into(),
                resume: None,
                parent_assistance: None,
            }),
            selection: HarnessModelSelection {
                model_id: "native-key".into(),
            },
        };
        let mut response = HarnessModelsResponse {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            instance_id: request.instance.instance_id.clone(),
            kind: HarnessModelsResponseKind::Selected {
                target: HarnessSessionTarget::Prepared(crate::HarnessPreparedSession {
                    open_id: "open-1".into(),
                    session_id: "session-1".into(),
                    instance_id: request.instance.instance_id.clone(),
                    generation: 7,
                    native_version: "exact-version".into(),
                    requested_native_session_id: None,
                }),
                evidence: HarnessModelEvidence {
                    requested_model_id: "native-key".into(),
                    effective_model_id: None,
                },
            },
        };
        assert!(response.validate_for(&request).is_ok());
        if let HarnessModelsResponseKind::Selected { evidence, .. } = &mut response.kind {
            evidence.effective_model_id = Some("native-key-concrete-version".into());
        }
        assert!(response.validate_for(&request).is_ok());
        if let HarnessModelsResponseKind::Selected { target, evidence } = &mut response.kind {
            evidence.effective_model_id = None;
            if let HarnessSessionTarget::Prepared(prepared) = target {
                prepared.open_id = "other-open".into();
            }
        }
        assert!(response.validate_for(&request).is_err());
    }

    #[test]
    fn legacy_open_needs_no_model_field() {
        let open: HarnessOpenRequest = serde_json::from_value(serde_json::json!({
            "open_id":"open-1", "session_id":"session-1", "generation":1,
            "working_directory":"/workspace", "resume":null
        }))
        .unwrap();
        assert_eq!(open.open_id, "open-1");
        assert!(open.parent_assistance.is_none());
    }

    #[test]
    fn unknown_extension_operation_is_not_legacy_open() {
        assert!(
            serde_json::from_str::<HarnessModelsRequestKind>(r#"{"operation":"open"}"#).is_err()
        );
        assert!(
            serde_json::from_str::<HarnessModelsRequestKind>(r#"{"operation":"discover_models"}"#)
                .is_ok()
        );
    }
}
