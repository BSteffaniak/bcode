//! Background device authentication. Only normalized effects cross into presentation.

use bcode_provider_auth_models::{
    AuthFlowEffect, AuthFlowOperation, AuthFlowRequest, AuthFlowResponse, AuthFlowStatus,
    AuthMethodContribution,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

pub struct DeviceLogin {
    pub lines: Vec<String>,
    pub terminal: bool,
    cancel: Arc<AtomicBool>,
    updates: mpsc::Receiver<(Vec<String>, bool)>,
}

impl DeviceLogin {
    pub fn start(provider: String, method: String, profile: String, vault: String) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let (send, updates) = mpsc::sync_channel(8);
        std::thread::spawn(move || {
            let result = run(&provider, &method, &profile, &vault, &worker_cancel, &send);
            let message = match result {
                Ok(()) => "Sign-in completed and credentials saved.",
                Err(message) => message,
            };
            let _ = send.send((vec![message.to_owned()], true));
        });
        Self {
            lines: vec!["Starting sign-in… Esc cancels.".to_owned()],
            terminal: false,
            cancel,
            updates,
        }
    }

    pub fn refresh(&mut self) {
        while let Ok((lines, terminal)) = self.updates.try_recv() {
            if !self.terminal {
                self.lines = lines;
                self.terminal = terminal;
            }
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
}
impl Drop for DeviceLogin {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn run(
    provider_id: &str,
    method_id: &str,
    profile: &str,
    vault: &str,
    cancel: &AtomicBool,
    send: &mpsc::SyncSender<(Vec<String>, bool)>,
) -> Result<(), &'static str> {
    let config = bcode_config::load_config().map_err(|_| "Cannot load configuration.")?;
    let selection =
        bcode_config::plugin_selection_with_default_plugin_ids(&config, std::iter::empty::<&str>());
    let mut host = bcode_plugin::PluginHost::load_defaults_with_static_bundled(
        &selection,
        &super::static_bundled_plugins(),
    )
    .map_err(|_| "Cannot load authentication providers.")?;
    let result = flow(&host, provider_id, method_id, profile, vault, cancel, send);
    let cleanup = host
        .deactivate_all()
        .map_err(|_| "Authentication provider cleanup failed.");
    result.and(cleanup)
}

fn flow(
    host: &bcode_plugin::PluginHost,
    provider_id: &str,
    method_id: &str,
    profile: &str,
    vault: &str,
    cancel: &AtomicBool,
    send: &mpsc::SyncSender<(Vec<String>, bool)>,
) -> Result<(), &'static str> {
    let config = bcode_config::load_config().map_err(|_| "Cannot load configuration.")?;
    let provider = host
        .auth_provider_registry()
        .get(provider_id)
        .ok_or("Unknown authentication provider.")?;
    let method = provider
        .contribution
        .methods
        .iter()
        .find(|method| method.method_id() == method_id)
        .ok_or("Unknown authentication method.")?;
    let AuthMethodContribution::Interactive { operation, .. } = method else {
        return Err("Select a device/browser authentication method.");
    };
    let prepared = bcode_provider_auth::enrollment::prepare(
        &config,
        &bcode_config::load_runtime_auth_subscriptions(),
        &provider.contribution,
        &provider.plugin_id,
        method_id,
        bcode_provider_auth::enrollment::EnrollmentDestination {
            profile: (!profile.is_empty()).then(|| profile.to_owned()),
            vault: (!vault.is_empty()).then(|| vault.into()),
            recipient_key: None,
        },
    )
    .map_err(|_| "Authentication profile is inconsistent.")?;
    let mut request = AuthFlowRequest {
        schema_version: bcode_provider_auth_models::AUTH_FLOW_SCHEMA_VERSION,
        provider_id: provider_id.to_owned(),
        method_id: method_id.to_owned(),
        profile: prepared.resolved.profile_name.clone(),
        operation: AuthFlowOperation::Begin,
        state: None,
        input: None,
        verify: false,
        revoke: false,
    };
    let mut display = vec!["Waiting for sign-in… Esc cancels.".to_owned()];
    loop {
        if cancel.load(Ordering::Acquire) {
            request.operation = AuthFlowOperation::Cancel;
        }
        request
            .validate()
            .map_err(|_| "Invalid authentication request.")?;
        let response: AuthFlowResponse = host
            .invoke_service_json(
                &provider.plugin_id,
                bcode_provider_auth_models::AUTH_INTERFACE_ID,
                operation,
                &request,
            )
            .map_err(|_| "Sign-in request failed. Retry when connected.")?;
        response
            .validate()
            .map_err(|_| "Provider returned an invalid authentication response.")?;
        if cancel.load(Ordering::Acquire) {
            if request.operation != AuthFlowOperation::Cancel {
                request.operation = AuthFlowOperation::Cancel;
                request.state = response.state;
                let _ = host.invoke_service_json::<_, AuthFlowResponse>(
                    &provider.plugin_id,
                    bcode_provider_auth_models::AUTH_INTERFACE_ID,
                    operation,
                    &request,
                );
            }
            return Err("Sign-in cancelled; credentials were not saved.");
        }
        match response.status {
            AuthFlowStatus::Succeeded => {
                let lifecycle = bcode_provider_auth::lifecycle::AuthVaultLifecycle::new(
                    &prepared.resolved,
                    provider_id,
                    &provider.plugin_id,
                    method,
                )
                .map_err(|_| "Profile ownership mismatch.")?;
                lifecycle.replace_owned(response.credentials).map_err(|_| "Sign-in succeeded but secure storage failed. Inspect authentication state before retrying.")?;
                if prepared.publish_runtime {
                    bcode_provider_auth::enrollment::publish(&prepared.resolved)
                        .map_err(|_| "Credentials saved but profile publication failed.")?;
                }
                return Ok(());
            }
            AuthFlowStatus::Failed => {
                return Err("Sign-in failed or expired. Close this panel and retry.");
            }
            AuthFlowStatus::Cancelled => return Err("Sign-in cancelled."),
            AuthFlowStatus::Pending => {}
        }
        let wait = present_effects(response.effects, &mut display)?;
        let _ = send.try_send((display.clone(), false));
        wait_or_cancel(wait, cancel);
        request.operation = AuthFlowOperation::Continue;
        request.state = response.state;
    }
}

fn present_effects(
    effects: Vec<AuthFlowEffect>,
    display: &mut Vec<String>,
) -> Result<u64, &'static str> {
    let mut wait = 1000;
    for effect in effects {
        match effect {
            AuthFlowEffect::DisplayDeviceCode {
                verification_url,
                user_code,
                ..
            } => {
                *display = vec![
                    "Open this URL in your browser:".to_owned(),
                    verification_url,
                    format!("Device code: {user_code}"),
                    "Waiting for authorization… Esc cancels.".to_owned(),
                ];
            }
            AuthFlowEffect::OpenBrowser { url } => {
                *display = vec![
                    "Open this URL in your browser:".to_owned(),
                    url,
                    "Waiting for authorization… Esc cancels.".to_owned(),
                ];
            }
            AuthFlowEffect::Wait { millis } => wait = millis.max(100),
            AuthFlowEffect::Message { message } => {
                if display.len() < 8 {
                    display.push(message);
                }
            }
            AuthFlowEffect::Prompt { .. } => {
                return Err(
                    "This method requires an additional prompt. Use the device-code method.",
                );
            }
        }
    }
    Ok(wait)
}

fn wait_or_cancel(wait: u64, cancel: &AtomicBool) {
    for _ in 0..wait.div_ceil(100) {
        if cancel.load(Ordering::Acquire) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_progress_cannot_be_reopened_by_late_updates() {
        let (send, updates) = mpsc::channel();
        let mut login = DeviceLogin {
            lines: Vec::new(),
            terminal: false,
            cancel: Arc::new(AtomicBool::new(false)),
            updates,
        };
        send.send((vec!["Completed".to_owned()], true)).unwrap();
        send.send((vec!["Pending".to_owned()], false)).unwrap();
        login.refresh();
        assert!(login.terminal);
        assert_eq!(login.lines, vec!["Completed"]);
    }

    #[test]
    fn browser_effect_is_presented_without_terminal_handoff() {
        let mut lines = Vec::new();
        let wait = present_effects(
            vec![AuthFlowEffect::OpenBrowser {
                url: "https://example.com/verify".to_owned(),
            }],
            &mut lines,
        )
        .unwrap();
        assert!(
            lines
                .iter()
                .any(|line| line == "https://example.com/verify")
        );
        assert!(wait > 0);
    }

    #[test]
    fn cancellation_interrupts_provider_wait() {
        let cancel = AtomicBool::new(true);
        let start = std::time::Instant::now();
        wait_or_cancel(300_000, &cancel);
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
