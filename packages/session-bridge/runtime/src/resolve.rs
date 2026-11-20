//! Runtime selection, separated from process and environment access so it is testable.

use std::path::{Path, PathBuf};

use crate::{AttachError, AutoPreference, RuntimeMode};

/// The installed Bcode an application names explicitly, most specific first.
///
/// `BCODE_BIN` sits between the two: a flag outranks it, and it outranks configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NamedExecutable {
    /// From the application's command line, such as `--bcode`.
    pub flag: Option<PathBuf>,
    /// From the application's configuration. Applications pass it only from trusted
    /// configuration: it names a program to run.
    pub configured: Option<PathBuf>,
}

/// The runtime [`resolve`] chose, with why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Start this installed Bcode's bridge.
    Installed {
        /// The executable.
        executable: PathBuf,
        /// Why, for the application to show.
        reason: String,
    },
    /// Host Bcode in this process.
    Embedded {
        /// Why, for the application to show.
        reason: String,
    },
}

/// Choose a runtime from the process environment (`BCODE_BIN`, `PATH`).
///
/// # Errors
/// [`AttachError::Unavailable`] when the chosen kind of Bcode does not exist; see the crate
/// documentation for the rules.
pub fn resolve(
    mode: RuntimeMode,
    preference: AutoPreference,
    named: &NamedExecutable,
    embedded_available: bool,
    naming_hint: &str,
) -> Result<Selection, AttachError> {
    let bcode_bin = std::env::var_os(bcode_session_bridge::BCODE_BIN_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    choose(
        &Inputs {
            mode,
            preference,
            named,
            bcode_bin: bcode_bin.as_deref(),
            on_path: on_path().as_deref(),
            embedded_available,
            naming_hint,
        },
        is_executable_file,
    )
}

pub struct Inputs<'a> {
    pub mode: RuntimeMode,
    pub preference: AutoPreference,
    pub named: &'a NamedExecutable,
    pub bcode_bin: Option<&'a Path>,
    pub on_path: Option<&'a Path>,
    pub embedded_available: bool,
    pub naming_hint: &'a str,
}

pub fn choose(
    inputs: &Inputs<'_>,
    is_executable: impl Fn(&Path) -> bool,
) -> Result<Selection, AttachError> {
    let named = inputs
        .named
        .flag
        .as_deref()
        .map(|path| (path, "the command line names it"))
        .or_else(|| inputs.bcode_bin.map(|path| (path, "BCODE_BIN names it")))
        .or_else(|| {
            inputs
                .named
                .configured
                .as_deref()
                .map(|path| (path, "configuration names it"))
        });
    let installed = |named: Option<(&Path, &str)>| -> Result<Selection, AttachError> {
        match named {
            Some((path, why)) if is_executable(path) => Ok(Selection::Installed {
                executable: path.to_owned(),
                reason: why.to_owned(),
            }),
            Some((path, _)) => Err(AttachError::Unavailable(format!(
                "There is no Bcode executable at {}: point {} at the Bcode executable, or \
                 remove it.",
                path.display(),
                inputs.naming_hint
            ))),
            None => inputs.on_path.map_or_else(
                || {
                    Err(AttachError::Unavailable(format!(
                        "Bcode is not installed: put `bcode` on PATH, or name it with {}.",
                        inputs.naming_hint
                    )))
                },
                |path| {
                    Ok(Selection::Installed {
                        executable: path.to_owned(),
                        reason: "found on PATH".to_owned(),
                    })
                },
            ),
        }
    };
    let embedded = |reason: &str| -> Result<Selection, AttachError> {
        if inputs.embedded_available {
            Ok(Selection::Embedded {
                reason: reason.to_owned(),
            })
        } else {
            Err(AttachError::Unavailable(
                "This build has no embedded Bcode: install Bcode, or use the installed runtime."
                    .to_owned(),
            ))
        }
    };
    match inputs.mode {
        RuntimeMode::Installed => installed(named),
        RuntimeMode::Embedded => embedded("the embedded runtime is selected"),
        RuntimeMode::Auto => {
            if named.is_some() {
                return installed(named);
            }
            match inputs.preference {
                AutoPreference::Embedded if inputs.embedded_available => {
                    embedded("auto prefers the embedded Bcode")
                }
                AutoPreference::Embedded => installed(None).map(|selection| {
                    with_reason(selection, "auto prefers embedded, but this build has none")
                }),
                AutoPreference::Installed
                    if inputs.on_path.is_none() && inputs.embedded_available =>
                {
                    embedded("auto prefers an installed Bcode, but none was found")
                }
                AutoPreference::Installed => installed(None),
            }
        }
    }
}

fn with_reason(selection: Selection, reason: &str) -> Selection {
    match selection {
        Selection::Installed { executable, .. } => Selection::Installed {
            executable,
            reason: reason.to_owned(),
        },
        Selection::Embedded { .. } => Selection::Embedded {
            reason: reason.to_owned(),
        },
    }
}

fn on_path() -> Option<PathBuf> {
    let name = if cfg!(windows) { "bcode.exe" } else { "bcode" };
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable_file(candidate))
}

/// Whether `path` is an executable file.
pub fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HINT: &str = "`--bcode`, BCODE_BIN, or `[app] bcode`";

    fn named(flag: Option<&str>, configured: Option<&str>) -> NamedExecutable {
        NamedExecutable {
            flag: flag.map(PathBuf::from),
            configured: configured.map(PathBuf::from),
        }
    }

    fn pick(
        mode: RuntimeMode,
        preference: AutoPreference,
        named: &NamedExecutable,
        bcode_bin: Option<&str>,
        on_path: Option<&str>,
        embedded_available: bool,
    ) -> Result<Selection, AttachError> {
        choose(
            &Inputs {
                mode,
                preference,
                named,
                bcode_bin: bcode_bin.map(Path::new),
                on_path: on_path.map(Path::new),
                embedded_available,
                naming_hint: HINT,
            },
            |path| !path.to_string_lossy().contains("missing"),
        )
    }

    fn executable(selection: &Result<Selection, AttachError>) -> Option<&str> {
        match selection {
            Ok(Selection::Installed { executable, .. }) => executable.to_str(),
            _ => None,
        }
    }

    #[test]
    fn named_executables_follow_flag_then_bcode_bin_then_configuration() {
        let all = named(Some("/flag/bcode"), Some("/config/bcode"));
        for preference in [AutoPreference::Embedded, AutoPreference::Installed] {
            let chosen = pick(
                RuntimeMode::Auto,
                preference,
                &all,
                Some("/env/bcode"),
                Some("/path/bcode"),
                true,
            );
            assert_eq!(executable(&chosen), Some("/flag/bcode"));
        }
        let chosen = pick(
            RuntimeMode::Auto,
            AutoPreference::Embedded,
            &named(None, Some("/config/bcode")),
            Some("/env/bcode"),
            None,
            true,
        );
        assert_eq!(executable(&chosen), Some("/env/bcode"));
        let chosen = pick(
            RuntimeMode::Auto,
            AutoPreference::Embedded,
            &named(None, Some("/config/bcode")),
            None,
            None,
            true,
        );
        assert_eq!(executable(&chosen), Some("/config/bcode"));
    }

    #[test]
    fn a_named_executable_that_is_missing_fails_instead_of_falling_back() {
        let chosen = pick(
            RuntimeMode::Auto,
            AutoPreference::Embedded,
            &named(Some("/missing/bcode"), None),
            None,
            Some("/path/bcode"),
            true,
        );
        assert!(matches!(chosen, Err(AttachError::Unavailable(message)) if message.contains(HINT)));
    }

    #[test]
    fn auto_follows_the_configured_preference_when_nothing_is_named() {
        let nothing = NamedExecutable::default();
        assert!(matches!(
            pick(
                RuntimeMode::Auto,
                AutoPreference::Embedded,
                &nothing,
                None,
                Some("/path/bcode"),
                true
            ),
            Ok(Selection::Embedded { .. })
        ));
        assert_eq!(
            executable(&pick(
                RuntimeMode::Auto,
                AutoPreference::Installed,
                &nothing,
                None,
                Some("/path/bcode"),
                true
            )),
            Some("/path/bcode")
        );
    }

    #[test]
    fn auto_uses_the_other_kind_only_when_the_preferred_one_is_absent() {
        let nothing = NamedExecutable::default();
        assert_eq!(
            executable(&pick(
                RuntimeMode::Auto,
                AutoPreference::Embedded,
                &nothing,
                None,
                Some("/path/bcode"),
                false
            )),
            Some("/path/bcode"),
            "prefers embedded, but this build has none"
        );
        assert!(matches!(
            pick(
                RuntimeMode::Auto,
                AutoPreference::Installed,
                &nothing,
                None,
                None,
                true
            ),
            Ok(Selection::Embedded { .. })
        ));
        assert!(matches!(
            pick(
                RuntimeMode::Auto,
                AutoPreference::Installed,
                &nothing,
                None,
                None,
                false
            ),
            Err(AttachError::Unavailable(_))
        ));
    }

    #[test]
    fn explicit_modes_never_substitute() {
        let nothing = NamedExecutable::default();
        assert!(matches!(
            pick(
                RuntimeMode::Embedded,
                AutoPreference::Installed,
                &nothing,
                None,
                Some("/path/bcode"),
                false
            ),
            Err(AttachError::Unavailable(_))
        ));
        assert!(matches!(
            pick(
                RuntimeMode::Installed,
                AutoPreference::Embedded,
                &nothing,
                None,
                None,
                true
            ),
            Err(AttachError::Unavailable(_))
        ));
    }
}
