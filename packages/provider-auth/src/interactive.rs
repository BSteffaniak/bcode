//! Renderer-neutral state transitions for provider-owned interactive authentication.
//! Opaque continuation state and terminal credentials never enter presentation snapshots.

use bcode_provider_auth_models::{
    AUTH_FLOW_SCHEMA_VERSION, AuthFlowEffect, AuthFlowInput, AuthFlowOperation, AuthFlowRequest,
    AuthFlowResponse, AuthFlowStatus,
};
use std::collections::BTreeMap;
use zeroize::Zeroize as _;

/// Validated public progress from one authentication step.
pub struct FlowProgress {
    /// Authoritative terminal or pending state.
    pub status: AuthFlowStatus,
    /// Normalized effects; never includes opaque continuation state or credentials.
    pub effects: Vec<AuthFlowEffect>,
}

/// Domain-owned authentication continuation and terminal fencing.
pub struct InteractiveEnrollment {
    request: AuthFlowRequest,
    terminal: bool,
    credentials: BTreeMap<String, String>,
}

impl InteractiveEnrollment {
    /// Start a provider-selected method with no implicit verification/revocation.
    #[must_use]
    pub const fn new(provider: String, method: String, profile: String) -> Self {
        Self {
            request: AuthFlowRequest {
                schema_version: AUTH_FLOW_SCHEMA_VERSION,
                provider_id: provider,
                method_id: method,
                profile,
                operation: AuthFlowOperation::Begin,
                state: None,
                input: None,
                verify: false,
                revoke: false,
            },
            terminal: false,
            credentials: BTreeMap::new(),
        }
    }

    /// Request for the next provider invocation.
    ///
    /// # Errors
    /// Returns an error after terminal completion.
    pub const fn request(&self) -> Result<&AuthFlowRequest, &'static str> {
        if self.terminal {
            Err("Authentication is already terminal")
        } else {
            Ok(&self.request)
        }
    }

    /// Validate and accept a response, fencing terminal outcomes.
    ///
    /// # Errors
    /// Returns an error for invalid responses or updates after terminal completion.
    pub fn accept(&mut self, mut response: AuthFlowResponse) -> Result<FlowProgress, &'static str> {
        if self.terminal {
            clear_credentials(&mut response.credentials);
            return Err("Authentication is already terminal");
        }
        if response.validate().is_err() {
            clear_credentials(&mut response.credentials);
            return Err("Invalid authentication response");
        }
        self.terminal = response.status != AuthFlowStatus::Pending;
        if self.request.operation == AuthFlowOperation::Cancel {
            clear_credentials(&mut response.credentials);
            self.terminal = true;
            return Ok(FlowProgress {
                status: AuthFlowStatus::Cancelled,
                effects: Vec::new(),
            });
        }
        self.request.operation = AuthFlowOperation::Continue;
        self.request.state = response.state.take();
        self.request.input = None;
        if response.status == AuthFlowStatus::Succeeded {
            self.credentials = std::mem::take(&mut response.credentials);
        } else {
            clear_credentials(&mut response.credentials);
        }
        Ok(FlowProgress {
            status: response.status,
            effects: if response.status == AuthFlowStatus::Pending {
                response.effects
            } else {
                Vec::new()
            },
        })
    }

    /// Submit an answer only to a prompt that accepts it.
    ///
    /// # Errors
    /// Returns an error for a terminal flow, non-prompt effect, or invalid answer.
    pub fn answer(&mut self, prompt: &AuthFlowEffect, value: String) -> Result<(), &'static str> {
        if self.terminal {
            return Err("Authentication is already terminal");
        }
        let AuthFlowEffect::Prompt { prompt_id, .. } = prompt else {
            return Err("Not an authentication prompt");
        };
        prompt
            .validate_answer(&value)
            .map_err(|_| "Invalid authentication answer")?;
        self.request.input = Some(AuthFlowInput {
            prompt_id: prompt_id.clone(),
            value,
        });
        Ok(())
    }

    /// Request provider cleanup; later success cannot authorize credential publication.
    pub fn cancel(&mut self) {
        if !self.terminal {
            self.request.operation = AuthFlowOperation::Cancel;
            self.request.input = None;
        }
    }

    /// Transfer successful terminal credentials to the secure custody operation.
    pub fn take_credentials(&mut self) -> BTreeMap<String, String> {
        std::mem::take(&mut self.credentials)
    }
}

fn clear_credentials(values: &mut BTreeMap<String, String>) {
    for value in values.values_mut() {
        value.zeroize();
    }
    values.clear();
}
impl Drop for InteractiveEnrollment {
    fn drop(&mut self) {
        clear_credentials(&mut self.credentials);
        if let Some(state) = &mut self.request.state {
            state.zeroize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn success() -> AuthFlowResponse {
        AuthFlowResponse {
            schema_version: AUTH_FLOW_SCHEMA_VERSION,
            status: AuthFlowStatus::Succeeded,
            state: None,
            effects: Vec::new(),
            credentials: BTreeMap::from([("token".to_owned(), "test-secret".to_owned())]),
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn terminal_outcomes_and_cancel_fence_credentials() {
        let mut flow = InteractiveEnrollment::new(
            "provider".to_owned(),
            "arbitrary-method".to_owned(),
            "account".to_owned(),
        );
        flow.cancel();
        assert_eq!(
            flow.accept(success()).unwrap().status,
            AuthFlowStatus::Cancelled
        );
        assert!(flow.take_credentials().is_empty());
        assert!(flow.accept(success()).is_err());
        assert!(flow.request().is_err());
    }

    #[test]
    fn successful_credentials_transfer_once() {
        let mut flow = InteractiveEnrollment::new(
            "another-provider".to_owned(),
            "organization-login".to_owned(),
            "account".to_owned(),
        );
        assert_eq!(
            flow.accept(success()).unwrap().status,
            AuthFlowStatus::Succeeded
        );
        assert_eq!(flow.take_credentials().len(), 1);
        assert!(flow.take_credentials().is_empty());
    }
}
