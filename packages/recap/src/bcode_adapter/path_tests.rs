#[cfg(unix)]
use super::*;

/// Nix/Home Manager commonly link config files into a store while keeping
/// relative include files beside the user-facing link.
#[cfg(unix)]
#[tokio::test]
async fn symlinked_configuration_retains_relative_pool_base_for_create_and_resume() {
    let root = tempfile::tempdir().unwrap();
    let managed = root.path().join("managed-store");
    let user = root.path().join("user");
    let providers = user.join("providers");
    std::fs::create_dir_all(&managed).unwrap();
    std::fs::create_dir_all(&providers).unwrap();
    let target = managed.join("sol.toml");
    std::fs::write(&target, "auth_pool_config='../auth-pools.toml'\n[model]\nprovider_plugin_id='bcode.openai-compatible'\nmodel_id='test-model'\nauth_pool='accounts'\n").unwrap();
    std::fs::write(user.join("auth-pools.toml"), "schema_version=1\n[auth.pools.accounts]\nprovider_plugin_id='bcode.openai-compatible'\nprofiles=[]\n").unwrap();
    let path = providers.join("sol.toml");
    std::os::unix::fs::symlink(&target, &path).unwrap();
    let selection = crate::connections::Selection {
        config_path: Some(path.clone()),
        ..Default::default()
    };
    let retained = retained_options(&selection, 8192, Duration::from_secs(180)).unwrap();
    assert!(retained.config_path.is_absolute());
    assert_eq!(retained.config_path.file_name(), path.file_name());
    assert_ne!(retained.config_path, target.canonicalize().unwrap());
    let fingerprint = configuration_fingerprint(&retained).unwrap();
    let restored: BcodeOptions =
        serde_json::from_slice(&serde_json::to_vec(&retained).unwrap()).unwrap();
    assert_eq!(configuration_fingerprint(&restored).unwrap(), fingerprint);
    assert!(
        configuration_fingerprint(&BcodeOptions {
            config_path: target,
            ..restored
        })
        .is_err()
    );
    // Changed shared policy is still detected; preserving the link is not a
    // compatibility-check bypass.
    std::fs::write(user.join("auth-pools.toml"), "schema_version=1\n[auth.pools.other]\nprovider_plugin_id='bcode.openai-compatible'\nprofiles=[]\n").unwrap();
    assert_ne!(configuration_fingerprint(&retained).unwrap(), fingerprint);
}
