//! Composed creation and initial admission of an ordinary canonical child session.
//!
//! The retained child identity identifies the entire immutable request, not only creation.
//! Retrying does not authorize a different input or execution selection. Supervision uses
//! ordinary session observation, interaction, messaging, and cancellation operations.

use crate::{SessionChildCreateRequest, SessionSummary, TurnAdmission, TurnAdmissionMetadata};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Immutable composed request retained in the child's canonical creation history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSpawnRequest {
    /// Composed compatibility boundary: v1 provider execution; v2 exact managed intent.
    pub version: u32,
    /// Caller-retained child identity, parent, initial title, and authorized working directory.
    pub creation: SessionChildCreateRequest,
    /// Optional existing worktree directory. Spawning neither creates nor owns its cleanup.
    /// When supplied, it must resolve to the creation working directory.
    pub worktree_directory: Option<PathBuf>,
    /// Initial ordinary turn input. No parent conversation is implicitly copied.
    pub input: String,
    /// Ordinary admission and immutable execution options, including agent/model/reasoning.
    /// Missing producer/key are filled deterministically before canonical creation.
    pub admission: TurnAdmissionMetadata,
    /// Optional configured authentication profile reference, never credentials.
    /// Unsupported or unavailable selections fail before creation or execution.
    pub auth_profile: Option<String>,
    /// Version-two managed execution intent. Version one must leave this absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed: Option<ManagedSessionSpawnIntent>,
}

/// Exact managed selection, retained as part of canonical child creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSessionSpawnIntent {
    /// Exact child `OpenSelected` envelope; no native default or provider fallback.
    pub selected_open: crate::external_harness::ExternalHarnessSelectedOpen,
    /// Explicit selection or exact parent selection snapshot requiring host reauthorization.
    pub source: ManagedSessionSpawnSource,
}

/// Inheritance records intent, not authority to control the parent or its native transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedSessionSpawnSource {
    Explicit,
    Inherited {
        owner: crate::external_harness::ExternalHarnessOwner,
        selected_open: Box<crate::external_harness::ExternalHarnessSelectedOpen>,
    },
}

impl SessionSpawnRequest {
    /// Validate canonical publication under the selected-open allocation.
    /// # Errors
    /// Rejects malformed execution intent or managed intent before schema57/writer25.
    pub fn validate_storage(&self, schema_version: u16, writer_epoch: u32) -> Result<(), String> {
        self.validate_execution()?;
        if self.managed.is_some()
            && (schema_version < crate::external_harness::SELECTED_OPEN_EVENT_SCHEMA_VERSION
                || writer_epoch < crate::external_harness::SELECTED_OPEN_STORAGE_WRITER_EPOCH)
        {
            return Err("managed spawn storage fence is unavailable".into());
        }
        Ok(())
    }

    /// Validate the independently versioned provider/managed spawn shape.
    /// # Errors
    /// Rejects unknown versions, backend mixing and changed inherited model/account intent.
    pub fn validate_execution(&self) -> Result<(), String> {
        match (self.version, &self.managed) {
            (1, None) => Ok(()),
            (2, Some(intent)) => {
                use crate::external_harness::ExternalHarnessRequest;
                let request =
                    ExternalHarnessRequest::from_models_request(&intent.selected_open.request)?;
                if intent.selected_open.version != 1
                    || self.creation.child_id == self.creation.parent_id
                    || request.target().0 != self.creation.child_id.to_string()
                    || request.target().1 != 1
                    || self.admission.execution.provider_plugin_id.is_some()
                    || self.admission.execution.model_id.is_some()
                    || self.admission.execution.request_context_id.is_some()
                    || self.auth_profile.is_some()
                    || request.open().is_none_or(|open| {
                        std::path::Path::new(&open.working_directory)
                            != self.creation.working_directory
                    })
                {
                    return Err("invalid managed spawn intent or mixed provider selection".into());
                }
                if let ManagedSessionSpawnSource::Inherited {
                    owner,
                    selected_open,
                } = &intent.source
                {
                    owner.validate()?;
                    let parent =
                        ExternalHarnessRequest::from_models_request(&selected_open.request)?;
                    if selected_open.version != 1
                        || owner.session_id != self.creation.parent_id
                        || parent.target()
                            != (owner.session_id.to_string().as_str(), owner.generation)
                        || parent.instance != request.instance
                    {
                        return Err("managed spawn inheritance owner/account mismatch".into());
                    }
                    let bcode_harness_models::HarnessModelsRequestKind::OpenSelected {
                        selection: parent_selection,
                        ..
                    } = &selected_open.request.kind
                    else {
                        return Err("invalid inherited selection".into());
                    };
                    let bcode_harness_models::HarnessModelsRequestKind::OpenSelected {
                        selection,
                        ..
                    } = &intent.selected_open.request.kind
                    else {
                        return Err("invalid child selection".into());
                    };
                    if selection != parent_selection {
                        return Err("managed spawn inherited model changed".into());
                    }
                }
                Ok(())
            }
            _ => Err("unsupported spawn execution version".into()),
        }
    }
}

/// Canonical Open result locators, verified by the host against ordinary session evidence.
/// Constructing this value alone grants no authority and proves no native execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSessionSpawnOpenReceipt {
    pub owner: crate::external_harness::ExternalHarnessOwner,
    pub request: crate::external_harness::ExternalHarnessRequest,
    pub response: crate::external_harness::ExternalHarnessResponse,
    pub configured_event_sequence: u64,
    pub claimed_event_sequence: u64,
    pub completed_event_sequence: u64,
}

/// Version-two composed outcome retaining native preparation/confirmation separately from admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSessionSpawnResult {
    pub version: u32,
    pub outcome: SessionSpawnResult,
    pub open: Option<ManagedSessionSpawnOpenReceipt>,
}
impl ManagedSessionSpawnResult {
    /// Validate correlation and stage/receipt consistency; canonical verification remains required.
    /// # Errors
    /// Rejects unsupported versions, missing/mismatched Open evidence and foreign children.
    pub fn validate_for(&self, request: &SessionSpawnRequest) -> Result<(), String> {
        request.validate_execution()?;
        self.outcome.validate().map_err(str::to_owned)?;
        let intent = request
            .managed
            .as_ref()
            .ok_or("managed spawn intent missing")?;
        if self.version != 2 || request.version != 2 {
            return Err("unsupported managed spawn result".into());
        }
        if let Some(session) = &self.outcome.session
            && (session.id != request.creation.child_id
                || session.parent_id != Some(request.creation.parent_id))
        {
            return Err("managed spawn child receipt mismatch".into());
        }
        if self
            .outcome
            .session
            .as_ref()
            .is_some_and(|session| session.working_directory != request.creation.working_directory)
        {
            return Err("managed spawn workspace receipt mismatch".into());
        }
        if let Some(receipt) = &self.open {
            receipt.owner.validate()?;
            let exact = crate::external_harness::ExternalHarnessRequest::from_models_request(
                &intent.selected_open.request,
            )?;
            if self.outcome.session.is_none()
                || receipt.request != exact
                || receipt.owner.session_id != request.creation.child_id
                || receipt.request.target()
                    != (
                        receipt.owner.session_id.to_string().as_str(),
                        receipt.owner.generation,
                    )
                || receipt.configured_event_sequence >= receipt.claimed_event_sequence
                || receipt.claimed_event_sequence >= receipt.completed_event_sequence
            {
                return Err("invalid canonical managed spawn Open receipt".into());
            }
            receipt.response.validate_for(&receipt.request)?;
            if !matches!(
                receipt.response.kind,
                crate::external_harness::ExternalHarnessResponseKind::Selected { .. }
            ) && (self.outcome.failure.is_none() || self.outcome.admission.is_some())
            {
                return Err("failed Open cannot authorize admission".into());
            }
        } else if self.outcome.failure.is_none() || self.outcome.admission.is_some() {
            return Err("managed spawn requires canonical Open receipt".into());
        }
        Ok(())
    }
}

/// Stage at which a composed operation could not continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSpawnStage {
    /// Structural validation or applicable effect authorization did not complete.
    Preparation,
    /// Canonical creation or publication did not complete or could not be verified.
    Creation,
    /// Ordinary turn admission or queue dispatch did not complete.
    Admission,
}

/// Normalized failure with no private provider, storage, or credential details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSpawnFailure {
    /// Failed composed stage, not a claim that earlier effects were rolled back.
    pub stage: SessionSpawnStage,
    /// Extensible normalized failure code; preserve unknown values.
    pub code: String,
    /// Secret-safe actionable diagnostic.
    pub message: String,
}

/// Inspectable composed outcome, including receipts available after partial failure.
///
/// Creation is not rolled back when admission fails. A missing receipt means it was not
/// verified, not that no effect occurred. An admission receipt does not establish execution
/// or completion. Retry only the identical request with the same retained child identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSpawnResult {
    /// Verified canonical session information, including after a later failure.
    pub session: Option<SessionSummary>,
    /// Exact ordinary admission variant, preserving turn/work/event-sequence identities.
    pub admission: Option<TurnAdmission>,
    /// Stage failure, possibly alongside already durable creation or admission receipts.
    pub failure: Option<SessionSpawnFailure>,
}

impl SessionSpawnResult {
    /// Verify structural receipt consistency without interpreting unknown failure codes.
    ///
    /// # Errors
    /// Rejects admission without creation, an empty outcome, or stage/receipt inconsistencies.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.admission.is_some() && self.session.is_none() {
            return Err("admission requires a verified session");
        }
        if let (Some(session), Some(admission)) = (&self.session, &self.admission) {
            let receipt = match admission {
                TurnAdmission::Accepted(receipt)
                | TurnAdmission::Existing(receipt)
                | TurnAdmission::Deferred(receipt) => Some(receipt),
                _ => None,
            };
            if receipt.is_some_and(|receipt| {
                *receipt
                    != crate::TurnReceipt::from_accepted_event(
                        session.id,
                        receipt.accepted_event_sequence,
                    )
            }) {
                return Err("admission receipt does not belong to the verified canonical session");
            }
        }
        match &self.failure {
            None if self.session.is_none() || self.admission.is_none() => {
                Err("successful spawning requires creation and admission receipts")
            }
            Some(failure) if failure.code.is_empty() || failure.message.is_empty() => {
                Err("failure requires a normalized code and message")
            }
            Some(failure)
                if failure.stage != SessionSpawnStage::Admission && self.admission.is_some() =>
            {
                Err("admission receipt is inconsistent with failure stage")
            }
            Some(failure)
                if failure.stage == SessionSpawnStage::Preparation && self.session.is_some() =>
            {
                Err("preparation failure cannot claim a newly created session")
            }
            Some(failure)
                if failure.stage == SessionSpawnStage::Admission && self.session.is_none() =>
            {
                Err("admission failure requires a verified session")
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn managed_request() -> SessionSpawnRequest {
        let child = crate::SessionId::new();
        let parent = crate::SessionId::new();
        let instance = bcode_harness_models::HarnessInstance {
            instance_id: "instance".into(),
            plugin_id: "plugin".into(),
            executable: "/native".into(),
            config_namespace: "namespace".into(),
            auth: bcode_harness_models::HarnessAuthBinding {
                provider_id: "provider".into(),
                profile: "native".into(),
                credential_namespace: "account".into(),
                custody: bcode_harness_models::HarnessAuthCustody::NativeManaged,
            },
        };
        let selected = crate::external_harness::ExternalHarnessSelectedOpen {
            version: 1,
            request: bcode_harness_models::HarnessModelsRequest {
                schema_version: 1,
                request_id: "child-open".into(),
                instance,
                kind: bcode_harness_models::HarnessModelsRequestKind::OpenSelected {
                    open: Box::new(bcode_harness_models::HarnessOpenRequest {
                        open_id: "child-open".into(),
                        session_id: child.to_string(),
                        generation: 1,
                        working_directory: "/checkout".into(),
                        resume: None,
                        parent_assistance: None,
                    }),
                    selection: bcode_harness_models::HarnessModelSelection {
                        model_id: "discovered-model".into(),
                    },
                },
            },
        };
        SessionSpawnRequest {
            version: 2,
            creation: SessionChildCreateRequest {
                child_id: child,
                parent_id: parent,
                name: None,
                working_directory: "/checkout".into(),
            },
            worktree_directory: None,
            input: "input".into(),
            admission: TurnAdmissionMetadata::default(),
            auth_profile: None,
            managed: Some(ManagedSessionSpawnIntent {
                selected_open: selected,
                source: ManagedSessionSpawnSource::Explicit,
            }),
        }
    }

    #[test]
    fn managed_spawn_fences_versions_and_rejects_provider_fallback() {
        let request = managed_request();
        request.validate_storage(57, 25).unwrap();
        request.validate_storage(58, 26).unwrap();
        assert!(request.validate_storage(55, 23).is_err());
        assert!(request.validate_storage(57, 24).is_err());
        let mut changed = request.clone();
        changed.version = 1;
        assert!(changed.validate_execution().is_err());
        let mut changed = request.clone();
        changed.admission.execution.provider_plugin_id = Some("unrelated-provider".into());
        assert!(changed.validate_execution().is_err());
        let mut changed = request.clone();
        changed.auth_profile = Some("unrelated-account".into());
        assert!(changed.validate_execution().is_err());
        let result = ManagedSessionSpawnResult {
            version: 2,
            outcome: SessionSpawnResult {
                session: None,
                admission: None,
                failure: None,
            },
            open: None,
        };
        assert!(result.validate_for(&request).is_err());
    }

    #[test]
    fn managed_spawn_inheritance_retains_exact_account_and_model() {
        let mut request = managed_request();
        let mut parent = request.managed.as_ref().unwrap().selected_open.clone();
        parent.request.request_id = "parent-open".into();
        if let bcode_harness_models::HarnessModelsRequestKind::OpenSelected { open, .. } =
            &mut parent.request.kind
        {
            open.open_id = "parent-open".into();
            open.session_id = request.creation.parent_id.to_string();
            open.generation = 3;
        }
        let owner = crate::external_harness::ExternalHarnessOwner {
            session_id: request.creation.parent_id,
            generation: 3,
            artifact_id: "artifact".into(),
            daemon_id: "daemon".into(),
        };
        request.managed.as_mut().unwrap().source = ManagedSessionSpawnSource::Inherited {
            owner,
            selected_open: Box::new(parent),
        };
        request.validate_execution().unwrap();
        let mut changed = request.clone();
        let ManagedSessionSpawnSource::Inherited { selected_open, .. } =
            &mut changed.managed.as_mut().unwrap().source
        else {
            unreachable!()
        };
        selected_open.request.instance.auth.credential_namespace = "other-account".into();
        assert!(changed.validate_execution().is_err());
        let mut changed = request.clone();
        if let bcode_harness_models::HarnessModelsRequestKind::OpenSelected { selection, .. } =
            &mut changed.managed.as_mut().unwrap().selected_open.request.kind
        {
            selection.model_id = "other-model".into();
        }
        assert!(changed.validate_execution().is_err());
        assert_eq!(
            serde_json::from_str::<SessionSpawnRequest>(&serde_json::to_string(&request).unwrap())
                .unwrap(),
            request
        );
    }

    #[test]
    fn empty_and_receiptless_admission_failures_are_not_successful_outcomes() {
        let mut result = SessionSpawnResult {
            session: None,
            admission: None,
            failure: None,
        };
        assert!(result.validate().is_err());
        result.failure = Some(SessionSpawnFailure {
            stage: SessionSpawnStage::Preparation,
            code: "denied".into(),
            message: "request was not authorized".into(),
        });
        assert!(result.validate().is_ok());
        result.failure.as_mut().unwrap().stage = SessionSpawnStage::Admission;
        assert!(result.validate().is_err());
        result.failure.as_mut().unwrap().stage = SessionSpawnStage::Creation;
        assert!(result.validate().is_ok());
        result.admission = Some(TurnAdmission::Rejected(
            crate::TurnRejectionReason::ExecutionPolicy,
        ));
        assert!(result.validate().is_err());
    }

    #[test]
    fn composed_request_rejects_unknown_fields_and_preserves_auth_references() {
        let request = SessionSpawnRequest {
            version: 1,
            creation: SessionChildCreateRequest {
                child_id: crate::SessionId::new(),
                parent_id: crate::SessionId::new(),
                name: None,
                working_directory: PathBuf::from("/checkout"),
            },
            worktree_directory: None,
            input: "input".into(),
            admission: TurnAdmissionMetadata::default(),
            auth_profile: Some("configured-profile".into()),
            managed: None,
        };
        let mut value = serde_json::to_value(&request).unwrap();
        assert!(
            value.get("managed").is_none(),
            "v1 serialization remains unchanged"
        );
        request.validate_execution().unwrap();
        assert_eq!(
            serde_json::from_value::<SessionSpawnRequest>(value.clone()).unwrap(),
            request
        );
        value
            .as_object_mut()
            .unwrap()
            .insert("unknown".into(), serde_json::Value::Bool(true));
        assert!(serde_json::from_value::<SessionSpawnRequest>(value).is_err());
    }
}
