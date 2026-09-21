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
    /// The owning service cancelled this retrieval; no snapshot was published.
    Cancelled,
    /// Caller supplied an invalid identifier, page size, or response budget.
    InvalidRequest,
    /// Credentials are missing, invalid, or require reconnect.
    AuthenticationRequired,
    /// Expiring credentials have a refresh token; owning custody must refresh first.
    RefreshRequired,
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
#[derive(Deserialize)]
pub struct HistorySummary {
    /// Remote API conversation ID, not a website URL conversion.
    pub id: String,
    /// Source title, if available.
    pub title: Option<String>,
    /// Upstream timestamp; not a reliable exclusive incremental cursor.
    pub update_time: Option<f64>,
}

impl std::fmt::Debug for HistorySummary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HistorySummary")
            .field("id", &"[redacted]")
            .field("title", &self.title.as_ref().map(|_| "[redacted]"))
            .finish_non_exhaustive()
    }
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

    /// Fetch a page using only the host-resolved, explicitly selected profile.
    ///
    /// This never resolves environment credentials or falls back to another profile.
    /// The host must refresh and durably persist expired credentials before invoking it.
    /// Account attributes are routing claims, not proof of durable remote identity.
    ///
    /// # Errors
    /// Rejects missing profile/scheme/account/token or expired credentials before retrieval,
    /// then returns the same bounded access errors as [`Self::list`].
    pub async fn list_for_auth(
        &self,
        auth: &bcode_model::ProviderAuthContext,
        offset: u64,
        limit: u16,
        archived: bool,
        cancellation: &bcode_plugin_sdk::ServiceCancellation,
    ) -> Result<HistoryPage, HistoryAccessError> {
        cancellable(cancellation, async {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| HistoryAccessError::InvalidRequest)?
                .as_secs();
            let (token, account) = history_credentials(auth, now)?;
            self.list(token, account, offset, limit, archived)
                .await
                .map_err(|error| auth_access_error(auth, error))
        })
        .await
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

    /// Retrieve a snapshot using the explicitly selected, host-resolved profile.
    ///
    /// As with [`Self::list_for_auth`], the caller owns refresh and durable custody.
    /// This operation does not establish verified remote account identity.
    ///
    /// # Errors
    /// Rejects invalid authentication before any request, then returns the bounded
    /// access and graph validation errors of [`Self::conversation`].
    pub async fn conversation_for_auth(
        &self,
        auth: &bcode_model::ProviderAuthContext,
        id: &str,
        cancellation: &bcode_plugin_sdk::ServiceCancellation,
    ) -> Result<HistorySnapshot, HistoryAccessError> {
        self.conversation_branch_for_auth(auth, id, None, cancellation)
            .await
    }

    /// Retrieve one explicit branch without modifying upstream selection.
    ///
    /// # Errors
    /// Returns authentication/access/graph errors, including an absent requested node.
    pub async fn conversation_branch_for_auth(
        &self,
        auth: &bcode_model::ProviderAuthContext,
        id: &str,
        selected_node: Option<&str>,
        cancellation: &bcode_plugin_sdk::ServiceCancellation,
    ) -> Result<HistorySnapshot, HistoryAccessError> {
        cancellable(cancellation, async {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| HistoryAccessError::InvalidRequest)?
                .as_secs();
            let (token, account) = history_credentials(auth, now)?;
            if !valid_id(id) {
                return Err(HistoryAccessError::InvalidRequest);
            }
            let bytes = self
                .get(token, account, &format!("conversation/{id}"))
                .await
                .map_err(|error| auth_access_error(auth, error))?;
            super::decode_history_branch(&bytes, id, self.max_response_bytes, selected_node)
                .map_err(HistoryAccessError::Decode)
        })
        .await
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

async fn cancellable<T>(
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
    operation: impl std::future::Future<Output = Result<T, HistoryAccessError>>,
) -> Result<T, HistoryAccessError> {
    if cancellation.is_cancelled() {
        return Err(HistoryAccessError::Cancelled);
    }
    tokio::pin!(operation);
    loop {
        tokio::select! {
            result = &mut operation => {
                return if cancellation.is_cancelled() {
                    Err(HistoryAccessError::Cancelled)
                } else {
                    result
                };
            }
            () = tokio::time::sleep(Duration::from_millis(25)) => {
                if cancellation.is_cancelled() {
                    return Err(HistoryAccessError::Cancelled);
                }
            }
        }
    }
}

// A rejected access token can need refresh even when its recorded expiry is in
// the future. Only the owning coordinator may rotate credentials; this client
// neither retries nor turns denied/challenged requests into refresh attempts.
fn auth_access_error(
    auth: &bcode_model::ProviderAuthContext,
    error: HistoryAccessError,
) -> HistoryAccessError {
    if error == HistoryAccessError::AuthenticationRequired
        && auth
            .credentials
            .get("refresh_token")
            .is_some_and(|token| !token.value.trim().is_empty())
    {
        HistoryAccessError::RefreshRequired
    } else {
        error
    }
}

fn history_credentials(
    auth: &bcode_model::ProviderAuthContext,
    now: u64,
) -> Result<(&str, &str), HistoryAccessError> {
    if auth.scheme.as_deref() != Some("chatgpt")
        || auth
            .profile
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
    {
        return Err(HistoryAccessError::AuthenticationRequired);
    }
    if let Some(expiry) = auth.credentials.get("expires_at") {
        let expiry = expiry
            .value
            .parse::<u64>()
            .map_err(|_| HistoryAccessError::AuthenticationRequired)?;
        if expiry <= now.saturating_add(60) {
            return Err(
                if auth
                    .credentials
                    .get("refresh_token")
                    .is_some_and(|token| !token.value.trim().is_empty())
                {
                    HistoryAccessError::RefreshRequired
                } else {
                    HistoryAccessError::AuthenticationRequired
                },
            );
        }
    }
    let credential = |name| {
        auth.credentials
            .get(name)
            .map(|credential| credential.value.as_str())
            .filter(|value| !value.trim().is_empty())
            .ok_or(HistoryAccessError::AuthenticationRequired)
    };
    Ok((credential("access_token")?, credential("account_id")?))
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
    let mut identities = std::collections::BTreeSet::new();
    if page.items.len() > usize::from(limit)
        || page.items.iter().any(|item| {
            !identities.insert(&item.id)
                || !valid_id(&item.id)
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
    let retry_headers = ["retry-after", "retry-after-ms"]
        .into_iter()
        .filter_map(|name| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(|value| (name.to_owned(), value.to_owned()))
        })
        .collect();
    let retry = bcode_model_provider_runtime::retry_hint_from_headers(&retry_headers)
        .and_then(|hint| hint.retry_after_ms)
        .map(|milliseconds| milliseconds.div_ceil(1_000));
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
    fn page_diagnostics_redact_source_identity_and_content() {
        let page = decode_page(
            br#"{"items":[{"id":"private-conversation-id","title":"private conversation title","update_time":123456789.0}]}"#,
            0,
            10,
        )
        .unwrap();
        let diagnostic = format!("{page:?}");
        for private in [
            "private-conversation-id",
            "private conversation title",
            "123456789",
        ] {
            assert!(!diagnostic.contains(private));
        }
        assert_eq!(page.items[0].id, "private-conversation-id");
        assert_eq!(
            page.items[0].title.as_deref(),
            Some("private conversation title")
        );
        assert_eq!(page.items[0].update_time, Some(123_456_789.0));
    }

    async fn mock_response(headers: &str, body: &[u8]) -> Response {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let wire = [headers.as_bytes(), body].concat();
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).unwrap();
            socket.write_all(&wire).unwrap();
        });
        let response = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap();
        worker.join().unwrap();
        response
    }

    #[tokio::test]
    async fn rate_limit_response_preserves_provider_retry_timing() {
        for (header, expected) in [
            ("Retry-After: 120", Some(120)),
            ("Retry-After-Ms: 1001", Some(2)),
            ("Retry-After: Wed, 21 Oct 2015 07:28:00 GMT", Some(0)),
            ("Retry-After: invalid", None),
        ] {
            let response = mock_response(
                &format!("HTTP/1.1 429 Too Many Requests\r\n{header}\r\nContent-Length: 0\r\n\r\n"),
                b"",
            )
            .await;
            assert_eq!(
                read_response(response, 1024).await,
                Err(HistoryAccessError::RateLimited {
                    retry_after_seconds: expected,
                })
            );
        }
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_stalled_response_body() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() <= 4096);
            }
            // Send valid headers and an incomplete JSON body, keeping the connection open.
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{").unwrap();
            let _ = released.recv_timeout(Duration::from_secs(5));
        });
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let response = client
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap();
        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        let cancel = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancellation.cancel();
        };
        let operation = cancellable(&cancellation, read_response(response, 1024));
        let (result, ()) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(1), operation),
            cancel
        );
        // Always release and join the fixture before asserting, even on a regression.
        let _ = release.send(());
        worker.join().unwrap();
        assert_eq!(result.unwrap(), Err(HistoryAccessError::Cancelled));
    }

    #[tokio::test]
    async fn bounded_response_reader_accepts_json_and_rejects_large_chunked_body() {
        let response = mock_response("HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: 2\r\nConnection: close\r\n\r\n", b"{}").await;
        assert_eq!(read_response(response, 2).await.unwrap(), b"{}");
        let response = mock_response("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n", b"4\r\n1234\r\n0\r\n\r\n").await;
        assert_eq!(
            read_response(response, 3).await,
            Err(HistoryAccessError::TooLarge)
        );
    }

    #[tokio::test]
    async fn response_failures_never_include_private_remote_bodies() {
        for (status, extra, expected) in [
            (
                "401 Unauthorized",
                "",
                HistoryAccessError::AuthenticationRequired,
            ),
            (
                "403 Forbidden",
                "cf-mitigated: challenge\r\n",
                HistoryAccessError::AccessChallenge,
            ),
            (
                "302 Found",
                "Location: https://example.invalid/\r\n",
                HistoryAccessError::IncompatibleResponse,
            ),
            (
                "429 Too Many Requests",
                "Retry-After: 17\r\n",
                HistoryAccessError::RateLimited {
                    retry_after_seconds: Some(17),
                },
            ),
        ] {
            let response = mock_response(
                &format!(
                    "HTTP/1.1 {status}\r\n{extra}Content-Length: 14\r\nConnection: close\r\n\r\n"
                ),
                b"private secret",
            )
            .await;
            assert_eq!(read_response(response, 1024).await, Err(expected));
        }
    }

    #[tokio::test]
    async fn html_success_is_not_mistaken_for_history() {
        let response = mock_response("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", b"").await;
        assert_eq!(
            read_response(response, 1024).await,
            Err(HistoryAccessError::IncompatibleResponse)
        );
    }

    #[tokio::test]
    async fn profile_operations_reject_missing_auth_before_validating_remote_requests() {
        let client = HistoryClient::new(1024).unwrap();
        let auth = bcode_model::ProviderAuthContext::default();
        assert!(matches!(
            client
                .list_for_auth(
                    &auth,
                    0,
                    0,
                    false,
                    &bcode_plugin_sdk::ServiceCancellation::default()
                )
                .await,
            Err(HistoryAccessError::AuthenticationRequired)
        ));
        assert!(matches!(
            client
                .conversation_for_auth(
                    &auth,
                    "../invalid",
                    &bcode_plugin_sdk::ServiceCancellation::default()
                )
                .await,
            Err(HistoryAccessError::AuthenticationRequired)
        ));
    }

    #[tokio::test]
    async fn cancellation_prevents_dispatch_and_drops_in_flight_work() {
        struct DropGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for DropGuard {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        cancellation.cancel();
        let result = cancellable::<()>(&cancellation, async {
            panic!("cancelled work must not be polled")
        })
        .await;
        assert_eq!(result, Err(HistoryAccessError::Cancelled));

        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let operation = async {
            let _guard = DropGuard(dropped.clone());
            cancellation.cancel();
            std::future::pending::<Result<(), HistoryAccessError>>().await
        };
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            cancellable(&cancellation, operation),
        )
        .await
        .unwrap();
        assert_eq!(result, Err(HistoryAccessError::Cancelled));
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn resolved_history_auth_has_no_fallback_and_rejects_expiry() {
        let mut auth = bcode_model::ProviderAuthContext {
            profile: Some("selected".to_owned()),
            scheme: Some("chatgpt".to_owned()),
            ..Default::default()
        };
        for (name, value) in [
            ("access_token", "synthetic-token"),
            ("account_id", "account-a"),
            ("expires_at", "1000"),
        ] {
            auth.credentials.insert(
                name.to_owned(),
                bcode_model::ProviderAuthCredential {
                    value: value.to_owned(),
                    source: None,
                },
            );
        }
        assert_eq!(
            history_credentials(&auth, 1),
            Ok(("synthetic-token", "account-a"))
        );
        assert_eq!(
            history_credentials(&auth, 940),
            Err(HistoryAccessError::AuthenticationRequired)
        );
        auth.credentials.insert(
            "refresh_token".into(),
            bcode_model::ProviderAuthCredential {
                value: "synthetic-refresh".into(),
                source: None,
            },
        );
        assert_eq!(
            history_credentials(&auth, 940),
            Err(HistoryAccessError::RefreshRequired)
        );
        auth.credentials.get_mut("refresh_token").unwrap().value = " ".into();
        assert_eq!(
            history_credentials(&auth, 940),
            Err(HistoryAccessError::AuthenticationRequired)
        );
        auth.profile = None;
        assert!(history_credentials(&auth, 1).is_err());
        auth.profile = Some("selected".to_owned());
        auth.scheme = Some("api_key".to_owned());
        assert!(history_credentials(&auth, 1).is_err());
        auth.scheme = Some("chatgpt".to_owned());
        auth.credentials.remove("account_id");
        assert!(history_credentials(&auth, 1).is_err());
    }

    #[test]
    fn rejected_access_tokens_request_refresh_only_when_available() {
        let mut auth = bcode_model::ProviderAuthContext::default();
        for value in ["", " ", "synthetic-refresh"] {
            auth.credentials.insert(
                "refresh_token".into(),
                bcode_model::ProviderAuthCredential {
                    value: value.into(),
                    source: None,
                },
            );
            assert_eq!(
                auth_access_error(&auth, HistoryAccessError::AuthenticationRequired),
                if value.trim().is_empty() {
                    HistoryAccessError::AuthenticationRequired
                } else {
                    HistoryAccessError::RefreshRequired
                }
            );
            for error in [
                HistoryAccessError::AccessDenied,
                HistoryAccessError::AccessChallenge,
                HistoryAccessError::Cancelled,
                HistoryAccessError::Transient,
                HistoryAccessError::RateLimited {
                    retry_after_seconds: Some(30),
                },
            ] {
                assert_eq!(auth_access_error(&auth, error), error);
            }
        }
    }

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
        assert_eq!(
            decode_page(br#"{"items":[{"id":"a"},{"id":"a"}]}"#, 0, 2).err(),
            Some(HistoryAccessError::IncompatibleResponse)
        );
        assert_eq!(
            decode_page(br#"{"items":[{"id":"a"},{"id":"b"}]}"#, 0, 2)
                .unwrap()
                .next_offset,
            Some(2)
        );
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
