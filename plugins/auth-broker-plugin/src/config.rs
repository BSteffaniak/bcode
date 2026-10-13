//! Non-secret single-owner broker configuration.
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

/// Exact GitHub repository and workflow identity admitted to a lease.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedWorkflow {
    /// Numeric repository ID, stable across repository renames.
    pub repository_id: String,
    /// Exact owner/repository name.
    pub repository: String,
    /// Exact `workflow_ref`, including its ref suffix.
    pub workflow_ref: String,
    /// Optional reusable workflow identity; when set must match `job_workflow_ref`.
    pub job_workflow_ref: Option<String>,
    /// Exact git ref, preventing untrusted branch admission.
    pub git_ref: String,
    /// Exact GitHub subject (including environment when applicable).
    pub subject: String,
}

/// Administrator-owned configuration accepted only by the local serve operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServeRequest {
    /// Correlation identity supplied by the local launcher and bound by the host bridge.
    pub invocation_id: String,
    /// Bind address. Non-loopback requires a trusted TLS reverse proxy.
    pub bind: SocketAddr,
    /// Explicit acknowledgement that a non-loopback listener is protected by TLS ingress.
    pub trusted_tls_ingress: bool,
    /// Exact audience requested from GitHub Actions.
    pub audience: String,
    /// Explicit repository/workflow allowlist.
    pub allow: Vec<TrustedWorkflow>,
    /// Maximum simultaneously retained leases.
    pub max_leases: usize,
    /// Lease TTL in seconds, at most 900 seconds and shortened by JWT expiry.
    #[serde(default = "default_lease_seconds")]
    pub lease_seconds: u64,
}

const fn default_lease_seconds() -> u64 {
    900
}

impl ServeRequest {
    /// Validate before binding or making any network requests.
    ///
    /// # Errors
    /// Rejects blank/broad policy, invalid limits, and unprotected public listeners.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(1..=900).contains(&self.lease_seconds) {
            return Err("broker lease_seconds must be between 1 and 900; renewal is unsupported");
        }
        if self.invocation_id.is_empty()
            || self.invocation_id.len() > 128
            || self.audience.is_empty()
            || self.audience.len() > 256
            || self.allow.is_empty()
            || self.allow.len() > 64
            || !(1..=4096).contains(&self.max_leases)
            || (!self.bind.ip().is_loopback() && !self.trusted_tls_ingress)
        {
            return Err("invalid broker configuration");
        }
        for rule in &self.allow {
            if rule.repository_id.is_empty()
                || !rule.repository_id.bytes().all(|byte| byte.is_ascii_digit())
                || !rule.repository.contains('/')
                || !rule.workflow_ref.contains("/.github/workflows/")
                || !rule.workflow_ref.contains('@')
                || !rule.git_ref.starts_with("refs/")
                || !rule.subject.starts_with("repo:")
                || [
                    &rule.repository,
                    &rule.workflow_ref,
                    &rule.git_ref,
                    &rule.subject,
                ]
                .iter()
                .any(|value| {
                    value.len() > 1024 || value.contains('*') || value.trim() != value.as_str()
                })
                || rule.job_workflow_ref.as_ref().is_some_and(|value| {
                    value.is_empty() || value.len() > 1024 || value.contains('*')
                })
            {
                return Err("invalid exact workflow allowlist");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configuration() -> serde_json::Value {
        serde_json::json!({
            "invocation_id":"serve", "bind":"127.0.0.1:8080", "trusted_tls_ingress":false,
            "audience":"https://broker.example/actions", "max_leases":1,
            "allow":[{"repository_id":"1", "repository":"owner/repo",
                "workflow_ref":"owner/repo/.github/workflows/agent.yml@refs/heads/main",
                "git_ref":"refs/heads/main", "subject":"repo:owner/repo:ref:refs/heads/main"}]
        })
    }

    #[test]
    fn lease_default_and_hard_limit_are_explicit() {
        let mut value = configuration();
        let config: ServeRequest = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(config.lease_seconds, 900);
        assert!(config.validate().is_ok());
        for seconds in [0, 901, 86400] {
            value["lease_seconds"] = seconds.into();
            let config: ServeRequest = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(
                config.validate(),
                Err("broker lease_seconds must be between 1 and 900; renewal is unsupported")
            );
        }
        value["lease_seconds"] = 900.into();
        assert!(
            serde_json::from_value::<ServeRequest>(value)
                .unwrap()
                .validate()
                .is_ok()
        );
    }
}
