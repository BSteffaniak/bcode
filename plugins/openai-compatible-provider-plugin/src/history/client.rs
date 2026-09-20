//! Bounded, read-only access to private `ChatGPT` history endpoints.
//!
//! Credentials are borrowed from an authorized provider operation. This client never
//! resolves profiles, refreshes tokens, writes credentials, or persists source bodies.

use super::{HistoryDecodeError, HistorySnapshot, decode_history};
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;
use std::time::Duration;

/// Safe categories for source status, without remote error bodies or credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryAccessError {
    /// Caller supplied an invalid identifier, page size, or response budget.
    InvalidRequest,
    /// Credentials need provider-owned refresh or reconnect.
    AuthenticationRequired,
    /// Authenticated access was denied.
    AccessDenied,
    /// A browser/access challenge prevents automated retrieval.
    AccessChallenge,
    /// The source asks the scheduler to wait; do not sleep inside the client.
    RateLimited { retry_after_seconds: Option<u64> },
    /// Requested source record is unavailable; this does not authorize local deletion.
    NotFound,
    /// Temporary transport/server failure.
    Transient,
    /// Redirect or unsupported response shape/status.
    IncompatibleResponse,
    /// Response exceeded the operation budget; it was not truncated into a snapshot.
    TooLarge,
    /// Conversation graph could not be decoded faithfully.
    Decode(HistoryDecodeError),
}

/// One remote conversation summary; identity is still scoped to the caller's account.
#[derive(Debug, Deserialize)]
pub struct HistorySummary {
    /// Remote API conversation ID, not a website URL conversion.
    pub id: String,
    /// Source title, if available.
    pub title: Option<String>,
    /// Upstream timestamp; not a reliable exclusive incremental cursor.
    pub update_time: Option<f64>,
}

/// A bounded summary page. A short page is not proof that every scope was enumerated.
#[derive(Debug)]
pub struct HistoryPage {
    /// Validated source summaries.
    pub items: Vec<HistorySummary>,
    /// Offset for continuation of a full page; callers must overlap/reconcile shifting pages.
    pub next_offset: Option<u64>,
}

#[derive(Deserialize)]
struct WirePage {
    items: Vec<HistorySummary>,
}

/// Fixed-origin client with no redirects, cookies, or persistent browser state.
pub struct HistoryClient {
    client: Client,
    max_response_bytes: usize,
}

impl HistoryClient {
    /// Build a client with a positive response budget and a finite request timeout.
    ///
    /// # Errors
    /// Rejects zero/excessive budgets or HTTP client initialization failures.
    pub fn new(max_response_bytes: usize) -> Result<Self, HistoryAccessError> {
        if max_response_bytes == 0 || max_response_bytes > 64 * 1024 * 1024 {
            return Err(HistoryAccessError::InvalidRequest);
        }
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| HistoryAccessError::Transient)?;
        Ok(Self {
            client,
            max_response_bytes,
        })
    }

    /// Fetch one ordinary or archived summary page with explicitly supplied account credentials.
    ///
    /// Dropping this future cancels its network work. No retry or background task is spawned.
    /// Archive support still requires upstream coverage verification by the integration.
    ///
    /// # Errors
    /// Returns normalized authentication, access, rate, transport, schema, or budget failures.
    pub async fn list(
        &self,
        token: &str,
        account: &str,
        offset: u64,
        limit: u16,
        archived: bool,
    ) -> Result<HistoryPage, HistoryAccessError> {
        if !(1..=100).contains(&limit) || offset.checked_add(u64::from(limit)).is_none() {
            return Err(HistoryAccessError::InvalidRequest);
        }
        let path = format!(
            "conversations?offset={offset}&limit={limit}&order=updated&is_archived={archived}"
        );
        let bytes = self.get(token, account, &path).await?;
        decode_page(&bytes, offset, limit)
    }

    /// Fetch and validate a complete selected-branch snapshot within the response budget.
    ///
    /// # Errors
    /// Returns normalized request/access failures or rejects malformed/inconsistent source graphs.
    pub async fn conversation(
        &self,
        token: &str,
        account: &str,
        id: &str,
    ) -> Result<HistorySnapshot, HistoryAccessError> {
        if !valid_id(id) {
            return Err(HistoryAccessError::InvalidRequest);
        }
        let bytes = self
            .get(token, account, &format!("conversation/{id}"))
            .await?;
        decode_history(&bytes, id, self.max_response_bytes).map_err(HistoryAccessError::Decode)
    }

    async fn get(
        &self,
        token: &str,
        account: &str,
        path: &str,
    ) -> Result<Vec<u8>, HistoryAccessError> {
        if token.is_empty() || account.is_empty() {
            return Err(HistoryAccessError::AuthenticationRequired);
        }
        let response = self
            .client
            .get(format!("https://chatgpt.com/backend-api/{path}"))
            .bearer_auth(token)
            .header("ChatGPT-Account-Id", account)
            .header("Accept", "application/json")
            .header("User-Agent", "bcode-history/1")
            .send()
            .await
            .map_err(|_| HistoryAccessError::Transient)?;
        read_response(response, self.max_response_bytes).await
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn decode_page(bytes: &[u8], offset: u64, limit: u16) -> Result<HistoryPage, HistoryAccessError> {
    let page: WirePage =
        serde_json::from_slice(bytes).map_err(|_| HistoryAccessError::IncompatibleResponse)?;
    if page.items.len() > usize::from(limit)
        || page.items.iter().any(|item| {
            !valid_id(&item.id)
                || item
                    .update_time
                    .is_some_and(|time| !time.is_finite() || time < 0.0)
        })
    {
        return Err(HistoryAccessError::IncompatibleResponse);
    }
    let next_offset = if page.items.len() == usize::from(limit) {
        Some(
            offset
                .checked_add(u64::from(limit))
                .ok_or(HistoryAccessError::InvalidRequest)?,
        )
    } else {
        None
    };
    Ok(HistoryPage {
        items: page.items,
        next_offset,
    })
}

const fn classify(
    status: StatusCode,
    challenge: bool,
    retry: Option<u64>,
) -> Result<(), HistoryAccessError> {
    if challenge {
        return Err(HistoryAccessError::AccessChallenge);
    }
    match status.as_u16() {
        200 => Ok(()),
        401 => Err(HistoryAccessError::AuthenticationRequired),
        403 => Err(HistoryAccessError::AccessDenied),
        404 => Err(HistoryAccessError::NotFound),
        429 => Err(HistoryAccessError::RateLimited {
            retry_after_seconds: retry,
        }),
        408 | 500..=599 => Err(HistoryAccessError::Transient),
        _ => Err(HistoryAccessError::IncompatibleResponse),
    }
}

async fn read_response(
    mut response: Response,
    max_bytes: usize,
) -> Result<Vec<u8>, HistoryAccessError> {
    let challenge = response
        .headers()
        .get("cf-mitigated")
        .is_some_and(|v| v == "challenge");
    let retry = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    classify(response.status(), challenge, retry)?;
    if !response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
        })
    {
        return Err(HistoryAccessError::IncompatibleResponse);
    }
    if response
        .content_length()
        .is_some_and(|size| size > max_bytes as u64)
    {
        return Err(HistoryAccessError::TooLarge);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| HistoryAccessError::Transient)?
    {
        if chunk.len() > max_bytes.saturating_sub(bytes.len()) {
            return Err(HistoryAccessError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_have_distinct_safe_categories() {
        assert_eq!(
            classify(StatusCode::UNAUTHORIZED, false, None),
            Err(HistoryAccessError::AuthenticationRequired)
        );
        assert_eq!(
            classify(StatusCode::FORBIDDEN, false, None),
            Err(HistoryAccessError::AccessDenied)
        );
        assert_eq!(
            classify(StatusCode::FORBIDDEN, true, None),
            Err(HistoryAccessError::AccessChallenge)
        );
        assert_eq!(
            classify(StatusCode::TOO_MANY_REQUESTS, false, Some(60)),
            Err(HistoryAccessError::RateLimited {
                retry_after_seconds: Some(60)
            })
        );
        assert_eq!(
            classify(StatusCode::FOUND, false, None),
            Err(HistoryAccessError::IncompatibleResponse)
        );
    }

    #[test]
    fn paging_validates_and_advances_without_a_total_history_ceiling() {
        let page = decode_page(
            br#"{"items":[{"id":"abc","title":"Synthetic","update_time":1}]}"#,
            10000,
            1,
        )
        .unwrap();
        assert_eq!(page.next_offset, Some(10001));
        assert_eq!(
            decode_page(br#"{"items":[]}"#, 10001, 1)
                .unwrap()
                .next_offset,
            None
        );
        assert!(decode_page(br#"{"items":[{"id":"../evil"}]}"#, 0, 1).is_err());
        assert!(decode_page(br#"{"items":[{"id":"a"},{"id":"b"}]}"#, 0, 1).is_err());
        assert!(decode_page(br#"{"unexpected":[]}"#, 0, 1).is_err());
    }

    #[test]
    fn rejects_website_ids_and_path_injection_without_guessing() {
        for id in ["", "WEB:abc", "../abc", "abc?x=1", "abc%2fdef"] {
            assert!(!valid_id(id));
        }
        assert!(valid_id("abc-123"));
        assert!(HistoryClient::new(0).is_err());
    }
}
