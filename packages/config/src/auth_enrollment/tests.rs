use crate::*;

#[test]
fn managed_opt_in_roundtrips_with_provider_identity() {
    let mut pool: AuthPoolConfig = toml::from_str("provider_plugin_id = 'example.plugin'\nautomatic_provider = 'example'\nautomatic_group = 'subscription'\n").unwrap();
    // Stabilize the existing priming default discrepancy; this test owns managed markers,
    // not omitted-vs-empty priming table semantics.
    pool.priming.fallback_reprime_after = Some("7d".to_owned());
    let roundtrip: AuthPoolConfig = toml::from_str(&toml::to_string(&pool).unwrap()).unwrap();
    assert_eq!(pool, roundtrip);
    let legacy: AuthPoolConfig = toml::from_str("provider_plugin_id = 'example.plugin'\n").unwrap();
    assert!(legacy.automatic_provider.is_none());
    assert!(legacy.automatic_group.is_none());
}

#[test]
fn atomic_publish_conflict_unknown_state_and_legacy_registry() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("registry.json");
    let expected = RuntimeAuthSubscriptions::default();
    let profile = RuntimeAuthProfile {
        provider_id: "example".to_owned(),
        owner_plugin_id: "example.plugin".to_owned(),
        backend: "sshenv".to_owned(),
        scheme: "browser".to_owned(),
        storage_profile: "one".to_owned(),
        ..Default::default()
    };
    let pool = RuntimeAuthSubscriptionPool {
        automatic_group: Some("subscription".to_owned()),
        provider_plugin_id: Some("example.plugin".to_owned()),
        owner_plugin_id: Some("example.plugin".to_owned()),
        provider_id: Some("example".to_owned()),
        profiles: vec![RuntimeAuthSubscriptionProfile {
            auth_profile: "one".to_owned(),
            ..Default::default()
        }],
        ..Default::default()
    };
    super::publish_auth_enrollment_at(&path, &expected, "one", &profile, Some(("default", &pool)))
        .unwrap();
    let saved = read_auth_subscriptions_for_update(&path).unwrap();
    assert!(saved.profiles.contains_key("one"));
    assert_eq!(saved.pools["default"].profiles[0].auth_profile, "one");
    let mut updated_profile = profile.clone();
    updated_profile.device_seal = Some("disabled".to_owned());
    let mut updated_pool = pool;
    updated_pool.profiles[0].device_seal = Some("disabled".to_owned());
    super::publish_auth_enrollment_at(
        &path,
        &saved,
        "one",
        &updated_profile,
        Some(("default", &updated_pool)),
    )
    .unwrap();
    let updated = read_auth_subscriptions_for_update(&path).unwrap();
    assert_eq!(
        updated.profiles["one"].device_seal.as_deref(),
        Some("disabled")
    );
    assert_eq!(
        updated.pools["default"].profiles[0].device_seal.as_deref(),
        Some("disabled")
    );
    let before = std::fs::read(&path).unwrap();
    assert!(super::publish_auth_enrollment_at(&path, &expected, "two", &profile, None).is_err());
    assert_eq!(before, std::fs::read(&path).unwrap());
    std::fs::write(&path, br#"{"pools":{},"future_field":true}"#).unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(super::publish_auth_enrollment_at(&path, &expected, "one", &profile, None).is_err());
    assert_eq!(before, std::fs::read(&path).unwrap());
    std::fs::write(
        &path,
        br#"{"pools":{"manual":{"profiles":[],"future_policy":"unknown"}}}"#,
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(super::publish_auth_enrollment_at(&path, &expected, "one", &profile, None).is_err());
    assert_eq!(before, std::fs::read(&path).unwrap());
    std::fs::write(&path, br#"{"pools":{"manual":{"profiles":[]}}}"#).unwrap();
    let old = read_auth_subscriptions_for_update(&path).unwrap();
    assert!(old.pools["manual"].automatic_group.is_none());
    super::publish_auth_enrollment_at(&path, &old, "one", &profile, None).unwrap();
    assert!(
        read_auth_subscriptions_for_update(&path).unwrap().pools["manual"]
            .automatic_group
            .is_none()
    );
}
