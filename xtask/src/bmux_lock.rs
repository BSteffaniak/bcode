//! Guards the committed lockfile against local BMUX development overrides.
//!
//! `scripts/bmux-dev.sh on` rewrites the workspace `[patch.crates-io]` `bmux_*` entries to local
//! paths, and Cargo then records those packages without a `source`. Neither may be committed. This
//! check requires every `bmux_*` patch to be a git patch and compares every locked `bmux_*` package
//! against the git locator that patch declares.

use std::collections::{BTreeMap, BTreeSet};

use crate::{Result, format_error, workspace_root};

const BMUX_PREFIX: &str = "bmux_";
const GIT_SOURCE_PREFIX: &str = "git+";
const COMMIT_HEX_LEN: usize = 40;

/// Runs the lockfile check for the repository workspace.
pub fn check() -> Result<()> {
    let failures = workspace_failures()?;
    if !failures.is_empty() {
        return Err(format_error(failures.join("\n")));
    }
    println!("Cargo.lock resolves every bmux_* package from its declared git source");
    Ok(())
}

/// Returns one message per non-git `bmux_*` patch declaration and per locked `bmux_*` package
/// with an unexpected source.
///
/// # Errors
///
/// * The workspace `Cargo.toml` or `Cargo.lock` cannot be read or parsed.
/// * `Cargo.lock` has no `[[package]]` entries.
pub fn workspace_failures() -> Result<Vec<String>> {
    let root = workspace_root();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))?;
    let lock = std::fs::read_to_string(root.join("Cargo.lock"))?;
    violations(&manifest, &lock)
}

/// Reports non-git `bmux_*` patches and compares lockfile `bmux_*` sources with the manifest's
/// git patch locators.
fn violations(manifest: &str, lock: &str) -> Result<Vec<String>> {
    let (patches, mut failures) = bmux_patch_locators(manifest)?;
    let known: BTreeSet<&str> = patches.values().map(String::as_str).collect();
    let lock = lock
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| format_error(format!("invalid Cargo.lock: {error}")))?;
    let packages = lock
        .get("package")
        .and_then(toml_edit::Item::as_array_of_tables)
        .ok_or_else(|| format_error("Cargo.lock has no [[package]] entries"))?;

    for package in packages {
        let Some(name) = package.get("name").and_then(toml_edit::Item::as_str) else {
            continue;
        };
        if !name.starts_with(BMUX_PREFIX) {
            continue;
        }
        let version = package
            .get("version")
            .and_then(toml_edit::Item::as_str)
            .unwrap_or("?");
        let label = format!("bmux lock: {name} {version}");
        let Some(source) = package.get("source").and_then(toml_edit::Item::as_str) else {
            failures.push(format!(
                "{label} has no source (path override); run `scripts/bmux-dev.sh off`"
            ));
            continue;
        };
        let Some((locator, commit)) = split_git_source(source) else {
            failures.push(format!("{label} is not a pinned git source: {source}"));
            continue;
        };
        let accepted = patches
            .get(name)
            .map_or_else(|| known.contains(locator), |expected| expected == locator);
        if !accepted {
            let expected = patches.get(name).map_or_else(
                || known.iter().copied().collect::<Vec<_>>().join(" or "),
                Clone::clone,
            );
            failures.push(format!(
                "{label} resolves from git+{locator}#{commit}; expected git+{expected}"
            ));
        }
    }
    Ok(failures)
}

/// Splits `git+<url>[?<reference>]#<commit>` into `(<url>[?<reference>], <commit>)`.
fn split_git_source(source: &str) -> Option<(&str, &str)> {
    let (locator, commit) = source.strip_prefix(GIT_SOURCE_PREFIX)?.rsplit_once('#')?;
    let pinned = commit.len() == COMMIT_HEX_LEN && commit.bytes().all(|b| b.is_ascii_hexdigit());
    pinned.then_some((locator, commit))
}

/// Maps each git-patched `bmux_*` crate to the locator Cargo records for its git source, and
/// returns one failure per `bmux_*` patch that does not use a `git` source.
fn bmux_patch_locators(manifest: &str) -> Result<(BTreeMap<String, String>, Vec<String>)> {
    let manifest = manifest
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| format_error(format!("invalid workspace Cargo.toml: {error}")))?;
    let Some(patches) = manifest
        .get("patch")
        .and_then(|patch| patch.get("crates-io"))
        .and_then(toml_edit::Item::as_table_like)
    else {
        return Ok((BTreeMap::new(), Vec::new()));
    };
    let mut locators = BTreeMap::new();
    let mut failures = Vec::new();
    for (name, entry) in patches.iter() {
        if !name.starts_with(BMUX_PREFIX) {
            continue;
        }
        let Some(git) = entry.get("git").and_then(toml_edit::Item::as_str) else {
            failures.push(format!(
                "bmux patch: workspace [patch.crates-io] {name} must use a `git` source; \
                 run `scripts/bmux-dev.sh off` to remove local path patches"
            ));
            continue;
        };
        let reference = ["branch", "tag", "rev"].into_iter().find_map(|key| {
            entry
                .get(key)
                .and_then(toml_edit::Item::as_str)
                .map(|value| format!("?{key}={value}"))
        });
        locators.insert(
            name.to_owned(),
            format!("{git}{}", reference.unwrap_or_default()),
        );
    }
    Ok((locators, failures))
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "52163c266ea480e876b9ec7a08b76ba60004d133";
    const MANIFEST: &str = r#"
[workspace]
members = ["app"]

[patch.crates-io]
bmux_tui   = { git = "https://github.com/BSteffaniak/bmux", branch = "master" }
bmux_color = { git = "https://github.com/BSteffaniak/bmux", branch = "master" }
other      = { git = "https://example.com/other", branch = "main" }
"#;

    fn lock(packages: &[(&str, Option<String>)]) -> String {
        use std::fmt::Write as _;
        let mut text = String::from("version = 4\n");
        for (name, source) in packages {
            writeln!(
                text,
                "\n[[package]]\nname = \"{name}\"\nversion = \"0.0.1\""
            )
            .unwrap();
            if let Some(source) = source {
                writeln!(text, "source = \"{source}\"").unwrap();
            }
        }
        text
    }

    fn git(locator: &str) -> String {
        format!("git+{locator}#{COMMIT}")
    }

    const BMUX: &str = "https://github.com/BSteffaniak/bmux?branch=master";

    #[test]
    fn git_pinned_bmux_packages_and_unrelated_sources_pass() {
        let lock = lock(&[
            ("app", None),
            ("bmux_tui", Some(git(BMUX))),
            ("bmux_color", Some(git(BMUX))),
            // Unpatched BMUX crates are accepted from the same declared repository.
            ("bmux_codec", Some(git(BMUX))),
            (
                "serde",
                Some("registry+https://github.com/rust-lang/crates.io-index".into()),
            ),
        ]);
        assert_eq!(violations(MANIFEST, &lock).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn path_patched_bmux_packages_fail_by_name() {
        let lock = lock(&[
            ("bmux_tui", None),
            ("bmux_color", Some(git(BMUX))),
            ("bmux_codec", None),
        ]);
        let failures = violations(MANIFEST, &lock).unwrap();
        assert_eq!(failures.len(), 2, "{failures:?}");
        assert!(failures[0].contains("bmux_tui") && failures[0].contains("no source"));
        assert!(failures[1].contains("bmux_codec") && failures[1].contains("no source"));
    }

    #[test]
    fn registry_unpinned_and_foreign_git_sources_fail() {
        let lock = lock(&[
            (
                "bmux_tui",
                Some("registry+https://github.com/rust-lang/crates.io-index".into()),
            ),
            (
                "bmux_color",
                Some("git+https://github.com/BSteffaniak/bmux?branch=master".into()),
            ),
            (
                "bmux_codec",
                Some(git("https://github.com/fork/bmux?branch=master")),
            ),
            (
                "bmux_ipc",
                Some(git("https://github.com/BSteffaniak/bmux?branch=dev")),
            ),
        ]);
        let failures = violations(MANIFEST, &lock).unwrap();
        assert_eq!(failures.len(), 4, "{failures:?}");
        for (failure, name) in
            failures
                .iter()
                .zip(["bmux_tui", "bmux_color", "bmux_codec", "bmux_ipc"])
        {
            assert!(failure.contains(name), "{failure}");
        }
    }

    #[test]
    fn patched_crate_must_match_its_own_declared_reference() {
        let manifest = MANIFEST.replace(
            "bmux_color = { git = \"https://github.com/BSteffaniak/bmux\", branch = \"master\" }",
            "bmux_color = { git = \"https://github.com/BSteffaniak/bmux\", rev = \"abc\" }",
        );
        let lock = lock(&[("bmux_color", Some(git(BMUX)))]);
        let failures = violations(&manifest, &lock).unwrap();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("?rev=abc"), "{}", failures[0]);
    }

    #[test]
    fn non_git_bmux_patch_declarations_are_rejected() {
        let manifest = "[patch.crates-io]\nbmux_tui = { path = \"../bmux/packages/tui\" }\n";
        let failures = violations(manifest, &lock(&[("bmux_tui", None)])).unwrap();
        assert_eq!(failures.len(), 2, "{failures:?}");
        assert!(
            failures[0].contains("bmux_tui") && failures[0].contains("must use a `git` source"),
            "{}",
            failures[0]
        );
        assert!(failures[1].contains("no source"), "{}", failures[1]);
    }
}
