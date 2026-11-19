//! An application that hosts the daemon inside its own process: clients in that process start
//! it through the registered host instead of launching `<executable> server run`, and the
//! hosted daemon leaves no lifecycle record or pinned executable image behind.
//!
//! The registration is process-wide, so this file is its own test binary with one test.
#![cfg(unix)]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bcode_agent_runtime::CancellationToken;
use bcode_server::InProcessServerOptions;

fn entries(directory: &Path) -> Vec<String> {
    std::fs::read_dir(directory)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_registered_host_serves_the_daemon_in_process() {
    let root = tempfile::tempdir().expect("temporary directory");
    let state = root.path().join("state");
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
        std::env::set_var(bcode_config::BCODE_STATE_DIR_ENV, &state);
        std::env::set_var("BCODE_SOCKET", run.join("bcode.sock"));
        std::env::remove_var(bcode_ipc::BCODE_IPC_ENDPOINT_ENV);
        std::env::remove_var(bcode_ipc::BCODE_IPC_ENDPOINT_NAMESPACE_ENV);
    }

    // The host runs the daemon on its own runtime, with Bcode's thread stack size.
    let daemon_runtime = tokio::runtime::Builder::new_multi_thread()
        .thread_name("bcode-daemon")
        .thread_stack_size(8 * 1024 * 1024)
        .enable_all()
        .build()
        .expect("daemon runtime");
    let handle = daemon_runtime.handle().clone();
    let shutdown = CancellationToken::new();
    let starts = Arc::new(AtomicUsize::new(0));
    let served = Arc::new(Mutex::new(None));
    let registered = bcode_daemon_lifecycle::register_in_process_daemon_host(Arc::new({
        let (shutdown, starts, served) =
            (shutdown.clone(), Arc::clone(&starts), Arc::clone(&served));
        move |endpoint| {
            starts.fetch_add(1, Ordering::SeqCst);
            let task = handle.spawn(bcode_server::run_in_process(
                endpoint.clone(),
                InProcessServerOptions {
                    static_plugins: Vec::new(),
                    host_name: "in-process test host".to_owned(),
                    bcode_cli: None,
                    idle_shutdown: false,
                    private_env: vec!["BCODE_TEST_HOST_CREDENTIAL".to_owned()],
                },
                shutdown.clone(),
            ));
            *served.lock().expect("served task") = Some(task);
            Ok(())
        }
    }));
    assert!(registered);
    assert!(bcode_daemon_lifecycle::hosts_daemon_in_process());

    let client_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("client runtime");
    client_runtime.block_on(async {
        // An ordinary auto-starting client starts the daemon through the host, and a second
        // one finds it running.
        let client = bcode_client::BcodeClient::default_endpoint();
        if let Err(error) = client.ensure_daemon_available().await {
            let task = served.lock().expect("served task").take();
            let outcome = match task {
                Some(task) if task.is_finished() => format!("{:?}", task.await),
                Some(_) => "still running".to_owned(),
                None => "never started".to_owned(),
            };
            panic!("in-process daemon did not start: {error}; daemon task: {outcome}");
        }
        let status = client.server_status().await.expect("daemon answers");
        assert_eq!(status.daemon.namespace, bcode_ipc::daemon_namespace());
        // No process id is advertised, so nothing can signal the host as a daemon.
        assert_eq!(status.daemon.pid, None);
        bcode_client::BcodeClient::default_endpoint()
            .ensure_daemon_available()
            .await
            .expect("running daemon is found");
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    });
    assert!(
        bcode_config::host_environment::host_private_env()
            .contains(&"BCODE_TEST_HOST_CREDENTIAL".to_owned())
    );

    // Nothing lets process-stopping tools signal the host, and the host's executable is not
    // pinned as Bcode.
    let records: Vec<String> = entries(&state.join("daemons"))
        .into_iter()
        .filter(|name| Path::new(name).extension().is_some_and(|ext| ext == "json"))
        .collect();
    assert_eq!(records, Vec::<String>::new());
    assert_eq!(entries(&state.join("daemon-images")), Vec::<String>::new());

    shutdown.cancel();
    let task = served
        .lock()
        .expect("served task")
        .take()
        .expect("host started the daemon");
    daemon_runtime
        .block_on(task)
        .expect("daemon task joins")
        .expect("daemon shuts down cleanly");
}
