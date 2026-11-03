//! Shared terminal presentation for guided authentication operations.

use super::{CliError, read_auth_input_line};
use std::collections::BTreeSet;
use std::io::{IsTerminal as _, Write};

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

pub struct Prompter<R, W> {
    pub reader: R,
    pub writer: W,
    pub interactive: bool,
    pub rich: bool,
    pub back_requested: bool,
}

pub fn terminal() -> Prompter<std::io::Stdin, std::io::Stderr> {
    Prompter {
        reader: std::io::stdin(),
        writer: std::io::stderr(),
        interactive: interactive(),
        rich: bcode_tui::auth_inline::supported(),
        back_requested: false,
    }
}

impl<R: std::io::Read, W: Write> Prompter<R, W> {
    fn navigation(&mut self, back: bool) -> CliError {
        self.back_requested = back;
        CliError::LoginProfile(if back {
            "Return to previous authentication step.".to_owned()
        } else {
            "Authentication cancelled before execution.".to_owned()
        })
    }

    pub fn choose(
        &mut self,
        title: &str,
        labels: &[String],
        hint: &str,
    ) -> Result<usize, CliError> {
        self.back_requested = false;
        if !self.interactive {
            return Err(CliError::LoginProfile(format!("{title}. {hint}")));
        }
        if labels.is_empty() {
            return Err(CliError::LoginProfile(format!(
                "{title}: no available choices. {hint}"
            )));
        }
        if self.rich {
            let config = bcode_config::load_config()?;
            return match bcode_tui::auth_inline::choose(title, labels, &config.tui)? {
                bcode_tui::auth_inline::AuthChoiceOutcome::Selected(index) => Ok(index),
                bcode_tui::auth_inline::AuthChoiceOutcome::Back => Err(self.navigation(true)),
                bcode_tui::auth_inline::AuthChoiceOutcome::Cancelled => Err(self.navigation(false)),
            };
        }
        writeln!(self.writer, "{title}")?;
        for (index, label) in labels.iter().enumerate() {
            writeln!(self.writer, "  {}) {label}", index + 1)?;
        }
        loop {
            write!(
                self.writer,
                "Select 1-{} (b back, q cancel): ",
                labels.len()
            )?;
            self.writer.flush()?;
            let answer =
                read_auth_input_line(std::io::BufReader::with_capacity(1, &mut self.reader))?;
            match answer.trim() {
                "q" | "Q" | "" => return Err(self.navigation(false)),
                "b" | "B" => return Err(self.navigation(true)),
                answer => {
                    if let Ok(number) = answer.parse::<usize>()
                        && let Some(index) =
                            number.checked_sub(1).filter(|index| *index < labels.len())
                    {
                        return Ok(index);
                    }
                }
            }
            writeln!(self.writer, "Enter a listed number, b, or q.")?;
        }
    }

    pub fn choose_many(
        &mut self,
        title: &str,
        labels: &[String],
        hint: &str,
    ) -> Result<Vec<usize>, CliError> {
        self.back_requested = false;
        if !self.interactive || labels.is_empty() {
            return Err(CliError::LoginProfile(format!("{title}. {hint}")));
        }
        if self.rich {
            let config = bcode_config::load_config()?;
            return match bcode_tui::auth_inline::choose_many(title, labels, &config.tui)? {
                bcode_tui::auth_inline::AuthMultiChoiceOutcome::Selected(indices) => Ok(indices),
                bcode_tui::auth_inline::AuthMultiChoiceOutcome::Back => Err(self.navigation(true)),
                bcode_tui::auth_inline::AuthMultiChoiceOutcome::Cancelled => {
                    Err(self.navigation(false))
                }
            };
        }
        writeln!(self.writer, "{title}")?;
        for (index, label) in labels.iter().enumerate() {
            writeln!(self.writer, "  {}) {label}", index + 1)?;
        }
        loop {
            write!(
                self.writer,
                "Select comma-separated numbers, all, b back, or q cancel: "
            )?;
            self.writer.flush()?;
            let answer =
                read_auth_input_line(std::io::BufReader::with_capacity(1, &mut self.reader))?;
            match answer.trim() {
                "q" | "Q" | "" => return Err(self.navigation(false)),
                "b" | "B" => return Err(self.navigation(true)),
                "all" => return Ok((0..labels.len()).collect()),
                answer => {
                    if let Some(indices) = parse_selection(answer, labels.len()) {
                        return Ok(indices);
                    }
                }
            }
            writeln!(
                self.writer,
                "Select at least one listed number, all, b, or q."
            )?;
        }
    }
}

fn parse_selection(answer: &str, count: usize) -> Option<Vec<usize>> {
    let indices = answer
        .split(',')
        .map(|part| {
            part.trim()
                .parse::<usize>()
                .ok()?
                .checked_sub(1)
                .filter(|index| *index < count)
        })
        .collect::<Option<BTreeSet<_>>>()?;
    (!indices.is_empty()).then(|| indices.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompter(input: &[u8], interactive: bool) -> Prompter<&[u8], Vec<u8>> {
        Prompter {
            reader: input,
            writer: Vec::new(),
            interactive,
            rich: false,
            back_requested: false,
        }
    }

    #[test]
    fn explicit_multi_selection_is_bounded_and_deduplicated() {
        assert_eq!(parse_selection("2,1,2", 2), Some(vec![0, 1]));
        for input in ["", "0", "3", "1,no", "1,"] {
            assert_eq!(parse_selection(input, 2), None);
        }
    }

    #[test]
    fn fallback_supports_back_cancel_and_explicit_all() {
        let labels = vec!["personal".to_owned(), "work".to_owned()];
        let mut prompt = prompter(b"b\n", true);
        assert!(
            prompt
                .choose_many("Accounts", &labels, "use --profile")
                .is_err()
        );
        assert!(prompt.back_requested);
        let mut prompt = prompter(b"q\n", true);
        assert!(prompt.choose("Accounts", &labels, "use --profile").is_err());
        assert!(!prompt.back_requested);
        assert_eq!(
            prompter(b"all\n", true)
                .choose_many("Accounts", &labels, "hint")
                .unwrap(),
            vec![0, 1]
        );
    }

    #[test]
    fn noninteractive_never_consumes_input() {
        let mut prompt = prompter(b"1\n", false);
        assert!(
            prompt
                .choose("Accounts", &["one".to_owned()], "Pass --profile")
                .is_err()
        );
        assert_eq!(prompt.reader, b"1\n");
    }
}
