//! Effective sshenv mapping comparisons for enrollment metadata only.

use bcode_config::{AuthCredentialMapping, RuntimeAuthProfile, RuntimeAuthSubscriptionProfile};
use bcode_provider_auth_models::{AuthMethodContribution, AuthProviderContribution};
use std::collections::BTreeMap;

// Match AuthVaultLifecycle::credential_storage_keys: key, then env, then the
// registered method's storage key. Keep undeclared entries intact so they cannot
// disappear from conflict detection. Never mutate the configured destination.
fn effective_map(
    map: &BTreeMap<String, AuthCredentialMapping>,
    method: &AuthMethodContribution,
) -> BTreeMap<String, AuthCredentialMapping> {
    let mut effective = map.clone();
    let mut include = |id: &str, default: &str| {
        let key = map
            .get(id)
            .and_then(|mapping| mapping.key.as_ref().or(mapping.env.as_ref()))
            .map_or(default, String::as_str);
        effective.insert(id.to_owned(), super::mapping(key));
    };
    match method {
        AuthMethodContribution::SecretFields { fields, .. } => {
            for field in fields {
                include(&field.credential_id, &field.storage_key);
            }
        }
        AuthMethodContribution::Interactive { credentials, .. } => {
            for credential in credentials {
                include(&credential.credential_id, &credential.storage_key);
            }
        }
    }
    effective
}

fn maps_match(
    left: &BTreeMap<String, AuthCredentialMapping>,
    right: &BTreeMap<String, AuthCredentialMapping>,
    scheme: &str,
    provider: &AuthProviderContribution,
) -> bool {
    provider
        .methods
        .iter()
        .find(|method| method.method_id() == scheme)
        .is_some_and(|method| effective_map(left, method) == effective_map(right, method))
}

pub(super) fn profiles_match(
    existing: &RuntimeAuthProfile,
    expected: &RuntimeAuthProfile,
    provider: &AuthProviderContribution,
) -> bool {
    let mut existing = existing.clone();
    if existing.backend == "sshenv"
        && maps_match(&existing.map, &expected.map, &expected.scheme, provider)
    {
        existing.map.clone_from(&expected.map);
    }
    existing == *expected
}

pub(super) fn members_match(
    existing: &RuntimeAuthSubscriptionProfile,
    expected: &RuntimeAuthSubscriptionProfile,
    provider: &AuthProviderContribution,
) -> bool {
    let mut existing = existing.clone();
    if maps_match(&existing.map, &expected.map, &expected.scheme, provider) {
        existing.map.clone_from(&expected.map);
    }
    existing == *expected
}
