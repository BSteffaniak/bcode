//! Parsed, command-independent argv permission patterns.
//!
//! Whitespace separates values; single/double quotes and backslash quote literal
//! syntax (including empty values). `[a | b c]` is optional; a following `*`
//! repeats whole branches. Groups cannot nest. `<arg>` consumes one value and
//! `...` consumes any trailing values. The executable must be a literal.
//!
//! `/expression/imsx` is a whole-value Rust regex, Unicode enabled by default.
//! The executable is always literal, including absolute paths. For arguments,
//! a slash inside a class or escaped slash is not a delimiter. A candidate must
//! close at a token boundary with no suffix or a suffix drawn from `imsxg`
//! (g is reserved and rejected), or contain explicit regex syntax
//! (`/\\[](){}*+?|^$`). Other slash-led tokens
//! are literal paths, so `/tmp/path` and `/usr/bin/git` retain their old meaning.
//! Collisions `/tmp/`, `/tmp/i`, and `/tmp/g` require literal quotes. Unknown
//! suffixes on plain bodies (e.g. `/tmp/path` and `/tmp/z`) are paths;
//! `/[a]/zz` is an invalid explicit regex. Duplicate flags and unsupported flags on explicit regexes
//! are errors, never fallback literals.
//!
//! `compile` admits at most 128 rules, 32 KiB combined pattern text, 4096
//! compiled argv states, and 64 regexes. Each regex has a 256 KiB compiled-size
//! limit and a 256 KiB lazy-DFA cache limit (16 MiB aggregate apiece). Counts
//! are checked before compiling the next regex; an invalid set fails atomically.
//!
//! `<options>*` consumes zero or more dash-led values other than `-`/`--`.
//! An option without `=` may also consume one non-dash-led value (including
//! empty or `-`). Both interpretations are retained, not greedily selected.
//! `--` may be consumed once and ends this options segment. Any earlier literal
//! `--` also prevents subsequent generic option consumption. This is generic
//! syntax, not an assertion about any executable's actual option semantics.

use bcode_agent_policy_models::Action;
use bcode_shell_command_analysis_models::{ShellCommand, ShellWord};
use regex::{Regex, RegexBuilder};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const MAX_PATTERN_BYTES: usize = 1024;
const MAX_RULES: usize = 128;
const MAX_TOTAL_PATTERN_BYTES: usize = 32 * 1024;
const MAX_REGEXES: usize = 64;
const MAX_TOTAL_STATES: usize = 4096;
const MAX_SEGMENTS: usize = 32;
const MAX_STATES: usize = 256;
const MAX_WORDS: usize = 256;
const MAX_WORD_BYTES: usize = 65_536;

#[derive(Debug, Clone)]
enum Atom {
    Literal(String),
    Regex { compiled: Regex, suffix: String },
    Any,
    Option,
    OptionWithValue,
    OptionValue,
    OptionsEnd,
}

impl Atom {
    fn accepts(&self, value: &str) -> bool {
        match self {
            Self::Literal(literal) => literal == value,
            Self::Regex { compiled, .. } => compiled.is_match(value),
            Self::Any => true,
            Self::Option => value.starts_with('-') && value != "-" && value != "--",
            Self::OptionWithValue => {
                value.starts_with('-') && value != "-" && value != "--" && !value.contains('=')
            }
            Self::OptionValue => !value.starts_with('-') || value == "-",
            Self::OptionsEnd => value == "--",
        }
    }
}

#[derive(Debug, Clone)]
enum Token {
    Atom(Atom),
    Open,
    Close,
    Pipe,
    Star,
    Rest,
    Options,
}

/// A compiled bounded argv rule.
#[derive(Debug, Clone)]
pub struct WordRule {
    pub pattern: String,
    pub action: Action,
    edges: Vec<Vec<(Option<Atom>, usize)>>,
    accept: usize,
    specificity: (usize, usize),
    broad: bool,
    regex_flags: Vec<String>,
}

fn regex_token(
    chars: &[char],
    start: usize,
    regexes: &mut usize,
) -> Result<Option<(Atom, usize)>, String> {
    let mut index = start + 1;
    // Rust regex classes can nest (set operations) and contain a leading `]`.
    let mut classes = Vec::<u8>::new();
    let mut expression = String::new();
    while let Some(&ch) = chars.get(index) {
        if ch == '\\' {
            let Some(&escaped) = chars.get(index + 1) else {
                break;
            };
            if escaped != '/' {
                expression.push('\\');
            }
            expression.push(escaped);
            if let Some(position) = classes.last_mut() {
                *position = 2;
            }
            index += 2;
            continue;
        }
        if ch == '/' && classes.is_empty() {
            let mut end = index + 1;
            while chars.get(end).is_some_and(char::is_ascii_alphabetic) {
                end += 1;
            }
            // A slash embedded in a longer path is not a regex delimiter.
            if chars
                .get(end)
                .is_some_and(|ch| !ch.is_whitespace() && !matches!(ch, ']' | '|'))
            {
                return Ok(None);
            }
            let suffix = &chars[index + 1..end];
            let explicit = expression.contains([
                '/', '\\', '[', ']', '(', ')', '{', '}', '*', '+', '?', '|', '^', '$',
            ]);
            if !explicit
                && !suffix
                    .iter()
                    .all(|ch| matches!(ch, 'i' | 'm' | 's' | 'x' | 'g'))
            {
                return Ok(None);
            }
            let mut flags = BTreeSet::new();
            for &flag in &chars[index + 1..end] {
                if !matches!(flag, 'i' | 'm' | 's' | 'x') || !flags.insert(flag) {
                    return Err(
                        "regex flags must be distinct and drawn from imsx (no g)".to_owned()
                    );
                }
            }
            // The newline keeps an x-mode trailing comment from swallowing the anchor.
            let expression = if flags.contains(&'x') {
                format!("\\A(?:{expression}\n)\\z")
            } else {
                format!("\\A(?:{expression})\\z")
            };
            if *regexes >= MAX_REGEXES {
                return Err("word rules exceed 64 regexes".to_owned());
            }
            *regexes += 1;
            let regex = RegexBuilder::new(&expression)
                .case_insensitive(flags.contains(&'i'))
                .multi_line(flags.contains(&'m'))
                .dot_matches_new_line(flags.contains(&'s'))
                .ignore_whitespace(flags.contains(&'x'))
                .unicode(true)
                .size_limit(256 * 1024)
                .dfa_size_limit(256 * 1024)
                .nest_limit(32)
                .build()
                .map_err(|error| format!("invalid or oversized value regex: {error}"))?;
            return Ok(Some((
                Atom::Regex {
                    compiled: regex,
                    suffix: suffix.iter().collect(),
                },
                end,
            )));
        }
        match ch {
            '[' => {
                if let Some(position) = classes.last_mut() {
                    *position = 2;
                }
                classes.push(0);
            }
            ']' if classes.last().is_some_and(|position| *position == 2) => {
                classes.pop();
            }
            '^' if classes.last() == Some(&0) => {
                *classes.last_mut().expect("class is present") = 1;
            }
            _ => {
                if let Some(position) = classes.last_mut() {
                    *position = 2;
                }
            }
        }
        expression.push(ch);
        index += 1;
    }
    Ok(None)
}

fn tokenize(pattern: &str, regexes: &mut usize) -> Result<Vec<Token>, String> {
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err("word pattern exceeds 1024 bytes".to_owned());
    }
    let chars = pattern.chars().collect::<Vec<_>>();
    let mut index = 0;
    let mut tokens = Vec::new();
    while let Some(&ch) = chars.get(index) {
        if ch.is_whitespace() {
            index += 1;
            continue;
        }
        let punctuation = match ch {
            '[' => Some(Token::Open),
            ']' => Some(Token::Close),
            '|' => Some(Token::Pipe),
            '*' => Some(Token::Star),
            _ => None,
        };
        if let Some(token) = punctuation {
            tokens.push(token);
            index += 1;
            continue;
        }
        if ch == '/'
            && !tokens.is_empty()
            && let Some((atom, end)) = regex_token(&chars, index, regexes)?
        {
            tokens.push(Token::Atom(atom));
            index = end;
            continue;
        }
        let mut value = String::new();
        let mut quoted = false;
        while let Some(&ch) = chars.get(index) {
            if ch.is_whitespace() || matches!(ch, '[' | ']' | '|' | '*') {
                break;
            }
            if ch == '\\' {
                quoted = true;
                index += 1;
                value.push(*chars.get(index).ok_or("dangling literal escape")?);
                index += 1;
            } else if matches!(ch, '\'' | '"') {
                quoted = true;
                let quote = ch;
                index += 1;
                loop {
                    let &ch = chars.get(index).ok_or("unterminated literal quote")?;
                    index += 1;
                    if ch == quote {
                        break;
                    }
                    if ch == '\\' && quote == '"' {
                        value.push(*chars.get(index).ok_or("dangling quoted escape")?);
                        index += 1;
                    } else {
                        value.push(ch);
                    }
                }
            } else {
                value.push(ch);
                index += 1;
            }
        }
        let token = match (quoted, value.as_str()) {
            (false, "...") => Token::Rest,
            (false, "<arg>") => Token::Atom(Atom::Any),
            (false, "<options>") => Token::Options,
            (false, value) if value.contains(['<', '>']) => {
                return Err("unknown placeholder".to_owned());
            }
            _ => Token::Atom(Atom::Literal(value)),
        };
        tokens.push(token);
    }
    Ok(tokens)
}

impl WordRule {
    /// Return syntactic breadth and each regex's validated suffix in source order.
    ///
    /// Any nonliteral argv construct is broad, including optional/repeated groups.
    /// Unflagged regexes contribute an empty suffix; quoted literals contribute none.
    #[must_use]
    pub fn preview_metadata(&self) -> (bool, Vec<String>) {
        (self.broad, self.regex_flags.clone())
    }

    fn state(&mut self) -> Result<usize, String> {
        if self.edges.len() >= MAX_STATES {
            return Err("word pattern exceeds state limit".to_owned());
        }
        self.edges.push(Vec::new());
        Ok(self.edges.len() - 1)
    }

    fn edge(&mut self, from: usize, to: usize, atom: Option<Atom>) {
        self.edges[from].push((atom, to));
    }

    fn atom(&mut self, from: usize, atom: Atom) -> Result<usize, String> {
        let to = self.state()?;
        self.edge(from, to, Some(atom));
        Ok(to)
    }
}

#[cfg(test)]
fn parse(pattern: &str, action: Action) -> Result<WordRule, String> {
    parse_with_budget(pattern, action, &mut 0)
}

fn parse_with_budget(
    pattern: &str,
    action: Action,
    regexes: &mut usize,
) -> Result<WordRule, String> {
    let tokens = tokenize(pattern, regexes)?;
    if !matches!(tokens.first(), Some(Token::Atom(Atom::Literal(value))) if !value.is_empty()) {
        return Err("a word pattern must start with a nonempty literal executable".to_owned());
    }
    let mut rule = WordRule {
        pattern: pattern.to_owned(),
        action,
        edges: vec![Vec::new()],
        accept: 0,
        specificity: (0, 0),
        broad: tokens
            .iter()
            .any(|token| !matches!(token, Token::Atom(Atom::Literal(_)))),
        regex_flags: tokens
            .iter()
            .filter_map(|token| match token {
                Token::Atom(Atom::Regex { suffix, .. }) => Some(suffix.clone()),
                _ => None,
            })
            .collect(),
    };
    let mut index = 0;
    let mut current = 0;
    let mut segments = 0;
    while let Some(token) = tokens.get(index) {
        segments += 1;
        if segments > MAX_SEGMENTS {
            return Err("word pattern exceeds 32 segments".to_owned());
        }
        index += 1;
        match token {
            Token::Atom(atom) => {
                rule.specificity.0 += usize::from(matches!(atom, Atom::Literal(_)));
                current = rule.atom(current, atom.clone())?;
            }
            Token::Rest if index == tokens.len() => {
                rule.edge(current, current, Some(Atom::Any));
            }
            Token::Options if matches!(tokens.get(index), Some(Token::Star)) => {
                index += 1;
                let end = rule.state()?;
                rule.edge(current, end, None);
                rule.edge(current, current, Some(Atom::Option));
                let value = rule.atom(current, Atom::OptionWithValue)?;
                rule.edge(value, current, Some(Atom::OptionValue));
                rule.edge(current, end, Some(Atom::OptionsEnd));
                current = end;
            }
            Token::Open => {
                let start = current;
                let end = rule.state()?;
                rule.edge(start, end, None);
                let mut branch = start;
                let mut count = 0;
                loop {
                    match tokens.get(index) {
                        Some(Token::Atom(atom)) => {
                            branch = rule.atom(branch, atom.clone())?;
                            count += 1;
                            index += 1;
                        }
                        Some(Token::Pipe | Token::Close) if count > 0 => {
                            rule.edge(branch, end, None);
                            let close = matches!(tokens[index], Token::Close);
                            index += 1;
                            if close { break; }
                            branch = start;
                            count = 0;
                        }
                        _ => return Err("groups require nonempty atomic value branches; no nesting/rest/options".to_owned()),
                    }
                }
                if matches!(tokens.get(index), Some(Token::Star)) {
                    rule.edge(end, start, None);
                    index += 1;
                }
                rule.specificity.1 += 1;
                current = end;
            }
            _ => {
                return Err(
                    "unexpected syntax; ... must be trailing and * must follow a group/options"
                        .to_owned(),
                );
            }
        }
    }
    rule.accept = current;
    Ok(rule)
}

pub fn compile(rules: &BTreeMap<String, Action>) -> Result<Vec<WordRule>, String> {
    if rules.len() > MAX_RULES {
        return Err("word rules exceed 128 rules".to_owned());
    }
    if rules.keys().map(String::len).sum::<usize>() > MAX_TOTAL_PATTERN_BYTES {
        return Err("word rules exceed 32768 pattern bytes".to_owned());
    }
    let mut regexes = 0;
    let mut states = 0;
    let mut compiled = Vec::with_capacity(rules.len());
    for (pattern, action) in rules {
        let rule = parse_with_budget(pattern, *action, &mut regexes)?;
        states += rule.edges.len();
        if states > MAX_TOTAL_STATES {
            return Err("word rules exceed 4096 states".to_owned());
        }
        compiled.push(rule);
    }
    Ok(compiled)
}

fn static_word(word: &ShellWord) -> Option<&str> {
    match word {
        ShellWord::Static { value, .. } => Some(value),
        ShellWord::Dynamic { .. } => None,
    }
}

/// Product of the rule NFA and argv. An unknown expansion denotes ANY sequence
/// of values, including zero, not a single wildcard. Revisited pairs are shared.
fn matches(rule: &WordRule, words: &[Option<&str>], uncertain: bool) -> bool {
    if words.len() > MAX_WORDS
        || words.iter().flatten().map(|s| s.len()).sum::<usize>() > MAX_WORD_BYTES
    {
        return uncertain;
    }
    if !uncertain && words.iter().any(Option::is_none) {
        return false;
    }
    let stride = words.len() + 1;
    let mut options_ended = vec![false; stride];
    for (index, word) in words.iter().enumerate() {
        options_ended[index + 1] = options_ended[index] || *word == Some("--");
    }
    let mut seen = vec![false; rule.edges.len() * stride];
    let mut queue = VecDeque::from([(0, 0)]);
    seen[0] = true;
    while let Some((state, index)) = queue.pop_front() {
        if state == rule.accept && index == words.len() {
            return true;
        }
        let mut visit = |state: usize, index: usize| {
            let key = state * stride + index;
            if !seen[key] {
                seen[key] = true;
                queue.push_back((state, index));
            }
        };
        if words.get(index) == Some(&None) {
            visit(state, index + 1);
        }
        for (atom, target) in &rule.edges[state] {
            match atom {
                None => visit(*target, index),
                Some(
                    Atom::Option | Atom::OptionWithValue | Atom::OptionValue | Atom::OptionsEnd,
                ) if options_ended[index] => {}
                Some(atom) => match words.get(index) {
                    Some(Some(value)) if atom.accepts(value) => visit(*target, index + 1),
                    Some(None) if uncertain => visit(*target, index),
                    _ => {}
                },
            }
        }
    }
    false
}

pub fn matching<'a>(rules: &'a [WordRule], command: &ShellCommand) -> Option<&'a WordRule> {
    let words = std::iter::once(&command.executable)
        .chain(&command.arguments)
        .map(static_word)
        .collect::<Vec<_>>();
    rules
        .iter()
        .filter(|rule| {
            (command.assignments.is_empty() || rule.action == Action::Deny)
                && matches(rule, &words, false)
        })
        .max_by(|lhs, rhs| rank(lhs, rhs))
}

/// A deny that may apply once dynamically expanded words are known. Assignment
/// prefixes never suppress denies and never grant allows through `matching`.
pub fn uncertain_deny<'a>(rules: &'a [WordRule], command: &ShellCommand) -> Option<&'a WordRule> {
    let words = std::iter::once(&command.executable)
        .chain(&command.arguments)
        .map(static_word)
        .collect::<Vec<_>>();
    if words.iter().all(Option::is_some)
        && words.len() <= MAX_WORDS
        && words.iter().flatten().map(|s| s.len()).sum::<usize>() <= MAX_WORD_BYTES
    {
        return None;
    }
    rules
        .iter()
        .filter(|rule| rule.action == Action::Deny && matches(rule, &words, true))
        .max_by(|lhs, rhs| rank(lhs, rhs))
}

fn rank(lhs: &WordRule, rhs: &WordRule) -> std::cmp::Ordering {
    action_priority(lhs.action)
        .cmp(&action_priority(rhs.action))
        .then_with(|| lhs.specificity.cmp(&rhs.specificity))
        .then_with(|| rhs.pattern.cmp(&lhs.pattern))
}

const fn action_priority(action: Action) -> u8 {
    match action {
        Action::Allow => 0,
        Action::Ask => 1,
        Action::Deny => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_shell_command_analysis_models::ShellAnalysisRequest;

    fn command(source: &str) -> ShellCommand {
        bcode_shell_command_analysis::analyze(&ShellAnalysisRequest::posix(source))
            .unwrap()
            .commands
            .into_iter()
            .next()
            .unwrap()
    }

    fn accepts(pattern: &str, argv: &[&str]) -> bool {
        let rule = parse(pattern, Action::Allow).unwrap();
        matches(
            &rule,
            &argv.iter().map(|value| Some(*value)).collect::<Vec<_>>(),
            false,
        )
    }

    #[test]
    fn old_literals_optional_rest_and_exact_executable() {
        for argv in [
            vec!["git", "diff"],
            vec!["git", "--no-pager", "diff", "a b"],
        ] {
            assert!(accepts("git [--no-pager] diff ...", &argv));
        }
        assert!(!accepts(
            "git [--no-pager] diff ...",
            &["/usr/bin/git", "diff"]
        ));
        assert!(!accepts("git diff", &["git", "diff", "extra"]));
        assert!(accepts("/usr/bin/git diff", &["/usr/bin/git", "diff"]));
        assert!(accepts("cmd", &["cmd"]));
    }

    #[test]
    fn literal_quoting_empty_and_metacharacters() {
        assert!(accepts(
            r#"cmd '' "a b" \[x\] \* \<arg\> '...' '/tmp/' "a\"b" a\ b"#,
            &[
                "cmd", "", "a b", "[x]", "*", "<arg>", "...", "/tmp/", "a\"b", "a b"
            ]
        ));
        assert!(accepts("cmd pre'post'\"fix\"", &["cmd", "prepostfix"]));
        assert!(accepts("cmd <arg>", &["cmd", ""]));
        assert!(!accepts("cmd <arg>", &["cmd"]));
        assert!(!accepts("cmd <arg>", &["cmd", "a", "b"]));
    }

    #[test]
    fn group_branches_are_atomic_and_repeatable() {
        let pattern = "tool [--no-pager | -C <arg> | -c <arg>]* diff [--stat | --name-only]";
        for argv in [
            vec!["tool", "diff"],
            vec![
                "tool",
                "-C",
                "dir",
                "-c",
                "x=y",
                "--no-pager",
                "diff",
                "--stat",
            ],
            vec!["tool", "--no-pager", "--no-pager", "diff"],
        ] {
            assert!(accepts(pattern, &argv), "{argv:?}");
        }
        for argv in [
            vec!["tool", "-C", "diff"],
            vec!["tool", "-C", "dir", "-c", "diff"],
            vec!["tool", "dir", "diff"],
            vec!["tool", "diff", "--stat", "--name-only"],
        ] {
            assert!(!accepts(pattern, &argv), "{argv:?}");
        }
        assert!(accepts("cmd [a b | c d] end", &["cmd", "a", "b", "end"]));
        assert!(!accepts("cmd [a b | c d] end", &["cmd", "a", "d", "end"]));
    }

    #[test]
    fn regexes_are_composable_whole_unicode_values() {
        assert!(accepts(
            r"cmd [/foo|bar/i | -C /[^ ]+/] /\d+/ ...",
            &["cmd", "FOO", "１２", "tail"]
        ));
        assert!(!accepts("cmd /foo/", &["cmd", "xfooy"]));
        assert!(!accepts("cmd /^foo$/m", &["cmd", "foo\nbar"]));
        assert!(accepts("cmd /a.b/s", &["cmd", "a\nb"]));
        assert!(accepts("cmd /a b/x", &["cmd", "ab"]));
        assert!(accepts("cmd /a #comment/x", &["cmd", "a"]));
        assert!(accepts(
            r"cmd /a\/b/ /[/]/ /a b/",
            &["cmd", "a/b", "/", "a b"]
        ));
        assert!(accepts("cmd '/tmp/file'", &["cmd", "/tmp/file"]));
        assert!(accepts(r"cmd /[a-z&&[^/]]+/", &["cmd", "abc"]));
        assert!(accepts(r"cmd /[]/]/", &["cmd", "/"]));
        assert!(accepts("cmd /tmp/file.txt", &["cmd", "/tmp/file.txt"]));
        assert!(accepts("cmd //", &["cmd", ""]));
    }

    #[test]
    fn options_keep_all_complete_interpretations() {
        let pattern = "cmd <options>* show";
        for argv in [
            vec!["cmd", "show"],
            vec!["cmd", "--quiet", "show"],
            vec!["cmd", "--key", "value", "show"],
            vec!["cmd", "--key=value", "-q", "show"],
            vec!["cmd", "-q", "--", "show"],
            vec!["cmd", "--key", "", "show"],
        ] {
            assert!(accepts(pattern, &argv), "{argv:?}");
        }
        for argv in [
            vec!["cmd", "-", "show"],
            vec!["cmd", "value", "show"],
            vec!["cmd", "--", "-q", "show"],
            vec!["cmd", "--key=value", "extra", "show"],
            vec!["cmd", "--key", "--", "value", "show"],
        ] {
            assert!(!accepts(pattern, &argv), "{argv:?}");
        }
        assert!(accepts("cmd -- <options>* show", &["cmd", "--", "show"]));
        assert!(!accepts(
            "cmd -- <options>* show",
            &["cmd", "--", "-q", "show"]
        ));
        assert!(!accepts(
            "cmd <options>* <options>* show",
            &["cmd", "--", "-q", "show"]
        ));
        assert!(accepts(
            "cmd <options>* show",
            &["cmd", "--key", "-", "show"]
        ));
        assert!(accepts(
            "cmd [-C <arg>]* diff",
            &["cmd", "-C", "-c", "diff"]
        ));
        assert!(accepts(
            "cmd <options>* tail end",
            &["cmd", "-x", "value", "tail", "end"]
        ));
        assert!(!accepts(
            "cmd <options>* tail end",
            &["cmd", "-x", "value", "junk", "tail", "end"]
        ));
    }

    #[test]
    fn preview_metadata_uses_parsed_constructs_and_regex_suffixes() {
        for (pattern, broad, flags) in [
            (r"cmd /a\/b/im", true, vec!["im"]),
            (r"cmd /[/]/s", true, vec!["s"]),
            (
                r"cmd [/a/i | /b/ms]* /c/xi //",
                true,
                vec!["i", "ms", "xi", ""],
            ),
            (
                r#"cmd '/secret/i' "/[/]/s" '\*' '...' '<arg>'"#,
                false,
                vec![],
            ),
            ("/usr/bin/git /tmp/path", false, vec![]),
            ("cmd [literal]", true, vec![]),
            ("cmd <arg>", true, vec![]),
            ("cmd <options>*", true, vec![]),
            ("cmd ...", true, vec![]),
        ] {
            let rule = parse(pattern, Action::Allow).unwrap();
            let expected = (broad, flags.into_iter().map(str::to_owned).collect());
            assert_eq!(rule.preview_metadata(), expected, "{pattern}");
            // The preview must not derive metadata from mutable display text.
            let mut display_changed = rule;
            display_changed.pattern = "not /a/g **".to_owned();
            assert_eq!(display_changed.preview_metadata(), expected);
        }
    }

    #[test]
    fn legacy_absolute_paths_and_delimiter_collisions() {
        // Values accepted by the original whitespace/literal grammar remain
        // literal for ordinary absolute executable and argument paths.
        for path in [
            "/usr/bin/git",
            "/tmp/path",
            "/tmp/z",
            "/tmp/file",
            "/tmp/file.txt",
            "/",
            "/usr/local/bin/tool",
            "/tmp/path/child",
        ] {
            assert!(
                accepts(&format!("{path} arg"), &[path, "arg"]),
                "executable {path}"
            );
            assert!(
                accepts(&format!("cmd {path}"), &["cmd", path]),
                "argument {path}"
            );
        }
        assert!(accepts("/tmp/i arg", &["/tmp/i", "arg"]));
        assert!(accepts("/cmd/ arg", &["/cmd/", "arg"]));
        assert!(accepts("cmd /tmp/path", &["cmd", "/tmp/path"]));
        assert!(!accepts("cmd /tmp/", &["cmd", "/tmp/"]));
        assert!(accepts("cmd /tmp/", &["cmd", "tmp"]));
        assert!(accepts("cmd /tmp/i", &["cmd", "TMP"]));
        assert!(accepts(
            "cmd '/tmp/' '/tmp/i' '/tmp/g' '/tmp/z'",
            &["cmd", "/tmp/", "/tmp/i", "/tmp/g", "/tmp/z"]
        ));
        for pattern in [
            "cmd /tmp/g",
            "cmd /[a]/g",
            "cmd /tmp/ii",
            "cmd /[a]/zz",
            "cmd /(/bad",
        ] {
            assert!(parse(pattern, Action::Allow).is_err(), "{pattern}");
        }
    }

    #[test]
    fn aggregate_compile_budgets() {
        let rules = (0..=MAX_RULES)
            .map(|i| (format!("cmd{i}"), Action::Deny))
            .collect();
        assert!(compile(&rules).unwrap_err().contains("128 rules"));
        let rules = (0..40)
            .map(|i| (format!("cmd{i} {}", "a".repeat(900)), Action::Deny))
            .collect();
        assert!(compile(&rules).unwrap_err().contains("pattern bytes"));
        let rules = (0..=MAX_REGEXES)
            .map(|i| (format!("cmd{i} /a/"), Action::Deny))
            .collect();
        assert!(compile(&rules).unwrap_err().contains("64 regexes"));
        let rules = (0..MAX_REGEXES)
            .map(|i| (format!("cmd{i} /a/"), Action::Deny))
            .collect();
        assert_eq!(compile(&rules).unwrap().len(), MAX_REGEXES);
        let rules = (0..128)
            .map(|i| (format!("cmd{i} [{}]", "a ".repeat(40)), Action::Deny))
            .collect();
        assert!(compile(&rules).unwrap_err().contains("4096 states"));
        let rules = (0..MAX_RULES)
            .map(|i| (format!("cmd{i}"), Action::Deny))
            .collect();
        assert_eq!(compile(&rules).unwrap().len(), MAX_RULES);
    }

    #[test]
    fn malformed_patterns_and_compile_limits() {
        for pattern in [
            "",
            "...",
            "[cmd] x",
            "<arg> x",
            "'' x",
            "cmd ... x",
            "cmd *",
            "cmd []",
            "cmd [a |]",
            "cmd [| a]",
            "cmd [a",
            "cmd a]",
            "cmd [[a]]",
            "cmd [a ...]",
            "cmd <options>",
            "cmd <unknown>",
            "cmd 'oops",
            "cmd \\",
            "cmd /a/g",
            "cmd /a/ii",
            "cmd /[a]/u",
            "cmd /[a]/z",
            "cmd /(/",
            "cmd /a{999999999}/",
            "cmd [<options>*]",
        ] {
            assert!(parse(pattern, Action::Allow).is_err(), "{pattern:?}");
        }
        assert!(parse(&format!("cmd {}", "a".repeat(1024)), Action::Allow).is_err());
        assert!(parse(&format!("cmd {}", "a ".repeat(32)), Action::Allow).is_err());
        assert!(parse(&format!("cmd [{}]", "a ".repeat(260)), Action::Allow).is_err());
        let rules = BTreeMap::from([
            ("cmd".to_owned(), Action::Allow),
            ("cmd []".to_owned(), Action::Deny),
        ]);
        assert!(compile(&rules).is_err());
    }

    #[test]
    fn assignments_never_grant_allows_but_keep_denies() {
        for source in ["X=1 cmd dangerous", "X=$unknown cmd dangerous"] {
            let command = command(source);
            let rules = compile(&BTreeMap::from([
                ("cmd ...".to_owned(), Action::Allow),
                ("cmd dangerous".to_owned(), Action::Deny),
            ]))
            .unwrap();
            assert_eq!(matching(&rules, &command).unwrap().action, Action::Deny);
            assert!(matching(&rules[..1], &command).is_none());
        }
    }

    #[test]
    fn dynamic_expansions_include_zero_and_many_words() {
        let rules = compile(&BTreeMap::from([
            ("cmd dangerous subcommand".to_owned(), Action::Deny),
            ("cmd ...".to_owned(), Action::Allow),
        ]))
        .unwrap();
        for source in [
            "cmd $args",
            "cmd dangerous $args subcommand",
            "cmd $args subcommand",
            "cmd dangerous $args",
            "X=1 cmd $args",
            "$exe",
            "cmd $(printf ignored)",
        ] {
            let command = command(source);
            assert!(matching(&rules, &command).is_none(), "{source}");
            assert!(uncertain_deny(&rules, &command).is_some(), "{source}");
        }
        let command = command("cmd unrelated $args");
        assert!(uncertain_deny(&rules, &command).is_none());
        assert!(uncertain_deny(&rules, &self::command("cmd dangerous subcommand")).is_none());
    }

    #[test]
    fn all_canonical_dynamic_kinds_are_uncertain_even_under_rest() {
        use bcode_shell_command_analysis_models::ShellExpansionKind;
        let rules = compile(&BTreeMap::from([
            ("cmd ...".to_owned(), Action::Allow),
            ("cmd a b end".to_owned(), Action::Deny),
        ]))
        .unwrap();
        for kind in [
            ShellExpansionKind::Parameter,
            ShellExpansionKind::Command,
            ShellExpansionKind::Arithmetic,
            ShellExpansionKind::Tilde,
            ShellExpansionKind::Process,
            ShellExpansionKind::Pattern,
        ] {
            let mut command = command("cmd placeholder end");
            let span = command.arguments[0].span();
            command.arguments[0] = ShellWord::Dynamic {
                expansions: vec![kind],
                span,
            };
            assert!(matching(&rules, &command).is_none(), "{kind:?}");
            assert!(uncertain_deny(&rules, &command).is_some(), "{kind:?}");
        }
        let deny = parse("cmd a b end", Action::Deny).unwrap();
        assert!(matches(
            &deny,
            &[Some("cmd"), None, None, Some("end")],
            true
        ));
        assert!(matches(&deny, &[None, Some("a"), None, Some("end")], true));
        assert!(!matches(
            &deny,
            &[Some("cmd"), None, Some("unrelated"), None],
            true
        ));
    }

    #[test]
    fn deny_priority_specificity_and_bounded_matching() {
        let rules = compile(&BTreeMap::from([
            ("cmd ...".to_owned(), Action::Deny),
            ("cmd exact".to_owned(), Action::Allow),
            ("cmd [exact]".to_owned(), Action::Ask),
            ("cmd exact ...".to_owned(), Action::Deny),
        ]))
        .unwrap();
        assert_eq!(
            matching(&rules, &command("cmd exact")).unwrap().pattern,
            "cmd exact ..."
        );
        let argv = vec![Some("cmd"); MAX_WORDS + 1];
        assert!(!matches(&rules[0], &argv, false));
        assert!(matches(&rules[0], &argv, true));
        let large = "x".repeat(MAX_WORD_BYTES + 1);
        assert!(!matches(&rules[0], &[Some("cmd"), Some(&large)], false));
        assert!(matches(&rules[0], &[Some("cmd"), Some(&large)], true));
        // Many ambiguous branches must be shared, not exponentially enumerated.
        let rule = parse("cmd [a | a a | a a a]* end", Action::Allow).unwrap();
        let mut words = vec![Some("a"); 250];
        words[0] = Some("cmd");
        words.push(Some("end"));
        assert!(matches(&rule, &words, false));
        words.pop();
        assert!(!matches(&rule, &words, false));
    }
}
