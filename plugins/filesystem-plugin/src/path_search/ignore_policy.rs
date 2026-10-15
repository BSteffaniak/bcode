//! Gitignore grammar over bounded, separately authorized in-memory policy text.
//! Never use the matcher's filesystem discovery or global configuration helpers.
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub(super) struct Rule {
    base: PathBuf,
    matcher: ignore::gitignore::Gitignore,
}

pub(super) fn parse(
    base: &Path,
    text: &str,
    remaining_rules: usize,
    admit: &mut dyn FnMut(usize) -> Result<(), &'static str>,
) -> Result<Vec<Rule>, &'static str> {
    let mut rules = Vec::new();
    for line in text.lines() {
        admit(0)?;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.len() > 512 {
            return Err("ignore_policy_limit");
        }
        if rules.len() >= remaining_rules.min(256) {
            return Err("ignore_rule_limit");
        }
        // Admission precedes matcher allocations. Compilation stays bounded by the
        // authorized file size, rule count and per-line limit, with ticks per rule.
        admit(
            line.len()
                .saturating_add(base.as_os_str().len())
                .saturating_add(256),
        )?;
        let mut builder = ignore::gitignore::GitignoreBuilder::new(base);
        builder
            .add_line(None, line)
            .map_err(|_| "ignore_policy_invalid")?;
        admit(0)?;
        let matcher = builder.build().map_err(|_| "ignore_policy_invalid")?;
        rules.push(Rule {
            base: base.to_owned(),
            matcher,
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
        if !path.starts_with(&rule.base) {
            continue;
        }
        match rule.matcher.matched(path, directory) {
            ignore::Match::Ignore(_) => ignored = true,
            ignore::Match::Whitelist(_) => ignored = false,
            ignore::Match::None => (),
        }
    }
    Ok(ignored)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_grammar_double_star_brackets_escapes_and_negation() {
        let rules = parse(
            Path::new(""),
            "**/build/**\n[ab].tmp\n\\#literal\n*.log\n!important.log\n",
            256,
            &mut |_| Ok(()),
        )
        .unwrap();
        for path in ["src/build/output", "a.tmp", "#literal", "error.log"] {
            assert!(
                ignored(Path::new(path), false, &rules, &mut || Ok(())).unwrap(),
                "{path}"
            );
        }
        assert!(!ignored(Path::new("important.log"), false, &rules, &mut || Ok(())).unwrap());
        assert_eq!(
            ignored(Path::new("a.tmp"), false, &rules, &mut || Err("cancelled")),
            Err("cancelled")
        );
    }
    #[test]
    fn allocation_is_admitted_before_compilation() {
        let mut charged = 0;
        let result = parse(Path::new("src"), "*.log\n*.tmp", 1024, &mut |cost| {
            charged += cost;
            if cost == 0 { Ok(()) } else { Err("byte_limit") }
        });
        assert!(matches!(result, Err("byte_limit")));
        assert_eq!(charged, 264);
    }
}
