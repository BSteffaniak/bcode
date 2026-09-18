use super::*;

fn record(root: &Path) -> DaemonRecord {
    DaemonRecord::current(
        &bcode_ipc::default_endpoint(),
        root.join("daemon.log"),
        None,
        "test-instance".into(),
    )
    .expect("record")
}

fn status(root: &Path, record: &DaemonRecord) -> ExecutionLifetimeStatus {
    execution_lifetime_status(
        root,
        &record.instance_id,
        &record.artifact_id.as_ref().unwrap().to_string(),
        record.state_location_id.as_deref().unwrap(),
    )
}

#[test]
fn release_survives_discovery_cleanup_and_cannot_be_reopened() {
    let root = tempfile::tempdir().unwrap();
    let record = record(root.path());
    assert_eq!(
        status(root.path(), &record),
        ExecutionLifetimeStatus::Missing
    );
    let guard = ExecutionLifetime::begin(root.path(), &record).unwrap();
    let path = crate::write_record(root.path(), &record).unwrap();
    assert_eq!(status(root.path(), &record), ExecutionLifetimeStatus::Live);
    crate::remove_record_path(&path).unwrap();
    assert_eq!(status(root.path(), &record), ExecutionLifetimeStatus::Live);
    assert!(ExecutionLifetime::begin(root.path(), &record).is_err());
    drop(guard);
    for _ in 0..3 {
        assert_eq!(
            status(root.path(), &record),
            ExecutionLifetimeStatus::Released
        );
    }
    assert!(ExecutionLifetime::begin(root.path(), &record).is_err());
}

#[test]
fn unsupported_damaged_and_foreign_evidence_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let record = record(root.path());
    let guard = ExecutionLifetime::begin(root.path(), &record).unwrap();
    drop(guard);
    let mut foreign = record.clone();
    foreign.state_location_id = Some("foreign-location".into());
    assert_eq!(
        status(root.path(), &foreign),
        ExecutionLifetimeStatus::Unverifiable
    );
    assert_eq!(
        execution_lifetime_status(
            root.path(),
            &record.instance_id,
            "foreign-artifact",
            record.state_location_id.as_deref().unwrap()
        ),
        ExecutionLifetimeStatus::Unverifiable
    );
    let path = evidence_path(root.path(), &record.instance_id);
    let original = fs::read(&path).unwrap();
    let mut future: serde_json::Value = serde_json::from_slice(&original).unwrap();
    future["version"] = serde_json::json!(VERSION + 1);
    fs::write(&path, serde_json::to_vec(&future).unwrap()).unwrap();
    assert_eq!(
        status(root.path(), &record),
        ExecutionLifetimeStatus::Unverifiable
    );
    fs::write(&path, b"{").unwrap();
    assert_eq!(
        status(root.path(), &record),
        ExecutionLifetimeStatus::Unverifiable
    );
    fs::write(&path, vec![b' '; usize::try_from(MAX_BYTES + 1).unwrap()]).unwrap();
    assert_eq!(
        status(root.path(), &record),
        ExecutionLifetimeStatus::Unverifiable
    );
}

#[test]
fn maintenance_excludes_execution_and_publishes_released_coordinator() {
    let root = tempfile::tempdir().unwrap();
    let record = record(root.path());
    let guard = ExecutionLifetime::begin(root.path(), &record).unwrap();
    assert!(ExecutionMaintenance::acquire(root.path()).is_err());
    drop(guard);
    let maintenance = ExecutionMaintenance::acquire(root.path()).unwrap();
    let mut next = record.clone();
    next.instance_id = "maintenance-owner".into();
    assert!(ExecutionLifetime::begin(root.path(), &next).is_err());
    maintenance.publish_coordinator(root.path(), &next).unwrap();
    assert_eq!(
        status(root.path(), &next),
        ExecutionLifetimeStatus::Released
    );
    assert!(ExecutionMaintenance::acquire(root.path()).is_err());
    drop(maintenance);
    let mut daemon = record;
    daemon.instance_id = "new-daemon".into();
    assert!(ExecutionLifetime::begin(root.path(), &daemon).is_ok());
}

#[test]
fn lifetime_crash_helper() {
    let Some(root) = std::env::var_os("BCODE_LIFETIME_TEST_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let record = record(&root);
    let _guard = ExecutionLifetime::begin(&root, &record).unwrap();
    fs::write(root.join("ready"), b"ready").unwrap();
    std::thread::sleep(std::time::Duration::from_secs(30));
}

#[test]
fn process_death_releases_published_lifetime_without_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let record = record(root.path());
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "execution_lifetime::tests::lifetime_crash_helper",
            "--exact",
        ])
        .env("BCODE_LIFETIME_TEST_ROOT", root.path())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !root.path().join("ready").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let ready = root.path().join("ready").exists();
    let live = status(root.path(), &record);
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(ready, "child did not publish lifetime");
    assert_eq!(live, ExecutionLifetimeStatus::Live);
    assert_eq!(
        status(root.path(), &record),
        ExecutionLifetimeStatus::Released
    );
}

#[cfg(unix)]
#[test]
fn symlink_evidence_is_not_followed() {
    let root = tempfile::tempdir().unwrap();
    let foreign = tempfile::tempdir().unwrap();
    let record = record(root.path());
    std::os::unix::fs::symlink(
        foreign.path(),
        root.path().join("daemon-execution-lifetimes"),
    )
    .unwrap();
    assert!(ExecutionLifetime::begin(root.path(), &record).is_err());
    assert_eq!(
        status(root.path(), &record),
        ExecutionLifetimeStatus::Unverifiable
    );
    assert_eq!(fs::read_dir(foreign.path()).unwrap().count(), 0);
}
