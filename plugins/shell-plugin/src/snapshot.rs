//! Materialization and complete post-command validation of inline targets.
use bcode_shell_models::DeliveredSnapshot;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub fn prepare(
    plan: &crate::contracts::ShellWorkflowCommandPlan,
) -> Result<Option<tempfile::TempDir>, String> {
    let Some(target) = &plan.delivered_snapshot else {
        return Ok(None);
    };
    if !plan.observe_files.is_empty() || plan.expected_content.is_some() || plan.commands.is_empty()
    {
        return Err(
            "snapshot verification requires commands and forbids selected-file observations".into(),
        );
    }
    materialize(target).map(Some)
}

pub fn materialize(target: &DeliveredSnapshot) -> Result<tempfile::TempDir, String> {
    target.validate()?;
    let directory = tempfile::tempdir().map_err(|_| "snapshot directory unavailable")?;
    for (path, contents) in &target.files {
        let file = directory.path().join(path);
        std::fs::create_dir_all(file.parent().ok_or("invalid snapshot path")?)
            .map_err(|_| "snapshot directory creation failed")?;
        std::fs::write(file, contents).map_err(|_| "snapshot materialization failed")?;
    }
    Ok(directory)
}

pub fn matches(directory: &Path, target: &DeliveredSnapshot) -> bool {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return false;
    };
    let mut roots = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        roots.push(PathBuf::from(entry.file_name()));
        if roots.len() > 64 {
            return false;
        }
    }
    let Ok(observed) = crate::content_observation::observe(directory, &roots) else {
        return false;
    };
    let mut expected_directories = std::collections::BTreeSet::new();
    for path in target.files.keys() {
        let mut parent = Path::new(path).parent();
        while let Some(directory) = parent.filter(|directory| !directory.as_os_str().is_empty()) {
            expected_directories.insert(directory.to_path_buf());
            parent = directory.parent();
        }
    }
    if observed
        .directories
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        != expected_directories
    {
        return false;
    }
    #[cfg(unix)]
    for path in target.files.keys() {
        use std::os::unix::fs::PermissionsExt;
        let Ok(metadata) = std::fs::symlink_metadata(directory.join(path)) else {
            return false;
        };
        if metadata.permissions().mode() & 0o111 != 0 {
            return false;
        }
    }
    observed.files.len() == target.files.len()
        && observed.files.iter().all(|file| {
            file.path
                .to_str()
                .and_then(|path| target.files.get(path))
                .is_some_and(|content| {
                    file.sha256 == hex::encode(Sha256::digest(content.as_bytes()))
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target() -> DeliveredSnapshot {
        DeliveredSnapshot {
            version: 1,
            files: [("a".into(), "original".into())].into(),
        }
    }
    #[test]
    fn unsupported_and_incomplete_targets_fail_closed() {
        for path in ["../escape", "/absolute", "a/./b", "a\\b", "", "a//b"] {
            let mut target = target();
            target.files = [(path.into(), "bytes".into())].into();
            assert!(materialize(&target).is_err());
        }
        let mut target = target();
        target.version = 2;
        assert!(materialize(&target).is_err());
        target.version = 1;
        target.files.insert("a/child".into(), "collision".into());
        assert!(materialize(&target).is_err());
    }

    #[test]
    fn success_stale_incomplete_and_failure() {
        let target = target();
        let dir = materialize(&target).unwrap();
        assert!(matches(dir.path(), &target));
        let mut evidence = bcode_shell_models::SnapshotVerification {
            version: 1,
            target: target.clone(),
            commands_passed: true,
            target_unchanged: true,
        };
        assert!(evidence.accept(&target).is_ok());
        let mut stale = target.clone();
        stale.files.insert("a".into(), "changed".into());
        assert!(evidence.accept(&stale).is_err());
        evidence.commands_passed = false;
        assert!(evidence.accept(&target).is_err());
        std::fs::write(dir.path().join("extra"), "uncovered").unwrap();
        assert!(!matches(dir.path(), &target));
        let mut incomplete = target;
        incomplete.files.clear();
        assert!(materialize(&incomplete).is_err());
    }
}
