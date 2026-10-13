//! GitHub OIDC bootstrap: bounded exchange and private, non-secret configuration.

use crate::{Failure, now};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

pub(crate) const INTERFACE: &str = "bcode.auth-broker-setup/v1";
const LIMIT: usize = 65_536;
const TOKEN_LIMIT: usize = 16_384;
const MAX_LEASE_SECS: u64 = 900;

/// Version-one bootstrap request. Credentials are read only from the process environment.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareRequest {
    /// Existing absolute, owner-only runner directory, outside the untrusted checkout.
    pub directory: PathBuf,
    /// Trusted HTTPS broker origin, without credentials, path, query or fragment.
    pub broker_url: String,
    /// GitHub OIDC audience admitted by that broker.
    pub audience: String,
    /// Execution budget; must fit within the actual returned lease minus ten seconds.
    pub timeout_secs: u64,
}

/// Non-secret bootstrap result. Artifact names are fixed by the version-one contract.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareResponse {
    /// Profile written into `broker.toml`.
    pub profile: String,
}

pub(crate) fn origin(value: &str) -> Result<Url, Failure> {
    let url = https(value)?;
    if url.path() != "/" || url.query().is_some() {
        return Err("invalid_broker_url");
    }
    Ok(url)
}
fn https(value: &str) -> Result<Url, Failure> {
    if value.len() > 8192 || value.chars().any(char::is_whitespace) {
        return Err("invalid_endpoint");
    }
    let url = Url::parse(value).map_err(|_| "invalid_endpoint")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("invalid_endpoint");
    }
    Ok(url)
}
fn issuer(value: &str, audience: &str) -> Result<Url, Failure> {
    if audience.is_empty() || audience.len() > 2048 || audience.chars().any(char::is_control) {
        return Err("invalid_audience");
    }
    let mut url = https(value)?;
    if !url
        .host_str()
        .is_some_and(|host| host.ends_with(".actions.githubusercontent.com"))
        || url.port_or_known_default() != Some(443)
    {
        return Err("invalid_oidc_endpoint");
    }
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| key != "audience")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    url.query_pairs_mut()
        .extend_pairs(pairs)
        .append_pair("audience", audience);
    Ok(url)
}
fn token(value: &str) -> Result<(), Failure> {
    if value.is_empty()
        || value.len() > TOKEN_LIMIT
        || !value.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err("invalid_credential");
    }
    Ok(())
}

#[derive(Deserialize)]
struct Identity {
    value: String,
}
#[derive(Deserialize)]
struct Lease {
    lease: String,
    expires_at: u64,
}
impl std::fmt::Debug for Lease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Lease { <redacted> }")
    }
}
fn validate_lease(lease: &Lease, budget: u64, time: u64) -> Result<(), Failure> {
    token(&lease.lease)?;
    let remaining = lease
        .expires_at
        .checked_sub(time)
        .ok_or("invalid_lease_expiry")?;
    if remaining <= 15 || remaining > MAX_LEASE_SECS {
        return Err("invalid_lease_expiry");
    }
    if budget == 0 || budget > remaining - 10 {
        return Err("execution_budget_exceeds_lease");
    }
    Ok(())
}
fn client() -> Result<Client, Failure> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .https_only(true)
        .no_proxy()
        .build()
        .map_err(|_| "broker_setup_transport_failed")
}
async fn request<T: DeserializeOwned>(
    client: &Client,
    url: Url,
    bearer: &str,
    post: bool,
) -> Result<T, Failure> {
    token(bearer)?;
    let request = if post {
        client.post(url)
    } else {
        client.get(url)
    };
    let mut response = request
        .bearer_auth(bearer)
        .send()
        .await
        .map_err(|_| "broker_setup_transport_failed")?;
    if response.status() != reqwest::StatusCode::OK {
        return Err("broker_setup_rejected");
    }
    if response
        .content_length()
        .is_some_and(|size| size > LIMIT as u64)
    {
        return Err("broker_setup_response_too_large");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "broker_setup_transport_failed")?
    {
        if body.len().saturating_add(chunk.len()) > LIMIT {
            return Err("broker_setup_response_too_large");
        }
        body.extend_from_slice(&chunk);
    }
    decode(&body)
}
fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, Failure> {
    if body.len() > LIMIT {
        return Err("broker_setup_response_too_large");
    }
    serde_json::from_slice(body).map_err(|_| "broker_setup_invalid_response")
}

#[derive(Serialize, Deserialize)]
struct Config {
    model: Model,
}
#[derive(Serialize, Deserialize)]
struct Model {
    profiles: BTreeMap<String, Profile>,
}
#[derive(Serialize, Deserialize)]
struct Profile {
    provider_plugin_id: String,
    model_id: String,
    settings: BTreeMap<String, String>,
}
fn config(input: &PrepareRequest, url: &Url) -> Result<String, Failure> {
    let directory = input
        .directory
        .to_str()
        .ok_or("invalid_private_directory")?;
    let settings = BTreeMap::from([
        (
            "broker_url".into(),
            url.as_str().trim_end_matches('/').into(),
        ),
        (
            "broker_lease_file".into(),
            format!("{directory}/broker-token"),
        ),
        (
            "broker_expiry_file".into(),
            format!("{directory}/broker-expiry"),
        ),
    ]);
    toml::to_string(&Config {
        model: Model {
            profiles: BTreeMap::from([(
                "github-broker".into(),
                Profile {
                    provider_plugin_id: crate::PROVIDER.into(),
                    model_id: "broker-scoped".into(),
                    settings,
                },
            )]),
        },
    })
    .map_err(|_| "broker_setup_encoding_failed")
}

/// Exchange GitHub OIDC once and prepare runner-private artifacts; no automatic retry.
///
/// # Errors
/// Returns normalized, secret-safe errors for invalid inputs, missing OIDC environment,
/// network/rejection/bounds failures, insufficient lease budget, or unsafe/existing files.
pub async fn prepare(input: &PrepareRequest) -> Result<PrepareResponse, &'static str> {
    let directory = private::Directory::open(&input.directory)?;
    if let Some(workspace) = std::env::var_os("GITHUB_WORKSPACE") {
        let workspace =
            std::fs::canonicalize(workspace).map_err(|_| "invalid_trusted_directory")?;
        if input.directory.starts_with(workspace) {
            return Err("invalid_trusted_directory");
        }
    }
    let base = origin(&input.broker_url)?;
    if input.timeout_secs == 0 || input.timeout_secs > MAX_LEASE_SECS - 10 {
        return Err("execution_budget_exceeds_lease");
    }
    let request_url =
        std::env::var("ACTIONS_ID_TOKEN_REQUEST_URL").map_err(|_| "oidc_environment_required")?;
    let request_token =
        std::env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN").map_err(|_| "oidc_environment_required")?;
    let issuer = issuer(&request_url, &input.audience)?;
    token(&request_token)?;
    let client = client()?;
    let identity: Identity = request(&client, issuer, &request_token, false).await?;
    token(&identity.value)?;
    let lease: Lease = request(
        &client,
        base.join("v1/leases").map_err(|_| "invalid_broker_url")?,
        &identity.value,
        true,
    )
    .await?;
    validate_lease(&lease, input.timeout_secs, now())?;
    let configuration = config(input, &base)?;
    directory.publish(&lease.lease, lease.expires_at, &configuration)?;
    Ok(PrepareResponse {
        profile: "github-broker".into(),
    })
}

#[cfg(unix)]
mod private;
#[cfg(not(unix))]
mod private {
    pub(super) struct Directory;
    impl Directory {
        pub(super) fn open(_: &std::path::Path) -> Result<Self, crate::Failure> {
            Err("private_files_require_unix")
        }
        pub(super) fn publish(&self, _: &str, _: u64, _: &str) -> Result<(), crate::Failure> {
            Err("private_files_require_unix")
        }
    }
}

pub(crate) fn read_lease(lease: &str, expiry: &str) -> Result<(String, u64), Failure> {
    #[cfg(unix)]
    {
        private::read_lease(lease, expiry)
    }
    #[cfg(not(unix))]
    {
        let _ = (lease, expiry);
        Err("private_files_require_unix")
    }
}

#[cfg(test)]
mod tests;
