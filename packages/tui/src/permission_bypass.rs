//! Stepping the attached session into or out of permission bypass at runtime (`/yolo`).
//!
//! * **The session's mode is the daemon's.** The app shows the mode the daemon reports for
//!   the attached session (read when it attaches, then from live events) and turns sent from
//!   here carry it. A launch with `--yolo` steps each session it creates or opens into bypass
//!   once; stepping out of one is not undone by switching back to it.
//! * **Confirmation.** The first step into bypass of a launch asks first (`/yolo` again
//!   confirms) unless the user chose `/yolo always`, which is remembered in the state
//!   directory ([`bcode_config::bypass_confirmation`]).
//! * **Waiting prompts.** Stepping in does what `permissions.bypass_waiting_prompts` says with
//!   the prompts the session already waits on: approve them once each, offer to (open the
//!   permission review), or leave them.

use std::collections::BTreeSet;

use bcode_client::ClientError;
use bcode_config::permissions::BypassWaitingPrompts;
use bcode_session_models::{SessionId, TurnPermissionMode};

use super::chat_loop::TuiRuntimeSettings;
use super::effects::TuiEffect;
use super::session_flow::ActiveChat;

/// What `/yolo` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BypassRequest {
    /// Step in (`true`), out (`false`), or toggle (`None`).
    pub on: Option<bool>,
    /// Stop asking before stepping in, from now on.
    pub dont_ask_again: bool,
}

impl BypassRequest {
    /// Read `/yolo [on|off|always]`'s arguments; `None` for anything else.
    #[must_use]
    pub fn parse(arguments: &[&str]) -> Option<Self> {
        let (on, dont_ask_again) = match arguments {
            [] => (None, false),
            ["on"] => (Some(true), false),
            ["off"] => (Some(false), false),
            ["always"] => (Some(true), true),
            _ => return None,
        };
        Some(Self { on, dont_ask_again })
    }
}

/// Usage of `/yolo`.
pub const USAGE: &str = "Usage: /yolo [on|off|always]";

/// What this launch knows about stepping sessions into bypass.
#[derive(Debug, Default)]
pub struct BypassLaunchState {
    /// Stepping into bypass was confirmed during this launch.
    confirmed: bool,
    /// The confirmation was shown; the next step in confirms it.
    asked: bool,
    /// Sessions this launch stepped into bypass because it started with `--yolo`.
    started: BTreeSet<SessionId>,
    /// The session whose mode the app was last brought up to date with.
    synced: Option<SessionId>,
    /// Where the choice not to be asked again is kept; the state directory's when `None`.
    confirmation_path: Option<std::path::PathBuf>,
}

impl BypassLaunchState {
    /// Keep the choice not to be asked again at `path`.
    #[cfg(test)]
    pub fn with_confirmation_path(path: std::path::PathBuf) -> Self {
        Self {
            confirmation_path: Some(path),
            ..Self::default()
        }
    }

    fn confirmation_path(&self) -> std::path::PathBuf {
        self.confirmation_path
            .clone()
            .unwrap_or_else(bcode_config::bypass_confirmation::default_path)
    }

    /// Whether the user chose not to be asked before stepping in.
    fn dismissed(&self) -> bool {
        bcode_config::bypass_confirmation::load_from(&self.confirmation_path())
            .is_ok_and(|state| state.dismissed)
    }

    /// Read the attached session's mode again on the next pass (after a resync).
    pub const fn resynced(&mut self) {
        self.synced = None;
    }
}

/// Bring the app up to date with the attached session's mode when the attached session
/// changed: step it into bypass when this launch started with `--yolo` and never did so for
/// it, else read it.
pub fn reconcile(
    state: &mut BypassLaunchState,
    chat: &mut ActiveChat,
    settings: &TuiRuntimeSettings,
) {
    let Some(session_id) = chat.attached_session_id() else {
        state.synced = None;
        return;
    };
    if state.synced == Some(session_id) {
        return;
    }
    state.synced = Some(session_id);
    let set = (settings.launch_options().permission_mode == TurnPermissionMode::Bypass
        && state.started.insert(session_id))
    .then_some(TurnPermissionMode::Bypass);
    chat.start_effect(TuiEffect::PermissionMode { session_id, set });
}

/// Handle `/yolo`: step the attached session out, or in after the launch's confirmation.
pub fn request(state: &mut BypassLaunchState, chat: &mut ActiveChat, request: BypassRequest) {
    let Some(session_id) = chat.attached_session_id() else {
        chat.app.set_status(
            "Permission bypass applies to a session: send a message first, then /yolo".to_owned(),
        );
        return;
    };
    let on = request
        .on
        .unwrap_or_else(|| chat.app.permission_mode() != TurnPermissionMode::Bypass);
    if !on {
        state.asked = false;
        chat.start_effect(TuiEffect::PermissionMode {
            session_id,
            set: Some(TurnPermissionMode::Enforce),
        });
        return;
    }
    if request.dont_ask_again {
        if let Err(error) =
            bcode_config::bypass_confirmation::dismiss_at(&state.confirmation_path())
        {
            chat.app
                .set_status(format!("could not remember not to ask again: {error}"));
        }
        state.confirmed = true;
    }
    if !state.confirmed && !state.dismissed() {
        if !state.asked {
            state.asked = true;
            chat.app
                .push_ephemeral_system_markdown(CONFIRMATION.to_owned());
            chat.app.set_status(
                "Permission bypass not on yet: /yolo again to step in, /yolo always to stop asking"
                    .to_owned(),
            );
            return;
        }
        state.confirmed = true;
    }
    state.asked = false;
    chat.start_effect(TuiEffect::PermissionMode {
        session_id,
        set: Some(TurnPermissionMode::Bypass),
    });
}

/// What the confirmation says.
const CONFIRMATION: &str = "**Permission bypass** lets this session's tools run without \
asking: commands, file writes and edits, and every other tool its agent offers, until you step \
out with `/yolo off`. Path confinement, read-only limits and tool availability still apply.\n\n\
Run `/yolo` again to step in, or `/yolo always` to step in and not be asked again.";

/// Apply a read or set of the session's mode.
pub fn apply_result(
    chat: &mut ActiveChat,
    settings: &TuiRuntimeSettings,
    session_id: SessionId,
    set: Option<TurnPermissionMode>,
    result: Result<TurnPermissionMode, ClientError>,
) {
    let mode = match result {
        Ok(mode) => mode,
        Err(error) => {
            if set.is_some() {
                chat.app.push_ephemeral_system_markdown(format!(
                    "Permission bypass unavailable: {error}"
                ));
            }
            return;
        }
    };
    if chat.attached_session_id() != Some(session_id) {
        return;
    }
    chat.app.set_session_permission_mode(mode);
    match set {
        Some(TurnPermissionMode::Bypass) => {
            chat.app.set_status(
                "Permission bypass on for this session: tools run without asking (/yolo off)"
                    .to_owned(),
            );
            waiting_prompts(chat, settings.permissions().bypass_waiting_prompts);
        }
        Some(TurnPermissionMode::Enforce) => {
            chat.app
                .set_status("Permission bypass off: tools ask again".to_owned());
        }
        None => {}
    }
}

/// What to do with the prompts the session already waits on after stepping in.
fn waiting_prompts(chat: &mut ActiveChat, behavior: BypassWaitingPrompts) {
    let waiting = chat.app.pending_permission_ids();
    if waiting.is_empty() {
        return;
    }
    match behavior {
        BypassWaitingPrompts::Approve => {
            for permission_id in waiting {
                chat.start_effect(TuiEffect::ResolvePermission {
                    permission_id,
                    approved: true,
                    remember: false,
                    apply_to_batch: false,
                    batch_id: None,
                });
            }
        }
        BypassWaitingPrompts::Ask => {
            chat.app.set_status(format!(
                "Permission bypass on; {} prompt(s) already waiting — review them",
                waiting.len()
            ));
            chat.replace_effect(TuiEffect::ListPermissions);
        }
        BypassWaitingPrompts::Leave => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yolo_arguments_toggle_or_say_which_way() {
        assert_eq!(
            BypassRequest::parse(&[]),
            Some(BypassRequest {
                on: None,
                dont_ask_again: false
            })
        );
        assert_eq!(
            BypassRequest::parse(&["on"]),
            Some(BypassRequest {
                on: Some(true),
                dont_ask_again: false
            })
        );
        assert_eq!(
            BypassRequest::parse(&["off"]),
            Some(BypassRequest {
                on: Some(false),
                dont_ask_again: false
            })
        );
        assert_eq!(
            BypassRequest::parse(&["always"]),
            Some(BypassRequest {
                on: Some(true),
                dont_ask_again: true
            })
        );
        assert_eq!(BypassRequest::parse(&["maybe"]), None);
    }
}
