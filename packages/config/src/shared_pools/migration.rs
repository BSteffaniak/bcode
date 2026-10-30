//! Reviewed consolidation of pool policy, with backups and recoverable publication order.

use super::{invalid, read};
use crate::ConfigError;
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, value};

struct Edit {
    path: PathBuf,
    original: Option<String>,
    updated: String,
}

/// Reviewed pool consolidation. Contents are deliberately not Debug/serializable.
pub struct PoolMigration {
    destination: PathBuf,
    final_policy: Option<String>,
    edits: Vec<Edit>,
    pools: Vec<String>,
}

impl PoolMigration {
    /// Pool identities consolidated by this plan (never account secrets).
    #[must_use]
    pub fn pools(&self) -> &[String] {
        &self.pools
    }

    /// Files changed by this plan, destination first.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.edits.iter().map(|edit| edit.path.as_path())
    }

    /// Mark a reviewed pool as the automatic default using a registered provider capability.
    ///
    /// # Errors
    /// Rejects missing pools, mismatched owners, and conflicting default identities.
    pub fn mark_default(
        &mut self,
        pool: &str,
        provider: &str,
        owner: &str,
        group: &str,
    ) -> Result<(), ConfigError> {
        if [provider, owner, group]
            .iter()
            .any(|value| value.trim().is_empty())
        {
            return Err(invalid(
                "Default pool requires a validated provider compatibility identity",
            ));
        }
        if !self.edits.iter().any(|edit| edit.path == self.destination) {
            let original = optional(&self.destination)?;
            self.edits.insert(
                0,
                Edit {
                    path: self.destination.clone(),
                    updated: original
                        .clone()
                        .ok_or_else(|| invalid("Shared policy is missing"))?,
                    original,
                },
            );
        }
        let edit = self
            .edits
            .first_mut()
            .ok_or_else(|| invalid("No shared policy changes planned"))?;
        let mut doc = document(&edit.updated)?;
        let raw: toml::Value =
            toml::from_str(&edit.updated).map_err(|_| invalid("Invalid shared policy"))?;
        let pools = super::pools(&raw)?;
        let selected = pools
            .get(pool)
            .ok_or_else(|| invalid("Default pool is not in shared policy"))?;
        if selected
            .get("provider_plugin_id")
            .and_then(toml::Value::as_str)
            != Some(owner)
        {
            return Err(invalid(
                "Default pool owner does not match registered provider",
            ));
        }
        for (name, policy) in pools {
            if name != pool
                && policy
                    .get("automatic_provider")
                    .and_then(toml::Value::as_str)
                    == Some(provider)
                && policy.get("automatic_group").and_then(toml::Value::as_str) == Some(group)
            {
                return Err(invalid(
                    "Another automatic default already exists for this provider group",
                ));
            }
        }
        for (key, expected) in [("automatic_provider", provider), ("automatic_group", group)] {
            if selected
                .get(key)
                .is_some_and(|value| value.as_str() != Some(expected))
            {
                return Err(invalid("Existing automatic pool identity conflicts"));
            }
            doc["auth"]["pools"][pool][key] = value(expected);
        }
        let updated = doc.to_string();
        if updated == edit.updated {
            if edit.original.as_deref() == Some(edit.updated.as_str()) {
                self.edits.remove(0);
            }
        } else {
            self.final_policy = Some(updated);
        }
        Ok(())
    }

    /// Apply an exact reviewed plan, preserving backups of existing files.
    ///
    /// Shared policy is published first. Interruption can leave duplicate policy, which
    /// fails closed until this same migration is replanned and applied. No credentials,
    /// account metadata, cooldowns or preferences in runtime state are touched.
    ///
    /// # Errors
    /// Rejects changed/symlink sources, locking or I/O failures. A publication I/O failure
    /// may leave a prefix applied; backups are retained and replanning is supported.
    pub fn apply(self) -> Result<(), ConfigError> {
        let mut ordered: Vec<_> = self.edits.iter().collect();
        ordered.sort_by_key(|edit| &edit.path);
        let mut locks = Vec::new();
        for edit in ordered {
            let lock_path = edit.path.with_extension("toml.lock");
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)
                .map_err(|source| io(&lock_path, source))?;
            lock.lock().map_err(|source| io(&lock_path, source))?;
            locks.push(lock);
        }
        for edit in &self.edits {
            if optional(&edit.path)? != edit.original {
                return Err(invalid(
                    "Pool migration source changed; review a fresh plan",
                ));
            }
        }
        // All backups are durable before any publication. Never overwrite older backups.
        for edit in &self.edits {
            if let Some(original) = &edit.original {
                let backup = edit.path.with_extension("toml.pool-migration.bak");
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&backup)
                {
                    Ok(mut file) => {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt as _;
                            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                                .map_err(|source| io(&backup, source))?;
                        }
                        file.write_all(original.as_bytes())
                            .map_err(|source| io(&backup, source))?;
                        file.sync_all().map_err(|source| io(&backup, source))?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        if read(&backup)? != *original {
                            return Err(invalid(
                                "Migration backup already exists with different contents; preserve it and choose a fresh backup location before retrying",
                            ));
                        }
                    }
                    Err(error) => return Err(io(&backup, error)),
                }
            }
            // Sync each backup directory before publishing any replacement.
            if let Some(parent) = edit.path.parent() {
                std::fs::File::open(parent)
                    .and_then(|file| file.sync_all())
                    .map_err(|source| io(parent, source))?;
            }
        }
        for edit in self.edits {
            crate::edit::atomic_replace(&edit.path, edit.updated.as_bytes())
                .map_err(|source| io(&edit.path, source))?;
        }
        if let Some(policy) = self.final_policy {
            crate::edit::atomic_replace(&self.destination, policy.as_bytes())
                .map_err(|source| io(&self.destination, source))?;
        }
        drop(locks);
        Ok(())
    }
}

fn io(path: &Path, source: std::io::Error) -> ConfigError {
    ConfigError::Io {
        path: path.to_owned(),
        source,
    }
}

fn optional(path: &Path) -> Result<Option<String>, ConfigError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => read(path).map(Some),
        Ok(_) => Err(invalid(
            "Pool migration requires regular files, not symlinks",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io(path, error)),
    }
}

fn document(contents: &str) -> Result<DocumentMut, ConfigError> {
    contents
        .parse()
        .map_err(|_| invalid("Pool migration requires valid TOML"))
}

/// Consolidate identical pool definitions into one explicitly referenced shared file.
///
/// Paths must name existing presets and an existing parent directory for the destination.
/// This is read-only until `apply`; differing definitions are never merged heuristically.
///
/// # Errors
/// Rejects conflicting policies, existing foreign references, malformed TOML, symlinks,
/// duplicate paths. Context-local policy is untouched and is not globalized.
pub fn plan(destination: &Path, sources: &[PathBuf]) -> Result<PoolMigration, ConfigError> {
    if sources.is_empty() {
        return Err(invalid("Select at least one model preset"));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| invalid("Shared policy needs a parent directory"))?;
    let destination = parent
        .canonicalize()
        .map_err(|error| io(parent, error))?
        .join(
            destination
                .file_name()
                .ok_or_else(|| invalid("Invalid shared policy path"))?,
        );
    let original = optional(&destination)?;
    let mut shared = document(
        original
            .as_deref()
            .unwrap_or("schema_version = 1\n[auth.pools]\n"),
    )?;
    let raw: toml::Value =
        toml::from_str(&shared.to_string()).map_err(|_| invalid("Invalid shared pool policy"))?;
    super::pools(&raw)?;
    let mut policies = BTreeMap::<String, toml::Value>::new();
    for (name, policy) in super::pools(&raw)? {
        policies.insert(name.clone(), policy.clone());
    }
    let mut edits = Vec::new();
    let mut seen = std::collections::BTreeSet::from([destination.clone()]);
    for source in sources {
        // canonicalize only the parent: do not hide a symlink at the selected file.
        let parent = source
            .parent()
            .ok_or_else(|| invalid("Preset needs a parent directory"))?;
        let path = parent
            .canonicalize()
            .map_err(|error| io(parent, error))?
            .join(
                source
                    .file_name()
                    .ok_or_else(|| invalid("Invalid preset path"))?,
            );
        if !seen.insert(path.clone()) {
            return Err(invalid("Migration paths must be unique"));
        }
        let contents = optional(&path)?.ok_or_else(|| invalid("Preset does not exist"))?;
        let mut doc = document(&contents)?;
        consolidate(&mut doc, &destination, &mut shared, &mut policies)?;
        let updated = doc.to_string();
        if updated != contents {
            edits.push(Edit {
                path,
                original: Some(contents),
                updated,
            });
        }
    }
    let updated = shared.to_string();
    if original.as_deref() != Some(updated.as_str()) {
        edits.insert(
            0,
            Edit {
                path: destination.clone(),
                original,
                updated,
            },
        );
    }
    Ok(PoolMigration {
        destination,
        final_policy: None,
        edits,
        pools: policies.into_keys().collect(),
    })
}

fn consolidate(
    doc: &mut DocumentMut,
    destination: &Path,
    shared: &mut DocumentMut,
    policies: &mut BTreeMap<String, toml::Value>,
) -> Result<(), ConfigError> {
    if let Some(reference) = doc.get("auth_pool_config")
        && !reference.as_str().is_some_and(|reference| {
            let path = Path::new(reference);
            path == destination || path.canonicalize().ok().as_deref() == Some(destination)
        })
    {
        return Err(invalid(
            "Preset already references another shared pool file",
        ));
    }
    // Pool definitions inside contexts are intentionally untouched.
    if doc.get("auth").and_then(|auth| auth.get("pools")).is_some() {
        // Parse the document instead of assuming table vs inline-table formatting.
        let raw: toml::Value =
            toml::from_str(&doc.to_string()).map_err(|_| invalid("Invalid preset"))?;
        let local = raw
            .get("auth")
            .and_then(|auth| auth.get("pools"))
            .and_then(toml::Value::as_table)
            .ok_or_else(|| invalid("Invalid auth.pools"))?;
        for (name, policy) in local {
            if let Some(existing) = policies.get(name) {
                if existing != policy {
                    return Err(invalid(format!(
                        "Conflicting definitions for pool '{name}'; no files changed"
                    )));
                }
            } else {
                policies.insert(name.clone(), policy.clone());
                let item = doc["auth"]["pools"][name].clone();
                shared["auth"]["pools"][name] = item;
            }
        }
        doc["auth"]
            .as_table_like_mut()
            .ok_or_else(|| invalid("Invalid auth table"))?
            .remove("pools");
    }
    if doc.get("auth_pool_config").is_none() {
        doc.as_table_mut().insert(
            "auth_pool_config",
            value(
                destination
                    .to_str()
                    .ok_or_else(|| invalid("Shared path must be UTF-8"))?,
            ),
        );
    }
    // Ensure collected shared policy still has the restricted contract.
    let raw: toml::Value = toml::from_str(&shared.to_string())
        .map_err(|_| invalid("Invalid resulting shared policy"))?;
    super::pools(&raw)?;
    Ok(())
}
