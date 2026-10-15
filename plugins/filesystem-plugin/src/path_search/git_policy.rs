//! Exact-grant, no-follow repository-local exclusion resources.
use super::{SearchPathsGrant, WorkspaceResourceGrant, ignore_policy, inventory};
use std::io::Read as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

pub(super) struct Policy {
    pub rules: Vec<ignore_policy::Rule>,
    pub fingerprint: [u8; 32],
    pub incomplete: Option<&'static str>,
}
struct Sources {
    home: Option<PathBuf>,
    xdg: Option<PathBuf>,
    configs: Vec<PathBuf>,
    incomplete: Option<&'static str>,
}
#[cfg(not(test))]
impl Sources {
    fn environment() -> Result<Self, &'static str> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        if home.as_ref().is_some_and(|path| !path.is_absolute())
            || xdg.as_ref().is_some_and(|path| !path.is_absolute())
        {
            return Err("git_config_environment_invalid");
        }
        let mut configs = Vec::new();
        let no_system = std::env::var_os("GIT_CONFIG_NOSYSTEM").is_some_and(|value| {
            !matches!(value.to_str(), Some("0" | "false" | "no" | "off" | ""))
        });
        if !no_system {
            configs.push(std::env::var_os("GIT_CONFIG_SYSTEM").map_or_else(
                || {
                    PathBuf::from(if cfg!(target_os = "macos") {
                        "/private/etc/gitconfig"
                    } else {
                        "/etc/gitconfig"
                    })
                },
                PathBuf::from,
            ));
        }
        if let Some(global) = std::env::var_os("GIT_CONFIG_GLOBAL") {
            configs.push(global.into());
        } else {
            configs.extend(super::git_config::user_sources(
                home.as_deref(),
                xdg.as_deref(),
            ));
        }
        configs.retain(|path| path != Path::new("/dev/null"));
        if configs.iter().any(|path| !path.is_absolute()) {
            return Err("git_config_environment_invalid");
        }
        let incomplete = if std::env::var_os("GIT_CONFIG_COUNT").is_some_and(|value| value != "0")
            || std::env::var_os("GIT_CONFIG_PARAMETERS").is_some()
        {
            Some("git_config_environment_overrides_unsupported")
        } else if home.is_none() && std::env::var_os("GIT_CONFIG_GLOBAL").is_none() {
            Some("git_config_home_unavailable")
        } else {
            None
        };
        Ok(Self {
            home,
            xdg,
            configs,
            incomplete,
        })
    }
}
struct Reader<'a> {
    grant: &'a SearchPathsGrant,
    authorize: &'a dyn Fn(&str) -> Option<WorkspaceResourceGrant>,
    external: &'a dyn Fn(&Path) -> Option<WorkspaceResourceGrant>,
    tick: &'a mut dyn FnMut() -> Result<(), &'static str>,
    seen: std::collections::BTreeMap<PathBuf, Option<[u8; 32]>>,
    bytes: usize,
}
impl Reader<'_> {
    fn read(&mut self, path: &Path) -> Result<Option<Vec<u8>>, &'static str> {
        use sha2::Digest as _;
        if self.seen.len() >= 128 {
            return Err("git_policy_resource_limit");
        }
        let bytes = read(path, self.grant, self.authorize, self.external, self.tick)?;
        self.bytes = self
            .bytes
            .saturating_add(bytes.as_ref().map_or(0, Vec::len));
        if self.bytes > 1024 * 1024 {
            return Err("git_policy_byte_limit");
        }
        let digest = bytes
            .as_ref()
            .map(|bytes| sha2::Sha256::digest(bytes).into());
        if self
            .seen
            .get(path)
            .is_some_and(|previous| *previous != digest)
        {
            return Err("git_metadata_changed");
        }
        self.seen.insert(path.to_owned(), digest);
        Ok(bytes)
    }
    fn finish(self) -> Result<[u8; 32], &'static str> {
        use sha2::Digest as _;
        let mut hash = sha2::Sha256::new();
        for (path, expected) in &self.seen {
            let bytes = read(path, self.grant, self.authorize, self.external, self.tick)?;
            let actual: Option<[u8; 32]> = bytes
                .as_ref()
                .map(|bytes| sha2::Sha256::digest(bytes).into());
            if actual != *expected {
                return Err("git_metadata_changed");
            }
            hash.update(path.as_os_str().as_encoded_bytes());
            hash.update([u8::from(actual.is_some())]);
            if let Some(digest) = actual {
                hash.update(digest);
            }
        }
        Ok(hash.finalize().into())
    }
}
pub(super) fn load(
    grant: &SearchPathsGrant,
    authorize: &dyn Fn(&str) -> Option<WorkspaceResourceGrant>,
    external: &dyn Fn(&Path) -> Option<WorkspaceResourceGrant>,
    tick: &mut dyn FnMut() -> Result<(), &'static str>,
) -> Result<Policy, &'static str> {
    #[cfg(not(test))]
    let sources = Sources::environment()?;
    #[cfg(test)]
    let sources = Sources {
        home: None,
        xdg: None,
        configs: Vec::new(),
        incomplete: None,
    };
    load_sources(grant, authorize, external, tick, sources)
}
fn load_sources(
    grant: &SearchPathsGrant,
    authorize: &dyn Fn(&str) -> Option<WorkspaceResourceGrant>,
    external: &dyn Fn(&Path) -> Option<WorkspaceResourceGrant>,
    tick: &mut dyn FnMut() -> Result<(), &'static str>,
    sources: Sources,
) -> Result<Policy, &'static str> {
    let mut reader = Reader {
        grant,
        authorize,
        external,
        tick,
        seen: std::collections::BTreeMap::new(),
        bytes: 0,
    };
    let mut settings = super::git_config::Settings::new();
    settings.incomplete = sources.incomplete;
    for path in &sources.configs {
        settings.load(path, sources.home.as_deref(), &mut |path| reader.read(path))?;
    }
    let repository = inventory::discover_repository(&grant.workspace_root, reader.tick)?;
    let info = if let Some(repository) = repository {
        if repository != grant.workspace_root {
            return Err("git_ancestor_ignore_sources_not_evaluated");
        }
        let gitdir = git_directory(&repository, &mut reader)?;
        let common = common_directory(&gitdir, &mut reader)?;
        settings.load(
            &common.join("config"),
            sources.home.as_deref(),
            &mut |path| reader.read(path),
        )?;
        if settings.worktree {
            settings.load(
                &gitdir.join("config.worktree"),
                sources.home.as_deref(),
                &mut |path| reader.read(path),
            )?;
        }
        Some(common.join("info/exclude"))
    } else {
        None
    };
    let global_ignore = match settings.excludes {
        Some(path) if path.as_os_str().is_empty() => None,
        Some(path) => Some(super::git_config::expand(
            path.to_str().ok_or("git_config_path_invalid")?,
            &grant.workspace_root,
            sources.home.as_deref(),
        )?),
        None => sources
            .xdg
            .map(|path| path.join("git/ignore"))
            .or_else(|| sources.home.map(|path| path.join(".config/git/ignore"))),
    };
    let mut rules = Vec::new();
    let mut cost = 0_usize;
    for path in global_ignore.into_iter().chain(info) {
        if let Some(bytes) = reader.read(&path)? {
            let text = std::str::from_utf8(&bytes).map_err(|_| "ignore_policy_invalid")?;
            rules.extend(ignore_policy::parse(
                Path::new(""),
                text,
                256_usize.saturating_sub(rules.len()),
                &mut |bytes| {
                    (reader.tick)()?;
                    cost = cost.saturating_add(bytes);
                    if cost > 1024 * 1024 {
                        Err("ignore_policy_limit")
                    } else {
                        Ok(())
                    }
                },
            )?);
        }
    }
    Ok(Policy {
        rules,
        fingerprint: reader.finish()?,
        incomplete: settings.incomplete,
    })
}
fn git_directory(repository: &Path, reader: &mut Reader<'_>) -> Result<PathBuf, &'static str> {
    let git = repository.join(".git");
    if super::super::confined::open_directory(&git).is_ok() {
        return Ok(git);
    }
    let bytes = reader.read(&git)?.ok_or("git_pointer_unavailable")?;
    let text = std::str::from_utf8(&bytes).map_err(|_| "git_pointer_invalid")?;
    let target = text
        .strip_prefix("gitdir: ")
        .ok_or("git_pointer_invalid")?
        .trim_end_matches(['\r', '\n']);
    if target.is_empty() || target.contains(['\r', '\n', '\0']) {
        return Err("git_pointer_invalid");
    }
    inventory::normalize_absolute(repository, target)
}
fn common_directory(gitdir: &Path, reader: &mut Reader<'_>) -> Result<PathBuf, &'static str> {
    let Some(bytes) = reader.read(&gitdir.join("commondir"))? else {
        return Ok(gitdir.to_owned());
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| "git_pointer_invalid")?
        .trim_end_matches(['\r', '\n']);
    if text.is_empty() || text.contains(['\r', '\n', '\0']) {
        return Err("git_pointer_invalid");
    }
    inventory::normalize_absolute(gitdir, text)
}

fn read(
    path: &Path,
    grant: &SearchPathsGrant,
    authorize: &dyn Fn(&str) -> Option<WorkspaceResourceGrant>,
    external: &dyn Fn(&Path) -> Option<WorkspaceResourceGrant>,
    tick: &mut dyn FnMut() -> Result<(), &'static str>,
) -> Result<Option<Vec<u8>>, &'static str> {
    tick()?;
    let parent = path.parent().ok_or("git_metadata_unavailable")?;
    match super::super::confined::open_directory(parent) {
        Ok(_) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("git_metadata_unavailable"),
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && metadata.len() <= 16_384 => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Ok(_) | Err(_) => return Err("git_metadata_unavailable"),
    }
    let resource = if let Ok(relative) = path.strip_prefix(&grant.workspace_root) {
        authorize(relative.to_str().ok_or("non_utf8_path")?)
    } else {
        external(path)
    }
    .filter(|resource| {
        resource.workspace_root.join(&resource.path) == path
            && PathBuf::from(&resource.path)
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
    })
    .ok_or("git_ignore_resource_denied")?;
    tick()?;
    let (_, file) =
        super::super::confined::open_target(&resource.workspace_root.join(&resource.path))
            .map_err(|_| "git_metadata_unavailable")?;
    let before = file.metadata().map_err(|_| "git_metadata_unavailable")?;
    let mut bytes = Vec::new();
    file.take(16_385)
        .read_to_end(&mut bytes)
        .map_err(|_| "git_metadata_unavailable")?;
    tick()?;
    let (_, current) =
        super::super::confined::open_target(path).map_err(|_| "git_metadata_changed")?;
    let after = current.metadata().map_err(|_| "git_metadata_changed")?;
    if bytes.len() > 16_384
        || before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        return Err("git_metadata_changed");
    }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "bcode-git-policy-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(root.join("repo/.git/info")).unwrap();
            Self(root.canonicalize().unwrap())
        }
        fn write(&self, path: &str, text: &str) {
            let path = self.0.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        fn grant(&self) -> SearchPathsGrant {
            SearchPathsGrant {
                workspace_root: self.0.join("repo"),
                excluded_prefixes: Vec::new(),
            }
        }
        fn sources(&self) -> Sources {
            Sources {
                home: Some(self.0.join("home")),
                xdg: Some(self.0.join("xdg")),
                configs: vec![self.0.join("system"), self.0.join("home/.gitconfig")],
                incomplete: None,
            }
        }
        fn policy(&self) -> Policy {
            let grant = self.grant();
            load_sources(
                &grant,
                &|path| {
                    Some(WorkspaceResourceGrant {
                        workspace_root: grant.workspace_root.clone(),
                        path: path.into(),
                    })
                },
                &external,
                &mut || Ok(()),
                self.sources(),
            )
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn external(path: &Path) -> Option<WorkspaceResourceGrant> {
        Some(WorkspaceResourceGrant {
            workspace_root: path.parent()?.to_owned(),
            path: path.file_name()?.to_str()?.into(),
        })
    }
    fn ignored(policy: &Policy, path: &str) -> bool {
        ignore_policy::ignored(Path::new(path), false, &policy.rules, &mut || Ok(())).unwrap()
    }
    #[test]
    fn system_global_include_repo_worktree_precedence_and_default_xdg() {
        let fixture = Fixture::new();
        fixture.write("xdg/git/ignore", "default.tmp\n");
        assert!(ignored(&fixture.policy(), "default.tmp"));
        fixture.write("system", "[core]\n excludesFile=system.ignore\n");
        fixture.write("repo/system.ignore", "system.tmp\n");
        assert!(ignored(&fixture.policy(), "system.tmp"));
        fixture.write("home/.gitconfig", "[include]\n path=include.cfg\n");
        fixture.write(
            "home/include.cfg",
            "[core]\n excludesFile=~/global.ignore\n",
        );
        fixture.write("home/global.ignore", "global.tmp\n");
        assert!(ignored(&fixture.policy(), "global.tmp"));
        fixture.write(
            "repo/.git/config",
            "[core]\n excludesFile=repo.ignore\n[extensions]\n worktreeConfig=true\n",
        );
        fixture.write("repo/repo.ignore", "repo.tmp\n");
        assert!(ignored(&fixture.policy(), "repo.tmp"));
        fixture.write(
            "repo/.git/config.worktree",
            "[core]\n excludesFile=worktree.ignore\n",
        );
        fixture.write("repo/worktree.ignore", "worktree.tmp\n");
        fixture.write("repo/.git/info/exclude", "info.tmp\n");
        let policy = fixture.policy();
        assert!(ignored(&policy, "worktree.tmp"));
        assert!(ignored(&policy, "info.tmp"));
        assert!(!ignored(&policy, "repo.tmp"));
        assert_eq!(policy.incomplete, None);
        let before = policy.fingerprint;
        fixture.write("repo/worktree.ignore", "changed.tmp\n");
        let changed = fixture.policy();
        assert_ne!(changed.fingerprint, before);
        assert!(ignored(&changed, "changed.tmp"));
    }
    #[test]
    fn denied_include_missing_and_stale_policy_are_distinct() {
        let fixture = Fixture::new();
        fixture.write("home/.gitconfig", "[include]\n path=include.cfg\n");
        fixture.write("home/include.cfg", "[core]\n excludesFile=secret.ignore\n");
        let grant = fixture.grant();
        let result = load_sources(
            &grant,
            &|path| {
                Some(WorkspaceResourceGrant {
                    workspace_root: grant.workspace_root.clone(),
                    path: path.into(),
                })
            },
            &|path| {
                if path == fixture.0.join("home/include.cfg") {
                    None
                } else {
                    external(path)
                }
            },
            &mut || Ok(()),
            fixture.sources(),
        );
        assert!(matches!(result, Err("git_ignore_resource_denied")));
        let resource = fixture.0.join("home/include.cfg");
        let mut tick = || Ok(());
        let mut reader = Reader {
            grant: &grant,
            authorize: &|_| None,
            external: &external,
            tick: &mut tick,
            seen: std::collections::BTreeMap::new(),
            bytes: 0,
        };
        assert!(reader.read(&fixture.0.join("missing")).unwrap().is_none());
        assert!(reader.read(&resource).unwrap().is_some());
        fixture.write("home/include.cfg", "[core]\n excludesFile=changed\n");
        assert_eq!(reader.finish(), Err("git_metadata_changed"));
    }
}
