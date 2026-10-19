//! Borrowed policy subjects. Canonical strings are temporary, never retained per wrapper level.

use std::borrow::Cow;

use bcode_shell_command_analysis_models::{ShellCommand, ShellSourceSpan, ShellWord};

pub trait CommandWords {
    fn executable(&self) -> &ShellWord;
    fn arguments(&self) -> &[ShellWord];
}

impl CommandWords for ShellCommand {
    fn executable(&self) -> &ShellWord {
        &self.executable
    }
    fn arguments(&self) -> &[ShellWord] {
        &self.arguments
    }
}

pub struct Subject<'a> {
    pub has_assignments: bool,
    pub executable: Cow<'a, ShellWord>,
    pub arguments: &'a [ShellWord],
    pub source: &'a str,
    pub span: ShellSourceSpan,
    pub candidates: Vec<Cow<'a, str>>,
}

impl CommandWords for Subject<'_> {
    fn executable(&self) -> &ShellWord {
        &self.executable
    }
    fn arguments(&self) -> &[ShellWord] {
        self.arguments
    }
}

impl<'a> Subject<'a> {
    pub fn original(command: &'a ShellCommand) -> Self {
        Self {
            has_assignments: !command.assignments.is_empty(),
            executable: Cow::Borrowed(&command.executable),
            arguments: &command.arguments,
            source: &command.source,
            span: command.span,
            candidates: command
                .match_candidates
                .iter()
                .map(|candidate| Cow::Borrowed(candidate.subject.as_str()))
                .collect(),
        }
    }

    pub fn restriction(
        original: &Self,
        executable: &'a ShellWord,
        arguments: &'a [ShellWord],
        normalize: bool,
    ) -> Self {
        let executable = if normalize {
            Cow::Owned(ShellWord::Static {
                value: literal(executable)
                    .expect("validated static executable")
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
                span: executable.span(),
            })
        } else {
            Cow::Borrowed(executable)
        };
        let source = executable
            .span()
            .start
            .checked_sub(original.span.start)
            .and_then(|offset| original.source.get(offset as usize..))
            .unwrap_or(original.source);
        let mut subject = Self {
            has_assignments: false,
            executable,
            arguments,
            source,
            span: original.span,
            candidates: vec![Cow::Borrowed(source)],
        };
        subject.add_canonical();
        subject
    }

    pub fn add_candidate(&mut self, candidate: Cow<'a, str>) {
        if !self.candidates.contains(&candidate) {
            self.candidates.push(candidate);
        }
    }

    pub fn add_canonical(&mut self) {
        let words = std::iter::once(self.executable())
            .chain(self.arguments())
            .map(literal)
            .collect::<Option<Vec<_>>>();
        if let Some(words) = words
            && words
                .iter()
                .all(|word| !word.is_empty() && !word.chars().any(char::is_whitespace))
        {
            self.add_candidate(Cow::Owned(words.join(" ")));
        }
    }
}

pub fn literal(word: &ShellWord) -> Option<&str> {
    match word {
        ShellWord::Static { value, .. } => Some(value),
        ShellWord::Dynamic { .. } => None,
    }
}

pub fn validate_executable(word: &ShellWord) -> Result<(), &'static str> {
    match literal(word) {
        None => Err("effective executable identity is unresolved"),
        Some("eval" | "." | "source") => Err("effective command contains unresolved shell source"),
        Some(_) => Ok(()),
    }
}
