//! One source-backed shell style for complete and progressive input.
//!
//! Shuck's AST supplies structure, not replacement text. Every edit is a whitespace gap between
//! validated original lexer tokens. Recovery-only suffixes are never emitted. Unclassified bytes,
//! quoting, arithmetic, substitutions, comments and heredoc tails retain their original spelling.

use std::collections::BTreeMap;
use std::ops::Range;

use shuck_ast::{Command, CompoundCommand, ForSyntax, IfSyntax, Span, Stmt, StmtSeq, TokenKind};
use shuck_parser::parser::{Lexer, Parser, ShellDialect, ShellProfile};

use super::document::{
    AdapterInput, Budget, Document, InputPolicy, LanguageAdapter, Role, SourceId,
};

/// Shell adapter, also usable for explicitly extracted nested shell programs.
pub struct ShellAdapter;

/// Present a shell command without changing execution/copy authority.
#[must_use]
pub fn present(source: &str, policy: InputPolicy, budget: &Budget) -> Document {
    let mut document = Document::original(source);
    let input = AdapterInput {
        source: SourceId(0),
        range: 0..source.len(),
        depth: 0,
    };
    if policy == InputPolicy::Literal || !ShellAdapter.present(&mut document, &input, budget) {
        document.push_source(SourceId(0), 0..source.len(), Role::Literal);
    }
    document
}

/// Extract source-validated simple-command invocation facts for embedded-language dispatch.
/// Recovery-only commands are never admitted. Invalid or exhausted input returns no facts.
#[must_use]
pub fn invocations(source: &str, budget: &Budget) -> Vec<super::extraction::Invocation> {
    if source.len() > budget.max_source_bytes || budget.max_depth == 0 {
        return Vec::new();
    }
    let parse = |text: &str| {
        Parser::with_limits_and_profile(
            text,
            syntax_depth(budget),
            budget.max_source_bytes.saturating_mul(32),
            ShellProfile::native(ShellDialect::Bash),
        )
        .parse()
    };
    let mut completed = source.to_owned();
    let mut partial_word = None;
    let mut parsed = parse(source);
    if !parsed.is_ok() {
        if let Some((candidate, range)) = quote_completion(source, budget.max_fragments) {
            completed = candidate;
            partial_word = Some(range);
            parsed = parse(&completed);
        }
        if !parsed.is_ok() {
            let (tokens, safe_end) = lex(&completed, budget.max_fragments);
            let Some(tokens) = tokens else {
                return Vec::new();
            };
            let Some(suffix) =
                recovery_suffix(&completed[..safe_end], &tokens, syntax_depth(budget))
            else {
                return Vec::new();
            };
            // A quote-completed word must remain in the input, not be discarded by recovery.
            if partial_word.is_some() && safe_end != completed.len() {
                return Vec::new();
            }
            completed = format!("{}\ntrue\n{suffix}", &completed[..safe_end]);
            parsed = parse(&completed);
            if !parsed.is_ok() {
                return Vec::new();
            }
        }
    }
    let mut plan = Plan {
        source,
        partial_word,
        tokens: &[],
        breaks: BTreeMap::new(),
        roles: BTreeMap::new(),
        max_depth: syntax_depth(budget),
        remaining: budget.max_fragments,
        invocations: Vec::new(),
        collect_invocations: true,
    };
    plan.sequence(&parsed.file.body, 0, false, 0);
    plan.invocations
}

impl LanguageAdapter for ShellAdapter {
    fn present(&self, document: &mut Document, input: &AdapterInput, budget: &Budget) -> bool {
        let Some(source) = document
            .sources
            .get(input.source.0)
            .and_then(|source| source.text.get(input.range.clone()))
        else {
            return false;
        };
        if source.len() > budget.max_source_bytes || input.depth >= budget.max_depth {
            return false;
        }
        // Build in a separate projection so failure cannot partially append or mutate sources.
        let mut projection = Document::original(source);
        if !project(&mut projection, budget) {
            return false;
        }
        if document
            .fragments
            .len()
            .saturating_add(projection.fragments.len())
            > budget.max_fragments
        {
            return false;
        }
        for mut fragment in projection.fragments {
            if let super::document::Provenance::Source { id, range } = &mut fragment.provenance {
                *id = input.source;
                range.start += input.range.start;
                range.end += input.range.start;
            }
            document.fragments.push(fragment);
        }
        true
    }
}

#[derive(Clone, Debug)]
struct Token {
    kind: TokenKind,
    range: Range<usize>,
}

#[derive(Clone, Copy)]
struct Break {
    indent: usize,
    newline: bool,
}

struct Plan<'a> {
    source: &'a str,
    partial_word: Option<Range<usize>>,
    tokens: &'a [Token],
    breaks: BTreeMap<usize, Break>,
    roles: BTreeMap<usize, Role>,
    max_depth: usize,
    remaining: usize,
    invocations: Vec<super::extraction::Invocation>,
    collect_invocations: bool,
}

fn project(document: &mut Document, budget: &Budget) -> bool {
    let source = document.execution_text().to_owned();
    let (tokens, safe_end) = lex(&source, budget.max_fragments);
    let Some(tokens) = tokens else { return false };
    let parse = |text: &str| {
        Parser::with_limits_and_profile(
            text,
            syntax_depth(budget),
            budget.max_source_bytes.saturating_mul(32),
            ShellProfile::native(ShellDialect::Bash),
        )
        .parse()
    };
    let mut parsed = parse(&source[..safe_end]);
    if !parsed.is_ok() {
        // Lexical nesting merely proposes a completion. A clean parser must verify it before
        // any original token receives structural meaning; recovered AST coverage is not trusted.
        let Some(suffix) = recovery_suffix(&source[..safe_end], &tokens, syntax_depth(budget))
        else {
            return false;
        };
        let completed = format!("{}\ntrue\n{suffix}", &source[..safe_end]);
        parsed = parse(&completed);
        if !parsed.is_ok() {
            return false;
        }
    }
    let mut plan = Plan {
        source: &source,
        partial_word: None,
        tokens: &tokens,
        breaks: BTreeMap::new(),
        roles: BTreeMap::new(),
        max_depth: syntax_depth(budget),
        remaining: budget.max_fragments,
        invocations: Vec::new(),
        collect_invocations: false,
    };
    plan.sequence(&parsed.file.body, 0, false, 0);
    emit(document, &source, &tokens, &plan, safe_end);
    document.fragments.len() <= budget.max_fragments
}

// Composition depth is caller policy; shell syntax depth is bounded independently by work
// admission. A four-level embedded budget must still permit ordinary if/loop/function syntax.
fn syntax_depth(budget: &Budget) -> usize {
    budget.max_fragments.min(128)
}

// The public lexer does not expose error payloads. A final error span beginning with an actual
// quote can be proposed for closure, but a second lexer must prove the completed interval is
// exactly one word. No quote counting, expansion guessing, or shell grammar is implemented here.
fn quote_completion(source: &str, limit: usize) -> Option<(String, Range<usize>)> {
    let mut lexer = Lexer::new(source);
    for _ in 0..limit {
        let token = lexer.next_lexed_token()?;
        if matches!(token.kind, TokenKind::HereDoc | TokenKind::HereDocStrip) {
            return None;
        }
        if token.kind != TokenKind::Error {
            continue;
        }
        let range = token.span.start.offset()..token.span.end.offset();
        if range.end != source.len() || lexer.next_lexed_token().is_some() {
            return None;
        }
        let text = source.get(range.clone())?;
        let quote = text.chars().next()?;
        if !matches!(quote, '\'' | '"') {
            return None;
        }
        let word = format!("{text}{quote}");
        let mut verification = Lexer::new(&word);
        let verified = verification.next_lexed_token()?;
        if !verified.kind.is_word_like()
            || verified.span.start.offset() != 0
            || verified.span.end.offset() != word.len()
            || verification.next_lexed_token().is_some()
        {
            return None;
        }
        return Some((format!("{source}{quote}"), range));
    }
    None
}

fn lex(source: &str, limit: usize) -> (Option<Vec<Token>>, usize) {
    let mut lexer = Lexer::new(source);
    let mut tokens = Vec::new();
    let mut end = 0;
    while let Some(token) = lexer.next_lexed_token() {
        let range = token.span.start.offset()..token.span.end.offset();
        if range.start < end || range.is_empty() || source.get(range.clone()).is_none() {
            return (None, 0);
        }
        // A standalone lexer does not own the parser's heredoc scheduling. The operator and
        // entire following tail remain literal, including multiple heredocs and <<- tabs.
        if matches!(
            token.kind,
            TokenKind::HereDoc | TokenKind::HereDocStrip | TokenKind::Error
        ) {
            return (Some(tokens), range.start);
        }
        end = range.end;
        tokens.push(Token {
            kind: token.kind,
            range,
        });
        if tokens.len() > limit {
            return (None, 0);
        }
    }
    (Some(tokens), source.len())
}

// These candidates cannot authorize formatting on their own. Only a clean parse of the exact
// original prefix plus this suffix can do that. Never splice or repair inside original bytes.
fn recovery_suffix(source: &str, tokens: &[Token], max_depth: usize) -> Option<String> {
    let mut stack: Vec<(&str, bool)> = Vec::new();
    let mut command_start = true;
    let mut arithmetic = 0_usize;
    for token in tokens {
        let text = source.get(token.range.clone())?;
        if token.kind == TokenKind::DoubleLeftParen {
            arithmetic += 1;
        } else if token.kind == TokenKind::DoubleRightParen {
            arithmetic = arithmetic.saturating_sub(1);
        }
        if arithmetic != 0 {
            continue;
        }
        if command_start && token.kind.is_word_like() {
            match text {
                "if" => stack.push(("fi", false)),
                "for" | "while" | "until" | "select" => stack.push(("done", false)),
                "case" => stack.push(("esac", false)),
                "then" | "do" | "in" => {
                    if let Some(top) = stack.last_mut() {
                        top.1 = true;
                    }
                }
                "fi" | "done" | "esac" if stack.last().is_some_and(|top| top.0 == text) => {
                    stack.pop();
                }
                _ => {}
            }
        }
        match token.kind {
            TokenKind::LeftBrace => stack.push(("}", true)),
            TokenKind::LeftParen => stack.push((")", true)),
            TokenKind::RightBrace | TokenKind::RightParen
                if stack.last().is_some_and(|top| top.0 == text) =>
            {
                stack.pop();
            }
            _ => {}
        }
        if stack.len() > max_depth {
            return None;
        }
        command_start = matches!(
            token.kind,
            TokenKind::Newline
                | TokenKind::Semicolon
                | TokenKind::And
                | TokenKind::Or
                | TokenKind::Pipe
                | TokenKind::PipeBoth
                | TokenKind::LeftBrace
                | TokenKind::LeftParen
        ) || matches!(text, "then" | "do" | "else" | "elif");
    }
    if arithmetic != 0 {
        return None;
    }
    let mut suffix = String::new();
    for (closer, body) in stack.into_iter().rev() {
        if !body {
            suffix.push_str(match closer {
                "fi" => "; then true; ",
                "done" => "; do true; ",
                // Incomplete case headers/patterns cannot be safely completed this way.
                _ => return None,
            });
        }
        suffix.push_str(closer);
        suffix.push('\n');
    }
    Some(suffix)
}

impl Plan<'_> {
    fn mark(&mut self, start: usize, indent: usize, newline: bool, role: Option<Role>) {
        if self
            .tokens
            .binary_search_by_key(&start, |token| token.range.start)
            .is_ok()
        {
            self.breaks
                .entry(start)
                .and_modify(|change| {
                    change.newline |= newline;
                    change.indent = indent;
                })
                .or_insert(Break { indent, newline });
            if let Some(role) = role {
                self.roles.insert(start, role);
            }
        }
    }

    fn keyword(&mut self, span: Span, spelling: &str, indent: usize, newline: bool) {
        let range = span.start.offset()..span.end.offset();
        let candidate = self
            .tokens
            .binary_search_by_key(&range.start, |token| token.range.start)
            .ok()
            .and_then(|index| self.tokens.get(index));
        if self.source.get(range.clone()) == Some(spelling)
            && candidate.is_some_and(|token| token.range == range)
        {
            self.mark(range.start, indent, newline, Some(Role::Keyword));
        }
    }

    fn between(&mut self, range: Range<usize>, spelling: &str, indent: usize, newline: bool) {
        // Only caller-provided AST grammar intervals authorize keyword interpretation.
        let first = self
            .tokens
            .partition_point(|token| token.range.start < range.start);
        let found = self.tokens[first..]
            .iter()
            .take_while(|token| token.range.end <= range.end)
            .find(|token| self.source.get(token.range.clone()) == Some(spelling));
        if let Some(token) = found {
            self.mark(token.range.start, indent, newline, Some(Role::Keyword));
        }
    }

    fn sequence(&mut self, sequence: &StmtSeq, indent: usize, first_break: bool, depth: usize) {
        for (index, statement) in sequence.stmts.iter().enumerate() {
            self.statement(statement, indent, first_break || index != 0, depth);
        }
    }

    fn statement(&mut self, statement: &Stmt, indent: usize, newline: bool, depth: usize) {
        if depth >= self.max_depth || self.remaining == 0 {
            return;
        }
        self.remaining -= 1;
        self.mark(statement.span.start.offset(), indent, newline, None);
        let collect = self.collect_invocations;
        // Enclosing compound redirections can override or supply stdin. Do not guess inheritance.
        if !statement.redirects.is_empty() && !matches!(statement.command, Command::Simple(_)) {
            self.collect_invocations = false;
        }
        match &statement.command {
            Command::Simple(command) => {
                if self.collect_invocations
                    && let Some(invocation) =
                        invocation(self.source, statement, command, self.partial_word.as_ref())
                {
                    self.invocations.push(invocation);
                }
                let range = command.name.span.start.offset()..command.name.span.end.offset();
                if self.tokens.iter().any(|token| token.range == range) {
                    self.roles.insert(range.start, Role::Command);
                }
            }
            Command::Binary(command) => {
                self.statement(&command.left, indent, false, depth + 1);
                // Keep pipelines and boolean chains cohesive; child compound bodies still expand.
                self.statement(&command.right, indent, false, depth + 1);
            }
            Command::Function(command) => {
                self.statement(&command.body, indent, false, depth + 1);
            }
            Command::AnonymousFunction(command) => {
                self.statement(&command.body, indent, false, depth + 1);
            }
            Command::Compound(command) => self.compound(command, statement.span, indent, depth + 1),
            Command::Builtin(_) | Command::Decl(_) => {}
        }
        self.collect_invocations = collect;
    }

    fn body(&mut self, body: &StmtSeq, indent: usize, depth: usize) {
        self.sequence(body, indent + 1, true, depth);
    }

    fn loop_body(
        &mut self,
        body: &StmtSeq,
        span: Span,
        closer: Option<Span>,
        indent: usize,
        depth: usize,
    ) {
        self.between(
            span.start.offset()..body.span.start.offset(),
            "do",
            indent,
            false,
        );
        self.body(body, indent, depth);
        if let Some(closer) = closer {
            self.keyword(closer, "done", indent, true);
        }
    }

    fn conditional(
        &mut self,
        command: &shuck_ast::IfCommand,
        span: Span,
        indent: usize,
        depth: usize,
    ) {
        self.between(
            span.start.offset()..command.condition.span.start.offset(),
            "if",
            indent,
            false,
        );
        self.sequence(&command.condition, indent, false, depth);
        if let IfSyntax::ThenFi { then_span, fi_span } = command.syntax {
            self.keyword(then_span, "then", indent, false);
            self.keyword(fi_span, "fi", indent, true);
        }
        self.body(&command.then_branch, indent, depth);
        let mut previous = command.then_branch.span.end.offset();
        for (condition, body) in &command.elif_branches {
            self.between(
                previous..condition.span.start.offset(),
                "elif",
                indent,
                true,
            );
            self.sequence(condition, indent, false, depth);
            self.between(
                condition.span.end.offset()..body.span.start.offset(),
                "then",
                indent,
                false,
            );
            self.body(body, indent, depth);
            previous = body.span.end.offset();
        }
        if let Some(body) = &command.else_branch {
            self.between(previous..body.span.start.offset(), "else", indent, true);
            self.body(body, indent, depth);
        }
    }

    fn compound(&mut self, command: &CompoundCommand, span: Span, indent: usize, depth: usize) {
        match command {
            CompoundCommand::If(command) => self.conditional(command, span, indent, depth),
            CompoundCommand::For(command) => {
                let closer = match command.syntax {
                    ForSyntax::InDoDone { done_span, .. }
                    | ForSyntax::ParenDoDone { done_span, .. } => Some(done_span),
                    _ => None,
                };
                self.loop_body(&command.body, command.span, closer, indent, depth);
            }
            CompoundCommand::While(command) => {
                self.sequence(&command.condition, indent, false, depth);
                self.loop_body(
                    &command.body,
                    command.span,
                    command.done_span,
                    indent,
                    depth,
                );
            }
            CompoundCommand::Until(command) => {
                self.sequence(&command.condition, indent, false, depth);
                self.loop_body(
                    &command.body,
                    command.span,
                    command.done_span,
                    indent,
                    depth,
                );
            }
            CompoundCommand::Select(command) => {
                self.loop_body(
                    &command.body,
                    command.span,
                    Some(command.done_span),
                    indent,
                    depth,
                );
            }
            CompoundCommand::ArithmeticFor(command) => {
                self.loop_body(
                    &command.body,
                    command.span,
                    command.done_span,
                    indent,
                    depth,
                );
            }
            CompoundCommand::Subshell(body) | CompoundCommand::BraceGroup(body) => {
                self.body(body, indent, depth);
                let closer = if matches!(command, CompoundCommand::Subshell(_)) {
                    ")"
                } else {
                    "}"
                };
                // Shuck's group sequence span ends after the consumed closer, unlike if/loop
                // body spans. Select the final original closer inside that grammar interval.
                let start = body
                    .stmts
                    .last()
                    .map_or_else(|| body.span.start.offset(), |stmt| stmt.span.start.offset());
                let end = body.span.end.offset().max(span.end.offset());
                let first = self
                    .tokens
                    .partition_point(|token| token.range.start < start);
                if let Some(token) = self.tokens[first..]
                    .iter()
                    .take_while(|token| token.range.end <= end)
                    .filter(|token| self.source.get(token.range.clone()) == Some(closer))
                    .last()
                {
                    self.mark(token.range.start, indent, true, Some(Role::Operator));
                }
            }
            CompoundCommand::Case(command) => {
                for item in &command.cases {
                    if let Some(pattern) = item.patterns.first() {
                        self.mark(pattern.span.start.offset(), indent + 1, true, None);
                    }
                    self.sequence(&item.body, indent + 2, true, depth);
                }
                self.keyword(command.esac_span, "esac", indent, true);
            }
            CompoundCommand::Time(command) => {
                if let Some(command) = &command.command {
                    self.statement(command, indent, false, depth);
                }
            }
            CompoundCommand::Coproc(command) => self.statement(&command.body, indent, false, depth),
            // Arithmetic and [[ ]] remain indivisible original source. Non-Bash extensions are
            // deliberately literal until an adapter with a corresponding dialect is requested.
            _ => {}
        }
    }
}

fn invocation(
    source: &str,
    statement: &Stmt,
    command: &shuck_ast::SimpleCommand,
    partial_word: Option<&Range<usize>>,
) -> Option<super::extraction::Invocation> {
    use shuck_ast::RedirectKind;
    let mut words = Vec::new();
    for word in std::iter::once(&command.name).chain(&command.args) {
        let mut range = word.span.start.offset()..word.span.end.offset();
        let partial = partial_word.is_some_and(|original| {
            original.start == range.start
                && original.end == source.len()
                && range.end == original.end + 1
        });
        if partial {
            range.end = source.len();
        }
        let text = source.get(range.clone())?;
        // Completion evidence was checked against a single exact lexer word. Only the final
        // synthetic quote is clipped; incomplete/dynamic content is left to static extraction.
        let validation = if partial {
            format!("{text}{}", text.chars().next()?)
        } else {
            text.to_owned()
        };
        let mut lexer = Lexer::new(&validation);
        let token = lexer.next_lexed_token()?;
        if !token.kind.is_word_like()
            || token.span.start.offset() != 0
            || token.span.end.offset() != validation.len()
            || lexer.next_lexed_token().is_some()
        {
            return None;
        }
        if words
            .last()
            .is_some_and(|previous: &Range<usize>| previous.end > range.start)
        {
            return None;
        }
        words.push(range);
    }
    let mut stdin = None;
    let mut previous = 0;
    for redirect in &statement.redirects {
        let range = redirect.span.start.offset()..redirect.span.end.offset();
        source.get(range.clone())?;
        if range.start < previous || redirect.fd_var.is_some() {
            return None;
        }
        previous = range.end;
        let input_kind = matches!(
            redirect.kind,
            RedirectKind::Input
                | RedirectKind::ReadWrite
                | RedirectKind::HereDoc
                | RedirectKind::HereDocStrip
                | RedirectKind::HereString
                | RedirectKind::DupInput
        );
        let fd = redirect.fd.unwrap_or_else(|| i32::from(!input_kind));
        if fd != 0 {
            continue;
        }
        // Every subsequent fd0 redirect replaces the earlier association, including 0>&N.
        stdin = None;
        if let Some(heredoc) = redirect.heredoc() {
            let body = heredoc.body.span.start.offset()..heredoc.body.span.end.offset();
            // The body span is original lexer geometry even when Shuck's decoded body parts
            // are not source-backed (notably <<-). Extraction consumes the original slice and
            // performs its own tab stripping; never use decoded AST body text here.
            source.get(body.clone())?;
            let closing = source
                .get(body.end..)?
                .split('\n')
                .next()?
                .trim_end_matches('\r');
            let closing = if heredoc.delimiter.strip_tabs {
                closing.trim_start_matches('\t')
            } else {
                closing
            };
            let complete = closing == heredoc.delimiter.cooked.as_str();
            if !complete && body.end != source.len() {
                return None;
            }
            stdin = Some(super::extraction::Heredoc {
                body,
                quoted: heredoc.delimiter.quoted,
                strip_tabs: heredoc.delimiter.strip_tabs,
                complete,
            });
        }
    }
    Some(super::extraction::Invocation { words, stdin })
}

fn emit(document: &mut Document, source: &str, tokens: &[Token], plan: &Plan<'_>, safe_end: usize) {
    let mut offset = 0;
    let mut previous_kind = None;
    for token in tokens {
        let gap = offset..token.range.start;
        let text = &source[gap.clone()];
        if let Some(change) = plan.breaks.get(&token.range.start)
            && text.bytes().all(|byte| matches!(byte, b' ' | b'\t' | b'\n'))
            // Never turn an escaped newline, comment terminator, or CRLF into generated bytes.
            && !source[..offset].ends_with('\\')
            && previous_kind != Some(TokenKind::Comment)
        {
            if change.newline && token.range.start != 0 {
                let prefix = if document
                    .fragments
                    .last()
                    .is_some_and(|fragment| fragment.text.ends_with('\n'))
                {
                    ""
                } else {
                    "\n"
                };
                document.push_generated(&format!("{prefix}{}", "  ".repeat(change.indent)));
            } else {
                document.push_source(SourceId(0), gap, Role::Whitespace);
            }
        } else {
            document.push_source(SourceId(0), gap, Role::Literal);
        }
        let role = plan
            .roles
            .get(&token.range.start)
            .copied()
            .unwrap_or_else(|| match token.kind {
                TokenKind::Comment => Role::Comment,
                TokenKind::QuotedWord => Role::String,
                TokenKind::Newline => Role::Whitespace,
                kind if kind.is_word_like() => Role::Literal,
                _ => Role::Operator,
            });
        document.push_source(SourceId(0), token.range.clone(), role);
        offset = token.range.end;
        previous_kind = Some(token.kind);
    }
    // Lexer scheduling and recovery never authorize eliding the unrecognized original tail.
    debug_assert!(offset <= safe_end);
    document.push_source(SourceId(0), offset..source.len(), Role::Literal);
}

#[cfg(test)]
mod tests {
    use super::super::document::Provenance;
    use super::*;

    fn display(source: &str) -> String {
        present(source, InputPolicy::Format, &Budget::default()).display_text()
    }

    fn assert_provenance(source: &str) {
        let document = present(source, InputPolicy::Format, &Budget::default());
        assert_eq!(document.execution_text(), source);
        let mut previous = 0;
        for fragment in &document.fragments {
            match &fragment.provenance {
                Provenance::Source { id, range } => {
                    assert_eq!(*id, SourceId(0));
                    assert!(range.start >= previous);
                    assert!(source[previous..range.start].trim().is_empty());
                    assert_eq!(fragment.text, source[range.clone()]);
                    previous = range.end;
                }
                Provenance::Generated => assert!(fragment.text.trim().is_empty()),
            }
        }
        assert!(source[previous..].trim().is_empty());
    }

    #[test]
    fn complete_and_progressive_compounds_use_one_style() {
        let source = "if true; then echo 'hello; world'; else echo tail; fi";
        assert!(display(source).contains("then\n  echo 'hello; world';\nelse\n  echo tail;\nfi"));
        for end in (0..=source.len()).filter(|end| source.is_char_boundary(*end)) {
            assert_provenance(&source[..end]);
        }
        let incomplete = "if true; then echo ready; echo \"unfinished";
        assert!(display(incomplete).contains("then\n  echo ready;"));
    }

    #[test]
    fn grammar_covers_loops_groups_functions_case_and_chains() {
        for source in [
            "for x in a b; do echo $x; done",
            "while test -e x; do echo yes; done",
            "until false; do echo done; done",
            "for ((i=0;i<3;i++)); do echo $i; done",
            "f() { echo yes; echo no; }; f | cat && echo ok",
            "(echo a; echo b) | cat",
            "case $x in a|b) echo one;; *) echo two;; esac",
            "if a; then b; elif c; then d; else e; fi",
        ] {
            assert_provenance(source);
            assert!(
                display(source).contains('\n'),
                "{source}: {}",
                display(source)
            );
        }
    }

    #[test]
    fn literal_regions_and_uncertain_source_are_not_reconstructed() {
        for source in [
            "echo 'if a; then b; fi'",
            "echo \"$(echo 'a;b')\"",
            "echo $((1 + 2))",
            "echo a; # comment; fi\necho b",
            "cat <<'PY'\n  print('a;b')\nPY",
            "cat <<-EOF <<'NEXT'\n\tfirst\nEOF\nsecond\nNEXT",
            "echo 界😀e\u{301}; echo 'two  spaces'",
            "echo a \\\n  b",
            "if bad @@@ ???; echo 'unfinished",
        ] {
            assert_provenance(source);
        }
        let heredoc = "echo before; cat <<-EOF\n\t untouched; if\nEOF";
        assert!(display(heredoc).ends_with("<<-EOF\n\t untouched; if\nEOF"));
        assert_eq!(display("echo 'a;b'"), "echo 'a;b'");
    }

    #[test]
    fn nested_source_has_explicit_authority_and_bounded_admission() {
        let mut document = Document::original("bash -c 'echo a; echo b'");
        let id = document
            .add_decoded("echo a; echo b".to_owned(), SourceId(0), 8..23)
            .unwrap();
        let input = AdapterInput {
            source: id,
            range: 0..14,
            depth: 1,
        };
        assert!(ShellAdapter.present(&mut document, &input, &Budget::default()));
        assert_eq!(document.execution_text(), "bash -c 'echo a; echo b'");
        assert!(document.fragments.iter().any(|fragment| matches!(fragment.provenance, Provenance::Source { id: source, .. } if source == id)));
        let previous = document.clone();
        let budget = Budget {
            max_depth: 1,
            ..Budget::default()
        };
        assert!(!ShellAdapter.present(&mut document, &input, &budget));
        assert_eq!(document, previous);
    }

    #[test]
    fn progressive_eval_words_keep_original_ranges_and_static_certainty() {
        let budget = Budget {
            max_depth: 4,
            ..Budget::default()
        };
        for source in [
            "python3 -c 'print(1)",
            "python3 -c \"print(1)",
            "if true; then python3 -c 'print(1)",
        ] {
            let facts = invocations(source, &budget);
            let fact = facts
                .iter()
                .find(|fact| &source[fact.words[0].clone()] == "python3")
                .unwrap();
            assert_eq!(&source[fact.words[2].clone()][1..], "print(1)");
            assert_eq!(fact.words[2].end, source.len());
            let extracted =
                super::super::extraction::extract(source, fact, budget.max_source_bytes).unwrap();
            assert_eq!(extracted.text, "print(1)");
            assert!(!extracted.complete);
            assert!(!extracted.uncertain);
        }
        let source = "python3 -c \"print($VALUE)";
        let facts = invocations(source, &budget);
        let extracted =
            super::super::extraction::extract(source, &facts[0], budget.max_source_bytes).unwrap();
        assert!(extracted.uncertain);
        // A substitution error is not a quote error and must not be guessed closed.
        assert!(quote_completion("python3 -c \"$(echo hi", budget.max_fragments).is_none());
    }

    #[test]
    fn composition_depth_does_not_limit_shell_syntax_depth() {
        let budget = Budget {
            max_depth: 4,
            ..Budget::default()
        };
        let source = "if true; then for x in a; do while true; do if true; then python3 -c 'print(1)'; fi; done; done; fi";
        let document = present(source, InputPolicy::Format, &budget);
        assert!(document.display_text().contains("\n        python3"));
        assert_eq!(document.execution_text(), source);
        assert!(
            invocations(source, &budget)
                .iter()
                .any(|fact| &source[fact.words[0].clone()] == "python3")
        );
    }

    #[test]
    fn invocation_facts_are_source_backed_and_fd_zero_is_ordered() {
        let budget = Budget::default();
        let source = "python -c 'print(1)'";
        let facts = invocations(source, &budget);
        assert_eq!(facts.len(), 1);
        assert_eq!(
            facts[0]
                .words
                .iter()
                .map(|range| &source[range.clone()])
                .collect::<Vec<_>>(),
            vec!["python", "-c", "'print(1)'"]
        );
        assert!(facts[0].stdin.is_none());
        let source = "python <<'PY'\nprint(1)\nPY\n";
        let facts = invocations(source, &budget);
        assert_eq!(facts.len(), 1);
        let heredoc = facts[0].stdin.as_ref().unwrap();
        assert_eq!(&source[heredoc.body.clone()], "print(1)\n");
        assert!(heredoc.quoted && heredoc.complete);
        let source = "python <<-'PY'\n\tprint(1)\n\tPY\n";
        let facts = invocations(source, &budget);
        assert!(facts[0].stdin.as_ref().unwrap().strip_tabs);
        let source = "python <<'PY' </dev/null\nprint(1)\nPY\n";
        let facts = invocations(source, &budget);
        assert!(facts[0].stdin.is_none());
        let source = "python <<'PY'\nprint(1)\n";
        let progressive = invocations(source, &budget);
        assert_eq!(progressive.len(), 1);
        let body = progressive[0].stdin.as_ref().unwrap();
        assert!(!body.complete);
        assert_eq!(&source[body.body.clone()], "print(1)\n");
        let extracted =
            super::super::extraction::extract(source, &progressive[0], budget.max_source_bytes)
                .unwrap();
        assert_eq!(extracted.text, "print(1)\n");
        assert!(!extracted.complete && !extracted.uncertain);
        let source = "if true; then python -c 'print(1)'";
        assert!(
            invocations(source, &budget)
                .iter()
                .any(|fact| &source[fact.words[0].clone()] == "python")
        );
        let source = "if true; then python -c 'print(";
        assert!(invocations(source, &budget).iter().all(|fact| {
            fact.words
                .iter()
                .all(|range| source.get(range.clone()).is_some())
        }));
    }

    #[test]
    fn disabled_and_exhausted_presentation_is_literal() {
        let source = "if true; then echo x; fi";
        assert_eq!(
            present(source, InputPolicy::Literal, &Budget::default()).display_text(),
            source
        );
        for budget in [
            Budget {
                max_source_bytes: 1,
                ..Budget::default()
            },
            Budget {
                max_fragments: 1,
                ..Budget::default()
            },
            Budget {
                max_depth: 0,
                ..Budget::default()
            },
        ] {
            assert_eq!(
                present(source, InputPolicy::Format, &budget).display_text(),
                source
            );
        }
    }
}
