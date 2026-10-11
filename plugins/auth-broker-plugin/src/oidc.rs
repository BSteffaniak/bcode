//! GitHub-only OIDC verification using library-owned signature validation.
use crate::config::TrustedWorkflow;
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{Jwk, JwkSet},
};
use serde::Deserialize;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const ISSUER: &str = "https://token.actions.githubusercontent.com";
const JWKS: &str = "https://token.actions.githubusercontent.com/.well-known/jwks";
const MAX_JWKS_BYTES: usize = 65_536;
const MAX_TOKEN_BYTES: usize = 16_384;

#[derive(Clone, Deserialize)]
struct Claims {
    aud: String,
    #[serde(rename = "sub")]
    subject: String,
    exp: u64,
    iat: u64,
    nbf: u64,
    repository: String,
    repository_id: String,
    workflow_ref: String,
    job_workflow_ref: Option<String>,
    #[serde(rename = "ref")]
    git_ref: String,
}

fn allowed(claims: &Claims, rules: &[TrustedWorkflow]) -> bool {
    rules.iter().any(|rule| {
        claims.repository == rule.repository
            && claims.repository_id == rule.repository_id
            && claims.workflow_ref == rule.workflow_ref
            && claims.git_ref == rule.git_ref
            && claims.subject == rule.subject
            && rule
                .job_workflow_ref
                .as_ref()
                .is_none_or(|expected| claims.job_workflow_ref.as_ref() == Some(expected))
    })
}

struct Keys {
    set: Option<JwkSet>,
    loaded: Option<Instant>,
    attempted: Option<Instant>,
}

pub struct Verifier {
    client: reqwest::Client,
    audience: String,
    rules: Vec<TrustedWorkflow>,
    keys: Mutex<Keys>,
}

fn select_key(set: &JwkSet, kid: &str) -> Result<Jwk, &'static str> {
    let mut matching = set
        .keys
        .iter()
        .filter(|key| key.common.key_id.as_deref() == Some(kid));
    let key = matching.next().ok_or("invalid_identity")?;
    if matching.next().is_some() {
        return Err("invalid_identity");
    }
    Ok(key.clone())
}

fn decoding_key(key: &Jwk) -> Result<DecodingKey, &'static str> {
    if key
        .common
        .key_algorithm
        .as_ref()
        .is_some_and(|alg| *alg != jsonwebtoken::jwk::KeyAlgorithm::RS256)
        || key
            .common
            .public_key_use
            .as_ref()
            .is_some_and(|usage| *usage != jsonwebtoken::jwk::PublicKeyUse::Signature)
    {
        return Err("invalid_identity");
    }
    let value = serde_json::to_value(key).map_err(|_| "invalid_identity")?;
    if let Some(operations) = value.get("key_ops").and_then(serde_json::Value::as_array)
        && !operations
            .iter()
            .any(|operation| operation.as_str() == Some("verify"))
    {
        return Err("invalid_identity");
    }
    DecodingKey::from_jwk(key).map_err(|_| "invalid_identity")
}

impl Verifier {
    pub(super) fn new(audience: String, rules: Vec<TrustedWorkflow>) -> Result<Self, &'static str> {
        let client = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| "OIDC client initialization failed")?;
        Ok(Self {
            client,
            audience,
            rules,
            keys: Mutex::new(Keys {
                set: None,
                loaded: None,
                attempted: None,
            }),
        })
    }

    async fn fetch_keys(&self) -> Result<JwkSet, &'static str> {
        let mut response = self
            .client
            .get(JWKS)
            .send()
            .await
            .map_err(|_| "identity_keys_unavailable")?
            .error_for_status()
            .map_err(|_| "identity_keys_unavailable")?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_JWKS_BYTES as u64)
        {
            return Err("identity_keys_unavailable");
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "identity_keys_unavailable")?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_JWKS_BYTES {
                return Err("identity_keys_unavailable");
            }
            bytes.extend_from_slice(&chunk);
        }
        drop(response);
        let set: JwkSet =
            serde_json::from_slice(&bytes).map_err(|_| "identity_keys_unavailable")?;
        if set.keys.is_empty() || set.keys.len() > 32 {
            return Err("identity_keys_unavailable");
        }
        Ok(set)
    }

    async fn key(&self, kid: &str) -> Result<Jwk, &'static str> {
        let mut keys = self.keys.lock().await;
        let fresh = keys
            .loaded
            .is_some_and(|loaded| loaded.elapsed() < Duration::from_mins(5));
        let known = keys.set.as_ref().is_some_and(|set| set.find(kid).is_some());
        if fresh && known {
            let selected = select_key(keys.set.as_ref().ok_or("identity_keys_unavailable")?, kid);
            drop(keys);
            return selected;
        }
        // Reserve the refresh before releasing the lock. Unknown kids cannot cause a fetch storm.
        // Other callers can still use fresh known keys while the single fetch is in flight.
        if keys
            .attempted
            .is_some_and(|attempt| attempt.elapsed() < Duration::from_secs(30))
        {
            return Err("identity_keys_unavailable");
        }
        keys.attempted = Some(Instant::now());
        drop(keys);
        let set = self.fetch_keys().await?;
        let selected = select_key(&set, kid);
        let mut keys = self.keys.lock().await;
        keys.set = Some(set);
        keys.loaded = Some(Instant::now());
        drop(keys);
        selected
    }

    /// Return verified token expiry. JWTs and claims are never included in errors or logs.
    pub(super) async fn verify(&self, token: &str, time: u64) -> Result<u64, &'static str> {
        if token.len() > MAX_TOKEN_BYTES {
            return Err("invalid_identity");
        }
        let header = decode_header(token).map_err(|_| "invalid_identity")?;
        if header.alg != Algorithm::RS256 || header.jku.is_some() || header.x5u.is_some() {
            return Err("invalid_identity");
        }
        let kid = header
            .kid
            .as_deref()
            .filter(|kid| !kid.is_empty() && kid.len() <= 256)
            .ok_or("invalid_identity")?;
        let key = decoding_key(&self.key(kid).await?)?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[ISSUER]);
        validation.set_audience(&[self.audience.as_str()]);
        validation.set_required_spec_claims(&["exp", "iat", "nbf", "iss", "aud", "sub"]);
        validation.validate_nbf = true;
        validation.leeway = 0;
        let claims = decode::<Claims>(token, &key, &validation)
            .map_err(|_| "invalid_identity")?
            .claims;
        if claims.aud != self.audience
            || claims.exp <= time
            || claims.iat > time
            || claims.nbf > time
            || claims.exp.saturating_sub(claims.iat) > 900
            || !allowed(&claims, &self.rules)
        {
            return Err("identity_not_allowed");
        }
        Ok(claims.exp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rule() -> TrustedWorkflow {
        TrustedWorkflow {
            repository_id: "123".into(),
            repository: "owner/repo".into(),
            workflow_ref: "owner/repo/.github/workflows/run.yml@refs/heads/main".into(),
            job_workflow_ref: None,
            git_ref: "refs/heads/main".into(),
            subject: "repo:owner/repo:ref:refs/heads/main".into(),
        }
    }
    #[test]
    fn exact_workflow_and_stable_repository_identity() {
        let rule = rule();
        let mut claims = Claims {
            aud: "aud".into(),
            subject: rule.subject.clone(),
            exp: 200,
            iat: 100,
            nbf: 100,
            repository: rule.repository.clone(),
            repository_id: rule.repository_id.clone(),
            workflow_ref: rule.workflow_ref.clone(),
            job_workflow_ref: None,
            git_ref: rule.git_ref.clone(),
        };
        assert!(allowed(&claims, std::slice::from_ref(&rule)));
        claims.repository_id = "456".into();
        assert!(!allowed(&claims, std::slice::from_ref(&rule)));
        claims.repository_id = "123".into();
        claims.git_ref = "refs/heads/attacker".into();
        assert!(!allowed(&claims, std::slice::from_ref(&rule)));
        claims.git_ref.clone_from(&rule.git_ref);
        claims.subject = "repo:other/repo:ref:refs/heads/main".into();
        assert!(!allowed(&claims, &[rule]));
    }
    #[tokio::test]
    async fn symmetric_and_unsigned_tokens_fail_before_key_fetch() {
        let verifier = Verifier::new("aud".into(), vec![rule()]).unwrap();
        assert!(
            verifier
                .verify("eyJhbGciOiJub25lIn0.e30.", 100)
                .await
                .is_err()
        );
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(Algorithm::HS256),
            &serde_json::json!({"exp":200}),
            &jsonwebtoken::EncodingKey::from_secret(b"not-an-rsa-key"),
        )
        .unwrap();
        assert!(verifier.verify(&token, 100).await.is_err());
        assert!(verifier.keys.lock().await.attempted.is_none());
    }
}
