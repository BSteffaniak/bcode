//! Native Bcode children behind the model-facing delegation tools.
//!
//! `harness.delegate` with the reserved target [`NATIVE_TARGET`] creates an ordinary Bcode child
//! session of the calling session, served by the same daemon and state location, instead of
//! opening an external coding agent. The host's native delegation interface admits every
//! operation against the authenticated invocation; this module only shapes requests, derives
//! retry-stable identities and presents bounded results.
//!
//! `harness.continue`, `harness.result` and `harness.interrupt` try the native interface first
//! and fall back to the external managed-session path when the session is not a native
//! delegated child. `harness.wait` and `harness.context` are native only.

use bcode_session_models::SessionId;
use bcode_session_models::delegation::{
    MAX_DELEGATION_WAIT_MS, NATIVE_DELEGATION_CONTEXT_OPERATION,
    NATIVE_DELEGATION_CONTINUE_OPERATION, NATIVE_DELEGATION_DELEGATE_OPERATION,
    NATIVE_DELEGATION_INTERRUPT_OPERATION, NATIVE_DELEGATION_RESULT_OPERATION,
    NATIVE_DELEGATION_WAIT_OPERATION, NativeAdmissionResponse, NativeContextRequest,
    NativeContextResponse, NativeContinueRequest, NativeControlResponse, NativeDelegateRequest,
    NativeDelegationLimits, NativeInterruptRequest, NativeResultRequest, NativeSummariesResponse,
    NativeWaitRequest, NativeWaitUntil, SessionDelegationPermissions,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::delegation::{CallFailure, InvocationScope, ManagedApplication, Outcome};

/// Reserved delegation target naming a native Bcode child.
pub const NATIVE_TARGET: &str = "bcode";
/// Wait for delegated Bcode children.
pub const WAIT_TOOL: &str = "harness.wait";
/// Describe the calling session's delegation context.
pub const CONTEXT_TOOL: &str = "harness.context";
/// Default wait deadline, in seconds.
const DEFAULT_WAIT_SECONDS: u64 = 300;
/// Host failure code for a session that is not a native delegated child of the caller.
const NOT_DELEGATED: &str = "native_delegation_not_delegated_child";

/// Native delegation settings: `plugins.config."bcode.harness".native`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeSettings {
    /// Whether native Bcode children are offered.
    pub enabled: bool,
    /// Limits enforced when delegating.
    pub limits: NativeDelegationLimits,
}

impl Default for NativeSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            limits: NativeDelegationLimits::default(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeSettingsDefinition {
    #[serde(default = "enabled_by_default")]
    enabled: bool,
    #[serde(default)]
    max_depth: Option<u32>,
    #[serde(default)]
    max_active_children: Option<u32>,
}

const fn enabled_by_default() -> bool {
    true
}

/// Read native settings from the plugin configuration.
///
/// Absent configuration enables native delegation with unlimited depth and the default
/// active-children limit.
///
/// # Errors
///
/// * Unknown fields, or zero-valued limits.
pub fn settings(config: &Value) -> Result<NativeSettings, String> {
    let Some(native) = config.get("native").filter(|value| !value.is_null()) else {
        return Ok(NativeSettings::default());
    };
    let definition: NativeSettingsDefinition = serde_json::from_value(native.clone())
        .map_err(|error| format!("invalid native delegation settings: {error}"))?;
    if definition.max_depth == Some(0) || definition.max_active_children == Some(0) {
        return Err("native delegation limits must be positive".into());
    }
    Ok(NativeSettings {
        enabled: definition.enabled,
        limits: NativeDelegationLimits {
            max_depth: definition.max_depth,
            max_active_children: definition
                .max_active_children
                .unwrap_or(bcode_session_models::delegation::DEFAULT_MAX_ACTIVE_DELEGATED_CHILDREN),
        },
    })
}

/// Native-only delegate options, accepted only for the native target.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeDelegateOptions {
    /// Child title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Agent profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Model reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Absolute working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Whether `cwd` is a registered worktree of the parent's repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<bool>,
    /// Authorization policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<SessionDelegationPermissions>,
    /// Whether interrupting the delegating turn interrupts the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_with_turn: Option<bool>,
}

impl NativeDelegateOptions {
    /// Whether any native-only option was supplied.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.agent.is_none()
            && self.model.is_none()
            && self.cwd.is_none()
            && self.worktree.is_none()
            && self.permissions.is_none()
            && self.cancel_with_turn.is_none()
    }
}

/// `harness.wait` input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitInput {
    /// Delegated Bcode children.
    pub session_ids: Vec<String>,
    /// Completion condition; `any` by default.
    #[serde(default)]
    pub until: NativeWaitUntil,
    /// Deadline in seconds.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

impl WaitInput {
    /// Validate the input.
    ///
    /// # Errors
    ///
    /// * Invalid session identities, empty or oversized batches, oversized timeouts.
    pub fn validate(&self) -> Result<(), String> {
        self.request().map(drop)
    }

    fn request(&self) -> Result<NativeWaitRequest, String> {
        let session_ids = self
            .session_ids
            .iter()
            .map(|id| {
                id.parse::<SessionId>()
                    .map_err(|_| "session_ids must be delegated child session IDs".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let timeout_ms = self
            .timeout_seconds
            .unwrap_or(DEFAULT_WAIT_SECONDS)
            .saturating_mul(1000);
        if timeout_ms > MAX_DELEGATION_WAIT_MS {
            return Err(format!(
                "timeout_seconds must be at most {}",
                MAX_DELEGATION_WAIT_MS / 1000
            ));
        }
        let request = NativeWaitRequest {
            session_ids,
            until: self.until,
            timeout_ms,
        };
        request.validate()?;
        Ok(request)
    }
}

/// `harness.context` input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextInput {}

/// Host failure codes are fixed strings; never present native or child text as a reason.
fn native_failure(session_id: Option<&str>, stage: &str, failure: &CallFailure) -> Outcome {
    let (reason, hint) = match failure {
        CallFailure::Host(code) => (
            code.clone(),
            match code.as_str() {
                "native_delegation_depth_exceeded" => {
                    "The configured maximum delegation depth would be exceeded; do the work in this session instead."
                }
                "native_delegation_active_children_exhausted" => {
                    "Too many children are active. Wait for some with harness.wait before delegating more."
                }
                "native_delegation_unknown_agent" => {
                    "Use an agent listed by harness.context."
                }
                NOT_DELEGATED => "Use a session_id returned by harness.delegate from this session.",
                "native_delegation_conflict" => {
                    "This exact delegation was already used with different settings; change the prompt or options."
                }
                _ => "The request was not accepted.",
            }
            .to_owned(),
        ),
        CallFailure::Unknown | CallFailure::Invalid => (
            "outcome_unknown".to_owned(),
            "The outcome is unknown. Call harness.result before retrying; retrying the identical delegate call is safe.".to_owned(),
        ),
        CallFailure::Harness(_) => ("failed".to_owned(), "The request was not accepted.".to_owned()),
    };
    Outcome::error(json!({
        "session_id": session_id,
        "status": "failed",
        "stage": stage,
        "reason": reason,
        "hint": hint,
    }))
}

fn call<T: serde::de::DeserializeOwned>(
    app: &dyn ManagedApplication,
    operation: &str,
    payload: &impl Serialize,
) -> Result<T, CallFailure> {
    let payload = serde_json::to_value(payload).map_err(|_| CallFailure::Invalid)?;
    let value = app.call(operation, payload)?;
    crate::protocol::bounded_response(&value).map_err(|_| CallFailure::Invalid)?;
    serde_json::from_value(value).map_err(|_| CallFailure::Invalid)
}

fn is_not_delegated(failure: &CallFailure) -> bool {
    matches!(failure, CallFailure::Host(code) if code == NOT_DELEGATED)
}

/// Delegate `prompt` to a fresh native Bcode child.
#[must_use]
pub fn delegate(
    app: &dyn ManagedApplication,
    scope: &InvocationScope,
    invocation_id: &str,
    prompt: &str,
    options: &NativeDelegateOptions,
    settings: NativeSettings,
) -> Outcome {
    let child_id = crate::delegation::DelegationIdentities::derive(
        scope,
        invocation_id,
        NATIVE_TARGET,
        prompt,
    )
    .session_id;
    let request = NativeDelegateRequest {
        child_id,
        title: options.title.clone(),
        prompt: prompt.to_owned(),
        agent: options.agent.clone(),
        model: options.model.clone(),
        working_directory: options.cwd.as_ref().map(Into::into),
        worktree: options.worktree.unwrap_or(false),
        permissions: options.permissions.unwrap_or_default(),
        cancel_with_turn: options.cancel_with_turn.unwrap_or(false),
        limits: settings.limits,
    };
    if let Err(message) = request.validate() {
        return Outcome::error(
            json!({"status":"failed","reason":"invalid_request","hint":message}),
        );
    }
    let child = child_id.to_string();
    match call::<NativeAdmissionResponse>(app, NATIVE_DELEGATION_DELEGATE_OPERATION, &request) {
        Ok(response) => Outcome::ok(json!({
            "session_id": child,
            "target": NATIVE_TARGET,
            "status": if response.reused { "already_submitted" } else { "submitted" },
            "child": response.child,
            "next": "The child runs in the background. Use harness.wait to block until it settles or needs attention, harness.result to check it, and harness.continue to send a follow-up. Its permission prompts and questions appear to the user in this session.",
        })),
        Err(failure) => native_failure(Some(&child), "delegate", &failure),
    }
}

fn parse_session(value: &str) -> Result<SessionId, Outcome> {
    value.parse::<SessionId>().map_err(|_| {
        Outcome::error(json!({"status":"failed","reason":"invalid_request","hint":"session_id must be a delegated child session ID"}))
    })
}

/// Continue a native child; `None` when the session is not a native delegated child.
#[must_use]
pub fn continue_child(
    app: &dyn ManagedApplication,
    scope: &InvocationScope,
    invocation_id: &str,
    session_id: &str,
    prompt: &str,
) -> Option<Outcome> {
    let child = match parse_session(session_id) {
        Ok(child) => child,
        Err(outcome) => return Some(outcome),
    };
    let request = NativeContinueRequest {
        session_id: child,
        prompt: prompt.to_owned(),
        idempotency_key: crate::delegation::derived_key(
            "native-continue",
            &[
                scope.parent_session_id.as_str(),
                invocation_id,
                session_id,
                prompt,
            ],
        ),
    };
    match call::<NativeAdmissionResponse>(app, NATIVE_DELEGATION_CONTINUE_OPERATION, &request) {
        Ok(response) => Some(Outcome::ok(json!({
            "session_id": session_id,
            "status": if response.reused { "already_submitted" } else { "submitted" },
            "child": response.child,
            "next": "Use harness.wait or harness.result to follow the child.",
        }))),
        Err(failure) if is_not_delegated(&failure) => None,
        Err(failure) => Some(native_failure(Some(session_id), "continue", &failure)),
    }
}

/// Summarize a native child; `None` when the session is not a native delegated child.
#[must_use]
pub fn result(app: &dyn ManagedApplication, session_id: &str) -> Option<Outcome> {
    let child = match parse_session(session_id) {
        Ok(child) => child,
        Err(outcome) => return Some(outcome),
    };
    let request = NativeResultRequest {
        session_ids: vec![child],
    };
    match call::<NativeSummariesResponse>(app, NATIVE_DELEGATION_RESULT_OPERATION, &request) {
        Ok(mut response) => response.children.pop().map(|summary| {
            Outcome::ok(json!({
                "session_id": session_id,
                "target": NATIVE_TARGET,
                "child": summary,
                "note": "settled means the child is idle, not that its work is accepted; review latest_result.",
            }))
        }),
        Err(failure) if is_not_delegated(&failure) => None,
        Err(failure) => Some(native_failure(Some(session_id), "result", &failure)),
    }
}

/// Interrupt a native child; `None` when the session is not a native delegated child.
#[must_use]
pub fn interrupt(app: &dyn ManagedApplication, session_id: &str) -> Option<Outcome> {
    let child = match parse_session(session_id) {
        Ok(child) => child,
        Err(outcome) => return Some(outcome),
    };
    let request = NativeInterruptRequest { session_id: child };
    match call::<NativeControlResponse>(app, NATIVE_DELEGATION_INTERRUPT_OPERATION, &request) {
        Ok(response) => Some(Outcome::ok(json!({
            "session_id": session_id,
            "status": "interrupt_requested",
            "child": response.child,
        }))),
        Err(failure) if is_not_delegated(&failure) => None,
        Err(failure) => Some(native_failure(Some(session_id), "interrupt", &failure)),
    }
}

/// Explain that native children's prompts are answered by the user; `None` for other sessions.
#[must_use]
pub fn respond(app: &dyn ManagedApplication, session_id: &str) -> Option<Outcome> {
    result(app, session_id).map(|summary| {
        if summary.is_error {
            return summary;
        }
        Outcome::error(json!({
            "session_id": session_id,
            "status": "failed",
            "reason": "user_answers_native_children",
            "hint": "Permission requests and questions from Bcode children are shown to the user in this session and only the user can answer them. Use harness.wait to continue once they are answered.",
        }))
    })
}

/// Wait for native children.
#[must_use]
pub fn wait(app: &dyn ManagedApplication, input: &WaitInput) -> Outcome {
    let request = match input.request() {
        Ok(request) => request,
        Err(message) => {
            return Outcome::error(
                json!({"status":"failed","reason":"invalid_request","hint":message}),
            );
        }
    };
    match call::<NativeSummariesResponse>(app, NATIVE_DELEGATION_WAIT_OPERATION, &request) {
        Ok(response) => Outcome::ok(json!({
            "timed_out": response.timed_out,
            "children": response.children,
            "next": if response.timed_out {
                "No child reached the condition before the deadline; wait again or check harness.result."
            } else {
                "Review each child's status and latest_result. needs_attention means the user must answer a prompt in this session."
            },
        })),
        Err(failure) => native_failure(None, "wait", &failure),
    }
}

/// Describe the calling session's delegation context.
#[must_use]
pub fn context(app: &dyn ManagedApplication, settings: NativeSettings) -> Outcome {
    let request = NativeContextRequest {
        limits: settings.limits,
    };
    match call::<NativeContextResponse>(app, NATIVE_DELEGATION_CONTEXT_OPERATION, &request) {
        Ok(response) => Outcome::ok(
            serde_json::to_value(response).unwrap_or_else(|_| json!({"status":"unavailable"})),
        ),
        Err(failure) => native_failure(None, "context", &failure),
    }
}

/// Model-facing schema properties for native-only delegate options.
#[must_use]
pub fn delegate_option_properties() -> Value {
    json!({
        "title": {"type":"string", "description":"Bcode child only: short title for the child session."},
        "agent": {"type":"string", "description":"Bcode child only: agent profile (see harness.context); the session default when omitted."},
        "model": {"type":"string", "description":"Bcode child only: model reference; this session's model when omitted."},
        "cwd": {"type":"string", "description":"Bcode child only: absolute working directory; this session's workspace when omitted."},
        "worktree": {"type":"boolean", "description":"Bcode child only: cwd is an existing registered worktree of this repository."},
        "permissions": {"type":"string", "enum":["inherit","agent_profile","always_ask"], "description":"Bcode child only: inherit (default; never more than this session), agent_profile (the child agent's policy), always_ask (the user approves every permission-gated action)."},
        "cancel_with_turn": {"type":"boolean", "description":"Bcode child only: interrupting this turn also interrupts the child (default false)."},
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    type Reply = Box<dyn Fn(&str) -> Result<Value, CallFailure>>;

    struct Recorder {
        calls: RefCell<Vec<(String, Value)>>,
        reply: Reply,
    }

    impl ManagedApplication for Recorder {
        fn call(&self, operation: &str, payload: Value) -> Result<Value, CallFailure> {
            self.calls
                .borrow_mut()
                .push((operation.to_owned(), payload));
            (self.reply)(operation)
        }
    }

    fn scope() -> InvocationScope {
        InvocationScope {
            parent_session_id: SessionId::new().to_string(),
            working_directory: "/repo".into(),
        }
    }

    fn summary(session_id: SessionId) -> Value {
        json!({"session_id": session_id, "status": "running"})
    }

    #[test]
    fn settings_default_to_enabled_unlimited_depth_and_bounded_children() {
        let defaults = settings(&Value::Null).unwrap();
        assert!(defaults.enabled);
        assert_eq!(defaults.limits.max_depth, None);
        assert_eq!(defaults.limits.max_active_children, 32);
        let configured =
            settings(&json!({"native": {"max_depth": 3, "max_active_children": 4}})).unwrap();
        assert_eq!(configured.limits.max_depth, Some(3));
        assert_eq!(configured.limits.max_active_children, 4);
        assert!(
            !settings(&json!({"native": {"enabled": false}}))
                .unwrap()
                .enabled
        );
        assert!(settings(&json!({"native": {"max_depth": 0}})).is_err());
        assert!(settings(&json!({"native": {"unknown": 1}})).is_err());
    }

    #[test]
    fn retried_delegations_reuse_the_derived_child_identity() {
        let scope = scope();
        let recorder = Recorder {
            calls: RefCell::default(),
            reply: Box::new(|_| Err(CallFailure::Unknown)),
        };
        let options = NativeDelegateOptions {
            permissions: Some(SessionDelegationPermissions::AlwaysAsk),
            cancel_with_turn: Some(true),
            ..NativeDelegateOptions::default()
        };
        let settings = NativeSettings::default();
        let first = delegate(&recorder, &scope, "call-1", "task", &options, settings);
        let second = delegate(&recorder, &scope, "call-1", "task", &options, settings);
        assert!(first.is_error && second.is_error);
        assert_eq!(first.output["reason"], "outcome_unknown");
        let calls = recorder.calls.borrow();
        assert_eq!(calls[0].1["child_id"], calls[1].1["child_id"]);
        assert_eq!(calls[0].1["permissions"], "always_ask");
        assert_eq!(calls[0].1["cancel_with_turn"], true);
        assert_eq!(calls[0].1["limits"]["max_active_children"], 32);
        drop(calls);
        let _ = delegate(&recorder, &scope, "call-2", "task", &options, settings);
        let calls = recorder.calls.borrow();
        assert_ne!(calls[0].1["child_id"], calls[2].1["child_id"]);
    }

    #[test]
    fn non_native_sessions_fall_back_to_the_external_path() {
        let recorder = Recorder {
            calls: RefCell::default(),
            reply: Box::new(|_| Err(CallFailure::Host(NOT_DELEGATED.into()))),
        };
        let child = SessionId::new().to_string();
        assert!(result(&recorder, &child).is_none());
        assert!(interrupt(&recorder, &child).is_none());
        assert!(continue_child(&recorder, &scope(), "call", &child, "next").is_none());
        assert!(respond(&recorder, &child).is_none());
    }

    #[test]
    fn native_children_cannot_be_answered_by_the_parent_agent() {
        let child = SessionId::new();
        let recorder = Recorder {
            calls: RefCell::default(),
            reply: Box::new(move |_| Ok(json!({"children": [summary(child)]}))),
        };
        let outcome = respond(&recorder, &child.to_string()).unwrap();
        assert!(outcome.is_error);
        assert_eq!(outcome.output["reason"], "user_answers_native_children");
    }

    #[test]
    fn waits_are_bounded() {
        let id = SessionId::new().to_string();
        assert!(
            WaitInput {
                session_ids: vec![id.clone()],
                until: NativeWaitUntil::All,
                timeout_seconds: Some(MAX_DELEGATION_WAIT_MS / 1000 + 1),
            }
            .validate()
            .is_err()
        );
        assert!(
            WaitInput {
                session_ids: vec!["not-a-session".into()],
                until: NativeWaitUntil::Any,
                timeout_seconds: None,
            }
            .validate()
            .is_err()
        );
        let request = WaitInput {
            session_ids: vec![id],
            until: NativeWaitUntil::Any,
            timeout_seconds: None,
        }
        .request()
        .unwrap();
        assert_eq!(request.timeout_ms, DEFAULT_WAIT_SECONDS * 1000);
    }
}
