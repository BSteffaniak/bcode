use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct Observed {
    builds: AtomicUsize,
    actions: Mutex<Vec<Action>>,
    text: Mutex<Option<String>>,
    shutdowns: AtomicUsize,
    failed: bool,
}
struct Fake(Arc<Observed>);
impl RuntimeControl for Fake {
    fn action(&self, action: Action) -> bcode_vox::Result<()> {
        self.0.actions.lock().unwrap().push(action);
        if self.0.failed {
            Err(Error::Backend("private transcript credential".into()))
        } else {
            Ok(())
        }
    }
    fn status(&self) -> Status {
        if self.0.failed {
            Status::Failed {
                message: "private transcript credential".into(),
            }
        } else {
            Status::Idle
        }
    }
    fn take_text(&self) -> Option<String> {
        self.0.text.lock().unwrap().take()
    }
    fn shutdown(&self) {
        self.0.shutdowns.fetch_add(1, Ordering::SeqCst);
    }
}
fn registry(observed: &Arc<Observed>) -> PluginTuiRegistry {
    let observed = observed.clone();
    registry_with_factory(Box::new(move || {
        observed.builds.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Fake(observed.clone())))
    }))
}

#[test]
fn observation_and_teardown_do_not_initialize_runtime() {
    let observed = Arc::new(Observed::default());
    let registry = registry(&observed);
    for id in ["vox.status", "vox.stop", "vox.cancel"] {
        let command = registry.frontend_command(id).unwrap();
        command.invoke().unwrap();
        assert_eq!(command.drain_text().unwrap(), None);
    }
    registry.frontend_command("vox.status").unwrap().shutdown();
    assert_eq!(observed.builds.load(Ordering::SeqCst), 0);
    assert!(
        registry
            .frontend_command("vox.start")
            .unwrap()
            .invoke()
            .is_err()
    );
    assert_eq!(observed.builds.load(Ordering::SeqCst), 0);
}

#[test]
fn all_commands_share_one_lazy_runtime_and_one_final_text_slot() {
    let observed = Arc::new(Observed::default());
    let registry = registry(&observed);
    assert_eq!(observed.builds.load(Ordering::SeqCst), 0);
    for id in [
        "vox.start",
        "vox.stop",
        "vox.toggle",
        "vox.cancel",
        "vox.status",
    ] {
        registry.frontend_command(id).unwrap().invoke().unwrap();
    }
    assert_eq!(observed.builds.load(Ordering::SeqCst), 1);
    assert_eq!(
        *observed.actions.lock().unwrap(),
        [Action::Start, Action::Stop, Action::Toggle, Action::Cancel]
    );
    *observed.text.lock().unwrap() = Some("literal 雪\nfinal".into());
    assert_eq!(
        registry
            .frontend_command("vox.stop")
            .unwrap()
            .drain_text()
            .unwrap(),
        Some("literal 雪\nfinal".into())
    );
    for id in [
        "vox.start",
        "vox.stop",
        "vox.toggle",
        "vox.cancel",
        "vox.status",
    ] {
        assert_eq!(
            registry.frontend_command(id).unwrap().drain_text().unwrap(),
            None
        );
        registry.frontend_command(id).unwrap().shutdown();
    }
    assert_eq!(observed.shutdowns.load(Ordering::SeqCst), 1);
    assert!(
        registry
            .frontend_command("vox.toggle")
            .unwrap()
            .invoke()
            .is_err()
    );
}

#[test]
fn runtime_failures_are_normalized_without_automatic_action_retries() {
    let observed = Arc::new(Observed {
        failed: true,
        ..Observed::default()
    });
    let registry = registry(&observed);
    let error = registry
        .frontend_command("vox.start")
        .unwrap()
        .invoke()
        .unwrap_err();
    assert!(!error.contains("private"));
    assert_eq!(observed.actions.lock().unwrap().len(), 1);
    let error = registry
        .frontend_command("vox.status")
        .unwrap()
        .invoke()
        .unwrap_err();
    assert_eq!(error, "Vox local operation failed");
    assert_eq!(observed.actions.lock().unwrap().len(), 1);
}

#[test]
fn failed_initialization_does_not_install_a_runtime_or_retry_it_implicitly() {
    let builds = Arc::new(AtomicUsize::new(0));
    let count = builds.clone();
    let registry = registry_with_factory(Box::new(move || {
        count.fetch_add(1, Ordering::SeqCst);
        Err("Cannot load the explicit VOX_CONFIG file".into())
    }));
    assert!(
        registry
            .frontend_command("vox.start")
            .unwrap()
            .invoke()
            .is_err()
    );
    assert_eq!(
        registry
            .frontend_command("vox.stop")
            .unwrap()
            .drain_text()
            .unwrap(),
        None
    );
    registry
        .frontend_command("vox.status")
        .unwrap()
        .invoke()
        .unwrap();
    assert_eq!(builds.load(Ordering::SeqCst), 1);
}

#[test]
fn explicit_path_is_required_and_host_delivery_is_forced() {
    assert!(config_path(None).is_err());
    assert!(config_path(Some(OsString::new())).is_err());
    assert_eq!(
        config_path(Some("relative-explicit.toml".into())).unwrap(),
        PathBuf::from("relative-explicit.toml")
    );
    let config = Config {
        delivery: DeliveryMode::Clipboard,
        ..Config::default()
    };
    assert_eq!(host_config(config).delivery, DeliveryMode::Host);
}

#[test]
fn keymap_declarations_select_registered_commands_without_surprise_defaults() {
    let registry = registry(&Arc::new(Observed::default()));
    assert_eq!(registry.keymap_actions().len(), 5);
    for definition in registry.keymap_actions() {
        assert_eq!(definition.scope, "chat");
        assert!(definition.defaults.is_empty());
        assert!(registry.frontend_command(&definition.id).is_some());
    }
}
