//! Model-facing delegation shapes over the same managed-session application boundary.
//!
//! A model supplies only a prompt (and optionally an instance reference or an existing child
//! session ID). The plugin resolves the configured instance, reads the child's exact retained
//! routing target through the host's delegated binding read, and derives every mutation
//! identity deterministically from the authenticated invocation and its normalized input.
//! Repeating the same invocation therefore reuses the same retained identities, so a lost
//! acknowledgement is reconciled by the host's canonical claims instead of replayed. A new
//! invocation never re-sends an uncertain mutation on its own; it reports the uncertainty.
//!
//! The host still admits every operation: delegated control is limited to the active tool
//! invocation of the child's immutable canonical parent, each mutation passes tool permission
//! policy, and native tool observations never invoke Bcode tools.

use std::collections::BTreeMap;

use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessControlRequest, HarnessExecutionEvent, HarnessInstance,
    HarnessInteraction, HarnessInteractionKind, HarnessObservation, HarnessObservationKind,
    HarnessObservationPage, HarnessOpenRequest, HarnessPlacement, HarnessRequest,
    HarnessRequestKind, HarnessResponse, HarnessResponseValue, HarnessServiceResponse,
    HarnessServiceResponseKind, HarnessSessionTarget, HarnessSubmission,
    HarnessTerminalDisposition, MANAGED_DELEGATION_BINDING_OPERATION,
    MANAGED_DELEGATION_HISTORY_OPERATION, MAX_HARNESS_ID_BYTES, MAX_HARNESS_OBSERVATIONS,
    MAX_HARNESS_TEXT_BYTES, ManagedDelegationBinding, ManagedDelegationBindingRequest,
    ManagedDelegationBindingResponse, ManagedDelegationHistoryRequest,
};
use bcode_session_models::SessionId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest as _;

/// Start a delegated child session and submit its first turn.
pub const DELEGATE_TOOL: &str = "harness.delegate";
/// Submit the next turn to an existing delegated child session.
pub const CONTINUE_TOOL: &str = "harness.continue";
/// Summarize a delegated child session's activity, latest result and pending interactions.
pub const RESULT_TOOL: &str = "harness.result";

/// Model-facing tools that accept only the simplified shape.
pub const SIMPLE_TOOLS: [&str; 3] = [DELEGATE_TOOL, CONTINUE_TOOL, RESULT_TOOL];

const MAX_LISTED_INSTANCES: usize = 16;
const HISTORY_PAGE: u16 = 64;
const MAX_HISTORY_PAGES: usize = 16;
const RESULT_PREVIEW_BYTES: usize = 4096;
const INTERACTION_PREVIEW_BYTES: usize = 512;
const MAX_PENDING: usize = 8;
const MAX_CHOICES: usize = 8;

/// Normalized simplified invocation. Unknown fields are rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum SimpleInvocation {
    /// `harness.delegate`.
    Delegate(DelegateInput),
    /// `harness.continue`.
    Continue(ContinueInput),
    /// `harness.result`.
    Result(ResultInput),
    /// Simplified `harness.respond`.
    Respond(RespondInput),
    /// Simplified `harness.interrupt`.
    Interrupt(InterruptInput),
}

/// Delegate a self-contained task to a configured external coding agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegateInput {
    /// Configured instance reference; the configured default when absent.
    #[serde(default)]
    pub instance_id: Option<String>,
    /// First turn submitted to the new child session.
    pub prompt: String,
}

/// Submit the next turn to an existing delegated child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinueInput {
    /// Delegated child session returned by `harness.delegate`.
    pub session_id: String,
    /// Next turn text.
    pub prompt: String,
}

/// Summarize a delegated child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultInput {
    /// Delegated child session returned by `harness.delegate`.
    pub session_id: String,
    /// Cursor returned by a previous `harness.result`; summarize only newer history.
    #[serde(default)]
    pub after: Option<String>,
}

/// Answer one pending child interaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RespondInput {
    /// Delegated child session.
    pub session_id: String,
    /// Pending interaction identity from `harness.result`.
    pub request_id: String,
    /// Approval decision for an approval request.
    #[serde(default)]
    pub approve: Option<bool>,
    /// Answer text for a question.
    #[serde(default)]
    pub answer: Option<String>,
    /// Optional choice identity for a question answer.
    #[serde(default)]
    pub choice_id: Option<String>,
}

/// Request interruption of a delegated child's active native turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterruptInput {
    /// Delegated child session.
    pub session_id: String,
}

impl SimpleInvocation {
    /// Parse a simplified invocation for `tool_name`.
    ///
    /// Returns `Ok(None)` for advanced `request_json` invocations of respond/interrupt.
    ///
    /// # Errors
    ///
    /// * Rejects unknown tools, unknown fields and malformed or oversized inputs.
    /// * Rejects respond inputs that do not choose exactly one of `approve` or `answer`.
    pub fn parse(tool_name: &str, arguments: &Value) -> Result<Option<Self>, String> {
        let invalid = |_| format!("invalid {tool_name} arguments");
        let invocation = match tool_name {
            DELEGATE_TOOL => {
                Self::Delegate(serde_json::from_value(arguments.clone()).map_err(invalid)?)
            }
            CONTINUE_TOOL => {
                Self::Continue(serde_json::from_value(arguments.clone()).map_err(invalid)?)
            }
            RESULT_TOOL => {
                Self::Result(serde_json::from_value(arguments.clone()).map_err(invalid)?)
            }
            "harness.respond" | "harness.interrupt" if arguments.get("request_json").is_some() => {
                return Ok(None);
            }
            "harness.respond" => {
                Self::Respond(serde_json::from_value(arguments.clone()).map_err(invalid)?)
            }
            "harness.interrupt" => {
                Self::Interrupt(serde_json::from_value(arguments.clone()).map_err(invalid)?)
            }
            _ => return Err("unknown harness tool".into()),
        };
        invocation.validate()?;
        Ok(Some(invocation))
    }

    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Delegate(input) => {
                if let Some(instance) = &input.instance_id {
                    identity(instance, "instance_id")?;
                }
                prompt(&input.prompt)
            }
            Self::Continue(input) => {
                session(&input.session_id)?;
                prompt(&input.prompt)
            }
            Self::Result(input) => {
                session(&input.session_id)?;
                input
                    .after
                    .as_deref()
                    .map_or(Ok(()), |cursor| identity(cursor, "after"))
            }
            Self::Respond(input) => {
                session(&input.session_id)?;
                identity(&input.request_id, "request_id")?;
                match (&input.approve, &input.answer, &input.choice_id) {
                    (Some(_), None, None) => Ok(()),
                    (None, Some(answer), choice) => {
                        if answer.len() > MAX_HARNESS_TEXT_BYTES {
                            return Err("answer exceeds bound".into());
                        }
                        choice
                            .as_deref()
                            .map_or(Ok(()), |choice| identity(choice, "choice_id"))
                    }
                    _ => Err("supply exactly one of approve or answer".into()),
                }
            }
            Self::Interrupt(input) => session(&input.session_id),
        }
    }

    /// Managed-session application operations this invocation may request.
    #[must_use]
    pub const fn required_operations(&self) -> &'static [&'static str] {
        match self {
            Self::Delegate(_) => &[MANAGED_DELEGATION_BINDING_OPERATION, "start", "followup"],
            Self::Continue(_) => &[
                MANAGED_DELEGATION_BINDING_OPERATION,
                MANAGED_DELEGATION_HISTORY_OPERATION,
                "followup",
            ],
            Self::Result(_) => &[MANAGED_DELEGATION_HISTORY_OPERATION],
            Self::Respond(_) => &[MANAGED_DELEGATION_BINDING_OPERATION, "respond"],
            Self::Interrupt(_) => &[MANAGED_DELEGATION_BINDING_OPERATION, "interrupt"],
        }
    }

    /// Whether this invocation only reads (and drains observations); all others mutate.
    #[must_use]
    pub const fn read_only(&self) -> bool {
        matches!(self, Self::Result(_))
    }
}

fn identity(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_HARNESS_ID_BYTES
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(format!("invalid {field}"));
    }
    Ok(())
}

fn session(value: &str) -> Result<(), String> {
    value
        .parse::<SessionId>()
        .map(drop)
        .map_err(|_| "session_id must be a delegated child session ID".to_owned())
}

fn prompt(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("prompt must not be empty".into());
    }
    if value.len() > MAX_HARNESS_TEXT_BYTES {
        return Err("prompt exceeds bound".into());
    }
    Ok(())
}

/// Host-issued invocation context captured during side-effect-free preparation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvocationScope {
    /// Calling (parent) session; host admission independently authenticates it.
    pub parent_session_id: String,
    /// Host-selected canonical workspace of the calling session.
    pub working_directory: String,
}

impl InvocationScope {
    /// Read the host workspace context entry supplied to tool preparation.
    ///
    /// # Errors
    ///
    /// * Rejects missing, duplicated or unsupported workspace context entries.
    /// * Rejects contexts without a session identity or an absolute working directory.
    pub fn from_host_context(entries: &[bcode_tool::ToolHostContextEntry]) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Workspace {
            working_directory: String,
            workspace_id: Option<String>,
        }
        let mut found = entries
            .iter()
            .filter(|entry| entry.schema == bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA);
        let entry = found
            .next()
            .ok_or("delegation requires a session workspace")?;
        if found.next().is_some()
            || entry.schema_version != bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION
        {
            return Err("unsupported session workspace context".into());
        }
        let workspace: Workspace = serde_json::from_value(entry.payload.clone())
            .map_err(|_| "invalid session workspace context")?;
        let parent = workspace
            .workspace_id
            .filter(|id| id.parse::<SessionId>().is_ok())
            .ok_or("delegation requires a calling session")?;
        if !std::path::Path::new(&workspace.working_directory).is_absolute() {
            return Err("delegation requires an absolute session workspace".into());
        }
        Ok(Self {
            parent_session_id: parent,
            working_directory: workspace.working_directory,
        })
    }
}

/// Failure of one managed application call. Codes are fixed host strings, never native text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallFailure {
    /// The host refused or could not route the call.
    Host(String),
    /// The application answered with a typed harness failure.
    Harness(bcode_harness_models::HarnessFailure),
    /// The bridge failed, timed out or was cancelled; outcome is unknown.
    Unknown,
    /// The response did not satisfy the expected correlation or bounds.
    Invalid,
}

impl CallFailure {
    fn label(&self) -> String {
        match self {
            Self::Host(code) => code.clone(),
            Self::Harness(failure) => serde_json::to_value(failure)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "harness_failure".into()),
            Self::Unknown => "outcome_unknown".into(),
            Self::Invalid => "invalid_response".into(),
        }
    }

    /// Whether a mutation might have been delivered despite this failure.
    fn uncertain(&self) -> bool {
        match self {
            Self::Unknown | Self::Invalid => true,
            Self::Harness(failure) => *failure == bcode_harness_models::HarnessFailure::Uncertain,
            Self::Host(code) => matches!(
                code.as_str(),
                "managed_session_uncertain" | "managed_session_deadline"
            ),
        }
    }
}

/// Bounded request/reply access to the managed-session application route.
pub trait ManagedApplication {
    /// Send one managed-session application request and return its `Responded` payload.
    ///
    /// # Errors
    ///
    /// Returns a [`CallFailure`] for host refusal, bridge loss or malformed responses.
    fn call(&self, operation: &str, payload: Value) -> Result<Value, CallFailure>;
}

/// Model-facing tool outcome: bounded JSON text and an error flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Bounded JSON presented to the model.
    pub output: Value,
    /// Whether the model should treat this as a failed tool call.
    pub is_error: bool,
}

impl Outcome {
    const fn ok(output: Value) -> Self {
        Self {
            output,
            is_error: false,
        }
    }

    const fn error(output: Value) -> Self {
        Self {
            output,
            is_error: true,
        }
    }
}

/// Deterministic identity derivation over length-prefixed parts.
fn digest(parts: &[&str]) -> [u8; 32] {
    let mut hash = sha2::Sha256::new();
    hash.update(b"bcode.harness.delegation/v1");
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    hash.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

/// UUID-shaped (version 8, RFC 4122 variant) identity derived from `parts`.
fn derived_uuid(parts: &[&str]) -> uuid::Uuid {
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest(parts)[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes)
}

fn derived_id(kind: &str, parts: &[&str]) -> String {
    format!("{kind}-{}", hex(&digest(parts)[..16]))
}

/// Retained identities for one delegation invocation.
#[allow(clippy::struct_field_names)] // Each field is a distinct protocol identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationIdentities {
    /// Child Bcode session identity.
    pub session_id: SessionId,
    /// Retained open mutation identity.
    pub open_id: String,
    /// Retained first submission identity.
    pub submission_id: String,
    /// Retained first submission attempt identity.
    pub attempt_id: String,
}

impl DelegationIdentities {
    /// Derive identities from the authenticated invocation and its normalized input.
    #[must_use]
    pub fn derive(
        scope: &InvocationScope,
        invocation_id: &str,
        instance_id: &str,
        prompt: &str,
    ) -> Self {
        let parts = [
            "delegate",
            scope.parent_session_id.as_str(),
            invocation_id,
            instance_id,
            prompt,
        ];
        Self {
            session_id: SessionId(derived_uuid(&parts)),
            open_id: derived_id("delegate-open", &parts),
            submission_id: derived_id("delegate-submit", &parts),
            attempt_id: derived_uuid(&[&parts[..], &["attempt"]].concat()).to_string(),
        }
    }
}

/// Resolve `requested` (or the default) against the plugin's configured instances.
///
/// # Errors
///
/// Rejects unconfigured, disabled or unknown instances without inspecting native state.
pub fn resolve_instance(
    config: &Value,
    requested: Option<&str>,
) -> Result<HarnessInstance, String> {
    let catalog = crate::configuration::catalog_value(config)
        .map_err(|()| "no external coding agent instance is configured".to_owned())?;
    let reference = requested
        .or(catalog.default_instance.as_deref())
        .ok_or("no default external agent instance; pass instance_id")?;
    catalog
        .instances
        .get(reference)
        .cloned()
        .ok_or_else(|| format!("external agent instance {reference:?} is not configured"))
}

/// Short description suffix listing configured instances, or `None` when none are enabled.
#[must_use]
pub fn configured_instances_summary(config: &Value) -> Option<String> {
    let catalog = crate::configuration::catalog_value(config).ok()?;
    if catalog.instances.is_empty() {
        return None;
    }
    let mut names = catalog
        .instances
        .iter()
        .take(MAX_LISTED_INSTANCES)
        .map(|(reference, instance)| {
            let default = catalog.default_instance.as_deref() == Some(reference.as_str());
            format!(
                "{reference} ({}{})",
                instance.plugin_id,
                if default { ", default" } else { "" }
            )
        })
        .collect::<Vec<_>>();
    if catalog.instances.len() > MAX_LISTED_INSTANCES {
        names.push("...".into());
    }
    Some(names.join(", "))
}

fn managed_request(child: Option<&str>, parent: Option<&str>, request: &HarnessRequest) -> Value {
    json!({
        "session_id": child,
        "harness": request,
        "parent_session_id": parent,
        "assistance_destination_session_id": null,
    })
}

fn harness_call(
    app: &dyn ManagedApplication,
    operation: &str,
    payload: Value,
) -> Result<HarnessServiceResponse, CallFailure> {
    let request: HarnessRequest =
        serde_json::from_value(payload["harness"].clone()).map_err(|_| CallFailure::Invalid)?;
    let payload = crate::validated_payload(operation, payload).map_err(|_| CallFailure::Invalid)?;
    let response = app.call(operation, payload)?;
    crate::protocol::bounded_response(&response).map_err(|_| CallFailure::Invalid)?;
    let response: HarnessServiceResponse =
        serde_json::from_value(response).map_err(|_| CallFailure::Invalid)?;
    response
        .validate_for(&request)
        .map_err(|_| CallFailure::Invalid)?;
    if let HarnessServiceResponseKind::Failure(failure) = response.kind {
        return Err(CallFailure::Harness(failure));
    }
    Ok(response)
}

fn binding(
    app: &dyn ManagedApplication,
    session_id: &str,
) -> Result<Option<ManagedDelegationBinding>, CallFailure> {
    let request = ManagedDelegationBindingRequest {
        session_id: session_id.to_owned(),
    };
    let value = app.call(
        MANAGED_DELEGATION_BINDING_OPERATION,
        serde_json::to_value(&request).map_err(|_| CallFailure::Invalid)?,
    )?;
    crate::protocol::bounded_response(&value).map_err(|_| CallFailure::Invalid)?;
    let response: ManagedDelegationBindingResponse =
        serde_json::from_value(value).map_err(|_| CallFailure::Invalid)?;
    response
        .validate_for(&request)
        .map_err(|_| CallFailure::Invalid)?;
    Ok(response.binding)
}

fn history(
    app: &dyn ManagedApplication,
    session_id: &str,
    after: Option<String>,
    limit: u16,
) -> Result<HarnessObservationPage, CallFailure> {
    let request = ManagedDelegationHistoryRequest {
        session_id: session_id.to_owned(),
        after,
        limit,
    };
    request.validate().map_err(|_| CallFailure::Invalid)?;
    let value = app.call(
        MANAGED_DELEGATION_HISTORY_OPERATION,
        serde_json::to_value(&request).map_err(|_| CallFailure::Invalid)?,
    )?;
    crate::protocol::bounded_response(&value).map_err(|_| CallFailure::Invalid)?;
    let page: HarnessObservationPage =
        serde_json::from_value(value).map_err(|_| CallFailure::Invalid)?;
    request
        .validate_page(&page)
        .map_err(|_| CallFailure::Invalid)?;
    Ok(page)
}

fn failure_output(session_id: Option<&str>, stage: &str, failure: &CallFailure) -> Outcome {
    let hint = match failure {
        CallFailure::Harness(bcode_harness_models::HarnessFailure::Conflict) => {
            "The child may still be running or its state changed. Call harness.result and retry once it is settled."
        }
        CallFailure::Harness(bcode_harness_models::HarnessFailure::StaleGeneration) => {
            "The child moved to a newer generation. Call harness.result before retrying."
        }
        CallFailure::Host(code) if code.ends_with("stale_owner") => {
            "This session is not a delegated child of the current session, or its native transport is no longer owned by this daemon."
        }
        _ if failure.uncertain() => {
            "The outcome is unknown. Do not repeat this request with different input; call harness.result to check whether it took effect."
        }
        _ => "The request was not accepted.",
    };
    Outcome::error(json!({
        "session_id": session_id,
        "status": if failure.uncertain() { "uncertain" } else { "failed" },
        "stage": stage,
        "reason": failure.label(),
        "hint": hint,
    }))
}

/// Execute one simplified invocation against the managed application.
#[must_use]
pub fn execute(
    app: &dyn ManagedApplication,
    scope: &InvocationScope,
    invocation_id: &str,
    invocation: &SimpleInvocation,
    instance: Option<&HarnessInstance>,
) -> Outcome {
    match invocation {
        SimpleInvocation::Delegate(input) => instance.map_or_else(
            || Outcome::error(json!({"status":"failed","reason":"instance_unavailable"})),
            |instance| delegate(app, scope, invocation_id, input, instance),
        ),
        SimpleInvocation::Continue(input) => continue_child(app, scope, invocation_id, input),
        SimpleInvocation::Result(input) => result(app, input),
        SimpleInvocation::Respond(input) => respond(app, scope, invocation_id, input),
        SimpleInvocation::Interrupt(input) => interrupt(app, scope, invocation_id, input),
    }
}

fn delegate(
    app: &dyn ManagedApplication,
    scope: &InvocationScope,
    invocation_id: &str,
    input: &DelegateInput,
    instance: &HarnessInstance,
) -> Outcome {
    let ids =
        DelegationIdentities::derive(scope, invocation_id, &instance.instance_id, &input.prompt);
    let child = ids.session_id.to_string();
    // Reconcile an exact retry of this invocation from canonical state before any open.
    let target = match binding(app, &child) {
        Ok(Some(existing)) => {
            if existing.instance != *instance {
                return failure_output(
                    Some(&child),
                    "reconcile",
                    &CallFailure::Harness(bcode_harness_models::HarnessFailure::Conflict),
                );
            }
            match existing.target {
                // Confirmation only follows native receipt of the first user input: the first
                // turn was delivered. Never re-send it under a different target.
                HarnessSessionTarget::Confirmed(_) => {
                    return Outcome::ok(json!({
                        "session_id": child,
                        "instance_id": instance.instance_id,
                        "status": "already_submitted",
                        "submission_id": ids.submission_id,
                        "next": "Poll harness.result with this session_id.",
                    }));
                }
                prepared @ HarnessSessionTarget::Prepared(_) => prepared,
            }
        }
        Ok(None) => {
            let request = HarnessRequest {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: ids.open_id.clone(),
                instance: instance.clone(),
                kind: HarnessRequestKind::Open(HarnessOpenRequest {
                    open_id: ids.open_id.clone(),
                    session_id: child.clone(),
                    generation: 1,
                    working_directory: scope.working_directory.clone(),
                    resume: None,
                    parent_assistance: None,
                }),
            };
            let payload = managed_request(Some(&child), Some(&scope.parent_session_id), &request);
            match harness_call(app, "start", payload) {
                Ok(HarnessServiceResponse {
                    kind: HarnessServiceResponseKind::Prepared(prepared),
                    ..
                }) => HarnessSessionTarget::Prepared(prepared),
                Ok(HarnessServiceResponse {
                    kind: HarnessServiceResponseKind::Opened(binding),
                    ..
                }) => HarnessSessionTarget::Confirmed(binding),
                Ok(_) => return failure_output(Some(&child), "open", &CallFailure::Invalid),
                Err(failure) => return failure_output(Some(&child), "open", &failure),
            }
        }
        Err(failure) => return failure_output(Some(&child), "reconcile", &failure),
    };
    match submit(
        app,
        &child,
        target,
        instance,
        &ids.submission_id,
        &ids.attempt_id,
        &input.prompt,
    ) {
        Ok(()) => Outcome::ok(json!({
            "session_id": child,
            "instance_id": instance.instance_id,
            "status": "submitted",
            "submission_id": ids.submission_id,
            "next": "The child runs in the background. Poll harness.result with this session_id; answer pending approvals or questions with harness.respond; send follow-ups with harness.continue once it is settled.",
        })),
        Err(failure) => failure_output(Some(&child), "submit", &failure),
    }
}

fn submit(
    app: &dyn ManagedApplication,
    child: &str,
    target: HarnessSessionTarget,
    instance: &HarnessInstance,
    submission_id: &str,
    attempt_id: &str,
    text: &str,
) -> Result<(), CallFailure> {
    let generation = target.generation();
    let request = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: submission_id.to_owned(),
        instance: instance.clone(),
        kind: HarnessRequestKind::Submit {
            target,
            submission: HarnessSubmission {
                submission_id: submission_id.to_owned(),
                attempt_id: attempt_id.to_owned(),
                session_id: child.to_owned(),
                generation,
                text: text.to_owned(),
                placement: HarnessPlacement::NextTurn,
            },
        },
    };
    let payload = managed_request(Some(child), None, &request);
    match harness_call(app, "followup", payload)? {
        HarnessServiceResponse {
            kind: HarnessServiceResponseKind::Receipt(_),
            ..
        } => Ok(()),
        _ => Err(CallFailure::Invalid),
    }
}

fn continue_child(
    app: &dyn ManagedApplication,
    scope: &InvocationScope,
    invocation_id: &str,
    input: &ContinueInput,
) -> Outcome {
    let child = input.session_id.as_str();
    // Commit any pending native observations first so a just-confirmed binding is current.
    if let Err(failure) = history(app, child, None, 1) {
        return failure_output(Some(child), "observe", &failure);
    }
    let existing = match binding(app, child) {
        Ok(Some(existing)) => existing,
        Ok(None) => {
            return Outcome::error(json!({
                "session_id": child, "status": "failed", "reason": "unknown_session",
                "hint": "Use the session_id returned by harness.delegate.",
            }));
        }
        Err(failure) => return failure_output(Some(child), "binding", &failure),
    };
    let parts = [
        "continue",
        scope.parent_session_id.as_str(),
        invocation_id,
        child,
        input.prompt.as_str(),
    ];
    let submission_id = derived_id("delegate-continue", &parts);
    let attempt_id = derived_uuid(&[&parts[..], &["attempt"]].concat()).to_string();
    match submit(
        app,
        child,
        existing.target,
        &existing.instance,
        &submission_id,
        &attempt_id,
        &input.prompt,
    ) {
        Ok(()) => Outcome::ok(json!({
            "session_id": child,
            "status": "submitted",
            "submission_id": submission_id,
            "next": "Poll harness.result with this session_id.",
        })),
        Err(failure) => failure_output(Some(child), "submit", &failure),
    }
}

fn respond(
    app: &dyn ManagedApplication,
    scope: &InvocationScope,
    invocation_id: &str,
    input: &RespondInput,
) -> Outcome {
    let child = input.session_id.as_str();
    let existing = match binding(app, child) {
        Ok(Some(existing)) => existing,
        Ok(None) => {
            return Outcome::error(
                json!({"session_id": child, "status": "failed", "reason": "unknown_session"}),
            );
        }
        Err(failure) => return failure_output(Some(child), "binding", &failure),
    };
    let value = match (&input.approve, &input.answer) {
        (Some(approved), _) => HarnessResponseValue::Approval {
            approved: *approved,
        },
        (None, Some(text)) => HarnessResponseValue::Answer {
            choice_id: input.choice_id.clone(),
            text: text.clone(),
        },
        (None, None) => {
            return Outcome::error(json!({"status":"failed","reason":"invalid_request"}));
        }
    };
    let encoded = serde_json::to_string(&value).unwrap_or_default();
    let response_id = derived_id(
        "delegate-respond",
        &[
            "respond",
            scope.parent_session_id.as_str(),
            invocation_id,
            child,
            input.request_id.as_str(),
            encoded.as_str(),
        ],
    );
    let generation = existing.target.generation();
    let request = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: response_id.clone(),
        instance: existing.instance,
        kind: HarnessRequestKind::Respond {
            target: existing.target,
            response: HarnessResponse {
                response_id,
                request_id: input.request_id.clone(),
                session_id: child.to_owned(),
                generation,
                value,
            },
        },
    };
    let result = harness_call(app, "respond", managed_request(Some(child), None, &request));
    match result {
        Ok(_) => Outcome::ok(json!({
            "session_id": child,
            "request_id": input.request_id,
            "status": "acknowledged",
            "next": "Poll harness.result to follow the child.",
        })),
        Err(failure) => failure_output(Some(child), "respond", &failure),
    }
}

fn interrupt(
    app: &dyn ManagedApplication,
    scope: &InvocationScope,
    invocation_id: &str,
    input: &InterruptInput,
) -> Outcome {
    let child = input.session_id.as_str();
    let existing = match binding(app, child) {
        Ok(Some(existing)) => existing,
        Ok(None) => {
            return Outcome::error(
                json!({"session_id": child, "status": "failed", "reason": "unknown_session"}),
            );
        }
        Err(failure) => return failure_output(Some(child), "binding", &failure),
    };
    let mutation_id = derived_id(
        "delegate-interrupt",
        &[
            "interrupt",
            scope.parent_session_id.as_str(),
            invocation_id,
            child,
        ],
    );
    let request = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: mutation_id.clone(),
        instance: existing.instance,
        kind: HarnessRequestKind::Interrupt(HarnessControlRequest {
            target: existing.target,
            mutation_id,
        }),
    };
    let result = harness_call(
        app,
        "interrupt",
        managed_request(Some(child), None, &request),
    );
    match result {
        Ok(_) => Outcome::ok(json!({
            "session_id": child,
            "status": "interrupt_requested",
            "note": "Acknowledgement is not settlement; poll harness.result.",
        })),
        Err(failure) => failure_output(Some(child), "interrupt", &failure),
    }
}

fn result(app: &dyn ManagedApplication, input: &ResultInput) -> Outcome {
    let child = input.session_id.as_str();
    let mut summary = Summary::default();
    let mut cursor = input.after.clone();
    let mut more = false;
    for page_index in 0..MAX_HISTORY_PAGES {
        let page = match history(app, child, cursor.clone(), HISTORY_PAGE) {
            Ok(page) => page,
            Err(failure) => return failure_output(Some(child), "history", &failure),
        };
        // Canonical history pages end at committed batch boundaries (possibly empty batches)
        // before `limit`; only an unchanged cursor marks the current end of history.
        for observation in &page.observations {
            summary.observe(observation);
        }
        let advanced = page.next_after.is_some() && page.next_after != cursor;
        if page.next_after.is_some() {
            cursor.clone_from(&page.next_after);
        }
        if !advanced {
            break;
        }
        more = page_index + 1 == MAX_HISTORY_PAGES;
    }
    let mut output = summary.present();
    output["session_id"] = json!(child);
    output["cursor"] = json!(cursor);
    output["more"] = json!(more);
    Outcome::ok(output)
}

#[derive(Default)]
struct Summary {
    /// Ordered start/terminal facts by submission.
    last_lifecycle: Option<Lifecycle>,
    terminal_output: Option<String>,
    result_text: Option<String>,
    text_tail: String,
    pending: BTreeMap<String, (usize, HarnessInteraction)>,
    order: usize,
    status: Option<bcode_harness_models::HarnessExecutionStatus>,
    observations: usize,
}

#[derive(Clone, Copy)]
enum Lifecycle {
    Started,
    Terminal(HarnessTerminalDisposition),
}

impl Summary {
    fn observe(&mut self, observation: &HarnessObservation) {
        self.order += 1;
        self.observations += 1;
        let progressed = match &observation.kind {
            HarnessObservationKind::SubmissionAccepted { .. }
            | HarnessObservationKind::SubmissionConsumed { .. } => {
                if !matches!(self.last_lifecycle, Some(Lifecycle::Started)) {
                    self.text_tail.clear();
                    self.result_text = None;
                    self.terminal_output = None;
                }
                self.last_lifecycle = Some(Lifecycle::Started);
                false
            }
            HarnessObservationKind::Execution {
                event:
                    HarnessExecutionEvent::SubmissionTerminal {
                        disposition,
                        output,
                        ..
                    },
            } => {
                self.last_lifecycle = Some(Lifecycle::Terminal(*disposition));
                self.terminal_output.clone_from(output);
                true
            }
            HarnessObservationKind::Text { text, .. } => {
                append_tail(&mut self.text_tail, text);
                true
            }
            HarnessObservationKind::Result { text } => {
                self.result_text = Some(text.clone());
                true
            }
            HarnessObservationKind::Interaction { interaction } => {
                if self.pending.len() < MAX_PENDING {
                    self.pending.insert(
                        interaction.request_id.clone(),
                        (self.order, interaction.clone()),
                    );
                }
                false
            }
            HarnessObservationKind::InteractionWithdrawn { request_id } => {
                self.pending.remove(request_id);
                false
            }
            HarnessObservationKind::Status { status } => {
                self.status = Some(status.clone());
                false
            }
            HarnessObservationKind::ToolResult { .. }
            | HarnessObservationKind::SubmissionResultObserved { .. }
            | HarnessObservationKind::NativeTurnTerminal { .. }
            | HarnessObservationKind::Settlement { .. } => true,
            _ => false,
        };
        // Native output after a request means the native side continued past it: the request
        // was answered or abandoned. Responses are host-validated, so this is advisory only.
        if progressed {
            let order = self.order;
            self.pending.retain(|_, (seen, _)| *seen > order);
        }
    }

    fn activity(&self) -> &'static str {
        use bcode_harness_models::{HarnessActivity, HarnessAttention};
        if !self.pending.is_empty() {
            return "needs_attention";
        }
        match self.last_lifecycle {
            Some(Lifecycle::Terminal(HarnessTerminalDisposition::Completed)) => return "settled",
            Some(Lifecycle::Terminal(HarnessTerminalDisposition::Interrupted)) => {
                return "interrupted";
            }
            Some(Lifecycle::Terminal(HarnessTerminalDisposition::Failed)) => return "failed",
            Some(Lifecycle::Started) => return "running",
            None => {}
        }
        match &self.status {
            Some(status)
                if matches!(
                    status.attention,
                    HarnessAttention::Approval | HarnessAttention::Question
                ) =>
            {
                "needs_attention"
            }
            Some(status) => match status.activity {
                HarnessActivity::Running => "running",
                HarnessActivity::Idle | HarnessActivity::Settled => "idle",
                HarnessActivity::Interrupted => "interrupted",
                HarnessActivity::Unknown => "unknown",
            },
            None if self.observations == 0 => "no_new_activity",
            None => "running",
        }
    }

    fn present(&self) -> Value {
        let latest = self
            .terminal_output
            .as_deref()
            .or(self.result_text.as_deref())
            .or_else(|| (!self.text_tail.is_empty()).then_some(self.text_tail.as_str()))
            .map(|text| preview(text, RESULT_PREVIEW_BYTES));
        let pending = self
            .pending
            .values()
            .map(|(_, interaction)| {
                json!({
                    "request_id": interaction.request_id,
                    "kind": match interaction.kind {
                        HarnessInteractionKind::Approval => "approval",
                        HarnessInteractionKind::Question => "question",
                        HarnessInteractionKind::Assistance => "assistance",
                    },
                    "prompt": preview(&interaction.prompt, INTERACTION_PREVIEW_BYTES),
                    "tool_name": interaction.tool_name,
                    "input_preview": interaction.input_preview.as_deref().map(|text| preview(text, INTERACTION_PREVIEW_BYTES)),
                    "choices": interaction.choices.iter().take(MAX_CHOICES).map(|choice| json!({
                        "choice_id": choice.choice_id,
                        "label": preview(&choice.label, INTERACTION_PREVIEW_BYTES),
                    })).collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();
        let hint = match self.activity() {
            "needs_attention" => {
                "Answer each pending request with harness.respond (approve for approvals, answer for questions)."
            }
            "running" | "no_new_activity" | "unknown" => {
                "Poll harness.result again later, passing cursor as after to read only newer activity."
            }
            _ => "Send a follow-up with harness.continue if more work is needed.",
        };
        json!({
            "activity": self.activity(),
            "latest_result": latest,
            "pending": pending,
            "observations": self.observations,
            "hint": hint,
        })
    }
}

fn append_tail(tail: &mut String, text: &str) {
    tail.push_str(text);
    if tail.len() > RESULT_PREVIEW_BYTES * 2 {
        let mut start = tail.len() - RESULT_PREVIEW_BYTES;
        while !tail.is_char_boundary(start) {
            start += 1;
        }
        tail.drain(..start);
    }
}

/// Bounded UTF-8 preview keeping the tail, where native results usually conclude.
fn preview(text: &str, maximum: usize) -> String {
    if text.len() <= maximum {
        return text.to_owned();
    }
    let mut start = text.len() - maximum;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

/// Ensure observation counts used by previews never exceed one page bound.
const _: () = assert!(HISTORY_PAGE as usize <= MAX_HARNESS_OBSERVATIONS);

#[cfg(test)]
mod tests;
