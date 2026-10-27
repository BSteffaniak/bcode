//! Explicit, revision-checked publication of runtime pool reconciliation.

use super::{ConfigError, RuntimeAuthSubscriptionPool, RuntimeAuthSubscriptions};

/// Publish a provider-auth-validated pool reconciliation without changing credentials.
///
/// `preserve_observations` runs under the registry lock, before membership changes. It must
/// retain source observations so interruption before registry publication is retryable.
///
/// # Errors
/// Rejects stale snapshots, unknown registry representations, or failed observation preservation.
pub fn publish_auth_pool_reconciliation<G>(
    expected: &RuntimeAuthSubscriptions,
    source: &str,
    destination: &str,
    replacement: &RuntimeAuthSubscriptionPool,
    preserve_observations: impl FnOnce() -> Result<G, ConfigError>,
) -> Result<(), ConfigError> {
    publish_auth_pool_reconciliation_at(
        &super::runtime_auth_subscriptions_path(),
        expected,
        source,
        destination,
        replacement,
        preserve_observations,
    )
}

fn publish_auth_pool_reconciliation_at<G>(
    path: &std::path::Path,
    expected: &RuntimeAuthSubscriptions,
    source: &str,
    destination: &str,
    replacement: &RuntimeAuthSubscriptionPool,
    preserve_observations: impl FnOnce() -> Result<G, ConfigError>,
) -> Result<(), ConfigError> {
    let _lock = super::lock_auth_subscriptions(path)?;
    let mut registry = super::read_auth_subscriptions_for_update(path)?;
    if &registry != expected || source == destination || !registry.pools.contains_key(source) {
        return Err(ConfigError::Composition {
            message: "Authentication registry changed; preview reconciliation again".into(),
        });
    }
    // Retain the exact supported registry snapshot before any observation or membership write.
    let parent = path.parent().ok_or_else(|| ConfigError::Composition {
        message: "auth registry requires a parent directory".into(),
    })?;
    let io_error = |source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    };
    let original = std::fs::read(path).map_err(io_error)?;
    let mut backup = tempfile::Builder::new()
        .prefix("subscriptions-before-reconcile-")
        .suffix(".json")
        .tempfile_in(parent)
        .map_err(io_error)?;
    std::io::Write::write_all(&mut backup, &original).map_err(io_error)?;
    backup.as_file().sync_all().map_err(io_error)?;
    backup.keep().map_err(|error| io_error(error.error))?;
    let _observation_ownership = preserve_observations()?;
    registry.pools.remove(source);
    registry
        .pools
        .insert(destination.to_owned(), replacement.clone());
    super::write_runtime_auth_subscriptions(path, &registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn publication_is_revision_checked_and_preserves_unknown_state_and_failed_copy() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("subscriptions.json");
        let mut registry = RuntimeAuthSubscriptions::default();
        registry
            .pools
            .insert("old".into(), RuntimeAuthSubscriptionPool::default());
        super::super::write_runtime_auth_subscriptions(&path, &registry).unwrap();
        let before = std::fs::read(&path).unwrap();
        let copied = Cell::new(false);
        let stale = RuntimeAuthSubscriptions::default();
        assert!(
            publish_auth_pool_reconciliation_at(
                &path,
                &stale,
                "old",
                "chosen",
                &registry.pools["old"],
                || {
                    copied.set(true);
                    Ok(())
                }
            )
            .is_err()
        );
        assert!(!copied.get());
        assert!(
            publish_auth_pool_reconciliation_at(
                &path,
                &registry,
                "old",
                "chosen",
                &registry.pools["old"],
                || Err::<(), _>(ConfigError::Composition {
                    message: "copy failed".into()
                })
            )
            .is_err()
        );
        assert_eq!(before, std::fs::read(&path).unwrap());
        publish_auth_pool_reconciliation_at(
            &path,
            &registry,
            "old",
            "chosen",
            &registry.pools["old"],
            || {
                copied.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert!(copied.get());
        let after = super::super::read_auth_subscriptions_for_update(&path).unwrap();
        assert!(!after.pools.contains_key("old"));
        assert_eq!(after.pools["chosen"], registry.pools["old"]);
        std::fs::write(&path, br#"{"pools":{},"future_field":true}"#).unwrap();
        let unknown = std::fs::read(&path).unwrap();
        assert!(
            publish_auth_pool_reconciliation_at(
                &path,
                &registry,
                "old",
                "chosen",
                &registry.pools["old"],
                || Ok(())
            )
            .is_err()
        );
        assert_eq!(unknown, std::fs::read(&path).unwrap());
    }
}
