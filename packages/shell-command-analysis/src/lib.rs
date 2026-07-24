#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bounded POSIX shell command analysis behind Bcode-owned contracts.

use bcode_shell_command_analysis_models::{
    SHELL_ANALYSIS_SCHEMA_VERSION, ShellAnalysis, ShellAnalysisCompleteness,
    ShellAnalysisDiagnostic, ShellAnalysisError, ShellAnalysisErrorKind, ShellAnalysisLimitKind,
    ShellAssignment, ShellCommand, ShellCommandContext, ShellCommandId, ShellCommandMatchCandidate,
    ShellCommandMatchCandidateKind, ShellCommandRelation, ShellDialect, ShellExpansionKind,
    ShellFileDescriptor, ShellIncompleteReason, ShellPipelinePosition, ShellRedirection,
    ShellRedirectionKind, ShellSourceSpan, ShellWord,
};
use brush_parser::ast::{self, SourceLocation};
use brush_parser::word::{self, WordPiece};
use brush_parser::{Parser, ParserOptions, SourceSpan};
use std::io::Cursor;

/// Analyze one complete shell program.
///
/// # Errors
///
/// Returns an error when:
///
/// * the request schema is unsupported;
/// * the source exceeds its configured byte limit;
/// * shell syntax is invalid; or
/// * parser source locations cannot be represented safely.
pub fn analyze(
    request: &bcode_shell_command_analysis_models::ShellAnalysisRequest,
) -> Result<ShellAnalysis, ShellAnalysisError> {
    if request.schema_version != SHELL_ANALYSIS_SCHEMA_VERSION {
        return Err(error(
            request.dialect,
            ShellAnalysisErrorKind::UnsupportedSchema,
            format!(
                "unsupported shell analysis schema version {}; expected {}",
                request.schema_version, SHELL_ANALYSIS_SCHEMA_VERSION
            ),
        ));
    }
    if request.source.len() > request.limits.max_source_bytes as usize {
        return Err(error(
            request.dialect,
            ShellAnalysisErrorKind::SourceLimitExceeded,
            format!(
                "shell source is {} bytes; maximum is {} bytes",
                request.source.len(),
                request.limits.max_source_bytes
            ),
        ));
    }

    let options = parser_options();
    let program = Parser::new(Cursor::new(request.source.as_bytes()), &options)
        .parse_program()
        .map_err(|parse_error| {
            error(
                request.dialect,
                ShellAnalysisErrorKind::Syntax,
                parse_error.to_string(),
            )
        })?;
    let mut adapter = Adapter::new(request, options);
    adapter.program(&program)?;
    Ok(adapter.finish())
}

fn parser_options() -> ParserOptions {
    ParserOptions {
        enable_extended_globbing: false,
        posix_mode: true,
        sh_mode: true,
        tilde_expansion_at_word_start: true,
        tilde_expansion_after_colon: false,
        ..ParserOptions::default()
    }
}

const fn error(
    dialect: ShellDialect,
    kind: ShellAnalysisErrorKind,
    message: String,
) -> ShellAnalysisError {
    ShellAnalysisError {
        kind,
        message,
        dialect,
        span: None,
    }
}

#[derive(Clone, Copy)]
struct WalkContext {
    relation: ShellCommandRelation,
    parent_id: Option<ShellCommandId>,
    conditional: bool,
    background: bool,
    loop_depth: u16,
    substitution_depth: u16,
}

impl Default for WalkContext {
    fn default() -> Self {
        Self {
            relation: ShellCommandRelation::Root,
            parent_id: None,
            conditional: false,
            background: false,
            loop_depth: 0,
            substitution_depth: 0,
        }
    }
}

struct Adapter<'a> {
    request: &'a bcode_shell_command_analysis_models::ShellAnalysisRequest,
    options: ParserOptions,
    char_to_byte: Vec<usize>,
    commands: Vec<ShellCommand>,
    redirections: Vec<ShellRedirection>,
    incomplete: Vec<ShellIncompleteReason>,
    diagnostics: Vec<ShellAnalysisDiagnostic>,
    nodes: u32,
}

impl<'a> Adapter<'a> {
    fn new(
        request: &'a bcode_shell_command_analysis_models::ShellAnalysisRequest,
        options: ParserOptions,
    ) -> Self {
        let mut char_to_byte = request
            .source
            .char_indices()
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        char_to_byte.push(request.source.len());
        Self {
            request,
            options,
            char_to_byte,
            commands: Vec::new(),
            redirections: Vec::new(),
            incomplete: Vec::new(),
            diagnostics: Vec::new(),
            nodes: 0,
        }
    }

    fn finish(self) -> ShellAnalysis {
        ShellAnalysis {
            schema_version: SHELL_ANALYSIS_SCHEMA_VERSION,
            dialect: self.request.dialect,
            source: self.request.source.clone(),
            commands: self.commands,
            redirections: self.redirections,
            completeness: if self.incomplete.is_empty() {
                ShellAnalysisCompleteness::Complete
            } else {
                ShellAnalysisCompleteness::Incomplete {
                    reasons: self.incomplete,
                }
            },
            diagnostics: self.diagnostics,
        }
    }

    fn visit_node(&mut self, depth: u16) {
        self.nodes = self.nodes.saturating_add(1);
        if self.nodes == self.request.limits.max_nodes.saturating_add(1) {
            self.incomplete.push(ShellIncompleteReason::LimitExceeded {
                limit: ShellAnalysisLimitKind::Nodes,
                maximum: self.request.limits.max_nodes,
            });
        }
        if depth > self.request.limits.max_nesting_depth
            && !self.incomplete.iter().any(|reason| {
                matches!(
                    reason,
                    ShellIncompleteReason::LimitExceeded {
                        limit: ShellAnalysisLimitKind::NestingDepth,
                        ..
                    }
                )
            })
        {
            self.incomplete.push(ShellIncompleteReason::LimitExceeded {
                limit: ShellAnalysisLimitKind::NestingDepth,
                maximum: u32::from(self.request.limits.max_nesting_depth),
            });
        }
    }

    const fn should_stop(&self, depth: u16) -> bool {
        self.nodes > self.request.limits.max_nodes || depth > self.request.limits.max_nesting_depth
    }

    fn program(&mut self, program: &ast::Program) -> Result<(), ShellAnalysisError> {
        for list in &program.complete_commands {
            self.compound_list(list, WalkContext::default(), 0)?;
        }
        Ok(())
    }

    fn compound_list(
        &mut self,
        list: &ast::CompoundList,
        context: WalkContext,
        depth: u16,
    ) -> Result<(), ShellAnalysisError> {
        self.visit_node(depth);
        if self.should_stop(depth) {
            return Ok(());
        }
        for ast::CompoundListItem(and_or, separator) in &list.0 {
            let mut item_context = context;
            item_context.background = matches!(separator, ast::SeparatorOperator::Async);
            self.and_or_list(and_or, item_context, depth.saturating_add(1))?;
        }
        Ok(())
    }

    fn and_or_list(
        &mut self,
        list: &ast::AndOrList,
        context: WalkContext,
        depth: u16,
    ) -> Result<(), ShellAnalysisError> {
        self.visit_node(depth);
        if self.should_stop(depth) {
            return Ok(());
        }
        let conditional = !list.additional.is_empty();
        for (_, pipeline) in list {
            let mut pipeline_context = context;
            pipeline_context.conditional |= conditional;
            self.pipeline(pipeline, pipeline_context, depth.saturating_add(1))?;
        }
        Ok(())
    }

    fn pipeline(
        &mut self,
        pipeline: &ast::Pipeline,
        context: WalkContext,
        depth: u16,
    ) -> Result<(), ShellAnalysisError> {
        self.visit_node(depth);
        if self.should_stop(depth) {
            return Ok(());
        }
        let count = u16::try_from(pipeline.seq.len()).unwrap_or(u16::MAX);
        for (index, command) in pipeline.seq.iter().enumerate() {
            let mut command_context = context;
            command_context.conditional |= pipeline.seq.len() > 1;
            self.command(
                command,
                command_context,
                (pipeline.seq.len() > 1).then_some(ShellPipelinePosition {
                    index: u16::try_from(index).unwrap_or(u16::MAX),
                    count,
                    negated: pipeline.bang,
                }),
                depth.saturating_add(1),
            )?;
        }
        Ok(())
    }

    fn command(
        &mut self,
        command: &ast::Command,
        context: WalkContext,
        pipeline: Option<ShellPipelinePosition>,
        depth: u16,
    ) -> Result<(), ShellAnalysisError> {
        self.visit_node(depth);
        if self.should_stop(depth) {
            return Ok(());
        }
        match command {
            ast::Command::Simple(simple) => self.simple_command(simple, context, pipeline, depth),
            ast::Command::Compound(compound, redirects) => {
                self.compound_command(compound, context, depth.saturating_add(1))?;
                if let Some(redirects) = redirects {
                    for redirect in &redirects.0 {
                        self.redirection(redirect, None)?;
                    }
                }
                Ok(())
            }
            ast::Command::Function(function) => {
                self.mark_unsupported("function_definition", function.location().as_ref());
                self.compound_command(&function.body.0, context, depth.saturating_add(1))?;
                if let Some(redirects) = &function.body.1 {
                    for redirect in &redirects.0 {
                        self.redirection(redirect, None)?;
                    }
                }
                Ok(())
            }
            ast::Command::ExtendedTest(test, redirects) => {
                self.mark_unsupported("extended_test", test.location().as_ref());
                if let Some(redirects) = redirects {
                    for redirect in &redirects.0 {
                        self.redirection(redirect, None)?;
                    }
                }
                Ok(())
            }
        }
    }

    fn compound_command(
        &mut self,
        command: &ast::CompoundCommand,
        mut context: WalkContext,
        depth: u16,
    ) -> Result<(), ShellAnalysisError> {
        match command {
            ast::CompoundCommand::Arithmetic(command) => {
                self.mark_unsupported("arithmetic_command", command.location().as_ref());
            }
            ast::CompoundCommand::ArithmeticForClause(command) => {
                self.mark_unsupported("arithmetic_for_clause", command.location().as_ref());
                context.loop_depth = context.loop_depth.saturating_add(1);
                self.compound_list(&command.body.list, context, depth)?;
            }
            ast::CompoundCommand::BraceGroup(command) => {
                context.relation = ShellCommandRelation::Nested;
                self.compound_list(&command.list, context, depth)?;
            }
            ast::CompoundCommand::Subshell(command) => {
                context.relation = ShellCommandRelation::Nested;
                self.compound_list(&command.list, context, depth)?;
            }
            ast::CompoundCommand::ForClause(command) => {
                context.relation = ShellCommandRelation::Nested;
                context.loop_depth = context.loop_depth.saturating_add(1);
                self.compound_list(&command.body.list, context, depth)?;
            }
            ast::CompoundCommand::CaseClause(command) => {
                context.relation = ShellCommandRelation::Nested;
                context.conditional = true;
                for item in &command.cases {
                    if let Some(list) = &item.cmd {
                        self.compound_list(list, context, depth)?;
                    }
                }
            }
            ast::CompoundCommand::IfClause(command) => {
                context.relation = ShellCommandRelation::Nested;
                context.conditional = true;
                self.compound_list(&command.condition, context, depth)?;
                self.compound_list(&command.then, context, depth)?;
                if let Some(elses) = &command.elses {
                    for else_clause in elses {
                        if let Some(condition) = &else_clause.condition {
                            self.compound_list(condition, context, depth)?;
                        }
                        self.compound_list(&else_clause.body, context, depth)?;
                    }
                }
            }
            ast::CompoundCommand::WhileClause(command)
            | ast::CompoundCommand::UntilClause(command) => {
                context.relation = ShellCommandRelation::Nested;
                context.conditional = true;
                context.loop_depth = context.loop_depth.saturating_add(1);
                self.compound_list(&command.0, context, depth)?;
                self.compound_list(&command.1.list, context, depth)?;
            }
            ast::CompoundCommand::Coprocess(command) => {
                self.mark_unsupported("coprocess", command.location().as_ref());
                context.relation = ShellCommandRelation::Nested;
                context.background = true;
                self.command(&command.body, context, None, depth)?;
            }
        }
        Ok(())
    }

    fn simple_command(
        &mut self,
        command: &ast::SimpleCommand,
        context: WalkContext,
        pipeline: Option<ShellPipelinePosition>,
        depth: u16,
    ) -> Result<(), ShellAnalysisError> {
        let Some(executable) = &command.word_or_name else {
            self.simple_items(command, None, depth)?;
            return Ok(());
        };
        if self.commands.len() >= self.request.limits.max_commands as usize {
            self.incomplete.push(ShellIncompleteReason::LimitExceeded {
                limit: ShellAnalysisLimitKind::Commands,
                maximum: self.request.limits.max_commands,
            });
            return Ok(());
        }
        let id = ShellCommandId(u32::try_from(self.commands.len()).unwrap_or(u32::MAX));
        let mut bounds = executable
            .loc
            .as_ref()
            .map(|span| (span.start.index, span.end.index));
        let mut arguments = Vec::new();
        let mut assignments = Vec::new();
        let mut redirects = Vec::new();
        if let Some(prefix) = &command.prefix {
            self.collect_items(
                &prefix.0,
                id,
                &mut bounds,
                &mut arguments,
                &mut assignments,
                &mut redirects,
                depth,
            )?;
        }
        if let Some(suffix) = &command.suffix {
            self.collect_items(
                &suffix.0,
                id,
                &mut bounds,
                &mut arguments,
                &mut assignments,
                &mut redirects,
                depth,
            )?;
        }
        let span = self.owned_bounds(bounds.ok_or_else(|| {
            error(
                self.request.dialect,
                ShellAnalysisErrorKind::Parser,
                "parser omitted source span for executable command".to_owned(),
            )
        })?)?;
        let source = self.source_slice(span)?.trim().to_owned();
        let executable_word = self.word(executable)?;
        let executable_dynamic = matches!(executable_word, ShellWord::Dynamic { .. });
        if executable_dynamic {
            self.incomplete
                .push(ShellIncompleteReason::DynamicExecutable {
                    span: executable_word.span(),
                });
        }
        let executable_value = static_word_value(&executable_word);
        if matches!(executable_value, Some("eval" | "." | "source")) {
            self.incomplete
                .push(ShellIncompleteReason::DynamicShellSource {
                    span: executable_word.span(),
                });
        }
        let command_context = ShellCommandContext {
            pipeline,
            conditional: context.conditional,
            background: context.background,
            loop_depth: context.loop_depth,
            substitution_depth: context.substitution_depth,
        };
        self.commands.push(ShellCommand {
            id,
            parent_id: context.parent_id,
            relation: context.relation,
            span,
            source: source.clone(),
            executable: executable_word,
            arguments,
            assignments,
            context: command_context,
            match_candidates: vec![ShellCommandMatchCandidate {
                subject: source,
                kind: ShellCommandMatchCandidateKind::Original,
                transformation: None,
            }],
        });
        self.redirections.extend(redirects);
        self.word_substitutions(executable, Some(id), context, depth)?;
        if let Some(suffix) = &command.suffix {
            for item in &suffix.0 {
                if let ast::CommandPrefixOrSuffixItem::Word(word) = item {
                    self.word_substitutions(word, Some(id), context, depth)?;
                }
            }
        }
        Ok(())
    }

    fn simple_items(
        &mut self,
        command: &ast::SimpleCommand,
        command_id: Option<ShellCommandId>,
        depth: u16,
    ) -> Result<(), ShellAnalysisError> {
        for item in command
            .prefix
            .iter()
            .flat_map(|prefix| &prefix.0)
            .chain(command.suffix.iter().flat_map(|suffix| &suffix.0))
        {
            match item {
                ast::CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                    self.redirection(redirect, command_id)?;
                }
                ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, command) => {
                    self.mark_unsupported("process_substitution", command.location().as_ref());
                }
                ast::CommandPrefixOrSuffixItem::Word(_)
                | ast::CommandPrefixOrSuffixItem::AssignmentWord(_, _) => {}
            }
        }
        self.visit_node(depth);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_items(
        &mut self,
        items: &[ast::CommandPrefixOrSuffixItem],
        command_id: ShellCommandId,
        bounds: &mut Option<(usize, usize)>,
        arguments: &mut Vec<ShellWord>,
        assignments: &mut Vec<ShellAssignment>,
        redirects: &mut Vec<ShellRedirection>,
        depth: u16,
    ) -> Result<(), ShellAnalysisError> {
        for item in items {
            match item {
                ast::CommandPrefixOrSuffixItem::Word(word) => {
                    extend_bounds(bounds, word.loc.as_ref());
                    arguments.push(self.word(word)?);
                }
                ast::CommandPrefixOrSuffixItem::AssignmentWord(assignment, _) => {
                    extend_bounds(bounds, Some(&assignment.loc));
                    assignments.push(self.assignment(assignment)?);
                }
                ast::CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                    let converted = self.convert_redirection(redirect, Some(command_id))?;
                    extend_owned_bounds(bounds, converted.span, &self.char_to_byte);
                    redirects.push(converted);
                }
                ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, command) => {
                    self.mark_unsupported("process_substitution", command.location().as_ref());
                    let context = WalkContext {
                        relation: ShellCommandRelation::ProcessSubstitution,
                        parent_id: Some(command_id),
                        substitution_depth: 1,
                        ..WalkContext::default()
                    };
                    self.compound_list(&command.list, context, depth.saturating_add(1))?;
                }
            }
        }
        Ok(())
    }

    fn assignment(
        &self,
        assignment: &ast::Assignment,
    ) -> Result<ShellAssignment, ShellAnalysisError> {
        let name = match &assignment.name {
            ast::AssignmentName::VariableName(name) => name.clone(),
            ast::AssignmentName::ArrayElementName(name, key) => format!("{name}[{key}]"),
        };
        let value = match &assignment.value {
            ast::AssignmentValue::Scalar(word) => self.word(word)?,
            ast::AssignmentValue::Array(_) => ShellWord::Dynamic {
                expansions: vec![ShellExpansionKind::Pattern],
                span: self.owned_span(&assignment.loc)?,
            },
        };
        Ok(ShellAssignment {
            name,
            value,
            span: self.owned_span(&assignment.loc)?,
        })
    }

    fn word(&self, word: &ast::Word) -> Result<ShellWord, ShellAnalysisError> {
        let span = word
            .loc
            .as_ref()
            .map(|span| self.owned_span(span))
            .transpose()?
            .unwrap_or_default();
        let pieces = word::parse(&word.value, &self.options).map_err(|word_error| {
            error(
                self.request.dialect,
                ShellAnalysisErrorKind::Parser,
                word_error.to_string(),
            )
        })?;
        let mut expansions = Vec::new();
        collect_expansions(&pieces, &mut expansions);
        expansions.sort_unstable_by_key(|kind| *kind as u8);
        expansions.dedup();
        if expansions.is_empty() {
            Ok(ShellWord::Static {
                value: brush_parser::unquote_str(&word.value),
                span,
            })
        } else {
            Ok(ShellWord::Dynamic { expansions, span })
        }
    }

    fn word_substitutions(
        &mut self,
        word: &ast::Word,
        parent_id: Option<ShellCommandId>,
        context: WalkContext,
        depth: u16,
    ) -> Result<(), ShellAnalysisError> {
        let pieces = word::parse(&word.value, &self.options).map_err(|word_error| {
            error(
                self.request.dialect,
                ShellAnalysisErrorKind::Parser,
                word_error.to_string(),
            )
        })?;
        let mut scripts = Vec::new();
        collect_substitutions(&pieces, &mut scripts);
        for script in scripts {
            if context.substitution_depth >= self.request.limits.max_substitutions {
                self.incomplete.push(ShellIncompleteReason::LimitExceeded {
                    limit: ShellAnalysisLimitKind::Substitutions,
                    maximum: u32::from(self.request.limits.max_substitutions),
                });
                continue;
            }
            let nested = Parser::new(Cursor::new(script.as_bytes()), &self.options)
                .parse_program()
                .map_err(|parse_error| {
                    error(
                        self.request.dialect,
                        ShellAnalysisErrorKind::Syntax,
                        format!("invalid command substitution: {parse_error}"),
                    )
                })?;
            let before = self.commands.len();
            let nested_request = bcode_shell_command_analysis_models::ShellAnalysisRequest {
                schema_version: self.request.schema_version,
                source: script.clone(),
                dialect: self.request.dialect,
                limits: self.request.limits,
            };
            let mut nested_adapter = Adapter::new(&nested_request, self.options.clone());
            let nested_context = WalkContext {
                relation: ShellCommandRelation::CommandSubstitution,
                parent_id,
                conditional: context.conditional,
                background: context.background,
                loop_depth: context.loop_depth,
                substitution_depth: context.substitution_depth.saturating_add(1),
            };
            for list in &nested.complete_commands {
                nested_adapter.compound_list(list, nested_context, depth.saturating_add(1))?;
            }
            for mut command in nested_adapter.commands {
                command.id = ShellCommandId(
                    u32::try_from(before + command.id.0 as usize).unwrap_or(u32::MAX),
                );
                command.parent_id = parent_id;
                self.commands.push(command);
            }
            self.redirections.extend(nested_adapter.redirections);
            self.incomplete.extend(nested_adapter.incomplete);
            self.diagnostics.extend(nested_adapter.diagnostics);
        }
        Ok(())
    }

    fn redirection(
        &mut self,
        redirect: &ast::IoRedirect,
        command_id: Option<ShellCommandId>,
    ) -> Result<(), ShellAnalysisError> {
        if self.redirections.len() >= self.request.limits.max_redirections as usize {
            self.incomplete.push(ShellIncompleteReason::LimitExceeded {
                limit: ShellAnalysisLimitKind::Redirections,
                maximum: self.request.limits.max_redirections,
            });
            return Ok(());
        }
        let converted = self.convert_redirection(redirect, command_id)?;
        self.redirections.push(converted);
        Ok(())
    }

    fn convert_redirection(
        &mut self,
        redirect: &ast::IoRedirect,
        command_id: Option<ShellCommandId>,
    ) -> Result<ShellRedirection, ShellAnalysisError> {
        match redirect {
            ast::IoRedirect::File(fd, kind, target) => {
                let (target_word, static_path) = match target {
                    ast::IoFileRedirectTarget::Filename(word)
                    | ast::IoFileRedirectTarget::Duplicate(word) => {
                        let owned = self.word(word)?;
                        let path = static_word_value(&owned).map(str::to_owned);
                        (owned, path)
                    }
                    ast::IoFileRedirectTarget::Fd(fd) => {
                        let value = fd.to_string();
                        (
                            ShellWord::Static {
                                value,
                                span: ShellSourceSpan::default(),
                            },
                            None,
                        )
                    }
                    ast::IoFileRedirectTarget::ProcessSubstitution(_, command) => {
                        self.mark_unsupported("process_substitution", command.location().as_ref());
                        (
                            ShellWord::Dynamic {
                                expansions: vec![ShellExpansionKind::Process],
                                span: command
                                    .location()
                                    .as_ref()
                                    .map(|span| self.owned_span(span))
                                    .transpose()?
                                    .unwrap_or_default(),
                            },
                            None,
                        )
                    }
                };
                let target_span = target_word.span();
                Ok(ShellRedirection {
                    command_id,
                    kind: match kind {
                        ast::IoFileRedirectKind::Read => ShellRedirectionKind::Input,
                        ast::IoFileRedirectKind::Write | ast::IoFileRedirectKind::Clobber => {
                            ShellRedirectionKind::OutputTruncate
                        }
                        ast::IoFileRedirectKind::Append => ShellRedirectionKind::OutputAppend,
                        ast::IoFileRedirectKind::ReadAndWrite => ShellRedirectionKind::InputOutput,
                        ast::IoFileRedirectKind::DuplicateInput
                        | ast::IoFileRedirectKind::DuplicateOutput => {
                            ShellRedirectionKind::Duplicate
                        }
                    },
                    source_fd: owned_fd(*fd),
                    target: target_word,
                    static_path,
                    span: self.redirection_span(target_span),
                })
            }
            ast::IoRedirect::HereDocument(fd, document) => {
                let target = self.word(&document.here_end)?;
                let target_span = target.span();
                Ok(ShellRedirection {
                    command_id,
                    kind: ShellRedirectionKind::HereDocument,
                    source_fd: owned_fd(*fd),
                    static_path: static_word_value(&target).map(str::to_owned),
                    target,
                    span: self.redirection_span(target_span),
                })
            }
            ast::IoRedirect::HereString(fd, word) => {
                self.mark_unsupported("here_string", word.location().as_ref());
                let target = self.word(word)?;
                let target_span = target.span();
                Ok(ShellRedirection {
                    command_id,
                    kind: ShellRedirectionKind::HereString,
                    source_fd: owned_fd(*fd),
                    static_path: None,
                    target,
                    span: self.redirection_span(target_span),
                })
            }
            ast::IoRedirect::OutputAndError(word, append) => {
                self.mark_unsupported("combined_output_redirection", word.location().as_ref());
                let target = self.word(word)?;
                let target_span = target.span();
                Ok(ShellRedirection {
                    command_id,
                    kind: if *append {
                        ShellRedirectionKind::OutputAppend
                    } else {
                        ShellRedirectionKind::OutputTruncate
                    },
                    source_fd: None,
                    static_path: static_word_value(&target).map(str::to_owned),
                    target,
                    span: self.redirection_span(target_span),
                })
            }
        }
    }

    fn redirection_span(&self, target: ShellSourceSpan) -> ShellSourceSpan {
        let mut start = target.start as usize;
        let bytes = self.request.source.as_bytes();
        while start > 0 && bytes[start - 1].is_ascii_whitespace() {
            start -= 1;
        }
        while start > 0 && matches!(bytes[start - 1], b'<' | b'>' | b'&' | b'|' | b'0'..=b'9') {
            start -= 1;
        }
        ShellSourceSpan::new(u32::try_from(start).unwrap_or(0), target.end)
    }

    fn mark_unsupported(&mut self, construct: &str, span: Option<&SourceSpan>) {
        let owned = span.and_then(|span| self.owned_span(span).ok());
        self.incomplete
            .push(ShellIncompleteReason::UnsupportedConstruct {
                construct: construct.to_owned(),
                span: owned,
            });
    }

    fn owned_span(&self, span: &SourceSpan) -> Result<ShellSourceSpan, ShellAnalysisError> {
        self.owned_bounds((span.start.index, span.end.index))
    }

    fn owned_bounds(
        &self,
        (start, end): (usize, usize),
    ) -> Result<ShellSourceSpan, ShellAnalysisError> {
        let start = *self.char_to_byte.get(start).ok_or_else(|| {
            error(
                self.request.dialect,
                ShellAnalysisErrorKind::Parser,
                "parser source span starts outside input".to_owned(),
            )
        })?;
        let end = *self.char_to_byte.get(end).ok_or_else(|| {
            error(
                self.request.dialect,
                ShellAnalysisErrorKind::Parser,
                "parser source span ends outside input".to_owned(),
            )
        })?;
        Ok(ShellSourceSpan::new(
            u32::try_from(start).map_err(|_| {
                error(
                    self.request.dialect,
                    ShellAnalysisErrorKind::Parser,
                    "source span exceeds contract range".to_owned(),
                )
            })?,
            u32::try_from(end).map_err(|_| {
                error(
                    self.request.dialect,
                    ShellAnalysisErrorKind::Parser,
                    "source span exceeds contract range".to_owned(),
                )
            })?,
        ))
    }

    fn source_slice(&self, span: ShellSourceSpan) -> Result<&str, ShellAnalysisError> {
        self.request
            .source
            .get(span.start as usize..span.end as usize)
            .ok_or_else(|| {
                error(
                    self.request.dialect,
                    ShellAnalysisErrorKind::Parser,
                    "parser source span is not a valid UTF-8 boundary".to_owned(),
                )
            })
    }
}

fn static_word_value(word: &ShellWord) -> Option<&str> {
    match word {
        ShellWord::Static { value, .. } => Some(value),
        ShellWord::Dynamic { .. } => None,
    }
}

fn owned_fd(fd: Option<i32>) -> Option<ShellFileDescriptor> {
    fd.and_then(|fd| u16::try_from(fd).ok())
        .map(ShellFileDescriptor)
}

fn extend_bounds(bounds: &mut Option<(usize, usize)>, span: Option<&SourceSpan>) {
    if let Some(span) = span {
        match bounds {
            Some((start, end)) => {
                *start = (*start).min(span.start.index);
                *end = (*end).max(span.end.index);
            }
            None => *bounds = Some((span.start.index, span.end.index)),
        }
    }
}

fn extend_owned_bounds(
    bounds: &mut Option<(usize, usize)>,
    span: ShellSourceSpan,
    char_to_byte: &[usize],
) {
    let start = char_to_byte.partition_point(|byte| *byte < span.start as usize);
    let end = char_to_byte.partition_point(|byte| *byte < span.end as usize);
    match bounds {
        Some((current_start, current_end)) => {
            *current_start = (*current_start).min(start);
            *current_end = (*current_end).max(end);
        }
        None => *bounds = Some((start, end)),
    }
}

fn collect_expansions(
    pieces: &[word::WordPieceWithSource],
    expansions: &mut Vec<ShellExpansionKind>,
) {
    for piece in pieces {
        match &piece.piece {
            WordPiece::Text(_)
            | WordPiece::SingleQuotedText(_)
            | WordPiece::AnsiCQuotedText(_)
            | WordPiece::EscapeSequence(_) => {}
            WordPiece::DoubleQuotedSequence(nested)
            | WordPiece::GettextDoubleQuotedSequence(nested) => {
                collect_expansions(nested, expansions);
            }
            WordPiece::TildeExpansion(_) => expansions.push(ShellExpansionKind::Tilde),
            WordPiece::ParameterExpansion(_) => expansions.push(ShellExpansionKind::Parameter),
            WordPiece::CommandSubstitution(_) | WordPiece::BackquotedCommandSubstitution(_) => {
                expansions.push(ShellExpansionKind::Command);
            }
            WordPiece::ArithmeticExpression(_) => expansions.push(ShellExpansionKind::Arithmetic),
        }
    }
}

fn collect_substitutions(pieces: &[word::WordPieceWithSource], substitutions: &mut Vec<String>) {
    for piece in pieces {
        match &piece.piece {
            WordPiece::CommandSubstitution(script)
            | WordPiece::BackquotedCommandSubstitution(script) => {
                substitutions.push(script.clone());
            }
            WordPiece::DoubleQuotedSequence(nested)
            | WordPiece::GettextDoubleQuotedSequence(nested) => {
                collect_substitutions(nested, substitutions);
            }
            WordPiece::Text(_)
            | WordPiece::SingleQuotedText(_)
            | WordPiece::AnsiCQuotedText(_)
            | WordPiece::TildeExpansion(_)
            | WordPiece::ParameterExpansion(_)
            | WordPiece::EscapeSequence(_)
            | WordPiece::ArithmeticExpression(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_shell_command_analysis_models::{ShellAnalysisLimits, ShellAnalysisRequest};

    #[test]
    fn rejects_unknown_schema_before_parsing() {
        let mut request = ShellAnalysisRequest::posix("true");
        request.schema_version += 1;
        assert_eq!(
            analyze(&request).unwrap_err().kind,
            ShellAnalysisErrorKind::UnsupportedSchema
        );
    }

    #[test]
    fn rejects_oversized_source_before_parsing() {
        let request = ShellAnalysisRequest {
            schema_version: SHELL_ANALYSIS_SCHEMA_VERSION,
            source: "true".to_owned(),
            dialect: ShellDialect::Posix,
            limits: ShellAnalysisLimits {
                max_source_bytes: 3,
                ..ShellAnalysisLimits::default()
            },
        };
        assert_eq!(
            analyze(&request).unwrap_err().kind,
            ShellAnalysisErrorKind::SourceLimitExceeded
        );
    }

    #[test]
    fn extracts_real_boundaries_but_not_quoted_separators() {
        let analysis = analyze(&ShellAnalysisRequest::posix(
            "rg 'foo|bar' .\nprintf '%s; %s' a b & rm generated",
        ))
        .unwrap();
        assert_eq!(analysis.commands.len(), 3);
        assert_eq!(analysis.commands[0].source, "rg 'foo|bar' .");
        assert_eq!(analysis.commands[1].source, "printf '%s; %s' a b");
        assert!(analysis.commands[1].context.background);
        assert_eq!(analysis.commands[2].source, "rm generated");
        assert!(analysis.completeness.is_complete());
    }

    #[test]
    fn traverses_control_flow_and_substitutions() {
        let analysis = analyze(&ShellAnalysisRequest::posix(
            "if test -f x; then printf '%s' \"$(cat x)\"; fi",
        ))
        .unwrap();
        assert_eq!(analysis.commands.len(), 3);
        assert_eq!(analysis.commands[0].source, "test -f x");
        assert_eq!(analysis.commands[1].source, "printf '%s' \"$(cat x)\"");
        assert_eq!(analysis.commands[2].source, "cat x");
        assert_eq!(
            analysis.commands[2].relation,
            ShellCommandRelation::CommandSubstitution
        );
    }

    #[test]
    fn extracts_redirections_without_heredoc_body_commands() {
        let analysis = analyze(&ShellAnalysisRequest::posix(
            "cat < input.txt\nprintf ok > output.txt\npython3 - <<'PY'\nrm -rf /\nPY\n",
        ))
        .unwrap();
        assert_eq!(analysis.commands.len(), 3);
        assert_eq!(analysis.redirections.len(), 3);
        assert_eq!(
            analysis.redirections[0].static_path.as_deref(),
            Some("input.txt")
        );
        assert_eq!(
            analysis.redirections[1].static_path.as_deref(),
            Some("output.txt")
        );
        assert_eq!(
            analysis.redirections[2].kind,
            ShellRedirectionKind::HereDocument
        );
    }

    #[test]
    fn dynamic_executable_and_eval_are_incomplete() {
        for source in ["cmd=printf; \"$cmd\" ok", "eval \"$SCRIPT\""] {
            let analysis = analyze(&ShellAnalysisRequest::posix(source)).unwrap();
            assert!(!analysis.completeness.is_complete());
        }
    }
}
