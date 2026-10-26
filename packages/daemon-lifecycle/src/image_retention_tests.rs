//! Process-level coverage of image retention, independent of daemon registry records.
use super::*;
use std::process::{Child, Command, Stdio};

fn image(root: &Path, name: &str) -> PathBuf {
    let path = root.join("daemon-images").join(name).join("digest/bcode");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"image fixture").unwrap();
    open_image_lease(root, &path, true).unwrap();
    path
}

struct LeaseProcess(Child);
impl Drop for LeaseProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "subprocess fixture for image retention tests"]
fn image_lease_child() {
    let root = PathBuf::from(std::env::var_os("BCODE_IMAGE_LEASE_TEST_ROOT").unwrap());
    let path = root.join("daemon-images/historical/digest/bcode");
    let publication = DaemonImageUseGuard::acquire(&root).unwrap();
    let lease = acquire_image_use(&root, &path)
        .unwrap()
        .expect("managed image lease");
    fs::write(&path, b"image fixture").unwrap();
    drop(publication);
    fs::write(root.join("ready"), b"ready").unwrap();
    loop {
        if root.join("exit").exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
        std::hint::black_box(&lease);
    }
}

fn spawn_lease(root: &Path) -> LeaseProcess {
    LeaseProcess(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "image_retention_tests::image_lease_child",
                "--ignored",
            ])
            .env("BCODE_IMAGE_LEASE_TEST_ROOT", root)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

fn wait_ready(root: &Path, child: &mut LeaseProcess) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !root.join("ready").exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "lease child exited before ready"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "lease child timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn live_image_without_registry_survives_and_exit_or_crash_releases_it() {
    for crash in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let active = image(root.path(), "historical");
        let unused = image(root.path(), "unused");
        let mut child = spawn_lease(root.path());
        wait_ready(root.path(), &mut child);
        assert_eq!(cleanup_stale_daemon_images(root.path()).unwrap(), 1);
        assert!(active.exists());
        assert!(!unused.exists());
        assert!(read_records(root.path()).is_empty());
        if crash {
            child.0.kill().unwrap();
        } else {
            fs::write(root.path().join("exit"), b"exit").unwrap();
        }
        let status = child.0.wait().unwrap();
        assert_eq!(status.success(), !crash);
        assert_eq!(cleanup_stale_daemon_images(root.path()).unwrap(), 1);
        assert!(!active.exists());
    }
}

#[test]
#[ignore = "subprocess fixture for the process-global executable pin"]
fn pinned_executable_child() {
    let pinned = pinned_bcode_executable().unwrap();
    let root = PathBuf::from(std::env::var_os("BCODE_IMAGE_LEASE_TEST_ROOT").unwrap());
    fs::write(
        root.join("pinned"),
        pinned.path().as_os_str().as_encoded_bytes(),
    )
    .unwrap();
    loop {
        std::thread::sleep(Duration::from_millis(10));
        std::hint::black_box(pinned);
    }
}

#[test]
fn process_pin_holds_the_published_image_lease() {
    let root = tempfile::tempdir().unwrap();
    let mut child = LeaseProcess(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "image_retention_tests::pinned_executable_child",
                "--ignored",
            ])
            .env("BCODE_STATE_DIR", root.path())
            .env("BCODE_IMAGE_LEASE_TEST_ROOT", root.path())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !root.path().join("pinned").exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "pin child exited before publishing"
        );
        assert!(std::time::Instant::now() < deadline, "pin child timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    let published = PathBuf::from(fs::read_to_string(root.path().join("pinned")).unwrap());
    assert!(published.starts_with(root.path().join("daemon-images")));
    let lease = open_image_lease(root.path(), &published, false).unwrap();
    assert!(matches!(
        lease.try_lock(),
        Err(fs::TryLockError::WouldBlock)
    ));
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    lease.try_lock().unwrap();
    assert!(published.exists());
}

#[test]
fn offline_maintenance_also_preserves_live_client_images() {
    let root = tempfile::tempdir().unwrap();
    let active = image(root.path(), "historical");
    let unused = image(root.path(), "unused");
    let mut child = spawn_lease(root.path());
    wait_ready(root.path(), &mut child);
    let maintenance = ExecutionMaintenance::acquire(root.path()).unwrap();
    maintenance.remove_daemon_images().unwrap();
    assert!(active.exists());
    assert!(!unused.exists());
}

#[test]
fn startup_cannot_publish_while_cleanup_holds_the_global_lock() {
    let root = tempfile::tempdir().unwrap();
    let active = image(root.path(), "historical");
    let cleanup = try_acquire_daemon_image_cleanup_lock(root.path())
        .unwrap()
        .unwrap();
    let mut child = spawn_lease(root.path());
    std::thread::sleep(Duration::from_millis(100));
    assert!(!root.path().join("ready").exists());
    assert!(child.0.try_wait().unwrap().is_none());
    drop(cleanup);
    wait_ready(root.path(), &mut child);
    assert_eq!(cleanup_stale_daemon_images(root.path()).unwrap(), 0);
    assert!(active.exists());
}

#[test]
fn unleased_legacy_images_and_unreadable_lease_evidence_are_preserved() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("daemon-images/legacy/digest/bcode");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"legacy").unwrap();
    assert_eq!(cleanup_stale_daemon_images(root.path()).unwrap(), 0);
    let publication = DaemonImageUseGuard::acquire(root.path()).unwrap();
    assert!(acquire_image_use(root.path(), &path).unwrap().is_none());
    drop(publication);
    let lease_path = root
        .path()
        .join("daemon-image-leases/legacy/digest/bcode.lock");
    fs::create_dir_all(&lease_path).unwrap(); // A directory cannot be opened as a lock file.
    assert_eq!(cleanup_stale_daemon_images(root.path()).unwrap(), 0);
    assert!(path.exists());
}
