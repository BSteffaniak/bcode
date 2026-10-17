use super::*;

#[test]
fn explicit_relative_reference_loads_only_policy_and_preserves_context() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pools.toml"),
        "schema_version=1\n[auth.pools.accounts]\nprovider_plugin_id='example'\n",
    )
    .unwrap();
    let preset = dir.path().join("model.toml");
    std::fs::write(&preset, "auth_pool_config='pools.toml'\n[model]\nauth_pool='accounts'\n[contexts.entries.private.auth.pools.private]\nprofiles=['private']\n").unwrap();
    let raw = crate::load_toml_file(&preset).unwrap();
    assert_eq!(
        raw["auth"]["pools"]["accounts"]["provider_plugin_id"].as_str(),
        Some("example")
    );
    assert!(raw.get("auth_pool_config").is_none());
    assert!(
        raw["contexts"]["entries"]["private"]["auth"]["pools"]
            .get("accounts")
            .is_none()
    );
}

#[test]
fn restricted_document_and_duplicate_policy_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let shared = dir.path().join("pools.toml");
    let preset = dir.path().join("model.toml");
    for contents in [
        "[auth.profiles.secret]\nscheme='key'",
        "auth_pool_config='loop.toml'\n[auth.pools.a]",
        "[model]\nauth_pool='a'\n[auth.pools.a]",
    ] {
        std::fs::write(&shared, contents).unwrap();
        let mut raw = toml::from_str("auth_pool_config='pools.toml'").unwrap();
        assert!(expand(&mut raw, &preset).is_err());
    }
    std::fs::write(&shared, "schema_version=1\n[auth.pools.a]").unwrap();
    let mut raw = toml::from_str("auth_pool_config='pools.toml'\n[auth.pools.a]").unwrap();
    assert!(expand(&mut raw, &preset).is_err());
}

#[test]
fn migration_is_reviewed_preserves_comments_accounts_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("pools.toml");
    let sources: Vec<_> = ["a.toml", "b.toml"]
        .map(|name| dir.path().join(name))
        .into();
    let contents = "# keep\n[model]\nauth_pool='accounts'\n[auth.profiles.one]\nscheme='browser'\n[auth.pools.accounts]\nprovider_plugin_id='example'\n[auth.pools.accounts.priming]\nenabled=true\n";
    for source in &sources {
        std::fs::write(source, contents).unwrap();
    }
    let plan = migration::plan(&destination, &sources).unwrap();
    assert!(!destination.exists());
    assert_eq!(plan.pools(), &["accounts"]);
    plan.apply().unwrap();
    for source in &sources {
        let text = read(source).unwrap();
        assert!(text.contains("# keep"));
        let raw = crate::load_toml_file(source).unwrap();
        assert_eq!(
            raw["auth"]["profiles"]["one"]["scheme"].as_str(),
            Some("browser")
        );
        assert_eq!(
            raw["auth"]["pools"]["accounts"]["priming"]["enabled"].as_bool(),
            Some(true)
        );
        assert_eq!(
            read(&source.with_extension("toml.pool-migration.bak")).unwrap(),
            contents
        );
    }
    assert_eq!(
        migration::plan(&destination, &sources)
            .unwrap()
            .paths()
            .count(),
        0
    );
}

#[test]
fn conflicts_and_concurrent_edits_preserve_originals() {
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("pools.toml");
    let a = dir.path().join("a.toml");
    let b = dir.path().join("b.toml");
    std::fs::write(&a, "[auth.pools.accounts]\nprofiles=['a']").unwrap();
    std::fs::write(&b, "[auth.pools.accounts]\nprofiles=['b']").unwrap();
    assert!(migration::plan(&destination, &[a.clone(), b]).is_err());
    assert!(!destination.exists());
    let plan = migration::plan(&destination, std::slice::from_ref(&a)).unwrap();
    std::fs::write(&a, "# edited").unwrap();
    assert!(plan.apply().is_err());
    assert!(!destination.exists());
    assert_eq!(read(&a).unwrap(), "# edited");
}

#[test]
fn file_override_context_and_transport_keep_authority_separate() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pools.toml"),
        "schema_version=1\n[auth.pools.shared]\nprovider_plugin_id='example'\n",
    )
    .unwrap();
    let path = dir.path().join("preset.toml");
    std::fs::write(&path, "auth_pool_config='pools.toml'\n[contexts]\nactive='private'\n[contexts.entries.private.auth.pools.local]\nprofiles=[]\n").unwrap();
    for overrides in [
        crate::ConfigLoadOverrides {
            exclusive_config_path: Some(path.clone()),
            ..Default::default()
        },
        crate::ConfigLoadOverrides {
            env_config_path: Some(path),
            ..Default::default()
        },
    ] {
        let config = crate::load_config_from_paths_with_overrides(&[], &overrides).unwrap();
        assert!(
            config
                .auth
                .pools
                .contains_key(&crate::contexts::qualify("private", "local").unwrap())
        );
        assert!(!config.auth.pools.contains_key("shared"));
        let encoded = crate::encode_effective_config(&config).unwrap();
        assert_eq!(
            crate::decode_effective_config(&encoded).unwrap().auth,
            config.auth
        );
    }
    assert!(crate::decode_effective_config("auth_pool_config='pools.toml'").is_err());
}

#[test]
fn default_opt_in_is_explicit_and_preserves_existing_policy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("preset.toml");
    let destination = dir.path().join("pools.toml");
    std::fs::write(&path, "[auth.pools.accounts]\nprovider_plugin_id='example'\npreferred_profile='two'\nstrategy='round_robin'\n[auth.pools.accounts.priming]\nenabled=true\n").unwrap();
    let mut plan = migration::plan(&destination, std::slice::from_ref(&path)).unwrap();
    assert!(
        plan.mark_default("accounts", "provider", "wrong-owner", "subscriptions")
            .is_err()
    );
    plan.mark_default("accounts", "provider", "example", "subscriptions")
        .unwrap();
    plan.apply().unwrap();
    let config = crate::load_toml_file(&path).unwrap();
    let pool = &config["auth"]["pools"]["accounts"];
    assert_eq!(pool["automatic_provider"].as_str(), Some("provider"));
    assert_eq!(pool["automatic_group"].as_str(), Some("subscriptions"));
    assert_eq!(pool["preferred_profile"].as_str(), Some("two"));
    assert_eq!(pool["strategy"].as_str(), Some("round_robin"));
    assert_eq!(pool["priming"]["enabled"].as_bool(), Some(true));
    let mut repeat = migration::plan(&destination, std::slice::from_ref(&path)).unwrap();
    repeat
        .mark_default("accounts", "provider", "example", "subscriptions")
        .unwrap();
    assert_eq!(repeat.paths().count(), 0);
    repeat.apply().unwrap();
}

#[test]
fn interrupted_publication_can_be_replanned_without_losing_sources() {
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("pools.toml");
    let a = dir.path().join("a.toml");
    let b = dir.path().join("b.toml");
    let original = "[auth.pools.accounts]\nprovider_plugin_id='example'\n";
    for path in [&a, &b] {
        std::fs::write(path, original).unwrap();
    }
    let plan = migration::plan(&destination, &[a.clone(), b.clone()]).unwrap();
    // Simulate loss of execution after shared file and one preset were published.
    std::fs::write(
        &destination,
        "schema_version=1\n[auth.pools.accounts]\nprovider_plugin_id='example'\n",
    )
    .unwrap();
    std::fs::write(a.with_extension("toml.pool-migration.bak"), original).unwrap();
    std::fs::write(
        &a,
        format!("auth_pool_config='{}'\n", destination.display()),
    )
    .unwrap();
    assert!(plan.apply().is_err());
    migration::plan(&destination, &[a.clone(), b.clone()])
        .unwrap()
        .apply()
        .unwrap();
    for path in [&a, &b] {
        assert_eq!(
            crate::load_toml_file(path).unwrap()["auth"]["pools"]["accounts"]["provider_plugin_id"]
                .as_str(),
            Some("example")
        );
    }
}

#[cfg(unix)]
#[test]
fn declarative_symlinks_are_never_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real.toml");
    let link = dir.path().join("preset.toml");
    std::fs::write(&real, "[auth.pools.accounts]").unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(migration::plan(&dir.path().join("pools.toml"), std::slice::from_ref(&link)).is_err());
    assert!(link.is_symlink());
    assert_eq!(read(&real).unwrap(), "[auth.pools.accounts]");
}
