//! Secret-safe identities and observations for actual provider attempts.
use super::{AuthSettings, ModelTurnRequest, Settings, StreamOutcome, TurnState};
use bcode_model::{
    ModelAvailabilityObservation, ModelCatalogSupportHint, ModelCheckReason, ModelCheckScope,
    ModelCheckStatus, ProviderError, ProviderTurnEvent,
};
use sha2::{Digest as _, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};

static ATTEMPT_ORDINAL: AtomicU64 = AtomicU64::new(0);

fn digest(domain: &str, values: &[&str]) -> String {
    use std::fmt::Write as _;
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    for value in values {
        hasher.update([0]);
        hasher.update(value.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
            out
        })
}

pub fn scope(settings: &Settings, profile: Option<&str>) -> Option<ModelCheckScope> {
    let provider = super::catalog_provider_id(settings)?;
    let (auth_mode, account) = match &settings.auth {
        AuthSettings::Missing => return None,
        AuthSettings::ApiKey(key) => (
            "api_key",
            digest("bcode.model-account.credential.v1", &[key]),
        ),
        AuthSettings::ChatGpt {
            account_id: Some(account),
            ..
        } => (
            "chatgpt_subscription",
            digest("bcode.model-account.identity.v1", &[account]),
        ),
        AuthSettings::ChatGpt { access_token, .. } => (
            "chatgpt_subscription",
            digest("bcode.model-account.credential.v1", &[access_token]),
        ),
    };
    let endpoint = if matches!(settings.auth, AuthSettings::ChatGpt { .. }) {
        super::codex_backend_base_and_style(settings).0
    } else {
        settings.base_url.clone()
    };
    let mut endpoint = reqwest::Url::parse(&endpoint).ok()?;
    let _ = endpoint.set_username("");
    let _ = endpoint.set_password(None);
    endpoint.set_fragment(None);
    // Query routing can affect serving identity. Digest it, never expose it.
    let api_surface = if matches!(settings.auth, AuthSettings::ChatGpt { .. }) {
        "chatgpt_codex"
    } else {
        settings.dialect.metadata_value()
    };
    Some(ModelCheckScope {
        target: ModelCatalogSupportHint {
            provider: provider.to_owned(),
            auth_mode: auth_mode.to_owned(),
            api_surface: api_surface.to_owned(),
            integration: Some("bcode".to_owned()),
        },
        profile: profile.map(str::to_owned),
        endpoint_fingerprint: digest("bcode.model-endpoint.v1", &[endpoint.as_str(), api_surface]),
        account_fingerprint: account,
    })
}

pub fn admission() -> (u64, u64) {
    (now_ms(), ATTEMPT_ORDINAL.fetch_add(1, Ordering::Relaxed))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

pub fn observe(
    request: &ModelTurnRequest,
    turn: &TurnState,
    admission: (u64, u64),
    outcome: &Result<StreamOutcome, ProviderError>,
) {
    let settings = super::settings_for_context(&request.provider_context);
    let model_id = if request.model_id.is_empty() {
        settings.default_model.clone().unwrap_or_default()
    } else {
        request.model_id.clone()
    };
    if model_id.is_empty() {
        return;
    }
    let Some(scope) = scope(&settings, request.provider_context.auth_profile.as_deref()) else {
        return;
    };
    let (status, reason) = match outcome {
        Ok(StreamOutcome::Cancelled) => (ModelCheckStatus::Cancelled, ModelCheckReason::Cancelled),
        Ok(_) => (ModelCheckStatus::Working, ModelCheckReason::ObservedSuccess),
        Err(error) => match error
            .diagnostic_context
            .get("model_availability")
            .map(String::as_str)
        {
            Some("unsupported_surface") => (
                ModelCheckStatus::Unsupported,
                ModelCheckReason::ProviderSurfaceUnsupported,
            ),
            Some("account_unavailable") => (
                ModelCheckStatus::AccountDenied,
                ModelCheckReason::AccountDenied,
            ),
            _ => (
                ModelCheckStatus::Inconclusive,
                ModelCheckReason::InconclusiveProviderFailure,
            ),
        },
    };
    turn.push(ProviderTurnEvent::ModelAvailability {
        observation: ModelAvailabilityObservation {
            model_id,
            status,
            reason,
            scope,
            attempt_started_at_ms: admission.0,
            attempt_ordinal: admission.1,
            observed_at_ms: now_ms().max(admission.0),
        },
    });
}
