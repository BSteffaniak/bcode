//! In-memory Git config interpretation; every source/include is explicitly read by caller.
use std::path::{Path, PathBuf};

type ReadConfig<'a> = dyn FnMut(&Path) -> Result<Option<Vec<u8>>, &'static str> + 'a;

pub(super) struct Settings {
    pub excludes: Option<PathBuf>,
    pub worktree: bool,
    pub remaining: usize,
    pub incomplete: Option<&'static str>,
}
impl Settings {
    pub const fn new() -> Self {
        Self {
            excludes: None,
            worktree: false,
            remaining: 64,
            incomplete: None,
        }
    }
    pub fn load(
        &mut self,
        path: &Path,
        home: Option<&Path>,
        read: &mut ReadConfig<'_>,
    ) -> Result<(), &'static str> {
        if self.remaining == 0 {
            return Err("git_config_include_limit");
        }
        self.remaining -= 1;
        let Some(bytes) = read(path)? else {
            return Ok(());
        };
        let config = gix_config::File::from_bytes_no_includes(
            &bytes,
            gix_config::file::Metadata::default(),
            gix_config::file::init::Options::default(),
        )
        .map_err(|_| "git_config_invalid")?;
        for section in config.sections() {
            let header = section.header();
            let name: &[u8] = header.name().as_ref();
            let body = section.body();
            if name.eq_ignore_ascii_case(b"includeif") {
                self.incomplete = Some("git_config_conditional_include_unsupported");
            } else if name.eq_ignore_ascii_case(b"include") && header.subsection_name().is_none() {
                for value in body.values("path") {
                    let value = std::str::from_utf8(value.as_ref())
                        .map_err(|_| "git_config_path_invalid")?;
                    let target =
                        expand(value, path.parent().ok_or("git_config_path_invalid")?, home)?;
                    self.load(&target, home, read)?;
                }
            } else if name.eq_ignore_ascii_case(b"core") && header.subsection_name().is_none() {
                if let Some(value) = body.value("excludesfile") {
                    let value = std::str::from_utf8(value.as_ref())
                        .map_err(|_| "git_config_path_invalid")?;
                    // Git resolves relative excludesFile against the invocation cwd.
                    self.excludes = Some(PathBuf::from(value));
                }
            } else if name.eq_ignore_ascii_case(b"extensions")
                && header.subsection_name().is_none()
                && let Some(value) = body.value_implicit("worktreeconfig")
            {
                self.worktree = match value {
                    None => true,
                    Some(value) => match std::str::from_utf8(value.as_ref())
                        .map_err(|_| "git_config_invalid")?
                        .to_ascii_lowercase()
                        .as_str()
                    {
                        "true" | "yes" | "on" | "1" => true,
                        "false" | "no" | "off" | "0" => false,
                        _ => return Err("git_config_invalid"),
                    },
                };
            }
        }
        Ok(())
    }
}

pub(super) fn expand(
    value: &str,
    base: &Path,
    home: Option<&Path>,
) -> Result<PathBuf, &'static str> {
    if value.contains(['\0', '\r', '\n']) {
        return Err("git_config_path_invalid");
    }
    if value.contains("%(prefix)") {
        return Err("git_config_install_prefix_unsupported");
    }
    if value.starts_with('~') {
        let relative = value
            .strip_prefix("~/")
            .ok_or("git_config_user_expansion_unsupported")?;
        super::inventory::normalize_absolute(home.ok_or("git_config_home_unavailable")?, relative)
    } else {
        super::inventory::normalize_absolute(base, value)
    }
}

#[cfg(not(test))]
pub(super) fn user_sources(home: Option<&Path>, xdg: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(xdg) = xdg {
        paths.push(xdg.join("git/config"));
    } else if let Some(home) = home {
        paths.push(home.join(".config/git/config"));
    }
    if let Some(home) = home {
        paths.push(home.join(".gitconfig"));
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    #[test]
    fn include_precedence_denial_and_conditional_are_explicit() {
        let files = BTreeMap::from([
            (PathBuf::from("/cfg/main"), b"[core]\n excludesFile=first\n[include]\n path=child\n[core]\n excludesFile=last\n".to_vec()),
            (PathBuf::from("/cfg/child"), b"[core]\n excludesFile=included\n".to_vec()),
        ]);
        let mut settings = Settings::new();
        settings
            .load(Path::new("/cfg/main"), None, &mut |path| {
                Ok(files.get(path).cloned())
            })
            .unwrap();
        assert_eq!(settings.excludes, Some("last".into()));
        let mut settings = Settings::new();
        assert_eq!(
            settings.load(Path::new("/cfg/main"), None, &mut |path| {
                if path == Path::new("/cfg/child") {
                    Err("git_ignore_resource_denied")
                } else {
                    Ok(files.get(path).cloned())
                }
            }),
            Err("git_ignore_resource_denied")
        );
        let mut settings = Settings::new();
        settings
            .load(Path::new("/cfg/conditional"), None, &mut |_| {
                Ok(Some(b"[includeIf \"gitdir:x\"]\n path=secret\n".to_vec()))
            })
            .unwrap();
        assert_eq!(
            settings.incomplete,
            Some("git_config_conditional_include_unsupported")
        );
    }
}
