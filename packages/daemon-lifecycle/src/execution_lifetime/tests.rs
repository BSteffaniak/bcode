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

#[cfg(unix)]
struct ForkChild {
    pid: libc::pid_t,
    release_fd: libc::c_int,
}

#[cfg(unix)]
impl ForkChild {
    fn reap(&mut self) -> io::Result<i32> {
        // SAFETY: this guard exclusively owns the descriptor and unreaped child.
        unsafe {
            if self.release_fd >= 0 {
                libc::close(self.release_fd);
                self.release_fd = -1;
            }
            let mut status = 0;
            loop {
                if libc::waitpid(self.pid, &raw mut status, 0) == self.pid {
                    self.pid = 0;
                    return Ok(status);
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

#[cfg(unix)]
impl Drop for ForkChild {
    fn drop(&mut self) {
        if self.pid > 0 {
            let _ = self.reap();
        }
    }
}

#[cfg(unix)]
#[test]
fn release_is_not_delayed_by_unrelated_fork_child() {
    if crate::run_isolated_fork_fixture(
        "execution_lifetime::tests::release_is_not_delayed_by_unrelated_fork_child",
    ) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let record = record(root.path());
    let guard = ExecutionLifetime::begin(root.path(), &record).unwrap();
    let store = bcode_workflow_store::WorkflowStore::open_in_state_dir(root.path()).unwrap();
    let ownership = OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.path().join("workflows/workflow.lock"))
        .unwrap();
    assert!(
        ownership.try_lock().is_err(),
        "live store retains ownership"
    );
    let mut pipe = [0; 2];
    // SAFETY: valid output buffer; the child executes only async-signal-safe
    // libc calls and exits without running inherited Rust destructors.
    unsafe {
        assert_eq!(libc::pipe(pipe.as_mut_ptr()), 0);
        let pid = libc::fork();
        assert!(pid >= 0);
        if pid == 0 {
            libc::close(pipe[1]);
            libc::alarm(10);
            let mut byte = 0_u8;
            let read = libc::read(pipe[0], std::ptr::from_mut(&mut byte).cast(), 1);
            libc::_exit(i32::from(read != 0));
        }
        libc::close(pipe[0]);
        let mut child = ForkChild {
            pid,
            release_fd: pipe[1],
        };
        drop(guard);
        drop(store);
        let store_released = ownership.try_lock();
        let observed = status(root.path(), &record);
        let maintenance = ExecutionMaintenance::acquire(root.path());
        let mut child_status = 0;
        let waited = libc::waitpid(pid, &raw mut child_status, libc::WNOHANG);
        let still_alive = waited == 0;
        if waited == pid {
            // WNOHANG may already have reaped an unexpectedly exited child.
            child.pid = 0;
            libc::close(child.release_fd);
            child.release_fd = -1;
        }
        assert!(still_alive, "child exited before release observations");
        let child_status = child.reap().expect("reap fork child");
        assert!(libc::WIFEXITED(child_status));
        assert_eq!(libc::WEXITSTATUS(child_status), 0);
        assert!(
            store_released.is_ok(),
            "store fence released before child exit"
        );
        assert_eq!(observed, ExecutionLifetimeStatus::Released);
        assert!(
            maintenance.is_ok(),
            "released execution must permit maintenance"
        );
    }
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
    let resumed = ExecutionLifetime::begin(root.path(), &daemon);
    assert!(
        resumed.is_ok(),
        "execution admission after maintenance release: {resumed:?}"
    );
}

#[cfg(unix)]
#[test]
fn maintenance_owner_release_survives_fork_and_child_drop_preserves_parent_fence() {
    if crate::run_isolated_fork_fixture(
        "execution_lifetime::tests::maintenance_owner_release_survives_fork_and_child_drop_preserves_parent_fence",
    ) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let coordinator = record(root.path());
    let maintenance = ExecutionMaintenance::acquire(root.path()).unwrap();
    maintenance
        .publish_coordinator(root.path(), &coordinator)
        .unwrap();
    // Retain a separate inherited descriptor after the child drops its guard.
    // This makes owner release, rather than descendant exit, necessary.
    let inherited = maintenance.file.try_clone().unwrap();
    let mut ready = [0; 2];
    let mut release = [0; 2];
    // SAFETY: output buffers are valid. The child performs the specific guard
    // drop under test (PID check and descriptor close), otherwise only libc
    // descriptor operations, then _exit without unrelated Rust destructors.
    unsafe {
        assert_eq!(libc::pipe(ready.as_mut_ptr()), 0);
        assert_eq!(libc::pipe(release.as_mut_ptr()), 0);
        let pid = libc::fork();
        assert!(pid >= 0);
        if pid == 0 {
            libc::close(ready[0]);
            libc::close(release[1]);
            libc::alarm(10);
            drop(maintenance);
            let byte = b'R';
            if libc::write(ready[1], std::ptr::from_ref(&byte).cast(), 1) != 1 {
                libc::_exit(125);
            }
            libc::close(ready[1]);
            let mut byte = 0_u8;
            let read = libc::read(release[0], std::ptr::from_mut(&mut byte).cast(), 1);
            // inherited remains open until _exit.
            libc::_exit(i32::from(read != 0));
        }
        libc::close(ready[1]);
        libc::close(release[0]);
        let mut child = ForkChild {
            pid,
            release_fd: release[1],
        };
        drop(inherited);
        let mut poll = libc::pollfd {
            fd: ready[0],
            events: libc::POLLIN,
            revents: 0,
        };
        let notified = libc::poll(&raw mut poll, 1, 3_000);
        let mut byte = 0_u8;
        let read = if notified == 1 {
            libc::read(ready[0], std::ptr::from_mut(&mut byte).cast(), 1)
        } else {
            -1
        };
        libc::close(ready[0]);
        assert_eq!(read, 1, "child guard drop notification missing");
        assert_eq!(byte, b'R');
        let mut daemon = coordinator.clone();
        daemon.instance_id = "post-maintenance-daemon".into();
        assert!(
            ExecutionMaintenance::acquire(root.path()).is_err(),
            "child drop must not release parent maintenance fence"
        );
        assert!(
            ExecutionLifetime::begin(root.path(), &daemon).is_err(),
            "execution must remain fenced until owner release"
        );
        assert_eq!(
            status(root.path(), &coordinator),
            ExecutionLifetimeStatus::Released
        );
        let mut child_status = 0;
        assert_eq!(
            libc::waitpid(pid, &raw mut child_status, libc::WNOHANG),
            0,
            "gated descendant must survive observations"
        );
        drop(maintenance);
        let execution = ExecutionLifetime::begin(root.path(), &daemon)
            .expect("owner release must admit execution while inherited descendant survives");
        assert_eq!(status(root.path(), &daemon), ExecutionLifetimeStatus::Live);
        assert!(ExecutionMaintenance::acquire(root.path()).is_err());
        drop(execution);
        let next_maintenance = ExecutionMaintenance::acquire(root.path())
            .expect("released execution must admit a new maintenance owner");
        let child_status = child.reap().expect("reap gated descendant");
        assert!(libc::WIFEXITED(child_status));
        assert_eq!(libc::WEXITSTATUS(child_status), 0);
        drop(next_maintenance);
    }
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

#[test]
fn image_removal_requires_offline_fence_and_preserves_canonical_state() {
    let root = tempfile::tempdir().unwrap();
    let images = root.path().join("daemon-images");
    fs::create_dir_all(images.join("artifact/digest")).unwrap();
    let path = images.join("artifact/digest/bcode");
    fs::write(&path, b"image").unwrap();
    super::super::open_image_lease(root.path(), &path, true).unwrap();
    let canonical = root.path().join("sessions");
    fs::create_dir(&canonical).unwrap();
    fs::write(canonical.join("history"), b"canonical").unwrap();
    let record = record(root.path());
    let lifetime = ExecutionLifetime::begin(root.path(), &record).unwrap();
    assert!(ExecutionMaintenance::acquire(root.path()).is_err());
    assert!(images.exists());
    drop(lifetime);
    let maintenance = ExecutionMaintenance::acquire(root.path()).unwrap();
    maintenance.remove_daemon_images().unwrap();
    assert!(!images.join("artifact").exists());
    assert_eq!(fs::read(canonical.join("history")).unwrap(), b"canonical");
    assert!(ExecutionLifetime::begin(root.path(), &record).is_err());
    drop(maintenance);
}
