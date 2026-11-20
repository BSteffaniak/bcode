//! The ready-made embedded daemon host: registering it routes this process's daemon starts to
//! a runtime it owns, named for the application, and shutting it down stops the daemon it
//! started so a later client finds none.
//!
//! The registration is process-wide, so this file is its own test binary with one test.
#![cfg(unix)]

use std::time::Duration;

use bcode_server::InProcessServerOptions;
use bcode_server::embedded_host::{EmbeddedDaemonHost, EmbeddedDaemonHostOptions};

#[test]
fn the_embedded_host_serves_and_stops_the_daemon_in_process() {
    let root = tempfile::tempdir().expect("temporary directory");
    // Bcode binds its socket only in a private directory.
    let run = root.path().join("run");
    std::fs::create_dir(&run).expect("socket directory");
    std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .expect("private socket directory");
    // SAFETY: the test sets the environment before any runtime or thread exists, and it is
    // the only test in this binary.
    unsafe {
        std::env::set_var("HOME", root.path().join("home"));
        std::env::set_var("XDG_CONFIG_HOME", root.path().join("config"));
        std::env::set_var(bcode_config::BCODE_STATE_DIR_ENV, root.path().join("state"));
        std::env::set_var("BCODE_SOCKET", run.join("bcode.sock"));
        std::env::remove_var(bcode_ipc::BCODE_IPC_ENDPOINT_ENV);
        std::env::remove_var(bcode_ipc::BCODE_IPC_ENDPOINT_NAMESPACE_ENV);
    }
    assert!(EmbeddedDaemonHost::get().is_none());
    let options = EmbeddedDaemonHostOptions {
        server: InProcessServerOptions {
            static_plugins: Vec::new(),
            host_name: "embedded host test".to_owned(),
            bcode_cli: None,
            idle_shutdown: false,
            private_env: Vec::new(),
        },
        thread_name: "embedded-test-daemon".to_owned(),
    };
    let host = EmbeddedDaemonHost::register(options.clone()).expect("host registers");
    assert!(bcode_daemon_lifecycle::hosts_daemon_in_process());
    // Registering again returns the same host.
    assert!(std::ptr::eq(
        host,
        EmbeddedDaemonHost::register(options).expect("registered host")
    ));
    // The daemon's runtime threads carry the application's name.
    let thread = host
        .handle()
        .block_on(async {
            tokio::task::spawn(async { std::thread::current().name().map(str::to_owned) }).await
        })
        .expect("task on the daemon runtime");
    assert_eq!(thread.as_deref(), Some("embedded-test-daemon"));

    let client_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("client runtime");
    client_runtime.block_on(async {
        let client = bcode_client::BcodeClient::default_endpoint();
        client
            .ensure_daemon_available()
            .await
            .expect("the client starts the daemon through the host");
        let status = client.server_status().await.expect("daemon answers");
        assert_eq!(
            status.daemon.pid, None,
            "an embedded daemon advertises no process"
        );

        assert!(
            host.shutdown(Duration::from_secs(30)).await,
            "the daemon stops in time"
        );
        let after = bcode_client::BcodeClient::default_endpoint()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning)
            .server_status()
            .await;
        assert!(after.is_err(), "the daemon still answers after shutdown");
    });
}
