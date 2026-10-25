//! Opt-in client-local commands; never installed in a daemon service instance.

use bcode_plugin_sdk::tui::{PluginFrontendCommand, PluginTuiRegistry, keymap::KeymapAction};
use bcode_vox::{
    config::Config,
    contracts::{Action, DeliveryMode, Error, Status},
    runtime::Handle,
};
use std::{
    ffi::OsString,
    path::PathBuf,
    sync::{Arc, Mutex},
};

trait RuntimeControl: Send + Sync {
    fn action(&self, action: Action) -> bcode_vox::Result<()>;
    fn status(&self) -> Status;
    fn take_text(&self) -> Option<String>;
    fn shutdown(&self);
}

impl RuntimeControl for Handle {
    fn action(&self, action: Action) -> bcode_vox::Result<()> {
        Self::action(self, action).map(|_| ())
    }
    fn status(&self) -> Status {
        Self::status(self)
    }
    fn take_text(&self) -> Option<String> {
        self.take_final_text().map(|value| value.text)
    }
    fn shutdown(&self) {
        Self::shutdown(self);
    }
}

type Factory = dyn Fn() -> Result<Box<dyn RuntimeControl>, String> + Send + Sync;

#[derive(Default)]
struct State {
    runtime: Option<Box<dyn RuntimeControl>>,
    closed: bool,
}

struct Shared {
    state: Mutex<State>,
    factory: Box<Factory>,
}

#[derive(Clone, Copy)]
enum Command {
    Action(Action),
    Status,
}

struct FrontendCommand {
    shared: Arc<Shared>,
    command: Command,
}

fn safe_error(error: &Error) -> String {
    match error {
        Error::Busy => "Vox is busy",
        Error::Cancelled => "Vox operation cancelled",
        Error::Invalid(_) => "Vox configuration is invalid",
        Error::Unsupported(_) => "Vox local capability is unavailable",
        Error::Io(_) | Error::Backend(_) => {
            "Vox local operation failed; do not retry automatically"
        }
    }
    .into()
}

fn config_path(value: Option<OsString>) -> Result<PathBuf, String> {
    value
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "Set VOX_CONFIG to an explicit Vox configuration file".to_owned())
}

const fn host_config(mut config: Config) -> Config {
    // This frontend consumes literal final text; external sinks are never selected.
    config.delivery = DeliveryMode::Host;
    config
}

fn create_runtime() -> Result<Box<dyn RuntimeControl>, String> {
    let path = config_path(std::env::var_os("VOX_CONFIG"))?;
    let config =
        Config::load(&path).map_err(|_| "Cannot load the explicit VOX_CONFIG file".to_owned())?;
    bcode_vox::runtime::spawn(host_config(config))
        .map(|handle| Box::new(handle) as Box<dyn RuntimeControl>)
        .map_err(|error| safe_error(&error))
}

impl PluginFrontendCommand for FrontendCommand {
    fn invoke(&self) -> Result<(), String> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| "Vox frontend state unavailable".to_owned())?;
        if state.closed {
            return Err("Vox frontend has shut down".into());
        }
        // Observation, stop and cancel must not initialize adapters or read config.
        if state.runtime.is_none()
            && matches!(
                self.command,
                Command::Action(Action::Start | Action::Toggle)
            )
        {
            state.runtime = Some((self.shared.factory)()?);
        }
        let Some(runtime) = &state.runtime else {
            return Ok(());
        };
        let result = match self.command {
            Command::Action(action) => runtime.action(action).map_err(|error| safe_error(&error)),
            Command::Status => match runtime.status() {
                Status::Failed { .. } => Err("Vox local operation failed".into()),
                _ => Ok(()),
            },
        };
        drop(state);
        result
    }

    fn drain_text(&self) -> Result<Option<String>, String> {
        let state = self
            .shared
            .state
            .lock()
            .map_err(|_| "Vox frontend state unavailable".to_owned())?;
        if state.closed {
            return Ok(None);
        }
        // All five commands consume the same runtime slot, with no text copies,
        // per-command caches, retry queues, or transcript-bearing diagnostics.
        Ok(state
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.take_text()))
    }

    fn shutdown(&self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        if let Some(runtime) = state.runtime.take() {
            runtime.shutdown();
        }
    }
}

fn registry_with_factory(factory: Box<Factory>) -> PluginTuiRegistry {
    let shared = Arc::new(Shared {
        state: Mutex::new(State::default()),
        factory,
    });
    let mut registry = PluginTuiRegistry::default();
    for (name, command) in [
        ("start", Command::Action(Action::Start)),
        ("stop", Command::Action(Action::Stop)),
        ("toggle", Command::Action(Action::Toggle)),
        ("cancel", Command::Action(Action::Cancel)),
        ("status", Command::Status),
    ] {
        let id = format!("vox.{name}");
        registry.register_frontend_command(
            id.clone(),
            Arc::new(FrontendCommand {
                shared: shared.clone(),
                command,
            }),
        );
        registry.register_keymap_action(KeymapAction {
            id,
            scope: "chat".into(),
            description: format!("Vox: {name}"),
            // No surprise microphone shortcut; users bind the generic action IDs.
            defaults: Vec::new(),
        });
    }
    registry
}

/// Register five commands sharing one lazy, frontend-owned Host-delivery runtime.
///
/// Registration does not read configuration, create adapters, or start capture.
/// Only Start/Toggle initializes from the required `VOX_CONFIG` file. No standalone
/// socket is claimed: separate frontend registries/processes can own independent
/// runtimes. Hosts must poll `drain_text` and insert its literal result once into
/// the currently focused editor; never submit it automatically or log it.
#[must_use]
pub fn tui_registry() -> PluginTuiRegistry {
    registry_with_factory(Box::new(create_runtime))
}

#[cfg(test)]
mod tests;
