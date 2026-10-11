//! Bounded subset of gitignore syntax; unsupported syntax fails closed.
use std::path::{Path, PathBuf};

pub(super) struct Rule {
    base: PathBuf,
    pattern: String,
    directory_only: bool,
    negated: bool,
    anchored: bool,
}

pub(super) fn parse(
    base: &Path,
    text: &str,
    remaining_rules: usize,
    admit: &mut dyn FnMut(usize) -> Result<(), &'static str>,
) -> Result<Vec<Rule>, &'static str> {
    let mut rules = Vec::new();
    for raw in text.lines() {
        admit(0)?;
        let line = raw.trim_end_matches(' ');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.len() > 512
            || line.contains(['\\', '[', ']'])
            || line.contains("**")
            || line.chars().any(char::is_control)
        {
            return Err("ignore_policy_unsupported");
        }
        let (negated, line) = line
            .strip_prefix('!')
            .map_or((false, line), |line| (true, line));
        let (anchored, line) = line
            .strip_prefix('/')
            .map_or((false, line), |line| (true, line));
        let directory_only = line.ends_with('/');
        let pattern = line.strip_suffix('/').unwrap_or(line);
        if pattern.is_empty()
            || pattern
                .split('/')
                .any(|part| matches!(part, ".." | "." | ""))
        {
            return Err("ignore_policy_unsupported");
        }
        if rules.len() >= remaining_rules.min(256) {
            return Err("ignore_rule_limit");
        }
        // Charge before String/PathBuf/Vec allocation (including capacity growth).
        admit(
            pattern
                .len()
                .saturating_add(base.as_os_str().len())
                .saturating_add(256),
        )?;
        rules.push(Rule {
            base: base.into(),
            pattern: pattern.into(),
            directory_only,
            negated,
            anchored,
        });
    }
    Ok(rules)
}

pub(super) fn ignored(
    path: &Path,
    directory: bool,
    rules: &[Rule],
    tick: &mut dyn FnMut() -> Result<(), &'static str>,
) -> Result<bool, &'static str> {
    let mut ignored = false;
    for rule in rules {
        tick()?;
        if rule.directory_only && !directory {
            continue;
        }
        let Ok(relative) = path.strip_prefix(&rule.base) else {
            continue;
        };
        let Some(relative) = relative.to_str() else {
            return Ok(true);
        };
        let candidate = if rule.anchored || rule.pattern.contains('/') {
            relative
        } else {
            relative.rsplit('/').next().unwrap_or(relative)
        };
        if glob(&rule.pattern, candidate, tick)? {
            ignored = !rule.negated;
        }
    }
    Ok(ignored)
}

// UTF-8 source offsets, not terminal geometry. No allocation for wildcard matching.
fn glob(
    pattern: &str,
    path: &str,
    tick: &mut dyn FnMut() -> Result<(), &'static str>,
) -> Result<bool, &'static str> {
    let (mut p, mut s, mut star, mut retry) = (0, 0, None, 0);
    while s < path.len() {
        tick()?;
        let character = path[s..]
            .chars()
            .next()
            .ok_or("ignore_policy_unsupported")?;
        let next = pattern[p..].chars().next();
        if next == Some(character) || (next == Some('?') && character != '/') {
            p += next.map_or(0, char::len_utf8);
            s += character.len_utf8();
        } else if next == Some('*') {
            star = Some(p);
            retry = s;
            p += 1;
        } else if let Some(position) = star {
            let Some(character) = path[retry..].chars().next() else {
                return Ok(false);
            };
            if character == '/' {
                return Ok(false);
            }
            retry += character.len_utf8();
            s = retry;
            p = position + 1;
        } else {
            return Ok(false);
        }
    }
    while pattern[p..].starts_with('*') {
        tick()?;
        p += 1;
    }
    Ok(p == pattern.len())
}

#[cfg(test)]
mod tests {
    use super::{glob, parse};
    #[test]
    fn wildcard_retry_checks_cancellation_without_allocating() {
        let mut calls = 0;
        let result = glob("*aaaaaaaaab", "aaaaaaaaaaaaaaaaaaaaaaaaac", &mut || {
            calls += 1;
            if calls >= 8 { Err("cancelled") } else { Ok(()) }
        });
        assert_eq!(result, Err("cancelled"));
        assert_eq!(calls, 8);
    }
    #[test]
    fn rule_allocation_is_admitted_before_construction() {
        let mut charged = 0;
        let result = parse(
            std::path::Path::new("src"),
            "*.log\n*.tmp",
            1024,
            &mut |cost| {
                charged += cost;
                if cost == 0 { Ok(()) } else { Err("byte_limit") }
            },
        );
        assert!(matches!(result, Err("byte_limit")));
        assert_eq!(charged, 264);
    }
}
