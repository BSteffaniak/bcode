//! Dedicated ephemeral auth-surface state, separate from chat/composer/history.
//! Rendering must use existing hidden-input BMUX components; this module does not
//! implement terminal measurement, painting, clipboard, or transcript integration.

use bcode_provider_auth_models::managed::{
    AuthSensitiveInput, InvalidSensitiveInput, ManagedAuthHandle, ManagedFlowPhase,
    ManagedFlowSnapshot, SensitiveAuthInteraction,
};
use zeroize::Zeroize as _;

/// Client-local state for one explicitly selected live daemon authentication flow.
/// Never serialize this object or include it in debug/session snapshots.
#[derive(Default)]
pub struct HarnessAuthSurface {
    session_id: Option<String>,
    handle: Option<ManagedAuthHandle>,
    status: Option<ManagedFlowSnapshot>,
    interaction: Option<SensitiveAuthInteraction>,
    hidden_input: String,
    submitted_request: Option<String>,
}

impl HarnessAuthSurface {
    /// Select an explicitly authorized session/flow; selection changes clear secrets.
    pub fn select(&mut self, session_id: String, handle: ManagedAuthHandle) {
        if self.session_id.as_ref() != Some(&session_id) || self.handle.as_ref() != Some(&handle) {
            self.clear();
            self.session_id = Some(session_id);
            self.handle = Some(handle);
        }
    }

    /// Accept only the selected live response. Late updates cannot reopen a terminal
    /// flow. Readiness is not remote verification and auth is not session settlement.
    #[must_use]
    pub fn update_status(&mut self, session_id: &str, status: ManagedFlowSnapshot) -> bool {
        if self.session_id.as_deref() != Some(session_id)
            || self.handle.as_ref() != Some(&status.handle)
            || self.status.as_ref().is_some_and(|previous| {
                previous.phase == ManagedFlowPhase::Settled
                    || status.revision < previous.revision
                    || (previous.phase == ManagedFlowPhase::Uncertain
                        && status.phase != ManagedFlowPhase::Settled)
            })
        {
            return false;
        }
        if !status.needs_sensitive_interaction
            || matches!(
                status.phase,
                ManagedFlowPhase::Settled
                    | ManagedFlowPhase::Uncertain
                    | ManagedFlowPhase::Cancelling
            )
        {
            self.clear_sensitive();
        }
        self.status = Some(status);
        true
    }

    /// Accept only dedicated auth-channel material, never a normal question/prompt.
    /// No automatic browser launch, clipboard copy, or shell execution is performed.
    #[must_use]
    pub fn update_interaction(
        &mut self,
        session_id: &str,
        handle: &ManagedAuthHandle,
        revision: u64,
        interaction: SensitiveAuthInteraction,
    ) -> bool {
        if self.session_id.as_deref() != Some(session_id)
            || self.handle.as_ref() != Some(handle)
            || !interaction.is_valid()
            || !self.status.as_ref().is_some_and(|status| {
                status.phase == ManagedFlowPhase::Pending
                    && status.needs_sensitive_interaction
                    && status.revision == revision
            })
            || self.submitted_request.as_deref() == Some(interaction.request_id())
        {
            return false;
        }
        if self.interaction.as_ref() == Some(&interaction) {
            return true;
        }
        self.clear_sensitive();
        self.interaction = Some(interaction);
        true
    }

    /// Borrow only for the dedicated auth modal, not normal transcript rendering.
    #[must_use]
    pub const fn interaction(&self) -> Option<&SensitiveAuthInteraction> {
        self.interaction.as_ref()
    }

    /// Push one character from a hidden, non-history input component. Do not feed
    /// this through command history, paste logging, chat submission or model context.
    #[must_use]
    pub fn push_hidden(&mut self, character: char) -> bool {
        if !matches!(
            self.interaction,
            Some(SensitiveAuthInteraction::Input { .. })
        ) || character.is_control()
            || self.hidden_input.len() + character.len_utf8() > 16384
        {
            return false;
        }
        self.hidden_input.push(character);
        true
    }

    /// Clear entry on cancellation, modal dismissal or input reset.
    pub fn clear_hidden(&mut self) {
        self.hidden_input.zeroize();
        self.hidden_input.clear();
    }

    /// Move the input to one trace-exempt auth continuation request. Never retries;
    /// a lost response requires status inspection, not resending callback material.
    /// # Errors
    /// Rejects missing interaction or invalid/empty hidden input.
    pub fn take_input(&mut self) -> Result<AuthSensitiveInput, InvalidSensitiveInput> {
        let Some(SensitiveAuthInteraction::Input { request_id }) = &self.interaction else {
            return Err(InvalidSensitiveInput);
        };
        let request_id = request_id.clone();
        let input =
            AuthSensitiveInput::new(request_id.clone(), std::mem::take(&mut self.hidden_input))?;
        self.submitted_request = Some(request_id);
        self.interaction = None;
        Ok(input)
    }

    /// A disconnect is not native cancellation. Drop all sensitive local material;
    /// retain only correlation so reconnect can request fresh live status. Never replay.
    pub fn disconnected(&mut self) {
        self.clear_sensitive();
        self.status = None;
    }

    /// Explicit cancel/session switch drops local state; server cancellation must go
    /// through its separately authorized waiter-cancellation application operation.
    pub fn clear(&mut self) {
        self.clear_sensitive();
        self.handle = None;
        self.session_id = None;
        self.status = None;
        self.submitted_request = None;
    }

    fn clear_sensitive(&mut self) {
        self.clear_hidden();
        self.interaction = None;
        // Keep the last submitted request until flow selection changes or terminal
        // observation; reconnect must not re-enable one-shot callback submission.
    }
}

impl Drop for HarnessAuthSurface {
    fn drop(&mut self) {
        self.clear_hidden();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_provider_auth_models::managed::AuthReadiness;

    fn handle() -> ManagedAuthHandle {
        ManagedAuthHandle {
            daemon_epoch: "boot".into(),
            flow_id: 1,
        }
    }

    fn status(phase: ManagedFlowPhase) -> ManagedFlowSnapshot {
        ManagedFlowSnapshot {
            handle: handle(),
            revision: 1,
            phase,
            readiness: AuthReadiness::LoginRequired,
            needs_sensitive_interaction: true,
        }
    }

    #[test]
    fn hidden_input_is_single_use_and_reconnect_does_not_replay() {
        let mut surface = HarnessAuthSurface::default();
        surface.select("session".into(), handle());
        assert!(surface.update_status("session", status(ManagedFlowPhase::Pending)));
        assert!(surface.update_interaction(
            "session",
            &handle(),
            1,
            SensitiveAuthInteraction::Input {
                request_id: "one".into()
            }
        ));
        assert!(surface.push_hidden('x'));
        let input = surface.take_input().unwrap();
        assert_eq!(input.expose_for_transport(), "x");
        assert!(surface.take_input().is_err());
        surface.disconnected();
        assert!(surface.interaction().is_none());
        assert!(surface.update_status("session", status(ManagedFlowPhase::Pending)));
        assert!(!surface.update_interaction(
            "session",
            &handle(),
            1,
            SensitiveAuthInteraction::Input {
                request_id: "one".into()
            }
        ));
    }

    #[test]
    fn identical_live_interaction_delivery_preserves_hidden_entry() {
        let mut surface = HarnessAuthSurface::default();
        surface.select("session".into(), handle());
        assert!(surface.update_status("session", status(ManagedFlowPhase::Pending)));
        let input = || SensitiveAuthInteraction::Input {
            request_id: "one".into(),
        };
        assert!(surface.update_interaction("session", &handle(), 1, input()));
        assert!(surface.push_hidden('x'));
        assert!(surface.update_interaction("session", &handle(), 1, input()));
        assert_eq!(surface.take_input().unwrap().expose_for_transport(), "x");
    }

    #[test]
    fn late_or_wrong_session_material_cannot_reopen_terminal_auth() {
        let mut surface = HarnessAuthSurface::default();
        surface.select("session".into(), handle());
        assert!(!surface.update_status("other", status(ManagedFlowPhase::Pending)));
        assert!(surface.update_status("session", status(ManagedFlowPhase::Settled)));
        assert!(!surface.update_status("session", status(ManagedFlowPhase::Pending)));
        assert!(!surface.update_interaction(
            "session",
            &handle(),
            1,
            SensitiveAuthInteraction::Input {
                request_id: "one".into()
            }
        ));
        assert!(!surface.push_hidden('x'));
    }
}
