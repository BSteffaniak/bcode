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

pub enum LoginUpdate {
    Progress(Vec<String>, bool),
    Prompt(AuthFlowEffect),
}

struct LoginChannel {
    updates: mpsc::SyncSender<LoginUpdate>,
    answers: mpsc::Receiver<String>,
}

pub struct DeviceLogin {
    pub prompt: Option<AuthFlowEffect>,
    pub answer: String,
    answers: mpsc::SyncSender<String>,
    pub lines: Vec<String>,
    pub terminal: bool,
    cancel: Arc<AtomicBool>,
    updates: mpsc::Receiver<LoginUpdate>,
}

impl DeviceLogin {
    pub fn start(provider: String, method: String, profile: String, vault: String) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let (send, updates) = mpsc::sync_channel(8);
        let (answers, receive_answers) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let channel = LoginChannel {
                updates: send,
                answers: receive_answers,
            };
            let result = run(
                &provider,
                &method,
                &profile,
                &vault,
                &worker_cancel,
                &channel,
            );
            let message = match result {
                Ok(()) => "Sign-in completed and credentials saved.",
                Err(message) => message,
            };
            let _ = channel
                .updates
                .send(LoginUpdate::Progress(vec![message.to_owned()], true));
        });
        Self {
            prompt: None,
            answer: String::new(),
            answers,
            lines: vec!["Starting sign-in… Esc cancels.".to_owned()],
            terminal: false,
            cancel,
            updates,
        }
    }

    pub fn refresh(&mut self) {
        while let Ok(update) = self.updates.try_recv() {
            if self.terminal {
                continue;
            }
            match update {
                LoginUpdate::Progress(lines, terminal) => {
                    self.lines = lines;
                    self.terminal = terminal;
                    if terminal {
                        self.prompt = None;
                        self.answer.clear();
                    }
                }
                LoginUpdate::Prompt(prompt) => {
                    self.prompt = Some(prompt);
                    self.answer.clear();
                }
            }
        }
    }

    pub fn submit_answer(&mut self) {
        if let Some(prompt) = &self.prompt {
            if prompt.validate_answer(&self.answer).is_err() {
                self.lines = vec!["Choose an offered answer or enter a valid response.".to_owned()];
            } else if self.answers.try_send(self.answer.clone()).is_ok() {
                self.prompt = None;
                self.answer.clear();
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
    send: &LoginChannel,
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
    send: &LoginChannel,
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
    let mut request = begin_request(provider_id, method_id, &prepared.resolved.profile_name);
    let mut browser = super::auth_browser::AuthBrowser::new(
        config
            .onboarding
            .browser_opening_enabled(&bcode_config::ProcessConfigEnvironment),
    );
    let mut domain_flow = bcode_provider_auth::interactive::InteractiveEnrollment::new(
        provider_id.to_owned(),
        method_id.to_owned(),
        prepared.resolved.profile_name.clone(),
    );
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
        let progress = domain_flow.accept(response)?;
        match progress.status {
            AuthFlowStatus::Succeeded => {
                let lifecycle = bcode_provider_auth::lifecycle::AuthVaultLifecycle::new(
                    &prepared.resolved,
                    provider_id,
                    &provider.plugin_id,
                    method,
                )
                .map_err(|_| "Profile ownership mismatch.")?;
                lifecycle.replace_owned(domain_flow.take_credentials()).map_err(|_| "Sign-in succeeded but secure storage failed. Inspect authentication state before retrying.")?;
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
        answer_prompts(&progress.effects, &mut domain_flow, send, cancel)?;
        present_progress(progress.effects, &mut display, &mut browser, send, cancel);
        request = domain_flow.request()?.clone();
    }
}

fn present_progress(
    effects: Vec<AuthFlowEffect>,
    display: &mut Vec<String>,
    browser: &mut super::auth_browser::AuthBrowser,
    send: &LoginChannel,
    cancel: &AtomicBool,
) {
    let wait = adapt_effects(effects, display, browser);
    let _ = send
        .updates
        .try_send(LoginUpdate::Progress(display.clone(), false));
    wait_or_cancel(wait, cancel);
}

fn answer_prompts(
    effects: &[AuthFlowEffect],
    flow: &mut bcode_provider_auth::interactive::InteractiveEnrollment,
    channel: &LoginChannel,
    cancel: &AtomicBool,
) -> Result<(), &'static str> {
    for effect in effects
        .iter()
        .filter(|effect| matches!(effect, AuthFlowEffect::Prompt { .. }))
    {
        channel
            .updates
            .send(LoginUpdate::Prompt(effect.clone()))
            .map_err(|_| "Authentication view closed")?;
        loop {
            if cancel.load(Ordering::Acquire) {
                return Ok(());
            }
            match channel.answers.recv_timeout(Duration::from_millis(100)) {
                Ok(answer) => {
                    flow.answer(effect, answer)?;
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("Authentication view closed");
                }
            }
        }
    }
    Ok(())
}

fn begin_request(provider_id: &str, method_id: &str, profile: &str) -> AuthFlowRequest {
    AuthFlowRequest {
        schema_version: bcode_provider_auth_models::AUTH_FLOW_SCHEMA_VERSION,
        provider_id: provider_id.to_owned(),
        method_id: method_id.to_owned(),
        profile: profile.to_owned(),
        operation: AuthFlowOperation::Begin,
        state: None,
        input: None,
        verify: false,
        revoke: false,
    }
}

fn open_effect_urls(
    effects: &[AuthFlowEffect],
    browser: &mut super::auth_browser::AuthBrowser,
) -> bool {
    effects
        .iter()
        .filter_map(|effect| match effect {
            AuthFlowEffect::OpenBrowser { url } => Some(url.as_str()),
            AuthFlowEffect::DisplayDeviceCode {
                verification_url, ..
            } => Some(verification_url.as_str()),
            _ => None,
        })
        .map(|url| !browser.open(url))
        .collect::<Vec<_>>()
        .into_iter()
        .any(|failed| failed)
}

fn present_effects(effects: Vec<AuthFlowEffect>, display: &mut Vec<String>) -> u64 {
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
            AuthFlowEffect::Prompt { .. } => {}
        }
    }
    wait
}

fn wait_or_cancel(wait: u64, cancel: &AtomicBool) {
    for _ in 0..wait.div_ceil(100) {
        if cancel.load(Ordering::Acquire) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn adapt_effects(
    effects: Vec<AuthFlowEffect>,
    display: &mut Vec<String>,
    browser: &mut super::auth_browser::AuthBrowser,
) -> u64 {
    let browser_failed = open_effect_urls(&effects, browser);
    let wait = present_effects(effects, display);
    if browser_failed {
        display.push(
            "Browser could not open. Copy the URL above; sign-in is still waiting.".to_owned(),
        );
    }
    wait
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_progress_cannot_be_reopened_by_late_updates() {
        let (send, updates) = mpsc::channel();
        let (answers, _) = mpsc::sync_channel(1);
        let mut login = DeviceLogin {
            prompt: None,
            answer: String::new(),
            answers,
            lines: Vec::new(),
            terminal: false,
            cancel: Arc::new(AtomicBool::new(false)),
            updates,
        };
        send.send(LoginUpdate::Progress(vec!["Completed".to_owned()], true))
            .unwrap();
        send.send(LoginUpdate::Progress(vec!["Pending".to_owned()], false))
            .unwrap();
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
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "https://example.com/verify")
        );
        assert!(wait > 0);
    }

    #[test]
    fn prompt_answer_is_validated_and_delivered_without_stdin() {
        let (send, updates) = mpsc::channel();
        let (answers, received) = mpsc::sync_channel(1);
        let mut login = DeviceLogin {
            prompt: None,
            answer: String::new(),
            answers,
            lines: Vec::new(),
            terminal: false,
            cancel: Arc::new(AtomicBool::new(false)),
            updates,
        };
        send.send(LoginUpdate::Prompt(AuthFlowEffect::Prompt {
            prompt_id: "account".to_owned(),
            message: "Choose account".to_owned(),
            choices: vec!["work".to_owned(), "personal".to_owned()],
        }))
        .unwrap();
        login.refresh();
        login.answer = "invalid".to_owned();
        login.submit_answer();
        assert!(received.try_recv().is_err());
        assert!(login.prompt.is_some());
        login.answer = "work".to_owned();
        login.submit_answer();
        assert_eq!(received.try_recv().unwrap(), "work");
        assert!(login.prompt.is_none());
    }

    #[test]
    fn cancellation_interrupts_provider_wait() {
        let cancel = AtomicBool::new(true);
        let start = std::time::Instant::now();
        wait_or_cancel(300_000, &cancel);
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
