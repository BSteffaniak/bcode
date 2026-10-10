//! Shell-token-backed presentation of complete and incomplete commands.
//!
//! Only whitespace outside source-backed words is changed. Quoted scripts,
//! substitutions, arithmetic and heredoc bodies are never rewritten.

use shuck_ast::TokenKind;
use shuck_parser::parser::Lexer;

pub(super) fn beautify(source: &str) -> String {
    let mut lexer = Lexer::new(source);
    let mut output = String::with_capacity(source.len());
    let mut offset = 0;
    let mut depth = 0_usize;
    let mut parens = 0_usize;
    let mut command_start = true;
    let mut header = false;
    while let Some(token) = lexer.next_lexed_token() {
        let start = token.span.start.offset();
        let end = token.span.end.offset();
        let Some(text) = source.get(start..end) else {
            break;
        };
        let gap = &source[offset..start];
        // The lexer is not the parser's heredoc scheduler. Preserve the complete remainder.
        if matches!(
            token.kind,
            TokenKind::HereDoc | TokenKind::HereDocStrip | TokenKind::Error
        ) {
            output.push_str(gap);
            output.push_str(&source[start..]);
            return output;
        }
        if !gap.trim().is_empty()
            || (!output.ends_with('\n') && (gap.contains('\n') || !output.is_empty()))
        {
            output.push_str(gap);
        }
        let keyword = command_start && token.kind == TokenKind::Word;
        if keyword && matches!(text, "fi" | "done" | "esac" | "else" | "elif") {
            depth = depth.saturating_sub(1);
            newline(&mut output);
        }
        if output.ends_with('\n') {
            output.push_str(&"  ".repeat(depth));
        }
        output.push_str(text);
        offset = end;
        match token.kind {
            TokenKind::LeftParen | TokenKind::DoubleLeftParen => parens += 1,
            TokenKind::RightParen | TokenKind::DoubleRightParen => {
                parens = parens.saturating_sub(1);
            }
            TokenKind::Semicolon if parens == 0 && !header => {
                newline(&mut output);
                command_start = true;
                continue;
            }
            TokenKind::And | TokenKind::Or | TokenKind::Pipe | TokenKind::PipeBoth
                if parens == 0 =>
            {
                newline(&mut output);
                command_start = true;
                continue;
            }
            TokenKind::Newline => {
                command_start = true;
                continue;
            }
            _ => {}
        }
        if keyword {
            match text {
                "for" | "select" | "case" => header = true,
                "then" | "do" | "else" => {
                    header = false;
                    depth += 1;
                    newline(&mut output);
                    command_start = true;
                    continue;
                }
                "if" | "elif" | "while" | "until" => {
                    command_start = true;
                    continue;
                }
                _ => {}
            }
        }
        // `then` and `do` follow statement terminators; a for header's separator ends it.
        if token.kind == TokenKind::Semicolon && parens == 0 {
            header = false;
            newline(&mut output);
            command_start = true;
        } else {
            command_start = false;
        }
    }
    output.push_str(&source[offset..]);
    output
}

fn newline(output: &mut String) {
    while output.ends_with([' ', '\t']) {
        output.pop();
    }
    if !output.ends_with('\n') {
        output.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::beautify;

    #[test]
    fn structure_is_readable_before_closing_quote_or_compound_arrives() {
        let source = "if true; then echo 'hello; world'; else echo \"unfinished";
        let rendered = beautify(source);
        assert!(
            rendered.contains("then\n  echo 'hello; world';\nelse\n  echo \"unfinished"),
            "{rendered}"
        );
        assert_eq!(beautify("echo 'a;b && c'"), "echo 'a;b && c'");
    }

    #[test]
    fn preserves_arithmetic_comments_and_heredocs() {
        for source in [
            "for ((i=0;i<3;i++)); do echo $i; done",
            "echo one; # untouched; comment\necho two",
            "cat <<'PY'\n  print('a;b')\nPY",
        ] {
            let rendered = beautify(source);
            assert!(
                rendered.contains(if source.starts_with("for") {
                    "((i=0;i<3;i++))"
                } else if source.starts_with("echo") {
                    "# untouched; comment"
                } else {
                    "  print('a;b')"
                }),
                "{rendered}"
            );
        }
    }
}
