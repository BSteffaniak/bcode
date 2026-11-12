//! Synthetic namespaces and inert fake executables only; no ambient environment mutation.
use super::*;
use std::{
    ffi::OsStr,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    input: ClaudeCodeDiscovery,
}
impl Fixture {
    fn new() -> Self {
        let root = env::temp_dir().join(format!(
            "bcode-claude-discovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let home = root.join("home");
        fs::create_dir_all(home.join(".claude")).unwrap();
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        write_executable(&bin.join("claude"), "#!/bin/sh\nexit 99\n");
        Self {
            root,
            input: ClaudeCodeDiscovery {
                home,
                search_path: bin.into_os_string(),
                config_directory: None,
            },
        }
    }
    fn candidate(&self) -> ClaudeCodeCandidate {
        discover_candidates_in(&self.input).pop().unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn write_executable(path: &Path, source: &str) {
    fs::write(path, source).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
}
fn evidence(candidate: &ClaudeCodeCandidate) -> ClaudeCodeCompatibility {
    ClaudeCodeCompatibility {
        candidate: candidate.clone(),
        exact_version: "fixture-1.2.3".into(),
        verified_native_auth_status: true,
        verified_minimal_settings: true,
    }
}

#[test]
fn passive_discovery_and_explicit_minimal_native_preparation() {
    let fixture = Fixture::new();
    let marker = fixture.root.join("must-not-execute");
    let bin = PathBuf::from(&fixture.input.search_path);
    // Any attempt to discover version/status by executing the fake is observable.
    write_executable(
        &bin.join("claude"),
        &format!("#!/bin/sh\ntouch '{}'\nexit 99\n", marker.display()),
    );
    // A secret-shaped file is deliberately invalid/unreadable as settings JSON.
    fs::write(
        fixture.input.home.join(".claude/.credentials.json"),
        "not-json-synthetic-only",
    )
    .unwrap();
    let candidate = fixture.candidate();
    let proof = evidence(&candidate);
    assert_eq!(
        candidate.prepare(false, &proof).err(),
        Some(ClaudeCodePreparationError::ConsentRequired)
    );
    let config = candidate.prepare(true, &proof).unwrap();
    assert_eq!(config.exact_version, "fixture-1.2.3");
    assert_eq!(config.setting_sources, Some(Vec::new()));
    assert!(config.verified_native_auth_status);
    assert!(!config.verified_native_auth);
    assert!(!config.verified_session_state);
    assert!(matches!(config.billing, ApiBilling::Native));
    let instance = candidate.harness_instance("synthetic-instance".into(), "native".into());
    assert_eq!(instance.auth.custody, HarnessAuthCustody::NativeManaged);
    assert_eq!(
        instance.config_namespace,
        instance.auth.credential_namespace
    );
    assert_eq!(
        instance.executable,
        candidate.executable().to_str().unwrap()
    );
    assert!(config.credential_selection(&instance).unwrap().is_none());
    let encoded = serde_json::to_value(&config).unwrap();
    assert_eq!(encoded["setting_sources"], serde_json::json!([]));
    assert_eq!(encoded["billing"]["route"], "native");
    let decoded: InstanceConfig = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded.exact_version, config.exact_version);
    assert!(!marker.exists());
}

#[test]
fn version_strings_never_supply_capability_evidence() {
    let fixture = Fixture::new();
    let candidate = fixture.candidate();
    let mut proof = evidence(&candidate);
    for version in ["", " fixture-1.2.3", "fixture\n1.2.3"] {
        proof.exact_version = version.into();
        assert_eq!(
            candidate.prepare(true, &proof).err(),
            Some(ClaudeCodePreparationError::CompatibilityRequired)
        );
    }
    proof.exact_version = "fixture-1.2.3".into();
    proof.verified_native_auth_status = false;
    assert!(candidate.prepare(true, &proof).is_err());
    proof.verified_native_auth_status = true;
    proof.verified_minimal_settings = false;
    assert!(candidate.prepare(true, &proof).is_err());
}

#[test]
fn executable_update_requires_new_exact_installation_evidence_even_same_version() {
    let fixture = Fixture::new();
    let original = fixture.candidate();
    let original_proof = evidence(&original);
    // Replace with different bytes while pretending --version would be unchanged.
    let replacement = original.executable().with_extension("replacement");
    write_executable(&replacement, "#!/bin/sh\n# replacement\nexit 99\n");
    fs::rename(replacement, original.executable()).unwrap();
    assert_eq!(
        original.prepare(true, &original_proof).err(),
        Some(ClaudeCodePreparationError::InstallationChanged)
    );
    let updated = fixture.candidate();
    assert_eq!(
        updated.prepare(true, &original_proof).err(),
        Some(ClaudeCodePreparationError::CompatibilityRequired)
    );
    assert!(updated.prepare(true, &evidence(&updated)).is_ok());
    fs::write(updated.executable(), "different-in-place-update").unwrap();
    assert_eq!(
        updated.prepare(true, &evidence(&updated)).err(),
        Some(ClaudeCodePreparationError::InstallationChanged)
    );
}

#[test]
fn namespace_override_missing_namespace_relative_search_and_deduplication() {
    let mut fixture = Fixture::new();
    let bin = fixture.input.search_path.clone();
    fixture.input.search_path =
        env::join_paths([OsStr::new(""), OsStr::new("relative"), &bin, &bin]).unwrap();
    assert_eq!(discover_candidates_in(&fixture.input).len(), 1);
    let alternative = fixture.root.join("selected-config");
    fs::create_dir(&alternative).unwrap();
    fixture.input.config_directory = Some(alternative.clone());
    let candidate = fixture.candidate();
    assert_eq!(
        candidate.config_directory(),
        alternative.canonicalize().unwrap()
    );
    fixture.input.config_directory = Some("relative-config".into());
    assert_eq!(discover_candidates_in(&fixture.input), Vec::new());
    fixture.input.config_directory = Some(fixture.root.join("missing"));
    assert_eq!(discover_candidates_in(&fixture.input), Vec::new());
    fs::remove_dir(&alternative).unwrap();
    assert_eq!(
        candidate.prepare(true, &evidence(&candidate)).err(),
        Some(ClaudeCodePreparationError::InstallationChanged)
    );
}

#[test]
#[cfg(unix)]
fn symlink_updates_and_nonexecutable_files_do_not_reuse_admission() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let fixture = Fixture::new();
    let executable = PathBuf::from(&fixture.input.search_path).join("claude");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(discover_candidates_in(&fixture.input), Vec::new());
    let target = fixture.root.join("native-target");
    write_executable(&target, "#!/bin/sh\nexit 99\n");
    fs::remove_file(&executable).unwrap();
    symlink(&target, &executable).unwrap();
    let candidate = fixture.candidate();
    assert_eq!(candidate.executable(), target.canonicalize().unwrap());
    let next_target = fixture.root.join("next-target");
    write_executable(&next_target, "#!/bin/sh\nexit 98\n");
    fs::remove_file(&executable).unwrap();
    symlink(&next_target, &executable).unwrap();
    let updated = fixture.candidate();
    assert!(updated.prepare(true, &evidence(&candidate)).is_err());
}
