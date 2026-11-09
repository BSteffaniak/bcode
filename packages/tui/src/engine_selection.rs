//! Transcript engine selection (TUI V2 specification section 7, rules EN1–EN5).
//!
//! The engine is resolved once per process from the `--tui-engine` flag, the
//! [`ENGINE_ENV_VAR`] environment variable and the `[tui] engine` configuration key, in that
//! precedence (EN1), and stays fixed for the process lifetime (EN3): there is no runtime
//! switching path.
//!
//! The CLI records the launch inputs once at startup with [`install_launch_selection`], which
//! validates the environment value so an invalid value is a startup error (EN2). TUI startup
//! then resolves the engine against the loaded configuration exactly once
//! ([`process_engine`]); every later TUI launch in the same process reuses that engine.

use std::sync::OnceLock;

pub use bcode_config::TuiEngine;

/// Environment variable selecting the transcript engine (`v1` or `v2`).
pub const ENGINE_ENV_VAR: &str = "BCODE_TUI_ENGINE";

/// Engine used when neither the flag, the environment nor the configuration selects one.
pub const DEFAULT_ENGINE: TuiEngine = TuiEngine::V1;

/// Why the transcript engine could not be selected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineSelectionError {
    /// The environment variable holds a value that names no engine.
    #[error(
        "invalid {ENGINE_ENV_VAR} value {value:?}: expected one of {}",
        valid_engine_values()
    )]
    InvalidEnvironment {
        /// The rejected value.
        value: String,
    },
    /// The environment variable is not valid Unicode.
    #[error(
        "invalid {ENGINE_ENV_VAR} value: not valid Unicode; expected one of {}",
        valid_engine_values()
    )]
    NonUnicodeEnvironment,
}

/// The accepted engine spellings, for error messages (`v1, v2`).
#[must_use]
pub fn valid_engine_values() -> String {
    TuiEngine::ALL
        .iter()
        .map(|engine| engine.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolve the transcript engine with EN1 precedence: `cli` > `env` > `config` >
/// [`DEFAULT_ENGINE`].
///
/// An empty or whitespace-only `env` value counts as unset. Values are matched ASCII
/// case-insensitively after trimming.
///
/// # Errors
///
/// * [`EngineSelectionError::InvalidEnvironment`] if `env` is set to a value that names no
///   engine. The environment is validated even when `cli` wins, so a misconfigured
///   environment never passes silently (EN2).
pub fn resolve_tui_engine(
    cli: Option<TuiEngine>,
    env: Option<&str>,
    config: Option<TuiEngine>,
) -> Result<TuiEngine, EngineSelectionError> {
    let env = parse_env_value(env)?;
    Ok(cli.or(env).or(config).unwrap_or(DEFAULT_ENGINE))
}

/// Parse an environment value: unset, empty or whitespace-only is `None`.
fn parse_env_value(env: Option<&str>) -> Result<Option<TuiEngine>, EngineSelectionError> {
    let Some(value) = env.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    TuiEngine::parse(value)
        .map(Some)
        .ok_or_else(|| EngineSelectionError::InvalidEnvironment {
            value: value.to_owned(),
        })
}

/// Launch inputs recorded by the CLI before any TUI starts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LaunchSelection {
    /// The `--tui-engine` flag, when given.
    pub cli: Option<TuiEngine>,
    /// The validated [`ENGINE_ENV_VAR`] value, when set.
    pub env: Option<TuiEngine>,
}

impl LaunchSelection {
    /// Validate launch inputs: `env` is the raw environment value, `None` when unset.
    ///
    /// # Errors
    ///
    /// * [`EngineSelectionError::InvalidEnvironment`] if `env` names no engine.
    pub fn new(cli: Option<TuiEngine>, env: Option<&str>) -> Result<Self, EngineSelectionError> {
        Ok(Self {
            cli,
            env: parse_env_value(env)?,
        })
    }

    /// Resolve against the configured engine (EN1). Infallible: the environment was validated
    /// when the selection was built.
    #[must_use]
    pub fn resolve(self, config: Option<TuiEngine>) -> TuiEngine {
        self.cli.or(self.env).or(config).unwrap_or(DEFAULT_ENGINE)
    }
}

static LAUNCH_SELECTION: OnceLock<LaunchSelection> = OnceLock::new();
static PROCESS_ENGINE: OnceLock<TuiEngine> = OnceLock::new();

/// Read [`ENGINE_ENV_VAR`] from the process environment.
///
/// # Errors
///
/// * [`EngineSelectionError::NonUnicodeEnvironment`] if the value is not valid Unicode.
pub fn read_engine_env() -> Result<Option<String>, EngineSelectionError> {
    match std::env::var(ENGINE_ENV_VAR) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(EngineSelectionError::NonUnicodeEnvironment),
    }
}

/// Record the process launch inputs: the `--tui-engine` flag and the validated environment.
///
/// Called once by the CLI at startup, before any TUI launch, with the flag value; the
/// [`ENGINE_ENV_VAR`] value is read and validated here. Later calls keep the first recorded
/// selection (EN3).
///
/// # Errors
///
/// * [`EngineSelectionError::InvalidEnvironment`] if the environment value names no engine.
/// * [`EngineSelectionError::NonUnicodeEnvironment`] if it is not valid Unicode.
pub fn install_launch_selection(cli: Option<TuiEngine>) -> Result<(), EngineSelectionError> {
    let env = read_engine_env()?;
    let selection = LaunchSelection::new(cli, env.as_deref())?;
    let _first_wins = LAUNCH_SELECTION.set(selection);
    Ok(())
}

/// The process transcript engine, fixed for the process lifetime (EN3).
///
/// Resolved on first use from the recorded launch selection and `config`; later calls return
/// the same engine whatever `config` says. Without a recorded launch selection (embedders that
/// bypass the CLI), only `config` and the default apply.
#[must_use]
pub fn process_engine(config: Option<TuiEngine>) -> TuiEngine {
    *PROCESS_ENGINE.get_or_init(|| {
        LAUNCH_SELECTION
            .get()
            .copied()
            .unwrap_or_default()
            .resolve(config)
    })
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_ENGINE, EngineSelectionError, LaunchSelection, TuiEngine, resolve_tui_engine,
    };

    #[test]
    fn default_is_v1_when_nothing_selects_an_engine() {
        assert_eq!(resolve_tui_engine(None, None, None), Ok(TuiEngine::V1));
        assert_eq!(DEFAULT_ENGINE, TuiEngine::V1);
    }

    #[test]
    fn precedence_is_cli_then_env_then_config() {
        use TuiEngine::{V1, V2};
        // Every combination: the first present source wins.
        for cli in [None, Some(V1), Some(V2)] {
            for env in [None, Some("v1"), Some("v2")] {
                for config in [None, Some(V1), Some(V2)] {
                    let expected = cli
                        .or_else(|| env.and_then(TuiEngine::parse))
                        .or(config)
                        .unwrap_or(V1);
                    assert_eq!(
                        resolve_tui_engine(cli, env, config),
                        Ok(expected),
                        "cli={cli:?} env={env:?} config={config:?}"
                    );
                }
            }
        }
        assert_eq!(resolve_tui_engine(Some(V1), Some("v2"), Some(V2)), Ok(V1));
        assert_eq!(resolve_tui_engine(None, Some("v2"), Some(V1)), Ok(V2));
        assert_eq!(resolve_tui_engine(None, None, Some(V2)), Ok(V2));
    }

    #[test]
    fn env_values_are_trimmed_and_case_insensitive_and_empty_is_unset() {
        assert_eq!(
            resolve_tui_engine(None, Some(" V2 "), None),
            Ok(TuiEngine::V2)
        );
        assert_eq!(
            resolve_tui_engine(None, Some(""), Some(TuiEngine::V2)),
            Ok(TuiEngine::V2)
        );
        assert_eq!(
            resolve_tui_engine(None, Some("  "), None),
            Ok(TuiEngine::V1)
        );
    }

    #[test]
    fn invalid_env_is_an_error_naming_the_valid_values_even_when_cli_wins() {
        for cli in [None, Some(TuiEngine::V2)] {
            let error = resolve_tui_engine(cli, Some("v3"), Some(TuiEngine::V1))
                .expect_err("invalid environment value is rejected");
            assert_eq!(
                error,
                EngineSelectionError::InvalidEnvironment {
                    value: "v3".to_owned()
                }
            );
            let message = error.to_string();
            assert!(message.contains("BCODE_TUI_ENGINE"), "{message}");
            assert!(message.contains("\"v3\""), "{message}");
            assert!(message.contains("v1, v2"), "{message}");
        }
    }

    #[test]
    fn launch_selection_validates_once_and_resolves_against_config() {
        assert!(LaunchSelection::new(None, Some("legacy")).is_err());
        let selection = LaunchSelection::new(None, Some("v2")).expect("valid");
        assert_eq!(selection.resolve(Some(TuiEngine::V1)), TuiEngine::V2);
        let selection = LaunchSelection::new(Some(TuiEngine::V1), Some("v2")).expect("valid");
        assert_eq!(selection.resolve(Some(TuiEngine::V2)), TuiEngine::V1);
        assert_eq!(
            LaunchSelection::default().resolve(Some(TuiEngine::V2)),
            TuiEngine::V2
        );
        assert_eq!(LaunchSelection::default().resolve(None), TuiEngine::V1);
    }

    /// X8/EN3: once resolved, a different configured engine does not change the process engine.
    #[test]
    fn process_engine_is_fixed_after_first_resolution() {
        let first = super::process_engine(Some(TuiEngine::V2));
        assert_eq!(super::process_engine(Some(TuiEngine::V1)), first);
        assert_eq!(super::process_engine(None), first);
    }
}
