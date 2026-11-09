//! Managed (external harness) chat controls.
//!
//! Pure TUI policy for managed conversations: which host commands remain available, which
//! slash commands are forwarded to the native harness as text, the native permission-mode
//! cycle, adapters that present `SessionView` pending external interactions through the
//! existing permission dialog and question surface, and retained response identities.
//!
//! Nothing here executes a Bcode tool or grants native authority. Responses are typed
//! `HarnessResponseValue`s that the managed application re-admits under generation checks.

use std::collections::{BTreeMap, BTreeSet};

use bcode_harness_models::{
    HarnessFailure, HarnessPermissionMode, HarnessResponseValue, HarnessServiceResponse,
    HarnessServiceResponseKind,
};
use bcode_session_view_models::PermissionView;
use bcode_session_view_models::external::{ExternalInteractionKind, ExternalInteractionView};

use super::interactive_surface::InteractiveSurfaceRequest;
use super::slash_registry::{self, BuiltinCommandId};

/// Slash commands (without the leading slash) that a managed chat forwards to the native
/// harness as ordinary submitted text, because the native agent implements them itself.
///
/// This is an explicit allow-list: unknown slash commands are never forwarded. `/clear` is
/// intentionally absent because it would discard native context without Bcode's knowledge.
pub const MANAGED_NATIVE_PASSTHROUGH_COMMANDS: &[&str] = &["compact"];

/// Inline question surface declared by the bundled question plugin.
pub const QUESTION_INLINE_SURFACE: &str = "bcode.question.inline";

/// Interaction-id prefix reserved for managed native interactions presented on a surface.
const MANAGED_SURFACE_PREFIX: &str = "native-harness";

const MAX_QUESTION_TEXT_BYTES: usize = 16 * 1024;
const MAX_OPTION_TEXT_BYTES: usize = 4 * 1024;
const MAX_QUESTION_OPTIONS: usize = 100;

/// How a slash command behaves inside a managed conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedCommandSupport {
    /// Host-side operation that does not depend on the native executor.
    Host,
    /// Managed control routed through the sealed managed command handle.
    Control,
    /// Forwarded verbatim to the native harness as a normal managed submission.
    NativePassthrough,
    /// Not available in a managed conversation; no model or plugin fallback is attempted.
    Unavailable,
}

/// Classify one builtin command for managed conversations.
#[must_use]
pub const fn builtin_support(id: BuiltinCommandId) -> ManagedCommandSupport {
    match id {
        BuiltinCommandId::Version
        | BuiltinCommandId::Sessions
        | BuiltinCommandId::Monitor
        | BuiltinCommandId::Search
        | BuiltinCommandId::Resync
        | BuiltinCommandId::New
        | BuiltinCommandId::Theme
        | BuiltinCommandId::Profile
        | BuiltinCommandId::Appearance
        | BuiltinCommandId::Streaming
        | BuiltinCommandId::Timeline
        | BuiltinCommandId::Exit => ManagedCommandSupport::Host,
        BuiltinCommandId::Native
        | BuiltinCommandId::Agent
        | BuiltinCommandId::Model
        | BuiltinCommandId::Stop
        | BuiltinCommandId::Permissions => ManagedCommandSupport::Control,
        BuiltinCommandId::Compact => ManagedCommandSupport::NativePassthrough,
        BuiltinCommandId::RescanImports
        | BuiltinCommandId::AuthPool
        | BuiltinCommandId::Provider
        | BuiltinCommandId::Context
        | BuiltinCommandId::Cwd
        | BuiltinCommandId::Worktree
        | BuiltinCommandId::Ralph
        | BuiltinCommandId::Skills
        | BuiltinCommandId::Skill
        | BuiltinCommandId::Thinking
        | BuiltinCommandId::CancelRuntime
        | BuiltinCommandId::Runtime => ManagedCommandSupport::Unavailable,
    }
}

/// Classify a submitted slash command; `None` when the message is not a slash command.
#[must_use]
pub fn command_support(message: &str) -> Option<ManagedCommandSupport> {
    let name = slash_registry::slash_command_name(message)?;
    if MANAGED_NATIVE_PASSTHROUGH_COMMANDS.contains(&name) {
        return Some(ManagedCommandSupport::NativePassthrough);
    }
    if name == "auth" {
        return Some(ManagedCommandSupport::Control);
    }
    Some(
        slash_registry::builtin_command(name)
            .map_or(ManagedCommandSupport::Unavailable, |command| {
                builtin_support(command.id())
            }),
    )
}

/// Whether a submitted message must be sent to the native harness as plain text.
#[must_use]
pub fn is_native_passthrough(message: &str) -> bool {
    command_support(message) == Some(ManagedCommandSupport::NativePassthrough)
}

/// Managed-only completions for commands without a usable ordinary registry entry.
const MANAGED_CONTROL_COMPLETIONS: &[(&str, &str)] = &[
    ("/auth status", "Show native authentication readiness"),
    ("/auth login", "Hand the terminal to the native login flow"),
    ("/native interrupt", "Interrupt the active native turn"),
    (
        "/native respond ",
        "Answer a native request: approve|deny|answer <choice>|assist <text> or JSON",
    ),
    ("/native retry ", "Retry a retained native submission"),
    (
        "/native composer next",
        "Queue the next input for the next native turn",
    ),
    ("/native composer steer ", "Steer an explicit native turn"),
];

fn managed_description(command: &str) -> Option<&'static str> {
    Some(match command {
        "/plan" => "Switch the native permission mode to plan",
        "/build" => "Switch the native permission mode to default",
        "/model" | "/models" => "Pick a native model for subsequent turns",
        "/set-model " => "Switch the native model by id",
        "/stop" => "Interrupt the active native turn",
        "/permissions" => "Review pending native approvals and questions again",
        "/compact" => "Send /compact to the native agent (native compaction)",
        _ => return None,
    })
}

/// Commands from the ordinary registry that are not meaningful in a managed chat even though
/// their builtin family is supported.
fn managed_completion_hidden(command: &str) -> bool {
    matches!(command, "/agent " | "/native status" | "/native history")
}

/// Slash completions offered in a managed conversation, derived from the same allow-list
/// used to execute commands.
#[must_use]
pub fn managed_slash_completions() -> Vec<(&'static str, &'static str)> {
    let mut completions: Vec<(&'static str, &'static str)> = MANAGED_CONTROL_COMPLETIONS.to_vec();
    for (id, completion) in slash_registry::builtin_completions() {
        if builtin_support(id) == ManagedCommandSupport::Unavailable
            || managed_completion_hidden(completion.command())
        {
            continue;
        }
        completions.push((
            completion.command(),
            managed_description(completion.command()).unwrap_or_else(|| completion.description()),
        ));
    }
    completions
}

/// Next permission mode for the agent-cycle key: Default → `AcceptEdits` → Plan → Default.
#[must_use]
pub const fn next_permission_mode(current: HarnessPermissionMode) -> HarnessPermissionMode {
    match current {
        HarnessPermissionMode::Default => HarnessPermissionMode::AcceptEdits,
        HarnessPermissionMode::AcceptEdits => HarnessPermissionMode::Plan,
        HarnessPermissionMode::Plan => HarnessPermissionMode::Default,
    }
}

/// Short user-facing label for a native permission mode.
#[must_use]
pub const fn permission_mode_label(mode: HarnessPermissionMode) -> &'static str {
    match mode {
        HarnessPermissionMode::Default => "default mode",
        HarnessPermissionMode::AcceptEdits => "accept-edits mode",
        HarnessPermissionMode::Plan => "plan mode",
    }
}

/// Parse `/plan`, `/build` and `/agent <mode>` into a native permission mode.
///
/// # Errors
/// Returns a usage message for unknown agent/mode names; no Bcode agent is selected.
pub fn permission_mode_command(parts: &[&str]) -> Result<HarnessPermissionMode, String> {
    match parts {
        ["/plan"] | ["/agent", "plan"] => Ok(HarnessPermissionMode::Plan),
        ["/build"] | ["/agent", "build" | "default"] => Ok(HarnessPermissionMode::Default),
        ["/agent", "accept-edits" | "accept_edits" | "acceptedits"] => {
            Ok(HarnessPermissionMode::AcceptEdits)
        }
        _ => Err(
            "Managed conversations select native permission modes: /plan, /build, or /agent plan|build|accept-edits"
                .into(),
        ),
    }
}

/// Parse the `/native respond` payload: the typed JSON form or a verb.
///
/// Verbs: `approve`, `deny`, `answer <choice-id> [text…]`, `assist <text…>`, `cancel`.
///
/// # Errors
/// Returns a usage message for unknown verbs or malformed JSON; nothing is sent.
pub fn parse_native_response(text: &str) -> Result<HarnessResponseValue, String> {
    const USAGE: &str = "Expected approve|deny|answer <choice> [text]|assist <text>|cancel, or typed JSON such as {\"kind\":\"approval\",\"approved\":true}; no native response sent";
    let trimmed = text.trim();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed).map_err(|_| USAGE.to_owned());
    }
    let mut words = trimmed.split_whitespace();
    let verb = words.next().ok_or_else(|| USAGE.to_owned())?;
    let rest = words.collect::<Vec<_>>();
    match (verb, rest.as_slice()) {
        ("approve", []) => Ok(HarnessResponseValue::Approval { approved: true }),
        ("deny", []) => Ok(HarnessResponseValue::Approval { approved: false }),
        ("cancel", []) => Ok(HarnessResponseValue::Cancel),
        ("answer", [choice, text @ ..]) => Ok(HarnessResponseValue::Answer {
            choice_id: Some((*choice).to_owned()),
            text: text.join(" "),
        }),
        ("assist", text) if !text.is_empty() => Ok(HarnessResponseValue::Assistance {
            text: text.join(" "),
        }),
        _ => Err(USAGE.to_owned()),
    }
}

/// Stable key for one pending interaction: (generation, native request id).
pub type ManagedInteractionKey = (u64, String);

/// Key of one presented interaction view.
#[must_use]
pub fn interaction_key(view: &ExternalInteractionView) -> ManagedInteractionKey {
    (view.provenance.generation, view.request_id.clone())
}

/// Surface interaction id for a managed interaction presented on the question surface.
#[must_use]
pub fn surface_interaction_id(key: &ManagedInteractionKey) -> String {
    format!("{MANAGED_SURFACE_PREFIX}:{}:{}", key.0, key.1)
}

/// Inverse of [`surface_interaction_id`]; `None` for ordinary interaction ids.
#[must_use]
pub fn parse_surface_interaction_id(id: &str) -> Option<ManagedInteractionKey> {
    let rest = id.strip_prefix(MANAGED_SURFACE_PREFIX)?.strip_prefix(':')?;
    let (generation, request_id) = rest.split_once(':')?;
    let generation = generation.parse().ok()?;
    (!request_id.is_empty()).then(|| (generation, request_id.to_owned()))
}

/// Permission-dialog identifier for a managed approval; distinct from Bcode permission ids.
#[must_use]
pub fn approval_permission_id(key: &ManagedInteractionKey) -> String {
    format!("{MANAGED_SURFACE_PREFIX}:{}:{}", key.0, key.1)
}

/// Present a native approval through the existing permission dialog (approve/deny only).
///
/// `tool_name` and `input_preview` are untrusted display content; they never dispatch a tool.
#[must_use]
pub fn approval_permission_view(
    owner: bcode_session_models::SessionId,
    view: &ExternalInteractionView,
) -> PermissionView {
    let key = interaction_key(view);
    let tool_name = view
        .tool_name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "native tool".to_owned());
    PermissionView {
        permission_id: approval_permission_id(&key),
        session_id: Some(owner),
        tool_call_id: view.request_id.clone(),
        tool_name,
        arguments_json: view.input_preview.clone().unwrap_or_default(),
        batch: None,
        agent_id: bcode_session_view::external_projection::harness_display_label(
            &view.provenance.plugin_id,
        ),
        title: Some(if view.prompt.trim().is_empty() {
            "Native permission requested".to_owned()
        } else {
            view.prompt.clone()
        }),
        policy_source: None,
        detail: (!view.scope.trim().is_empty()).then(|| view.scope.clone()),
        resolved: false,
        approved: None,
        can_remember: false,
        // Native harness approvals are not produced by a Bcode plugin.
        producer_plugin_id: None,
    }
}

fn truncate_utf8(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// Build the question-surface request for a native question or assistance request.
///
/// Questions present one radio question whose option values are native choice ids, with
/// free text allowed. Assistance presents free text only. Approvals return `None`.
#[must_use]
pub fn question_surface_request(
    view: &ExternalInteractionView,
) -> Option<InteractiveSurfaceRequest> {
    let options = match view.interaction_kind {
        ExternalInteractionKind::Approval => return None,
        ExternalInteractionKind::Question => view
            .choices
            .iter()
            .take(MAX_QUESTION_OPTIONS)
            .map(|choice| {
                serde_json::json!({
                    "label": truncate_utf8(
                        if choice.label.trim().is_empty() { &choice.id } else { &choice.label },
                        MAX_OPTION_TEXT_BYTES,
                    ),
                    "value": truncate_utf8(&choice.id, MAX_OPTION_TEXT_BYTES),
                    "description": null,
                })
            })
            .collect::<Vec<_>>(),
        ExternalInteractionKind::Assistance => Vec::new(),
    };
    let prompt = if view.prompt.trim().is_empty() {
        match view.interaction_kind {
            ExternalInteractionKind::Assistance => "The native agent requests assistance",
            _ => "The native agent asks a question",
        }
    } else {
        view.prompt.as_str()
    };
    let header = view
        .tool_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .map(|name| truncate_utf8(name, MAX_OPTION_TEXT_BYTES));
    let request = serde_json::json!({
        "questions": [{
            "header": header,
            "question": truncate_utf8(prompt, MAX_QUESTION_TEXT_BYTES),
            "options": options,
            "control": "radio",
            "selection_mode": "single",
            "custom": true,
            "custom_mode": "exclusive",
            "required": true,
        }]
    });
    Some(InteractiveSurfaceRequest::new(
        surface_interaction_id(&interaction_key(view)),
        QUESTION_INLINE_SURFACE,
        request.to_string(),
    ))
}

#[derive(serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum QuestionSurfacePayload {
    Answered {
        questions: Vec<QuestionSurfaceAnswer>,
    },
    Unanswered,
    Dismissed,
}

#[derive(serde::Deserialize)]
struct QuestionSurfaceAnswer {
    #[serde(default)]
    selected: Vec<String>,
    #[serde(default)]
    custom: Option<String>,
}

/// Map a question-surface resolution to a native response; `None` means no response
/// (dismissed, unanswered, cancelled by the host, or not representable).
///
/// Question answers carry the selected choice id and the custom text, or the choice label
/// when no custom text was entered. Assistance answers carry the free text only.
#[must_use]
pub fn question_response_value(
    view: &ExternalInteractionView,
    resolution: &bcode_session_models::ToolExchangeResolution,
) -> Option<HarnessResponseValue> {
    let bcode_session_models::ToolExchangeResolution::Responded { payload } = resolution else {
        return None;
    };
    let QuestionSurfacePayload::Answered { questions } =
        serde_json::from_value::<QuestionSurfacePayload>(payload.clone()).ok()?
    else {
        return None;
    };
    let answer = questions.into_iter().next()?;
    let custom = answer.custom.filter(|text| !text.trim().is_empty());
    match view.interaction_kind {
        ExternalInteractionKind::Approval => None,
        ExternalInteractionKind::Assistance => {
            custom.map(|text| HarnessResponseValue::Assistance { text })
        }
        ExternalInteractionKind::Question => {
            let choice = answer
                .selected
                .into_iter()
                .next()
                .and_then(|value| view.choices.iter().find(|choice| choice.id == value));
            match (choice, custom) {
                (None, None) => None,
                (choice, Some(text)) => Some(HarnessResponseValue::Answer {
                    choice_id: choice.map(|choice| choice.id.clone()),
                    text,
                }),
                (Some(choice), None) => Some(HarnessResponseValue::Answer {
                    choice_id: Some(choice.id.clone()),
                    text: choice.label.clone(),
                }),
            }
        }
    }
}

/// Outcome class of one managed response attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedResponseOutcome {
    /// The managed application acknowledged the response.
    Acknowledged,
    /// The response was definitively refused; a new decision may use a new identity.
    Refused,
    /// The outcome is unknown; a later decision must reuse the same response identity.
    Uncertain,
}

/// Classify a managed response result without retrying it.
#[must_use]
pub fn classify_response(
    result: &Result<HarnessServiceResponse, bcode_client::ClientError>,
) -> ManagedResponseOutcome {
    match result {
        Ok(response) => match &response.kind {
            HarnessServiceResponseKind::Failure(HarnessFailure::Uncertain) => {
                ManagedResponseOutcome::Uncertain
            }
            HarnessServiceResponseKind::Failure(_) => ManagedResponseOutcome::Refused,
            _ => ManagedResponseOutcome::Acknowledged,
        },
        Err(bcode_client::ClientError::Server { code, .. }) => {
            if code == "managed_session_uncertain" || code == "managed_session_deadline" {
                ManagedResponseOutcome::Uncertain
            } else {
                ManagedResponseOutcome::Refused
            }
        }
        Err(_) => ManagedResponseOutcome::Uncertain,
    }
}

/// Whether a client error reports that the daemon no longer recognizes this owner's
/// generation (for example after a daemon restart), so recovery is required.
#[must_use]
pub fn is_stale_owner(error: &bcode_client::ClientError) -> bool {
    matches!(error, bcode_client::ClientError::Server { code, .. } if code == "managed_session_stale_owner")
}

/// Retained presentation and response identity state for managed native interactions.
///
/// One response identity is minted per decision and reused while the outcome is uncertain,
/// so a repeated decision can never become a second, independent native mutation. Nothing
/// here re-sends a response automatically.
#[derive(Debug, Default)]
pub struct ManagedInteractionTracker {
    response_ids: BTreeMap<ManagedInteractionKey, String>,
    in_flight: BTreeSet<ManagedInteractionKey>,
    settled: BTreeSet<ManagedInteractionKey>,
    held: BTreeSet<ManagedInteractionKey>,
    presented: Option<ManagedInteractionKey>,
    surfaces: BTreeSet<ManagedInteractionKey>,
}

impl ManagedInteractionTracker {
    /// Remember that `key` was queued on the question surface.
    pub fn note_surface(&mut self, key: ManagedInteractionKey) {
        self.surfaces.insert(key);
    }

    /// Surface keys that are no longer pending; they are forgotten as they are returned.
    pub fn stale_surface_keys(
        &mut self,
        live: &BTreeSet<ManagedInteractionKey>,
    ) -> Vec<ManagedInteractionKey> {
        let stale = self
            .surfaces
            .iter()
            .filter(|key| !live.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        self.surfaces.retain(|key| live.contains(key));
        stale
    }

    /// Response identity for the next decision on `key`, reusing an uncertain one.
    pub fn response_id(&mut self, key: &ManagedInteractionKey) -> String {
        self.response_ids
            .entry(key.clone())
            .or_insert_with(|| uuid::Uuid::new_v4().to_string())
            .clone()
    }

    /// Record that a decision for `key` was dispatched.
    pub fn begin(&mut self, key: &ManagedInteractionKey) {
        self.in_flight.insert(key.clone());
        if self.presented.as_ref() == Some(key) {
            self.presented = None;
        }
    }

    /// Record the classified outcome of a dispatched decision.
    pub fn finish(&mut self, key: &ManagedInteractionKey, outcome: ManagedResponseOutcome) {
        self.in_flight.remove(key);
        match outcome {
            ManagedResponseOutcome::Acknowledged => {
                self.settled.insert(key.clone());
            }
            ManagedResponseOutcome::Refused => {
                self.response_ids.remove(key);
                self.held.insert(key.clone());
            }
            ManagedResponseOutcome::Uncertain => {
                self.held.insert(key.clone());
            }
        }
    }

    /// The user dismissed the presentation without answering; do not reopen automatically.
    pub fn dismiss(&mut self, key: &ManagedInteractionKey) {
        self.held.insert(key.clone());
        if self.presented.as_ref() == Some(key) {
            self.presented = None;
        }
    }

    /// Allow held (dismissed, refused or uncertain) interactions to be presented again.
    pub fn review_again(&mut self) {
        self.held.clear();
    }

    /// Mark `key` as currently presented in a dialog or surface.
    pub fn present(&mut self, key: ManagedInteractionKey) {
        self.presented = Some(key);
    }

    /// Currently presented interaction.
    #[must_use]
    pub const fn presented(&self) -> Option<&ManagedInteractionKey> {
        self.presented.as_ref()
    }

    /// Forget a presentation that the host closed for another reason.
    pub fn clear_presented(&mut self) {
        self.presented = None;
    }

    /// Whether `key` currently has a dispatched, unsettled decision.
    #[must_use]
    pub fn is_in_flight(&self, key: &ManagedInteractionKey) -> bool {
        self.in_flight.contains(key)
    }

    /// Drop state for interactions no longer pending in the authoritative snapshot.
    /// Returns whether the presented interaction disappeared.
    pub fn retain_pending(&mut self, pending: &[ExternalInteractionView]) -> bool {
        let live = pending.iter().map(interaction_key).collect::<BTreeSet<_>>();
        self.response_ids.retain(|key, _| live.contains(key));
        self.in_flight.retain(|key| live.contains(key));
        self.settled.retain(|key| live.contains(key));
        self.held.retain(|key| live.contains(key));
        if self
            .presented
            .as_ref()
            .is_some_and(|key| !live.contains(key))
        {
            self.presented = None;
            return true;
        }
        false
    }

    /// First pending interaction that should be presented now, if nothing is presented.
    #[must_use]
    pub fn next_to_present<'a>(
        &self,
        pending: &'a [ExternalInteractionView],
    ) -> Option<&'a ExternalInteractionView> {
        if self.presented.is_some() {
            return None;
        }
        pending.iter().find(|view| {
            let key = interaction_key(view);
            view.response.enabled
                && !self.in_flight.contains(&key)
                && !self.settled.contains(&key)
                && !self.held.contains(&key)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_view_models::external::{
        ExternalActionKind, ExternalActionView, ExternalChoiceView, ExternalProvenanceView,
    };

    fn view(kind: ExternalInteractionKind, request_id: &str) -> ExternalInteractionView {
        ExternalInteractionView {
            request_id: request_id.into(),
            provenance: ExternalProvenanceView {
                session_id: "session".into(),
                instance_id: "claude".into(),
                plugin_id: "plugin".into(),
                native_session_id: "native".into(),
                generation: 3,
            },
            interaction_kind: kind,
            kind: "question".into(),
            prompt: "Which file?".into(),
            scope: "workspace".into(),
            choices: vec![
                ExternalChoiceView {
                    id: "a".into(),
                    label: "src/a.rs".into(),
                },
                ExternalChoiceView {
                    id: "b".into(),
                    label: "src/b.rs".into(),
                },
            ],
            tool_name: Some("Edit".into()),
            input_preview: Some(r#"{"file_path":"src/a.rs"}"#.into()),
            response: ExternalActionView {
                action: ExternalActionKind::Respond,
                enabled: true,
                reason: String::new(),
            },
        }
    }

    #[test]
    fn allow_list_classifies_host_control_passthrough_and_unavailable() {
        assert_eq!(command_support("/theme"), Some(ManagedCommandSupport::Host));
        // Leaving works the same in a managed conversation, under either name.
        for exit in ["/exit", "/quit"] {
            assert_eq!(command_support(exit), Some(ManagedCommandSupport::Host));
            assert_eq!(
                slash_registry::builtin_command(&exit[1..])
                    .map(slash_registry::BuiltinSlashCommand::id),
                Some(BuiltinCommandId::Exit)
            );
        }
        assert_eq!(
            command_support("/sessions"),
            Some(ManagedCommandSupport::Host)
        );
        assert_eq!(
            command_support("/model"),
            Some(ManagedCommandSupport::Control)
        );
        assert_eq!(
            command_support("/plan"),
            Some(ManagedCommandSupport::Control)
        );
        assert_eq!(
            command_support("/auth status"),
            Some(ManagedCommandSupport::Control)
        );
        assert_eq!(
            command_support("/compact"),
            Some(ManagedCommandSupport::NativePassthrough)
        );
        assert_eq!(
            command_support("/clear"),
            Some(ManagedCommandSupport::Unavailable)
        );
        assert_eq!(
            command_support("/skills"),
            Some(ManagedCommandSupport::Unavailable)
        );
        assert_eq!(
            command_support("/frobnicate"),
            Some(ManagedCommandSupport::Unavailable)
        );
        assert_eq!(command_support("hello"), None);
        assert!(is_native_passthrough("/compact keep the plan"));
        assert!(!is_native_passthrough("/clear"));
        assert!(!is_native_passthrough("compact"));
    }

    #[test]
    fn managed_completions_follow_the_execution_allow_list() {
        let completions = managed_slash_completions();
        let commands = completions
            .iter()
            .map(|(command, _)| *command)
            .collect::<Vec<_>>();
        for expected in [
            "/theme",
            "/model",
            "/plan",
            "/build",
            "/compact",
            "/auth status",
            "/native respond ",
        ] {
            assert!(commands.contains(&expected), "{expected} missing");
        }
        for hidden in [
            "/skills",
            "/thinking",
            "/worktree",
            "/clear",
            "/native status",
            "/agent ",
            "/provider",
        ] {
            assert!(!commands.contains(&hidden), "{hidden} offered");
        }
        for (command, _) in &completions {
            let support = command_support(command.trim_end()).unwrap();
            assert_ne!(support, ManagedCommandSupport::Unavailable, "{command}");
        }
    }

    #[test]
    fn permission_mode_cycle_and_commands() {
        let mut mode = HarnessPermissionMode::Default;
        let mut seen = Vec::new();
        for _ in 0..3 {
            mode = next_permission_mode(mode);
            seen.push(mode);
        }
        assert_eq!(
            seen,
            [
                HarnessPermissionMode::AcceptEdits,
                HarnessPermissionMode::Plan,
                HarnessPermissionMode::Default
            ]
        );
        assert_eq!(
            permission_mode_command(&["/plan"]),
            Ok(HarnessPermissionMode::Plan)
        );
        assert_eq!(
            permission_mode_command(&["/build"]),
            Ok(HarnessPermissionMode::Default)
        );
        assert_eq!(
            permission_mode_command(&["/agent", "accept-edits"]),
            Ok(HarnessPermissionMode::AcceptEdits)
        );
        assert!(permission_mode_command(&["/agent", "review"]).is_err());
    }

    #[test]
    fn native_respond_accepts_json_and_verbs() {
        assert_eq!(
            parse_native_response(r#"{"kind":"approval","approved":true}"#),
            Ok(HarnessResponseValue::Approval { approved: true })
        );
        assert_eq!(
            parse_native_response("approve"),
            Ok(HarnessResponseValue::Approval { approved: true })
        );
        assert_eq!(
            parse_native_response("deny"),
            Ok(HarnessResponseValue::Approval { approved: false })
        );
        assert_eq!(
            parse_native_response("answer b use this one"),
            Ok(HarnessResponseValue::Answer {
                choice_id: Some("b".into()),
                text: "use this one".into()
            })
        );
        assert_eq!(
            parse_native_response("assist  look at the logs "),
            Ok(HarnessResponseValue::Assistance {
                text: "look at the logs".into()
            })
        );
        assert_eq!(
            parse_native_response("cancel"),
            Ok(HarnessResponseValue::Cancel)
        );
        for invalid in [
            "",
            "approve now",
            "answer",
            "assist",
            "true",
            r#"{"kind":"approval"}"#,
        ] {
            assert!(parse_native_response(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn approval_adapter_presents_tool_and_preview_without_remember() {
        let owner = bcode_session_models::SessionId::new();
        let permission =
            approval_permission_view(owner, &view(ExternalInteractionKind::Approval, "req"));
        assert_eq!(permission.tool_name, "Edit");
        assert_eq!(permission.arguments_json, r#"{"file_path":"src/a.rs"}"#);
        assert!(!permission.can_remember);
        assert!(permission.batch.is_none());
        assert_eq!(permission.session_id, Some(owner));
        assert_eq!(permission.title.as_deref(), Some("Which file?"));
        assert!(
            question_surface_request(&view(ExternalInteractionKind::Approval, "req")).is_none()
        );
    }

    #[test]
    fn question_adapter_round_trips_choice_and_custom_text() {
        let question = view(ExternalInteractionKind::Question, "q1");
        let request = question_surface_request(&question).unwrap();
        assert_eq!(
            parse_surface_interaction_id(request.interaction_id()),
            Some((3, "q1".into()))
        );
        let answered = |payload: serde_json::Value| {
            question_response_value(
                &question,
                &bcode_session_models::ToolExchangeResolution::Responded { payload },
            )
        };
        assert_eq!(
            answered(
                serde_json::json!({"status":"answered","questions":[{"question_index":0,"selected":["b"]}]})
            ),
            Some(HarnessResponseValue::Answer {
                choice_id: Some("b".into()),
                text: "src/b.rs".into()
            })
        );
        assert_eq!(
            answered(
                serde_json::json!({"status":"answered","questions":[{"question_index":0,"selected":[],"custom":"other.rs"}]})
            ),
            Some(HarnessResponseValue::Answer {
                choice_id: None,
                text: "other.rs".into()
            })
        );
        assert_eq!(answered(serde_json::json!({"status":"dismissed"})), None);
        assert_eq!(
            answered(
                serde_json::json!({"status":"answered","questions":[{"question_index":0,"selected":["forged"]}]})
            ),
            None
        );
        let assistance = view(ExternalInteractionKind::Assistance, "help");
        let request: serde_json::Value =
            serde_json::from_str(&question_surface_request_json(&assistance)).unwrap();
        assert_eq!(
            request["questions"][0]["options"].as_array().map(Vec::len),
            Some(0)
        );
        assert_eq!(
            question_response_value(
                &assistance,
                &bcode_session_models::ToolExchangeResolution::Responded {
                    payload: serde_json::json!({"status":"answered","questions":[{"question_index":0,"selected":[],"custom":"retry with --verbose"}]})
                }
            ),
            Some(HarnessResponseValue::Assistance {
                text: "retry with --verbose".into()
            })
        );
        assert_eq!(parse_surface_interaction_id("ordinary-exchange"), None);
    }

    fn question_surface_request_json(view: &ExternalInteractionView) -> String {
        let request = question_surface_request(view).unwrap();
        request.request_json_for_test().to_owned()
    }

    #[test]
    fn tracker_reuses_uncertain_identity_and_never_reopens_settled() {
        let pending = vec![
            view(ExternalInteractionKind::Approval, "one"),
            view(ExternalInteractionKind::Question, "two"),
        ];
        let mut tracker = ManagedInteractionTracker::default();
        let first = tracker.next_to_present(&pending).unwrap();
        let key = interaction_key(first);
        tracker.present(key.clone());
        assert!(tracker.next_to_present(&pending).is_none());
        let response_id = tracker.response_id(&key);
        tracker.begin(&key);
        assert!(tracker.is_in_flight(&key));
        tracker.finish(&key, ManagedResponseOutcome::Uncertain);
        // Held after an uncertain outcome: not auto-presented, identity retained for reuse.
        assert_eq!(tracker.next_to_present(&pending).unwrap().request_id, "two");
        assert_eq!(tracker.response_id(&key), response_id);
        tracker.review_again();
        assert_eq!(tracker.next_to_present(&pending).unwrap().request_id, "one");
        tracker.begin(&key);
        tracker.finish(&key, ManagedResponseOutcome::Acknowledged);
        tracker.review_again();
        assert_eq!(tracker.next_to_present(&pending).unwrap().request_id, "two");
        // A refused decision may use a fresh identity.
        let second = interaction_key(&pending[1]);
        let refused_id = tracker.response_id(&second);
        tracker.begin(&second);
        tracker.finish(&second, ManagedResponseOutcome::Refused);
        assert_ne!(tracker.response_id(&second), refused_id);
        // Interactions resolved elsewhere drop all retained state.
        tracker.present(second.clone());
        assert!(tracker.retain_pending(&pending[..1]));
        assert!(tracker.presented().is_none());
    }

    #[test]
    fn response_classification_distinguishes_uncertain_from_refused() {
        let response = |kind| {
            Ok(HarnessServiceResponse {
                schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
                request_id: "r".into(),
                operation: bcode_harness_models::HarnessOperation::Respond,
                kind,
            })
        };
        assert_eq!(
            classify_response(&response(HarnessServiceResponseKind::Acknowledged {
                mutation_id: "r".into()
            })),
            ManagedResponseOutcome::Acknowledged
        );
        assert_eq!(
            classify_response(&response(HarnessServiceResponseKind::Failure(
                HarnessFailure::Conflict
            ))),
            ManagedResponseOutcome::Refused
        );
        assert_eq!(
            classify_response(&response(HarnessServiceResponseKind::Failure(
                HarnessFailure::Uncertain
            ))),
            ManagedResponseOutcome::Uncertain
        );
        assert_eq!(
            classify_response(&Err(bcode_client::ClientError::RequestTimeout {
                timeout: std::time::Duration::from_secs(1)
            })),
            ManagedResponseOutcome::Uncertain
        );
        let stale = bcode_client::ClientError::Server {
            code: "managed_session_stale_owner".into(),
            message: "stale".into(),
        };
        assert!(is_stale_owner(&stale));
        assert_eq!(
            classify_response(&Err(stale)),
            ManagedResponseOutcome::Refused
        );
    }
}
