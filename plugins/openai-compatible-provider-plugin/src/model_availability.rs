//! Conservative provider-owned interpretation of model availability failures.
//!
//! HTTP status alone is never definitive evidence that a model is unsupported. In particular,
//! `OpenAI`'s `model_not_found` can mean either an unknown model or missing account access.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvailabilityOutcome {
    UnsupportedSurface,
    AccountUnavailable,
    Ambiguous,
    Authentication,
    RateLimited,
    InvalidRequest,
    Transient,
}

impl AvailabilityOutcome {
    pub const fn diagnostic(self) -> &'static str {
        match self {
            Self::UnsupportedSurface => "unsupported_surface",
            Self::AccountUnavailable => "account_unavailable",
            Self::Ambiguous => "ambiguous",
            Self::Authentication => "authentication",
            Self::RateLimited => "rate_limited",
            Self::InvalidRequest => "invalid_request",
            Self::Transient => "transient",
        }
    }
}

pub fn classify(status: u16, code: &str, message: &str) -> AvailabilityOutcome {
    let code = code.to_ascii_lowercase();
    let message = message.to_ascii_lowercase();
    // Transport/auth/quota failures take precedence over text mentioning model support.
    if status == 401 {
        return AvailabilityOutcome::Authentication;
    }
    if status == 429 {
        return AvailabilityOutcome::RateLimited;
    }
    if status == 408 || status >= 500 {
        return AvailabilityOutcome::Transient;
    }
    // These provider codes explicitly identify model availability rather than a parameter.
    if matches!(status, 400 | 403 | 404 | 422) {
        if matches!(code.as_str(), "model_not_supported" | "unsupported_model") {
            return AvailabilityOutcome::UnsupportedSurface;
        }
        if matches!(
            code.as_str(),
            "model_access_denied" | "model_not_available_for_account"
        ) {
            return AvailabilityOutcome::AccountUnavailable;
        }
    }
    // The Codex subscription surface returns an invalid_request_error without a unique code.
    // Require an explicit model + surface + account statement, not generic unsupported wording.
    if matches!(status, 400 | 403 | 404 | 422)
        && message.starts_with("the '")
        && message.contains("' model is not supported when using codex with a chatgpt account")
    {
        return AvailabilityOutcome::UnsupportedSurface;
    }
    if status == 404 || code == "model_not_found" {
        return AvailabilityOutcome::Ambiguous;
    }
    match status {
        403 => AvailabilityOutcome::Authentication,
        400 | 422 => AvailabilityOutcome::InvalidRequest,
        _ => AvailabilityOutcome::Ambiguous,
    }
}

#[cfg(test)]
mod tests {
    use super::{AvailabilityOutcome as Outcome, classify};

    #[test]
    fn only_explicit_model_rejections_are_definitive() {
        for (status, code, message, expected) in [
            (
                400,
                "unsupported_model",
                "model rejected",
                Outcome::UnsupportedSurface,
            ),
            (
                403,
                "model_access_denied",
                "model unavailable",
                Outcome::AccountUnavailable,
            ),
            (
                400,
                "invalid_request_error",
                "The 'gpt-x' model is not supported when using Codex with a ChatGPT account.",
                Outcome::UnsupportedSurface,
            ),
            (
                404,
                "model_not_found",
                "Model does not exist or you do not have access",
                Outcome::Ambiguous,
            ),
            (
                400,
                "model_not_found",
                "Model does not exist",
                Outcome::Ambiguous,
            ),
            (404, "", "Not found", Outcome::Ambiguous),
            (
                400,
                "unsupported_parameter",
                "Model does not support temperature",
                Outcome::InvalidRequest,
            ),
            (400, "", "Model is not supported", Outcome::InvalidRequest),
            (
                401,
                "unsupported_model",
                "bad credential",
                Outcome::Authentication,
            ),
            (403, "", "Forbidden", Outcome::Authentication),
            (
                429,
                "unsupported_model",
                "quota exhausted",
                Outcome::RateLimited,
            ),
            (
                503,
                "unsupported_model",
                "temporarily unavailable",
                Outcome::Transient,
            ),
        ] {
            assert_eq!(classify(status, code, message), expected);
        }
    }

    #[test]
    fn normalized_result_contains_no_provider_payload() {
        let result = classify(404, "secret-code", "secret body");
        assert_eq!(result.diagnostic(), "ambiguous");
    }
}
