#![cfg(all(feature = "app", feature = "daemon-client"))]
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! An app that is not the Bcode executable reaches the user's installed Bcode: it finds `bcode`
//! on `PATH`, attaches only when allowed to, starts the daemon when needed, and talks to it.
//! Run with `app,daemon-client`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bcode_client::{BcodeClient, InstalledBcodeError, InstalledDaemonStart};

const CHILD: &str = "BCODE_INSTALLED_CONSUMER_CHILD";

#[test]
fn consumer_app_starts_and_connects_to_the_installed_bcode() {
    if std::env::var_os(CHILD).is_some() {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(75), installed_consumer()).await
            })
            .expect("installed consumer inner deadline");
        return;
    }
    let root = tempfile::Builder::new()
        .prefix("bcc-")
        .tempdir_in("/tmp")
        .unwrap();
    // The installed Bcode is a separate copy on PATH, never the consumer executable.
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_bcode"), bin.join("bcode")).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "consumer_app_starts_and_connects_to_the_installed_bcode",
            "--nocapture",
        ])
        .env_clear()
        .env(CHILD, "1")
        .env("PATH", &bin)
        .env("HOME", root.path())
        .env("BCODE_STATE_DIR", root.path().join("state"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .current_dir(root.path())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "installed consumer process failed: {status}"
            );
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("installed consumer process exceeded watchdog");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Stops the daemon the test started, also when the test fails.
struct StopDaemon(PathBuf);

impl Drop for StopDaemon {
    fn drop(&mut self) {
        let _ = std::process::Command::new(&self.0)
            .args(["server", "stop", "--force", "--yes"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

async fn installed_consumer() {
    let path = std::env::var_os("PATH").unwrap();
    let installed = Path::new(&path).join("bcode").canonicalize().unwrap();
    let _stop = StopDaemon(installed.clone());

    let attach_only = BcodeClient::for_installed_bcode(InstalledDaemonStart::Never).await;
    assert!(
        matches!(attach_only, Err(InstalledBcodeError::NotRunning)),
        "{attach_only:?}"
    );

    let started = BcodeClient::for_installed_bcode(InstalledDaemonStart::IfNeeded)
        .await
        .expect("start and verify the installed Bcode's daemon");
    assert_eq!(started.executable(), installed);
    let status = started
        .client()
        .verified_server_status()
        .await
        .expect("talk to the installed Bcode's daemon");
    assert_ne!(
        status.daemon.artifact_id,
        Some(bcode_ipc::ArtifactId::current()),
        "the daemon is the installed Bcode's, not one named after the consumer"
    );

    let attached = BcodeClient::for_installed_bcode(InstalledDaemonStart::Never)
        .await
        .expect("attach to the running daemon");
    attached
        .client()
        .verified_server_status()
        .await
        .expect("talk to the running daemon");
}
